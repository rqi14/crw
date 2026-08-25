//! Shared helpers and limits used by every tool. Keeping these in one place
//! prevents per-tool drift — every tool that accepts `timeout_ms` should call
//! [`clamp_timeout`], every tool that builds a response should go through
//! [`ok_result`] / [`err_result`].

use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;

use crate::errors::{ErrorCode, ErrorResponse, RetryHint};
use crate::response::ToolResponse;
use crate::session::{BrowserSession, RefLookup};

/// Upper bound for per-call `timeout_ms` — anything larger gets clamped. Keeps
/// a rogue client from pinning a CDP session for hours on a typoed value. When
/// the clamp fires, a `warnings` entry is added to the response so the caller
/// knows the effective value differs from what they asked for.
pub const MAX_TIMEOUT_MS: u64 = 120_000;

/// Upper bound for `tree` output size — the AX tree of a big page can blow up
/// LLM context if not capped. Clamped silently to the cap; callers see the
/// effective truncation through `data.node_count > tree line count`.
pub const MAX_TREE_NODES: u32 = 5_000;

/// Maximum byte length of a `goto` URL. RFC doesn't mandate a hard limit but
/// 2048 covers every practical real-world URL and blocks megabyte-sized
/// prompt-injection payloads that would otherwise burn CPU in `url::Url::parse`
/// and flood logs.
pub const MAX_URL_LEN: usize = 2048;

/// URL schemes accepted by `goto`. Everything else — including but not limited
/// to `file://`, `data:`, `javascript:`, `chrome:`, `about:`, `blob:`,
/// `view-source:`, `intent:`, `filesystem:`, `chrome-extension:`, `ws://`,
/// `ftp://` — is rejected with `InvalidArgs` so a prompt-injection payload
/// can't pivot the browser onto the local filesystem, an app-launch URI, or
/// an in-page code-execution protocol. The allowlist is explicit (not
/// blacklist) per OWASP fail-closed guidance.
pub const ALLOWED_GOTO_SCHEMES: &[&str] = &["http", "https"];

/// Cap on the UTF-16 code-unit length of `text` tool output. The tree of a
/// big page can blow up LLM context; this cap matches MCP-friendly response
/// sizes. Counted in JS `.length` units (UTF-16), not bytes — surrogate-pair
/// emoji each cost 2 units. Truncation is applied page-side in `Runtime.evaluate`
/// so we never even shuttle the oversized payload over CDP.
pub const MAX_PAGE_TEXT_LEN: usize = 50_000;

/// Cap on the input length of the `type` tool. 4 KiB is well above any
/// legitimate keystroke sequence; anything larger is almost certainly a
/// prompt-injection payload or a buggy caller. Rejected with `InvalidArgs`.
pub const MAX_TYPE_TEXT_LEN: usize = 4_096;

/// Cap on the UTF-16 code-unit length of `html` tool output. Higher than
/// `MAX_PAGE_TEXT_LEN` because rendered HTML carries markup overhead — a
/// page that produces 50 KB of text often produces 200+ KB of HTML.
/// Truncation applied page-side, same rationale as `MAX_PAGE_TEXT_LEN`.
pub const MAX_HTML_LEN: usize = 200_000;

/// Resolve a `@e<N>` ref to a DOM `backendNodeId`. Returns the id on success,
/// or a ready-to-emit error response on failure:
///
/// - In the map but mapped to `None` → `ELEMENT_NOT_FOUND` (the AX node
///   exists but has no DOM counterpart, e.g. a text fragment or a virtual
///   scrollable group; clicking it is meaningless).
/// - Not in the map, but N ≤ max_ref ever issued → `NODE_STALE` (the ref
///   was valid in a prior snapshot, the page has since navigated/refreshed).
/// - Not in the map, and N > max_ref or unparseable → `NODE_UNKNOWN` (no
///   snapshot ever produced this ref; almost certainly a hallucination or
///   typo). Different recovery: the caller can't just re-`tree`, the LLM
///   needs to actually look at the snapshot output and pick a real ref.
pub(crate) async fn resolve_ref(
    session: &BrowserSession,
    ref_id: &str,
) -> Result<i64, ErrorResponse> {
    match session.lookup_ref(ref_id).await {
        RefLookup::Node(id) => Ok(id),
        RefLookup::NoDomNode => Err(ErrorResponse::new(
            ErrorCode::ElementNotFound,
            format!("ref {ref_id} resolves to an AX node with no DOM mapping"),
        )),
        RefLookup::Unknown => {
            let max = session.max_ref();
            let parsed = crate::session::parse_ref_index(ref_id);
            let is_known_range = parsed.is_some_and(|n| n >= 1 && n <= max);
            if is_known_range {
                Err(ErrorResponse::new(
                    ErrorCode::NodeStale,
                    format!(
                        "ref {ref_id} is from an older snapshot (the ref map \
                         was replaced by a later `tree` call or cleared on \
                         navigation) — call `tree` again to get fresh refs"
                    ),
                )
                .with_retry(RetryHint::Snapshot))
            } else {
                // Hint depends on cause: only "no snapshot yet" benefits from
                // a `tree` retry. A ref that exceeds the issued max OR a
                // malformed `@eN` is a typo / hallucination — re-snapshotting
                // won't surface it. Returning `RetryHint::None` for those
                // signals "fix your ref" instead of looping forever.
                let (detail, hint) = match parsed {
                    Some(n) if max == 0 => (
                        format!(
                            "ref {ref_id} requested but no `tree` snapshot has been taken yet (n={n})"
                        ),
                        RetryHint::Snapshot,
                    ),
                    Some(n) => (
                        format!(
                            "ref {ref_id} (n={n}) exceeds the highest ref ever issued in this session (max={max})"
                        ),
                        RetryHint::None,
                    ),
                    None => (
                        format!("ref {ref_id:?} is not a valid `@e<N>` ref"),
                        RetryHint::None,
                    ),
                };
                Err(ErrorResponse::new(ErrorCode::NodeUnknown, detail).with_retry(hint))
            }
        }
    }
}

