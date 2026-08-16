//! Search subcommand — web search through CRW Cloud or a local backend.

use super::diag::is_managed_api_url;
use clap::{Args, ValueEnum};
use crw_core::config::AppConfig;
use crw_core::types::SearchResult;
use crw_search::{SearchError, SearxngClient, SearxngParams, transform_flat};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_CLOUD_API_URL: &str = "https://api.fastcrw.com";
const DEFAULT_LOCAL_SEARCH_URL: &str = "http://127.0.0.1:8080";

#[derive(Debug, PartialEq, Eq)]
enum SearchTarget {
    Cloud {
        api_url: String,
        api_key: Option<String>,
    },
    Local {
        backend_url: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SearchFormat {
    /// JSON output with full result details
    Json,
    /// Concise text output (title + URL per line)
    Text,
    /// Markdown output with links
    Markdown,
}

#[derive(Args)]
#[command(after_help = "EXAMPLES:\n  \
    # Plain text (default)\n  \
    crw search \"rust web scraper\"\n\n  \
    # One-shot LLM-ready JSON (title + url + snippet only)\n  \
    crw search \"renewable energy 2024\" --json --fields title,url,snippet --limit 3\n\n  \
    # Save to file\n  \
    crw search \"climate news\" --json -o results.json\n")]
pub struct SearchArgs {
    /// Search query
    pub query: String,

    /// Maximum number of results to return
    #[arg(short, long, default_value = "10")]
    pub limit: u32,

    /// Search backend instance URL.
    ///
    /// Passing this explicitly selects local search, even when CRW Cloud was
    /// configured by `crw setup`. Without it, Cloud credentials are used when
    /// present; otherwise CRW falls back to the configured local backend.
    ///
    /// `--searxng-url` and `CRW_SEARXNG_URL` are the original names and still
    /// work.
    #[arg(
        long,
        visible_alias = "searxng-url",
        env = "CRW_SEARCH_BACKEND_URL",
        value_name = "URL"
    )]
    pub search_backend_url: Option<String>,

    /// Output format
    #[arg(short, long, value_enum, default_value = "text")]
    pub format: SearchFormat,

    /// Shorthand for `--format json`. Industry-standard alias (gh, kubectl,
    /// docker, jq). Wins over `--format` when both are supplied.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,

    /// Write output to file instead of stdout
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<String>,

    /// Search category (general, images, news, videos, etc.)
    #[arg(long)]
    pub category: Option<String>,

    /// Language code (e.g., en, de, fr)
    #[arg(long)]
    pub language: Option<String>,

    /// Time range filter (day, week, month, year)
    #[arg(long)]
    pub time_range: Option<String>,

    /// Safe search level (0 = off, 1 = moderate, 2 = strict)
    #[arg(long)]
    pub safesearch: Option<u8>,

    /// Request timeout in seconds
    #[arg(long, default_value = "30")]
    pub timeout: u64,

    /// Project JSON output to a comma-separated subset of fields
    /// (e.g. `--fields title,url,snippet`). Only applies to `--format json`.
    /// Available: title, url, description, snippet, position, score, category.
    #[arg(long, value_name = "LIST")]
    pub fields: Option<String>,
}

