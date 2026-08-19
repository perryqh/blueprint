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

/// Models the unpublish-during-publish race: a publish landing while
/// `shutdown-if-empty` checks the count must either commit first (count > 0 →
/// CONFLICT, daemon stays up) or commit after (204 + notify, but the in-flight
/// publish is still served).
///
/// Which of the two happens is genuinely non-deterministic, so this asserts the
/// invariant that holds either way — the create is never swallowed — and then
/// *couples* the returned status to the observable outcome. The previous version
/// asserted `status == 204 || status == 409`, which are the only two codes the
/// endpoint can return, so that line was `assert!(true)` and the name's promise
/// of `then_409s` went unchecked.
#[tokio::test]
async fn concurrent_create_is_never_swallowed_by_shutdown() {
    let h = spawn().await;
    let c = http();

    // Arm the notify listener before racing, so we can tell afterwards whether
    // the shutdown path actually fired rather than inferring it from the status.
    let waiter = h.shutdown.clone();
    let notified = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_millis(500), waiter.notified())
            .await
            .is_ok()
    });

    let create = c
        .post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "race" }))
        .send();
    let shutdown = c.post(format!("{}/api/shutdown-if-empty", h.base)).send();
    let (cr, sr) = tokio::join!(create, shutdown);

    let create_status = cr.unwrap().status();
    let shutdown_status = sr.unwrap().status();
    assert_eq!(create_status, 200, "the create must succeed either way");

    // The invariant, regardless of ordering: the plan is persisted.
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
        "the racing create must be persisted regardless of shutdown timing"
    );

    // And the status must agree with what actually happened to the notify.
    let fired = notified.await.unwrap();
    match shutdown_status.as_u16() {
        409 => assert!(
            !fired,
            "409 means the create won the race, so shutdown must NOT have fired the notify"
        ),
        204 => assert!(
            fired,
            "204 means the daemon accepted shutdown, so the notify must have fired"
        ),
        other => panic!("shutdown-if-empty returned unexpected {other}"),
    }
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

/// Renamed from `batch_reply_to_earlier_draft_in_same_batch`, which described
/// intra-batch parent references and then asserted something else entirely — the
/// body itself admitted it couldn't express the documented case. The real
/// intra-batch behaviour now has its own test below, at the layer where it *is*
/// expressible.
#[tokio::test]
async fn batch_with_unknown_parent_rejects_whole_batch() {
    let h = spawn().await;
    let c = http();
    c.post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>x y</p>", "slug": "b4" }))
        .send()
        .await
        .unwrap();

    // A parent_id that's neither already in the database nor among this batch's
    // own ids must reject the batch atomically.
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

/// The behaviour the old `batch_reply_to_earlier_draft_in_same_batch` claimed to
/// cover: a draft may name a *sibling in the same batch* as its parent, which is
/// what validate-then-insert ordering in `add_comments_batch` buys.
///
/// It isn't expressible over HTTP because ids are server-assigned there, so this
/// drives the store directly — the layer that actually owns the guarantee.
#[tokio::test]
async fn batch_draft_may_reply_to_a_sibling_in_the_same_batch() {
    use blueprint::store::{AuthorRole, CommentDraft};

    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path().join("blueprints.db")).unwrap();
    store
        .insert_blueprint("sib", b"<p>x y</p>", "tok", None)
        .unwrap();

    let selector = |exact: &str| {
        serde_json::from_value(json!({ "type": "TextQuoteSelector", "exact": exact })).unwrap()
    };
    let draft = |id: &str, parent: Option<&str>, exact: &str| CommentDraft {
        id: id.to_string(),
        author: "a".into(),
        body: "b".into(),
        selector: selector(exact),
        parent_id: parent.map(str::to_string),
        author_user_id: None,
        author_avatar_url: None,
        role: AuthorRole::Guest,
        is_agent: false,
    };

    // The child names the parent that is being inserted in this same call.
    let inserted = store
        .add_comments_batch(
            "sib",
            &[
                draft("c_parent", None, "x"),
                draft("c_child", Some("c_parent"), "y"),
            ],
        )
        .expect("a sibling parent in the same batch must be accepted");

    assert_eq!(inserted.len(), 2);
    let child = inserted
        .iter()
        .find(|c| c.id == "c_child")
        .expect("child was inserted");
    assert_eq!(child.parent_id.as_deref(), Some("c_parent"));

    // The other direction must be a clean rejection, not a crash. Inserts run in
    // slice order and SQLite checks the foreign key immediately, so a draft
    // naming a parent that comes *later* cannot be satisfied. Writing this test
    // is what surfaced the bug: validation used to build its seen-set from the
    // whole batch, so a forward reference passed the check and then died on a raw
    // FOREIGN KEY violation — a 500 where the caller deserved a 400.
    let err = store
        .add_comments_batch(
            "sib",
            &[
                draft("c_kid", Some("c_dad"), "y"),
                draft("c_dad", None, "x"),
            ],
        )
        .expect_err("a forward reference must be rejected, not attempted");
    assert!(
        matches!(err, blueprint::error::AppError::BadRequest(ref m) if m.contains("c_dad")),
        "expected a 400-shaped rejection naming the missing parent, got: {err:?}"
    );

    // And the rejection was atomic — neither row landed.
    let listed = store.list_comments("sib", None).unwrap();
    assert!(
        listed.iter().all(|c| c.id != "c_kid" && c.id != "c_dad"),
        "a rejected batch must not persist any of its rows"
    );
}

