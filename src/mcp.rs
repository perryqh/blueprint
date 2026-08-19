//! Stdio MCP server — a second front door onto the daemon.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout (newline-delimited, one message per
//! line), the transport Claude Code and other MCP clients use for a local
//! `command`-style server. It exposes five tools that wrap the exact same
//! daemon HTTP API the CLI drives, so the role-gated review loop works
//! identically whether an agent goes through the `/blueprint` skill (CLI) or
//! connects this server directly.
//!
//! The server is a thin process: it holds no state, spawns/reuses the daemon
//! over HTTP (`ensure_daemon` on publish, `require_running` otherwise), and
//! reuses `cli_http_client` so it inherits the CLI bearer token and
//! `X-Client-Cwd` header — meaning its writes are stamped `is_agent = true`
//! and role `user`, exactly like the CLI's, never tripping an owner edit.

use crate::cli::{base_url, cli_http_client, ensure_daemon, require_running};
use anyhow::Result;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Protocol revision we advertise in `initialize`. Kept deliberately at a
/// widely-supported baseline; the tool surface is unchanged across revisions.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Largest stdin line we'll buffer, in bytes. `publish_blueprint` carries a whole
/// HTML document as a single JSON line, so the cap has to be generous — but
/// unbounded means one malformed or hostile line can make us allocate until the
/// OOM killer decides the matter. Over the cap we answer with a JSON-RPC error
/// and keep serving.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Read-dispatch-write loop over stdio. Each stdin line is one JSON-RPC
/// message; requests (with an `id`) get exactly one response line, flushed
/// immediately so a client blocked on our stdout unblocks at once.
/// Notifications (no `id`, e.g. `notifications/initialized`) are consumed
/// silently. EOF on stdin ends the loop.
pub async fn run() -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();

    loop {
        let line = match read_line(&mut reader).await {
            ReadLine::Eof => return Ok(()),
            ReadLine::Line(l) => l,
            // A transient read error or a client that sent invalid UTF-8 must not
            // take the server down with it — only EOF ends the loop. Skipping the
            // line loses one message; exiting loses the session.
            ReadLine::Skip(reason) => {
                tracing::warn!(%reason, "skipping unreadable stdin line");
                continue;
            }
            ReadLine::TooLong => {
                write_response(&mut stdout, &parse_error("line exceeds the size limit")).await?;
                continue;
            }
        };

        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // The spec's answer to unparseable input is a response with a null
            // id and -32700, not silence: a client that sent slightly-malformed
            // JSON would otherwise hang until its own timeout expires.
            Err(e) => {
                write_response(&mut stdout, &parse_error(&e.to_string())).await?;
                continue;
            }
        };

        // A notification is a message with *no* id. `"id": null` deserializes to
        // `Some(Value::Null)`, which a bare `let Some(id)` happily accepts — and
        // then we'd reply to e.g. `notifications/cancelled`, which JSON-RPC 2.0
        // forbids outright.
        let Some(id) = msg.get("id").filter(|v| !v.is_null()).cloned() else {
            continue;
        };

        let response = match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => initialize_result(&id),
            Some("tools/list") => tools_list_result(&id),
            Some("tools/call") => tools_call_result(&id, msg.get("params")).await,
            Some("ping") => ok_response(&id, json!({})),
            Some(other) => {
                error_response(&id, METHOD_NOT_FOUND, &format!("method not found: {other}"))
            }
            // No `method` at all isn't an unknown method — it isn't a request.
            None => error_response(&id, INVALID_REQUEST, "missing `method`"),
        };

        write_response(&mut stdout, &response).await?;
    }
}

/// JSON-RPC 2.0 reserved error codes, spelled out so the call sites read as
/// intent rather than as magic numbers.
const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;

/// What one read attempt produced. Separating "stop" from "skip this line" is the
/// whole point: only `Eof` may end `run()`.
enum ReadLine {
    Line(String),
    Eof,
    TooLong,
    Skip(String),
}

/// Read one newline-terminated line, capped at `MAX_LINE_BYTES`.
///
/// Reads bytes rather than using `lines()` so an over-long line can be drained to
/// its newline and reported, and so invalid UTF-8 is a per-line problem instead of
/// an error that propagates out of the loop.
///
/// The cap is enforced by filling from the reader's buffer in chunks and checking
/// the running total, rather than by `AsyncReadExt::take` — `take` consumes the
/// reader, which is no good when we need it again on the next iteration.
async fn read_line<R>(reader: &mut R) -> ReadLine
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    loop {
        // Read up to the cap, then one more chunk to learn whether the line
        // actually ran over rather than merely reached the limit exactly.
        let budget = MAX_LINE_BYTES.saturating_sub(buf.len()) + 1;
        let mut chunk = Vec::new();
        match read_until_limited(reader, &mut chunk, budget).await {
            Ok(0) if buf.is_empty() => return ReadLine::Eof,
            // EOF mid-line: treat the partial as a line, so a client that closed
            // without a trailing newline still gets its last message handled.
            Ok(0) => return finish_line(buf),
            Ok(_) => {
                let complete = chunk.ends_with(b"\n");
                buf.extend_from_slice(&chunk);
                if complete {
                    return finish_line(buf);
                }
                if buf.len() > MAX_LINE_BYTES {
                    drain_to_newline(reader).await;
                    return ReadLine::TooLong;
                }
            }
            Err(e) => return ReadLine::Skip(e.to_string()),
        }
    }
}

