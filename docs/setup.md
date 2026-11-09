# Local setup

Everything below assumes a fresh clone of this repo and `~/.cargo/bin` on your
`$PATH`. The whole loop runs on `127.0.0.1` — nothing leaves your machine.

For the short path, see the [Quick start](../README.md#quick-start) in the
README. GitHub OAuth (optional, enables the `owner` role) lives in
[oauth.md](oauth.md).

## 1. Prerequisites

- Rust toolchain matching [`rust-toolchain.toml`](../rust-toolchain.toml).
  `rustup` is the easy path and will install the pin automatically.
- macOS or Linux. On Linux, install `libsqlite3-dev` and `pkg-config` if a build
  fails on rusqlite; macOS has them via the SDK. The crate uses the `bundled`
  feature, so a system SQLite is usually unnecessary.
- Claude Code CLI installed and authenticated, if you want the `/blueprint`
  skill to drive the loop.

## 2. Build and install the binary

```bash
cargo install --path .
```

This drops the `blueprint` binary at `~/.cargo/bin/blueprint`. Sanity-check:

```bash
blueprint --version
```

Release binaries for Linux x86_64 and macOS (arm64 + x86_64) are also attached
to [GitHub Releases](https://github.com/perryqh/blueprint/releases) once a `v*`
tag is pushed.

## 3. (Optional) Configure GitHub OAuth + the owner login

Anonymous browser commenters always work. OAuth adds **provenance**: each
comment is tagged with a server-stamped `role`, and only the `owner` role
triggers a plan edit from the agent. Full details — env file, roles table,
callback URL, session storage — are in [oauth.md](oauth.md).

## 4. Smoke-test the daemon manually

```bash
echo '<h1>hello</h1><p>This is a smoke test.</p>' > /tmp/hello.html
blueprint publish /tmp/hello.html              # spawns the daemon, opens the URL in your browser
blueprint status                                # shows running daemon + blueprint list
blueprint comment <slug> --quote "smoke test" 'looks good'
blueprint fetch <slug>                          # writes ./.blueprint/<slug>/review.json
blueprint unpublish <slug>                      # daemon auto-stops when no blueprints remain
```

If the publish step prints a `127.0.0.1:7321` URL and the page renders with a
sidebar, you're wired up. If port 7321 is already in use, `lsof -i :7321` to
find the squatter — or pass `--port` / set `BLUEPRINT_PORT` (OAuth login won't
work on a different port; see [oauth.md](oauth.md)).

## 5. Install the Claude Code skill

The skill lives in this repo at `integrations/claude-code/skills/blueprint/`.
Symlink it into your Claude Code skill directory:

```bash
ln -s "$PWD/integrations/claude-code/skills/blueprint" ~/.claude/skills/blueprint
```

Verify Claude Code sees it — start `claude` in any repo and run `/help`;
`/blueprint` should be listed under user-invocable skills.

Other agents can drive the same loop over the [stdio MCP server](../README.md#cli-reference)
(`blueprint mcp`) or the HTTP API; the Claude skill is the batteries-included
path.

## 6. Drive the loop with Claude

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

1. Write a self-contained HTML blueprint (executive summary, mockups,
   file-by-file plan, verification steps) to `~/.blueprint/drafts/<slug>.html`.
2. Run `blueprint publish --no-open --json` and print the
   `127.0.0.1:7321/b/<slug>` URL back at you to open when you're ready.
3. Start `blueprint watch <slug> --stream` in the background and use the
   `Monitor` tool to wake on each Submit-all batch, plus a plain
   `blueprint watch <slug>` to catch the **Finish Review** click.
4. On each batch: triage by role (owner edits → HTML edit + reply; user / guest
   comments → reply only — see [oauth.md](oauth.md)), then
   `blueprint publish --slug <slug> --update` once per batch and post threaded
   replies. The browser shows a "Plan updated" banner; click Refresh to reload
   the iframe in place.

Stage drafts in the sidebar and hit **Submit all** — that's one round trip, one
wake-up. When you're done, click **Finish Review** in the browser or tell Claude
in chat to wrap up; `blueprint watch` exits and the loop ends. The click is
recorded server-side, so it isn't lost if Claude's waiter hasn't reconnected
yet, and the button keeps showing when the review was finished across reloads.

## 7. Stop and clean up

```bash
blueprint status                  # check what's running
blueprint unpublish <slug>        # remove one blueprint (daemon stops when empty)
pkill -f 'blueprint serve'        # nuke the daemon
rm -rf ~/.blueprint               # full reset — drops SQLite, drafts, env, lock file
```
