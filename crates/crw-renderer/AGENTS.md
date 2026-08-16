# crw-renderer

Scoped notes for this crate only. Read the root `AGENTS.md` first - its
runtime invariants live here, and breaking one costs the most in this crate.

## The escalation ladder

Default JS-renderer fallback order is `["lightpanda", "chrome",
"chrome_proxy"]` (`src/lib.rs`). Direct egress is tried before proxied
egress; a hard-pinned renderer (`RequestedRenderer` other than `auto`) skips
the ladder entirely and its failure must surface as an error, never a
silent fallback to a different tier.

## Policy lives separately from execution

- **Execution**: `lib.rs` (`fetch_with_js` and friends) runs the sequential
  try-next-tier loop and owns the per-tier fetchers (`http_only.rs`,
  `cdp.rs`, `browser.rs`, `camoufox.rs`).
- **Policy - when to promote**: `preference.rs` tracks a sliding window of
  per-host LightPanda failures and decides when to skip straight to Chrome.
- **Policy - what a response means**: `detector.rs` classifies HTML as
  thin/SPA-shell/blocked; `blocklist.rs` and `cloak.rs` handle vendor
  block-page and challenge signals.
- **Policy - which egress to try first**: `egress.rs` is a TTL latch on
  hosts that recently hard-blocked direct egress, reordering to proxy-first
  until it expires. It never suppresses direct egress outright.

New signals belong in a policy module, read by `lib.rs` - not inlined into
the fetch loop - so the ladder stays testable independent of any one
detector's heuristics.

## What matters here specifically

- Changing tier order, promotion thresholds, or fallback conditions can
  move scrape-success. Treat that as a benchmark-gated change, not a
  refactor.
- `is_hard_pinned` and `screenshot_requested()` gate several fallback
  branches in `lib.rs` - a new fallback path must respect both, per the
  root invariants (no silent fallback on a hard pin; screenshot needs a
  capture-capable tier).
