//! Native `POST /v1/extract` (+ `GET /v1/extract/{id}`). Multi-URL structured
//! extraction as an async job. Unlike the FC-legacy `/v2/extract` (which merges
//! every URL's JSON into one object, last-write-wins), the native route returns
//! a **per-URL array** (`results:[{url,status,data,error,llmUsage}]`) that keeps
//! each URL's object distinct and carries per-URL LLM usage for downstream
//! billing. Carries the standard native `success` envelope (like every other
//! `/v1` response), but none of the FC-legacy `urlTrace`/deprecation warning.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crw_core::error::CrwError;
use crw_core::evidence::{Basis, BasisWarning};
use crw_core::types::{ExtractOptions, LlmUsage, OutputFormat, ScrapeRequest};

use crate::error::AppError;
use crate::routes::v2::adapters::system_time_rfc3339;
use crate::state::{AppState, ExtractRecord, ExtractStatus, PreparedUrl, UrlResult};

/// Native extract request. camelCase like every other v1 public type.
/// NOTE: no `#[derive(Debug)]` — `llm_api_key` is a secret and must never land
/// in a `{:?}` log line.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractRequest {
    #[serde(default)]
    pub urls: Vec<String>,
    /// Free-text extraction objective (LLM infers the shape). Wired into the
    /// extractor's `extract.prompt` slot — the field JSON extraction actually
    /// reads (NOT `summary_prompt`, which only drives the summary format).
    #[serde(default)]
    pub prompt: Option<String>,
    /// JSON Schema constraining the output.
    #[serde(default)]
    pub schema: Option<Value>,
    // BYOK passthrough.
    #[serde(default)]
    pub llm_api_key: Option<String>,
    #[serde(default)]
    pub llm_provider: Option<String>,
    #[serde(default)]
    pub llm_model: Option<String>,
    // `base_url` is parsed only so we can REJECT it with a clear 400 instead of
    // silently ignoring it (which would route a BYOK key to the wrong endpoint).
    // It flows unvalidated into the LLM client (`build_byok_llm_config`), an SSRF
    // vector shared engine-wide; not accepted here until that path validates it.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Per-field attribution. Each top-level **scalar** property of `schema`
    /// comes back with an honest evidence record (value, citation, status).
    /// Requires `schema`; the model's claimed attribution is verified
    /// server-side, so an unverifiable field says so rather than carrying a
    /// fabricated citation. Reported by `GET /v1/capabilities`
    /// (`extract.perFieldAttribution`).
    #[serde(default)]
    pub basis: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractStartResponse {
    /// Response envelope carried by every native `/v1` response (and required by
    /// the MCP `crw_extract` outputSchema, which the engine's own `/mcp` advertises
    /// and emits this body against — proxy mode advertises no schema, since the body
    /// there comes from a remote we do not author). Always `true` here — a rejected
    /// start is a 4xx error.
    pub success: bool,
    pub id: String,
    pub status: String,
    /// Count of URLs actually enqueued for fetch (preflight-failed URLs are in
    /// the status `results`, not this count).
    pub urls: usize,
}

pub async fn start_extract(
    State(state): State<AppState>,
    body: Result<Json<ExtractRequest>, JsonRejection>,
) -> Result<Json<ExtractStartResponse>, AppError> {
    let Json(req) = body.map_err(AppError::from)?;
    let prepared = prepare_extract(&state, req).await?;
    let urls = prepared.valid_count;
    let id = state
        .start_extract_job(prepared.entries, prepared.template)
        .await;
    Ok(Json(ExtractStartResponse {
        success: true,
        id: id.to_string(),
        status: "processing".to_string(),
        urls,
    }))
}

/// Validated + SSRF-preflighted extract inputs, ready for `start_extract_job`.
pub(crate) struct PreparedExtract {
    pub entries: Vec<PreparedUrl>,
    pub template: ScrapeRequest,
    /// Count of URLs enqueued for fetch (preflight-failed URLs excluded).
    pub valid_count: usize,
}

