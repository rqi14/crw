# docs/

Scoped notes for this subtree. Read the root `AGENTS.md` first.

## Authored source vs. generated output

`docs/docs/<slug>.md` is the authored source for every docs page.
`docs/<slug>/index.html` is generated publishing output produced from it by
`scripts/build-docs-pages.mjs`, so Google indexes each page at its own URL
instead of one hash-routed SPA. Both the Markdown and the generated HTML are
committed.

**Never hand-edit an `index.html` under `docs/`.** Edit the Markdown source
in `docs/docs/` and regenerate:

```
node scripts/build-docs-pages.mjs
```

The generator reads `docs/site.config.js` (the sidebar) to know which slugs
exist and what section/title each belongs to. A page missing from the
sidebar config will not get a generated page even if the Markdown exists.

## Guards

`scripts/docs-guards.sh` runs in CI and enforces, among other checks: the
SaaS-only control-plane base URL (`fastcrw.com/api`) never leaks outside its
whitelist (engine docs use `api.fastcrw.com`); `docs/agent-onboarding/SKILL.md`
and `mcp/crw-mcp/skills/SKILL.md` stay byte-for-byte identical, so edit both
together; and the search backend's real identity never appears in
user-facing prose, while its config keys/env vars/docker service name stay
untouched. Run it locally before pushing: `bash scripts/docs-guards.sh`.

## What's authoritative here vs. elsewhere

`docs/openapi.json` / `docs/openapi-3.0.json` are committed copies that CI
(`openapi-check.yml`) requires to stay byte-equal to what the running binary
serves at `/openapi.json`. The binary's Axum registration is the real
source per the root matrix; resync these files from it, don't hand-edit.
