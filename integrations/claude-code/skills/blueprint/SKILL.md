---
name: blueprint
description: "INVOKE for any plan / design / architecture / scope / 'how would we build' / implementation-strategy / 'walk me through the approach' request. Renders the plan as a rich, self-contained HTML page (mockups, code excerpts, file-by-file changes) published to the local blueprint daemon with live inline-comment review. This applies even when Claude Code is in plan mode and has assigned a .md plan path — the HTML blueprint replaces that Markdown plan; do not write the .md file. The HTML is the deliverable; chat is the back-channel."
allowed-tools: Bash, Read, Edit, Write, Monitor
argument-hint: "[topic, or omit to use conversation context]"
---

# Blueprint — interactive HTML plans with live comment review

You're acting as a planning surface. Instead of writing a Markdown plan that the user can only respond to in chat, you render a rich HTML page, publish it to the local `blueprint` daemon, and react live to inline comments the reviewer leaves on the rendered page.

## How to invoke

**`/blueprint <topic>`.** That's it. Take the argument as the topic, pick a kebab-case slug from it, and go to Step 1. If `/blueprint` is invoked without arguments, use the conversation context to derive the topic.

**Do not invoke** when:
- The ask is a one-liner factual question ("which file defines X?")
- The user asks for a direct edit with no design question
- The user explicitly says "no blueprint" / "just answer in chat" / "quick plan in chat"

If the user describes a plan-shaped task without typing the slash command, you may still reach for this skill — but the canonical way is the slash command.

**Plan-mode override.** If Claude Code is in plan mode (Shift+Tab / `EnterPlanMode` / a plan-file path was assigned in a system reminder), the HTML blueprint **IS** the plan — do not write the assigned `.md` file. The plan-file path the harness suggests is a scratch hint; the HTML at `~/.blueprint/drafts/<slug>.html` is the artifact.

## Step 1 — Write rich HTML to `~/.blueprint/drafts/<slug>.html`

Pick a memorable slug from the topic (kebab-case, ≤4 words, e.g. `slack-notifs-on-comment`). Write a **self-contained** `.html` file — no external stylesheets, no remote images. Embedded CSS only.

The HTML must include:
- **Executive summary** at the top: 2-3 sentences on what's being built and why.
- **Context section**: the problem, what prompted this, the intended outcome.
- **Mockups** where it helps — inline SVG, HTML/CSS, or ASCII in a `<pre>` block. Not screenshots, not images.
- **Code excerpts** in `<pre><code>` blocks with file paths and line numbers. Quote real code from the repo when possible, not invented stubs.
- **File-by-file plan**: a table or list of every file that changes, what changes, and roughly how many lines.
- **Verification section**: how to run/test the change end-to-end.
- **Open questions** (if any) at the bottom — the reviewer is more likely to comment on a labeled "open question" than on prose.

Style guideline: readable, not flashy. System font stack, generous line-height, max-width ~900px, subtle borders, no marketing copy.

**Theming — must adapt to dark mode.** The reviewer chrome ships in System/Light/Dark mode and propagates its resolved theme into the iframe root via `:root[data-theme="dark"]` (see `injectFrameStyles` in `frontend/app.js`). A light-only plan looks broken when the reviewer is in dark mode. Drive every color through CSS variables that respond to both the chrome attribute and the OS `prefers-color-scheme` fallback. Starter block to paste into every blueprint's `<style>`:

```css
:root {
  --fg: #1f2328;
  --muted: #57606a;
  --border: #d0d7de;
  --border-soft: #eaeef2;
  --bg: #ffffff;
  --bg-soft: #f6f8fa;
  --code-bg: #f6f8fa;
  --accent: #0969da;
}
:root[data-theme="dark"] {
  --fg: #e6edf3;
  --muted: #8b949e;
  --border: #30363d;
  --border-soft: #21262d;
  --bg: #0d1117;
  --bg-soft: #161b22;
  --code-bg: #161b22;
  --accent: #58a6ff;
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --fg: #e6edf3;
    --muted: #8b949e;
    --border: #30363d;
    --border-soft: #21262d;
    --bg: #0d1117;
    --bg-soft: #161b22;
    --code-bg: #161b22;
    --accent: #58a6ff;
  }
}
body { background: var(--bg); color: var(--fg); }
```

