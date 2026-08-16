# ADR 0002: Source-of-truth matrix for generated and derived docs

- Status: accepted
- Applies to: all documentation and packaged content derived from code,
  config, or route/schema definitions (docs/, skills/, mcp/, README.md,
  COMPATIBILITY-firecrawl.md, the docs site build).

## Context

This repository commits both the authoritative definitions of behavior (Rust
enums, clap arg structs, the axum router, a committed OpenAPI artifact) and
prose or generated files that describe those definitions. Prose drifts from
code silently: a subcommand gets added and nobody documents it, a route gets
removed and a skill still tells an agent to call it, a crate gets added to the
workspace and the architecture diagram does not know.

A handful of mechanical drift checks now exist to catch specific drift
classes. They only work because each one has a single, unambiguous ground
truth to diff the doc against. Before adding another derived surface or
another check, that ground truth needs to be named explicitly, once, so a
future check is built against the same authority rather than against whatever
doc happened to look canonical at the time.

## Decision

For each concern below, exactly one source is authoritative. Every other file
that describes that concern is a derived surface: it may be committed (for
build-output or performance reasons), but it must never be hand-edited to
diverge from its source, and where a drift check exists, hand-editing it out
of sync is a CI-visible failure, not a style nit.

| Concern | Authoritative source | Derived surfaces |
| --- | --- | --- |
| HTTP route existence (interim) | Axum registration (`crates/crw-server/src/routes/v1/mod.rs`, `routes/v2/mod.rs`) | route inventory and drift checks |
| HTTP public schemas (interim) | committed OpenAPI artifact (`docs/openapi.json`, `docs/openapi-3.0.json`) | API reference and schema checks |
| Runtime capabilities | typed capability evaluator (`crates/crw-server/src/routes/capabilities.rs`) | `/v1/capabilities` and capability docs |
| Configuration fields and defaults | typed Rust config (`crw-core::config::AppConfig` and friends) | config reference and completion |
| CLI commands and options | clap command definitions (`crates/crw-cli/src/main.rs` `Commands` enum, each `commands::*::*Args`) | CLI reference and examples |
| Error taxonomy | typed error enums (`crw-core::error::CrwError` and sibling enums) | error reference and SDK helpers |
| Workspace and crate graph | `cargo metadata` | architecture diagram and crate table |
| Product concepts and guidance | authored Markdown (`docs/docs/*.md`) | rendered site (`docs/<slug>/index.html`) and llms indexes (`docs/llms.txt`, `docs/llms-full.txt`) |
| Agent workflows | canonical skills and references (`skills/`) | packaged MCP and plugin copies (`mcp/crw-mcp/skills/`, `.claude-plugin/plugin.json`) |

The operative rule: no derived surface may be updated by hand without going
through its generator, or without its drift check having run and passed. A
derived file is only ever a build artifact of its authoritative source, even
when it is committed to git.

### What is enforced today versus convention only

Four matrix rows have a mechanical drift check wired into `make check` right
now:

- Workspace and crate graph: `scripts/check-crate-graph-doc.sh` diffs
  `docs/docs/architecture.md`'s crate list and dependency edges against
  `cargo metadata --no-deps`.
- CLI commands and options: `scripts/check-cli-command-doc.sh` diffs every
  `crw <word>` mention across the docs against the `Commands` enum in
  `crates/crw-cli/src/main.rs`.
- HTTP route existence (interim): `scripts/check-skill-route-links.sh` diffs
  every `METHOD /path` mention in `skills/` and `docs/` against the routes
  registered in `routes/v1/mod.rs` and `routes/v2/mod.rs`.
- Internal relative links and docs-site slugs: `scripts/check-doc-links.sh`
  resolves every relative link and `/docs/<slug>` reference against the
  filesystem and `docs/site.config.js`.

One more check exists but is not part of `make check`: `scripts/check-openapi.sh`
diffs a running `crw-server` binary's live `/openapi.json` against the
committed `docs/openapi.json` and checks its version against the workspace
version. It only runs when a release binary is present and is not gated in CI
today, so the "public schemas" row is checked manually and locally, not
enforced on every change.

One additional focused truth guard is wired into the documentation CI:

- Renderer credit pricing: `scripts/docs-guards.sh` derives the flat price
  from `crw-renderer::credit_for` and checks both user-facing credit reference
  pages. Deleting the documented price also fails, so the guard cannot pass
  vacuously.

Every other row in the matrix (runtime capabilities, configuration fields and
defaults, error taxonomy, product concepts and guidance, agent workflows) has
no drift check today. Keeping the doc in sync with its source for those rows
is convention only. This ADR does not claim otherwise; it records the target
authority so a future check has something unambiguous to diff against.

### Why route existence and public schemas have two separate interim authorities

A single generated contract (one artifact that is simultaneously the source
of truth for "does this route exist" and "what does its request or response
look like") does not exist in this codebase yet. Route existence is checked
against the axum router directly, because that is the cheapest and always-
available ground truth: it needs no build step beyond a source-file regex.
Public schemas are checked against a committed OpenAPI artifact diffed against
a live binary's own `/openapi.json`, because the shape of a request or
response is not something a static regex over router code can derive.

These two checks can disagree in a narrow window: a route can be registered
in the router before its OpenAPI schema is regenerated, or vice versa. Until a
single generated contract (for example, an OpenAPI spec generated directly
from the router and handler types, with both existence and shape checked
against that one artifact) replaces both, this split is accepted as the
interim state, not the target state.

## Consequences

- A contributor adding a CLI flag, a route, or a workspace crate has an
  unambiguous answer to "what do I edit" (the typed source) and, for four
  concerns, a check that fails loudly if a derived doc is left behind.
- Hand-editing a derived surface to fix a drift-check failure is a mistake:
  the fix belongs in the authoritative source or in the prose that describes
  it, whichever the check is actually diffing against; editing generated HTML
  output directly is exactly this mistake for the docs site (see
  `docs/AGENTS.md`).
- Adding a new derived surface without adding a row here, or adding a row
  without eventually building a check for it, is allowed but should be a
  deliberate, visible decision, not a silent gap.
- A future generated OpenAPI contract that unifies route existence and public
  schemas under one authority should update this ADR's route and schema rows
  and retire the "two separate interim authorities" caveat.
