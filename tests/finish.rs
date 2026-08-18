//! The durable "Finish Review" latch. A click is persisted on the blueprint
//! rather than broadcast into an in-memory channel, so it survives having no
//! `blueprint watch` subscribed at the moment it lands — and is claimed exactly
//! once, by exactly one waiter.

mod common;

use common::{client, spawn};
use serde_json::{Value, json};
use std::time::Duration;

async fn publish(http: &reqwest::Client, base: &str, slug: &str) {
    let r = http
        .post(format!("{base}/api/blueprints"))
        .json(&json!({ "html": "<p>plan</p>", "slug": slug }))
        .send()
        .await
        .expect("publish");
    assert!(r.status().is_success(), "publish failed: {}", r.status());
}

async fn finish(http: &reqwest::Client, base: &str, slug: &str) -> i64 {
    let r = http
        .post(format!("{base}/api/blueprints/{slug}/finish"))
        .send()
        .await
        .expect("finish");
    assert_eq!(r.status(), 200);
    r.json::<Value>().await.unwrap()["finished_at"]
        .as_i64()
        .expect("finish returns its timestamp")
}

/// The case the whole design exists for: click with nothing listening, then
/// connect. Before the latch this dropped the click and hung for 4 hours.
#[tokio::test]
async fn finish_clicked_with_no_waiter_is_claimed_by_a_later_wait() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "p").await;

    let finished_at = finish(&http, &s.base, "p").await;

    let r = http
        .get(format!("{}/api/blueprints/p/wait", s.base))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("a finish predating the waiter must still resolve it");
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["finished_at"]
            .as_i64()
            .unwrap(),
        finished_at
    );
}

/// One click must wake exactly one of several parked waiters. The others keep
/// waiting — a single finish is not a broadcast to every listener.
#[tokio::test]
async fn one_finish_is_claimed_by_exactly_one_of_several_parked_waiters() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "p").await;

    // Park three waiters, each with a timeout well short of the test's patience
    // so the losers report a timeout rather than hanging the suite.
    let waiters: Vec<_> = (0..3)
        .map(|_| {
            let base = s.base.clone();
            tokio::spawn(async move {
                reqwest::Client::new()
                    .get(format!("{base}/api/blueprints/p/wait"))
                    .timeout(Duration::from_secs(3))
                    .send()
                    .await
                    .map(|r| r.status().as_u16())
            })
        })
        .collect();

    // Let all three reach the slow path before the single click lands.
    tokio::time::sleep(Duration::from_millis(200)).await;
    finish(&http, &s.base, "p").await;

    let mut claimed = 0;
    for w in waiters {
        if let Ok(Ok(200)) = w.await {
            claimed += 1;
        }
    }
    assert_eq!(
        claimed, 1,
        "exactly one waiter should claim a single finish, got {claimed}"
    );
}

/// A claimed latch must not resolve the next round, or every subsequent review
/// would end the instant it started.
#[tokio::test]
async fn a_claimed_finish_does_not_resolve_the_next_round() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "p").await;
    finish(&http, &s.base, "p").await;

    // First wait claims it.
    let r = http
        .get(format!("{}/api/blueprints/p/wait", s.base))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("first wait claims the finish");
    assert_eq!(r.status(), 200);

    // Second wait has nothing to claim and must park. Timing out is the pass.
    let second = http
        .get(format!("{}/api/blueprints/p/wait", s.base))
        .timeout(Duration::from_millis(300))
        .send()
        .await;
    assert!(
        second.is_err(),
        "an already-claimed finish must not resolve a later wait"
    );

    // Clicking again re-raises the latch, so a new round can end.
    let refinished = finish(&http, &s.base, "p").await;
    let r = http
        .get(format!("{}/api/blueprints/p/wait", s.base))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("re-finishing raises the latch again");
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.json::<Value>().await.unwrap()["finished_at"]
            .as_i64()
            .unwrap(),
        refinished
    );
}

/// `finished_at` is never cleared by a claim — it's what the reviewer header
/// renders, so the finished state has to survive both the claim and a reload.
#[tokio::test]
async fn finished_at_survives_the_claim_and_is_exposed_on_comments() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "p").await;

    let before: Value = http
        .get(format!("{}/api/blueprints/p/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        before.get("finished_at").is_none(),
        "a never-finished blueprint omits finished_at"
    );

    let finished_at = finish(&http, &s.base, "p").await;
    http.get(format!("{}/api/blueprints/p/wait", s.base))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("claim the latch");

    let after: Value = http
        .get(format!("{}/api/blueprints/p/comments", s.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        after["finished_at"].as_i64().unwrap(),
        finished_at,
        "finished_at outlives the claim so the header stays accurate"
    );
}

#[tokio::test]
async fn finish_and_wait_404_on_unknown_slug() {
    let s = spawn().await;
    let http = client();

    let r = http
        .post(format!("{}/api/blueprints/nope/finish", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    let r = http
        .get(format!("{}/api/blueprints/nope/wait", s.base))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("wait should 404 rather than park on an unknown slug");
    assert_eq!(r.status(), 404);
}

/// Deleting a blueprint out from under a parked waiter must release it with a
/// 404 rather than leaving it pinned until its timeout.
#[tokio::test]
async fn deleting_a_blueprint_releases_a_parked_waiter() {
    let s = spawn().await;
    let http = client();
    publish(&http, &s.base, "p").await;

    let base = s.base.clone();
    let waiter = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("{base}/api/blueprints/p/wait"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().as_u16())
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let r = http
        .delete(format!("{}/api/blueprints/p", s.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    let status = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter released promptly on delete")
        .unwrap();
    assert_eq!(status.unwrap(), 404);
}
