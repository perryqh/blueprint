//! Security hardening (Step 1): CSP sandbox header on /raw, request-body cap,
//! and the held-connection ceiling on long-poll endpoints.

mod common;

use common::{client, spawn};
use serde_json::json;
use std::time::Duration;

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

/// The case the two assertions above didn't cover, and the one that mattered:
/// the gate reads a header the *client* controls, so the interesting question is
/// what happens when it says something we didn't anticipate — or nothing at all.
///
/// It used to ask "is this `Sec-Fetch-Dest: document`?" and sandbox only then, so
/// a missing header (curl, an older browser, a hand-rolled request) or any other
/// destination served agent-authored HTML *unsandboxed* in the daemon's origin.
/// Now anything that isn't positively our own iframe embed gets the sandbox.
#[tokio::test]
async fn raw_sandboxes_by_default_when_fetch_metadata_is_absent_or_unexpected() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "csp-default").await;

    // `embed` and `object` are real framing destinations a hostile page can use;
    // `frame` is the legacy one; the garbage value stands in for anything future
    // browsers add. None of them are our iframe, so all must be sandboxed.
    for dest in [
        None,
        Some("embed"),
        Some("object"),
        Some("frame"),
        Some("!?"),
    ] {
        let mut req = http.get(format!("{}/api/blueprints/csp-default/raw", s.base));
        if let Some(d) = dest {
            req = req.header("sec-fetch-dest", d);
        }
        let r = req.send().await.unwrap();
        assert_eq!(r.status(), 200);

        let label = dest.unwrap_or("<absent>");
        assert_eq!(
            r.headers()
                .get("content-security-policy")
                .unwrap_or_else(|| panic!("no sandbox CSP for sec-fetch-dest: {label}"))
                .to_str()
                .unwrap(),
            "sandbox allow-scripts",
            "sec-fetch-dest {label} must be sandboxed"
        );
        assert_eq!(
            r.headers()
                .get("x-frame-options")
                .expect("framing policy must always be set")
                .to_str()
                .unwrap(),
            "SAMEORIGIN",
            "any origin could otherwise frame /raw and read it cross-origin"
        );
    }
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
