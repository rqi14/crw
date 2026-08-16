# Crates

crw is split into focused crates that can be used independently or together as libraries.

## crw-core

Core types, configuration, and error handling shared by all crw crates.

```bash
cargo add crw-core
```

```rust
use crw_core::{AppConfig, CrwError, CrwResult};
use crw_core::types::{OutputFormat, ScrapeData};

let config = AppConfig::load()?;
println!("Server port: {}", config.server.port);
```

## crw-mcp-proto

Shared MCP (Model Context Protocol) JSON-RPC 2.0 types and tool definitions, used by `crw-server`'s HTTP MCP endpoint, `crw-mcp`, and `crw-browse`.

```bash
cargo add crw-mcp-proto
```

```rust
use crw_mcp_proto::{JsonRpcRequest, PROTOCOL_VERSION};
```

## crw-diff

Stateless change-tracking diff engine used by monitors: given a current scrape and a caller-supplied previous snapshot, classifies the page as same/changed and computes the requested diff surfaces.

```bash
cargo add crw-diff
```

```rust
use crw_diff::{DiffLimits, DEFAULT_MAX_DIFF_CHANGES};
```

## crw-search

SearXNG-backed search client and result transforms for the CRW web scraper.

```bash
cargo add crw-search
```

```rust
use crw_search::{SearxngClient, SearxngParams, transform_flat};
```

## crw-renderer

HTTP fetcher and CDP-based headless browser rendering with automatic SPA detection.

```bash
cargo add crw-renderer                # HTTP only
cargo add crw-renderer --features cdp # HTTP + CDP rendering
```

```rust
use crw_renderer::FallbackRenderer;
use crw_core::config::RendererConfig;
use std::collections::HashMap;

let config = RendererConfig::default();
let renderer = FallbackRenderer::new(&config, "my-bot/1.0", None);

let result = renderer.fetch(
    "https://example.com",
    &HashMap::new(),
    None,  // render_js: None = auto-detect
    None,  // wait_for_ms
).await?;

println!("Status: {}, HTML length: {}", result.status_code, result.html.len());
```

## crw-extract

HTML content extraction — converts raw HTML to markdown, plain text, or cleaned HTML.

```bash
cargo add crw-extract
```

```rust
use crw_extract::extract;
use crw_core::types::OutputFormat;

let html = "<html><body><h1>Title</h1><p>Content here.</p></body></html>";
let data = extract(
    html,
    "https://example.com",
    200,
    None,               // rendered_with
    42,                 // elapsed_ms
    &[OutputFormat::Markdown, OutputFormat::PlainText],
    true,               // only_main_content
    &[],                // include_tags
    &[],                // exclude_tags
);

println!("{}", data.markdown.unwrap());
```

## crw-crawl

Async BFS web crawler with rate limiting, robots.txt compliance, and sitemap support.

```bash
cargo add crw-crawl
```

```rust
use crw_crawl::robots;

// Check robots.txt before crawling
let allowed = robots::is_allowed(
    "https://example.com/robots.txt",
    "https://example.com/page",
    "my-bot",
).await?;
```

## crw-server

Axum-based HTTP API server — Firecrawl-compatible REST endpoints and built-in MCP transport.

```bash
cargo add crw-server
```

```rust
use crw_server::app;
use crw_core::AppConfig;

let config = AppConfig::load()?;
let app = app::build_app(config).await;

let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
axum::serve(listener, app).await?;
```

## crw-cli

Standalone CLI binary — scrape any URL to markdown, JSON, or plain text without a server.

```bash
cargo install crw-cli
```

```bash
crw https://example.com              # markdown to stdout
crw https://example.com --format json
crw https://example.com -o page.md
```

`crw` also has `crawl`, `map`, and `browse` subcommands:

```bash
crw crawl https://example.com -d 2 -l 10        # BFS crawl, depth 2, 10 pages max
crw crawl https://example.com --js --format json

crw map https://example.com                     # discover URLs, one per line
crw map https://example.com --sitemap-only --format json

crw browse                                      # interactive MCP server over CDP (stdio), ws://localhost:9222 by default
crw browse --ws-url ws://localhost:9222
```

`crw browse` wraps the same interactive browser-automation MCP server as the standalone `crw-browse` binary below (same tool, invoked through `crw` instead of directly).