pub(crate) fn ok_result<T: serde::Serialize>(resp: &ToolResponse<T>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(resp.to_json())])
}

pub(crate) fn err_result(err: &ErrorResponse) -> CallToolResult {
    let mut result = CallToolResult::success(vec![ContentBlock::text(err.to_json())]);
    result.is_error = Some(true);
    result
}

/// Apply [`MAX_TIMEOUT_MS`] to a caller-supplied `timeout_ms` and return both
/// the effective `Duration` and a flag indicating whether clamping occurred.
/// Pure — unit-testable without a CDP connection.
pub(crate) fn clamp_timeout(timeout_ms: Option<u64>, default: Duration) -> (Duration, bool) {
    // Defence-in-depth: the cap applies even to the config-sourced default.
    // `BrowseConfig::page_timeout` is `pub` so an embedder could construct an
    // oversized default; without this floor the `None` branch would silently
    // bypass `MAX_TIMEOUT_MS`.
    let cap = Duration::from_millis(MAX_TIMEOUT_MS);
    match timeout_ms {
        Some(ms) => {
            let clamped = ms > MAX_TIMEOUT_MS;
            let effective = ms.min(MAX_TIMEOUT_MS);
            (Duration::from_millis(effective), clamped)
        }
        None => (default.min(cap), false),
    }
}

/// Apply [`MAX_TREE_NODES`] to a caller-supplied `max_nodes`, defaulting to
/// [`DEFAULT_TREE_NODES`] when unset. Returns both the effective value and
/// a clamp flag. Pure.
pub(crate) fn clamp_max_nodes(max_nodes: Option<u32>) -> (u32, bool) {
    match max_nodes {
        Some(n) => {
            let clamped = n > MAX_TREE_NODES;
            (n.min(MAX_TREE_NODES), clamped)
        }
        None => (DEFAULT_TREE_NODES, false),
    }
}

/// Default `max_nodes` when the caller doesn't specify. Bumped from 500 to
/// 1500 in v0.4.1 after R3 dogfood: modern docs SPAs (react.dev, MDN) put the
/// nav sidebar past index 500, so the default-clamped tree often missed the
/// link the LLM was looking for. 1500 covers the sidebar-plus-content slice
/// of every site we measured while staying well under the 5000 hard cap.
pub(crate) const DEFAULT_TREE_NODES: u32 = 1_500;

/// Emit a `SessionClosed` error with `RetryHint::NewSession` when no session
/// has been opened yet. The retry hint tells the LLM to call `goto` (which
/// auto-creates the default session) rather than ping `tree`/`text` again
/// hoping the *element* will appear.
pub(crate) fn no_session_err() -> ErrorResponse {
    ErrorResponse::new(
        ErrorCode::SessionClosed,
        "no session yet — call `goto` first",
    )
    .with_retry(RetryHint::NewSession)
}

/// Same shape as [`no_session_err`] but for the case where the session
/// exists but `ensure_attached` hasn't run yet (no CDP target id). In
/// practice this only happens if a tool is called between session creation
/// and the first `goto` — the retry hint is the same: open a new session
/// (or just call `goto`, which attaches lazily).
pub(crate) fn no_target_err() -> ErrorResponse {
    ErrorResponse::new(
        ErrorCode::SessionClosed,
        "session has no attached target — call `goto` first",
    )
    .with_retry(RetryHint::NewSession)
}

