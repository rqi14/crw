//! `crw bench` — reproducible search-quality benchmark harness.
//!
//! Runs a QA dataset (FRAMES) through a [`SearchProvider`] (crw's `/v1/search`
//! answer path) and grades each answer with an LLM judge, then writes a
//! snapshot to `bench/runs/<unixts>/` (results jsonl + report json/md) so a
//! run is reproducible and diffable across code changes.
//!
//! This is a **local/release tool, never a CI gate**: it needs a running crw
//! server (with SearXNG + an LLM for the answer path), an LLM key for the
//! judge, and network access to fetch the dataset — none of which exist in CI.

use clap::Args;
use crw_core::config::{AppConfig, LlmConfig};
use crw_extract::llm;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::teardown::CmdError;

#[derive(Args)]
pub struct BenchArgs {
    /// Dataset to run. `frames` auto-downloads google/frames-benchmark.
    #[arg(long, default_value = "frames")]
    pub dataset: String,

    /// Use a local TSV/JSONL dataset file instead of downloading. TSV must have
    /// `Prompt` + `Answer` columns; JSONL objects must have `prompt`/`answer`
    /// (or `Prompt`/`Answer`) keys.
    #[arg(long)]
    pub dataset_file: Option<PathBuf>,

    /// Base URL of the running crw server under test.
    #[arg(long, default_value = "http://localhost:3000")]
    pub server: String,

    /// Bearer key for the server under test, if it requires auth.
    #[arg(long, env = "CRW_API_KEY")]
    pub api_key: Option<String>,

    /// Cap the number of questions (0 = all).
    #[arg(long, default_value_t = 0)]
    pub limit: usize,

    /// Number of search results the answer leg may draw from.
    #[arg(long, default_value_t = 10)]
    pub search_limit: u32,

    /// Judge model — overrides the configured `extraction.llm` model.
    #[arg(long)]
    pub judge_model: Option<String>,

    /// Output directory root for run snapshots.
    #[arg(long, default_value = "bench/runs")]
    pub output: PathBuf,

    /// Per-request timeout (seconds) to the server under test.
    #[arg(long, default_value_t = 120)]
    pub timeout_secs: u64,

    /// RNG seed for the bootstrap CI, so the reported interval is reproducible.
    #[arg(long, default_value_t = 42)]
    pub seed: u64,

    /// Enable adaptive multi-round retrieval (a 2nd evidence-scout round fires
    /// when round-1 abstains). Off = single-shot floor. The route honors this
    /// per-request override.
    #[arg(long)]
    pub multi_round: bool,

    /// Number of diverse query rewrites fetched + unioned per question (recall
    /// lever for long multi-hop queries). Omitted = server default (off).
    #[arg(long, value_name = "N")]
    pub query_expand: Option<usize>,

    /// How many questions to run concurrently. 1 = sequential. Higher cuts
    /// wall-clock but the ceiling is the upstream limits — search backend
    /// engine blocks, residential-proxy connection caps, and the synth model's
    /// TPM — not CPU. Watch the empty/error rate when raising it.
    #[arg(long, default_value_t = 1)]
    pub concurrency: usize,
}

/// One graded question.
#[derive(Debug, Clone)]
struct QaItem {
    prompt: String,
    answer: String,
}

