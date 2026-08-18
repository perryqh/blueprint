//! MCP stdio server (Step 3): protocol-level handshake over the real binary.
//!
//! Drives `blueprint mcp` as a subprocess and speaks JSON-RPC over its
//! stdin/stdout. `initialize` and `tools/list` need no daemon, so this stays
//! hermetic (isolated $HOME, no port binding). The `tools/call` paths are
//! covered by the HTTP-level tests plus a manual smoke run, since they require
//! a live daemon.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_blueprint")
}

#[test]
fn initialize_and_tools_list_over_stdio() {
    let home = tempfile::tempdir().expect("isolated HOME");
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn blueprint mcp");

    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
    )
    .unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#).unwrap();
    // EOF ends the server's read loop.
    drop(stdin);

    let out = child.wait_with_output().expect("wait mcp");
    assert!(out.status.success(), "mcp exited non-zero");
    let reader = BufReader::new(&out.stdout[..]);
    let responses: Vec<Value> = reader
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();

    // Response 1: initialize.
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
}
