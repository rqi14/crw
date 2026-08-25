//! Live research data layer for the `/v1/search/research/*` endpoints.
//!
//! Ports the proven `arxivqa-bench/research_tools.py` cascade (which scored
//! 59.6% recall on ArXivQA, beating Firecrawl's 53.3%) to Rust: OpenAlex
//! `/works` search + Semantic Scholar `/paper/search` + `/snippet/search`
//! booster, and the citation graph (SS references/citations/recommendations
//! with OpenAlex fallback). NO self-hosted index — all live.
//!
//! This crate owns only the OpenAlex + SS HTTP legs. The OWN fastCRW SearXNG
//! search leg (the primary recall driver) and any arXiv PDF scrape live in the
//! route handler (`crw-server`, which has `state.searxng` + `state.renderer`),
//! which merges its hits into [`merge_rank`]. Keys are passed per-call from the
//! route's `AppConfig` (this module holds only stateless infra: client, cache,
//! semaphore).
//!
//! Etiquette: dedicated client + descriptive UA, per-source concurrency cap,
//! 24h cache, exponential backoff on 429/5xx (OpenAlex ~10 rps; SS 1 rps shared
//! key — SS is a BOOSTER, its failures degrade gracefully to OpenAlex + SearXNG).

use crw_core::research_types::{ResearchPaperMeta, ResearchPaperResult};
use moka::future::Cache;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};

const UA: &str = "crw-opencore/0.x (https://fastcrw.com; contact@fastcrw.com) reqwest";
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_CONCURRENCY: usize = 8;
const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
// Entries hold full OpenAlex/SS JSON responses (~10-50KB each) and, since the
// arXiv pool, raw Atom XML bodies too. Those are the fattest entries here: up
// to 40 <entry> blocks carrying authors, links, categories and full abstracts,
// with none of the field pruning `select=` gives the JSON responses. moka's
// `max_capacity` counts ENTRIES, not bytes, so the practical ceiling is above
// the old ~50-150MB estimate. 3k keeps the cache useful while still bounding
// it; revisit with a byte-weigher if crw-api's RSS ever tracks research load.
const CACHE_CAP: u64 = 3_000;

/// arXiv's Atom API. No key, no account.
const ARXIV_URL: &str = "https://export.arxiv.org/api/query";
/// arXiv's terms: "no more than one request every three seconds, and limit
/// requests to a single connection at a time ... collectively" across every
/// machine you control. 3.5s buys margin over their 3s floor.
///
/// A per-process pacer IS collectively compliant here because this code runs in
/// the engine, which is a single container. The one window where it is not is a
/// blue/green deploy, when the next colour briefly runs alongside the current
/// one; that is bounded to the length of a deploy and costs at most a doubled
/// rate for those minutes.
const ARXIV_MIN_INTERVAL: Duration = Duration::from_millis(3_500);
/// Benchmark escape hatch, read once. Production never sets it, so the default
/// above is what ships; a long measurement run can widen the interval to stay
/// well clear of arXiv's cool-off, which is slow to recover once tripped.
fn arxiv_interval() -> Duration {
    static I: OnceLock<Duration> = OnceLock::new();
    *I.get_or_init(|| {
        std::env::var("CRW_ARXIV_MIN_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(ARXIV_MIN_INTERVAL)
    })
}
/// Retries for arXiv only, deliberately lower than `get_json`'s four. That path
/// can spend ~94s worst case (4 attempts x 20s + 2+4+8s backoff), and the
/// server's outer timeout returns 504 at 60s — so its own retry budget can
/// outlive the request it belongs to. One retry keeps this pool inside the
/// budget even when arXiv is refusing.
const ARXIV_MAX_ATTEMPTS: u32 = 2;
/// Hard ceiling on how long the arXiv pool may take, queue wait INCLUDED.
///
/// The pacing gate serialises every arXiv call process-wide, so under
/// concurrency the Nth caller waits (N-1) intervals before it even starts.
/// `search_papers_pools` joins its pools with `tokio::join!`, which waits for
/// ALL of them — so without this ceiling a queue of arXiv callers would hold
/// finished OpenAlex/SS results hostage until the server's outer 60s timeout
/// turned the whole request into a 504. A request that used to succeed with
/// three pools must never fail because a fourth one was added.
///
/// 12s: enough for the gate's 3.5s plus a slow answer, small enough that four
/// of them still fit inside the outer budget with room for the other legs.
const ARXIV_BUDGET: Duration = Duration::from_secs(12);

/// Per-call credentials, borrowed from the route's `AppConfig`.
#[derive(Clone, Copy, Default)]
pub struct ResearchKeys<'a> {
    pub openalex_key: Option<&'a str>,
    pub openalex_mailto: Option<&'a str>,
    pub s2_key: Option<&'a str>,
}

/// OpenAlex `/works` filters (all optional, AND-combined). Maps the Firecrawl
/// `authors`/`categories`/`from`/`to` query params.
#[derive(Clone, Default)]
pub struct SearchFilters {
    pub authors: Option<String>,
    pub categories: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// Citation-graph expansion mode for `/papers/{id}/similar`.
#[derive(Clone, Copy)]
pub enum Mode {
    Similar,
    Citers,
    References,
}

struct Infra {
    http: reqwest::Client,
    cache: Cache<String, serde_json::Value>,
}

fn infra() -> Option<&'static Infra> {
    static I: OnceLock<Option<Infra>> = OnceLock::new();
    I.get_or_init(|| {
        let http = reqwest::Client::builder()
            .user_agent(UA)
            .timeout(TIMEOUT)
            .build()
            .ok()?;
        Some(Infra {
            http,
            cache: Cache::builder()
                .max_capacity(CACHE_CAP)
                .time_to_live(CACHE_TTL)
                .build(),
        })
    })
    .as_ref()
}

/// Serialises arXiv calls process-wide and spaces them.
///
/// A Mutex, not a Semaphore: the requirement is "one connection at a TIME",
/// so the lock is held across the request, and the next caller waits out the
/// remaining interval before starting. Measured: called this way arXiv failed 5
/// of 191 real queries (2.6%); called from 6 parallel workers it failed 150 of
/// 191 (79%). The pacing is the whole difference.
fn arxiv_gate() -> &'static Mutex<Option<std::time::Instant>> {
    static G: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(None))
}

fn sem() -> &'static Semaphore {
    static S: OnceLock<Semaphore> = OnceLock::new();
    S.get_or_init(|| Semaphore::new(MAX_CONCURRENCY))
}

fn arxiv_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\d{4}\.\d{4,5}").unwrap())
}

fn ver_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)v\d+$").unwrap())
}

/// URL-encode a query-string value (via the `url` crate, no extra dep).
fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Normalize an arXiv id: strip a leading `arXiv:`/`arxiv:` prefix and a trailing
/// version (`arXiv:2105.05233v3` -> `2105.05233`), lowercase. Matches
/// `research_tools.py`'s `re.sub(r"v\d+$", "", id)` (NOT a split on 'v', which
/// mangles the `arxiv:` prefix).
fn norm_arxiv(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("arXiv:")
        .or_else(|| s.strip_prefix("arxiv:"))
        .unwrap_or(s);
    ver_re().replace(s, "").to_lowercase()
}

/// Cached GET → JSON with exponential backoff on 429/5xx (2s, 4s, 8s).
/// `x_api_key` adds the SS `x-api-key` header.
async fn get_json(url: &str, x_api_key: Option<&str>) -> Option<serde_json::Value> {
    let inf = infra()?;
    let ck = format!("{}|{}", x_api_key.unwrap_or(""), url);
    if let Some(hit) = inf.cache.get(&ck).await {
        return Some(hit);
    }
    let _permit = sem().acquire().await.ok()?;
    for i in 0..4u32 {
        let mut req = inf.http.get(url);
        if let Some(k) = x_api_key {
            req = req.header("x-api-key", k);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                if let Ok(v) = r.json::<serde_json::Value>().await {
                    inf.cache.insert(ck, v.clone()).await;
                    return Some(v);
                }
                return None;
            }
            Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {
                if i == 3 {
                    return None;
                }
                tokio::time::sleep(Duration::from_secs(2u64 << i)).await;
            }
            _ => return None, // other 4xx / network error -> give up (no point retrying)
        }
    }
    None
}