/// Per-item run record (one line of `frames_results.jsonl`).
#[derive(Debug, Serialize)]
struct ItemResult {
    prompt: String,
    truth: String,
    prediction: String,
    passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Aggregate run report (`report.json`).
#[derive(Debug, Serialize)]
struct Report {
    dataset: String,
    provider: String,
    server: String,
    judge_model: String,
    n: usize,
    passed: usize,
    score: f64,
    ci_low: f64,
    ci_high: f64,
    seed: u64,
    /// Search config under test, so floor vs tuned runs are self-describing.
    multi_round: bool,
    query_expand: Option<usize>,
    timestamp_unix: u64,
}

/// A thing the bench can ask a question and get back a synthesized answer.
/// One impl today ([`CrwHttp`]); the trait is the seam where a Brave/Tavily/
/// reference provider drops in for head-to-head runs.
#[allow(async_fn_in_trait)] // private trait, static dispatch only — no async-trait dep needed
trait SearchProvider {
    async fn answer(&self, query: &str) -> Result<String, String>;
    fn name(&self) -> &str;
}

/// Posts `/v1/search` with `answer:true` and returns the synthesized answer.
struct CrwHttp {
    client: reqwest::Client,
    base: String,
    key: Option<String>,
    search_limit: u32,
    multi_round: bool,
    query_expand: Option<usize>,
}

impl SearchProvider for CrwHttp {
    async fn answer(&self, query: &str) -> Result<String, String> {
        // Minimal local view of the envelope so the bench stays decoupled from
        // crw-core's full SearchResponseData shape.
        #[derive(Deserialize)]
        struct Envelope {
            data: Option<Data>,
        }
        #[derive(Deserialize)]
        struct Data {
            answer: Option<String>,
        }

        // `answer` synthesis is server-gated on `scrapeOptions` being present
        // (it needs page markdown to synthesize from) — omit it and the server
        // returns no answer and a "scrapeOptions required" warning. An empty
        // object is enough; formats defaults to markdown server-side.
        // `answerTemperature: 0` makes the synthesized answer deterministic so
        // A/B bench runs are reproducible (the route honors this override).
        let mut body = serde_json::json!({
            "query": query,
            "answer": true,
            "limit": self.search_limit,
            "scrapeOptions": {},
            "answerTemperature": 0,
        });
        // Tuned-run levers — omitted entirely on a floor run so the server
        // applies its (off) defaults.
        if self.multi_round {
            body["multiRound"] = serde_json::json!(true);
        }
        if let Some(n) = self.query_expand {
            body["queryExpandVariants"] = serde_json::json!(n);
        }
        let mut req = self
            .client
            .post(format!("{}/v1/search", self.base.trim_end_matches('/')))
            .json(&body);
        if let Some(k) = &self.key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await.map_err(|e| format!("request: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| format!("body: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                body.chars().take(200).collect::<String>()
            ));
        }
        let env: Envelope = serde_json::from_str(&body).map_err(|e| format!("decode: {e}"))?;
        Ok(env.data.and_then(|d| d.answer).unwrap_or_default())
    }

    fn name(&self) -> &str {
        "crw"
    }
}

pub async fn run(args: BenchArgs) -> Result<(), CmdError> {
    if let Err(e) = run_inner(args).await {
        eprintln!("bench error: {e}");
        return Err(CmdError::code_only(1));
    }
    Ok(())
}

