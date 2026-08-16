# sdks/

Scoped notes for this subtree. Read the root `AGENTS.md` first.

## What these are

Two thin clients over the native `/v1` HTTP API: `sdks/python` (PyPI package
`crw`) and `sdks/typescript` (npm package `crw-sdk`). Both are MIT-licensed,
separately from the AGPL-3.0 engine. They are primarily `/v1` clients, not
Firecrawl-compat shims, but `parse()` and the batch-scrape helpers call `/v2`
directly (`/v2/parse`, `/v2/batch/scrape`, `/v2/batch/scrape/{id}`) because
there is no `/v1` equivalent for those operations.

Both constructors take a bare host URL, not a base path: `apiUrl` /
`api_url` (or `CRW_API_URL`) is something like `http://localhost:3000` or
`https://api.fastcrw.com`, and each method appends its own path
(`/v1/scrape`, `/v1/crawl`, `/v1/capabilities`, ...). Omit it and the client
defaults to the managed cloud, requiring an API key; set `CRW_LOCAL` to opt
into an unauthenticated local subprocess instead.

If a route is added or changed under `/v1`, both clients need the matching
method/type update in the same change - there is no shared generator
between the Rust server and these two clients, so drift is silent until a
contract test catches it.

## Running the tests

- Python: `cd sdks/python && uv run python -m pytest`. Tests live in
  `sdks/python/tests/`; anything marked `integration` needs a live
  `CRW_API_KEY` and hits real endpoints, so it will not pass offline.
- TypeScript: `cd sdks/typescript && npm ci && npm test`. This is what CI
  (`.github/workflows/ci.yml` job `sdk-ts`) and `make check-ci` both run; use
  npm here even though the rest of the repo's JS/TS tooling is bun. The
  `test` script builds first (`tsc` to both ESM and CJS, plus a
  `tsconfig.test.json` build), then runs on Node's built-in test runner
  (`node --test`) - there is no separate unit-test framework to install.

## What's authoritative here vs. elsewhere

Neither SDK owns its own request/response shapes: those follow `/v1` as
registered in `crw-server`, per the root source-of-truth matrix. A type or
field only belongs in `sdks/python/src/crw/` or `sdks/typescript/src/` after
it exists on the server side.