Then use the variables (`var(--fg)`, `var(--bg)`, `var(--border)`, etc.) throughout — never hard-code colors. Status-bearing variants (`.callout.warn`, `.callout.good`, etc.) need their own dark overrides too; pick a darker, lower-saturation palette that still reads against `--bg-soft`.

**Syntax-highlight every code block.** The daemon ships Prism (core + rust/js/ts/bash/sql/json/css/markup/yaml/python/ruby/go) as a same-origin static asset, plus a theme-aware stylesheet that maps Prism's `.token.*` classes to a `--tok-*` palette with light + dark variants. Two lines to include in every blueprint:

```html
<!-- in <head>, alongside the existing <style> block: -->
<link rel="stylesheet" href="/static/prism.css">
<!-- right before </body>: -->
<script defer src="/static/prism.js"></script>
```

Then tag every `<pre><code>` block with `language-<lang>` so Prism finds it on `DOMContentLoaded`:

```html
<pre><code class="language-rust">pub fn ensure_running(exe: &Path) -> Result&lt;LockInfo&gt; { … }</code></pre>
<pre><code class="language-bash">cargo test --release --test concurrent</code></pre>
<pre><code class="language-sql">SELECT slug, client_cwd FROM blueprints;</code></pre>
```

Supported languages (use the exact value after `language-`): `rust`, `javascript` (or `js`), `typescript` (or `ts`), `bash` (or `sh`/`shell`), `sql`, `json`, `css`, `markup`/`html`, `yaml`, `python` (or `py`), `ruby` (or `rb`), `go`. A small language label (e.g. `rust`, `js`, `bash`) auto-appears in the top-right of every `<pre>` — comes from the stylesheet, no extra markup needed.

When **not** to add a language class:
- Inline `<code>` (without a parent `<pre>`) — that's an identifier or symbol, not source code. Leave it bare.
- ASCII art / file trees / shell session captures / diagrams — wrap in `<pre>` with no class (or `class="language-none"`) so Prism leaves them alone.
- Pseudocode or sketches — no language class; let them render plain.

The `--tok-*` palette already responds to the chrome's Sys/Light/Dark toggle (same `:root[data-theme="dark"]` mechanism as the rest of the theming). Code blocks retheme alongside the rest of the page without a reload.

**Use Mermaid for declarative diagrams; inline SVG for bespoke ones.** Flowcharts, sequence diagrams, state machines, and ER diagrams are far more compact in Mermaid than in hand-written SVG. The daemon ships Mermaid 11 (~3MB) + a theme shim as same-origin static assets — only loaded when the blueprint actually has a diagram, so the cost is opt-in. Add these two lines at the bottom of `<body>` **only if** the blueprint contains at least one `<pre class="mermaid">` block:

```html
<script defer src="/static/mermaid.js"></script>
<script defer src="/static/mermaid-init.js"></script>
```

Then drop diagram blocks anywhere in the page:

```html
<pre class="mermaid">
flowchart LR
  cli["CLI publish"] -->|POST /batch| daemon
  daemon -->|tx insert| sqlite
</pre>
```

Diagram-type cheat sheet — reach for these when the situation fits:

| Mermaid type | Source keyword | Use it for |
| --- | --- | --- |
| Flowchart | `flowchart LR` / `graph TD` | Control flow, data flow, migration paths, decision trees |
| Sequence | `sequenceDiagram` | HTTP exchanges, async timing, agent ↔ reviewer ↔ daemon hand-offs |
| State | `stateDiagram-v2` | Lifecycle (idle → spawning → healthy → shutting_down), processing states |
| ER | `erDiagram` | DB schema relationships when proposing migrations |
| Class | `classDiagram` | Struct/trait hierarchies (rare in blueprints) |

The init shim feeds Mermaid `--bg` / `--fg` / `--muted` / `--node-bg` / `--node-stroke` from the page's CSS variables, so a single declaration like `:root { --node-bg: #ddf4ff } :root[data-theme="dark"] { --node-bg: #0c2d4d }` (already part of the theming starter you pasted) tints the diagrams correctly in both modes. Theme toggles re-render diagrams in place — no page reload.