pub async fn run(args: SearchArgs) {
    let http = Arc::new(
        reqwest::Client::builder()
            .redirect(crw_core::url_safety::safe_redirect_policy())
            .build()
            .expect("failed to build HTTP client"),
    );

    let target = resolve_search_target(args.search_backend_url.as_deref());

    let results = match &target {
        SearchTarget::Cloud { api_url, api_key } => {
            match fetch_cloud_results(http.as_ref(), api_url, api_key.as_deref(), &args).await {
                Ok(results) => results,
                Err(error) => {
                    if is_managed_api_url(api_url) {
                        eprintln!("error: cloud search failed: {error}");
                        eprintln!();
                        eprintln!("hint: check your network and API key, then run:");
                        eprintln!("          crw setup --cloud");
                    } else {
                        eprintln!("error: remote search failed: {error}");
                        eprintln!();
                        eprintln!(
                            "hint: check that {api_url} is reachable and its search backend is configured"
                        );
                    }
                    std::process::exit(1);
                }
            }
        }
        SearchTarget::Local { backend_url } => {
            let client = SearxngClient::new(
                Arc::clone(&http),
                backend_url,
                Duration::from_secs(args.timeout),
            );
            let params = SearxngParams {
                q: args.query.clone(),
                categories: args.category.clone(),
                language: args.language.clone(),
                time_range: args.time_range.clone(),
                engines: None,
                pageno: None,
                safesearch: args.safesearch,
                // Self-host/CLI never spends on a metered backend tier.
                paid_rescue: false,
            };
            match client.fetch(&params).await {
                Ok(response) => transform_flat(&response, args.limit),
                Err(error) => {
                    eprintln!(
                        "error: local search failed: {}",
                        local_error_message(&error)
                    );
                    eprintln!();
                    eprintln!("hint: the local search backend is unreachable at {backend_url}");
                    eprintln!();
                    eprintln!("      Let crw setup configure it for you:");
                    eprintln!("          crw setup --local");
                    std::process::exit(1);
                }
            }
        }
    };

    // `--json` shorthand wins over `--format` (clap enforces no double-set
    // via conflicts_with, but if only --json is passed we still need to
    // route to the JSON renderer).
    let format = if args.json {
        SearchFormat::Json
    } else {
        args.format
    };

    let rendered = match format {
        SearchFormat::Json => {
            // `description` is the canonical body field; `snippet` is emitted as
            // an alias so downstream LLM pipelines that ask for "snippet" don't
            // need a rename step. `--fields` projects to a user-chosen subset.
            let selected: Option<Vec<String>> = args.fields.as_ref().map(|s| {
                s.split(',')
                    .map(|f| f.trim().to_string())
                    .filter(|f| !f.is_empty())
                    .collect()
            });
            let enriched: Vec<serde_json::Value> = results
                .iter()
                .map(|r| {
                    let mut obj = serde_json::Map::new();
                    let mut insert = |k: &str, v: serde_json::Value| {
                        if let Some(ref keep) = selected {
                            if keep.iter().any(|f| f == k) {
                                obj.insert(k.to_string(), v);
                            }
                        } else {
                            obj.insert(k.to_string(), v);
                        }
                    };
                    insert("title", serde_json::json!(r.title));
                    insert("url", serde_json::json!(r.url));
                    insert("description", serde_json::json!(r.description));
                    insert("snippet", serde_json::json!(r.description));
                    insert("position", serde_json::json!(r.position));
                    insert("score", serde_json::json!(r.score));
                    insert("category", serde_json::json!(r.category));
                    serde_json::Value::Object(obj)
                })
                .collect();
            match serde_json::to_string_pretty(&enriched) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("error: failed to serialize JSON: {e}");
                    std::process::exit(1);
                }
            }
        }
        SearchFormat::Text => {
            if results.is_empty() {
                format!("No results found for: {}", args.query)
            } else {
                let mut out = String::new();
                for result in &results {
                    out.push_str(&result.title);
                    out.push('\n');
                    out.push_str("  ");
                    out.push_str(&result.url);
                    out.push('\n');
                    if !result.description.is_empty() {
                        let truncated: String = result.description.chars().take(200).collect();
                        out.push_str("  ");
                        if truncated.len() < result.description.len() {
                            out.push_str(&truncated);
                            out.push_str("...");
                        } else {
                            out.push_str(&result.description);
                        }
                        out.push('\n');
                    }
                    out.push('\n');
                }
                out
            }
        }
        SearchFormat::Markdown => {
            if results.is_empty() {
                format!("No results found for: {}", args.query)
            } else {
                let mut out = format!("# Search results for: {}\n\n", args.query);
                for (i, result) in results.iter().enumerate() {
                    out.push_str(&format!("{}. [{}]({})\n", i + 1, result.title, result.url));
                    if !result.description.is_empty() {
                        out.push_str(&format!("   > {}\n", result.description));
                    }
                    out.push('\n');
                }
                out
            }
        }
    };

    match args.output {
        Some(path) => {
            if let Err(e) = std::fs::write(&path, &rendered) {
                eprintln!("error: failed to write {path}: {e}");
                std::process::exit(1);
            }
        }
        None => print!("{rendered}"),
    }
}