async fn run_inner(args: BenchArgs) -> Result<(), String> {
    // ── Judge config: configured extraction.llm, model overridden, temp 0 so a
    // real quality lever is distinguishable from sampling noise. ──
    let app_config = AppConfig::load().unwrap_or_default();
    let mut judge_cfg: LlmConfig = app_config.extraction.llm.ok_or_else(|| {
        "bench judge requires an LLM — set CRW_EXTRACTION__LLM__API_KEY (and model)".to_string()
    })?;
    if let Some(m) = &args.judge_model {
        judge_cfg.model = m.clone();
    }
    judge_cfg.temperature = Some(0.0);
    if judge_cfg.api_key.is_empty() {
        return Err("bench judge requires a non-empty LLM api_key".to_string());
    }

    // ── Dataset ──
    let dataset_path = ensure_dataset(&args).await?;
    let mut items = load_dataset(&dataset_path)?;
    if args.limit > 0 && items.len() > args.limit {
        items.truncate(args.limit);
    }
    if items.is_empty() {
        return Err(format!(
            "no questions loaded from {}",
            dataset_path.display()
        ));
    }
    eprintln!(
        "bench: {} questions from {} → server {} (judge {})",
        items.len(),
        dataset_path.display(),
        args.server,
        judge_cfg.model
    );

    // ── Run ──
    let provider = CrwHttp {
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(args.timeout_secs))
            .build()
            .map_err(|e| format!("http client: {e}"))?,
        base: args.server.clone(),
        key: args.api_key.clone(),
        search_limit: args.search_limit,
        multi_round: args.multi_round,
        query_expand: args.query_expand,
    };

    // Snapshot dir + incremental results sink, opened *before* the loop so a
    // multi-hour run survives a crash/kill: each verdict is appended and flushed
    // as it lands, not buffered to the end. write_snapshot() later rewrites a
    // clean canonical file from the full in-memory vec.
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let run_dir = args.output.join(ts.to_string());
    std::fs::create_dir_all(&run_dir).map_err(|e| format!("mkdir {}: {e}", run_dir.display()))?;
    let sink = std::io::BufWriter::new(
        std::fs::File::create(run_dir.join("frames_results.jsonl"))
            .map_err(|e| format!("create results file: {e}"))?,
    );

    // `--concurrency N` keeps N questions in flight (buffer_unordered). The work
    // is I/O-bound (search + scrape + LLM), so overlapping awaits — not CPU
    // parallelism — is the win. The real ceiling is upstream (SearXNG engine
    // blocks, residential-proxy connection caps, synth-model TPM), so the safe N
    // is empirical: watch the empty/error rate. The sink is a std Mutex locked
    // only across the sync write, never across an await (no runtime deadlock).
    use futures::stream::StreamExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let sink = Mutex::new(sink);
    let done = AtomicUsize::new(0);
    let pass_count = AtomicUsize::new(0);
    let total = items.len();
    let conc = args.concurrency.max(1);

    let results: Vec<ItemResult> = futures::stream::iter(items.iter())
        .map(|item| {
            let provider = &provider;
            let judge_cfg = &judge_cfg;
            let sink = &sink;
            let done = &done;
            let pass_count = &pass_count;
            async move {
                let (prediction, mut err) = match provider.answer(&item.prompt).await {
                    Ok(a) => (a, None),
                    Err(e) => (String::new(), Some(e)),
                };
                let passed = if prediction.is_empty() {
                    false
                } else {
                    match judge(judge_cfg, &item.prompt, &item.answer, &prediction).await {
                        Ok(p) => p,
                        Err(e) => {
                            err = Some(format!("judge: {e}"));
                            false
                        }
                    }
                };
                let item_result = ItemResult {
                    prompt: item.prompt.clone(),
                    truth: item.answer.clone(),
                    prediction,
                    passed,
                    error: err,
                };
                // Persist incrementally (crash safety). Lock spans only the sync
                // write — never an await — so it can't stall the runtime.
                if let Ok(line) = serde_json::to_string(&item_result) {
                    use std::io::Write;
                    if let Ok(mut s) = sink.lock() {
                        let _ = writeln!(s, "{line}");
                        let _ = s.flush();
                    }
                }
                if passed {
                    pass_count.fetch_add(1, Ordering::Relaxed);
                }
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_multiple_of(10) || n == total {
                    eprintln!(
                        "  {n}/{total} done · {} pass",
                        pass_count.load(Ordering::Relaxed)
                    );
                }
                item_result
            }
        })
        .buffer_unordered(conc)
        .collect()
        .await;

    let _ = sink.into_inner(); // flush + close the results file

    // ── Aggregate + snapshot ──
    let passed = results.iter().filter(|r| r.passed).count();
    let n = results.len();
    let score = passed as f64 / n as f64;
    let (ci_low, ci_high) = bootstrap_ci(&results, args.seed);

    let report = Report {
        dataset: args.dataset.clone(),
        provider: provider.name().to_string(),
        server: args.server.clone(),
        judge_model: judge_cfg.model.clone(),
        n,
        passed,
        score,
        ci_low,
        ci_high,
        seed: args.seed,
        multi_round: args.multi_round,
        query_expand: args.query_expand,
        timestamp_unix: ts,
    };

    write_snapshot(&run_dir, &report, &results)?;

    println!(
        "\n{} {}/{} = {:.1}% (95% CI {:.1}–{:.1}%)\n→ {}",
        report.dataset,
        passed,
        n,
        score * 100.0,
        ci_low * 100.0,
        ci_high * 100.0,
        run_dir.display()
    );
    Ok(())
}

