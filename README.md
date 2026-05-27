# blueprint

Share interactive HTML blueprints with reviewers and let them leave inline anchored comments — with an HTTP API Claude can drive too.

`crit` shows source; `blueprint` shows the *rendered* page. Same daemon/CLI shape so muscle memory transfers.

## Status

**Phase 0 — localhost MVP.** Single Rust binary, SQLite at `~/.blueprint/blueprints.db`, optional GitHub OAuth, vanilla-JS text-quote anchoring. Phase 1 (personal hosting) and Phase 2 (internal + Okta) are designed but not built.

## Run locally end-to-end

Everything below assumes a fresh clone of this repo and `~/.cargo/bin` on your `$PATH`. The whole loop runs on `127.0.0.1` — nothing leaves your machine.

### 1. Prereqs

- Rust toolchain (1.74+). `rustup` is the easy path.
- macOS or Linux. On Linux, install `libsqlite3-dev` and `pkg-config` if a build fails on rusqlite; macOS has them via the SDK.
- Claude Code CLI installed and authenticated, if you want the `/blueprint` skill to drive the loop.

### 2. Build and install the binary

```bash
cargo install --path .
```

This drops the `blueprint` binary at `~/.cargo/bin/blueprint`. Sanity-check:

```bash
blueprint --version
```

### 3. (Optional) Configure GitHub OAuth

Comments work anonymously out of the box — the daemon accepts `author` from the CLI / API. If you want commenters to sign in with GitHub (so author identity is verified in the browser), populate `~/.blueprint/env`:

```ini
# ~/.blueprint/env
GITHUB_CLIENT_ID=Iv1.xxxxxxxxxxxxxxxx
GITHUB_CLIENT_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
SESSION_SECRET=any-long-random-string-used-as-a-marker
```

The registered OAuth app's callback URL is `http://127.0.0.1:7321/auth/github/callback`, so the daemon **must** bind port 7321 for the round-trip to work. That's the default; don't override unless you also re-register the OAuth app. Missing or partial env = OAuth disabled (and that's fine for local solo use).

### 4. Smoke-test the daemon manually

```bash
echo '<h1>hello</h1><p>This is a smoke test.</p>' > /tmp/hello.html
blueprint publish /tmp/hello.html              # spawns the daemon, opens the URL in your browser
blueprint status                                # shows running daemon + blueprint list
blueprint comment <slug> --quote "smoke test" 'looks good'
blueprint fetch <slug>                          # writes ./.blueprint/<slug>/review.json
blueprint unpublish <slug>                      # daemon auto-stops when no blueprints remain
```

If the publish step prints a `127.0.0.1:7321` URL and the page renders with a sidebar, you're wired up. If port 7321 is already in use, `lsof -i :7321` to find the squatter — or pass `--port` / set `BLUEPRINT_PORT` (OAuth login won't work on a different port).

### 5. Install the Claude Code skill

The skill lives in this repo at `integrations/claude-code/skills/blueprint/`. Symlink it into your Claude Code skill directory so the description triggers on plan-shaped asks:

```bash
ln -s "$PWD/integrations/claude-code/skills/blueprint" ~/.claude/skills/blueprint
```

Verify Claude Code sees it — start `claude` in any repo and run `/help`; you should see `blueprint` listed. The skill is intentionally broad: it auto-triggers on "plan", "design", "scope this out", "what's the implementation strategy for…", etc. — you rarely need to type `/blueprint` literally.

### 6. Drive the loop with Claude

Open a Claude Code session in the repo you're planning against and ask a plan-shaped question — e.g. `"plan how I'd add Slack notifications when a comment lands"`. The skill will:

1. Write a self-contained HTML blueprint (executive summary, mockups, file-by-file plan, verification steps) to `~/.blueprint/drafts/<slug>.html`.
2. Run `blueprint publish --no-open --json` and print the `127.0.0.1:7321/b/<slug>` URL back at you to open when you're ready.
3. Start `blueprint watch <slug> --stream` in the background and use the `Monitor` tool to wake on each Submit-all batch.
4. On each batch: edit the HTML in place, `blueprint publish --slug <slug> --update` (you'll see a "Blueprint updated" banner in the browser), and post threaded replies under each comment.

Stage drafts in the sidebar and hit **Submit all** — that's one round trip, one wake-up. When you're done, click **Finish Review** in the browser or tell Claude in chat to wrap up; `blueprint watch` exits and the loop ends.

### 7. Stop and clean up

```bash
blueprint status                  # check what's running
blueprint unpublish <slug>        # remove one blueprint (daemon stops when empty)
pkill -f 'blueprint serve'        # nuke the daemon
rm -rf ~/.blueprint               # full reset — drops SQLite, drafts, env, lock file
```

## CLI reference

```bash
blueprint publish path/to/blueprint.html        # auto-spawns daemon, opens in browser
blueprint publish file.html --slug <slug> --update   # revise in place; reviewers see a refresh banner
blueprint publish file.html --no-open --json    # script-friendly; prints {slug, url}
blueprint status                                 # show running daemon + blueprints with comment counts
blueprint comment <slug> --quote "..." 'comment body'
blueprint comment <slug> --reply-to c_xxx 'reply body'
blueprint watch <slug>                           # blocks until reviewer clicks Finish
blueprint watch <slug> --stream                  # streams each new comment as JSON to stdout
blueprint fetch <slug>                           # writes ./.blueprint/<slug>/review.json
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