fn finish_line(buf: Vec<u8>) -> ReadLine {
    match String::from_utf8(buf) {
        Ok(s) => ReadLine::Line(s),
        Err(e) => ReadLine::Skip(format!("invalid utf-8: {e}")),
    }
}

/// `read_until(b'\n')` that stops after `limit` bytes even if no newline arrives.
async fn read_until_limited<R>(
    reader: &mut R,
    out: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    while out.len() < limit {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }
        let take = (limit - out.len()).min(available.len());
        match available[..take].iter().position(|&b| b == b'\n') {
            Some(i) => {
                out.extend_from_slice(&available[..=i]);
                reader.consume(i + 1);
                break;
            }
            None => {
                out.extend_from_slice(&available[..take]);
                reader.consume(take);
            }
        }
    }
    Ok(out.len())
}

/// Skip the remainder of an oversized line so the next read starts on a message
/// boundary rather than mid-JSON, where it would fail to parse and desynchronize
/// us from the client for every message after it.
async fn drain_to_newline<R>(reader: &mut R)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let Ok(available) = reader.fill_buf().await else {
            return;
        };
        if available.is_empty() {
            return;
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(i) => {
                reader.consume(i + 1);
                return;
            }
            None => {
                let n = available.len();
                reader.consume(n);
            }
        }
    }
}

/// Serialize one response and flush it, so a client blocked on our stdout
/// unblocks immediately rather than when the buffer happens to fill.
async fn write_response<W>(stdout: &mut W, response: &Value) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut out = serde_json::to_string(response)?;
    out.push('\n');
    stdout.write_all(out.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

fn error_response(id: &Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A parse failure has no id to correlate against, and the spec says to send
/// `null` rather than guess.
fn parse_error(detail: &str) -> Value {
    error_response(&Value::Null, PARSE_ERROR, &format!("parse error: {detail}"))
}

/// Wrap a result payload in the JSON-RPC 2.0 success envelope. One place so the
/// envelope shape can't drift between the initialize/tools-list/tools-call/ping
/// responses.
fn ok_response(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn initialize_result(id: &Value) -> Value {
    ok_response(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "blueprint", "version": env!("CARGO_PKG_VERSION") }
        }),
    )
}

fn tools_list_result(id: &Value) -> Value {
    ok_response(id, json!({ "tools": tool_specs() }))
}

/// The five tools, mirroring the CLI surface. Schemas are intentionally small:
/// a blueprint is a single HTML artifact, so there is no per-surface sprawl.
fn tool_specs() -> Value {
    json!([
        {
            "name": "publish_blueprint",
            "description": "Publish a self-contained HTML blueprint and return its review URL. Auto-spawns the daemon if needed. Reviewers open the URL, leave inline anchored comments, and you react via wait_for_comments.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "html": { "type": "string", "description": "Self-contained HTML (embedded CSS; /static/prism.css and /static/mermaid.js are available same-origin)." },
                    "slug": { "type": "string", "description": "Optional stable slug; omit for a random adjective-month-animal slug." }
                },
                "required": ["html"]
            }
        },
        {
            "name": "update_blueprint",
            "description": "Replace a published blueprint's HTML in place. The prior version is archived and comments authored against it keep resolving against their original text; the reviewer sees a 'Plan updated' banner.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "html": { "type": "string" }
                },
                "required": ["slug", "html"]
            }
        },
        {
            "name": "wait_for_comments",
            "description": "Long-poll for new comments (~30s server timeout). Returns {comments, server_ts, blueprint_version, batch_processing}; pass the returned server_ts back as `since` on the next call to stream only new comments. Each comment carries a server-stamped `role` (owner/user/guest) — only `owner` comments should trip a plan edit.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "since": { "type": "integer", "minimum": 0, "description": "Epoch-ms cursor; pass the prior response's server_ts. Omit (0) to read the full backlog once." }
                },
                "required": ["slug"]
            }
        },
        {
            "name": "reply_to_comment",
            "description": "Post a threaded reply to a comment. Reply bodies support Markdown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "comment_id": { "type": "string" },
                    "body": { "type": "string" },
                    "author": { "type": "string", "description": "Defaults to 'Claude Code'." }
                },
                "required": ["slug", "comment_id", "body"]
            }
        },
        {
            "name": "list_blueprints",
            "description": "List published blueprints with comment counts and last-activity times.",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

/// Run one `tools/call`.
///
/// Two failure shapes, deliberately kept apart. A tool that *ran* and failed gets
/// `isError: true` inside a success envelope — the MCP convention for a failure
/// the model should read and reason about. A call that was never valid to begin
/// with (unknown tool, missing required argument) gets a real `-32602`. Folding
/// both into `isError` costs the model the one signal it needs to decide whether
/// retrying could possibly help.
async fn tools_call_result(id: &Value, params: Option<&Value>) -> Value {
    let params = params.cloned().unwrap_or_else(|| json!({}));
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match dispatch(name, args).await {
        Ok(text) => ok_response(id, json!({ "content": [{ "type": "text", "text": text }] })),
        Err(CallError::InvalidParams(msg)) => error_response(id, INVALID_PARAMS, &msg),
        Err(CallError::Failed(e)) => ok_response(
            id,
            json!({
                "content": [{ "type": "text", "text": format!("error: {e}") }],
                "isError": true
            }),
        ),
    }
}

/// Why a `tools/call` didn't produce a result — see `tools_call_result` for why
/// the distinction is load-bearing.
enum CallError {
    /// The request itself was malformed. Never reached the daemon.
    InvalidParams(String),
    /// The operation ran and failed.
    Failed(anyhow::Error),
}

/// Blanket so `?` keeps working on the reqwest/serde/anyhow errors inside each
/// tool body. Everything that escapes an HTTP call is a `Failed`; only the
/// explicit `InvalidParams` construction sites classify a request as malformed.
impl<E> From<E> for CallError
where
    E: Into<anyhow::Error>,
{
    fn from(e: E) -> Self {
        CallError::Failed(e.into())
    }
}

type CallResult = std::result::Result<String, CallError>;

async fn dispatch(name: &str, args: Value) -> CallResult {
    match name {
        "publish_blueprint" => publish(args).await,
        "update_blueprint" => update(args).await,
        "wait_for_comments" => wait_for_comments(args).await,
        "reply_to_comment" => reply(args).await,
        "list_blueprints" => list().await,
        other => Err(CallError::InvalidParams(format!("unknown tool: {other}"))),
    }
}

fn req_str(args: &Value, key: &str) -> std::result::Result<String, CallError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| CallError::InvalidParams(format!("`{key}` is required")))
}