/// Call a remote CRW API (managed or self-hosted) and normalize its public
/// response into the result type already consumed by the CLI renderers.
async fn fetch_cloud_results(
    http: &reqwest::Client,
    api_url: &str,
    api_key: Option<&str>,
    args: &SearchArgs,
) -> Result<Vec<SearchResult>, String> {
    if is_managed_api_url(api_url) && api_key.is_none() {
        return Err("the Cloud API key is missing; run crw setup --cloud again".to_string());
    }
    if args.safesearch.is_some() {
        return Err(
            "--safesearch is only available with --search-backend-url local search".to_string(),
        );
    }

    let mut body = Map::new();
    body.insert("query".to_string(), json!(args.query));
    body.insert("limit".to_string(), json!(args.limit));
    if let Some(language) = &args.language {
        body.insert("lang".to_string(), json!(language));
    }
    if let Some(category) = &args.category {
        body.insert("categories".to_string(), json!([category]));
    }
    if let Some(time_range) = &args.time_range {
        let tbs = match time_range.as_str() {
            "day" => "qdr:d",
            "week" => "qdr:w",
            "month" => "qdr:m",
            "year" => "qdr:y",
            other => {
                return Err(format!(
                    "unsupported time range {other:?}; use day, week, month, or year"
                ));
            }
        };
        body.insert("tbs".to_string(), json!(tbs));
    }

    let endpoint = format!("{}/v1/search", api_url.trim_end_matches('/'));
    let mut request = http
        .post(endpoint)
        .timeout(Duration::from_secs(args.timeout))
        .json(&Value::Object(body));
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key);
    }
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            "the configured API timed out".to_string()
        } else {
            "could not reach the configured API".to_string()
        }
    })?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|error| format!("the API returned invalid JSON: {error}"))?;

    parse_cloud_response(status.as_u16(), payload)
}

fn parse_cloud_response(status: u16, payload: Value) -> Result<Vec<SearchResult>, String> {
    let success = payload
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !(200..300).contains(&status) || !success {
        let detail = payload
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| {
                payload
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
            })
            .or_else(|| payload.get("message").and_then(Value::as_str))
            .unwrap_or("request rejected");
        return Err(format!("API returned status {status}: {detail}"));
    }

    let data = payload
        .get("data")
        .ok_or_else(|| "the API response did not contain data".to_string())?;
    // Cloud returns the flat result array directly in data; self-hosted
    // deployments wrap it in data.results. Accept both remote contracts.
    let rows = data.get("results").unwrap_or(data).clone();
    serde_json::from_value(rows)
        .map_err(|error| format!("the API returned an unexpected search result shape: {error}"))
}

fn local_error_message(error: &SearchError) -> String {
    match error {
        SearchError::Timeout => "the backend timed out".to_string(),
        SearchError::Upstream { status, .. } => {
            format!("the backend returned HTTP status {status}")
        }
        SearchError::InvalidResponse(_) => "the backend returned an invalid response".to_string(),
        SearchError::Transport(_) => "could not connect to the backend".to_string(),
    }
}

fn resolve_search_target(explicit_backend: Option<&str>) -> SearchTarget {
    let legacy_backend = non_empty_env("CRW_SEARXNG_URL");
    let env_api_url = non_empty_env("CRW_API_URL");
    let env_api_key = non_empty_env("CRW_API_KEY");
    let config = AppConfig::load().ok();
    choose_search_target(
        explicit_backend,
        legacy_backend.as_deref(),
        env_api_url.as_deref(),
        env_api_key.as_deref(),
        config.as_ref(),
    )
}

