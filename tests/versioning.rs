//! Version history + version-pinned comments (Step 2).
//!
//! Proves the data-loss fix: `--update` archives the prior HTML instead of
//! overwriting it, comments keep the version they were authored against, and
//! `/raw?version=N` serves the exact snapshot a comment anchored to.

use blueprint::server::{AppState, router};
use blueprint::store::Store;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;

struct TestServer {
    base: String,
    _tmp: TempDir,
}

async fn spawn() -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(&tmp.path().join("blueprints.db")).expect("store"));
    let app = router(AppState::with_auth(store, None));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TestServer { base, _tmp: tmp }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

fn selector(exact: &str) -> Value {
    json!({ "type": "TextQuoteSelector", "exact": exact, "prefix": "", "suffix": "" })
}

#[tokio::test]
async fn update_archives_prior_html_and_pins_comment_versions() {
    let s = spawn().await;
    let http = client();

    // Publish v1.
    let r: Value = http
        .post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>ORIGINAL TEXT</p>", "slug": "vtest" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["slug"], "vtest");

    // A comment authored against v1 is stamped blueprint_version = 1.
    let c1: Value = http
        .post(format!("{}/api/blueprints/vtest/comments", s.base))
        .json(&json!({ "author": "rev", "body": "on v1", "selector": selector("ORIGINAL") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c1["blueprint_version"], 1, "comment stamped with v1");
    let c1_id = c1["id"].as_str().unwrap().to_string();

    // The comments list reports the current version as 1.
    let list: Value = http
        .get(format!("{}/api/blueprints/vtest/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["blueprint_version"], 1);

    // Update to v2 — this must ARCHIVE v1, not overwrite it.
    let up = http
        .put(format!("{}/api/blueprints/vtest", s.base))
        .json(&json!({ "html": "<p>REVISED TEXT</p>" }))
        .send()
        .await
        .unwrap();
    assert_eq!(up.status(), 204);

    // Current version is now 2.
    let list: Value = http
        .get(format!("{}/api/blueprints/vtest/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["blueprint_version"], 2);

    // The v1 comment STILL reports blueprint_version = 1 (not silently drifted
    // to the current version) — this is the crux of the fix.
    let comments = list["comments"].as_array().unwrap();
    let found = comments.iter().find(|c| c["id"] == c1_id).unwrap();
    assert_eq!(found["blueprint_version"], 1);

    // Live /raw is the revised text; /raw?version=1 is the archived original;
    // /raw?version=2 is the live text.
    let live = http
        .get(format!("{}/api/blueprints/vtest/raw", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(live.contains("REVISED TEXT"), "live raw = v2");

    let v1 = http
        .get(format!("{}/api/blueprints/vtest/raw?version=1", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        v1.contains("ORIGINAL TEXT"),
        "archived v1 recovered exactly: {v1}"
    );

    let v2 = http
        .get(format!("{}/api/blueprints/vtest/raw?version=2", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(v2.contains("REVISED TEXT"), "explicit v2 = live");

    // The versions endpoint lists both versions and marks the current one.
    let versions: Value = http
        .get(format!("{}/api/blueprints/vtest/versions", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["current"], 2);
    assert_eq!(versions["versions"], json!([1, 2]));

    // A comment authored after the update is stamped v2.
    let c2: Value = http
        .post(format!("{}/api/blueprints/vtest/comments", s.base))
        .json(&json!({ "author": "rev", "body": "on v2", "selector": selector("REVISED") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c2["blueprint_version"], 2);
}
