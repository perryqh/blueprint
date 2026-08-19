//! GitHub OAuth + session wiring.
//!
//! Reads config from environment variables (sourced from ~/.blueprint/env on
//! daemon startup). Cookie-backed sessions are persisted in SQLite via
//! `crate::session_store` — the daemon restarts too often for a process-local
//! store to hold a login, let alone an in-flight OAuth handshake. The user
//! identity in session is just the row id of the `users` table.

use crate::error::AppError;
use crate::server::AppState;
use axum::Json;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use oauth2::basic::BasicClient;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet,
    RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tower_sessions::Session;

/// `BasicClient` once the auth and token endpoints are set. oauth2 5 tracks
/// which endpoints are configured in the type, so naming the state keeps the
/// signature of `oauth_client` readable.
type ConfiguredClient = BasicClient<
    EndpointSet,    // auth
    EndpointNotSet, // device auth
    EndpointNotSet, // introspection
    EndpointNotSet, // revocation
    EndpointSet,    // token
>;

const SESSION_USER_ID: &str = "user_id";
const SESSION_OAUTH_STATE: &str = "oauth_state";
const SESSION_POST_LOGIN_REDIRECT: &str = "post_login_redirect";

#[derive(Clone)]
pub struct AuthConfig {
    pub client_id: String,
    pub client_secret: String,
    pub callback_url: String,
    pub enabled: bool,
    /// Override OAuth endpoints for testing against a mock GitHub.
    pub authorize_url: String,
    pub token_url: String,
    pub profile_url: String,
    /// Shared secret read by the CLI from ~/.blueprint/cli-token and sent as
    /// `Authorization: Bearer ...` on write requests. Auto-generated on daemon
    /// startup if the file is missing.
    pub cli_token: String,
    /// GitHub login (case-insensitive) of the blueprint owner — the one user
    /// whose comments should trip a plan edit. Read from
    /// `BLUEPRINT_OWNER_GITHUB_LOGIN`. `None` = no owner; every comment is
    /// `guest` or `user` and (per the skill) every comment trips an edit, so
    /// the daemon logs a startup WARN to make that obvious.
    pub owner_login: Option<String>,
}

impl AuthConfig {
    /// Build config from a parsed env file, with the real process environment
    /// taking precedence. If client_id+secret aren't set, auth is disabled and
    /// the daemon runs in legacy local-trust mode.
    pub fn from_env_file(env: &EnvFile) -> Self {
        let client_id = env.get("GITHUB_CLIENT_ID").unwrap_or_default();
        let client_secret = env.get("GITHUB_CLIENT_SECRET").unwrap_or_default();
        let enabled = !client_id.is_empty() && !client_secret.is_empty();
        AuthConfig {
            client_id,
            client_secret,
            callback_url: env
                .get("OAUTH_CALLBACK_URL")
                .unwrap_or_else(|| "http://127.0.0.1:7321/auth/github/callback".into()),
            enabled,
            authorize_url: env
                .get("OAUTH_AUTHORIZE_URL")
                .unwrap_or_else(|| "https://github.com/login/oauth/authorize".into()),
            token_url: env
                .get("OAUTH_TOKEN_URL")
                .unwrap_or_else(|| "https://github.com/login/oauth/access_token".into()),
            profile_url: env
                .get("OAUTH_PROFILE_URL")
                .unwrap_or_else(|| "https://api.github.com/user".into()),
            cli_token: ensure_cli_token(),
            owner_login: env
                .get("BLUEPRINT_OWNER_GITHUB_LOGIN")
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }

    /// Validate the three endpoint URLs and build the OAuth client.
    ///
    /// Returns `Err` rather than panicking: these strings come from
    /// `~/.blueprint/env`, so a typo used to take the whole daemon down at the
    /// first `/login` with `expect("authorize URL parses")`.
    fn oauth_client(&self) -> Result<ConfiguredClient, AppError> {
        let bad = |what: &str, e: oauth2::url::ParseError| {
            AppError::Config(format!("{what} is not a valid URL: {e}"))
        };
        let auth_uri =
            AuthUrl::new(self.authorize_url.clone()).map_err(|e| bad("OAUTH_AUTHORIZE_URL", e))?;
        let token_uri =
            TokenUrl::new(self.token_url.clone()).map_err(|e| bad("OAUTH_TOKEN_URL", e))?;
        let redirect_uri = RedirectUrl::new(self.callback_url.clone())
            .map_err(|e| bad("OAUTH_CALLBACK_URL", e))?;
        Ok(BasicClient::new(ClientId::new(self.client_id.clone()))
            .set_client_secret(ClientSecret::new(self.client_secret.clone()))
            .set_auth_uri(auth_uri)
            .set_token_uri(token_uri)
            .set_redirect_uri(redirect_uri))
    }
}

/// Read or auto-create ~/.blueprint/cli-token (mode 600). Returned even if the
/// home dir lookup fails — degenerate empty string disables the CLI bypass.
fn ensure_cli_token() -> String {
    use std::io::Write;
    let path = match dirs::home_dir() {
        Some(h) => h.join(".blueprint").join("cli-token"),
        None => return String::new(),
    };
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    let token = crate::slug::random_alphanumeric(32);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        let _ = writeln!(f, "{token}");
    }
    token
}

