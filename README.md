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
- No wasm toolchain needed to build or run: the anchoring module is committed under `frontend/pkg/`. You only need `wasm-pack` if you change `anchor/` — see [Rebuilding the wasm module](#rebuilding-the-wasm-module).
- Claude Code CLI installed and authenticated, if you want the `/blueprint` skill to drive the loop.

### 2. Build and install the binary

```bash
cargo install --path .
```

This drops the `blueprint` binary at `~/.cargo/bin/blueprint`. Sanity-check:

```bash
blueprint --version
```

### 3. (Optional) Configure GitHub OAuth + the owner login

Anonymous browser commenters always work — they always have. What OAuth adds is **provenance**: each comment is tagged with a server-stamped `role`, and only one specific role triggers a plan edit from Claude. There are three roles:

| Role    | Identified by                                                              | Triggers a plan edit? |
| ------- | -------------------------------------------------------------------------- | --------------------- |
| `owner` | logged-in GitHub session whose login matches `BLUEPRINT_OWNER_GITHUB_LOGIN` | **Yes**               |
| `user`  | any other logged-in session (or the CLI/agent bearer)                       | No — reply only       |
| `guest` | anonymous browser commenter (no session)                                    | No — reply only       |

In legacy mode (no OAuth env), everything lands as `guest` and there is no owner; Claude treats every comment as a potential plan edit, same as before this change.

To enable OAuth and the owner role, populate `~/.blueprint/env`:

```ini
# ~/.blueprint/env
GITHUB_CLIENT_ID=Iv1.xxxxxxxxxxxxxxxx
GITHUB_CLIENT_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
BLUEPRINT_OWNER_GITHUB_LOGIN=your-github-login   # case-insensitive
```

Sessions are cookie-backed and stored in `~/.blueprint/blueprints.db` (mode
`0600` — the session id is a bearer credential, so the file is owner-only).
There is no `SESSION_SECRET`: nothing signs the cookie, the random session id
*is* the secret. Sessions survive a daemon restart, which is what keeps the
OAuth round-trip and the owner role intact when the daemon respawns mid-flow.

`BLUEPRINT_OWNER_GITHUB_LOGIN` is **optional**. If you set it without the two `GITHUB_*` vars, the daemon prints a startup WARN — owner-role assignment depends on the OAuth session, so the env var is dead config without it. Same the other way: enable OAuth without setting the owner, and you'll get a different WARN noting that no comment will trip a plan edit.

The registered OAuth app's callback URL is `http://127.0.0.1:7321/auth/github/callback`, so the daemon **must** bind port 7321 for the round-trip to work. That's the default; don't override unless you also re-register the OAuth app. To drop back to anonymous mode, delete `~/.blueprint/env` (or unset the two `GITHUB_*` vars) and restart the daemon.

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

The skill lives in this repo at `integrations/claude-code/skills/blueprint/`. Symlink it into your Claude Code skill directory:

```bash
ln -s "$PWD/integrations/claude-code/skills/blueprint" ~/.claude/skills/blueprint
```

Verify Claude Code sees it — start `claude` in any repo and run `/help`; `/blueprint` should be listed under user-invocable skills.

### 6. Drive the loop with Claude

Inside a Claude Code session in the repo you're planning against:

```
/blueprint <one-line topic>
```

That's the whole interface. Examples:

```
/blueprint add slack notifications when a comment lands
/blueprint scope out multi-tenant support
/blueprint refactor the comment-batching path
```

The skill will:

1. Write a self-contained HTML blueprint (executive summary, mockups, file-by-file plan, verification steps) to `~/.blueprint/drafts/<slug>.html`.
2. Run `blueprint publish --no-open --json` and print the `127.0.0.1:7321/b/<slug>` URL back at you to open when you're ready.
3. Start `blueprint watch <slug> --stream` in the background and use the `Monitor` tool to wake on each Submit-all batch, plus a plain `blueprint watch <slug>` to catch the **Finish Review** click.
4. On each batch: triage by role (owner edits → HTML edit + reply; user / guest comments → reply only — see Step 3's role table), then `blueprint publish --slug <slug> --update` once per batch and post threaded replies. The browser shows a "Plan updated" banner; click Refresh to reload the iframe in place.

Stage drafts in the sidebar and hit **Submit all** — that's one round trip, one wake-up. When you're done, click **Finish Review** in the browser or tell Claude in chat to wrap up; `blueprint watch` exits and the loop ends. The click is recorded server-side, so it isn't lost if Claude's waiter hasn't reconnected yet, and the button keeps showing when the review was finished across reloads.

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

**Anchoring.** Comments store a [TextQuoteSelector](https://www.w3.org/TR/annotation-model/#text-quote-selector) — `exact` text plus 32 UTF-16 code units of `prefix` and `suffix`. The browser walks the iframe's text nodes, finds the occurrence whose context matches, wraps the range in a `<span data-ps-hl>`, and binds a click handler that scrolls the matching comment group in the sidebar.

Choosing *which* occurrence lives in the `blueprint-anchor` crate, compiled to wasm and loaded by `anchor.js` — one implementation shared by the daemon and the browser rather than a Rust copy and a JS copy kept in step by hand. The JS keeps what needs a DOM: flattening text, mapping offsets onto text nodes, building the Ranges. Offsets are UTF-16 code units end to end, because that is what JS string indices are; see the crate docs for why context crosses the boundary as a `Uint16Array` and not a string.

Three outcomes, three renderings:

| Outcome | Sidebar |
| --- | --- |
| Context confirmed the occurrence | no badge |
| Quote found, but no occurrence matched the recorded context | **may be misplaced** (outlined badge) |
| `exact` no longer appears at all | **drifted** (yellow badge) |

The middle case used to be indistinguishable from the first: resolution returned a bare index, so a comment whose surroundings had been edited would silently attach to the first occurrence of the quote — some other paragraph — and present as a clean match. It still anchors there, because reporting drift on text that is plainly still present tested worse; it now says so.

**Bidirectional click-to-scroll.** Click a highlighted span → sidebar group flashes blue. Click a sidebar group's body → the highlight in the iframe pulses. Inputs/buttons inside the group are excluded so reply forms still work.

**Threaded comments with collapse.** Replies render nested under their parent at arbitrary depth. Each thread can be collapsed via the quote bar (showing a "N messages" badge); a sidebar header offers **Collapse all** / **Expand all**. Collapse state persists in `localStorage` per-slug.

**Batch comment submission.** The reviewer stages drafts in a sidebar panel and clicks **Submit all** once. One POST → one broadcast event → one agent wake-up, instead of N noisy round-trips.

**Update banner, not auto-reload.** When the blueprint is `--update`d, the `blueprint_version` bumps. The frontend polls every 1.5s and, on version change, shows a "Blueprint updated" banner with **Refresh** and **Dismiss** buttons — reviewers keep their scroll position and refresh on their own terms.

**Live monitoring for agents.** `blueprint watch <slug> --stream` long-polls `/api/blueprints/:slug/wait-comment` and prints one JSON line per new comment to stdout, with line-flushed output suitable for piping into an agent that reacts event-driven. Reconnects automatically if the daemon restarts.

**Cache hygiene.** `/static/*` and `/api/blueprints/:slug/raw` set `Cache-Control: no-store` so a browser never serves stale frontend assets or stale blueprint HTML.

**Roles and the write gate.** Every comment is server-stamped with a `role` (`owner` / `user` / `guest`) and an `is_agent` boolean derived from the request's identity at write time (see `src/auth.rs::role_for` and `::is_agent`). The Claude skill keys its triage off `c.role` — only `owner` comments trigger plan edits; everyone else gets a reply only.

Comments themselves have no 401 gate: anonymous ones land as `guest`, and provenance via the `role` tag is the defense — that's what makes drive-by review work. Destructive requests are gated, though. Once OAuth is configured, creating, replacing, or deleting a blueprint and `POST /api/shutdown-if-empty` all require a session or the CLI bearer, and answer 401 without one (`WriteKind::Blueprint` in `src/auth.rs`). With no OAuth configured the daemon stays in legacy local-trust mode and enforces nothing, since that's the only way the CLI works before credentials exist. See Step 3 above for the env-var configuration.

**Batch-processing indicator.** When the skill wakes on a Submit-all batch, it calls `blueprint batch-processing start <slug> --parent <id>...` to light a slug-level "Claude is working on N comments" pill in the sidebar. The server tracks the batch's `pending_parents` and auto-clears the pill when the last reply lands — no explicit DELETE needed on the happy path. A 5-minute TTL evicts stale entries if the skill crashes mid-batch.

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
| `GET`    | `/api/me`                                           | `{ id, login, name, avatar_url, is_owner }` or 401 — used by the chrome    |
| `POST`   | `/api/blueprints/:slug/batch-processing`            | `{ "author": "Claude Code", "parent_ids": [...] }` — light the indicator   |
| `DELETE` | `/api/blueprints/:slug/batch-processing`            | Clear the indicator. Mostly redundant — the server auto-clears on replies  |

Once OAuth is configured, `POST`/`PUT`/`DELETE /api/blueprints` and `POST /api/shutdown-if-empty` require a session cookie or the CLI bearer and answer **401** without one. Everything else — including posting comments — is open. See **Roles and the write gate** above.

Every `Comment` returned by the API includes `role` (`"owner" | "user" | "guest"`) and `is_agent` (boolean) alongside `author`, `body`, `selector`, etc. — both fields are server-stamped from the request's identity, not from anything the client sends. The comments-list response also includes an optional `batch_processing: { author, count, started_at }` while the agent is working on a Submit-all batch.

## Architecture

```
blueprint/
├── Cargo.toml              # workspace root + the daemon package
├── src/
│   ├── lib.rs              # public re-exports for tests
│   ├── main.rs             # CLI entry
│   ├── cli.rs              # publish / watch / fetch / comment / unpublish / status / watch --stream
│   ├── daemon.rs           # detached-child spawn, PID-scoped lock file, spawn flock
│   ├── server.rs           # axum routes + AppState (blueprint_versions, finish latch, events broadcast)
│   ├── store.rs            # rusqlite — blueprints, comments, users, batch-insert, migrations
│   ├── session_store.rs    # SQLite-backed session store, so OAuth survives a restart
│   ├── auth.rs             # GitHub OAuth + session wiring + CLI bearer + the write gate
│   ├── selector.rs         # re-export of the selector type from blueprint-anchor
│   ├── slug.rs             # adjective-month-animal generator + entropy suffix
│   ├── mcp.rs              # stdio MCP server (JSON-RPC 2.0)
│   ├── review_file.rs      # crit-compatible review.json writer
│   └── error.rs            # AppError → axum response
├── anchor/                 # workspace member — the only crate that also targets wasm
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          # quote resolution + the UTF-16 rationale, with its tests
│       ├── selector.rs     # TextQuoteSelector
│       └── wasm.rs         # wasm-bindgen surface (feature = "wasm")
├── frontend/               # embedded via rust-embed
│   ├── index.html          # landing
│   ├── reviewer.html       # /b/:slug shell — iframe, sidebar, drafts bar, update banner
│   ├── app.js              # wiring: DOM lookups, app state, delegated handlers
│   ├── anchor.js           # DOM side of anchoring; calls into the wasm module
│   ├── render.js           # sidebar rendering (no innerHTML)
│   ├── poll.js             # polling with backoff + disconnect banner
│   ├── drafts.js           # staged drafts, per-slug localStorage
│   ├── dom.js, toast.js    # small shared helpers
│   ├── pkg/                # wasm-pack output — CHECKED IN, see Tests
│   └── styles.css
├── js-tests/               # vitest, jsdom — the frontend modules
├── integrations/
│   └── claude-code/skills/blueprint/SKILL.md
└── tests/
    ├── e2e.rs              # in-process daemon, HTTP-level e2e, OAuth, write gate
    ├── concurrent.rs       # multi-repo + batch endpoint
    ├── finish.rs           # the exactly-once finish latch
    ├── security.rs         # /raw sandboxing, body cap, long-poll pools
    ├── mcp.rs              # MCP protocol + tools/call
    ├── versioning.rs       # --update archives prior HTML
    └── cli_smoke.rs        # spawns the binary, round-trips publish/status/unpublish
```

Stack: `axum`, `tokio`, `rusqlite` (bundled), `nix` (flock), `serde`, `clap`, `rust-embed`, `reqwest`, `oauth2`, `tower-sessions`, `parking_lot`. Frontend: no framework, no bundler — native ES modules plus one wasm module (`wasm-bindgen`).

## Tests

```bash
cargo test --workspace   # Rust: daemon, store, CLI, MCP, and quote resolution
npm test                 # frontend ES modules, via vitest
```

`--workspace` matters: without it cargo tests only the root package and skips
`anchor/`, where the resolution algorithm and its cases live.

### Rebuilding the wasm module

`frontend/pkg/` is a build output that is **checked in**, because `rust-embed`
reads `frontend/` at compile time — so the artifact has to exist before
`cargo build`, or `cargo install --path .` produces a daemon that 404s its own
anchoring module. Nobody needs `wasm-pack` to build or run blueprint; you need it
only when you change `anchor/`:

```bash
cargo install wasm-pack --version 0.15.0 --locked   # once
wasm-pack build anchor --target web --out-dir ../frontend/pkg \
  --out-name anchor --no-typescript --no-pack --release -- --features wasm
rm -f frontend/pkg/.gitignore   # wasm-pack writes an ignore-everything file
```

Then commit the result. A forgotten rebuild is the one failure mode every other
check passes — the Rust tests exercise the source, the JS tests and production
exercise the stale artifact — so CI guards it in two parts:

- **The glue** (`anchor.js`) is compared byte for byte. wasm-bindgen's JS output
  is deterministic, and it changes whenever the exported API does.
- **The `.wasm`** is compared *behaviourally* against a fresh build, by
  `scripts/check-wasm-parity.mjs`, which runs ~3,000 cases through both modules
  and requires identical answers. Bytes can't be compared here: wasm codegen
  isn't reproducible across platforms, so a macOS laptop and an ubuntu runner
  emit functionally identical modules that differ byte for byte.

The corpus is mostly seeded-random, plus hand-written cases for the shapes random
generation won't reliably produce — self-overlapping quotes especially, which are
the only way to observe the scan's step size.

Rust covers: publish → comment → reply → finish → fetch → update → drift → unpublish; random-slug generation; empty-HTML / empty-body / unknown-parent rejection; the `GET /api/blueprints` summary shape; `wait-comment` fast-path and slow-path; OAuth round-trip against a mock GitHub; CLI bearer-token write-auth and the blueprint write gate in both directions; multi-repo concurrency (`shutdown-if-empty`, `X-Client-Cwd`); the batch endpoint (atomicity, single wake-up); schema migrations; that the wasm module is embedded and served as `application/wasm`; and a CLI subprocess smoke test.

`anchor/` covers quote resolution as a table of cases: duplicate quotes disambiguated by prefix, suffix, or both; the known-wrong first-occurrence fallback, now asserted to *report itself* as a fallback; a prefix longer than the window; a prefix opening on an unpaired surrogate, which is the case that forces code units rather than `&str`; combining marks left unnormalized; and the empty quote that made the JS original spin forever.

JS covers the frontend modules with real behaviour, not smoke tests: the DOM side of anchoring against the actual wasm module (so the JS↔wasm boundary is exercised, not mocked) — offsets landing on text-node boundaries, quotes spanning inline markup, emoji ahead of the quote — plus poll backoff and the disconnect banner, draft storage, and rendering.

Not covered by either suite: click-to-scroll and collapse interactions in a live browser (manual verification only).

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
