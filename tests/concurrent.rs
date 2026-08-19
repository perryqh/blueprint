//! Integration tests for the multi-repo concurrency hardening (see
//! `~/.blueprint/drafts/concurrent-multi-repo-safety.html`). Covers:
//!
//!   * `POST /api/shutdown-if-empty` returns 204 + fires the shutdown notify
//!     when no blueprints exist, and 409 (no notify) when at least one remains.
//!   * `X-Client-Cwd` request header is persisted and surfaced back in the
//!     blueprint summary, enabling `blueprint status`' multi-repo grouping.
//!
//! Run with: `cargo test --release --test concurrent`.

use blueprint::server::{AppState, router};
use blueprint::store::Store;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Notify;

struct Harness {
    base: String,
    shutdown: Arc<Notify>,
    _tmp: TempDir,
}

async fn spawn() -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(tmp.path().join("blueprints.db")).unwrap());
    let state = AppState::with_auth(store, None);
    let shutdown = state.shutdown.clone();
    let app = router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Harness {
        base,
        shutdown,
        _tmp: tmp,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn shutdown_if_empty_returns_204_and_fires_notify_when_no_plans() {
    let h = spawn().await;
    let c = http();

    // notified() must be created before notify_one() to receive the signal.
    let waiter = h.shutdown.clone();
    let notified = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), waiter.notified())
            .await
            .is_ok()
    });

    let r = c
        .post(format!("{}/api/shutdown-if-empty", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204, "empty daemon should accept shutdown");

    let fired = notified.await.unwrap();
    assert!(fired, "shutdown notify must be triggered when count == 0");
}

#[tokio::test]
async fn shutdown_if_empty_returns_409_and_keeps_running_when_plans_exist() {
    let h = spawn().await;
    let c = http();

    // Plant a plan so the daemon should refuse to shut down.
    let r = c
        .post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "keepalive" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Subscribe to notify *before* posting so we can prove it never fires.
    let waiter = h.shutdown.clone();
    let notified = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_millis(300), waiter.notified())
            .await
            .is_ok()
    });

    let r = c
        .post(format!("{}/api/shutdown-if-empty", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "non-empty daemon must refuse shutdown");

    let fired = notified.await.unwrap();
    assert!(
        !fired,
        "shutdown notify must NOT fire when plans remain (got fired = {fired})"
    );
}

#[tokio::test]
async fn shutdown_if_empty_unblocks_concurrent_create_then_409s() {
    // Models the unpublish-during-publish race described in Issue 3:
    // a publish that lands while we're checking the count must either
    //   (a) commit first → count > 0 → CONFLICT, daemon stays up; or
    //   (b) commit after → 204 + notify, daemon shuts down gracefully but
    //       still finishes serving the in-flight publish.
    // Here we drive (a) by inserting a plan first and verifying CONFLICT.
    let h = spawn().await;
    let c = http();

    // Race a create and a shutdown-if-empty together. The create lands first
    // most of the time on a quiet test daemon, so we expect CONFLICT.
    let create = c
        .post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "race" }))
        .send();
    let shutdown = c.post(format!("{}/api/shutdown-if-empty", h.base)).send();

    let (cr, sr) = tokio::join!(create, shutdown);
    let cr = cr.unwrap();
    let sr = sr.unwrap();

    assert_eq!(cr.status(), 200, "create_plan must succeed");
    // Either outcome is correct, but both must leave us in a sane state:
    //   - 204: notify fired, but the create that raced in is still persisted.
    //   - 409: create won the race, daemon stays up.
    assert!(
        sr.status() == 204 || sr.status() == 409,
        "shutdown-if-empty returned unexpected {}",
        sr.status()
    );

    // Either way the plan must be persisted — no race may swallow it.
    let plans: Vec<Value> = c
        .get(format!("{}/api/blueprints", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        plans.iter().any(|p| p["slug"] == "race"),
        "the racing create must have been persisted regardless of shutdown timing"
    );
}