/// Contents of `~/.blueprint/env`, parsed but *not* installed into the process
/// environment.
///
/// This used to call `std::env::set_var` under an `unsafe` block whose comment
/// claimed it ran "before any threads spawn". That was false: `#[tokio::main]`
/// builds the multi-threaded runtime — and its whole worker pool — before the
/// async body runs, so the mutation raced every one of those threads. That race
/// is exactly the UB that made `set_var` unsafe in edition 2024.
///
/// Passing the values explicitly removes the `unsafe`, removes the ordering
/// constraint, and makes `AuthConfig` constructible in a unit test without
/// touching ambient state.
#[derive(Debug, Default, Clone)]
pub struct EnvFile(HashMap<String, String>);

impl EnvFile {
    /// Read and parse the env file. A missing file is not an error (it just
    /// means auth is disabled); a *malformed* one is, so a typo surfaces at
    /// startup instead of silently disabling login.
    pub fn load() -> Result<Self, AppError> {
        let Some(path) = dirs::home_dir().map(|h| h.join(".blueprint").join("env")) else {
            return Ok(Self::default());
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        Self::parse(&content).map_err(|e| AppError::Config(format!("{}: {e}", path.display())))
    }

    fn parse(content: &str) -> Result<Self, String> {
        let mut map = HashMap::new();
        for (lineno, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("malformed line {}: {line}", lineno + 1))?;
            map.insert(
                key.trim().to_string(),
                value.trim().trim_matches('"').to_string(),
            );
        }
        Ok(EnvFile(map))
    }

    /// The real environment wins over the file, preserving the precedence the
    /// old `set_var` version had (it skipped keys already set).
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().or_else(|| self.0.get(key).cloned())
    }
}

/// Whoever is logged in for the current request, if anyone.
pub async fn current_user(
    session: &Session,
    state: &AppState,
) -> Result<Option<crate::store::User>, AppError> {
    let user_id: Option<i64> = session.get(SESSION_USER_ID).await.unwrap_or(None);
    match user_id {
        Some(id) => state.store.get_user(id),
        None => Ok(None),
    }
}

/// Resolution of the request's "who's calling" state.
#[derive(Debug, Clone)]
pub enum Identity {
    SessionUser(crate::store::User),
    /// Valid `Authorization: Bearer <cli-token>` header — CLI / agent / Claude.
    /// We don't pin this to a specific user; the caller's `author` field is trusted.
    CliBearer,
    None,
}

/// Resolve who's calling. Checks the bearer token first because CLI / agent
/// traffic is the hot path (every `blueprint` command, every `watch --stream`
/// reconnect) and skipping the session lookup for them avoids a SQLite hit per
/// request.
pub async fn identity_from_request(
    session: &Session,
    headers: &HeaderMap,
    state: &AppState,
) -> Result<Identity, AppError> {
    if let Some(cfg) = &state.auth
        && !cfg.cli_token.is_empty()
        && let Some(hdr) = headers.get("authorization")
        && let Ok(s) = hdr.to_str()
        && let Some(token) = s.strip_prefix("Bearer ")
        && secret_eq(token.trim(), &cfg.cli_token)
    {
        return Ok(Identity::CliBearer);
    }
    if let Some(user) = current_user(session, state).await? {
        return Ok(Identity::SessionUser(user));
    }
    Ok(Identity::None)
}

/// Compare a secret against the expected value without short-circuiting on the
/// first differing byte.
///
/// The daemon is localhost-only, which narrows but does not remove the exposure:
/// any local process — including a page in the user's browser fetching
/// `127.0.0.1:7321` — can time responses and recover the token byte by byte.
/// Length is still observable, which is fine for a fixed-length generated token.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What a write is trying to do, which decides whether anonymity is acceptable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// Adding a comment or reply. Open to anonymous browsers by design — the
    /// `guest` role exists precisely to tag these, and the agent decides what
    /// to do with them based on `role`.
    Comment,
    /// Creating, replacing, or deleting a blueprint. Requires a real identity
    /// when auth is configured.
    Blueprint,
}

