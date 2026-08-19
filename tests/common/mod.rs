//! Shared integration-test harness: an in-process daemon on an OS-assigned
//! port backed by a fresh SQLite store, plus a client and small builders.
//! Included via `mod common;` in the integration tests that need it.
#![allow(dead_code)] // each test crate uses a different subset of these

pub mod oauth;

use blueprint::server::{AppState, router};
use blueprint::store::Store;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;

pub struct TestServer {
    pub base: String,
    /// The live `AppState` the daemon is serving. Tests that need to observe
    /// server-internal state — `shutdown`, `batch_processing`, semaphore
    /// permits — reach through this instead of hand-rolling a fourth spawn
    /// function to keep a handle on it.
    pub state: AppState,
    pub _tmp: TempDir,
}

/// Bind a fresh listener on an OS-assigned port, returning the listener and
/// its `http://addr/` base URL.
///
/// Split out from `spawn` because auth setup is circular: `AuthConfig`'s
/// `callback_url` has to name the daemon's own address, so the port must be
/// known before the state exists.
pub async fn bind_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    (listener, format!("http://{addr}"))
}

/// Serve `state` on `listener`, returning the base URL.
pub fn serve(listener: TcpListener, state: AppState) {
    let app = router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
}

/// Spawn a daemon on the given listener with a fresh SQLite store, returning
/// the temp dir holding the database and the state being served.
pub async fn spawn_daemon_on(
    listener: TcpListener,
    auth: Option<Arc<blueprint::auth::AuthConfig>>,
) -> (TempDir, AppState) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(Store::open(tmp.path().join("blueprints.db")).expect("open store"));
    let state = AppState::with_auth(store, auth);
    serve(listener, state.clone());
    (tmp, state)
}

/// Spawn a legacy/no-auth daemon on an OS-assigned port with a fresh store.
pub async fn spawn() -> TestServer {
    let (listener, base) = bind_listener().await;
    let (tmp, state) = spawn_daemon_on(listener, None).await;
    TestServer {
        base,
        state,
        _tmp: tmp,
    }
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

/// How long a readiness poll will wait before declaring the thing it's waiting
/// for never happened. Generous, because it only costs wall-clock time on an
/// actual failure — the success path returns as soon as the condition holds.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Gap between readiness probes. Short enough that a satisfied condition is
/// observed almost immediately, long enough not to spin a core.
const READY_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Poll `cond` until it returns true, or panic with `what` after
/// `READY_TIMEOUT`.
///
/// This exists to replace `sleep(200ms)`-as-synchronisation. A fixed sleep is
/// wrong in both directions at once: too short and the test is flaky under load,
/// too long and every run pays the tax whether it needed to or not. Polling the
/// actual condition is both faster in the common case and correct in the slow
/// one.
pub async fn wait_until<F>(what: &str, mut cond: F)
where
    F: FnMut() -> bool,
{
    let deadline = std::time::Instant::now() + READY_TIMEOUT;
    loop {
        if cond() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out after {READY_TIMEOUT:?} waiting for {what}"
        );
        tokio::time::sleep(READY_POLL_INTERVAL).await;
    }
}

/// Wait until at least `n` permits have been taken out of `sem`.
///
/// The alternative — sleeping and hoping — is exactly the flakiness this
/// replaces: a long-poll handler holds its permit for precisely as long as it's
/// parked, so the permit count *is* the "N handlers have parked" signal and can
/// be observed directly instead of guessed at.
///
/// Reads capacity from the semaphore's own idle count rather than taking it as
/// an argument, so a test never hard-codes a ceiling that `server.rs` owns.
pub async fn wait_for_parked_pollers(sem: &tokio::sync::Semaphore, n: usize) {
    let capacity = sem.available_permits();
    assert!(
        capacity >= n,
        "asked to wait for {n} parked pollers but the semaphore only has {capacity} permits"
    );
    wait_until(&format!("{n} long-poll(s) to park"), || {
        capacity - sem.available_permits() >= n
    })
    .await;
}