/// Validate the `selector`/`ref` pair carried by every targeted tool
/// (`click`, `fill`, etc.). Returns an `ErrorResponse` to surface and short-
/// circuit on; `None` means the inputs are well-formed.
///
/// Two failure modes:
/// 1. Both unset, or both set → `NoSelector` (recovery: pick exactly one).
/// 2. `selector` set but empty string → `InvalidArgs`
///    (recovery: send a real CSS selector or use `ref`).
///
/// Pulled out of each tool's `handle()` so the contract is stated once and
/// can be unit-tested without spinning up a session.
pub(crate) fn validate_selector_or_ref(
    selector: Option<&str>,
    ref_id: Option<&str>,
) -> Option<ErrorResponse> {
    if selector.is_some() == ref_id.is_some() {
        return Some(ErrorResponse::new(
            ErrorCode::NoSelector,
            "exactly one of `selector` or `ref` is required",
        ));
    }
    if let Some(s) = selector
        && s.is_empty()
    {
        return Some(ErrorResponse::new(
            ErrorCode::InvalidArgs,
            "selector must not be empty",
        ));
    }
    if let Some(r) = ref_id
        && r.is_empty()
    {
        return Some(ErrorResponse::new(
            ErrorCode::InvalidArgs,
            "ref must not be empty",
        ));
    }
    None
}

/// Outcome of a `Runtime.evaluate` call. Distinguishes "the JS threw" from
/// "the CDP transport failed" — they map to different error codes
/// (`InvalidExpression` vs `CdpError`) and the LLM should react differently.
pub(crate) enum EvalOutcome {
    /// Expression returned cleanly. `value` is the `result.value` field
    /// (`returnByValue=true`); `description` is `result.description` for
    /// non-primitive returns where `value` is absent.
    Ok {
        value: Option<Value>,
        description: Option<String>,
    },
    /// Expression itself threw. Carries the human-readable exception message.
    Threw(String),
}

/// Map a `DOM.resolveNode` transport error string into the right structured
/// response. Chromium phrases "the backend node id is no longer attached to a
/// document" several different ways depending on whether the document was
/// swapped, the node was detached mid-call, or the id was never valid. All of
/// those collapse into `NODE_STALE` for the caller — re-snapshot is the
/// correct recovery in every case. Anything else stays `CDP_ERROR` so a real
/// transport bug doesn't masquerade as user error.
fn map_resolve_node_error(ref_id: &str, error_msg: &str) -> ErrorResponse {
    let lower = error_msg.to_ascii_lowercase();
    let is_stale = lower.contains("does not belong to the document")
        || lower.contains("could not find node")
        || lower.contains("no node with given id")
        || lower.contains("node with given id");
    if is_stale {
        ErrorResponse::new(
            ErrorCode::NodeStale,
            format!("ref {ref_id} no longer attached to the current document — call `tree` again"),
        )
        .with_retry(RetryHint::Snapshot)
    } else {
        ErrorResponse::new(
            ErrorCode::CdpError,
            format!("DOM.resolveNode failed: {error_msg}"),
        )
    }
}

/// Resolve a `@e<N>` ref into a `Runtime.RemoteObject` `objectId` — the
/// handle CDP needs to call methods on the element via
/// `Runtime.callFunctionOn`. Two-step: ref → backendNodeId → DOM.resolveNode.
pub(crate) async fn ref_to_object_id(
    session: &BrowserSession,
    cdp_sid: &str,
    ref_id: &str,
    timeout: Duration,
) -> Result<String, ErrorResponse> {
    let backend_id = resolve_ref(session, ref_id).await?;
    let resp = session
        .conn
        .send_recv(
            "DOM.resolveNode",
            serde_json::json!({ "backendNodeId": backend_id }),
            Some(cdp_sid),
            timeout,
        )
        .await
        .map_err(|e| map_resolve_node_error(ref_id, &e.to_string()))?;
    let object_id = resp
        .get("object")
        .and_then(|o| o.get("objectId"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            ErrorResponse::new(
                ErrorCode::ElementNotFound,
                format!("ref {ref_id} backend node has no resolvable RemoteObject"),
            )
        })?;
    Ok(object_id.to_string())
}