This is a standalone binary, not a library. See [Quick Start](#quick-start) for usage examples.

### crw bench

`crw bench` is an internal benchmarking tool: it runs a question-answering
dataset through a running crw server's `/v1/search` answer path and grades
each response with an LLM judge, writing a reproducible run snapshot (results
JSONL + `report.json` / `report.md`) to disk. It needs a running crw server
(with search and an LLM answer path configured) and an LLM judge key, so it is
a local/release tool, not something that runs in CI.

```bash
crw bench                                       # FRAMES dataset against http://localhost:3000
crw bench --dataset-file my-questions.jsonl --limit 50
crw bench --multi-round --query-expand 3 --concurrency 4
```

Flags:

- `--dataset`: dataset to run; `frames` auto-downloads the FRAMES benchmark. Default `frames`.
- `--dataset-file`: use a local TSV/JSONL file instead of downloading (TSV needs `Prompt`/`Answer` columns; JSONL needs `prompt`/`answer` keys).
- `--server`: base URL of the crw server under test. Default `http://localhost:3000`.
- `--api-key`: bearer key for the server under test, if required. Env `CRW_API_KEY`.
- `--limit`: cap the number of questions run; `0` runs the full dataset. Default `0`.
- `--search-limit`: number of search results the answer leg may draw from. Default `10`.
- `--judge-model`: overrides the configured `extraction.llm` model for grading.
- `--output`: output directory root for run snapshots. Default `bench/runs`.
- `--timeout-secs`: per-request timeout to the server under test, in seconds. Default `120`.
- `--seed`: RNG seed for the bootstrap confidence interval, for reproducibility. Default `42`.
- `--multi-round`: enable adaptive multi-round retrieval (a second evidence-scout round fires when round one abstains). Off by default.
- `--query-expand`: number of diverse query rewrites fetched and unioned per question. Omitted uses the server default (off).
- `--concurrency`: number of questions to run concurrently; `1` is sequential. Default `1`.

### crw doctor

`crw doctor` is read-only diagnostics: it never installs, starts, repairs, or
rewrites anything. When a check fails it points at `crw setup` or names the
exact fix, rather than attempting one itself.

```bash
crw doctor                                      # diagnose whatever config.toml resolves to
crw doctor --target cloud                       # force the cloud-API checks
crw doctor --json                                # machine-readable output
```

Bare `crw doctor` (no `--target`) resolves which backend to diagnose from
`config.toml`: no `[client].api_url` configured resolves to `local` (this is
also what a fresh install with no config file resolves to); a configured
`[client].api_url` with no local renderer configured resolves to `cloud`;
configuring both at once is ambiguous, and the command exits `2` naming the
exact `--target local` / `--target cloud` choice. `--target mcp` is never
auto-resolved.

Local checks make no internet request. Cloud checks only call the
non-billable `GET /v1/capabilities` endpoint. Each reachability probe
(renderer, search backend, proxy, cloud API) is capped at 5 seconds; a run
where every configured probe times out (up to four renderer entries, plus
search, plus proxy) takes on the order of 30 seconds in the worst case.

Flags:

- `--target local|cloud|mcp`: which backend to diagnose. Auto-resolved from config when omitted.
- `--json`: emit machine-readable JSON instead of human-readable text.

Check IDs (stable, appear as-is in `--json` output): `binary.build`,
`config.load`, `config.source`, `config.unknown-fields`, `fs.cache-writable`,
`renderer.connectivity`, `renderer.screenshot`, `search.reachability`,
`llm.configured`, `proxy.parse`, `proxy.reachability`, `server.listen-port`,
`capabilities.snapshot`, `cloud.api-key`, `cloud.reachability`, `mcp.mode`.
`config.unknown-fields`
currently always reports `skip`: the config loader merges layers
(`config.default.toml`, the user config, `CRW_*` env vars) without tracking
per-field provenance, so a stale or renamed key is silently dropped rather
than reported.

Exit codes (shared with `crw smoke`): `0` every check passed (warnings and
skips are informational and do not affect the exit code), `1` at least one
check failed, `2` a usage error, a `--target` ambiguity, or a refused
billable operation.

`crw doctor` never prints a stored credential — an API key or proxy password
is redacted before it reaches the report.

### crw smoke

`crw smoke` runs deterministic sanity checks across the cli/server/mcp
surfaces. Bare `crw smoke` is exactly `crw smoke --offline --surface cli`: no
network, never billable.

```bash
crw smoke                                        # offline, cli surface only
crw smoke --surface all                          # cli + server + mcp
crw smoke --live https://example.com --allow-billable
```

Flags:

- `--offline`: run against local fixtures only. Deterministic, no network, never billable. Default when neither this nor `--live` is given.
- `--live URL`: run a real scrape of `URL` through every selected surface instead of the offline fixture. `server` posts through `/v1/scrape`; `mcp` calls the `crw_scrape` tool rather than stopping at `tools/list`.
- `--surface cli|server|mcp|all`: which surface(s) to smoke-test. Default `cli`.
- `--allow-billable`: required before a `--live` run against the managed fastCRW API; without it that combination is refused with exit `2` rather than silently spending credits. Each selected live surface performs its own scrape and may consume one credit. Custom `CRW_API_URL` self-hosted servers are never assumed billable.
- `--json`: emit machine-readable JSON instead of human-readable text.

`crw smoke` resolves config the same way `crw doctor` does and has no
`--target` flag of its own — it diagnoses whichever backend the config
resolves to.

Check IDs: `smoke.cli.scrape`, `smoke.server.boot`, `smoke.mcp.protocol`.

Exit codes: the same contract as `crw doctor` (`0`/`1`/`2` above).

Like `crw doctor`, `crw smoke` never prints a stored credential.

## crw-browse

Standalone MCP server for interactive browser automation over CDP: stateful multi-step sessions (click, fill, read the DOM) rather than one-shot scraping.

```bash
cargo install crw-browse
```

This is a standalone binary, not a library. See [Browser Automation (crw-browse)](/docs/crw-browse) for setup instructions.

## crw-mcp

MCP stdio proxy binary — connects AI assistants to a running crw server.

```bash
cargo install crw-mcp
```

This is a standalone binary, not a library. See [MCP Server](#mcp) for setup instructions.