/// Drain a reqwest response into its body text, erroring with status + body on
/// non-2xx so the model sees the daemon's actual complaint (e.g. a 413 or a
/// validation 400). Reuses the CLI's `ensure_success` for the error shape.
///
/// Always a `Failed`, never `InvalidParams`: by this point the call was
/// well-formed enough to reach the daemon, so whatever went wrong is the
/// operation's outcome rather than the request's shape.
async fn body_or_err(resp: reqwest::Response, what: &str) -> CallResult {
    Ok(crate::cli::ensure_success(resp, what).await?.text().await?)
}

async fn publish(args: Value) -> CallResult {
    let html = req_str(&args, "html")?;
    let slug = args.get("slug").and_then(Value::as_str);
    let info = ensure_daemon().await?;
    let resp = cli_http_client()
        .post(format!("{}/api/blueprints", base_url(&info)))
        .json(&json!({ "html": html, "slug": slug }))
        .send()
        .await?;
    let text = body_or_err(resp, "publish").await?;
    let v: Value = serde_json::from_str(&text)?;
    let slug_out = v.get("slug").and_then(Value::as_str).unwrap_or_default();
    Ok(json!({
        "slug": slug_out,
        "url": format!("{}/b/{}", base_url(&info), slug_out)
    })
    .to_string())
}

async fn update(args: Value) -> CallResult {
    let slug = req_str(&args, "slug")?;
    let html = req_str(&args, "html")?;
    let info = require_running()?;
    let resp = cli_http_client()
        .put(format!("{}/api/blueprints/{}", base_url(&info), slug))
        .json(&json!({ "html": html }))
        .send()
        .await?;
    body_or_err(resp, "update").await?;
    Ok(format!("updated blueprint `{slug}`"))
}

async fn wait_for_comments(args: Value) -> CallResult {
    let slug = req_str(&args, "slug")?;
    let since = args.get("since").and_then(Value::as_i64).unwrap_or(0);
    let info = require_running()?;
    let resp = cli_http_client()
        .get(format!(
            "{}/api/blueprints/{}/wait-comment?since={}",
            base_url(&info),
            slug,
            since
        ))
        // A hair over the daemon's ~30s long-poll so we get its clean empty
        // return rather than tripping the client timeout first.
        .timeout(Duration::from_secs(60))
        .send()
        .await?;
    body_or_err(resp, "wait_for_comments").await
}

async fn reply(args: Value) -> CallResult {
    let slug = req_str(&args, "slug")?;
    let comment_id = req_str(&args, "comment_id")?;
    let body = req_str(&args, "body")?;
    let author = args
        .get("author")
        .and_then(Value::as_str)
        .unwrap_or("Claude Code");
    let info = require_running()?;
    let resp = cli_http_client()
        .post(format!(
            "{}/api/blueprints/{}/comments/{}/replies",
            base_url(&info),
            slug,
            comment_id
        ))
        .json(&json!({ "author": author, "body": body }))
        .send()
        .await?;
    body_or_err(resp, "reply").await
}

async fn list() -> CallResult {
    let info = require_running()?;
    let resp = cli_http_client()
        .get(format!("{}/api/blueprints", base_url(&info)))
        .send()
        .await?;
    body_or_err(resp, "list").await
}
