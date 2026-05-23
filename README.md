# blueprint

Share interactive HTML blueprints with reviewers and let them leave inline anchored comments — with an HTTP API Claude can drive too.

`crit` shows source; `blueprint` shows the *rendered* page. Same daemon/CLI shape so muscle memory transfers.

## Status

**Phase 0 — localhost MVP.** Single Rust binary, SQLite at `~/.blueprint/blueprints.db`, optional GitHub OAuth, vanilla-JS text-quote anchoring. Phase 1 (personal hosting) and Phase 2 (internal + Okta) are designed but not built.

## Install

```bash
cargo install --path .
```

Binary lands at `~/.cargo/bin/blueprint`.

## Use it

```bash
blueprint publish path/to/blueprint.html        # auto-spawns daemon, opens in browser
blueprint status                                 # show running daemon + blueprints with comment counts
blueprint comment <slug> --quote "..." 'comment body'
blueprint comment <slug> --reply-to c_xxx 'reply body'
blueprint watch <slug>                           # blocks until reviewer clicks Finish
blueprint watch <slug> --stream                  # streams each new comment as JSON to stdout
blueprint fetch <slug>                           # writes ./.blueprint/<slug>/review.json
blueprint publish file.html --slug <slug> --update   # revise in place; reviewers see a refresh banner
blueprint unpublish <slug>                       # daemon stops when no blueprints remain
```

`--author` defaults to `$USER`. `--quote` must appear literally in the rendered HTML; if it doesn't, `blueprint comment` exits non-zero with a clear error.

## How it works

**Daemon.** Runs on `127.0.0.1:7321` by default. Lock file at `~/.blueprint/daemon.lock` records PID + port. Any CLI subcommand reuses a live daemon or spawns a detached child. The lock file is PID-scoped, so a graceful shutdown of an old daemon can't clobber a replacement's lock during handoff.