/// The watermark a polling client feeds back as `since` must never run ahead of
/// the newest row it was actually given.
///
/// `build_comments_response` used to stamp `server_ts` with `now_ms()` *after* the
/// caller had already read from SQLite. A comment inserted in that window got a
/// `created_at` below the watermark while never appearing in the response — and
/// because `list_comments` filters on a strict `created_at > since`, that comment
/// was invisible to every subsequent poll, permanently.
///
/// The window is a few microseconds wide in-process, so rather than trying to
/// lose the race this asserts the invariant that makes the race unlosable:
/// `server_ts` equals the newest returned `created_at`. If it ever exceeds it,
/// there is a range of timestamps that can be written but never read.
#[tokio::test]
async fn comment_watermark_never_runs_ahead_of_the_rows_returned() {
    let h = spawn().await;
    let c = http();
    c.post(format!("{}/api/blueprints", h.base))
        .json(&json!({ "html": "<p>alpha beta gamma</p>", "slug": "wm" }))
        .send()
        .await
        .unwrap();

    for word in ["alpha", "beta", "gamma"] {
        let r = c
            .post(format!("{}/api/blueprints/wm/comments", h.base))
            .json(&json!({
                "author": "a", "body": word,
                "selector": { "type": "TextQuoteSelector", "exact": word }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "seed comment {word}");
    }

    let listing: Value = c
        .get(format!("{}/api/blueprints/wm/comments", h.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let comments = listing["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 3);
    let newest = comments
        .iter()
        .map(|c| c["created_at"].as_i64().unwrap())
        .max()
        .unwrap();
    let server_ts = listing["server_ts"].as_i64().unwrap();
    assert_eq!(
        server_ts, newest,
        "server_ts must be the newest returned created_at, not a later clock read \
         (a gap here is a window where comments are written but never returned)"
    );

    // Round-trip the contract the client actually relies on: polling with the
    // returned watermark yields nothing, and every row is still reachable by
    // polling from just before it.
    let followup: Value = c
        .get(format!(
            "{}/api/blueprints/wm/comments?since={}",
            h.base, server_ts
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        followup["comments"].as_array().unwrap().len(),
        0,
        "polling at the watermark must not re-deliver rows"
    );

    let replay: Value = c
        .get(format!(
            "{}/api/blueprints/wm/comments?since={}",
            h.base,
            newest - 1
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !replay["comments"].as_array().unwrap().is_empty(),
        "the newest row must still be reachable from just before its timestamp"
    );
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
