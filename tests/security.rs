//! Security hardening (Step 1): CSP sandbox header on /raw, request-body cap,
//! and the held-connection ceiling on long-poll endpoints.

use blueprint::server::{AppState, router};
use blueprint::store::Store;
use serde_json::json;
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
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

async fn publish(http: &reqwest::Client, base: &str, slug: &str) {
    http.post(format!("{base}/api/blueprints"))
        .json(&json!({ "html": "<p>hi</p>", "slug": slug }))
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn raw_sandboxes_top_level_open_but_not_iframe_embed() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "csp").await;

    // A top-level navigation (Sec-Fetch-Dest: document) — a shared link or the
    // version-badge "open snapshot" — is sandboxed into an opaque origin.
    let top = http
        .get(format!("{}/api/blueprints/csp/raw", s.base))
        .header("sec-fetch-dest", "document")
        .send()
        .await
        .unwrap();
    assert_eq!(top.status(), 200);
    assert_eq!(
        top.headers()
            .get("content-security-policy")
            .expect("CSP on top-level open")
            .to_str()
            .unwrap(),
        "sandbox allow-scripts"
    );

    // The reviewer's iframe embed (Sec-Fetch-Dest: iframe) must NOT be
    // sandboxed — app.js reaches into contentDocument for anchoring and theme
    // injection, which an opaque origin would break.
    let embed = http
        .get(format!("{}/api/blueprints/csp/raw", s.base))
        .header("sec-fetch-dest", "iframe")
        .send()
        .await
        .unwrap();
    assert_eq!(embed.status(), 200);
    assert!(
        embed.headers().get("content-security-policy").is_none(),
        "iframe embed must stay same-origin (no sandbox CSP)"
    );
}

#[tokio::test]
async fn oversize_body_is_rejected_with_413() {
    let s = spawn().await;
    let http = client();

    // ~9 MiB of HTML, over the 8 MiB DefaultBodyLimit.
    let huge = "a".repeat(9 * 1024 * 1024);
    let r = http
        .post(format!("{}/api/blueprints", s.base))
        .json(&json!({ "html": huge, "slug": "toobig" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413, "oversize body must be refused");
}

#[tokio::test]
async fn held_long_polls_are_capped_at_the_ceiling() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "cap").await;

    // Occupy all 32 permits with slow-path long-polls (no comments exist, so
    // each parks on the ~30s timeout holding a permit). Fire-and-forget: the
    // runtime aborts them when the test returns.
    for _ in 0..32 {
        let http = http.clone();
        let url = format!("{}/api/blueprints/cap/wait-comment?since=0", s.base);
        tokio::spawn(async move {
            let _ = http.get(url).send().await;
        });
    }
    // Give the 32 a beat to acquire their permits.
    tokio::time::sleep(Duration::from_millis(750)).await;

    // The 33rd is refused fast rather than pinning another connection.
    let r = http
        .get(format!(
            "{}/api/blueprints/cap/wait-comment?since=0",
            s.base
        ))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "over-ceiling long-poll must 503");
}
