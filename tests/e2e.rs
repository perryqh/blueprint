//! End-to-end smoke test that exercises the §9 verification checklist
//! against an in-process daemon. Mirrors what the CLI / browser would do.

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

/// Bind a fresh listener on an OS-assigned port, returning the listener and
/// its `http://addr/` base URL.
async fn bind_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    (listener, format!("http://{}", addr))
}

/// Spawn a daemon on the given listener with a fresh SQLite store. Shared by
/// `spawn` (legacy/no-auth) and `spawn_with_auth` (GitHub OAuth enabled).
async fn spawn_daemon_on(
    listener: TcpListener,
    auth: Option<Arc<blueprint::auth::AuthConfig>>,
) -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(&tmp.path().join("blueprints.db")).expect("open store"));
    let state = AppState::with_auth(store, auth);
    let app = router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tmp
}

async fn spawn() -> TestServer {
    let (listener, base) = bind_listener().await;
    let tmp = spawn_daemon_on(listener, None).await;
    TestServer { base, _tmp: tmp }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

#[tokio::test]
async fn publish_comment_reply_finish_fetch_drift_unpublish() {
    let s = spawn().await;
    let http = client();

    // 1. Health.
    let r = http
        .get(format!("{}/api/health", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 2. Publish a plan.
    let html = r#"<p>Hello <em>world</em>, this is a <strong>plan</strong>.</p>"#;
    let r: Value = http
        .post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": html, "slug": "test-plan" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["slug"], "test-plan");
    assert_eq!(r["url"], "/b/test-plan");

    // 3. Raw HTML round-trips.
    let raw = http
        .get(format!("{}/api/blueprints/test-plan/raw", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(raw, html);

    // 4. Reviewer page returns 200 (the embedded reviewer.html shell).
    let r = http
        .get(format!("{}/b/test-plan", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(
        body.contains("blueprint"),
        "reviewer.html should mention blueprint"
    );
    assert!(
        body.contains("iframe"),
        "reviewer.html should have an iframe"
    );

    // 5. Initial blueprint_version is 1.
    let listing: Value = http
        .get(format!("{}/api/blueprints/test-plan/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["blueprint_version"], 1);
    assert_eq!(listing["comments"].as_array().unwrap().len(), 0);

    // 6. Add an anchored comment.
    let comment: Value = http
        .post(format!("{}/api/blueprints/test-plan/comments", s.base))
        .json(&json!({
            "author": "alice",
            "body": "Should this be lowercase?",
            "selector": {
                "type": "TextQuoteSelector",
                "exact": "world",
                "prefix": "Hello ",
                "suffix": ", this is"
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let comment_id = comment["id"].as_str().unwrap().to_string();
    assert!(comment_id.starts_with("c_"));
    assert_eq!(comment["author"], "alice");
    assert_eq!(comment["resolved"], false);

    // 7. Add a reply (inherits the parent's selector).
    let reply: Value = http
        .post(format!(
            "{}/api/blueprints/test-plan/comments/{}/replies",
            s.base, comment_id
        ))
        .json(&json!({ "author": "claude", "body": "Yes, by convention." }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reply["parent_id"], comment_id);
    assert_eq!(reply["selector"]["exact"], "world");

    // 8. Listing has both, sorted by created_at.
    let listing: Value = http
        .get(format!("{}/api/blueprints/test-plan/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cs = listing["comments"].as_array().unwrap();
    assert_eq!(cs.len(), 2);
    assert_eq!(cs[0]["id"], comment_id);
    assert_eq!(cs[1]["parent_id"], comment_id);

    // 9. `since=` polling returns nothing if we ask after the last ts.
    let last_ts = listing["server_ts"].as_i64().unwrap();
    let later: Value = http
        .get(format!(
            "{}/api/blueprints/test-plan/comments?since={}",
            s.base,
            last_ts + 1000
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(later["comments"].as_array().unwrap().len(), 0);

    // 10. `wait` unblocks on `finish`.
    let wait_base = s.base.clone();
    let waiter = tokio::spawn(async move {
        let r = reqwest::Client::new()
            .get(format!("{}/api/blueprints/test-plan/wait", wait_base))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .unwrap();
        r.status()
    });
    // Give the waiter a moment to register.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = http
        .post(format!("{}/api/blueprints/test-plan/finish", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let waiter_status = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("wait unblocked in time")
        .unwrap();
    assert_eq!(waiter_status, 204);

    // 11. Anchor survives revision: keep "world", change surrounding text.
    let html_v2 = r#"<p>Hello <em>world</em>, this is the SECOND <strong>iteration</strong>.</p>"#;
    let r = http
        .put(format!("{}/api/blueprints/test-plan", s.base))
        .json(&json!({ "html": html_v2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let listing: Value = http
        .get(format!("{}/api/blueprints/test-plan/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["blueprint_version"], 2, "version bumps on PUT");
    assert_eq!(
        listing["comments"].as_array().unwrap().len(),
        2,
        "comments persist across update"
    );
    // Raw HTML is now the new version
    let raw = http
        .get(format!("{}/api/blueprints/test-plan/raw", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(raw.contains("SECOND"));

    // 12. Drift: remove "world", verify comment still in store. (Browser-side
    //     renders this with the yellow "drifted" badge.)
    let html_v3 = r#"<p>Hello there, no anchor here.</p>"#;
    let r = http
        .put(format!("{}/api/blueprints/test-plan", s.base))
        .json(&json!({ "html": html_v3 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let listing: Value = http
        .get(format!("{}/api/blueprints/test-plan/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["blueprint_version"], 3);
    assert_eq!(listing["comments"].as_array().unwrap().len(), 2);
    let raw = http
        .get(format!("{}/api/blueprints/test-plan/raw", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!raw.contains("world"));

    // 13. Unpublish: 404 afterwards.
    let r = http
        .delete(format!("{}/api/blueprints/test-plan", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let r = http
        .get(format!("{}/api/blueprints/test-plan/comments", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let r = http
        .get(format!("{}/api/blueprints/test-plan/raw", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn create_with_random_slug_when_none_provided() {
    let s = spawn().await;
    let http = client();
    let r: Value = http
        .post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slug = r["slug"].as_str().unwrap();
    // adjective-month-animal pattern: three hyphen-separated words
    assert_eq!(
        slug.split('-').count(),
        3,
        "slug should be three words, got {slug:?}"
    );
}

#[tokio::test]
async fn rejects_empty_html() {
    let s = spawn().await;
    let http = client();
    let r = http
        .post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn rejects_empty_comment_body() {
    let s = spawn().await;
    let http = client();
    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "p1" }))
        .send()
        .await
        .unwrap();
    let r = http
        .post(format!("{}/api/blueprints/p1/comments", s.base))
        .json(&json!({
            "author": "x",
            "body": "   ",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn list_plans_includes_comment_counts() {
    let s = spawn().await;
    let http = client();

    // Empty
    let plans: Value = http
        .get(format!("{}/api/blueprints", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(plans.as_array().unwrap().len(), 0);

    // Publish two plans
    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>a</p>", "slug": "alpha" }))
        .send()
        .await
        .unwrap();
    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>b</p>", "slug": "beta" }))
        .send()
        .await
        .unwrap();

    // Add comments to alpha: 2 unresolved
    for body in ["c1", "c2"] {
        http.post(format!("{}/api/blueprints/alpha/comments", s.base))
            .json(&json!({
                "author": "x",
                "body": body,
                "selector": { "type": "TextQuoteSelector", "exact": "a" }
            }))
            .send()
            .await
            .unwrap();
    }

    let plans: Value = http
        .get(format!("{}/api/blueprints", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let arr = plans.as_array().unwrap();
    assert_eq!(arr.len(), 2);

    let alpha = arr.iter().find(|p| p["slug"] == "alpha").unwrap();
    assert_eq!(alpha["comment_count"], 2);
    assert_eq!(alpha["unresolved_count"], 2);
    assert!(alpha["last_activity_at"].as_i64().unwrap() >= alpha["created_at"].as_i64().unwrap());

    let beta = arr.iter().find(|p| p["slug"] == "beta").unwrap();
    assert_eq!(beta["comment_count"], 0);
    assert_eq!(beta["unresolved_count"], 0);
    assert_eq!(
        beta["last_activity_at"], beta["created_at"],
        "no comments → last_activity == created_at"
    );
}

#[tokio::test]
async fn rejects_reply_to_unknown_parent() {
    let s = spawn().await;
    let http = client();
    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "p2" }))
        .send()
        .await
        .unwrap();
    let r = http
        .post(format!(
            "{}/api/blueprints/p2/comments/c_nope42/replies",
            s.base
        ))
        .json(&json!({ "author": "x", "body": "huh" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn processing_sets_and_auto_clears_on_reply() {
    let s = spawn().await;
    let http = client();
    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "proc" }))
        .send()
        .await
        .unwrap();
    // Create a top-level comment.
    let parent: Value = http
        .post(format!("{}/api/blueprints/proc/comments", s.base))
        .json(&json!({
            "author": "alice", "body": "need a hand here",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let parent_id = parent["id"].as_str().unwrap();

    // Mark as processing.
    let r = http
        .post(format!(
            "{}/api/blueprints/proc/comments/{}/processing",
            s.base, parent_id
        ))
        .json(&json!({ "author": "Claude" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // GET /comments should now show the comment with processing_by=Claude.
    let listing: Value = http
        .get(format!("{}/api/blueprints/proc/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let head = listing["comments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == *parent_id)
        .unwrap();
    assert_eq!(head["processing_by"], "Claude");
    assert!(head["processing_started_at"].as_i64().unwrap() > 0);

    // Post a reply — should auto-clear processing on parent.
    http.post(format!(
        "{}/api/blueprints/proc/comments/{}/replies",
        s.base, parent_id
    ))
    .json(&json!({ "author": "Claude", "body": "here's the answer" }))
    .send()
    .await
    .unwrap();

    let listing: Value = http
        .get(format!("{}/api/blueprints/proc/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let head = listing["comments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == *parent_id)
        .unwrap();
    assert!(
        head.get("processing_by").is_none() || head["processing_by"].is_null(),
        "processing_by should be cleared after reply, got {head:?}"
    );
}

#[tokio::test]
async fn processing_rejects_unknown_comment() {
    let s = spawn().await;
    let http = client();
    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "px" }))
        .send()
        .await
        .unwrap();
    let r = http
        .post(format!(
            "{}/api/blueprints/px/comments/c_nope/processing",
            s.base
        ))
        .json(&json!({ "author": "Claude" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    let r = http
        .post(format!(
            "{}/api/blueprints/px/comments/c_nope/processing",
            s.base
        ))
        .json(&json!({ "author": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn resolve_toggles_resolved_bit() {
    let s = spawn().await;
    let http = client();
    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "rez" }))
        .send()
        .await
        .unwrap();
    let c: Value = http
        .post(format!("{}/api/blueprints/rez/comments", s.base))
        .json(&json!({
            "author": "alice", "body": "thing",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = c["id"].as_str().unwrap();

    let r = http
        .post(format!(
            "{}/api/blueprints/rez/comments/{}/resolve",
            s.base, id
        ))
        .json(&json!({ "resolved": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let listing: Value = http
        .get(format!("{}/api/blueprints/rez/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["comments"][0]["resolved"], true);

    // Toggle back off.
    let r = http
        .post(format!(
            "{}/api/blueprints/rez/comments/{}/resolve",
            s.base, id
        ))
        .json(&json!({ "resolved": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let listing: Value = http
        .get(format!("{}/api/blueprints/rez/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listing["comments"][0]["resolved"], false);

    // 404 on unknown comment.
    let r = http
        .post(format!(
            "{}/api/blueprints/rez/comments/c_nope/resolve",
            s.base
        ))
        .json(&json!({ "resolved": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn wait_for_comment_returns_existing_immediately() {
    let s = spawn().await;
    let http = client();

    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "p1" }))
        .send()
        .await
        .unwrap();
    http.post(format!("{}/api/blueprints/p1/comments", s.base))
        .json(&json!({
            "author": "x", "body": "existing",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let r: Value = http
        .get(format!("{}/api/blueprints/p1/wait-comment?since=0", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 200,
        "fast path should be near-instant, took {elapsed:?}"
    );
    let comments = r["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"], "existing");
}

#[tokio::test]
async fn wait_for_comment_unblocks_on_new_comment() {
    let s = spawn().await;
    let http = client();

    http.post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": "<p>x</p>", "slug": "p2" }))
        .send()
        .await
        .unwrap();

    // Use the *current* server_ts as `since` so the fast path returns nothing
    // and we exercise the long-poll path.
    let listing: Value = http
        .get(format!("{}/api/blueprints/p2/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let since = listing["server_ts"].as_i64().unwrap();

    // Post a comment after a short delay, racing the wait.
    let base = s.base.clone();
    let poster = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        reqwest::Client::new()
            .post(format!("{}/api/blueprints/p2/comments", base))
            .json(&json!({
                "author": "x", "body": "live!",
                "selector": { "type": "TextQuoteSelector", "exact": "x" }
            }))
            .send()
            .await
            .unwrap();
    });

    let start = std::time::Instant::now();
    let r: Value = http
        .get(format!(
            "{}/api/blueprints/p2/wait-comment?since={}",
            s.base, since
        ))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_millis() < 1500,
        "slow-path should unblock within ~1s, took {elapsed:?}"
    );
    let comments = r["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["body"], "live!");
    poster.await.unwrap();
}

#[tokio::test]
async fn wait_for_comment_404_on_unknown_slug() {
    let s = spawn().await;
    let http = client();
    let r = http
        .get(format!(
            "{}/api/blueprints/no-such-slug/wait-comment?since=0",
            s.base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

/// Build a minimal `Comment` fixture for tests, varying only the fields that
/// matter for the assertion. Defaults match an unresolved, non-processing,
/// no-user-attribution comment.
fn test_comment(
    id: &str,
    author: &str,
    body: &str,
    selector: blueprint::selector::TextQuoteSelector,
    parent_id: Option<String>,
    created_at: i64,
) -> blueprint::store::Comment {
    blueprint::store::Comment {
        id: id.into(),
        slug: "p".into(),
        author: author.into(),
        body: body.into(),
        selector,
        parent_id,
        resolved: false,
        created_at,
        processing_by: None,
        processing_started_at: None,
        author_user_id: None,
        author_avatar_url: None,
        role: blueprint::store::AuthorRole::Guest,
        is_agent: false,
    }
}

#[tokio::test]
async fn review_file_shape_is_crit_compatible() {
    use blueprint::review_file;
    use blueprint::selector::TextQuoteSelector;

    let selector = TextQuoteSelector {
        ty: "TextQuoteSelector".into(),
        exact: "the quote".into(),
        prefix: Some("before ".into()),
        suffix: Some(" after".into()),
    };
    let parent = test_comment("c_a1b2c3", "alice", "head", selector.clone(), None, 1);
    let reply = test_comment(
        "c_xyz",
        "bob",
        "reply",
        selector,
        Some("c_a1b2c3".into()),
        2,
    );
    let rf = review_file::build("p", vec![parent, reply]);
    let v = serde_json::to_value(&rf).unwrap();
    assert!(v["review_comments"].is_array());
    let file = &v["files"]["p.html"];
    assert!(file.is_object(), "files.<path>.comments expected");
    let comments = &file["comments"];
    assert_eq!(comments.as_array().unwrap().len(), 1);
    let c = &comments[0];
    assert_eq!(c["id"], "c_a1b2c3");
    assert_eq!(c["quote"], "the quote");
    assert_eq!(c["anchor"]["exact"], "the quote");
    assert_eq!(c["replies"].as_array().unwrap().len(), 1);
    assert_eq!(c["replies"][0]["author"], "bob");
}

// ---------------------------------------------------------------------------
// OAuth + auth-gate tests
// ---------------------------------------------------------------------------

use axum::Json as AxumJson;
use axum::extract::Query as AxumQuery;
use axum::response::Redirect;
use axum::routing::{get, post};
use blueprint::auth::AuthConfig;
use std::collections::HashMap;

struct AuthedTest {
    daemon_base: String,
    cli_token: String,
    _tmp: TempDir,
}

/// Spawn a daemon with auth enabled, plus an in-process mock GitHub server.
/// The daemon's AuthConfig.oauth_* URLs point at the mock.
async fn spawn_with_auth() -> AuthedTest {
    // Mock GitHub.
    let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let mock_base = format!("http://{}", mock_addr);
    let mock_app = axum::Router::new()
        .route("/login/oauth/authorize", get(mock_authorize))
        .route("/login/oauth/access_token", post(mock_token))
        .route("/user", get(mock_user));
    tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_app).await;
    });

    // Daemon. AuthConfig.callback_url needs the daemon URL, so bind first.
    let (listener, daemon_base) = bind_listener().await;
    let cli_token = "test-cli-token-deadbeef".to_string();
    let auth = AuthConfig {
        client_id: "test-client".into(),
        client_secret: "test-secret".into(),
        callback_url: format!("{}/auth/github/callback", daemon_base),
        enabled: true,
        authorize_url: format!("{}/login/oauth/authorize", mock_base),
        token_url: format!("{}/login/oauth/access_token", mock_base),
        profile_url: format!("{}/user", mock_base),
        cli_token: cli_token.clone(),
        owner_login: None,
    };
    let tmp = spawn_daemon_on(listener, Some(Arc::new(auth))).await;

    AuthedTest {
        daemon_base,
        cli_token,
        _tmp: tmp,
    }
}

async fn mock_authorize(AxumQuery(p): AxumQuery<HashMap<String, String>>) -> Redirect {
    // GitHub would render a consent UI here; we just bounce straight back to the
    // daemon's callback URL with a synthetic code, preserving the state nonce.
    let redirect_uri = p.get("redirect_uri").cloned().unwrap_or_default();
    let state = p.get("state").cloned().unwrap_or_default();
    Redirect::to(&format!("{}?code=mock-code&state={}", redirect_uri, state))
}

async fn mock_token() -> AxumJson<Value> {
    AxumJson(json!({
        "access_token": "mock-access-token",
        "token_type": "bearer",
        "scope": "read:user"
    }))
}

async fn mock_user() -> AxumJson<Value> {
    AxumJson(json!({
        "id": 4242,
        "login": "mockuser",
        "name": "Mock User",
        "avatar_url": "https://example.com/mockuser.png"
    }))
}

/// A reqwest client with a cookie jar and *no* automatic redirect following — we
/// need to inspect the 302 from /login and the 302 from /callback.
fn browser_client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn oauth_login_redirects_to_authorize_with_state() {
    let s = spawn_with_auth().await;
    let http = browser_client();
    let r = http
        .get(format!("{}/login", s.daemon_base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 303);
    let loc = r.headers().get("location").unwrap().to_str().unwrap();
    assert!(loc.contains("/login/oauth/authorize"), "got {loc}");
    assert!(loc.contains("state="), "no state nonce in {loc}");
    assert!(loc.contains("client_id=test-client"));
}

#[tokio::test]
async fn me_unauthenticated_returns_401() {
    let s = spawn_with_auth().await;
    let r = browser_client()
        .get(format!("{}/api/me", s.daemon_base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn full_oauth_round_trip_authenticates_user() {
    let s = spawn_with_auth().await;
    let http = browser_client();

    // /login → 303 to mock authorize. Follow manually to extract state.
    let r1 = http
        .get(format!("{}/login", s.daemon_base))
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), 303);
    let authorize_url = r1
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Hit mock authorize → 303 back to daemon's callback.
    let r2 = http.get(&authorize_url).send().await.unwrap();
    assert_eq!(r2.status(), 303);
    let callback_url = r2
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(callback_url.starts_with(&format!("{}/auth/github/callback", s.daemon_base)));

    // Hit callback → exchanges code, fetches profile, sets session, 303 to home.
    let r3 = http.get(&callback_url).send().await.unwrap();
    let status = r3.status();
    let body = r3.text().await.unwrap_or_default();
    assert_eq!(
        status, 303,
        "callback should redirect after setting session; got body: {body}"
    );

    // /api/me should now return the mock user.
    let r4: Value = http
        .get(format!("{}/api/me", s.daemon_base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r4["login"], "mockuser");
    assert_eq!(r4["name"], "Mock User");
    assert_eq!(r4["avatar_url"], "https://example.com/mockuser.png");
}

#[tokio::test]
async fn callback_with_bad_state_returns_400() {
    let s = spawn_with_auth().await;
    let http = browser_client();
    // Prime the session via /login so a real state is stored.
    let _ = http
        .get(format!("{}/login", s.daemon_base))
        .send()
        .await
        .unwrap();
    // Now hit callback with a wrong state.
    let r = http
        .get(format!(
            "{}/auth/github/callback?code=mock-code&state=WRONG",
            s.daemon_base
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
}

#[tokio::test]
async fn write_without_auth_succeeds_and_tags_comment_as_guest() {
    // Phase 0: localhost-only, no impersonation defense. Anonymous browser
    // writes go through; the per-comment `role` tag makes provenance visible.
    let s = spawn_with_auth().await;
    let http = client();

    // Publish works without auth.
    let r = http
        .post(format!("{}/api/blueprints", s.daemon_base))
        .json(&json!({ "html": "<p>x</p>", "slug": "anon-pub" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Anonymous comment lands with role: "guest", is_agent: false.
    let c: Value = http
        .post(format!(
            "{}/api/blueprints/anon-pub/comments",
            s.daemon_base
        ))
        .json(&json!({
            "author": "drive-by",
            "body": "hi",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c["role"], "guest");
    assert_eq!(c["is_agent"], false);
    assert_eq!(c["author"], "drive-by");
}

#[tokio::test]
async fn anonymous_empty_author_defaults_to_anonymous_server_side() {
    let s = spawn_with_auth().await;
    let http = client();
    http.post(format!("{}/api/blueprints", s.daemon_base))
        .json(&json!({ "html": "<p>x</p>", "slug": "empty-author" }))
        .send()
        .await
        .unwrap();
    let c: Value = http
        .post(format!(
            "{}/api/blueprints/empty-author/comments",
            s.daemon_base
        ))
        .json(&json!({
            "author": "   ",
            "body": "hi",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c["author"], "anonymous");
}

#[tokio::test]
async fn cli_bearer_comment_lands_with_role_user_and_is_agent_true() {
    // D5: the path Claude itself uses for replies. role = user (orthogonal to
    // owner-edit decisions), is_agent = true so the frontend renders it as
    // an agent reply without depending on the brittle author-string heuristic.
    let s = spawn_with_auth().await;
    let http = client();
    http.post(format!("{}/api/blueprints", s.daemon_base))
        .bearer_auth(&s.cli_token)
        .json(&json!({ "html": "<p>x</p>", "slug": "bearer-test" }))
        .send()
        .await
        .unwrap();
    let c: Value = http
        .post(format!(
            "{}/api/blueprints/bearer-test/comments",
            s.daemon_base
        ))
        .bearer_auth(&s.cli_token)
        .json(&json!({
            "author": "Claude Code",
            "body": "agent reply",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c["role"], "user");
    assert_eq!(c["is_agent"], true);
    assert_eq!(c["author"], "Claude Code");
}

/// Session-authenticated comment from a user who is NOT the configured owner
/// → role: "user", is_agent: false, is_owner on /api/me is false.
#[tokio::test]
async fn session_non_owner_lands_as_user_role() {
    let s = spawn_with_auth().await;
    let http = browser_client();
    oauth_round_trip(&http, &s.daemon_base).await;

    let me: Value = http
        .get(format!("{}/api/me", s.daemon_base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["login"], "mockuser");
    assert_eq!(me["is_owner"], false);

    http.post(format!("{}/api/blueprints", s.daemon_base))
        .json(&json!({ "html": "<p>x</p>", "slug": "non-owner" }))
        .send()
        .await
        .unwrap();
    let c: Value = http
        .post(format!(
            "{}/api/blueprints/non-owner/comments",
            s.daemon_base
        ))
        .json(&json!({
            "author": "ignored",
            "body": "hi",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c["author"], "mockuser");
    assert_eq!(c["role"], "user");
    assert_eq!(c["is_agent"], false);
}

/// Session-authenticated comment from the configured owner login →
/// role: "owner", is_owner: true on /api/me.
#[tokio::test]
async fn session_owner_lands_as_owner_role() {
    let s = spawn_with_auth_owner("mockuser").await;
    let http = browser_client();
    oauth_round_trip(&http, &s.daemon_base).await;

    let me: Value = http
        .get(format!("{}/api/me", s.daemon_base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["is_owner"], true);

    http.post(format!("{}/api/blueprints", s.daemon_base))
        .json(&json!({ "html": "<p>x</p>", "slug": "owner-test" }))
        .send()
        .await
        .unwrap();
    let c: Value = http
        .post(format!(
            "{}/api/blueprints/owner-test/comments",
            s.daemon_base
        ))
        .json(&json!({
            "author": "ignored",
            "body": "drive the plan",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(c["role"], "owner");
    assert_eq!(c["is_agent"], false);
}

/// Owner-matching is case-insensitive — GitHub login comparison shouldn't
/// care whether the env var was written with the same casing as the GH login.
#[tokio::test]
async fn owner_login_match_is_case_insensitive() {
    let s = spawn_with_auth_owner("MockUser").await;
    let http = browser_client();
    oauth_round_trip(&http, &s.daemon_base).await;

    let me: Value = http
        .get(format!("{}/api/me", s.daemon_base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me["is_owner"], true);
}

/// D4 migration: an existing comments DB written by the pre-role code path
/// should backfill `role = 'user'` for every row with `author_user_id`
/// set, leaving everything else as `'guest'`. The ALTER + UPDATE run in
/// a single transaction so a crash between them can't strand old rows.
#[tokio::test]
async fn migration_backfills_role_for_existing_logged_in_comments() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("blueprints.db");

    // Hand-write a DB that looks like the pre-role schema.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE blueprints (
          slug TEXT PRIMARY KEY,
          html BLOB NOT NULL,
          created_at INTEGER NOT NULL,
          delete_token TEXT NOT NULL
        );
        CREATE TABLE comments (
          id TEXT PRIMARY KEY,
          slug TEXT NOT NULL,
          author TEXT NOT NULL,
          body TEXT NOT NULL,
          selector TEXT NOT NULL,
          parent_id TEXT,
          resolved INTEGER NOT NULL DEFAULT 0,
          created_at INTEGER NOT NULL,
          processing_by TEXT,
          processing_started_at INTEGER,
          author_user_id INTEGER
        );
        INSERT INTO blueprints (slug, html, created_at, delete_token) VALUES ('legacy', x'7878', 0, 'tok');
        INSERT INTO comments (id, slug, author, body, selector, created_at, author_user_id)
            VALUES ('c_old_logged_in', 'legacy', 'mockuser',
                    'old reply', '{"type":"TextQuoteSelector","exact":"x"}', 0, 42);
        INSERT INTO comments (id, slug, author, body, selector, created_at, author_user_id)
            VALUES ('c_old_anon', 'legacy', 'drive-by',
                    'old anon', '{"type":"TextQuoteSelector","exact":"x"}', 0, NULL);
        "#,
    )
    .unwrap();
    drop(conn);

    // Now open the same DB via Store — migration should run.
    let _store = Store::open(&db_path).expect("migration");

    // Verify the column exists with the expected values.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let logged_in_role: String = conn
        .query_row(
            "SELECT role FROM comments WHERE id = 'c_old_logged_in'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let anon_role: String = conn
        .query_row(
            "SELECT role FROM comments WHERE id = 'c_old_anon'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(logged_in_role, "user");
    assert_eq!(anon_role, "guest");

    // is_agent should default to 0 for everything (no agent traffic predates
    // this migration).
    let is_agent: i64 = conn
        .query_row(
            "SELECT is_agent FROM comments WHERE id = 'c_old_logged_in'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(is_agent, 0);
}

// -------------------------------------------------------------------------
// Batch-processing indicator (slug-level "Claude is working on N comments")
// See `src/server.rs::BatchProcessing` and the `/batch-processing` endpoints.
// -------------------------------------------------------------------------

/// Publish a blueprint then create N parent comments on it. Returns the
/// freshly-minted parent IDs in insertion order. Centralizes the boilerplate
/// shared by every batch-processing test below — the assertions stay focused
/// on the indicator's lifecycle, not the setup.
async fn seed_batch_parents(
    http: &reqwest::Client,
    base: &str,
    slug: &str,
    n: usize,
) -> Vec<String> {
    http.post(format!("{base}/api/blueprints"))
        .json(&json!({ "html": "<p>x</p>", "slug": slug }))
        .send()
        .await
        .unwrap();
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let c: Value = http
            .post(format!("{base}/api/blueprints/{slug}/comments"))
            .json(&json!({
                "author": "perryqh",
                "body": format!("parent {i}"),
                "selector": { "type": "TextQuoteSelector", "exact": "x" }
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ids.push(c["id"].as_str().unwrap().to_string());
    }
    ids
}

/// Start endpoint stamps the indicator; the comments-list response surfaces it.
#[tokio::test]
async fn batch_processing_start_appears_in_comments_response() {
    let s = spawn().await;
    let http = client();
    let ids = seed_batch_parents(&http, &s.base, "bp1", 1).await;

    let r = http
        .post(format!("{}/api/blueprints/bp1/batch-processing", s.base))
        .json(&json!({ "author": "Claude Code", "parent_ids": &ids }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let listing: Value = http
        .get(format!("{}/api/blueprints/bp1/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let bp = &listing["batch_processing"];
    assert_eq!(bp["author"], "Claude Code");
    assert_eq!(bp["count"], 1);
    assert!(bp["started_at"].is_i64());
}

/// Explicit DELETE clears the indicator.
#[tokio::test]
async fn batch_processing_delete_clears_entry() {
    let s = spawn().await;
    let http = client();
    let ids = seed_batch_parents(&http, &s.base, "bp2", 1).await;

    http.post(format!("{}/api/blueprints/bp2/batch-processing", s.base))
        .json(&json!({ "author": "Claude Code", "parent_ids": &ids }))
        .send()
        .await
        .unwrap();

    let r = http
        .delete(format!("{}/api/blueprints/bp2/batch-processing", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let listing: Value = http
        .get(format!("{}/api/blueprints/bp2/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listing.get("batch_processing").is_none(),
        "DELETE should clear the entry; got {listing}"
    );
}

/// When every parent_id in the batch receives a reply, the indicator clears
/// server-side without anyone calling DELETE.
#[tokio::test]
async fn batch_processing_auto_clears_after_all_parents_replied() {
    let s = spawn().await;
    let http = client();
    let ids = seed_batch_parents(&http, &s.base, "bp3", 2).await;

    http.post(format!("{}/api/blueprints/bp3/batch-processing", s.base))
        .json(&json!({ "author": "Claude Code", "parent_ids": &ids }))
        .send()
        .await
        .unwrap();

    // First reply: still active (one pending parent left).
    http.post(format!(
        "{}/api/blueprints/bp3/comments/{}/replies",
        s.base, &ids[0]
    ))
    .json(&json!({ "author": "Claude Code", "body": "done" }))
    .send()
    .await
    .unwrap();
    let listing: Value = http
        .get(format!("{}/api/blueprints/bp3/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listing.get("batch_processing").is_some(),
        "indicator should still be active after only the first reply"
    );
    // count stays at 2 — never decremented; only pending_parents shrinks.
    assert_eq!(listing["batch_processing"]["count"], 2);

    // Second reply on the last pending parent → indicator clears.
    http.post(format!(
        "{}/api/blueprints/bp3/comments/{}/replies",
        s.base, &ids[1]
    ))
    .json(&json!({ "author": "Claude Code", "body": "also done" }))
    .send()
    .await
    .unwrap();
    let listing: Value = http
        .get(format!("{}/api/blueprints/bp3/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listing.get("batch_processing").is_none(),
        "indicator should auto-clear after the last reply lands; got {listing}"
    );
}

/// TTL safety net: an entry older than 5 minutes is evicted on read so a
/// crashed skill can't pin the indicator forever.
#[tokio::test]
async fn batch_processing_ttl_evicts_stale_entries() {
    use blueprint::server::BatchProcessing;
    use std::collections::HashSet;

    // Reach into AppState directly so we can implant an entry that's already
    // older than the TTL — no point waiting 5 real minutes in a unit test.
    let (listener, base) = bind_listener().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(
        blueprint::store::Store::open(&tmp.path().join("blueprints.db")).expect("open store"),
    );
    let state = blueprint::server::AppState::with_auth(store.clone(), None);
    let app = blueprint::server::router(state.clone());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    store
        .insert_blueprint("ttl-test", b"<p>x</p>", "tok", None)
        .unwrap();
    // Implant a stale entry: started_at is 10 minutes ago.
    {
        let mut m = state.batch_processing.lock().await;
        m.insert(
            "ttl-test".to_string(),
            BatchProcessing {
                author: "Claude Code".to_string(),
                count: 1,
                started_at: blueprint::store::now_ms() - 10 * 60 * 1000,
                pending_parents: HashSet::from(["c_stale".to_string()]),
            },
        );
    }
    let http = client();
    let listing: Value = http
        .get(format!("{}/api/blueprints/ttl-test/comments", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        listing.get("batch_processing").is_none(),
        "stale entry should be evicted on read"
    );
    // And the entry should be gone from state too (lazy eviction).
    let m = state.batch_processing.lock().await;
    assert!(!m.contains_key("ttl-test"));
}

/// Like spawn_with_auth but takes an owner login to plumb into AuthConfig.
async fn spawn_with_auth_owner(owner: &str) -> AuthedTest {
    let mock_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mock_addr = mock_listener.local_addr().unwrap();
    let mock_base = format!("http://{}", mock_addr);
    let mock_app = axum::Router::new()
        .route("/login/oauth/authorize", get(mock_authorize))
        .route("/login/oauth/access_token", post(mock_token))
        .route("/user", get(mock_user));
    tokio::spawn(async move {
        let _ = axum::serve(mock_listener, mock_app).await;
    });

    let (listener, daemon_base) = bind_listener().await;
    let cli_token = "test-cli-token-deadbeef".to_string();
    let auth = AuthConfig {
        client_id: "test-client".into(),
        client_secret: "test-secret".into(),
        callback_url: format!("{}/auth/github/callback", daemon_base),
        enabled: true,
        authorize_url: format!("{}/login/oauth/authorize", mock_base),
        token_url: format!("{}/login/oauth/access_token", mock_base),
        profile_url: format!("{}/user", mock_base),
        cli_token: cli_token.clone(),
        owner_login: Some(owner.to_string()),
    };
    let tmp = spawn_daemon_on(listener, Some(Arc::new(auth))).await;

    AuthedTest {
        daemon_base,
        cli_token,
        _tmp: tmp,
    }
}

/// Walk the mock OAuth round-trip end-to-end so the browser_client cookie jar
/// holds an authenticated session. Used by every role-test that needs to act
/// as a logged-in user.
async fn oauth_round_trip(http: &reqwest::Client, daemon_base: &str) {
    let r1 = http
        .get(format!("{}/login", daemon_base))
        .send()
        .await
        .unwrap();
    let authorize_url = r1
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let r2 = http.get(&authorize_url).send().await.unwrap();
    let callback_url = r2
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let _ = http.get(&callback_url).send().await.unwrap();
}

#[tokio::test]
async fn write_with_cli_bearer_token_succeeds() {
    let s = spawn_with_auth().await;
    let http = client();
    let r = http
        .post(format!("{}/api/blueprints", s.daemon_base))
        .bearer_auth(&s.cli_token)
        .json(&json!({ "html": "<p>x</p>", "slug": "p2" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn write_with_session_stamps_author_from_session() {
    let s = spawn_with_auth().await;
    let http = browser_client();
    // Round-trip OAuth to get a session.
    let r1 = http
        .get(format!("{}/login", s.daemon_base))
        .send()
        .await
        .unwrap();
    let authorize_url = r1
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let r2 = http.get(&authorize_url).send().await.unwrap();
    let callback_url = r2
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let _ = http.get(&callback_url).send().await.unwrap();

    // Publish a plan first (with the session).
    http.post(format!("{}/api/blueprints", s.daemon_base))
        .json(&json!({ "html": "<p>x</p>", "slug": "p3" }))
        .send()
        .await
        .unwrap();

    // POST a comment — client tries to lie about the author. Server should ignore.
    let c: Value = http
        .post(format!("{}/api/blueprints/p3/comments", s.daemon_base))
        .json(&json!({
            "author": "i-am-impostor",
            "body": "claim",
            "selector": { "type": "TextQuoteSelector", "exact": "x" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        c["author"], "mockuser",
        "server must stamp author from session"
    );
    assert!(
        c["author_user_id"].is_number(),
        "author_user_id should be set"
    );
}