/// Gate a write on the caller's identity.
///
/// This function used to be an unconditional `Ok(())` while `WriteIdentity`'s
/// doc comment promised it enforced something — so with OAuth fully configured,
/// anyone who could reach the port could `DELETE` a blueprint. Anonymous
/// *comments* are deliberate; anonymous destruction was not.
pub fn require_identity_for_writes(
    identity: &Identity,
    state: &AppState,
    kind: WriteKind,
) -> Result<(), AppError> {
    // Legacy local-trust mode: nothing is configured, so nothing is enforced.
    if state.auth.is_none() {
        return Ok(());
    }
    match (kind, identity) {
        (WriteKind::Comment, _) => Ok(()),
        (WriteKind::Blueprint, Identity::SessionUser(_) | Identity::CliBearer) => Ok(()),
        (WriteKind::Blueprint, Identity::None) => Err(AppError::Unauthorized),
    }
}

/// Map an `Identity` to the role we stamp on writes.
///
/// - `SessionUser` whose login matches `owner_login` (case-insensitive) → `Owner`.
/// - Any other `SessionUser` → `User`.
/// - `CliBearer` → `User`. The agent posts as a "user" role; its
///   distinguishing flag is the orthogonal `is_agent` boolean.
/// - `None` → `Guest`.
pub fn role_for(identity: &Identity, state: &AppState) -> crate::store::AuthorRole {
    use crate::store::AuthorRole;
    match identity {
        Identity::SessionUser(u) => {
            let owner = state
                .auth
                .as_ref()
                .and_then(|a| a.owner_login.as_deref())
                .unwrap_or("");
            if !owner.is_empty() && owner.eq_ignore_ascii_case(&u.login) {
                AuthorRole::Owner
            } else {
                AuthorRole::User
            }
        }
        Identity::CliBearer => AuthorRole::User,
        Identity::None => AuthorRole::Guest,
    }
}

/// Is the calling identity the agent (CLI bearer)? Used to render Claude's
/// replies distinctly in the frontend. Replaces the brittle
/// `author.toLowerCase() === 'claude'` heuristic — the CLI posts as
/// `'Claude Code'` so the string check never fired.
pub fn is_agent(identity: &Identity) -> bool {
    matches!(identity, Identity::CliBearer)
}

/// Is `login` the configured blueprint owner? Used to render the "owner" pill
/// on `/api/me` so the frontend can distinguish the driving user without
/// re-deriving it from the role on every comment.
pub fn is_owner_login(state: &AppState, login: &str) -> bool {
    let owner = state
        .auth
        .as_ref()
        .and_then(|a| a.owner_login.as_deref())
        .unwrap_or("");
    !owner.is_empty() && owner.eq_ignore_ascii_case(login)
}

async fn identity_for(
    parts: &mut axum::http::request::Parts,
    state: &AppState,
    kind: WriteKind,
) -> Result<Identity, AppError> {
    let session = Session::from_request_parts(parts, state)
        .await
        .map_err(|(_, msg)| AppError::Session(msg.to_string()))?;
    let identity = identity_from_request(&session, &parts.headers, state).await?;
    require_identity_for_writes(&identity, state, kind)?;
    Ok(identity)
}

/// Extractor for comment writes. Anonymous callers are allowed through and land
/// as `guest`; the role tag is what carries provenance, not the gate.
pub struct WriteIdentity(pub Identity);

/// Extractor for blueprint create/replace/delete. Requires a session or the CLI
/// bearer once auth is configured — unlike comments, there is no sensible
/// anonymous story for destroying someone's plan.
pub struct BlueprintWrite(pub Identity);

#[axum::async_trait]
impl axum::extract::FromRequestParts<AppState> for WriteIdentity {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(WriteIdentity(
            identity_for(parts, state, WriteKind::Comment).await?,
        ))
    }
}

#[axum::async_trait]
impl axum::extract::FromRequestParts<AppState> for BlueprintWrite {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(BlueprintWrite(
            identity_for(parts, state, WriteKind::Blueprint).await?,
        ))
    }
}

#[derive(Deserialize)]
pub struct LoginParams {
    #[serde(default)]
    pub next: Option<String>,
}

/// GET /login — generate a state nonce, stash it in the session, redirect to GitHub.
pub async fn login(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<LoginParams>,
) -> Result<Response, AppError> {
    let auth = state.auth.as_ref().ok_or(AppError::AuthNotConfigured)?;
    let client = auth.oauth_client()?;
    let (auth_url, csrf_state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("read:user".into()))
        .url();
    session
        .insert(SESSION_OAUTH_STATE, csrf_state.secret().clone())
        .await
        .map_err(|e| AppError::Session(e.to_string()))?;
    if let Some(next) = params.next {
        session
            .insert(SESSION_POST_LOGIN_REDIRECT, next)
            .await
            .map_err(|e| AppError::Session(e.to_string()))?;
    }
    Ok(Redirect::to(auth_url.as_str()).into_response())
}

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: String,
    pub state: String,
}