**Anchoring.** Comments store a [TextQuoteSelector](https://www.w3.org/TR/annotation-model/#text-quote-selector) — `exact` text plus ~32 chars of `prefix` and `suffix`. The browser walks the iframe's text nodes, finds the first occurrence whose context matches, wraps the range in a `<span data-ps-hl>`, and binds a click handler that scrolls the matching comment group in the sidebar. If `exact` no longer appears, the comment renders as **drifted** (yellow badge).

**Bidirectional click-to-scroll.** Click a highlighted span → sidebar group flashes blue. Click a sidebar group's body → the highlight in the iframe pulses. Inputs/buttons inside the group are excluded so reply forms still work.

**Threaded comments with collapse.** Replies render nested under their parent at arbitrary depth. Each thread can be collapsed via the quote bar (showing a "N messages" badge); a sidebar header offers **Collapse all** / **Expand all**. Collapse state persists in `localStorage` per-slug.

**Batch comment submission.** The reviewer stages drafts in a sidebar panel and clicks **Submit all** once. One POST → one broadcast event → one agent wake-up, instead of N noisy round-trips.

**Update banner, not auto-reload.** When the blueprint is `--update`d, the `blueprint_version` bumps. The frontend polls every 1.5s and, on version change, shows a "Blueprint updated" banner with **Refresh** and **Dismiss** buttons — reviewers keep their scroll position and refresh on their own terms.

**Live monitoring for agents.** `blueprint watch <slug> --stream` long-polls `/api/blueprints/:slug/wait-comment` and prints one JSON line per new comment to stdout, with line-flushed output suitable for piping into an agent that reacts event-driven. Reconnects automatically if the daemon restarts.

**Cache hygiene.** `/static/*` and `/api/blueprints/:slug/raw` set `Cache-Control: no-store` so a browser never serves stale frontend assets or stale blueprint HTML.

## HTTP API

The CLI is a thin wrapper over a REST API at `http://127.0.0.1:7321`:

| Method   | Path                                                | Notes                                                                      |
| -------- | --------------------------------------------------- | -------------------------------------------------------------------------- |
| `POST`   | `/api/blueprints`                                   | `{ "html": "...", "slug": "optional" }` → `{ slug, url }`                  |
| `GET`    | `/api/blueprints`                                   | List with `comment_count`, `unresolved_count`, `last_activity_at`          |
| `PUT`    | `/api/blueprints/:slug`                             | `{ "html": "..." }` — replace HTML, bump `blueprint_version`               |
| `GET`    | `/api/blueprints/:slug/raw`                         | Raw uploaded HTML (served into the iframe), `no-store`                     |
| `GET`    | `/api/blueprints/:slug/comments?since=ts`           | `{ comments, server_ts, blueprint_version }`                               |
| `POST`   | `/api/blueprints/:slug/comments`                    | `{ author, body, selector, parent_id? }` — single top-level comment        |
| `POST`   | `/api/blueprints/:slug/comments/batch`              | `[ { author, body, selector?, parent_id? }, ... ]` — atomic Submit-all     |
| `POST`   | `/api/blueprints/:slug/comments/:id/replies`        | `{ author, body }` — inherits parent's selector                            |
| `POST`   | `/api/blueprints/:slug/finish`                      | Mark review round complete; wakes any `/wait` watchers                     |
| `GET`    | `/api/blueprints/:slug/wait`                        | Long-polls until `/finish` or deletion                                     |
| `GET`    | `/api/blueprints/:slug/wait-comment?since=ts`       | Long-polls until a new comment arrives, ~30s timeout — used by `--stream`  |
| `DELETE` | `/api/blueprints/:slug`                             | Unpublish                                                                  |
| `POST`   | `/api/shutdown-if-empty`                            | Server-side count-and-stop; safe under concurrent publish                  |

## Claude Code skill: `/blueprint`

Ships at `integrations/claude-code/skills/blueprint/`. It's the default planning surface — its description is broad enough that Claude reaches for it whenever you'd otherwise get a plain Markdown plan, not just when you literally type `/blueprint`.

What it does:

1. Renders a **rich, self-contained HTML blueprint** — executive summary, context, code excerpts with file paths, mockups (HTML/CSS or inline SVG), verification steps. Includes the dark-mode-aware CSS variable starter so the blueprint adapts to the reviewer's theme.
2. Publishes via `blueprint publish --no-open --json` — no browser auto-launch, just the URL printed back at you to open when you're ready.
3. Starts `blueprint watch <slug> --stream` **in the background** and uses Claude Code's `Monitor` tool to react to each new Submit-all batch as it lands. You stage comments, hit submit; Claude wakes up, edits the HTML, re-publishes with `--update`, and posts threaded replies.
4. Loop until you click **Finish Review** in the browser or tell Claude in chat to wrap up.

Install (until a plugin marketplace is wired up):

```bash
ln -s "$PWD/integrations/claude-code/skills/blueprint" ~/.claude/skills/blueprint
```

The daemon binds port **7321** by default. That's the port hard-coded into the registered GitHub OAuth app's callback URL, so once you populate `~/.blueprint/env` with your credentials the redirect works end-to-end with no extra env exports. Override with `--port` on `blueprint serve` or `BLUEPRINT_PORT` if 7321 is already taken — but OAuth login won't work on any other port.

## Architecture

```
blueprint/
├── Cargo.toml
├── src/
│   ├── lib.rs              # public re-exports for tests
│   ├── main.rs             # CLI entry
│   ├── cli.rs              # publish / watch / fetch / comment / unpublish / status / watch --stream
│   ├── daemon.rs           # detached-child spawn, PID-scoped lock file, spawn flock
│   ├── server.rs           # axum routes + AppState (blueprint_versions, finish_signals, events broadcast)
│   ├── store.rs            # rusqlite — blueprints, comments, users, batch-insert
│   ├── auth.rs             # GitHub OAuth + session wiring + CLI bearer token
│   ├── selector.rs         # TextQuoteSelector type
│   ├── slug.rs             # adjective-month-animal generator
│   ├── review_file.rs      # crit-compatible review.json writer
│   └── error.rs            # AppError → axum response
├── frontend/               # embedded via rust-embed (no Node build step)
│   ├── index.html          # landing
│   ├── reviewer.html       # /b/:slug shell — iframe, sidebar, drafts bar, update banner
│   ├── app.js              # iframe wiring, TextQuoteSelector matcher, drafts batching
│   └── styles.css
├── integrations/
│   └── claude-code/skills/blueprint/SKILL.md
└── tests/
    ├── e2e.rs                   # in-process daemon, HTTP-level e2e
    ├── concurrent.rs            # multi-repo + batch endpoint
    ├── cli_smoke.rs             # spawns the binary, round-trips publish/status/unpublish
    └── no_legacy_references.rs  # safety net for the rename — fails on stale strings
```

Stack: `axum`, `tokio`, `rusqlite` (bundled), `nix` (flock), `serde`, `clap`, `rust-embed`, `reqwest`, `oauth2`, `tower-sessions`.

## Tests

```bash
cargo test
```

Covers: publish → comment → reply → finish → fetch → update → drift → unpublish; random-slug generation; empty-HTML / empty-body / unknown-parent rejection; the `GET /api/blueprints` summary shape; `wait-comment` fast-path and slow-path; OAuth round-trip against a mock GitHub; CLI bearer-token write-auth; multi-repo concurrency (`shutdown-if-empty`, `X-Client-Cwd`); the batch endpoint (atomicity, single wake-up); CLI subprocess smoke test; and a grep-based "no stale strings" check that proves the rename is complete.

Browser-side anchoring, collapse, and click-to-scroll are not covered by the test suite (manual verification only).

A [`rusty-hook`](https://github.com/swellaby/rusty-hook) pre-commit hook is installed via the build script the first time you run `cargo test` (or `cargo test --no-run`). It runs:

```
cargo clippy --all-targets --all-features -- -D warnings && cargo test && cargo fmt -- --check
```

See `.rusty-hook.toml`. To bypass for an emergency commit: `git commit --no-verify`.

## What's intentionally cut from Phase 0

- **Cross-machine sharing.** Run an SSH tunnel if you need it before Phase 1.
- **Multi-version diffing.** `--update` replaces the HTML; old comments either re-anchor, render as "refresh to see," or drift.
- **Realtime collab via SSE / WebSockets.** 1.5s polling for the sidebar; long-poll for `wait` and `wait-comment`. No fan-out streaming.
- **Resolve workflow UI.** Comments have a `resolved` column but no UI affordance.
- **Slack / email notifications.** `blueprint watch` and `blueprint watch --stream` are the only "tell me when something happens" surfaces.
- **XDG-compliant data dir.** SQLite lives at `~/.blueprint/blueprints.db` regardless of `$XDG_DATA_HOME`.

## Phase 1 / 2 roadmap

- **Phase 1 — personal hosting.** Deploy the same binary to Fly.io / Railway. GitHub OAuth for commenter identity. SQLite on a persistent volume. `blueprint publish --remote` to push to the hosted instance.
- **Phase 2 — internal.** Okta SSO. Switchboard catalog entry. Postgres. Slack notifications. Service maps under the owning team.

The skill surface (`publish`, `watch`, `fetch`, `comment`, `unpublish`, `stream`) is identical across phases — only auth and host change.