/// Reconstruct plaintext from OpenAlex's `abstract_inverted_index`
/// (`{word: [positions...]}`) → ordered words joined by spaces.
fn reconstruct_abstract(inv: &serde_json::Value) -> Option<String> {
    let obj = inv.as_object()?;
    let mut pairs: Vec<(u64, &str)> = Vec::new();
    for (word, positions) in obj {
        if let Some(arr) = positions.as_array() {
            for p in arr {
                if let Some(pos) = p.as_u64() {
                    pairs.push((pos, word.as_str()));
                }
            }
        }
    }
    if pairs.is_empty() {
        return None;
    }
    pairs.sort_by_key(|(p, _)| *p);
    Some(
        pairs
            .into_iter()
            .map(|(_, w)| w)
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Internal candidate before merge/rank.
#[derive(Clone)]
pub struct PaperHit {
    pub work_id: Option<String>,
    pub arxiv: Option<String>,
    pub doi: Option<String>,
    pub title: String,
    pub abstract_: Option<String>,
    pub cited_by: u64,
    pub score: f64,
}

impl PaperHit {
    /// Dedup key: arXiv id wins, then DOI, then lowercased title.
    fn key(&self) -> String {
        if let Some(a) = &self.arxiv {
            return format!("arxiv:{a}");
        }
        if let Some(d) = &self.doi {
            return format!("doi:{}", d.to_lowercase());
        }
        format!("title:{}", self.title.to_lowercase())
    }

    /// Build a minimal hit from a SearXNG result (route passes these in). The
    /// arXiv id is regex-extracted from the url/title/content.
    pub fn from_searxng(title: &str, blob: &str, score: f64) -> Option<Self> {
        let arxiv = arxiv_re().find(blob).map(|m| norm_arxiv(m.as_str()));
        arxiv.as_ref()?; // only keep scholarly (arXiv) hits from the web leg
        Some(PaperHit {
            work_id: None,
            arxiv,
            doi: None,
            title: title.to_string(),
            abstract_: None,
            cited_by: 0,
            score,
        })
    }

    pub fn into_result(self) -> ResearchPaperResult {
        let mut ids: HashMap<String, Vec<String>> = HashMap::new();
        if let Some(a) = &self.arxiv {
            ids.insert("arxiv".into(), vec![a.clone()]);
        }
        if let Some(d) = &self.doi {
            ids.insert("doi".into(), vec![d.clone()]);
        }
        if let Some(w) = &self.work_id {
            ids.insert("openalex".into(), vec![w.clone()]);
        }
        let primary_id = if let Some(a) = &self.arxiv {
            format!("arxiv:{a}")
        } else if let Some(d) = &self.doi {
            format!("doi:{d}")
        } else if let Some(w) = &self.work_id {
            w.clone()
        } else {
            self.title.clone()
        };
        let paper_id = self.work_id.clone().unwrap_or_else(|| primary_id.clone());
        ResearchPaperResult {
            paper_id,
            primary_id,
            ids,
            title: self.title,
            abstract_: self.abstract_,
            score: self.score,
            signals: None, // we can't compute Firecrawl's structural graph signals live
        }
    }
}

/// Parse one OpenAlex `/works` result object into a [`PaperHit`].
fn openalex_work_to_hit(w: &serde_json::Value) -> Option<PaperHit> {
    let title = w.get("display_name")?.as_str()?.to_string();
    let work_id = w
        .get("id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.rsplit('/').next())
        .map(|s| s.to_string());
    let ids = w.get("ids");
    let doi = ids
        .and_then(|i| i.get("doi"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim_start_matches("https://doi.org/").to_string());
    // arXiv id is encoded in the DOI as 10.48550/arxiv.<id> for arXiv works
    let arxiv = doi.as_ref().and_then(|d| {
        let dl = d.to_lowercase();
        dl.strip_prefix("10.48550/arxiv.").map(norm_arxiv)
    });
    let abstract_ = w
        .get("abstract_inverted_index")
        .and_then(reconstruct_abstract);
    let cited_by = w
        .get("cited_by_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let score = w
        .get("relevance_score")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    Some(PaperHit {
        work_id,
        arxiv,
        doi,
        title,
        abstract_,
        cited_by,
        score,
    })
}

fn openalex_base(keys: &ResearchKeys<'_>) -> String {
    let mut q = String::new();
    if let Some(k) = keys.openalex_key {
        q.push_str(&format!("&api_key={k}"));
    }
    if let Some(m) = keys.openalex_mailto {
        q.push_str(&format!("&mailto={}", enc(m)));
    }
    q
}

/// OpenAlex `/works?search=` + filters → hits.
/// Strip OpenAlex's wildcard metacharacters from a free-text search term.
///
/// OpenAlex reads `?` and `*` in `search=` as WILDCARDS and rejects the whole
/// query with a 400 ("Wildcards (* or ?) require exact (no-stem) search"),
/// which `get_json` turns into `None` and the pool into an empty `Vec` — a
/// silent total loss of this pool, with nothing logged anywhere.
///
/// It is not an edge case: research queries are questions. Measured on the
/// 191-question ArXivQA set, **121 of them (63%) contain `?`**, so before this
/// the OpenAlex pool contributed nothing to nearly two thirds of real traffic.
///
/// Replaced with a space rather than deleted, so `what?why` cannot become one
/// run-together token. Only these two characters matter; everything else in a
/// natural-language question is already safe once form-urlencoded.
fn oa_sanitize(query: &str) -> String {
    query.replace(['?', '*'], " ").trim().to_string()
}

async fn openalex_search(
    keys: &ResearchKeys<'_>,
    query: &str,
    k: usize,
    f: &SearchFilters,
) -> Vec<PaperHit> {
    let mut filter = String::new();
    if let Some(from) = &f.from {
        filter.push_str(&format!(",from_publication_date:{from}"));
    }
    if let Some(to) = &f.to {
        filter.push_str(&format!(",to_publication_date:{to}"));
    }
    if let Some(a) = &f.authors {
        filter.push_str(&format!(",raw_author_name.search:{}", enc(a)));
    }
    // ponytail: `f.categories` (arXiv cat like "cs.LG") needs an arXiv-cat ->
    // OpenAlex-concept/topic map to filter on; deferred. Currently ignored.
    let filter_param = if filter.is_empty() {
        String::new()
    } else {
        format!("&filter={}", filter.trim_start_matches(','))
    };
    let url = format!(
        "https://api.openalex.org/works?search={}{}&per_page={}&select=id,display_name,ids,abstract_inverted_index,cited_by_count,relevance_score{}",
        enc(&oa_sanitize(query)),
        filter_param,
        k.min(50),
        openalex_base(keys),
    );
    match get_json(&url, None).await {
        Some(v) => v
            .get("results")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(openalex_work_to_hit).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Semantic Scholar `/paper/search` → hits (booster). Failures return empty.
async fn ss_search(keys: &ResearchKeys<'_>, query: &str, k: usize) -> Vec<PaperHit> {
    let url = format!(
        "https://api.semanticscholar.org/graph/v1/paper/search?query={}&limit={}&fields=title,abstract,externalIds,citationCount",
        enc(query),
        k.min(50),
    );
    let Some(v) = get_json(&url, keys.s2_key).await else {
        return Vec::new();
    };
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let title = p.get("title")?.as_str()?.to_string();
                    let ext = p.get("externalIds");
                    let arxiv = ext
                        .and_then(|e| e.get("ArXiv"))
                        .and_then(|v| v.as_str())
                        .map(norm_arxiv);
                    let doi = ext
                        .and_then(|e| e.get("DOI"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    Some(PaperHit {
                        work_id: None,
                        arxiv,
                        doi,
                        title,
                        abstract_: p.get("abstract").and_then(|v| v.as_str()).map(String::from),
                        cited_by: p.get("citationCount").and_then(|v| v.as_u64()).unwrap_or(0),
                        score: 0.0,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// SS full-text snippet search → arXiv ids only (recovers body-relevant papers
/// keyword/abstract search misses). The 59.6% harness's big lever.
async fn ss_snippet_ids(keys: &ResearchKeys<'_>, query: &str) -> Vec<String> {
    let url = format!(
        "https://api.semanticscholar.org/graph/v1/snippet/search?limit=100&query={}",
        enc(query),
    );
    let Some(v) = get_json(&url, keys.s2_key).await else {
        return Vec::new();
    };
    let blob = v.to_string();
    let mut seen = std::collections::HashSet::new();
    arxiv_re()
        .find_iter(&blob)
        .map(|m| norm_arxiv(m.as_str()))
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

/// Merge candidate pools, dedup by [`PaperHit::key`], rank, cap at `k`.
/// Ranking: search-frequency (how many sources surfaced it) first, then
/// relevance score, then citation count — coverage-first, matching the harness.
pub fn merge_rank(pools: Vec<Vec<PaperHit>>, k: usize) -> Vec<ResearchPaperResult> {
    let mut by_key: HashMap<String, (PaperHit, u32)> = HashMap::new();
    for pool in pools {
        // dedup WITHIN a pool first, so an intra-pool duplicate doesn't fake
        // multi-source agreement (frequency = how many SOURCES surfaced it).
        let mut seen_in_pool = std::collections::HashSet::new();
        let unique: Vec<PaperHit> = pool
            .into_iter()
            .filter(|h| seen_in_pool.insert(h.key()))
            .collect();
        for hit in unique {
            let key = hit.key();
            by_key
                .entry(key)
                .and_modify(|(existing, freq)| {
                    *freq += 1;
                    // keep the richest record (prefer one with abstract / work_id)
                    if existing.abstract_.is_none() && hit.abstract_.is_some() {
                        existing.abstract_ = hit.abstract_.clone();
                    }
                    if existing.work_id.is_none() && hit.work_id.is_some() {
                        existing.work_id = hit.work_id.clone();
                    }
                    if existing.doi.is_none() && hit.doi.is_some() {
                        existing.doi = hit.doi.clone();
                    }
                    existing.cited_by = existing.cited_by.max(hit.cited_by);
                    existing.score = existing.score.max(hit.score);
                })
                .or_insert((hit, 1));
        }
    }
    let mut ranked: Vec<(PaperHit, u32)> = by_key.into_values().collect();
    ranked.sort_by(|(a, fa), (b, fb)| {
        fb.cmp(fa)
            .then(
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.cited_by.cmp(&a.cited_by))
    });
    ranked
        .into_iter()
        .take(k)
        .map(|(h, _)| h.into_result())
        .collect()
}

/// arXiv Atom XML -> [`PaperHit`]s.
///
/// PANIC-FREE BY CONSTRUCTION: no `unwrap`, no `expect`, no indexing, no
/// slicing. `search_papers_pools` joins its pools with `tokio::join!`, which
/// does NOT isolate a panic, and `crw-server` has no `catch_unwind` layer — a
/// panic here would drop the caller's connection with no HTTP response at all,
/// not even a 500.
fn parse_arxiv_atom(body: &str) -> Vec<PaperHit> {
    static ENTRY: OnceLock<Option<Regex>> = OnceLock::new();
    static ID: OnceLock<Option<Regex>> = OnceLock::new();
    static TITLE: OnceLock<Option<Regex>> = OnceLock::new();
    static SUMMARY: OnceLock<Option<Regex>> = OnceLock::new();
    static DOI: OnceLock<Option<Regex>> = OnceLock::new();

    // `(?s)` on every multi-line field: the regex crate's `.` does not match
    // `\n`, and arXiv wraps long titles and abstracts across lines, so without
    // DOTALL they silently truncate at the first newline.
    let (Some(entry), Some(id), Some(title), Some(summary), Some(doi)) = (
        ENTRY
            .get_or_init(|| Regex::new(r"(?s)<entry>(.*?)</entry>").ok())
            .as_ref(),
        ID.get_or_init(|| Regex::new(r"<id>([^<]*)</id>").ok())
            .as_ref(),
        TITLE
            .get_or_init(|| Regex::new(r"(?s)<title>(.*?)</title>").ok())
            .as_ref(),
        SUMMARY
            .get_or_init(|| Regex::new(r"(?s)<summary>(.*?)</summary>").ok())
            .as_ref(),
        DOI.get_or_init(|| Regex::new(r"<arxiv:doi>([^<]*)</arxiv:doi>").ok())
            .as_ref(),
    ) else {
        // A literal that fails to compile is a bug, but returning an empty pool
        // is the one response here that cannot take the request down with it.
        return Vec::new();
    };

    let grab = |re: &Regex, hay: &str| -> Option<String> {
        re.captures(hay)
            .and_then(|c| c.get(1))
            .map(|m| unescape_xml(m.as_str().trim()))
    };

    // Scoped to each <entry> block FIRST. The feed carries its own
    // document-level <id> and <title> (4 <id> for 3 entries, verified), so a
    // document-wide regex would misalign ids to entries.
    entry
        .captures_iter(body)
        .filter_map(|c| c.get(1))
        .filter_map(|block| {
            let b = block.as_str();
            // arXiv's <id> is a URL whose tail is the versioned id
            // (".../abs/2410.17954v2"); `norm_arxiv` strips the version.
            let arxiv = grab(id, b).and_then(|u| {
                arxiv_re()
                    .find(&u)
                    .map(|m| norm_arxiv(m.as_str()))
                    .filter(|_| u.contains("arxiv.org/abs/"))
            })?;
            let t = grab(title, b).unwrap_or_default();
            Some(PaperHit {
                work_id: None,
                arxiv: Some(arxiv),
                // Present only for papers with a registered journal DOI; most
                // preprints have none.
                doi: grab(doi, b),
                title: t,
                abstract_: grab(summary, b),
                // arXiv exposes no citation count anywhere in the response.
                cited_by: 0,
                // No native relevance score in the body. `ss_search` already
                // sets 0.0 for the same reason, and `merge_rank` ranks on
                // cross-pool frequency first anyway, so a synthetic score would
                // be invented weight, not information.
                score: 0.0,
            })
        })
        .collect()
}

/// The five XML entities arXiv actually emits. Nothing in the crate decodes
/// them, so without this a literal `&amp;` leaks into titles and abstracts.
fn unescape_xml(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        // `&amp;` LAST, or "&amp;lt;" would decode twice into "<".
        .replace("&amp;", "&")
}

/// Clean a free-text query for arXiv, which is NOT the same rule as OpenAlex's.
///
/// The two characters `oa_sanitize` strips are different kinds of thing, and
/// only one of them is punctuation:
///
/// - `?` is sentence punctuation. Research queries are questions — 121 of the
///   191 real ArXivQA questions (63%) end in one — and a trailing `?` is not
///   part of any term anyone is searching for.
/// - `*` is part of real terms: `A*` search, `C*-algebra`. Stripping it does
///   not return nothing, it returns the WRONG thing, because arXiv ORs
///   space-separated terms and `A*` would become a bare `A`.
///
/// So arXiv keeps `*` and loses `?`. OpenAlex has to lose both, because it
/// reads both as wildcards and 400s the whole query either way. Two APIs, two
/// rules, deliberately not shared.
fn arxiv_sanitize(query: &str) -> String {
    query
        .replace('?', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// arXiv `/api/query` -> hits. Failures return empty, like every other pool.
async fn arxiv_search(query: &str, k: usize) -> Vec<PaperHit> {
    let q = arxiv_sanitize(query);
    if q.is_empty() {
        return Vec::new();
    }
    // Verified server behaviour: `all:mixture of experts` is rewritten to
    // `all:mixture OR all:of OR all:experts` — space-joined terms are OR'd, not
    // treated as a phrase. That is what we want for recall on a natural
    // language question, so it is left alone rather than quoted into a phrase.
    let url = format!(
        "{}?search_query=all:{}&max_results={}&sortBy=relevance",
        ARXIV_URL,
        enc(&q),
        k.min(40),
    );
    match arxiv_get(&url).await {
        Some(body) => parse_arxiv_atom(&body),
        None => Vec::new(),
    }
}

/// One paced arXiv fetch. Never panics, never propagates an error.
///
/// Shares `infra()`'s client (so it inherits the 20s timeout) and its cache, but
/// takes `arxiv_gate()` rather than the shared `sem()`: the other pools may run
/// 8-wide, arXiv must run 1-wide.
async fn arxiv_get(url: &str) -> Option<String> {
    let inf = infra()?;
    let ck = format!("arxiv|{url}");
    if let Some(hit) = inf.cache.get(&ck).await {
        // Cached as a JSON string so the one shared cache can hold both the
        // JSON pools' values and this one.
        return hit.as_str().map(|s| s.to_string());
    }
    let mut gate = arxiv_gate().lock().await;
    for attempt in 0..ARXIV_MAX_ATTEMPTS {
        if let Some(prev) = *gate {
            let since = prev.elapsed();
            let interval = arxiv_interval();
            if since < interval {
                tokio::time::sleep(interval - since).await;
            }
        }
        *gate = Some(std::time::Instant::now());
        let resp = inf.http.get(url).header("User-Agent", UA).send().await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.ok()?;
                inf.cache
                    .insert(ck, serde_json::Value::String(body.clone()))
                    .await;
                return Some(body);
            }
            // 429 here carries an empty body and no usable Retry-After, so
            // there is nothing to branch on: wait out one more interval and
            // give up. Any other status is not worth a retry.
            Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {
                if attempt + 1 == ARXIV_MAX_ATTEMPTS {
                    return None;
                }
            }
            _ => return None,
        }
    }
    None
}

/// `search_papers` OpenAlex + SS legs (the route adds the SearXNG leg + calls
/// [`merge_rank`]). Returns raw pools so the route can union its own search.
pub async fn search_papers_pools(
    keys: &ResearchKeys<'_>,
    query: &str,
    k: usize,
    f: &SearchFilters,
) -> Vec<Vec<PaperHit>> {
    let (oa, ss, snip, ax) = tokio::join!(
        openalex_search(keys, query, k, f),
        ss_search(keys, query, k),
        ss_snippet_ids(keys, query),
        // Budgeted, and a timeout degrades to an empty pool rather than
        // failing the request — the same "failure = empty" contract every other
        // pool follows. This is what stops the serialised arXiv queue from
        // turning a healthy request into a 504.
        async {
            tokio::time::timeout(ARXIV_BUDGET, arxiv_search(query, k))
                .await
                .unwrap_or_default()
        },
    );
    // snippet ids -> thin hits (arxiv only) so the union picks up body matches
    let snip_hits: Vec<PaperHit> = snip
        .into_iter()
        .map(|a| PaperHit {
            work_id: None,
            arxiv: Some(a),
            doi: None,
            title: String::new(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        })
        .collect();
    vec![oa, ss, snip_hits, ax]
}

/// Is `id` an arXiv-form id (`arxiv:X`, `arXiv:X`, or a bare `NNNN.NNNNN`)?
/// Returns the bare normalized arXiv id if so.
fn as_arxiv_id(id: &str) -> Option<String> {
    if id.starts_with('W') || id.starts_with("doi:") {
        return None;
    }
    let stripped = id
        .strip_prefix("arxiv:")
        .or_else(|| id.strip_prefix("arXiv:"))
        .unwrap_or(id);
    if arxiv_re().is_match(stripped) {
        Some(norm_arxiv(stripped))
    } else {
        None
    }
}

/// SS `/paper/arXiv:<id>` → metadata. SS is keyed directly by arXiv id, so it
/// resolves reliably where OpenAlex's `10.48550/arxiv.<id>` DOI lookup misses
/// (published papers carry their venue DOI in OpenAlex, not the arXiv one).
async fn ss_inspect(keys: &ResearchKeys<'_>, arxiv: &str) -> Option<ResearchPaperMeta> {
    let url = format!(
        "https://api.semanticscholar.org/graph/v1/paper/arXiv:{arxiv}?fields=title,abstract,authors,externalIds,publicationDate,fieldsOfStudy"
    );
    let v = get_json(&url, keys.s2_key).await?;
    let title = v.get("title")?.as_str()?.to_string();
    let mut ids: HashMap<String, Vec<String>> = HashMap::new();
    ids.insert("arxiv".into(), vec![arxiv.to_string()]);
    if let Some(d) = v
        .get("externalIds")
        .and_then(|e| e.get("DOI"))
        .and_then(|x| x.as_str())
    {
        ids.insert("doi".into(), vec![d.to_string()]);
    }
    let authors = v.get("authors").and_then(|a| a.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|x| x.get("name")?.as_str().map(String::from))
            .collect::<Vec<_>>()
    });
    let categories = v
        .get("fieldsOfStudy")
        .and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
    let date = v
        .get("publicationDate")
        .and_then(|x| x.as_str())
        .map(String::from);
    Some(ResearchPaperMeta {
        paper_id: format!("arxiv:{arxiv}"),
        ids: Some(ids),
        title,
        abstract_: v.get("abstract").and_then(|x| x.as_str()).map(String::from),
        authors,
        categories,
        created_date: date.clone(),
        update_date: date,
    })
}

/// OpenAlex inspect for work ids / DOIs / arXiv-preprint-only papers.
async fn openalex_inspect(keys: &ResearchKeys<'_>, id: &str) -> Option<ResearchPaperMeta> {
    let filter = if let Some(d) = id.strip_prefix("doi:") {
        format!("filter=doi:{d}")
    } else if id.starts_with('W') {
        format!("filter=openalex_id:{id}")
    } else {
        format!("filter=doi:10.48550/arxiv.{}", norm_arxiv(id))
    };
    let url = format!(
        "https://api.openalex.org/works?{}&select=id,display_name,ids,abstract_inverted_index,authorships,primary_topic,publication_date{}",
        filter,
        openalex_base(keys),
    );
    let v = get_json(&url, None).await?;
    let w = v.get("results")?.as_array()?.first()?;
    let hit = openalex_work_to_hit(w)?;
    let authors = w.get("authorships").and_then(|a| a.as_array()).map(|arr| {
        arr.iter()
            .filter_map(|x| {
                x.get("author")?
                    .get("display_name")?
                    .as_str()
                    .map(String::from)
            })
            .collect::<Vec<_>>()
    });
    let categories = w
        .get("primary_topic")
        .and_then(|t| t.get("display_name"))
        .and_then(|v| v.as_str())
        .map(|s| vec![s.to_string()]);
    let date = w
        .get("publication_date")
        .and_then(|v| v.as_str())
        .map(String::from);
    let result = hit.into_result();
    Some(ResearchPaperMeta {
        paper_id: result.paper_id,
        ids: Some(result.ids),
        title: result.title,
        abstract_: result.abstract_,
        authors,
        categories,
        created_date: date.clone(),
        update_date: date,
    })
}

/// `GET /papers/{id}` metadata. arXiv ids resolve via Semantic Scholar (keyed by
/// arXiv); work ids / DOIs via OpenAlex. SS failure falls back to OpenAlex.
pub async fn inspect(keys: &ResearchKeys<'_>, id: &str) -> Option<ResearchPaperMeta> {
    if let Some(arxiv) = as_arxiv_id(id)
        && let Some(m) = ss_inspect(keys, &arxiv).await
    {
        return Some(m);
    }
    openalex_inspect(keys, id).await
}

/// One SS paper object (`{externalIds, title}`) → a thin [`PaperHit`] (arXiv only).
fn ss_paper_to_hit(p: &serde_json::Value) -> Option<PaperHit> {
    let arxiv = p
        .get("externalIds")?
        .get("ArXiv")?
        .as_str()
        .map(norm_arxiv)?;
    Some(PaperHit {
        work_id: None,
        arxiv: Some(arxiv),
        doi: None,
        title: p
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        abstract_: None,
        cited_by: 0,
        score: 0.0,
    })
}

/// SS citation-graph expansion → [`PaperHit`]s (with titles, so `/similar`
/// results aren't empty-titled). `ponytail:` no OpenAlex fallback yet — on SS
/// 429/failure this returns empty; the OpenAlex referenced_works/cites fallback
/// (research_tools.py `openalex_expand`) is the recall upgrade.
async fn ss_expand(keys: &ResearchKeys<'_>, arxiv: &str, mode: Mode) -> Vec<PaperHit> {
    let (path, field) = match mode {
        Mode::References => ("references", "citedPaper"),
        Mode::Citers => ("citations", "citingPaper"),
        Mode::Similar => {
            let url = format!(
                "https://api.semanticscholar.org/recommendations/v1/papers/forpaper/arXiv:{arxiv}?fields=externalIds,title&limit=100"
            );
            let Some(v) = get_json(&url, keys.s2_key).await else {
                return Vec::new();
            };
            return v
                .get("recommendedPapers")
                .and_then(|d| d.as_array())
                .map(|arr| arr.iter().filter_map(ss_paper_to_hit).collect())
                .unwrap_or_default();
        }
    };
    let url = format!(
        "https://api.semanticscholar.org/graph/v1/paper/arXiv:{arxiv}/{path}?fields={field}.externalIds,{field}.title&limit=100"
    );
    let Some(v) = get_json(&url, keys.s2_key).await else {
        return Vec::new();
    };
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|row| ss_paper_to_hit(row.get(field)?))
                .collect()
        })
        .unwrap_or_default()
}

/// `GET /papers/{id}/similar` — citation-graph expansion → ranked results.
/// `mode` selects references / citers / similar. Accepts an `arxiv:`-prefixed,
/// bare, or versioned id (normalized).
pub async fn related(
    keys: &ResearchKeys<'_>,
    id: &str,
    mode: Mode,
    k: usize,
) -> Vec<ResearchPaperResult> {
    let aid = norm_arxiv(id);
    let hits: Vec<PaperHit> = ss_expand(keys, &aid, mode)
        .await
        .into_iter()
        .filter(|h| h.arxiv.as_deref() != Some(aid.as_str()))
        .collect();
    merge_rank(vec![hits], k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arxiv_extraction_and_norm() {
        assert_eq!(norm_arxiv("2105.05233v3"), "2105.05233");
        // the critical fix: prefixed ids must NOT be split on 'v'
        assert_eq!(norm_arxiv("arXiv:1706.03762"), "1706.03762");
        assert_eq!(norm_arxiv("arxiv:2105.05233v12"), "2105.05233");
        let ids: Vec<_> = arxiv_re()
            .find_iter("see 1706.03762 and arXiv:2301.07041v2")
            .map(|m| norm_arxiv(m.as_str()))
            .collect();
        assert_eq!(ids, vec!["1706.03762", "2301.07041"]);
    }

    #[test]
    fn abstract_reconstruction() {
        let inv = json!({"Fully": [0], "Homomorphic": [1], "Encryption": [2], "is": [3]});
        assert_eq!(
            reconstruct_abstract(&inv).unwrap(),
            "Fully Homomorphic Encryption is"
        );
    }

    #[test]
    fn openalex_work_maps_arxiv_from_doi() {
        let w = json!({
            "id": "https://openalex.org/W123",
            "display_name": "Attention Is All You Need",
            "ids": {"doi": "https://doi.org/10.48550/arXiv.1706.03762"},
            "cited_by_count": 99999,
            "relevance_score": 12.3
        });
        let h = openalex_work_to_hit(&w).unwrap();
        assert_eq!(h.work_id.as_deref(), Some("W123"));
        assert_eq!(h.arxiv.as_deref(), Some("1706.03762"));
        let r = h.into_result();
        assert_eq!(r.primary_id, "arxiv:1706.03762");
        assert_eq!(r.paper_id, "W123");
        assert_eq!(r.ids["arxiv"][0], "1706.03762");
    }

    /// Live end-to-end smoke test against real OpenAlex + Semantic Scholar.
    /// Ignored by default (network). Run with keys in env:
    ///   OPENALEX_KEY=.. S2_KEY=.. cargo test -p crw-search live_smoke -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn live_smoke() {
        let oa = std::env::var("OPENALEX_KEY").ok();
        let s2 = std::env::var("S2_KEY").ok();
        let keys = ResearchKeys {
            openalex_key: oa.as_deref(),
            openalex_mailto: Some("team@fastcrw.com"),
            s2_key: s2.as_deref(),
        };
        // inspect a famous paper
        let meta = inspect(&keys, "arxiv:1706.03762").await.expect("inspect");
        println!("inspect title: {}", meta.title);
        assert!(meta.title.to_lowercase().contains("attention"));
        assert!(meta.authors.as_ref().is_some_and(|a| !a.is_empty()));

        // search merges OpenAlex + SS
        let pools = search_papers_pools(
            &keys,
            "flash attention efficient transformers",
            20,
            &SearchFilters::default(),
        )
        .await;
        let results = merge_rank(pools, 20);
        println!("search returned {} papers", results.len());
        assert!(!results.is_empty(), "search returned nothing");

        // citation graph (references of the transformer paper)
        let refs = related(&keys, "1706.03762", Mode::References, 20).await;
        println!("references: {}", refs.len());
    }

    /// The OpenAlex pool was silently empty on 63% of real research traffic:
    /// `?` is a WILDCARD to OpenAlex, and a query carrying one is rejected with
    /// a 400 that `get_json` converts to `None` and the pool to an empty Vec.
    /// Measured on the 191-question ArXivQA set, 121 questions contain `?`.
    /// The arXiv pool parses Atom by regex because the crate has no XML parser
    /// and should not grow one. That is only safe with the three guards this
    /// pins: entry-scoping, DOTALL, and entity unescaping.
    ///
    /// The fixture is shaped like a REAL response (verified against a live
    /// fetch): a feed-level <id>/<title> before the entries, a multi-line
    /// title, an escaped ampersand, and one entry with a DOI and one without.
    /// The arXiv pool must never hold the other pools hostage.
    ///
    /// The pacing gate serialises arXiv process-wide, and `search_papers_pools`
    /// joins with `tokio::join!`, so without a budget a queue of arXiv callers
    /// would keep already-finished OpenAlex/SS results waiting until the
    /// server's outer timeout turned the whole request into a 504. A request
    /// that succeeded with three pools must not fail because a fourth was
    /// added.
    #[tokio::test]
    async fn arxiv_budget_degrades_to_an_empty_pool_instead_of_stalling() {
        // The budget must be well under the server's outer 60s timeout, or it
        // could not protect anything: the request would 504 before it fired.
        assert!(
            ARXIV_BUDGET < Duration::from_secs(30),
            "ARXIV_BUDGET must leave the other pools room inside the request"
        );
        // Stands in for an arXiv call stuck behind a long queue. A tiny budget
        // keeps the test instant while exercising the identical code path.
        let stalled = async {
            tokio::time::sleep(Duration::from_secs(600)).await;
            vec![PaperHit {
                work_id: None,
                arxiv: Some("2401.00001".into()),
                doi: None,
                title: "never arrives".into(),
                abstract_: None,
                cited_by: 0,
                score: 0.0,
            }]
        };
        let out: Vec<PaperHit> = tokio::time::timeout(Duration::from_millis(10), stalled)
            .await
            .unwrap_or_default();
        assert!(
            out.is_empty(),
            "a stalled arXiv call must yield an empty pool, not block the join"
        );
    }

    /// `oa_sanitize` is an OpenAlex rule and must not reach arXiv: `?` and `*`
    /// are part of real search terms, so stripping them returns the WRONG
    /// answer rather than no answer.
    /// arXiv and OpenAlex need DIFFERENT cleaning, and the difference is not
    /// cosmetic: `*` belongs to real terms, `?` is punctuation.
    #[test]
    fn arxiv_sanitize_keeps_star_and_drops_question_mark() {
        // The reviewer's case: `*` must survive or the term is destroyed.
        assert_eq!(arxiv_sanitize("A* search algorithm"), "A* search algorithm");
        assert_eq!(
            arxiv_sanitize("C*-algebra classification"),
            "C*-algebra classification"
        );
        // The common case: 63% of real research queries end in a question mark.
        assert_eq!(
            arxiv_sanitize("Which paper improves upon GRPO?"),
            "Which paper improves upon GRPO"
        );
        // Collapsing whitespace, so a stripped `?` cannot leave a double space
        // that turns into an empty OR term.
        assert_eq!(arxiv_sanitize("what? why?"), "what why");
        assert_eq!(arxiv_sanitize("   "), "");
        // And the OpenAlex rule stays stricter, because its API is stricter.
        assert_eq!(oa_sanitize("A* search algorithm"), "A  search algorithm");
    }

    #[test]
    fn parse_arxiv_atom_handles_a_real_shaped_feed() {
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <id>http://arxiv.org/api/xyzzy</id>
  <title>ArXiv Query: search_query=all:mixture&amp;start=0</title>
  <entry>
    <id>http://arxiv.org/abs/2410.17954v2</id>
    <title>ExpertFlow: Efficient Inference
  via Predictive Caching &amp; Token Scheduling</title>
    <summary>Sparse models can outperform dense ones.</summary>
    <arxiv:doi>10.1145/3770743.3804292</arxiv:doi>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/1701.06538</id>
    <title>Outrageously Large Neural Networks</title>
    <summary>Conditional computation.</summary>
  </entry>
</feed>"#;
        let hits = parse_arxiv_atom(body);

        // Two entries, NOT three: the feed-level <id>/<title> must not be read
        // as an entry. Without entry-scoping the ids misalign to the titles.
        assert_eq!(hits.len(), 2);

        // The version suffix arXiv always returns is stripped, so this key can
        // match the same paper arriving from OpenAlex or Semantic Scholar.
        assert_eq!(hits[0].arxiv.as_deref(), Some("2410.17954"));
        assert_eq!(hits[1].arxiv.as_deref(), Some("1701.06538"));

        // DOTALL: without it the title truncates at the newline.
        assert!(hits[0].title.contains("via Predictive Caching"));
        // Entities are decoded, or a literal "&amp;" reaches the caller.
        assert!(hits[0].title.contains("Caching & Token"));

        // DOI only where the paper has one; a preprint without it stays None
        // rather than becoming an empty string that would key wrongly.
        assert_eq!(hits[0].doi.as_deref(), Some("10.1145/3770743.3804292"));
        assert_eq!(hits[1].doi, None);

        // arXiv exposes neither, and inventing them would be fabricated weight
        // in a ranker that sorts on them.
        assert!(hits.iter().all(|h| h.cited_by == 0 && h.score == 0.0));
        assert!(hits.iter().all(|h| h.work_id.is_none()));
    }

    /// Malformed input must yield an empty pool, never a panic. `tokio::join!`
    /// does not isolate panics and the server has no catch_unwind layer, so a
    /// panic here drops the caller's connection with no response at all.
    #[test]
    fn parse_arxiv_atom_never_panics_on_junk() {
        for body in [
            "",
            "not xml at all",
            "<feed><entry></entry></feed>",
            "<entry><id>http://arxiv.org/abs/</id></entry>",
            // An entry whose id is not an arXiv abs URL must be dropped, not
            // mined for any digits that happen to look like an id.
            "<entry><id>http://example.com/1234.5678</id></entry>",
            "<entry><id>http://arxiv.org/abs/2410.17954v2</id>",
        ] {
            let _ = parse_arxiv_atom(body);
        }
        // The one that IS well-formed but non-arXiv yields nothing.
        assert!(
            parse_arxiv_atom("<entry><id>http://example.com/1234.5678</id></entry>").is_empty()
        );
    }

    #[test]
    fn oa_sanitize_strips_wildcards_that_400_the_whole_query() {
        // The exact shape that fails live: a natural-language question.
        assert_eq!(
            oa_sanitize("Which paper improves upon GRPO?"),
            "Which paper improves upon GRPO"
        );
        // `*` is the other wildcard OpenAlex rejects.
        assert_eq!(oa_sanitize("transformer* scaling"), "transformer  scaling");
        // Replaced with a SPACE, never deleted: joining the two sides would
        // invent a token that is in neither the query nor the index.
        assert_eq!(oa_sanitize("what?why"), "what why");
        // A query with neither character must come through untouched, so this
        // cannot quietly alter the 37% that were already working.
        assert_eq!(
            oa_sanitize("mixture of experts routing"),
            "mixture of experts routing"
        );
        // Trailing whitespace left by the strip is trimmed, so the encoded URL
        // does not carry a dangling `+`.
        assert_eq!(oa_sanitize("scaling laws?  "), "scaling laws");
    }

    #[test]
    fn merge_rank_dedups_and_orders_by_frequency() {
        let a = PaperHit {
            work_id: None,
            arxiv: Some("1.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 1,
            score: 0.0,
        };
        let a2 = PaperHit {
            work_id: None,
            arxiv: Some("1.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: Some("x".into()),
            cited_by: 5,
            score: 0.0,
        };
        let b = PaperHit {
            work_id: None,
            arxiv: Some("2.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 99,
            score: 0.0,
        };
        let out = merge_rank(vec![vec![a, b], vec![a2]], 10);
        assert_eq!(out.len(), 2);
        // "1.1" appears in 2 pools -> ranks first despite lower citations
        assert_eq!(out[0].primary_id, "arxiv:1.1");
        assert_eq!(out[0].abstract_.as_deref(), Some("x")); // merged the richer record
    }

    // --- norm_arxiv ---

    #[test]
    fn norm_arxiv_empty_string_is_empty() {
        assert_eq!(norm_arxiv(""), "");
    }

    #[test]
    fn norm_arxiv_leaves_a_version_free_id_unchanged() {
        assert_eq!(norm_arxiv("2311.12345"), "2311.12345");
    }

    #[test]
    fn norm_arxiv_trims_surrounding_whitespace() {
        assert_eq!(norm_arxiv("  1706.03762  "), "1706.03762");
    }

    #[test]
    fn norm_arxiv_does_not_strip_a_wrong_case_prefix() {
        // only the exact `arXiv:`/`arxiv:` prefixes are stripped; the whole
        // string still gets lowercased.
        assert_eq!(norm_arxiv("ARXIV:1234.5678"), "arxiv:1234.5678");
    }

    // --- reconstruct_abstract ---

    #[test]
    fn reconstruct_abstract_empty_object_is_none() {
        assert!(reconstruct_abstract(&json!({})).is_none());
    }

    #[test]
    fn reconstruct_abstract_orders_out_of_order_positions() {
        let inv = json!({"world": [1], "hello": [0]});
        assert_eq!(reconstruct_abstract(&inv).unwrap(), "hello world");
    }

    #[test]
    fn reconstruct_abstract_keeps_unicode_words() {
        let inv = json!({"café": [0], "日本語": [1]});
        assert_eq!(reconstruct_abstract(&inv).unwrap(), "café 日本語");
    }

    #[test]
    fn reconstruct_abstract_skips_non_array_positions() {
        let inv = json!({"Good": [0], "Bad": "not-an-array"});
        assert_eq!(reconstruct_abstract(&inv).unwrap(), "Good");
    }

    #[test]
    fn reconstruct_abstract_skips_non_integer_position_values() {
        let inv = json!({"Only": [1.5]});
        assert!(reconstruct_abstract(&inv).is_none());
    }

    // --- openalex_work_to_hit ---

    #[test]
    fn openalex_work_to_hit_none_without_a_display_name() {
        let w = json!({"id": "https://openalex.org/W1"});
        assert!(openalex_work_to_hit(&w).is_none());
    }

    #[test]
    fn openalex_work_to_hit_survives_a_missing_id_field() {
        let w = json!({"display_name": "A Paper With No Id"});
        let h = openalex_work_to_hit(&w).unwrap();
        assert!(h.work_id.is_none());
        assert!(h.doi.is_none());
        assert_eq!(h.cited_by, 0);
        assert_eq!(h.score, 0.0);
    }

    #[test]
    fn openalex_work_to_hit_keeps_a_non_arxiv_doi() {
        let w = json!({
            "display_name": "A Journal Paper",
            "ids": {"doi": "https://doi.org/10.1145/123456"}
        });
        let h = openalex_work_to_hit(&w).unwrap();
        assert_eq!(h.doi.as_deref(), Some("10.1145/123456"));
        assert!(h.arxiv.is_none());
    }

    #[test]
    fn openalex_work_to_hit_tolerates_a_malformed_ids_field() {
        // `ids` shaped as a string instead of an object must not panic: every
        // downstream `.get()` on a non-object `Value` just returns `None`.
        let w = json!({"display_name": "T", "ids": "not-an-object"});
        let h = openalex_work_to_hit(&w).unwrap();
        assert!(h.doi.is_none());
    }

    #[test]
    fn openalex_work_to_hit_accepts_an_id_with_no_slash() {
        let w = json!({"id": "W123", "display_name": "T"});
        let h = openalex_work_to_hit(&w).unwrap();
        assert_eq!(h.work_id.as_deref(), Some("W123"));
    }

    // --- oa_sanitize / arxiv_sanitize ---

    #[test]
    fn oa_sanitize_leaves_adjacent_wildcard_runs_as_spaces() {
        assert_eq!(oa_sanitize("a??**b"), "a    b");
    }

    #[test]
    fn oa_sanitize_passes_unicode_through_untouched() {
        assert_eq!(oa_sanitize("日本語 テスト"), "日本語 テスト");
    }

    #[test]
    fn arxiv_sanitize_keeps_unicode_terms() {
        assert_eq!(arxiv_sanitize("café société?"), "café société");
    }

    #[test]
    fn arxiv_sanitize_collapses_repeated_internal_whitespace() {
        assert_eq!(arxiv_sanitize("too   many    spaces"), "too many spaces");
    }

    // --- unescape_xml ---

    #[test]
    fn unescape_xml_leaves_plain_text_untouched() {
        assert_eq!(unescape_xml("hello world"), "hello world");
    }

    #[test]
    fn unescape_xml_decodes_all_five_entities() {
        assert_eq!(
            unescape_xml("a &lt;b&gt; &quot;c&quot; &amp; d&#39;s"),
            "a <b> \"c\" & d's"
        );
    }

    #[test]
    fn unescape_xml_amp_last_avoids_double_decoding() {
        // If `&amp;` ran first, "&amp;lt;" would decode to "&lt;" and then to
        // "<" on a second pass. Decoding `&amp;` LAST keeps it at "&lt;".
        assert_eq!(unescape_xml("&amp;lt;"), "&lt;");
    }

    // --- parse_arxiv_atom ---

    #[test]
    fn parse_arxiv_atom_drops_entries_whose_id_is_not_an_abs_url() {
        let body = "<entry><id>http://arxiv.org/pdf/1234.5678</id><title>T</title></entry>";
        assert!(parse_arxiv_atom(body).is_empty());
    }

    #[test]
    fn parse_arxiv_atom_leaves_abstract_none_when_summary_is_absent() {
        let body = "<entry><id>http://arxiv.org/abs/1234.5678</id><title>T</title></entry>";
        let hits = parse_arxiv_atom(body);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].abstract_.is_none());
    }

    #[test]
    fn parse_arxiv_atom_does_not_dedup_the_same_paper_across_entries() {
        // Two entries for the same paper at different arXiv versions: deduping
        // across sources is merge_rank's job, not the parser's.
        let body = r#"<entry><id>http://arxiv.org/abs/1234.5678v1</id><title>T</title></entry>
<entry><id>http://arxiv.org/abs/1234.5678v2</id><title>T</title></entry>"#;
        let hits = parse_arxiv_atom(body);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.arxiv.as_deref() == Some("1234.5678")));
    }

    #[test]
    fn parse_arxiv_atom_keeps_unicode_in_title_and_summary() {
        let body = "<entry><id>http://arxiv.org/abs/1234.5678</id><title>日本語のタイトル</title><summary>résumé en français</summary></entry>";
        let hits = parse_arxiv_atom(body);
        assert_eq!(hits[0].title, "日本語のタイトル");
        assert_eq!(hits[0].abstract_.as_deref(), Some("résumé en français"));
    }

    #[test]
    fn parse_arxiv_atom_handles_a_large_number_of_entries_without_panicking() {
        let mut body = String::new();
        for i in 0..40 {
            body.push_str(&format!(
                "<entry><id>http://arxiv.org/abs/24{i:02}.00001</id><title>Paper {i}</title></entry>"
            ));
        }
        let hits = parse_arxiv_atom(&body);
        assert_eq!(hits.len(), 40);
    }

    #[test]
    fn parse_arxiv_atom_uses_the_first_id_when_an_entry_has_more_than_one() {
        let body = "<entry><id>http://arxiv.org/abs/1111.1111</id><id>http://arxiv.org/abs/2222.2222</id><title>T</title></entry>";
        let hits = parse_arxiv_atom(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].arxiv.as_deref(), Some("1111.1111"));
    }

    // --- as_arxiv_id ---

    #[test]
    fn as_arxiv_id_rejects_openalex_work_ids() {
        assert!(as_arxiv_id("W123456").is_none());
    }

    #[test]
    fn as_arxiv_id_rejects_doi_prefixed_ids() {
        assert!(as_arxiv_id("doi:10.1000/xyz").is_none());
    }

    #[test]
    fn as_arxiv_id_accepts_a_prefixed_id() {
        assert_eq!(
            as_arxiv_id("arxiv:1706.03762").as_deref(),
            Some("1706.03762")
        );
        assert_eq!(
            as_arxiv_id("arXiv:1706.03762v3").as_deref(),
            Some("1706.03762")
        );
    }

    #[test]
    fn as_arxiv_id_accepts_a_bare_id() {
        assert_eq!(as_arxiv_id("1706.03762").as_deref(), Some("1706.03762"));
    }

    #[test]
    fn as_arxiv_id_rejects_non_matching_text() {
        assert!(as_arxiv_id("hello world").is_none());
        assert!(as_arxiv_id("arxiv:hello").is_none());
    }

    // --- PaperHit::key ---

    #[test]
    fn paper_hit_key_prefers_arxiv_over_doi_and_title() {
        let h = PaperHit {
            work_id: None,
            arxiv: Some("1.1".into()),
            doi: Some("10.1/x".into()),
            title: "T".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        assert_eq!(h.key(), "arxiv:1.1");
    }

    #[test]
    fn paper_hit_key_falls_back_to_lowercased_doi() {
        let h = PaperHit {
            work_id: None,
            arxiv: None,
            doi: Some("10.1/ABC".into()),
            title: "T".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        assert_eq!(h.key(), "doi:10.1/abc");
    }

    #[test]
    fn paper_hit_key_falls_back_to_lowercased_title() {
        let h = PaperHit {
            work_id: None,
            arxiv: None,
            doi: None,
            title: "Attention Is All You Need".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        assert_eq!(h.key(), "title:attention is all you need");
    }

    // --- PaperHit::from_searxng ---

    #[test]
    fn from_searxng_returns_none_without_an_arxiv_id() {
        assert!(PaperHit::from_searxng("Some Blog Post", "no ids here", 0.5).is_none());
    }

    #[test]
    fn from_searxng_extracts_the_arxiv_id_from_the_blob() {
        let h = PaperHit::from_searxng(
            "ExpertFlow paper",
            "see https://arxiv.org/abs/2410.17954v2 for details",
            0.73,
        )
        .unwrap();
        assert_eq!(h.arxiv.as_deref(), Some("2410.17954"));
        assert_eq!(h.title, "ExpertFlow paper");
        assert_eq!(h.score, 0.73);
        assert_eq!(h.cited_by, 0);
        assert!(h.doi.is_none() && h.work_id.is_none() && h.abstract_.is_none());
    }

    #[test]
    fn from_searxng_keeps_unicode_and_emoji_in_the_title() {
        let h = PaperHit::from_searxng("论文 🚀 arXiv:1706.03762", "arxiv.org/abs/1706.03762", 0.1)
            .unwrap();
        assert_eq!(h.title, "论文 🚀 arXiv:1706.03762");
    }

    // --- PaperHit::into_result ---

    #[test]
    fn into_result_falls_back_to_the_title_when_nothing_else_identifies_the_paper() {
        let h = PaperHit {
            work_id: None,
            arxiv: None,
            doi: None,
            title: "Untitled Preprint".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        let r = h.into_result();
        assert_eq!(r.primary_id, "Untitled Preprint");
        assert_eq!(r.paper_id, "Untitled Preprint");
        assert!(r.ids.is_empty());
    }

    #[test]
    fn into_result_uses_the_work_id_as_paper_id_when_present() {
        let h = PaperHit {
            work_id: Some("W99".into()),
            arxiv: None,
            doi: Some("10.1/y".into()),
            title: "T".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        let r = h.into_result();
        // arxiv absent, so primary_id falls back to doi; paper_id always
        // prefers the canonical OpenAlex work id when we have one.
        assert_eq!(r.primary_id, "doi:10.1/y");
        assert_eq!(r.paper_id, "W99");
        assert_eq!(r.ids["doi"][0], "10.1/y");
        assert_eq!(r.ids["openalex"][0], "W99");
    }

    #[test]
    fn into_result_prefers_arxiv_primary_id_even_with_doi_and_work_id_present() {
        let h = PaperHit {
            work_id: Some("W1".into()),
            arxiv: Some("1.1".into()),
            doi: Some("10.1/z".into()),
            title: "T".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        let r = h.into_result();
        assert_eq!(r.primary_id, "arxiv:1.1");
        assert_eq!(r.paper_id, "W1");
        assert_eq!(r.ids.len(), 3);
    }

    // --- merge_rank ---

    #[test]
    fn merge_rank_empty_pools_yield_empty_result() {
        assert!(merge_rank(vec![], 10).is_empty());
        assert!(merge_rank(vec![vec![], vec![]], 10).is_empty());
    }

    #[test]
    fn merge_rank_k_zero_returns_nothing_even_with_hits() {
        let a = PaperHit {
            work_id: None,
            arxiv: Some("2.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        assert!(merge_rank(vec![vec![a]], 0).is_empty());
    }

    #[test]
    fn merge_rank_k_larger_than_pool_returns_everything() {
        let a = PaperHit {
            work_id: None,
            arxiv: Some("2.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        let b = PaperHit {
            work_id: None,
            arxiv: Some("2.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        let out = merge_rank(vec![vec![a, b]], 100);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn merge_rank_breaks_frequency_ties_by_score() {
        let a = PaperHit {
            work_id: None,
            arxiv: Some("7.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.1,
        };
        let b = PaperHit {
            work_id: None,
            arxiv: Some("7.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.8,
        };
        let out = merge_rank(vec![vec![a, b]], 10);
        assert_eq!(out[0].primary_id, "arxiv:7.2");
        assert_eq!(out[1].primary_id, "arxiv:7.1");
    }

    #[test]
    fn merge_rank_breaks_score_ties_by_citation_count() {
        let a = PaperHit {
            work_id: None,
            arxiv: Some("8.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 2,
            score: 0.0,
        };
        let b = PaperHit {
            work_id: None,
            arxiv: Some("8.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 40,
            score: 0.0,
        };
        let out = merge_rank(vec![vec![a, b]], 10);
        assert_eq!(out[0].primary_id, "arxiv:8.2");
        assert_eq!(out[1].primary_id, "arxiv:8.1");
    }

    #[test]
    fn merge_rank_keeps_the_first_known_doi_when_a_duplicate_has_a_different_one() {
        let a = PaperHit {
            work_id: None,
            arxiv: Some("3.1".into()),
            doi: Some("10.1/first".into()),
            title: "T".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        let b = PaperHit {
            work_id: None,
            arxiv: Some("3.1".into()),
            doi: Some("10.1/second".into()),
            title: "T".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.0,
        };
        let out = merge_rank(vec![vec![a], vec![b]], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ids["doi"][0], "10.1/first");
    }

    #[test]
    fn merge_rank_keeps_the_max_citation_count_across_duplicates() {
        let a1 = PaperHit {
            work_id: None,
            arxiv: Some("5.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 3,
            score: 0.0,
        };
        let a2 = PaperHit {
            work_id: None,
            arxiv: Some("5.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 10,
            score: 0.0,
        };
        let b1 = PaperHit {
            work_id: None,
            arxiv: Some("5.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 5,
            score: 0.0,
        };
        let b2 = PaperHit {
            work_id: None,
            arxiv: Some("5.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 5,
            score: 0.0,
        };
        // Both papers are surfaced by 2 distinct pools (frequency tied at 2),
        // so the tie-break falls through to citation count: the merged
        // max(3,10)=10 must beat B's flat 5, proving the merge keeps the MAX,
        // not the last-seen value.
        let out = merge_rank(vec![vec![a1], vec![a2], vec![b1], vec![b2]], 10);
        assert_eq!(out[0].primary_id, "arxiv:5.1");
        assert_eq!(out[1].primary_id, "arxiv:5.2");
    }

    #[test]
    fn merge_rank_keeps_the_max_relevance_score_across_duplicates() {
        let a1 = PaperHit {
            work_id: None,
            arxiv: Some("6.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.2,
        };
        let a2 = PaperHit {
            work_id: None,
            arxiv: Some("6.1".into()),
            doi: None,
            title: "A".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.9,
        };
        let b1 = PaperHit {
            work_id: None,
            arxiv: Some("6.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.5,
        };
        let b2 = PaperHit {
            work_id: None,
            arxiv: Some("6.2".into()),
            doi: None,
            title: "B".into(),
            abstract_: None,
            cited_by: 0,
            score: 0.5,
        };
        let out = merge_rank(vec![vec![a1], vec![a2], vec![b1], vec![b2]], 10);
        assert_eq!(out[0].primary_id, "arxiv:6.1");
        assert_eq!(out[0].score, 0.9);
        assert_eq!(out[1].primary_id, "arxiv:6.2");
    }

    #[test]
    fn merge_rank_intra_pool_duplicate_does_not_inflate_frequency() {
        let x1 = PaperHit {
            work_id: None,
            arxiv: Some("9.1".into()),
            doi: None,
            title: "X".into(),
            abstract_: None,
            cited_by: 100,
            score: 0.0,
        };
        let x2 = x1.clone(); // literal duplicate within the SAME pool
        let y1 = PaperHit {
            work_id: None,
            arxiv: Some("9.2".into()),
            doi: None,
            title: "Y".into(),
            abstract_: None,
            cited_by: 1,
            score: 0.0,
        };
        let y2 = PaperHit {
            work_id: None,
            arxiv: Some("9.2".into()),
            doi: None,
            title: "Y".into(),
            abstract_: None,
            cited_by: 1,
            score: 0.0,
        };
        // Y was surfaced by 2 DISTINCT pools (frequency 2); X is the same pool
        // counted twice and must dedup down to frequency 1, so it must rank
        // BELOW Y despite its far higher citation count.
        let out = merge_rank(vec![vec![x1, x2, y1], vec![y2]], 10);
        assert_eq!(out[0].primary_id, "arxiv:9.2");
        assert_eq!(out[1].primary_id, "arxiv:9.1");
    }

    // --- enc ---

    #[test]
    fn enc_form_urlencodes_reserved_characters() {
        assert_eq!(enc("a b&c=d"), "a+b%26c%3Dd");
    }

    #[test]
    fn enc_empty_string_stays_empty() {
        assert_eq!(enc(""), "");
    }

    // --- Default impls ---

    #[test]
    fn research_keys_default_is_all_none() {
        let k = ResearchKeys::default();
        assert!(k.openalex_key.is_none());
        assert!(k.openalex_mailto.is_none());
        assert!(k.s2_key.is_none());
    }

    #[test]
    fn search_filters_default_is_all_none() {
        let f = SearchFilters::default();
        assert!(
            f.authors.is_none() && f.categories.is_none() && f.from.is_none() && f.to.is_none()
        );
    }

    // --- regex helpers ---

    #[test]
    fn arxiv_re_requires_a_four_digit_year_month_prefix() {
        assert!(arxiv_re().find("123.1234").is_none());
        assert!(arxiv_re().find("1234.1234").is_some());
    }

    #[test]
    fn arxiv_re_accepts_both_four_and_five_digit_suffixes() {
        assert_eq!(arxiv_re().find("1234.1234").unwrap().as_str(), "1234.1234");
        assert_eq!(
            arxiv_re().find("1234.12345").unwrap().as_str(),
            "1234.12345"
        );
    }

    #[test]
    fn ver_re_matches_a_trailing_version_case_insensitively() {
        assert!(ver_re().is_match("2105.05233V12"));
        assert!(!ver_re().is_match("2105.05233vX"));
        assert!(!ver_re().is_match("2105.05233v12x"));
    }
}

#[cfg(test)]
mod live_arxiv {
    use super::*;

    /// Live, network-gated. Proves the pool actually talks to arXiv and that the
    /// paced gate holds: two back-to-back calls must be >= ARXIV_MIN_INTERVAL
    /// apart, which is the single rule that took arXiv's failure rate from 79%
    /// to 2.6% in measurement.
    #[tokio::test]
    #[ignore = "live network"]
    async fn arxiv_pool_answers_and_paces() {
        let t0 = std::time::Instant::now();
        let a = arxiv_search("mixture of experts routing", 20).await;
        let b = arxiv_search("retrieval augmented generation", 20).await;
        let elapsed = t0.elapsed();

        assert!(!a.is_empty(), "first arXiv call returned nothing");
        assert!(!b.is_empty(), "second arXiv call returned nothing");
        assert!(
            a.iter().all(|h| h.arxiv.is_some()),
            "every hit must carry an arXiv id, it is the dedup key"
        );
        assert!(
            elapsed >= ARXIV_MIN_INTERVAL,
            "two calls completed in {elapsed:?}, faster than the pacing gate allows"
        );
    }
}
