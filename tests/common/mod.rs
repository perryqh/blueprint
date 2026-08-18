//! Shared integration-test harness: an in-process daemon on an OS-assigned
//! port backed by a fresh SQLite store, plus a client and small builders.
//! Included via `mod common;` in the integration tests that need it.
#![allow(dead_code)] // each test crate uses a different subset of these

use blueprint::server::{AppState, router};
use blueprint::store::Store;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;

pub struct TestServer {
    pub base: String,
    pub _tmp: TempDir,
}

/// Spawn a legacy/no-auth daemon on an OS-assigned port with a fresh store.
pub async fn spawn() -> TestServer {
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

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// A minimal TextQuoteSelector JSON payload for a comment anchored on `exact`.
pub fn selector(exact: &str) -> Value {
    json!({ "type": "TextQuoteSelector", "exact": exact, "prefix": "", "suffix": "" })
}