/// LLM judge: PASS if the prediction answers the question per the ground truth.
async fn judge(
    cfg: &LlmConfig,
    question: &str,
    truth: &str,
    prediction: &str,
) -> Result<bool, String> {
    let sys = "You are a strict grader for a question-answering benchmark. Given a QUESTION, \
        the GROUND TRUTH answer, and a model PREDICTION, decide whether the prediction is \
        correct. It is correct if it contains the ground-truth answer or an equivalent (same \
        entity/value, wording may differ). Extra correct detail is fine; a wrong, missing, or \
        contradicted answer is incorrect. Reply with EXACTLY one word: PASS or FAIL.";
    let user = format!(
        "QUESTION:\n{question}\n\nGROUND TRUTH:\n{truth}\n\nPREDICTION:\n{prediction}\n\nVerdict (PASS or FAIL):"
    );
    let out = llm::chat(cfg, sys, &user)
        .await
        .map_err(|e| e.to_string())?;
    Ok(out.content.trim().to_ascii_uppercase().starts_with("PASS"))
}

/// Seeded bootstrap 95% CI on the pass rate (percentile method, 1000 resamples).
/// Seeded so the reported interval is reproducible across runs.
fn bootstrap_ci(results: &[ItemResult], seed: u64) -> (f64, f64) {
    let flags: Vec<u8> = results.iter().map(|r| r.passed as u8).collect();
    if flags.is_empty() {
        return (0.0, 0.0);
    }
    let mut rng = StdRng::seed_from_u64(seed);
    let n = flags.len();
    let mut means: Vec<f64> = (0..1000)
        .map(|_| {
            let sum: u32 = (0..n)
                .map(|_| *flags.choose(&mut rng).unwrap() as u32)
                .sum();
            sum as f64 / n as f64
        })
        .collect();
    means.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (means[24], means[974]) // 2.5th / 97.5th percentile of 1000
}

/// Resolve the dataset file: explicit `--dataset-file`, else download a known
/// dataset to `bench/datasets/<name>/` (cached).
async fn ensure_dataset(args: &BenchArgs) -> Result<PathBuf, String> {
    if let Some(f) = &args.dataset_file {
        return Ok(f.clone());
    }
    match args.dataset.as_str() {
        "frames" => {
            let cache = PathBuf::from("bench/datasets/frames/test.tsv");
            if cache.exists() {
                return Ok(cache);
            }
            let url =
                "https://huggingface.co/datasets/google/frames-benchmark/resolve/main/test.tsv";
            eprintln!("bench: downloading FRAMES → {}", cache.display());
            download(url, &cache).await?;
            Ok(cache)
        }
        other => Err(format!(
            "unknown dataset '{other}'; pass --dataset-file <path> (TSV with Prompt/Answer, or JSONL)"
        )),
    }
}

async fn download(url: &str, dest: &Path) -> Result<(), String> {
    let body = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download {url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download {url}: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("download body: {e}"))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    std::fs::write(dest, &body).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(())
}

/// Parse a dataset file: `.tsv` → Prompt/Answer columns; otherwise JSONL with
/// `prompt`/`answer` (or `Prompt`/`Answer`) keys.
fn load_dataset(path: &Path) -> Result<Vec<QaItem>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if path.extension().is_some_and(|e| e == "tsv") {
        parse_tsv(&text)
    } else {
        parse_jsonl(&text)
    }
}

