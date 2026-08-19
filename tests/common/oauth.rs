//! Mock GitHub OAuth rig: an in-process server that plays GitHub's three
//! endpoints, plus the helpers for driving the handshake against it.
//!
//! Lives here rather than in `e2e.rs` because it's the harness, not a test —
//! and because a second copy of the mock router had already appeared in that
//! file, which is precisely how two mocks drift into disagreeing about what
//! GitHub does.

use super::{bind_listener, client, serve, spawn_daemon_on};
use axum::Json as AxumJson;
use axum::extract::Query as AxumQuery;
use axum::response::Redirect;
use axum::routing::{get, post};
use blueprint::auth::AuthConfig;
use blueprint::server::AppState;
use blueprint::store::Store;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

/// The CLI bearer token every auth-enabled test daemon is configured with.
pub const TEST_CLI_TOKEN: &str = "test-cli-token-deadbeef";

/// The login the mock profile endpoint reports. Tests that care about the
/// owner/user role split compare against this.
pub const MOCK_LOGIN: &str = "mockuser";

pub struct AuthedTest {
    pub daemon_base: String,
    pub cli_token: String,
    /// The daemon's own state, so a test can observe server-side effects the
    /// HTTP response doesn't carry — notably whether the shutdown notify fired.
    pub state: AppState,
    pub _tmp: TempDir,
}

/// Spawn an in-process mock GitHub and return its base URL.
pub async fn spawn_mock_github() -> String {
    let (listener, base) = bind_listener().await;
    let app = axum::Router::new()
        .route("/login/oauth/authorize", get(mock_authorize))
        .route("/login/oauth/access_token", post(mock_token))
        .route("/user", get(mock_user));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    base
}

/// The `AuthConfig` an auth-enabled test daemon runs with: real OAuth flow,
/// GitHub's endpoints swapped for `mock_base`.
fn test_auth_config(daemon_base: &str, mock_base: &str, owner_login: Option<String>) -> AuthConfig {
    AuthConfig {
        client_id: "test-client".into(),
        client_secret: "test-secret".into(),
        callback_url: format!("{daemon_base}/auth/github/callback"),
        enabled: true,
        authorize_url: format!("{mock_base}/login/oauth/authorize"),
        token_url: format!("{mock_base}/login/oauth/access_token"),
        profile_url: format!("{mock_base}/user"),
        cli_token: TEST_CLI_TOKEN.into(),
        owner_login,
    }
}

/// Spawn an auth-enabled daemon over a caller-supplied store, with its OAuth
/// endpoints pointed at `mock_base`. Taking the store lets a test stand up a
/// *second* daemon over the same database to simulate a restart.
pub async fn spawn_auth_daemon_with_store(store: Arc<Store>, mock_base: &str) -> String {
    // AuthConfig.callback_url needs the daemon URL, so bind first.
    let (listener, daemon_base) = bind_listener().await;
    let auth = test_auth_config(&daemon_base, mock_base, None);
    serve(
        listener,
        blueprint::server::AppState::with_auth(store, Some(Arc::new(auth))),
    );
    daemon_base
}

/// Spawn a daemon with auth enabled plus its own mock GitHub.
///
/// `owner_login` is what makes this one function serve both the "no configured
/// owner" tests and the owner/user role tests — the second used to re-inline
/// the whole mock router to vary this single field.
pub async fn spawn_with_auth_owner(owner_login: Option<&str>) -> AuthedTest {
    let mock_base = spawn_mock_github().await;
    let (listener, daemon_base) = bind_listener().await;
    let auth = test_auth_config(&daemon_base, &mock_base, owner_login.map(str::to_string));
    let (tmp, state) = spawn_daemon_on(listener, Some(Arc::new(auth))).await;
    AuthedTest {
        daemon_base,
        cli_token: TEST_CLI_TOKEN.into(),
        state,
        _tmp: tmp,
    }
}

/// `spawn_with_auth_owner(None)` — auth on, no owner configured.
pub async fn spawn_with_auth() -> AuthedTest {
    spawn_with_auth_owner(None).await
}

async fn mock_authorize(AxumQuery(p): AxumQuery<HashMap<String, String>>) -> Redirect {
    // GitHub would render a consent UI here; we just bounce straight back to the
    // daemon's callback URL with a synthetic code, preserving the state nonce.
    let redirect_uri = p.get("redirect_uri").cloned().unwrap_or_default();
    let state = p.get("state").cloned().unwrap_or_default();
    Redirect::to(&format!("{redirect_uri}?code=mock-code&state={state}"))
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
        "login": MOCK_LOGIN,
        "name": "Mock User",
        "avatar_url": "https://example.com/mockuser.png"
    }))
}

/// A reqwest client with a cookie jar and *no* automatic redirect following — we
/// need to inspect the 302 from /login and the 302 from /callback.
pub fn browser_client() -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// `Location` header of a redirect response, as an owned String.
pub fn location(r: &reqwest::Response) -> String {
    r.headers()
        .get("location")
        .expect("redirect has a Location")
        .to_str()
        .unwrap()
        .to_string()
}

/// The `ps_session=<value>` pair from a response's `Set-Cookie`, if it set one.
pub fn session_cookie(r: &reqwest::Response) -> Option<String> {
    r.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.split(';').next().filter(|p| p.starts_with("ps_session=")))
        .map(|p| p.to_string())
}

/// Send `cookie` to `/api/me` from a client holding no other state — replaying a
/// captured session id the way a stale tab or a copied cookie would.
pub async fn replay_session(base: &str, cookie: &str) -> reqwest::StatusCode {
    client()
        .get(format!("{base}/api/me"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .status()
}

/// Drive the full mock-GitHub round trip on `http`, returning the session
/// cookie the daemon ended up issuing.
///
/// The one round-trip helper. There were three walks of this same three-hop
/// redirect chain — this one, a copy that asserted nothing, and a third inlined
/// into a test body — so a change to the handshake had to be made in three
/// places to stay honest.
pub async fn login_via_mock(http: &reqwest::Client, base: &str) -> String {
    let r1 = http.get(format!("{base}/login")).send().await.unwrap();
    let issued = session_cookie(&r1);
    let r2 = http.get(location(&r1)).send().await.unwrap();
    let r3 = http.get(location(&r2)).send().await.unwrap();
    assert_eq!(r3.status(), 303, "callback should complete the login");
    // The callback re-saves the session; it only re-issues the cookie if the id
    // changed, so fall back to the one /login handed out.
    session_cookie(&r3)
        .or(issued)
        .expect("login issues a session cookie")
}