fn choose_search_target(
    explicit_backend: Option<&str>,
    legacy_backend: Option<&str>,
    env_api_url: Option<&str>,
    env_api_key: Option<&str>,
    config: Option<&AppConfig>,
) -> SearchTarget {
    if let Some(backend_url) = explicit_backend.or(legacy_backend) {
        return SearchTarget::Local {
            backend_url: backend_url.to_string(),
        };
    }

    let config_api_url = config.and_then(|cfg| cfg.client.api_url.as_deref());
    let config_api_key = config.and_then(|cfg| cfg.client.api_key.as_deref());
    let api_key = env_api_key.or(config_api_key);
    if env_api_url.is_some() || api_key.is_some() || config_api_url.is_some() {
        return SearchTarget::Cloud {
            api_url: env_api_url
                .or(config_api_url)
                .unwrap_or(DEFAULT_CLOUD_API_URL)
                .to_string(),
            api_key: api_key.map(str::to_string),
        };
    }

    SearchTarget::Local {
        backend_url: config
            .and_then(|cfg| cfg.search.search_backend_url.clone())
            .unwrap_or_else(|| DEFAULT_LOCAL_SEARCH_URL.to_string()),
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_json() -> Value {
        json!({
            "url": "https://example.com/rust",
            "title": "Rust tutorial",
            "description": "Learn Rust",
            "snippet": "Learn Rust",
            "position": 1,
            "score": 0.9,
            "publishedDate": null,
            "category": "general",
            "markdown": null,
            "html": null,
            "rawHtml": null,
            "links": null,
            "metadata": null,
            "summary": null,
            "error": null
        })
    }

    #[test]
    fn cloud_setup_credentials_route_search_to_cloud() {
        let mut cfg = AppConfig::default();
        cfg.client.api_url = Some("https://cloud.example".to_string());
        cfg.client.api_key = Some("fc-test".to_string());

        assert_eq!(
            choose_search_target(None, None, None, None, Some(&cfg)),
            SearchTarget::Cloud {
                api_url: "https://cloud.example".to_string(),
                api_key: Some("fc-test".to_string()),
            }
        );
    }

    #[test]
    fn explicit_local_backend_wins_over_cloud_setup() {
        let mut cfg = AppConfig::default();
        cfg.client.api_url = Some("https://cloud.example".to_string());
        cfg.client.api_key = Some("fc-test".to_string());

        assert_eq!(
            choose_search_target(
                Some("http://search.local:8080"),
                None,
                None,
                None,
                Some(&cfg)
            ),
            SearchTarget::Local {
                backend_url: "http://search.local:8080".to_string(),
            }
        );
    }

    #[test]
    fn cloud_credentials_win_over_stale_local_config() {
        let mut cfg = AppConfig::default();
        cfg.client.api_url = Some("https://cloud.example".to_string());
        cfg.client.api_key = Some("fc-test".to_string());
        cfg.search.search_backend_url = Some("http://old-local:8080".to_string());

        assert!(matches!(
            choose_search_target(None, None, None, None, Some(&cfg)),
            SearchTarget::Cloud { .. }
        ));
    }

    #[test]
    fn api_key_env_uses_default_cloud_url() {
        assert_eq!(
            choose_search_target(None, None, None, Some("fc-env"), None),
            SearchTarget::Cloud {
                api_url: DEFAULT_CLOUD_API_URL.to_string(),
                api_key: Some("fc-env".to_string()),
            }
        );
    }

    #[test]
    fn managed_api_requires_a_key_but_custom_remote_does_not() {
        assert!(is_managed_api_url("https://api.fastcrw.com"));
        assert!(is_managed_api_url("https://API.FASTCRW.COM/v1"));
        assert!(!is_managed_api_url("http://localhost:3000"));
        assert!(!is_managed_api_url("https://crw.internal.example"));
    }

    #[test]
    fn unconfigured_search_uses_local_default() {
        assert_eq!(
            choose_search_target(None, None, None, None, None),
            SearchTarget::Local {
                backend_url: DEFAULT_LOCAL_SEARCH_URL.to_string(),
            }
        );
    }

    #[test]
    fn parses_hosted_flat_response() {
        let results = parse_cloud_response(200, json!({"success": true, "data": [result_json()]}))
            .expect("hosted response should parse");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust tutorial");
    }

    #[test]
    fn parses_self_hosted_wrapped_response() {
        let results = parse_cloud_response(
            200,
            json!({"success": true, "data": {"results": [result_json()]}}),
        )
        .expect("wrapped response should parse");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn surfaces_cloud_api_error_without_local_backend_advice() {
        let error =
            parse_cloud_response(401, json!({"success": false, "error": "invalid API key"}))
                .expect_err("error response should fail");
        assert_eq!(error, "API returned status 401: invalid API key");
    }
}