// ponytail: naive TSV (split on \n then \t) — correct for FRAMES, whose rows
// are single-line and whose fields hold no tabs/newlines. Swap in a quoted-field
// CSV reader only if a future dataset embeds tabs or newlines in a field.
fn parse_tsv(text: &str) -> Result<Vec<QaItem>, String> {
    let mut lines = text.lines();
    let header = lines.next().ok_or("empty TSV")?;
    let cols: Vec<&str> = header.split('\t').collect();
    let pi = cols
        .iter()
        .position(|c| c.eq_ignore_ascii_case("prompt"))
        .ok_or("TSV missing 'Prompt' column")?;
    let ai = cols
        .iter()
        .position(|c| c.eq_ignore_ascii_case("answer"))
        .ok_or("TSV missing 'Answer' column")?;
    let mut items = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if let (Some(p), Some(a)) = (f.get(pi), f.get(ai))
            && !p.trim().is_empty()
        {
            items.push(QaItem {
                prompt: p.trim().to_string(),
                answer: a.trim().to_string(),
            });
        }
    }
    Ok(items)
}

fn parse_jsonl(text: &str) -> Result<Vec<QaItem>, String> {
    #[derive(Deserialize)]
    struct Row {
        #[serde(alias = "Prompt")]
        prompt: Option<String>,
        #[serde(alias = "Answer")]
        answer: Option<String>,
    }
    let mut items = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Row = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        if let (Some(p), Some(a)) = (row.prompt, row.answer)
            && !p.trim().is_empty()
        {
            items.push(QaItem {
                prompt: p,
                answer: a,
            });
        }
    }
    Ok(items)
}