/// GET /auth/github/callback — exchange code, fetch profile, upsert user, set session.
pub async fn callback(
    State(state): State<AppState>,
    session: Session,
    Query(params): Query<CallbackParams>,
) -> Result<Response, AppError> {
    let auth = state.auth.as_ref().ok_or(AppError::AuthNotConfigured)?;

    // Verify the state nonce matches what we stashed at /login.
    let expected: Option<String> = session.get(SESSION_OAUTH_STATE).await.unwrap_or(None);
    match expected {
        Some(s) if s == params.state => {
            // Consumed on the happy path. Note the removal only reaches the
            // store if this request ends in a non-5xx response — tower-sessions
            // deliberately skips saving on server errors. So if the token
            // exchange below fails, the nonce survives and the user can retry
            // the whole round trip; that's the behavior we want, not an
            // oversight, but it does mean the nonce isn't single-use when the
            // exchange breaks.
            session.remove::<String>(SESSION_OAUTH_STATE).await.ok();
        }
        // Present but different: a replayed, stale, or forged callback. Refuse
        // it and don't invite a retry.
        Some(_) => return Err(AppError::BadRequest("state mismatch".into())),
        // No nonce at all — this session never started a login. Cookies
        // cleared mid-flow, or the session expired while the consent screen
        // sat open. Nothing is authenticated either way, and a fresh /login
        // mints a new nonce, so hand back a way forward instead of a dead end.
        None => return Ok(restart_login_page()),
    }

    // One HTTP client for both legs of the handshake. oauth2 5 takes the client
    // as a parameter rather than shipping its own, which is what collapsed the
    // duplicate reqwest 0.11 + 0.12 stacks this binary used to link.
    let http = reqwest::Client::new();

    // Exchange the code for an access token.
    let client = auth.oauth_client()?;
    let token = client
        .exchange_code(AuthorizationCode::new(params.code))
        .request_async(&http)
        .await
        .map_err(AppError::upstream)?;

    // Fetch the GitHub user profile. GitHub requires a User-Agent.
    let access_token = token.access_token().secret();
    let profile: GitHubUser = http
        .get(&auth.profile_url)
        .bearer_auth(access_token)
        .header("User-Agent", "blueprint")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(AppError::upstream)?
        .error_for_status()
        .map_err(AppError::upstream)?
        .json()
        .await
        .map_err(AppError::upstream)?;

    // Upsert into users table.
    let user_id = state.store.upsert_user(
        profile.id,
        &profile.login,
        profile.name.as_deref(),
        profile.avatar_url.as_deref(),
    )?;
    session
        .insert(SESSION_USER_ID, user_id)
        .await
        .map_err(|e| AppError::Session(e.to_string()))?;

    // Honor post_login_redirect from /login if present.
    let redirect_to: Option<String> = session
        .remove(SESSION_POST_LOGIN_REDIRECT)
        .await
        .ok()
        .flatten();
    let target = redirect_to.unwrap_or_else(|| "/".into());
    Ok(Redirect::to(&target).into_response())
}

/// 400 page for a callback whose session carries no login attempt. Deliberately
/// a one-click restart rather than a bare error string — the old plain-text
/// `state mismatch or missing` left the browser on a black dead-end page with
/// no hint that retrying would work.
fn restart_login_page() -> Response {
    const BODY: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>Sign-in expired</title>
<style>
  body { background:#111; color:#eee; font:15px/1.6 -apple-system, system-ui, sans-serif;
         display:grid; place-content:center; height:100vh; margin:0; text-align:center }
  a { color:#7aa2f7 }
</style>
<h1>Sign-in expired</h1>
<p>This sign-in attempt is no longer valid — it was started by a different
   browser session, or it sat too long before finishing.</p>
<p><a href="/login">Sign in with GitHub again</a></p>
"#;
    (
        StatusCode::BAD_REQUEST,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        BODY,
    )
        .into_response()
}

#[derive(Deserialize)]
struct GitHubUser {
    id: i64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

/// POST /logout — clear the session cookie.
pub async fn logout(session: Session) -> Result<StatusCode, AppError> {
    session
        .delete()
        .await
        .map_err(|e| AppError::Session(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct MeResponse {
    pub id: i64,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub is_owner: bool,
}

/// GET /api/me — returns the current user, or 401.
pub async fn me(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<MeResponse>, AppError> {
    let user = current_user(&session, &state)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let is_owner = is_owner_login(&state, &user.login);
    Ok(Json(MeResponse {
        id: user.id,
        login: user.login,
        name: user.name,
        avatar_url: user.avatar_url,
        is_owner,
    }))
}
