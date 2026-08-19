//! MCP stdio server (Step 3): protocol-level conformance over the real binary.
//!
//! Drives `blueprint mcp` as a subprocess and speaks JSON-RPC over its
//! stdin/stdout. Two flavours:
//!
//!   * `initialize` / `tools/list` / the JSON-RPC error paths need no daemon, so
//!     they run against an isolated `$HOME` and bind nothing.
//!   * `tools/call` needs a daemon. Rather than let the subprocess spawn a real
//!     one, the harness stands up an in-process daemon and plants a lock file in
//!     the isolated `$HOME` pointing at it — see `with_daemon`. The subprocess
//!     discovers it exactly the way a user's CLI would, and the test still owns
//!     the database and the port.

mod common;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_blueprint")
}

/// Drive `blueprint mcp` with `requests` on stdin and return one parsed response
/// per line of stdout.
///
/// `home` is the isolated `$HOME` the subprocess sees, which is the whole
/// isolation story: `~/.blueprint` is where the daemon lock, the database, and
/// the auth config all live, so a per-test HOME means a per-test world. EOF on
/// stdin is what ends the server's read loop, so the writes are followed by a
/// drop rather than a kill.
fn run_mcp(home: &std::path::Path, requests: &[String]) -> Vec<Value> {
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn blueprint mcp");

    let mut stdin = child.stdin.take().unwrap();
    for r in requests {
        writeln!(stdin, "{r}").unwrap();
    }
    drop(stdin);

    let out = child.wait_with_output().expect("wait mcp");
    assert!(out.status.success(), "mcp exited non-zero");
    BufReader::new(&out.stdout[..])
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).expect("each response line is JSON"))
        .collect()
}

/// An isolated `$HOME` plus an in-process daemon the MCP subprocess will find.
struct McpWorld {
    home: tempfile::TempDir,
    server: common::TestServer,
}

/// Stand up an in-process daemon and plant a `~/.blueprint/daemon.lock` naming
/// it, so `blueprint mcp`'s `discover_running()` resolves to *our* server.
///
/// The lock carries this test process's own pid because discovery liveness-checks
/// it with `kill(pid, 0)` — and the claim is honest, since the daemon really is
/// running in this process. The alternative (letting the subprocess spawn a
/// daemon) would write to the developer's real `~/.blueprint` and leave a stray
/// process behind on failure.
async fn with_daemon() -> McpWorld {
    let server = common::spawn().await;
    let port: u16 = server
        .base
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .expect("base URL ends in a port");

    let home = tempfile::tempdir().expect("isolated HOME");
    let dir = home.path().join(".blueprint");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("daemon.lock"),
        json!({ "pid": std::process::id(), "port": port, "started_at": 0 }).to_string(),
    )
    .unwrap();

    McpWorld { home, server }
}

fn req(id: u32, method: &str, params: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string()
}

