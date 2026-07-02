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
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Protocol revision we advertise in `initialize`. Kept deliberately at a
/// widely-supported baseline; the tool surface is unchanged across revisions.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Read-dispatch-write loop over stdio. Each stdin line is one JSON-RPC
/// message; requests (with an `id`) get exactly one response line, flushed
/// immediately so a client blocked on our stdout unblocks at once.
/// Notifications (no `id`, e.g. `notifications/initialized`) are consumed
/// silently. EOF on stdin ends the loop.
pub async fn run() -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // A malformed line with no id we can echo — drop it rather than
            // emit a response the client can't correlate.
            Err(_) => continue,
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

        // Notifications carry no id and take no response.
        let Some(id) = id else { continue };

        let response = match method {
            "initialize" => initialize_result(&id),
            "tools/list" => tools_list_result(&id),
            "tools/call" => tools_call_result(&id, msg.get("params")).await,
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            other => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {other}") }
            }),
        };

        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        stdout.write_all(out.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn initialize_result(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "blueprint", "version": env!("CARGO_PKG_VERSION") }
        }
    })
}

fn tools_list_result(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "tools": tool_specs() }
    })
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
                    "since": { "type": "integer", "description": "Epoch-ms cursor; pass the prior response's server_ts. Omit (0) to read the full backlog once." }
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

/// Run one `tools/call`, wrapping the result (or error) in MCP tool-result
/// shape. A tool that fails returns `isError: true` with the message as text —
/// the MCP convention for a tool-level failure the model should see, as
/// opposed to a protocol error.
async fn tools_call_result(id: &Value, params: Option<&Value>) -> Value {
    let params = params.cloned().unwrap_or_else(|| json!({}));
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    match dispatch(name, args).await {
        Ok(text) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "content": [{ "type": "text", "text": text }] }
        }),
        Err(e) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("error: {e}") }],
                "isError": true
            }
        }),
    }
}

async fn dispatch(name: &str, args: Value) -> Result<String> {
    match name {
        "publish_blueprint" => publish(args).await,
        "update_blueprint" => update(args).await,
        "wait_for_comments" => wait_for_comments(args).await,
        "reply_to_comment" => reply(args).await,
        "list_blueprints" => list().await,
        other => bail!("unknown tool: {other}"),
    }
}

fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("`{key}` is required"))
}

/// Drain a reqwest response, erroring with status + body on non-2xx so the
/// model sees the daemon's actual complaint (e.g. a 413 or a validation 400).
async fn body_or_err(resp: reqwest::Response, what: &str) -> Result<String> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("{what} failed: {status} {text}");
    }
    Ok(text)
}

async fn publish(args: Value) -> Result<String> {
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

async fn update(args: Value) -> Result<String> {
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

async fn wait_for_comments(args: Value) -> Result<String> {
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

async fn reply(args: Value) -> Result<String> {
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

async fn list() -> Result<String> {
    let info = require_running()?;
    let resp = cli_http_client()
        .get(format!("{}/api/blueprints", base_url(&info)))
        .send()
        .await?;
    body_or_err(resp, "list").await
}
