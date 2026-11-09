# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] — 2026-11-09

First numbered release of the Phase 0 localhost MVP, plus Week 1 repositioning
toward an open-source launch.

### Added

- Local Rust daemon + CLI (`publish`, `watch`, `watch --stream`, `fetch`,
  `comment`, `unpublish`, `status`, `batch-processing`) over a SQLite store.
- Rendered-HTML reviewer UI with text-quote anchoring, threaded comments,
  batch Submit-all, resolve/reopen, update banner, and version history dropdown.
- Optional GitHub OAuth with server-stamped `owner` / `user` / `guest` roles;
  Claude skill triages plan edits behind the owner role.
- Stdio MCP server (`blueprint mcp`) exposing the same daemon API, including
  long-poll `wait-comment`.
- Durable Finish Review latch, batch-processing indicator with auto-clear,
  session persistence in SQLite, and a blueprint write gate once OAuth is on.
- Frontend Vitest suite (anchoring vectors, poll, drafts, render) alongside
  Rust e2e / CLI / concurrency tests.
- Pinned Rust 1.98.0 toolchain; CI enforces `fmt`, clippy `-D warnings`,
  `cargo test`, and the Vitest job.
- Dual MIT OR Apache-2.0 licensing, Contributor Covenant 2.1, and CONTRIBUTING.
- GitHub Actions release workflow that attaches Linux x86_64 and macOS
  (arm64 + x86_64) binaries on `v*` tags.

### Changed

- README led with trust/provenance positioning and an honest comparison to
  crit and Plannotator; long setup and OAuth detail moved to `docs/`.

### Fixed

- Anchoring hang; finish long-poll bounds and lost-wakeup holes; resolved-thread
  semantics made daemon-authoritative; assorted daemon/CLI/MCP/store sweep
  fixes from the review pass.

## [0.1.0] — unreleased archive

Initial commit through early branding and README — never tagged. Treat 0.2.0
as the first public version line.