fn write_snapshot(run_dir: &Path, report: &Report, results: &[ItemResult]) -> Result<(), String> {
    std::fs::create_dir_all(run_dir).map_err(|e| format!("mkdir {}: {e}", run_dir.display()))?;

    let mut jsonl = String::new();
    for r in results {
        jsonl.push_str(&serde_json::to_string(r).map_err(|e| e.to_string())?);
        jsonl.push('\n');
    }
    std::fs::write(run_dir.join("frames_results.jsonl"), jsonl).map_err(|e| e.to_string())?;
    std::fs::write(
        run_dir.join("report.json"),
        serde_json::to_string_pretty(report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(run_dir.join("report.md"), report_md(report)).map_err(|e| e.to_string())?;
    Ok(())
}

fn report_md(r: &Report) -> String {
    format!(
        "# crw bench — {dataset}\n\n\
         - provider: `{provider}` @ `{server}`\n\
         - judge: `{judge}`\n\
         - config: multiRound={mr}, queryExpand={qe}\n\
         - questions: {n}\n\
         - **score: {score:.1}%** ({passed}/{n})\n\
         - 95% CI (bootstrap, seed {seed}): {lo:.1}–{hi:.1}%\n\
         - timestamp (unix): {ts}\n",
        dataset = r.dataset,
        provider = r.provider,
        server = r.server,
        judge = r.judge_model,
        mr = r.multi_round,
        qe = r
            .query_expand
            .map(|n| n.to_string())
            .unwrap_or_else(|| "off".into()),
        n = r.n,
        score = r.score * 100.0,
        passed = r.passed,
        seed = r.seed,
        lo = r.ci_low * 100.0,
        hi = r.ci_high * 100.0,
        ts = r.timestamp_unix,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tsv_picks_prompt_and_answer_by_header() {
        let tsv = "Prompt\tAnswer\twiki_links\n\
                   What is 2+2?\t4\thttp://x\n\
                   \t\t\n\
                   Capital of France?\tParis\thttp://y\n";
        let items = parse_tsv(tsv).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].prompt, "What is 2+2?");
        assert_eq!(items[0].answer, "4");
        assert_eq!(items[1].answer, "Paris");
    }

    #[test]
    fn parse_jsonl_accepts_both_casings() {
        let jsonl =
            "{\"prompt\":\"q1\",\"answer\":\"a1\"}\n{\"Prompt\":\"q2\",\"Answer\":\"a2\"}\n";
        let items = parse_jsonl(jsonl).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].prompt, "q2");
        assert_eq!(items[1].answer, "a2");
    }

    #[test]
    fn bootstrap_ci_brackets_the_point_estimate() {
        let mk = |pass: bool| ItemResult {
            prompt: String::new(),
            truth: String::new(),
            prediction: String::new(),
            passed: pass,
            error: None,
        };
        // 70/100 pass → score 0.70; CI should bracket it and stay in [0,1].
        let results: Vec<ItemResult> = (0..100).map(|i| mk(i < 70)).collect();
        let (lo, hi) = bootstrap_ci(&results, 42);
        assert!(lo <= 0.70 && 0.70 <= hi, "CI [{lo},{hi}] must bracket 0.70");
        assert!(lo >= 0.0 && hi <= 1.0);
        // Deterministic under a fixed seed.
        assert_eq!((lo, hi), bootstrap_ci(&results, 42));
    }

    fn mk_result(pass: bool) -> ItemResult {
        ItemResult {
            prompt: String::new(),
            truth: String::new(),
            prediction: String::new(),
            passed: pass,
            error: None,
        }
    }

    #[test]
    fn bootstrap_ci_empty_results_returns_zero_zero() {
        assert_eq!(bootstrap_ci(&[], 42), (0.0, 0.0));
    }

    #[test]
    fn bootstrap_ci_all_pass_is_a_point_mass_at_one() {
        let results: Vec<ItemResult> = (0..20).map(|_| mk_result(true)).collect();
        assert_eq!(bootstrap_ci(&results, 1), (1.0, 1.0));
    }

    #[test]
    fn bootstrap_ci_all_fail_is_a_point_mass_at_zero() {
        let results: Vec<ItemResult> = (0..20).map(|_| mk_result(false)).collect();
        assert_eq!(bootstrap_ci(&results, 1), (0.0, 0.0));
    }

    #[test]
    fn bootstrap_ci_single_item_matches_its_own_flag() {
        assert_eq!(bootstrap_ci(&[mk_result(true)], 7), (1.0, 1.0));
        assert_eq!(bootstrap_ci(&[mk_result(false)], 7), (0.0, 0.0));
    }

    #[test]
    fn bootstrap_ci_different_seeds_can_differ_but_stay_in_bounds() {
        let results: Vec<ItemResult> = (0..50).map(|i| mk_result(i % 3 == 0)).collect();
        let (lo1, hi1) = bootstrap_ci(&results, 1);
        let (lo2, hi2) = bootstrap_ci(&results, 2);
        for (lo, hi) in [(lo1, hi1), (lo2, hi2)] {
            assert!((0.0..=1.0).contains(&lo));
            assert!((0.0..=1.0).contains(&hi));
            assert!(lo <= hi);
        }
    }

    #[test]
    fn parse_tsv_empty_input_is_an_error() {
        let err = parse_tsv("").unwrap_err();
        assert_eq!(err, "empty TSV");
    }

    #[test]
    fn parse_tsv_missing_prompt_column_is_an_error() {
        let err = parse_tsv("Answer\tFoo\nbar\tbaz\n").unwrap_err();
        assert!(err.contains("Prompt"), "got: {err}");
    }

    #[test]
    fn parse_tsv_missing_answer_column_is_an_error() {
        let err = parse_tsv("Prompt\tFoo\nbar\tbaz\n").unwrap_err();
        assert!(err.contains("Answer"), "got: {err}");
    }

    #[test]
    fn parse_tsv_header_column_match_is_case_insensitive() {
        let items = parse_tsv("PROMPT\tANSWER\nq\ta\n").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].prompt, "q");
        assert_eq!(items[0].answer, "a");
    }

    #[test]
    fn parse_tsv_columns_out_of_declared_order_still_resolve_by_header() {
        let items = parse_tsv("Answer\tPrompt\tExtra\n4\twhat is 2+2\tignored\n").unwrap();
        assert_eq!(items[0].prompt, "what is 2+2");
        assert_eq!(items[0].answer, "4");
    }

    #[test]
    fn parse_tsv_row_shorter_than_header_is_skipped() {
        // `f.get(pi)`/`f.get(ai)` return None for a truncated row rather than
        // panicking, and the row is silently dropped.
        let tsv = "Prompt\tAnswer\tExtra\nonly-one-field\nq\ta\tx\n";
        let items = parse_tsv(tsv).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].prompt, "q");
    }

    #[test]
    fn parse_tsv_skips_rows_with_blank_prompt() {
        let tsv = "Prompt\tAnswer\n\tsome answer\nreal question\treal answer\n";
        let items = parse_tsv(tsv).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].prompt, "real question");
    }

    #[test]
    fn parse_tsv_trims_whitespace_around_fields() {
        let items = parse_tsv("Prompt\tAnswer\n  padded q  \t  padded a  \n").unwrap();
        assert_eq!(items[0].prompt, "padded q");
        assert_eq!(items[0].answer, "padded a");
    }

    #[test]
    fn parse_tsv_preserves_unicode() {
        let items = parse_tsv("Prompt\tAnswer\n日本語の質問\t答え🎉\n").unwrap();
        assert_eq!(items[0].prompt, "日本語の質問");
        assert_eq!(items[0].answer, "答え🎉");
    }

    #[test]
    fn parse_jsonl_skips_blank_lines() {
        let jsonl =
            "{\"prompt\":\"q1\",\"answer\":\"a1\"}\n\n\n{\"prompt\":\"q2\",\"answer\":\"a2\"}\n";
        let items = parse_jsonl(jsonl).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn parse_jsonl_skips_rows_missing_prompt_or_answer() {
        let jsonl = "{\"prompt\":\"only prompt\"}\n{\"answer\":\"only answer\"}\n{\"prompt\":\"q\",\"answer\":\"a\"}\n";
        let items = parse_jsonl(jsonl).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].prompt, "q");
    }

    #[test]
    fn parse_jsonl_skips_blank_prompt() {
        let jsonl = "{\"prompt\":\"  \",\"answer\":\"a\"}\n{\"prompt\":\"q\",\"answer\":\"a\"}\n";
        let items = parse_jsonl(jsonl).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].prompt, "q");
    }

    #[test]
    fn parse_jsonl_malformed_line_reports_one_based_line_number() {
        let jsonl = "{\"prompt\":\"q1\",\"answer\":\"a1\"}\nnot json at all\n";
        let err = parse_jsonl(jsonl).unwrap_err();
        assert!(err.starts_with("line 2:"), "got: {err}");
    }

    #[test]
    fn parse_jsonl_empty_input_yields_no_items() {
        assert_eq!(parse_jsonl("").unwrap().len(), 0);
    }

    #[test]
    fn load_dataset_dispatches_on_tsv_extension() {
        let dir = std::env::temp_dir().join(format!("crw-bench-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.tsv");
        std::fs::write(&path, "Prompt\tAnswer\nq\ta\n").unwrap();
        let items = load_dataset(&path).unwrap();
        assert_eq!(items.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_dataset_defaults_to_jsonl_for_other_extensions() {
        let dir = std::env::temp_dir().join(format!("crw-bench-load-jsonl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.jsonl");
        std::fs::write(&path, "{\"prompt\":\"q\",\"answer\":\"a\"}\n").unwrap();
        let items = load_dataset(&path).unwrap();
        assert_eq!(items.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_dataset_missing_file_is_an_error() {
        let path = std::env::temp_dir().join("crw-bench-does-not-exist.tsv");
        let err = load_dataset(&path).unwrap_err();
        assert!(err.contains("read"), "got: {err}");
    }

    #[tokio::test]
    async fn ensure_dataset_unknown_name_without_file_is_an_error() {
        let args = BenchArgs {
            dataset: "not-a-real-dataset".to_string(),
            dataset_file: None,
            server: "http://localhost:3000".to_string(),
            api_key: None,
            limit: 0,
            search_limit: 10,
            judge_model: None,
            output: PathBuf::from("bench/runs"),
            timeout_secs: 120,
            seed: 42,
            multi_round: false,
            query_expand: None,
            concurrency: 1,
        };
        let err = ensure_dataset(&args).await.unwrap_err();
        assert!(err.contains("not-a-real-dataset"));
        assert!(err.contains("--dataset-file"));
    }

    #[tokio::test]
    async fn ensure_dataset_explicit_file_short_circuits_download() {
        let args = BenchArgs {
            dataset: "frames".to_string(),
            dataset_file: Some(PathBuf::from("/tmp/whatever-not-touched.tsv")),
            server: "http://localhost:3000".to_string(),
            api_key: None,
            limit: 0,
            search_limit: 10,
            judge_model: None,
            output: PathBuf::from("bench/runs"),
            timeout_secs: 120,
            seed: 42,
            multi_round: false,
            query_expand: None,
            concurrency: 1,
        };
        // Must return the explicit path as-is without ever touching the
        // network, even though `dataset` names the downloadable "frames" set.
        let resolved = ensure_dataset(&args).await.unwrap();
        assert_eq!(resolved, PathBuf::from("/tmp/whatever-not-touched.tsv"));
    }

    #[test]
    fn report_md_renders_all_fields() {
        let r = Report {
            dataset: "frames".to_string(),
            provider: "crw".to_string(),
            server: "http://localhost:3000".to_string(),
            judge_model: "deepseek-chat".to_string(),
            n: 100,
            passed: 76,
            score: 0.76,
            ci_low: 0.68,
            ci_high: 0.84,
            seed: 42,
            multi_round: true,
            query_expand: Some(3),
            timestamp_unix: 1_700_000_000,
        };
        let md = report_md(&r);
        assert!(md.contains("crw bench — frames"));
        assert!(md.contains("`crw` @ `http://localhost:3000`"));
        assert!(md.contains("judge: `deepseek-chat`"));
        assert!(md.contains("multiRound=true, queryExpand=3"));
        assert!(md.contains("score: 76.0%** (76/100)"));
        assert!(md.contains("68.0–84.0%"));
        assert!(md.contains("seed 42"));
        assert!(md.contains("1700000000"));
    }

    #[test]
    fn report_md_query_expand_off_renders_as_off() {
        let r = Report {
            dataset: "frames".to_string(),
            provider: "crw".to_string(),
            server: "http://localhost:3000".to_string(),
            judge_model: "deepseek-chat".to_string(),
            n: 1,
            passed: 0,
            score: 0.0,
            ci_low: 0.0,
            ci_high: 0.0,
            seed: 1,
            multi_round: false,
            query_expand: None,
            timestamp_unix: 0,
        };
        let md = report_md(&r);
        assert!(md.contains("multiRound=false, queryExpand=off"));
    }

    #[test]
    fn write_snapshot_creates_all_three_files_with_expected_content() {
        let dir = std::env::temp_dir().join(format!("crw-bench-snapshot-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let results = vec![mk_result(true), mk_result(false)];
        let report = Report {
            dataset: "frames".to_string(),
            provider: "crw".to_string(),
            server: "http://localhost:3000".to_string(),
            judge_model: "deepseek-chat".to_string(),
            n: 2,
            passed: 1,
            score: 0.5,
            ci_low: 0.0,
            ci_high: 1.0,
            seed: 1,
            multi_round: false,
            query_expand: None,
            timestamp_unix: 123,
        };
        write_snapshot(&dir, &report, &results).unwrap();

        let jsonl = std::fs::read_to_string(dir.join("frames_results.jsonl")).unwrap();
        assert_eq!(jsonl.lines().count(), 2);

        let json = std::fs::read_to_string(dir.join("report.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["n"], 2);
        assert_eq!(parsed["dataset"], "frames");

        let md = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(md.contains("crw bench — frames"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn item_result_serde_omits_error_when_none_but_keeps_it_when_some() {
        let ok = mk_result(true);
        let ok_json = serde_json::to_value(&ok).unwrap();
        assert!(ok_json.get("error").is_none());

        let mut failed = mk_result(false);
        failed.error = Some("boom".to_string());
        let failed_json = serde_json::to_value(&failed).unwrap();
        assert_eq!(failed_json["error"], "boom");
    }
}