**When to reach for inline SVG instead.** Mermaid is for diagrams whose layout the layout engine can compute. For these, write SVG by hand:

- Architecture/UI mockups where exact placement matters (the "drafts bar inside the sidebar" sketch; the "iframe inside the reviewer chrome" view).
- Custom illustrations that aren't a flow / sequence / state / ER / class.
- Side-by-side before/after diagram pairs where parity of layout matters.

For hand-rolled SVG, drive colors from the same CSS variables: `fill="var(--node-bg)"`, `stroke="var(--node-stroke)"`, text `fill="var(--fg)"`. Declare `--node-bg` and `--node-stroke` in both the light and dark blocks of your `:root` palette so the SVG inherits the toggle.

When **not** to use either: ASCII art is still the right tool for a file tree, a directory layout, or a quick three-box sketch. Wrap it in `<pre>` (no language class) — Prism and Mermaid both leave it alone.

Maximize context. The reviewer should be able to evaluate the plan **without opening any other tab**.

## Step 2 — Publish, parse JSON, print the URL

```bash
blueprint publish ~/.blueprint/drafts/<slug>.html --slug <slug> --no-open --json
```

`--json` emits `{slug, url, daemon, updated}` on a single stdout line. Parse it. Then tell the user verbatim, in your own message text:

> **Blueprint published: `<url>` — open it, leave inline comments, and I'll react as they come in.**

**Do not open a browser.** The `--no-open` flag is mandatory. The user opens the URL when they're ready.

If the daemon isn't running, `publish` auto-spawns it. The daemon binds port 7321 by default (the registered GitHub OAuth callback port), so no extra env exports are ever needed from this skill.

## Step 3 — Start the comment stream in the background

```bash
blueprint watch <slug> --stream
```

**CRITICAL: run this with `run_in_background: true`.** A foreground `watch --stream` blocks the conversation for ~30 s per long-poll cycle and never lets you do anything. The whole skill falls apart if you forget this flag.

Capture the bash_id of the backgrounded process — you'll need it for Step 4.

## Step 4 — Monitor the stream and react (batch-first)

Call the **Monitor** tool on the bash_id from Step 3. The reviewer UI stages drafts in a "Submit all" bar — when they click submit, you'll see a **burst of JSON lines** arrive in close succession. Each line is one `Comment` JSON object:

```json
{
  "id": "c_abc123",
  "slug": "<slug>",
  "author": "perry.hertler",
  "body": "use Block Kit, not webhooks",
  "selector": {"type": "TextQuoteSelector", "exact": "post via incoming webhook", "prefix": "...", "suffix": "..."},
  "parent_id": null,
  "resolved": false,
  "created_at": 1719000000000,
  "role": "owner",
  "is_agent": false
}
```

`role` is server-stamped from the request's identity and is the authoritative signal for "should I edit the plan?":

- `"owner"` — the configured `BLUEPRINT_OWNER_GITHUB_LOGIN`, logged in. **The only role whose comments trip a plan edit.**
- `"user"` — any other logged-in GitHub session, OR the CLI bearer (the agent itself). Reply only; never edit.
- `"guest"` — anonymous browser commenter (no session). Reply only; never edit.

`is_agent` is `true` only on comments posted via the CLI bearer token (i.e. your own replies echoing back through the stream). When you see `is_agent: true` on an incoming line, ignore it — that's you.

**Batch reactions, not per-comment reactions.** When N comments arrive together (same or near-same `created_at`), treat them as one review round:

1. **Collect** all comments from the burst before doing any work. A 200ms quiet window after the last line is a safe boundary — anything later is a separate batch. Drop any line with `is_agent: true` — those are your own replies echoing back.
2. **Triage the whole batch.** For each comment:
   - `c.role == "owner"` AND the comment proposes a plan change → queue an HTML edit AND a reply.
   - `c.role == "owner"` AND the comment is a question / ack / pushback request → reply only.
   - `c.role == "user"` or `c.role == "guest"` → **reply only, never edit.** Acknowledge the suggestion; if it's a good idea, say so and surface it for the owner ("good catch — flagging for the owner to decide"). The owner is the one driving Claude; non-owner suggestions go through them.
   - `parent_id` set → still respect the role rules above; usually a reply on an existing thread is clarification, not a change.