/// Call a JavaScript function on a previously resolved `objectId` (the `this`
/// of the function body). `function_declaration` must be a complete function
/// expression, e.g. `"function(v) { this.value = v; }"`. `arguments_json` is
/// the literal JSON array passed as `arguments`.
pub(crate) async fn call_function_on(
    session: &BrowserSession,
    cdp_sid: &str,
    object_id: &str,
    function_declaration: &str,
    arguments: serde_json::Value,
    timeout: Duration,
) -> Result<EvalOutcome, ErrorResponse> {
    let resp = session
        .conn
        .send_recv(
            "Runtime.callFunctionOn",
            serde_json::json!({
                "objectId": object_id,
                "functionDeclaration": function_declaration,
                "arguments": arguments,
                "returnByValue": true,
                "awaitPromise": true,
            }),
            Some(cdp_sid),
            timeout,
        )
        .await
        .map_err(|e| {
            ErrorResponse::new(
                ErrorCode::CdpError,
                format!("Runtime.callFunctionOn failed: {e}"),
            )
        })?;

    if let Some(exc) = resp.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|v| v.get("description").or_else(|| v.get("value")))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| exc.get("text").and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_else(|| "callFunctionOn threw".to_string());
        return Ok(EvalOutcome::Threw(msg));
    }
    let result = resp.get("result");
    let value = result.and_then(|r| r.get("value")).cloned();
    let description = result
        .and_then(|r| r.get("description"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(EvalOutcome::Ok { value, description })
}

/// Release a `Runtime.RemoteObject` `objectId` previously obtained via
/// [`ref_to_object_id`]. Call this in tool cleanup paths so the page-side
/// object table doesn't accumulate stale handles for the duration of the
/// session. Errors are deliberately swallowed: by the time we're releasing
/// the handle the tool's success/error has already been determined, and a
/// failed release shouldn't change what the LLM sees. Logged at `debug`
/// level for forensics.
pub(crate) async fn release_object_id(
    session: &BrowserSession,
    cdp_sid: &str,
    object_id: &str,
    timeout: Duration,
) {
    let res = session
        .conn
        .send_recv(
            "Runtime.releaseObject",
            serde_json::json!({ "objectId": object_id }),
            Some(cdp_sid),
            timeout,
        )
        .await;
    if let Err(e) = res {
        tracing::debug!(error = %e, "Runtime.releaseObject failed (non-fatal)");
    }
}

/// Run `Runtime.evaluate` against the session's current target with
/// `returnByValue=true` and `awaitPromise=true`. Centralises the JSON
/// boilerplate so every tool that needs to poke the page (text, html,
/// evaluate, fill, storage…) shares one CDP shape.
pub(crate) async fn runtime_evaluate(
    session: &BrowserSession,
    cdp_sid: &str,
    expression: &str,
    timeout: Duration,
) -> Result<EvalOutcome, ErrorResponse> {
    let resp = session
        .conn
        .send_recv(
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
            Some(cdp_sid),
            timeout,
        )
        .await
        .map_err(|e| {
            ErrorResponse::new(ErrorCode::CdpError, format!("Runtime.evaluate failed: {e}"))
        })?;

    if let Some(exc) = resp.get("exceptionDetails") {
        let msg = exc
            .get("exception")
            .and_then(|v| v.get("description").or_else(|| v.get("value")))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| exc.get("text").and_then(|v| v.as_str()).map(String::from))
            .unwrap_or_else(|| "expression threw".to_string());
        return Ok(EvalOutcome::Threw(msg));
    }

    let result = resp.get("result");
    let value = result.and_then(|r| r.get("value")).cloned();
    let description = result
        .and_then(|r| r.get("description"))
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(EvalOutcome::Ok { value, description })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_timeout_preserves_in_range() {
        let (d, clamped) = clamp_timeout(Some(30_000), Duration::from_secs(10));
        assert_eq!(d, Duration::from_millis(30_000));
        assert!(!clamped);
    }

    #[test]
    fn clamp_timeout_caps_excessive() {
        let (d, clamped) = clamp_timeout(Some(999_999_999), Duration::from_secs(10));
        assert_eq!(d, Duration::from_millis(MAX_TIMEOUT_MS));
        assert!(clamped);
    }

    #[test]
    fn clamp_timeout_none_uses_default() {
        let default = Duration::from_secs(45);
        let (d, clamped) = clamp_timeout(None, default);
        assert_eq!(d, default);
        assert!(!clamped);
    }

    #[test]
    fn clamp_max_nodes_default_when_unset() {
        let (n, clamped) = clamp_max_nodes(None);
        assert_eq!(n, DEFAULT_TREE_NODES);
        assert!(!clamped);
    }

    #[test]
    fn clamp_max_nodes_preserves_in_range() {
        let (n, clamped) = clamp_max_nodes(Some(1000));
        assert_eq!(n, 1000);
        assert!(!clamped);
    }

    #[test]
    fn clamp_max_nodes_caps_excessive() {
        let (n, clamped) = clamp_max_nodes(Some(u32::MAX));
        assert_eq!(n, MAX_TREE_NODES);
        assert!(clamped);
    }

    #[test]
    fn clamp_timeout_at_exact_cap_is_not_clamped() {
        let (d, clamped) = clamp_timeout(Some(MAX_TIMEOUT_MS), Duration::from_secs(10));
        assert_eq!(d, Duration::from_millis(MAX_TIMEOUT_MS));
        assert!(!clamped, "exact cap should pass through");
    }

    #[test]
    fn clamp_max_nodes_at_exact_cap_is_not_clamped() {
        let (n, clamped) = clamp_max_nodes(Some(MAX_TREE_NODES));
        assert_eq!(n, MAX_TREE_NODES);
        assert!(!clamped, "exact cap should pass through");
    }

    #[test]
    fn resolve_node_error_maps_stale_phrases() {
        for stale in [
            "Node with given id does not belong to the document",
            "Could not find node with given id",
            "No node with given id found",
            "node with given id (123) is gone",
        ] {
            let err = map_resolve_node_error("@e5", stale);
            assert_eq!(
                err.code,
                ErrorCode::NodeStale,
                "expected NODE_STALE for: {stale}"
            );
            assert_eq!(err.retry, Some(RetryHint::Snapshot));
        }
    }

    #[test]
    fn resolve_node_error_passes_through_real_cdp_errors() {
        for real in [
            "WebSocket connection closed unexpectedly",
            "timeout waiting for CDP response",
            "Internal error: out of memory",
        ] {
            let err = map_resolve_node_error("@e5", real);
            assert_eq!(
                err.code,
                ErrorCode::CdpError,
                "expected CDP_ERROR for: {real}"
            );
        }
    }

    #[test]
    fn clamp_timeout_none_floors_oversized_default() {
        let oversized = Duration::from_millis(MAX_TIMEOUT_MS * 10);
        let (d, clamped) = clamp_timeout(None, oversized);
        assert_eq!(d, Duration::from_millis(MAX_TIMEOUT_MS));
        assert!(!clamped);
    }

    #[test]
    fn validate_selector_or_ref_rejects_neither_set() {
        let err = validate_selector_or_ref(None, None).expect("must error");
        assert_eq!(err.code, ErrorCode::NoSelector);
    }

    #[test]
    fn validate_selector_or_ref_rejects_both_set() {
        let err = validate_selector_or_ref(Some("#x"), Some("@e1")).expect("must error");
        assert_eq!(err.code, ErrorCode::NoSelector);
    }

    #[test]
    fn validate_selector_or_ref_rejects_empty_selector() {
        let err = validate_selector_or_ref(Some(""), None).expect("must error");
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("must not be empty"));
    }

    #[test]
    fn validate_selector_or_ref_accepts_real_selector() {
        assert!(validate_selector_or_ref(Some("#submit"), None).is_none());
    }

    #[test]
    fn validate_selector_or_ref_accepts_real_ref() {
        assert!(validate_selector_or_ref(None, Some("@e3")).is_none());
    }

    #[test]
    fn validate_selector_or_ref_rejects_empty_ref() {
        // R4 (API contract review) flagged the previous "empty ref slips
        // through to resolve_ref" behavior as asymmetric: empty selector
        // returned `INVALID_ARGS` early, but empty ref returned a delayed
        // `NODE_UNKNOWN` from resolve_ref. We now reject both early so
        // the contract is symmetric.
        let err = validate_selector_or_ref(None, Some("")).expect("must error");
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        assert!(err.message.contains("ref"));
    }

    // -- additional clamp_timeout / clamp_max_nodes boundaries --------------

    #[test]
    fn clamp_timeout_zero_is_preserved_uncapped() {
        let (d, clamped) = clamp_timeout(Some(0), Duration::from_secs(10));
        assert_eq!(d, Duration::from_millis(0));
        assert!(!clamped);
    }

    #[test]
    fn clamp_timeout_one_over_cap_is_clamped() {
        let (d, clamped) = clamp_timeout(Some(MAX_TIMEOUT_MS + 1), Duration::from_secs(10));
        assert_eq!(d, Duration::from_millis(MAX_TIMEOUT_MS));
        assert!(clamped);
    }

    #[test]
    fn clamp_timeout_none_at_exact_cap_default_is_not_clamped() {
        let default = Duration::from_millis(MAX_TIMEOUT_MS);
        let (d, clamped) = clamp_timeout(None, default);
        assert_eq!(d, default);
        assert!(!clamped);
    }

    #[test]
    fn clamp_max_nodes_zero_is_preserved_uncapped() {
        let (n, clamped) = clamp_max_nodes(Some(0));
        assert_eq!(n, 0);
        assert!(!clamped);
    }

    #[test]
    fn clamp_max_nodes_one_over_cap_is_clamped() {
        let (n, clamped) = clamp_max_nodes(Some(MAX_TREE_NODES + 1));
        assert_eq!(n, MAX_TREE_NODES);
        assert!(clamped);
    }

    #[test]
    fn default_tree_nodes_is_1500() {
        // Documents the R3-dogfood-derived default (bumped from 500). A
        // silent regression here would shrink every unclamped `tree` call.
        assert_eq!(DEFAULT_TREE_NODES, 1_500);
    }

    // -- no_session_err / no_target_err --------------------------------------

    #[test]
    fn no_session_err_shape() {
        let err = no_session_err();
        assert_eq!(err.code, ErrorCode::SessionClosed);
        assert_eq!(err.retry, Some(RetryHint::NewSession));
        assert!(err.message.contains("goto"));
    }

    #[test]
    fn no_target_err_shape() {
        let err = no_target_err();
        assert_eq!(err.code, ErrorCode::SessionClosed);
        assert_eq!(err.retry, Some(RetryHint::NewSession));
        assert!(err.message.contains("goto"));
    }

    #[test]
    fn no_session_and_no_target_err_messages_differ() {
        // Both share a code/retry shape but the message must distinguish
        // "never had a session" from "session exists, never attached" —
        // otherwise the two distinct call sites collapse into one string
        // and debugging which branch fired requires re-reading the code.
        assert_ne!(no_session_err().message, no_target_err().message);
    }

    // -- ok_result / err_result -----------------------------------------------

    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .first()
            .and_then(|c| c.as_text().map(|t| t.text.clone()))
            .expect("expected text content block")
    }

    #[derive(serde::Serialize)]
    struct DummyData {
        n: u32,
    }

    #[test]
    fn ok_result_is_not_flagged_as_error() {
        let resp = ToolResponse::new("s1", None, DummyData { n: 1 });
        let result = ok_result(&resp);
        assert_eq!(result.is_error, Some(false));
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["data"]["n"], 1);
    }

    #[test]
    fn err_result_is_flagged_as_error() {
        let err = ErrorResponse::new(ErrorCode::InvalidArgs, "bad input");
        let result = err_result(&err);
        assert_eq!(result.is_error, Some(true));
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["code"], "INVALID_ARGS");
        assert_eq!(json["message"], "bad input");
    }

    // -- map_resolve_node_error: broader phrase coverage ---------------------

    #[test]
    fn resolve_node_error_stale_match_is_case_insensitive() {
        let err =
            map_resolve_node_error("@e9", "NODE WITH GIVEN ID Does Not Belong To The Document");
        assert_eq!(err.code, ErrorCode::NodeStale);
    }

    #[test]
    fn resolve_node_error_context_error_is_not_misclassified_as_stale() {
        // A destroyed execution context is a different failure mode than a
        // detached DOM node — it must stay CDP_ERROR so the caller doesn't
        // get told "re-snapshot" when re-snapshotting won't help.
        let err = map_resolve_node_error("@e9", "Cannot find context with specified id");
        assert_eq!(err.code, ErrorCode::CdpError);
    }

    #[test]
    fn resolve_node_error_stale_message_preserves_ref_id() {
        let err = map_resolve_node_error("@e123", "no node with given id");
        assert!(err.message.contains("@e123"));
    }

    // -- validate_selector_or_ref: documents current whitespace behavior ----

    #[test]
    fn validate_selector_or_ref_accepts_whitespace_only_selector() {
        // The function only checks `.is_empty()`, not `.trim().is_empty()` —
        // a whitespace-only selector passes validation here and would fail
        // later as an invalid CSS selector at the CDP layer. Documenting the
        // current (permissive) behavior rather than assuming it's a bug.
        assert!(validate_selector_or_ref(Some("   "), None).is_none());
    }

    #[test]
    fn validate_selector_or_ref_accepts_whitespace_only_ref() {
        assert!(validate_selector_or_ref(None, Some("   ")).is_none());
    }

    #[test]
    fn validate_selector_or_ref_accepts_unicode_selector() {
        // `:contains()` isn't real CSS, but validate_selector_or_ref does no
        // CSS parsing — any non-empty string is a well-formed input to it.
        assert!(validate_selector_or_ref(Some("button:has-text(\"確認\")"), None).is_none());
    }

    #[test]
    fn validate_selector_or_ref_accepts_ref_without_at_prefix() {
        // validate_selector_or_ref doesn't validate the `@e<N>` shape itself
        // — that's `resolve_ref`'s job via `parse_ref_index`. A malformed
        // ref like "e5" (missing `@`) passes this gate and fails later with
        // NODE_UNKNOWN, not here.
        assert!(validate_selector_or_ref(None, Some("e5")).is_none());
    }

    // -- more clamp_timeout / clamp_max_nodes parameter space ---------------

    #[test]
    fn clamp_timeout_u64_max_is_clamped() {
        let (d, clamped) = clamp_timeout(Some(u64::MAX), Duration::from_secs(10));
        assert_eq!(d, Duration::from_millis(MAX_TIMEOUT_MS));
        assert!(clamped);
    }

    #[test]
    fn clamp_timeout_smallest_nonzero_value_is_preserved() {
        let (d, clamped) = clamp_timeout(Some(1), Duration::from_secs(10));
        assert_eq!(d, Duration::from_millis(1));
        assert!(!clamped);
    }

    #[test]
    fn clamp_timeout_none_zero_default_is_preserved() {
        let (d, clamped) = clamp_timeout(None, Duration::from_millis(0));
        assert_eq!(d, Duration::from_millis(0));
        assert!(!clamped);
    }

    #[test]
    fn clamp_timeout_none_default_one_over_cap_is_floored() {
        let (d, clamped) = clamp_timeout(None, Duration::from_millis(MAX_TIMEOUT_MS + 1));
        assert_eq!(d, Duration::from_millis(MAX_TIMEOUT_MS));
        // `None` never sets the `clamped` flag — the caller didn't ask for a
        // value, so there's nothing to warn them about being adjusted.
        assert!(!clamped);
    }

    #[test]
    fn clamp_max_nodes_smallest_nonzero_value_is_preserved() {
        let (n, clamped) = clamp_max_nodes(Some(1));
        assert_eq!(n, 1);
        assert!(!clamped);
    }

    #[test]
    fn clamp_max_nodes_u32_max_is_clamped_to_cap() {
        let (n, clamped) = clamp_max_nodes(Some(u32::MAX));
        assert_eq!(n, MAX_TREE_NODES);
        assert!(clamped);
    }

    // -- map_resolve_node_error: one phrase per test for clear failure output

    #[test]
    fn resolve_node_error_does_not_belong_to_document_is_stale() {
        let err = map_resolve_node_error("@e1", "Node does not belong to the document");
        assert_eq!(err.code, ErrorCode::NodeStale);
        assert_eq!(err.retry, Some(RetryHint::Snapshot));
    }

    #[test]
    fn resolve_node_error_could_not_find_node_is_stale() {
        let err = map_resolve_node_error("@e1", "Could not find node with given id");
        assert_eq!(err.code, ErrorCode::NodeStale);
    }

    #[test]
    fn resolve_node_error_no_node_with_given_id_is_stale() {
        let err = map_resolve_node_error("@e1", "No node with given id");
        assert_eq!(err.code, ErrorCode::NodeStale);
    }

    #[test]
    fn resolve_node_error_websocket_closed_is_cdp_error() {
        let err = map_resolve_node_error("@e1", "WebSocket connection closed unexpectedly");
        assert_eq!(err.code, ErrorCode::CdpError);
        // Non-stale errors carry no retry hint from this mapper.
        assert_eq!(err.retry, None);
    }

    #[test]
    fn resolve_node_error_timeout_is_cdp_error() {
        let err = map_resolve_node_error("@e1", "timeout waiting for CDP response");
        assert_eq!(err.code, ErrorCode::CdpError);
    }

    #[test]
    fn resolve_node_error_out_of_memory_is_cdp_error() {
        let err = map_resolve_node_error("@e1", "Internal error: out of memory");
        assert_eq!(err.code, ErrorCode::CdpError);
    }

    #[test]
    fn resolve_node_error_empty_message_is_cdp_error() {
        let err = map_resolve_node_error("@e1", "");
        assert_eq!(err.code, ErrorCode::CdpError);
    }

    #[test]
    fn resolve_node_error_cdp_message_includes_original_text() {
        let err = map_resolve_node_error("@e1", "some transport failure");
        assert!(err.message.contains("some transport failure"));
        assert!(err.message.contains("DOM.resolveNode failed"));
    }

    // -- ok_result / err_result: envelope fidelity ---------------------------

    #[test]
    fn ok_result_round_trips_warnings_and_title() {
        let resp = ToolResponse::new("s1", Some("https://example.com".into()), DummyData { n: 9 })
            .with_title("Example")
            .with_navigated(true)
            .with_elapsed_ms(42)
            .with_warning("clamped");
        let result = ok_result(&resp);
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(json["title"], "Example");
        assert_eq!(json["navigated"], true);
        assert_eq!(json["elapsed_ms"], 42);
        assert_eq!(json["warnings"][0], "clamped");
    }

    #[test]
    fn err_result_carries_retry_hint() {
        let err = ErrorResponse::new(ErrorCode::Timeout, "took too long")
            .with_retry(RetryHint::BackoffMs(500));
        let result = err_result(&err);
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(json["retry"]["backoff_ms"], 500);
    }

    #[test]
    fn err_result_omits_absent_optional_fields() {
        let err = ErrorResponse::new(ErrorCode::NotFound, "missing");
        let result = err_result(&err);
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        let obj = json.as_object().unwrap();
        assert!(!obj.contains_key("retry"));
        assert!(!obj.contains_key("stale_anchor"));
        assert!(!obj.contains_key("allowed_pattern"));
        assert!(!obj.contains_key("partial_count"));
    }

    // -- shared limits: regression guards on public contract constants ------
    // Each of these numbers is depended on by other tools/docs; a silent
    // change here would be a breaking behavior change nobody asked for.

    #[test]
    fn const_max_timeout_ms_is_120_seconds() {
        assert_eq!(MAX_TIMEOUT_MS, 120_000);
    }

    #[test]
    fn const_max_tree_nodes_is_5000() {
        assert_eq!(MAX_TREE_NODES, 5_000);
    }

    #[test]
    fn const_max_url_len_is_2048() {
        assert_eq!(MAX_URL_LEN, 2048);
    }

    #[test]
    fn const_allowed_goto_schemes_is_http_and_https_only() {
        assert_eq!(ALLOWED_GOTO_SCHEMES, &["http", "https"]);
    }

    #[test]
    fn const_max_page_text_len_is_50000() {
        assert_eq!(MAX_PAGE_TEXT_LEN, 50_000);
    }

    #[test]
    fn const_max_type_text_len_is_4096() {
        assert_eq!(MAX_TYPE_TEXT_LEN, 4_096);
    }

    #[test]
    fn const_max_html_len_is_200000() {
        assert_eq!(MAX_HTML_LEN, 200_000);
    }

    // -- map_resolve_node_error: substring matching must not be over-broad --

    #[test]
    fn resolve_node_error_stale_phrase_embedded_in_longer_message() {
        let err = map_resolve_node_error(
            "@e7",
            "DOM.resolveNode: no node with given id 42 in this frame",
        );
        assert_eq!(err.code, ErrorCode::NodeStale);
    }

    #[test]
    fn resolve_node_error_similar_but_non_matching_phrase_stays_cdp_error() {
        // Contains "Node" and "id" individually, but not any of the four
        // recognised stale phrases — must not false-positive into NODE_STALE.
        let err = map_resolve_node_error("@e7", "Node ID out of range for this document");
        assert_eq!(err.code, ErrorCode::CdpError);
    }

    // -- validate_selector_or_ref: length has no cap in this function -------

    #[test]
    fn validate_selector_or_ref_accepts_very_long_selector() {
        let long = "a".repeat(5_000);
        assert!(validate_selector_or_ref(Some(&long), None).is_none());
    }

    // -- ok_result / err_result: content-block shape and generic payloads ---

    #[test]
    fn ok_result_produces_exactly_one_text_content_block() {
        let resp = ToolResponse::new("s1", None, DummyData { n: 1 });
        let result = ok_result(&resp);
        assert_eq!(result.content.len(), 1);
        assert!(result.content[0].as_text().is_some());
    }

    #[test]
    fn err_result_produces_exactly_one_text_content_block() {
        let err = ErrorResponse::new(ErrorCode::Internal, "boom");
        let result = err_result(&err);
        assert_eq!(result.content.len(), 1);
        assert!(result.content[0].as_text().is_some());
    }

    #[test]
    fn ok_result_passes_through_arbitrary_vec_payload() {
        let resp = ToolResponse::new("s1", None, vec![1, 2, 3]);
        let result = ok_result(&resp);
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(json["data"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn ok_result_url_none_is_omitted_from_json() {
        let resp = ToolResponse::new("s1", None, DummyData { n: 1 });
        let result = ok_result(&resp);
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert!(!json.as_object().unwrap().contains_key("url"));
    }

    #[test]
    fn ok_result_url_some_is_present_in_json() {
        let resp = ToolResponse::new("s1", Some("https://a.example/".into()), DummyData { n: 1 });
        let result = ok_result(&resp);
        let json: Value = serde_json::from_str(&text_of(&result)).unwrap();
        assert_eq!(json["url"], "https://a.example/");
    }
}