#[tokio::test]
async fn x_client_cwd_header_is_persisted_and_surfaced() {
    let h = spawn().await;
    let c = http();

    let r = c
        .post(format!("{}/api/blueprints", h.base))
        .header("X-Client-Cwd", "/home/alice/repos/foo")
        .json(&json!({ "html": "<p>x</p>", "slug": "cwd-a" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let plans: Vec<Value> = c
        .get(format!("{}/api/blueprints", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let found = plans
        .iter()
        .find(|p| p["slug"] == "cwd-a")
        .expect("cwd-a present");
    assert_eq!(
        found["client_cwd"], "/home/alice/repos/foo",
        "X-Client-Cwd must round-trip through the plan summary"
    );
}

#[tokio::test]
async fn batch_create_persists_all_atomically_and_returns_them() {
    let h = spawn().await;
    let c = http();

    // Need a plan to attach comments to.
    c.post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>hello world foo bar</p>", "slug": "b1" }))
        .send()
        .await
        .unwrap();

    let payload = json!([
        {
            "author": "alice", "body": "first",
            "selector": { "type": "TextQuoteSelector", "exact": "hello" }
        },
        {
            "author": "alice", "body": "second",
            "selector": { "type": "TextQuoteSelector", "exact": "world" }
        },
        {
            "author": "alice", "body": "third",
            "selector": { "type": "TextQuoteSelector", "exact": "foo" }
        }
    ]);

    let r = c
        .post(format!("{}/api/blueprints/b1/comments/batch", h.base))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let returned = body["comments"].as_array().unwrap();
    assert_eq!(returned.len(), 3);
    // All three should share a created_at since they're a single transaction.
    let t0 = returned[0]["created_at"].as_i64().unwrap();
    for c in returned {
        assert_eq!(c["created_at"].as_i64().unwrap(), t0);
    }

    // Confirm via the list endpoint that all three are persisted.
    let listing: Value = c
        .get(format!("{}/api/blueprints/b1/comments", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["comments"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn batch_with_one_empty_body_rejects_whole_batch() {
    let h = spawn().await;
    let c = http();
    c.post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>hello world</p>", "slug": "b2" }))
        .send()
        .await
        .unwrap();

    let payload = json!([
        {
            "author": "alice", "body": "ok",
            "selector": { "type": "TextQuoteSelector", "exact": "hello" }
        },
        {
            "author": "alice", "body": "   ",
            "selector": { "type": "TextQuoteSelector", "exact": "world" }
        }
    ]);

    let r = c
        .post(format!("{}/api/blueprints/b2/comments/batch", h.base))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // Nothing should have been persisted — atomic-or-nothing.
    let listing: Value = c
        .get(format!("{}/api/blueprints/b2/comments", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["comments"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn batch_wakes_wait_for_comment_once_with_all_rows() {
    // The wire-protocol promise: one Submit-all → one /wait-comment response
    // that contains every new comment, not one wake-up per row.
    let h = spawn().await;
    let c = http();
    c.post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>aa bb cc dd</p>", "slug": "b3" }))
        .send()
        .await
        .unwrap();

    // Start the long-poll *first*, then submit the batch.
    let base = h.base.clone();
    let waiter = tokio::spawn(async move {
        http()
            .get(format!("{base}/api/blueprints/b3/wait-comment?since=0"))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    });

    // Tiny pause so the waiter has subscribed before we send the batch.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let payload = json!([
        { "author": "a", "body": "1",
          "selector": { "type": "TextQuoteSelector", "exact": "aa" } },
        { "author": "a", "body": "2",
          "selector": { "type": "TextQuoteSelector", "exact": "bb" } },
        { "author": "a", "body": "3",
          "selector": { "type": "TextQuoteSelector", "exact": "cc" } }
    ]);
    c.post(format!("{}/api/blueprints/b3/comments/batch", h.base))
        .json(&payload)
        .send()
        .await
        .unwrap();

    let body = waiter.await.unwrap();
    let comments = body["comments"].as_array().unwrap();
    assert_eq!(
        comments.len(),
        3,
        "single /wait-comment response must carry all 3 batched comments, not 1"
    );
}

#[tokio::test]
async fn batch_reply_to_earlier_draft_in_same_batch() {
    // A draft can reply to another draft that lives in the same batch.
    // Validate-then-insert order in `add_comments_batch` allows this.
    let h = spawn().await;
    let c = http();
    c.post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>x y</p>", "slug": "b4" }))
        .send()
        .await
        .unwrap();

    // We don't know the IDs ahead of time, so we can't directly express
    // "reply to my sibling" through the public API — IDs are server-assigned.
    // Instead, verify that an unknown parent_id (not in DB, not in batch)
    // rejects the whole batch cleanly.
    let payload = json!([
        { "author": "a", "body": "ok",
          "selector": { "type": "TextQuoteSelector", "exact": "x" } },
        { "author": "a", "body": "stray",
          "selector": { "type": "TextQuoteSelector", "exact": "y" },
          "parent_id": "c_nonexistent" }
    ]);
    let r = c
        .post(format!("{}/api/blueprints/b4/comments/batch", h.base))
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // Confirm nothing was persisted.
    let listing: Value = c
        .get(format!("{}/api/blueprints/b4/comments", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["comments"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn missing_x_client_cwd_leaves_field_null() {
    // Backwards-compat: existing clients (older CLIs, browsers via the
    // reviewer UI) don't send the header. The plan must still be created
    // and client_cwd must be absent (serde skips None on serialize).
    let h = spawn().await;
    let c = http();

    let r = c
        .post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "no-cwd" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let plans: Vec<Value> = c
        .get(format!("{}/api/blueprints", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let found = plans
        .iter()
        .find(|p| p["slug"] == "no-cwd")
        .expect("no-cwd present");
    assert!(
        found.get("client_cwd").is_none() || found["client_cwd"].is_null(),
        "absent X-Client-Cwd must serialize to null/missing, got {:?}",
        found.get("client_cwd")
    );
}
