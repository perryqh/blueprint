# Contributing to blueprint

Thanks for taking a look. This document is what CI and the pre-commit hook
already enforce — run the same commands locally before opening a PR.

## Code of Conduct

Participation is governed by [`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).

## Prerequisites

- The pinned Rust toolchain in [`rust-toolchain.toml`](rust-toolchain.toml)
  (`rustup` installs it automatically on first `cargo` invocation, including
  `rustfmt` and `clippy`).
- Node.js 22+ for the frontend Vitest suite (`npm ci`).
- On Linux, `libsqlite3-dev` and `pkg-config` if a build fails on `rusqlite`
  (macOS gets them via the SDK). The crate uses the `bundled` feature, so a
  system SQLite is usually unnecessary.

## Build and test (mirrors CI)

CI lives in [`.github/workflows/rust.yml`](.github/workflows/rust.yml). The
commands below are what that workflow runs:

```bash
# Formatting
cargo fmt --check

# Lints (deny warnings)
cargo clippy --all-targets --all-features -- -D warnings

# Build + Rust tests (daemon, store, CLI, MCP, e2e)
cargo build --verbose
cargo test --verbose

# Frontend ES-module tests (anchoring, poll, drafts, render)
npm ci
npx vitest run
# equivalent: npm test

# Rename safety net — fails if a stale pre-rename project name reappears.
# Mirror of the "No stale plan" + "share" check in .github/workflows/rust.yml:
# git grep for the old hyphenated / underscored / SCREAMING forms across
# *.rs *.toml *.md *.js *.html *.css *.json (excluding Cargo.lock).
```

A [`rusty-hook`](https://github.com/swellaby/rusty-hook) pre-commit hook is
installed the first time you run `cargo test` (or `cargo test --no-run`). It
runs:

```
cargo clippy --all-targets --all-features -- -D warnings && cargo test && cargo fmt -- --check
```

See [`.rusty-hook.toml`](.rusty-hook.toml). Bypass for an emergency commit with
`git commit --no-verify`.

## Pull requests

- Keep changes focused; prefer several small PRs over one kitchen-sink PR.
- Match existing style. `cargo fmt` and clippy-with-`-D warnings` are required.
- Add or extend tests for behavior you change — Rust coverage lives under
  `tests/` and module-level `#[cfg(test)]`; JS coverage lives under
  `js-tests/` and runs through Vitest.
- Update the README or `docs/` when you change user-facing behavior (CLI flags,
  env vars, HTTP endpoints, OAuth).
- Do not bump the crate version or cut a release tag in a feature PR; version
  bumps ride with the release workflow on `v*` tags.
- Describe *what* changed and *why* in the PR body. Link any related issue.

## License

By contributing, you agree that your contributions are licensed under the same
dual MIT OR Apache-2.0 terms as the rest of the project (see `LICENSE-MIT` and
`LICENSE-APACHE`).