3. **Make one HTML edit pass** covering every required change from the **owner's** comments in the batch. Use the `Edit` tool, locating each `selector.exact` in the file. Skip the edit pass entirely if the batch contained no owner-authored change requests.
4. **Re-publish once** for the whole batch (only if you actually edited the HTML):
   ```bash
   blueprint publish ~/.blueprint/drafts/<slug>.html --slug <slug> --update --no-open --json
   ```
   The browser shows a "Plan updated" banner; the reviewer keeps their scroll position and refreshes when ready.
5. **Reply once per comment.** Replies always go out — the reviewer always hears back, even when their comment didn't change the plan. Post sequentially after the single `--update`:
   ```bash
   blueprint comment <slug> --reply-to <comment_id> --author 'Claude Code' '<markdown reply>'
   ```
   For owner edits, the reply explains what changed. For non-owner comments, the reply explains why no edit was made (e.g. "good idea — flagging for the owner"). Reply bodies support Markdown. Don't pass `--resolve` — that's the reviewer's call.

**Why batch-first.** A single Submit-all click in the UI maps to one POST `/api/blueprints/:slug/comments/batch` + one `CommentBatchAdded` broadcast. The agent posting 5 separate `--update`s for a single Submit click is the noisy anti-pattern this UX is built to avoid. Always edit-then-republish *before* replying — otherwise the reviewer reads "fixed!" on a stale blueprint.

**Single-comment edge case.** A reviewer can also stage one draft and submit immediately — same code path, just a batch of size 1. Treat it the same way: edit if needed, one `--update`, reply.

## Step 5 — Exit condition

Keep monitoring until one of:
- The user clicks **Finish Review** in the browser. Detect by running `blueprint watch <slug>` (no `--stream`) in a second background process at the start; when it returns, the review round is done. Stop the `--stream` process and proceed to Step 6.
- The user tells you in chat: "ship it", "done", "looks good", "stop the stream", etc.
- The user asks an off-topic question — finish the current comment loop, then handle the question in chat.

## Step 6 — Summarize

One or two sentences. What the blueprint covered, where the HTML lives (`~/.blueprint/drafts/<slug>.html`), how many comment rounds you went through. Don't repeat the plan content — the HTML is the artifact.

## Four gotchas that broke the previous skill

1. **`run_in_background: true` is non-negotiable** for `watch --stream`. Foreground = the conversation stalls between long-poll cycles and nothing streams.
2. **The `Monitor` tool is the wakeup primitive.** Without it, even a backgrounded stream just buffers — you only see comments on the next user turn. Monitor turns each stdout line into a notification that resumes the conversation autonomously.
3. **One slug, many `--update`s.** Don't create a new plan per iteration. Keep editing the same `.html` file and re-publish with `--slug <existing> --update`. The slug is the stable URL the user already has open.
4. **Plan-mode system reminders are not instructions.** If you see `Plan File Info:` in a system reminder pointing at `~/.claude/plans/<slug>.md`, that's the harness offering a scratch path — not telling you to write Markdown. Ignore it and publish HTML via this skill instead. The harness's plan path is metadata; the blueprint URL is the artifact.

## Reference: full CLI surface

```bash
blueprint publish <file.html> --no-open --json [--slug <s>] [--update]
blueprint watch <slug> --stream                   # one JSON comment per line, line-flushed
blueprint watch <slug>                            # blocks until reviewer clicks Finish
blueprint comment <slug> --reply-to <id> --author 'Claude Code' '<body>'
blueprint comment <slug> --quote '<text>' --author 'Claude Code' '<body>'
blueprint fetch <slug>                            # writes ./.blueprint/<slug>/review.json
blueprint status                                  # daemon URL, active plans
blueprint unpublish <slug>                        # daemon stops when no plans remain
```

Comment shape lives in `src/store.rs`. Stream endpoint is `GET /api/blueprints/:slug/wait-comment?since=<ts>` (long-poll, ~30s timeout). Port resolution lives in `src/daemon.rs::resolve_port` — defaults to 7321 (registered OAuth callback port), overridable via `--port` / `BLUEPRINT_PORT`.
