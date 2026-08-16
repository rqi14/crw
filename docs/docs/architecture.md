# Architecture

## Crate Structure

crw is a Rust workspace with 11 crates, each with a focused responsibility:

```
crw-mcp-proto   shared MCP JSON-RPC types                    (leaf)
crw-core        types, config, errors                        → crw-mcp-proto
crw-diff        change-tracking diff engine                  → crw-core
crw-extract     HTML cleaning, format conversion              → crw-core
crw-renderer    HTTP + CDP fetcher                            → crw-core, crw-extract
crw-search      search client and result transforms           → crw-core
crw-crawl       BFS crawler, single-page scraper               → crw-core, crw-diff, crw-extract, crw-renderer
crw-server      Axum HTTP API (library + own binary)          → crw-core, crw-crawl, crw-diff, crw-extract, crw-renderer, crw-search
crw-browse      interactive browser-automation MCP server      → crw-core, crw-renderer
crw-mcp         stdio MCP proxy                                → crw-core, crw-renderer, crw-server
crw-cli         command-line binary                            → crw-browse, crw-core, crw-crawl, crw-extract, crw-renderer, crw-search, crw-server
```

`crw-server` has no explicit `[[bin]]` section in its `Cargo.toml`, but it does
have a `src/main.rs`, so Cargo auto-discovers a `crw-server` binary target
alongside the library. That binary is the production container entrypoint
(`Dockerfile` `CMD ["crw-server"]`). The `crw` binary is a separate build, from
`crw-cli`, which embeds the `crw-server` library for `crw serve`. The workspace
produces four binaries: `crw` (from `crw-cli`), `crw-server` (from
`crw-server`), `crw-mcp` (from `crw-mcp`), and `crw-browse` (from `crw-browse`,
a separate standalone MCP server for interactive browser automation, see
[crw-browse](/docs/crw-browse)).

## Request Flow

### Scrape request

```
Client → POST /v1/scrape
  → Auth middleware (Bearer token check)
  → scrape handler
    → FallbackRenderer.fetch(url)
      → HTTP request (reqwest)
      → SPA detection heuristics
      → CDP rendering if needed (LightPanda/Playwright/Chrome)
    → HTML cleaning (lol_html)
      → Remove script/style/iframe/svg
      → Remove nav/footer/header if onlyMainContent
      → Apply includeTags/excludeTags CSS selectors
    → Readability extraction
      → Try: article → main → [role=main] → .post-content → body
    → Format conversion
      → Markdown (fast_html2md with fallback chain)
      → HTML / RawHTML / PlainText / Links
      → JSON (LLM extraction via Anthropic/OpenAI)
    → Metadata extraction
      → title, description, og:*, canonical, lang
  → JSON response
```

### Crawl request

```
Client → POST /v1/crawl
  → Create crawl job (UUID)
  → Spawn async BFS task
    → Fetch robots.txt
    → BFS loop with VecDeque:
      → Dequeue URL
      → Check robots.txt allowance
      → Acquire semaphore (max_concurrency)
      → Rate limit (requests_per_second)
      → Scrape page (same pipeline as single scrape)
      → Extract links from page
      → Filter: same origin, not visited, under max_depth
      → Enqueue new URLs
      → Update crawl state via watch::Sender
    → Until: maxPages reached, maxDepth exhausted, or queue empty
  → Return job ID immediately

Client → GET /v1/crawl/{id}
  → Read crawl state via watch::Receiver
  → Return progress + results
```

## Middleware Stack

The Axum server applies middleware in this order:

1. **CORS** — `CorsLayer::permissive()`
2. **Trace** — HTTP request logging via `tracing`
3. **Body limit** — Max 1 MB request body
4. **Timeout** — Configurable request timeout
5. **Auth** — Bearer token validation (if `auth.api_keys` is set)

## Feature Flags

| Flag | Crate | Effect |
|------|-------|--------|
| `cdp` | `crw-renderer` | Enables CDP rendering via `tokio-tungstenite` |
| `cdp` | `crw-server` | Passes through to `crw-renderer/cdp` |
| `test-utils` | `crw-server` | Exposes internal functions for testing |

## Key Dependencies

| Dependency | Purpose |
|-----------|---------|
| `axum` 0.8 | HTTP API framework |
| `tokio` | Async runtime |
| `reqwest` | HTTP client (rustls) |
| `tokio-tungstenite` | CDP WebSocket (with `cdp` feature) |
| `lol_html` | Streaming HTML rewriting |
| `scraper` | CSS selector-based HTML parsing |
| `fast_html2md` | HTML → Markdown conversion |
| `jsonschema` | LLM output validation |
| `config` | Layered TOML configuration |
| `tracing` | Structured logging |
