# Contributing to fastCRW

Contributions are welcome — issues and PRs both.

## Contributor License Agreement

On your first pull request a bot will ask you to sign our
[Contributor License Agreement](CLA.md). It takes one comment and covers all of
your future contributions. We can't merge PRs until it's signed.

## Development setup

Requires **rustc 1.85 or newer** (the workspace uses `edition = "2024"`, which
1.85 is the first stable toolchain to support). There is no pinned
`rust-toolchain.toml`; install a recent stable toolchain via
[rustup](https://rustup.rs) and `rustup update` if `cargo build` reports an
edition error.

1. Fork the repository
2. Install pre-commit hooks: `make hooks`
3. Create a feature branch: `git checkout -b feat/my-feature`
4. Commit your changes: `git commit -m 'feat: add my feature'`
5. Push and open a Pull Request

The pre-commit hook runs fmt, clippy, the browser-teardown guard, and the
full test suite. For the complete CI-equivalent check (including the release
guard scripts and documentation drift checks), run:

```bash
make check-fast   # fmt + clippy only, fast inner loop
make check        # everything CI's `check` job runs, plus drift checks
```

## Architecture

The workspace has 11 crates under `crates/`. The authoritative crate table
and dependency graph live in
[docs.fastcrw.com/architecture/](https://docs.fastcrw.com/architecture/)
(source: `docs/docs/architecture.md`) - read that instead of a copy here, so
this file cannot go stale when a crate is added or removed.

## Documentation

Before editing anything under `docs/`, read [`docs/AGENTS.md`](docs/AGENTS.md)
first: it explains which files are authored source and which are generated
output, and hand-editing the wrong one gets silently overwritten.

## Contributors

<p>
  <a href="https://github.com/us"><img src="https://github.com/us.png?size=64" width="64" height="64" alt="us"/></a>
  <a href="https://github.com/adambenhassen"><img src="https://github.com/adambenhassen.png?size=64" width="64" height="64" alt="adambenhassen"/></a>
  <a href="https://github.com/mj520"><img src="https://github.com/mj520.png?size=64" width="64" height="64" alt="mj520"/></a>
</p>