/// Shared validation, SSRF preflight, and template build for the HTTP route and
/// the MCP `crw_extract` tool. Returns `CrwError::InvalidRequest` (→ 400) on any
/// rejected input so both callers get identical semantics.
pub(crate) async fn prepare_extract(
    state: &AppState,
    req: ExtractRequest,
) -> Result<PreparedExtract, CrwError> {
    if req.urls.is_empty() {
        return Err(CrwError::InvalidRequest(
            "`urls` is required and must be non-empty".into(),
        ));
    }
    let cap = state.config.crawler.max_extract_urls;
    if req.urls.len() > cap {
        return Err(CrwError::InvalidRequest(format!(
            "too many urls: {} exceeds the per-request limit of {cap}",
            req.urls.len()
        )));
    }
    // A whitespace-only prompt is treated as absent (the extractor filters it to
    // empty anyway) so we reject upfront instead of fetching then failing.
    let has_prompt = req.prompt.as_deref().is_some_and(|p| !p.trim().is_empty());
    if !has_prompt && req.schema.is_none() {
        return Err(CrwError::InvalidRequest(
            "nothing to extract: provide a non-empty `prompt`, a `schema`, or both".into(),
        ));
    }
    // Evidence is emitted per top-level scalar schema property, so a prompt-only
    // extraction has nothing to attribute. Reject upfront (the worker would fail
    // the same way per URL, but only after paying for every fetch).
    let basis = req.basis.unwrap_or(false);
    if basis && req.schema.is_none() {
        return Err(CrwError::InvalidRequest(
            "`basis` (per-field attribution) requires a `schema`: evidence is emitted per schema \
             property, so a prompt-only extraction has no fields to attribute"
                .into(),
        ));
    }
    if req.base_url.is_some() {
        return Err(CrwError::InvalidRequest(
            "`baseUrl` is not supported on /v1/extract; configure the LLM endpoint \
             server-side ([extraction.llm.base_url])"
                .into(),
        ));
    }

    // LLM-availability guards, upfront (cheaper than failing in the worker).
    // Mirror /v1/scrape's BYOK-header guard: the worker reaches the LLM directly,
    // bypassing the scrape handler's check.
    if let Some(cfg) = state.config.extraction.llm.as_ref()
        && cfg.require_byok_header.is_some()
        && req.llm_api_key.is_none()
    {
        return Err(CrwError::InvalidRequest(
            "LLM features require a per-request llm_api_key (BYOK header guard active)".into(),
        ));
    }
    if state.config.extraction.llm.is_none() && req.llm_api_key.is_none() {
        return Err(CrwError::InvalidRequest(
            "extraction requires an LLM: set [extraction.llm] in server config or pass \
             llm_api_key in the request body"
                .into(),
        ));
    }

    // Per-URL preflight. Each SSRF check does a DNS lookup, so a serial loop over
    // up to `max_extract_urls` (50) URLs added tens of seconds of cold DNS before
    // any extraction started. Validate concurrently; `join_all` preserves order,
    // which the response relies on (`entries` align 1:1 with `req.urls`). Bad
    // parse / SSRF failures become `failed` results (surfaced, not dropped).
    let entries: Vec<PreparedUrl> =
        futures::future::join_all(req.urls.iter().map(|u| async move {
            match url::Url::parse(u) {
                Ok(parsed) => match crw_core::url_safety::validate_safe_url_resolved(&parsed).await
                {
                    Ok(()) => PreparedUrl {
                        url: u.clone(),
                        preflight_error: None,
                    },
                    Err(e) => PreparedUrl {
                        url: u.clone(),
                        preflight_error: Some(e),
                    },
                },
                Err(e) => PreparedUrl {
                    url: u.clone(),
                    preflight_error: Some(format!("invalid URL: {e}")),
                },
            }
        }))
        .await;
    let valid_count = entries
        .iter()
        .filter(|e| e.preflight_error.is_none())
        .count();
    if valid_count == 0 {
        return Err(CrwError::InvalidRequest(
            "no valid URLs to extract (all failed URL parsing or the SSRF safety check)".into(),
        ));
    }

    let template = ScrapeRequest {
        formats: vec![OutputFormat::Json],
        json_schema: req.schema.clone(),
        basis,
        // `extract.prompt` is the field JSON extraction reads (single.rs).
        extract: Some(ExtractOptions {
            schema: None,
            prompt: req.prompt.clone(),
        }),
        llm_api_key: req.llm_api_key.clone(),
        llm_provider: req.llm_provider.clone(),
        llm_model: req.llm_model.clone(),
        ..Default::default()
    };

    Ok(PreparedExtract {
        entries,
        template,
        valid_count,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractUrlResult {
    pub url: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_usage: Option<LlmUsage>,
    /// Per-field evidence for this URL; present only when the request set
    /// `basis: true`. One entry per top-level scalar schema property.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<Vec<Basis>>,
    /// Coded reasons for every basis downgrade on this URL. Closed, crw-owned
    /// code set — never upstream text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub basis_warnings: Vec<BasisWarning>,
    /// `"sha256:"`-prefixed hash of the canonical text sent to the extraction
    /// LLM. The independent record a consumer checks a citation's `sourceHash`
    /// against, so the check is not circular.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_input_hash: Option<String>,
}

impl From<UrlResult> for ExtractUrlResult {
    fn from(r: UrlResult) -> Self {
        ExtractUrlResult {
            url: r.url,
            status: r.status.as_str().to_string(),
            data: r.data,
            error: r.error,
            llm_usage: r.llm_usage,
            basis: r.basis,
            basis_warnings: r.basis_warnings,
            llm_input_hash: r.llm_input_hash,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtractStatusResponse {
    /// Response envelope carried by every native `/v1` response (and required by
    /// the MCP `crw_check_extract_status` / `crw_cancel_extract` outputSchema, which
    /// the engine's own `/mcp` advertises and emits this body against — proxy mode
    /// advertises no schema, since the body there comes from a remote we do not
    /// author). `false` only when the whole job failed.
    pub success: bool,
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ExtractUrlResult>,
    /// Job-level error, set only when every URL failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub expires_at: String,
    pub credits_used: u32,
    pub tokens_used: u32,
}

/// The one canonical HTTP/MCP serializer for extract lifecycle state.
pub(crate) fn serialize_extract_status(id: Uuid, rec: ExtractRecord) -> ExtractStatusResponse {
    let expires_at = system_time_rfc3339(rec.expires_at);
    ExtractStatusResponse {
        success: rec.status != ExtractStatus::Failed,
        id: id.to_string(),
        status: rec.status.as_str().to_string(),
        results: rec
            .per_url
            .into_iter()
            .map(ExtractUrlResult::from)
            .collect(),
        error: rec.error,
        expires_at,
        credits_used: rec.credits_used,
        tokens_used: rec.tokens_used,
    }
}

pub(crate) async fn get_extract_status(
    state: &AppState,
    id: Uuid,
) -> Result<ExtractStatusResponse, CrwError> {
    let rec = state.get_extract_job(id).await?;
    Ok(serialize_extract_status(id, rec))
}

pub(crate) async fn cancel_extract_status(
    state: &AppState,
    id: Uuid,
) -> Result<ExtractStatusResponse, CrwError> {
    let rec = state.cancel_extract_job(id).await?;
    Ok(serialize_extract_status(id, rec))
}

pub async fn get_extract(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ExtractStatusResponse>, AppError> {
    Ok(Json(get_extract_status(&state, id).await?))
}

pub async fn cancel_extract(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<ExtractStatusResponse>, AppError> {
    Ok(Json(cancel_extract_status(&state, id).await?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crw_core::config::AppConfig;

    // The stdio/CLI MCP proxies ship the raw `/v1/extract` start body verbatim as
    // `structuredContent`, so this serialized shape must satisfy the advertised
    // `crw_extract` outputSchema — regression lock for issue #318 (a start body
    // missing `success` failed strict-client validation).
    #[test]
    fn start_response_satisfies_mcp_output_schema() {
        let body = serde_json::to_value(ExtractStartResponse {
            success: true,
            id: "d1e2f3".to_string(),
            status: "processing".to_string(),
            urls: 2,
        })
        .unwrap();
        let schema = crw_core::mcp::tool_output_schema("crw_extract").unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&body)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "start body vs schema: {errors:#?}");
    }

    // ── prepare_extract: validation, SSRF preflight, template build ────────
    //
    // No real network happens here: `validate_safe_url_resolved` never resolves
    // DNS for a literal IP host, so a public-IP URL like `http://8.8.8.8/x`
    // exercises the "valid URL" branch fully hermetically, and a loopback/
    // private-range literal is rejected by the IP-range check alone.

    fn state_with_llm() -> AppState {
        let config: AppConfig = toml::from_str("[extraction.llm]\napi_key = \"k\"\n").unwrap();
        AppState::new(config).expect("AppState::new failed")
    }

    fn state_no_llm() -> AppState {
        let config: AppConfig = toml::from_str("").unwrap();
        AppState::new(config).expect("AppState::new failed")
    }

    fn state_with_byok_header_guard() -> AppState {
        let config: AppConfig =
            toml::from_str("[extraction.llm]\napi_key = \"k\"\nrequire_byok_header = \"X-Key\"\n")
                .unwrap();
        AppState::new(config).expect("AppState::new failed")
    }

    fn bare_req(urls: Vec<&str>) -> ExtractRequest {
        ExtractRequest {
            urls: urls.into_iter().map(String::from).collect(),
            prompt: None,
            schema: None,
            llm_api_key: None,
            llm_provider: None,
            llm_model: None,
            base_url: None,
            basis: None,
        }
    }

    const PUBLIC_IP_URL: &str = "http://8.8.8.8/page";

    fn err_msg(e: CrwError) -> String {
        e.to_string()
    }

    #[tokio::test]
    async fn empty_urls_rejected_with_a_specific_message() {
        let state = state_with_llm();
        let mut req = bare_req(vec![]);
        req.prompt = Some("summarize".to_string());
        let err = match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        };
        assert!(matches!(err, CrwError::InvalidRequest(_)));
        assert!(err_msg(err).contains("`urls` is required and must be non-empty"));
    }

    #[tokio::test]
    async fn too_many_urls_rejected_with_the_cap_in_the_message() {
        let state = state_with_llm();
        let cap = state.config.crawler.max_extract_urls;
        let mut req = bare_req(vec![PUBLIC_IP_URL; cap + 1]);
        req.prompt = Some("go".to_string());
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("too many urls"));
        assert!(err.contains(&cap.to_string()));
    }

    #[tokio::test]
    async fn no_prompt_and_no_schema_is_rejected() {
        let state = state_with_llm();
        let req = bare_req(vec![PUBLIC_IP_URL]);
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("nothing to extract"));
        assert!(err.contains("prompt"));
        assert!(err.contains("schema"));
    }

    #[tokio::test]
    async fn whitespace_only_prompt_counts_as_absent() {
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("   \n\t  ".to_string());
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("nothing to extract"));
    }

    #[tokio::test]
    async fn prompt_only_succeeds_without_a_schema() {
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("extract the author".to_string());
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert_eq!(prepared.valid_count, 1);
        assert_eq!(prepared.template.formats, vec![OutputFormat::Json]);
        assert_eq!(
            prepared
                .template
                .extract
                .as_ref()
                .unwrap()
                .prompt
                .as_deref(),
            Some("extract the author")
        );
        assert!(prepared.template.json_schema.is_none());
    }

    #[tokio::test]
    async fn schema_only_succeeds_without_a_prompt() {
        let state = state_with_llm();
        let schema = serde_json::json!({"type": "object"});
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.schema = Some(schema.clone());
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert_eq!(prepared.template.json_schema, Some(schema));
        assert!(prepared.template.extract.as_ref().unwrap().prompt.is_none());
    }

    #[tokio::test]
    async fn basis_without_a_schema_is_rejected_with_a_clear_message() {
        // The priority case: a client that asks for per-field attribution but
        // sends no jsonSchema must get a specific, actionable 400 — not a
        // generic parse failure and not a silent no-op.
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        req.basis = Some(true);
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("`basis`"));
        assert!(err.contains("requires a `schema`"));
    }

    #[tokio::test]
    async fn basis_with_a_schema_succeeds() {
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.schema = Some(serde_json::json!({"type": "object"}));
        req.basis = Some(true);
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert!(prepared.template.basis);
    }

    #[tokio::test]
    async fn basis_explicit_false_does_not_require_a_schema() {
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        req.basis = Some(false);
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert!(!prepared.template.basis);
    }

    #[tokio::test]
    async fn base_url_is_rejected_outright() {
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        req.base_url = Some("https://evil.example/v1".to_string());
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("`baseUrl` is not supported"));
    }

    #[tokio::test]
    async fn byok_header_guard_rejects_a_request_without_an_llm_api_key() {
        let state = state_with_byok_header_guard();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("BYOK header guard active"));
    }

    #[tokio::test]
    async fn byok_header_guard_allows_a_request_carrying_an_llm_api_key() {
        let state = state_with_byok_header_guard();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        req.llm_api_key = Some("caller-key".to_string());
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert_eq!(prepared.valid_count, 1);
    }

    #[tokio::test]
    async fn no_server_llm_and_no_byok_key_is_rejected() {
        let state = state_no_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("extraction requires an LLM"));
    }

    #[tokio::test]
    async fn no_server_llm_but_a_byok_key_succeeds() {
        let state = state_no_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        req.llm_api_key = Some("byok".to_string());
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert_eq!(prepared.valid_count, 1);
        assert_eq!(prepared.template.llm_api_key.as_deref(), Some("byok"));
    }

    #[tokio::test]
    async fn a_syntactically_invalid_url_fails_preflight_with_no_valid_urls() {
        let state = state_with_llm();
        let mut req = bare_req(vec!["not a url at all"]);
        req.prompt = Some("go".to_string());
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("no valid URLs"));
    }

    #[tokio::test]
    async fn a_loopback_url_fails_preflight_with_no_valid_urls() {
        let state = state_with_llm();
        let mut req = bare_req(vec!["http://127.0.0.1/admin"]);
        req.prompt = Some("go".to_string());
        let err = err_msg(match prepare_extract(&state, req).await {
            Ok(_) => panic!("expected an error"),
            Err(e) => e,
        });
        assert!(err.contains("no valid URLs"));
    }

    #[tokio::test]
    async fn mixed_valid_and_blocked_urls_keeps_only_the_valid_one_and_preserves_order() {
        let state = state_with_llm();
        let mut req = bare_req(vec!["http://127.0.0.1/blocked", PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert_eq!(prepared.valid_count, 1);
        assert_eq!(prepared.entries.len(), 2);
        assert!(prepared.entries[0].preflight_error.is_some());
        assert_eq!(prepared.entries[0].url, "http://127.0.0.1/blocked");
        assert!(prepared.entries[1].preflight_error.is_none());
        assert_eq!(prepared.entries[1].url, PUBLIC_IP_URL);
    }

    #[tokio::test]
    async fn template_always_requests_json_format_regardless_of_input() {
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert_eq!(prepared.template.formats, vec![OutputFormat::Json]);
    }

    #[tokio::test]
    async fn llm_provider_and_model_thread_through_to_the_template() {
        let state = state_with_llm();
        let mut req = bare_req(vec![PUBLIC_IP_URL]);
        req.prompt = Some("go".to_string());
        req.llm_provider = Some("openai".to_string());
        req.llm_model = Some("gpt-4o".to_string());
        let prepared = prepare_extract(&state, req).await.unwrap();
        assert_eq!(prepared.template.llm_provider.as_deref(), Some("openai"));
        assert_eq!(prepared.template.llm_model.as_deref(), Some("gpt-4o"));
    }

    // ── response shapes: camelCase field pinning ────────────────────────────

    #[test]
    fn extract_url_result_serializes_camel_case_and_omits_none() {
        let result = ExtractUrlResult {
            url: "https://x".to_string(),
            status: "completed".to_string(),
            data: None,
            error: None,
            llm_usage: None,
            basis: None,
            basis_warnings: Vec::new(),
            llm_input_hash: None,
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["url"], "https://x");
        assert_eq!(v["status"], "completed");
        for key in ["data", "error", "llmUsage", "basis", "llmInputHash"] {
            assert!(v.get(key).is_none(), "expected `{key}` omitted");
        }
        // empty Vec also omitted (skip_serializing_if = "Vec::is_empty")
        assert!(v.get("basisWarnings").is_none());
    }

    #[test]
    fn extract_status_response_success_is_false_only_when_failed() {
        for (status, expect_success) in [
            (ExtractStatus::Processing, true),
            (ExtractStatus::Completed, true),
            (ExtractStatus::Cancelling, true),
            (ExtractStatus::Failed, false),
        ] {
            let rec = ExtractRecord {
                status,
                data: None,
                per_url: vec![],
                tokens_used: 0,
                credits_used: 0,
                error: None,
                created_at: std::time::Instant::now(),
                expires_at: std::time::SystemTime::now(),
                claimed_index: None,
            };
            let resp = serialize_extract_status(Uuid::nil(), rec);
            assert_eq!(resp.success, expect_success, "status={status:?}");
        }
    }

    #[test]
    fn extract_status_response_omits_empty_results_array() {
        let rec = ExtractRecord {
            status: ExtractStatus::Processing,
            data: None,
            per_url: vec![],
            tokens_used: 0,
            credits_used: 0,
            error: None,
            created_at: std::time::Instant::now(),
            expires_at: std::time::SystemTime::now(),
            claimed_index: None,
        };
        let resp = serialize_extract_status(Uuid::nil(), rec);
        let v = serde_json::to_value(&resp).unwrap();
        assert!(v.get("results").is_none());
        assert!(v.get("error").is_none());
    }

    #[test]
    fn extract_status_response_credits_and_tokens_pass_through() {
        let rec = ExtractRecord {
            status: ExtractStatus::Completed,
            data: None,
            per_url: vec![],
            tokens_used: 42,
            credits_used: 7,
            error: None,
            created_at: std::time::Instant::now(),
            expires_at: std::time::SystemTime::now(),
            claimed_index: None,
        };
        let resp = serialize_extract_status(Uuid::nil(), rec);
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["creditsUsed"], 7);
        assert_eq!(v["tokensUsed"], 42);
    }
}
