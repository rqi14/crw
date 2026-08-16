//! Shared diagnostics for `crw doctor` and `crw smoke`.
//!
//! Both commands need the same three things: which backend (local engine,
//! hosted API, or MCP) a bare invocation should diagnose, a way to report a
//! check result consistently, and, for local checks, the exact same
//! capability facts `GET /v1/capabilities` reports. Keeping all three here
//! is what stops doctor and smoke from ever disagreeing with each other or
//! with the REST route about what this build/config can actually do.

use clap::ValueEnum;
use crw_core::config::AppConfig;
use serde::Serialize;

/// Which backend a diagnostic run targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    /// The embedded local engine: renderer, search backend, listen port.
    Local,
    /// A configured hosted API (`[client].api_url`, written by `crw setup --cloud`).
    Cloud,
    /// The MCP surface: embedded or proxy, whichever `crw mcp` would use.
    Mcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    /// The check could not run at all: a build/config limitation, not a
    /// failure of the thing being checked.
    Skip,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub id: &'static str,
    pub status: Status,
    pub message: String,
    /// Corrective action. Always present on Warn/Fail, never on Pass/Skip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl CheckResult {
    pub fn pass(id: &'static str, message: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Pass,
            message: message.into(),
            fix: None,
        }
    }

    pub fn warn(id: &'static str, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Warn,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }

    pub fn fail(id: &'static str, message: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Fail,
            message: message.into(),
            fix: Some(fix.into()),
        }
    }

    pub fn skip(id: &'static str, message: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Skip,
            message: message.into(),
            fix: None,
        }
    }
}

/// Resolve which backend a bare (no `--target`) invocation should diagnose.
///
/// `crw` is local-first: a fresh install with no `[client].api_url` and no
/// configured local renderer resolves to `Local`. A configured
/// `[client].api_url` (written by `crw setup --cloud`) with no local
/// renderer configured resolves to `Cloud`. Configuring both at once is
/// ambiguous: there is no way to guess which one the caller wants
/// diagnosed, so the caller must say so with `--target`.
pub fn resolve_target(config: &AppConfig, requested: Option<Target>) -> Result<Target, String> {
    if let Some(t) = requested {
        return Ok(t);
    }
    // The public one-shot override is itself an explicit mode selection. It
    // must beat renderer sections inherited from config.default.toml; asking
    // the user to add `--target cloud` after they already exported
    // CRW_API_URL defeats the shared resolver contract.
    if std::env::var("CRW_API_URL")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Ok(Target::Cloud);
    }
    let cloud_configured = config.client.api_url.is_some();
    let local_configured = config.renderer.chrome.is_some()
        || config.renderer.lightpanda.is_some()
        || config.renderer.chrome_proxy.is_some()
        || config.renderer.playwright.is_some()
        || config.renderer.camoufox.is_some();
    match (cloud_configured, local_configured) {
        (true, true) => Err(
            "both a cloud API ([client].api_url) and a local renderer are configured; \
             pass --target local or --target cloud to pick one"
                .to_string(),
        ),
        (true, false) => Ok(Target::Cloud),
        (false, _) => Ok(Target::Local),
    }
}

/// Strip userinfo from a URL before it is ever printed. Falls back to a
/// fixed placeholder for a string that doesn't parse as a URL rather than
/// echoing it back unredacted.
pub fn redact_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => "<unparseable url, redacted>".to_string(),
    }
}

/// Whether a configured remote URL is the hosted, credit-metered fastCRW API.
/// Custom URLs are self-hosted by default: they may be unauthenticated and
/// must not trigger Cloud-only API-key or billing requirements.
pub fn is_managed_api_url(raw: &str) -> bool {
    url::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|host| host == "api.fastcrw.com")
}

/// Attempt a TCP connection to the host:port encoded in a URL, with a bound
/// on how long to wait. Used for reachability checks only: never sends any
/// request body, so it cannot itself be billable.
pub async fn tcp_reachable(raw_url: &str, timeout: std::time::Duration) -> Result<(), ()> {
    let parsed = url::Url::parse(raw_url).map_err(|_| ())?;
    let host = parsed.host_str().ok_or(())?;
    let port = parsed.port_or_known_default().ok_or(())?;
    tokio::time::timeout(timeout, tokio::net::TcpStream::connect((host, port)))
        .await
        .map_err(|_| ())?
        .map(|_| ())
        .map_err(|_| ())
}

/// Mirrors `AppConfig::load()`'s own precedence order (see its doc comment)
/// as a read-only inspection: no new resolver, just re-checking the same
/// paths `load()` already probes, so this can never drift from what a real
/// `crw serve` / `crw scrape` actually reads.
pub fn describe_config_layers() -> Vec<String> {
    let mut layers = Vec::new();
    if std::path::Path::new("config.default.toml").exists() {
        layers.push("config.default.toml (cwd)".to_string());
    }
    if let Some(p) = crw_core::config::user_config_path()
        && p.exists()
    {
        layers.push(p.display().to_string());
    }
    if let Ok(extra) = std::env::var("CRW_CONFIG") {
        layers.push(format!("{extra} (via $CRW_CONFIG)"));
    } else if std::path::Path::new("config.local.toml").exists() {
        layers.push("config.local.toml (cwd)".to_string());
    }
    layers
}

/// A local capability snapshot, computed the same way `GET /v1/capabilities`
/// does, but in-process with no HTTP listener ever bound. This is the ONE
/// place doctor and smoke build an `AppState` from a resolved config, so the
/// two commands can't derive capability facts differently from each other or
/// from the REST route.
#[cfg(feature = "mcp-embedded")]
pub async fn local_capabilities(
    config: AppConfig,
) -> Result<crw_server::routes::capabilities::Capabilities, String> {
    let state = crw_server::state::AppState::new(config)
        .map_err(|e| format!("failed to build the local engine state: {e}"))?;
    let axum::Json(caps) =
        crw_server::routes::capabilities::capabilities(axum::extract::State(state)).await;
    Ok(caps)
}

/// Render a check list, human-readable by default or as JSON with `--json`,
/// and return the exit code the caller should use. Shared so doctor and
/// smoke print in the same shape and apply the same contract: any `Fail`
/// exits 1, otherwise 0 (warnings are informational).
pub fn print_report(program: &str, target: Target, checks: &[CheckResult], json: bool) -> i32 {
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    let exit_code = if failed > 0 { 1 } else { 0 };

    if json {
        let body = serde_json::json!({
            "target": target,
            "checks": checks,
            "exitCode": exit_code,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&body).expect("check report always serializes")
        );
        return exit_code;
    }

    println!("{program} (target: {target:?})");
    for c in checks {
        let tag = match c.status {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        };
        println!("[{tag}] {}: {}", c.id, c.message);
        if let Some(fix) = &c.fix {
            println!("       fix: {fix}");
        }
    }
    println!();
    if failed == 0 {
        println!("{program}: ok");
    } else {
        println!("{program}: {failed} check(s) failed");
    }
    exit_code
}