fn call(id: u32, name: &str, arguments: Value) -> String {
    req(
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

/// The text payload of a successful `tools/call`, asserting it wasn't an
/// `isError` envelope on the way through.
fn call_text(resp: &Value) -> String {
    assert!(
        resp.get("error").is_none(),
        "expected a result, got a JSON-RPC error: {resp}"
    );
    let content = &resp["result"]["content"][0];
    assert_eq!(content["type"], "text");
    assert_ne!(
        resp["result"]["isError"], true,
        "tool reported failure: {content}"
    );
    content["text"].as_str().unwrap().to_string()
}

#[test]
fn initialize_and_tools_list_over_stdio() {
    let home = tempfile::tempdir().expect("isolated HOME");
    let responses = run_mcp(
        home.path(),
        &[
            req(1, "initialize", json!({})),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string(),
        ],
    );

    // Response 1: initialize. The notification in between draws no reply, which
    // is why `tools/list` is response *2* and not *3*.
    let init = &responses[0];
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["serverInfo"]["name"], "blueprint");
    assert!(init["result"]["protocolVersion"].is_string());

    // Response 2: tools/list advertises the five tools by name.
    let tools = &responses[1];
    assert_eq!(tools["id"], 2);
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for expected in [
        "publish_blueprint",
        "update_blueprint",
        "wait_for_comments",
        "reply_to_comment",
        "list_blueprints",
    ] {
        assert!(
            names.contains(&expected),
            "missing tool {expected}: {names:?}"
        );
    }
    assert_eq!(responses.len(), 2, "a notification must draw no response");
}

/// The whole tool surface driven end-to-end against a real daemon: publish,
/// list, update, read comments back, reply to one.
///
/// One test rather than five because the tools are a chain — a reply needs a
/// comment, a comment needs a blueprint — and splitting it would mean re-running
/// the same setup through the same subprocess four more times.
#[tokio::test(flavor = "multi_thread")]
async fn tools_call_drives_the_full_publish_comment_reply_loop() {
    let w = with_daemon().await;

    // Seed a comment over HTTP: the MCP surface can reply to comments but not
    // author top-level ones, which is the reviewer's job.
    let http = common::client();
    let published = tokio::task::spawn_blocking({
        let home = w.home.path().to_path_buf();
        move || {
            run_mcp(
                &home,
                &[call(
                    1,
                    "publish_blueprint",
                    json!({ "html": "<p>plan body</p>", "slug": "mcp-loop" }),
                )],
            )
        }
    })
    .await
    .unwrap();
    let out: Value = serde_json::from_str(&call_text(&published[0])).unwrap();
    assert_eq!(out["slug"], "mcp-loop");
    assert!(
        out["url"].as_str().unwrap().ends_with("/b/mcp-loop"),
        "publish returns a reviewer URL: {}",
        out["url"]
    );

    let comment: Value = http
        .post(format!(
            "{}/api/blueprints/mcp-loop/comments",
            w.server.base
        ))
        .json(&json!({
            "author": "alice", "body": "tighten this",
            "selector": { "type": "TextQuoteSelector", "exact": "plan body" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let comment_id = comment["id"].as_str().unwrap().to_string();

    let responses = tokio::task::spawn_blocking({
        let home = w.home.path().to_path_buf();
        let cid = comment_id.clone();
        move || {
            run_mcp(
                &home,
                &[
                    call(2, "list_blueprints", json!({})),
                    call(
                        3,
                        "update_blueprint",
                        json!({ "slug": "mcp-loop", "html": "<p>revised body</p>" }),
                    ),
                    // since=0 reads the backlog, so the seeded comment is there
                    // already and this returns on the fast path.
                    call(
                        4,
                        "wait_for_comments",
                        json!({ "slug": "mcp-loop", "since": 0 }),
                    ),
                    call(
                        5,
                        "reply_to_comment",
                        json!({ "slug": "mcp-loop", "comment_id": cid, "body": "done" }),
                    ),
                ],
            )
        }
    })
    .await
    .unwrap();

    let listed = call_text(&responses[0]);
    assert!(
        listed.contains("mcp-loop"),
        "list_blueprints should mention the slug: {listed}"
    );

    call_text(&responses[1]);
    let raw = http
        .get(format!("{}/api/blueprints/mcp-loop/raw", w.server.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(raw, "<p>revised body</p>", "update_blueprint must persist");

    let waited: Value = serde_json::from_str(&call_text(&responses[2])).unwrap();
    let bodies: Vec<&str> = waited["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, vec!["tighten this"]);

    call_text(&responses[3]);
    // The reply is only real if it's threaded under the parent and attributed to
    // the agent — the point of routing through the CLI bearer token.
    let listing: Value = http
        .get(format!(
            "{}/api/blueprints/mcp-loop/comments",
            w.server.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reply = listing["comments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["parent_id"] == comment_id.as_str())
        .expect("reply_to_comment must thread under the parent");
    assert_eq!(reply["body"], "done");
    assert_eq!(reply["author"], "Claude Code", "default author");
}

/// A tool that reached the daemon and failed there gets `isError: true` inside a
/// *success* envelope, not a JSON-RPC error — the distinction the model needs to
/// decide whether a retry could help. An unknown slug is the cheapest way to
/// produce a genuine operational failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_that_runs_and_fails_reports_is_error_not_a_protocol_error() {
    let w = with_daemon().await;
    let responses = tokio::task::spawn_blocking({
        let home = w.home.path().to_path_buf();
        move || {
            run_mcp(
                &home,
                &[call(
                    1,
                    "update_blueprint",
                    json!({ "slug": "no-such-slug", "html": "<p>x</p>" }),
                )],
            )
        }
    })
    .await
    .unwrap();

    let r = &responses[0];
    assert!(
        r.get("error").is_none(),
        "an operational failure must not surface as a JSON-RPC error: {r}"
    );
    assert_eq!(r["result"]["isError"], true);
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("error: "),
        "the failure text should read as an error for the model"
    );
}

/// The JSON-RPC error surface. All four of these are decided before any daemon
/// is consulted, so this test needs no lock file and no server.
#[test]
fn malformed_and_unknown_requests_get_the_right_jsonrpc_error_codes() {
    let home = tempfile::tempdir().expect("isolated HOME");
    let responses = run_mcp(
        home.path(),
        &[
            // Unparseable JSON → -32700, with a null id since there's no
            // request to attribute it to.
            "{not json at all".to_string(),
            // Parses, has an id, but names no method → not a request at all.
            json!({ "jsonrpc": "2.0", "id": 2 }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "no/such/method" }).to_string(),
            call(4, "no_such_tool", json!({})),
            // Known tool, required argument missing → invalid params, not a
            // failed call: it was never valid to send.
            call(5, "reply_to_comment", json!({ "slug": "s" })),
        ],
    );
    assert_eq!(responses.len(), 5);

    assert_eq!(responses[0]["error"]["code"], -32700);
    assert!(
        responses[0]["id"].is_null(),
        "an unparseable line has no id to echo"
    );

    assert_eq!(responses[1]["id"], 2);
    assert_eq!(responses[1]["error"]["code"], -32600);

    assert_eq!(responses[2]["id"], 3);
    assert_eq!(responses[2]["error"]["code"], -32601);

    assert_eq!(responses[3]["id"], 4);
    assert_eq!(responses[3]["error"]["code"], -32602);

    assert_eq!(responses[4]["id"], 5);
    assert_eq!(responses[4]["error"]["code"], -32602);
    assert!(
        responses[4]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("comment_id"),
        "the message should name the missing argument: {}",
        responses[4]["error"]["message"]
    );
}

/// `"id": null` marks a notification, and JSON-RPC 2.0 forbids replying to one.
/// Worth its own assertion because `let Some(id) = msg.get("id")` happily accepts
/// `Some(Value::Null)` — the bug this guards against is a one-character fix away.
#[test]
fn notifications_draw_no_response() {
    let home = tempfile::tempdir().expect("isolated HOME");
    let responses = run_mcp(
        home.path(),
        &[
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": null, "method": "tools/list" }).to_string(),
            // A real request last, to prove the loop survived both and is still
            // answering rather than having silently wedged.
            json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" }).to_string(),
        ],
    );
    assert_eq!(
        responses.len(),
        1,
        "only the ping should be answered, got {responses:?}"
    );
    assert_eq!(responses[0]["id"], 9);
}
