.PHONY: hooks fmt fmt-check clippy test build check check-fast check-ci sync-docs-changelog

# Install git pre-commit hook
hooks:
	@cp scripts/pre-commit .git/hooks/pre-commit
	@chmod +x .git/hooks/pre-commit
	@echo "Pre-commit hook installed."

# Individual CI-equivalent targets
fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

build:
	cargo build --workspace --all-targets

# Fast inner loop: format check + clippy only, nothing else. clippy already
# type-checks every target (--all-targets), so this catches most breakage in
# ~15s on a warm cache. Deliberately does NOT run `cargo test`, the guard
# scripts, or the drift checks below (those are what `make check` is for).
# Run this before every commit; run `make check` before pushing/opening a PR.
check-fast: fmt-check clippy

# Full local verification. Runs every step .github/workflows/ci.yml's `check`
# job runs, in the same order, so a green `make check` means a green CI
# `check` job. Then runs additional drift checks that are local-only for now,
# not yet gated in ci.yml.
check: fmt-check clippy
	@echo "==> CI parity (mirrors .github/workflows/ci.yml 'check' job)"
	bash scripts/check-no-process-exit.sh
	bash scripts/check-internal-dep-versions.sh
	python3 scripts/release/audit_release_please_config.py
	python3 scripts/release/test_guards.py
	cargo test --workspace
	cargo test -p crw-server
	@echo "==> documentation drift checks (local-only, not yet in ci.yml)"
	bash scripts/check-crate-graph-doc.sh
	bash scripts/check-cli-command-doc.sh
	bash scripts/check-skill-route-links.sh
	bash scripts/check-doc-links.sh

# Full CI parity: everything `make check` covers, plus the two ci.yml jobs it
# does not reach. Prints exactly what it cannot reproduce instead of skipping
# silently.
check-ci: check
	@echo "==> sdk-ts (mirrors .github/workflows/ci.yml 'sdk-ts' job)"
	cd sdks/typescript && npm ci && npm run build && npm test
	cd mcp/crw-mcp && npm test
	@echo ""
	@echo "NOT reproduced by this target:"
	@echo "  - sdk-ts Node matrix: ci.yml runs Node 22 AND 24; this ran only $$(node --version) (whatever is on your PATH)."
	@echo "  - conformance job: needs 'cargo build --release --bin crw', a live"
	@echo "    'crw serve' instance, and network access to real external sites"
	@echo "    (firecrawl.dev, w3.org). Too slow/stateful for a Makefile target."
	@echo "    Run it yourself: cargo build --release --bin crw && ./target/release/crw serve &"
	@echo "    then: cd conformance && CRW_URL=http://localhost:3000 uv run ./run.sh compare"

sync-docs-changelog:
	python3 scripts/sync-docs-changelog.py
