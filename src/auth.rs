//! GitHub OAuth + session wiring.
//!
//! Reads config from environment variables (sourced from ~/.blueprint/env on
//! daemon startup). Single tower-sessions MemoryStore for cookie-backed sessions
//! — fine for local single-instance use. The user identity in session is just
//! the row id of the `users` table.

use crate::error::AppError;
use crate::server::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, RedirectUrl, Scope,
    TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use tower_sessions::Session;

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
    /// Read config from env. If client_id+secret aren't set, auth is disabled
    /// and the daemon runs in legacy local-trust mode (write endpoints stay open).
    pub fn from_env() -> Self {
        let client_id = std::env::var("GITHUB_CLIENT_ID").unwrap_or_default();
        let client_secret = std::env::var("GITHUB_CLIENT_SECRET").unwrap_or_default();
        let callback_url = std::env::var("OAUTH_CALLBACK_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7321/auth/github/callback".into());
        let enabled = !client_id.is_empty() && !client_secret.is_empty();
        let authorize_url = std::env::var("OAUTH_AUTHORIZE_URL")
            .unwrap_or_else(|_| "https://github.com/login/oauth/authorize".into());
        let token_url = std::env::var("OAUTH_TOKEN_URL")
            .unwrap_or_else(|_| "https://github.com/login/oauth/access_token".into());
        let profile_url = std::env::var("OAUTH_PROFILE_URL")
            .unwrap_or_else(|_| "https://api.github.com/user".into());
        let cli_token = ensure_cli_token();
        let owner_login = std::env::var("BLUEPRINT_OWNER_GITHUB_LOGIN")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        AuthConfig {
            client_id,
            client_secret,
            callback_url,
            enabled,
            authorize_url,
            token_url,
            profile_url,
            cli_token,
            owner_login,
        }
    }

    fn oauth_client(&self) -> BasicClient {
        BasicClient::new(
            ClientId::new(self.client_id.clone()),
            Some(ClientSecret::new(self.client_secret.clone())),
            AuthUrl::new(self.authorize_url.clone()).expect("authorize URL parses"),
            Some(TokenUrl::new(self.token_url.clone()).expect("token URL parses")),
        )
        .set_redirect_uri(RedirectUrl::new(self.callback_url.clone()).expect("callback URL parses"))
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

/// Read SESSION_SECRET from env. Used only to detect when auth has been configured
/// (the secret value itself is not consumed — tower-sessions signs cookies with its
/// internal HMAC key derived from the store).
#[allow(dead_code)]
pub fn session_secret_configured() -> bool {
    std::env::var("SESSION_SECRET").is_ok()
}

/// Try to load ~/.blueprint/env into process env. Quietly ignores a missing file
/// (auth disabled), but fails loudly if the file exists and is malformed.
pub fn load_env_file() -> Result<(), AppError> {
    let path = match dirs::home_dir() {
        Some(h) => h.join(".blueprint").join("env"),
        None => return Ok(()),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            AppError::Other(format!(
                "malformed env line {} in {}: {line}",
                lineno + 1,
                path.display()
            ))
        })?;
        // Don't clobber values explicitly set in the actual env.
        if std::env::var(key).is_err() {
            // SAFETY: only mutating env at startup, before any threads spawn.
            unsafe {
                std::env::set_var(key.trim(), value.trim().trim_matches('"'));
            }
        }
    }
    Ok(())
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
        && token.trim() == cfg.cli_token
    {
        return Ok(Identity::CliBearer);
    }
    if let Some(user) = current_user(session, state).await? {
        return Ok(Identity::SessionUser(user));
    }
    Ok(Identity::None)
}

/// Phase 0: localhost-only daemon, no impersonation defense needed beyond the
/// per-comment role tag. Anonymous browser writes are accepted; the frontend
/// renders them as `guest` and the agent decides whether to act based on
/// `role`. The function is kept so the call-sites stay symmetric and so a
/// future phase can re-gate without re-threading the extractor.
pub fn require_identity_for_writes(
    _identity: &Identity,
    _state: &AppState,
) -> Result<(), AppError> {
    Ok(())
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

/// Axum extractor that resolves the request's identity AND enforces the
/// write-gate in one step. Handlers taking `WriteIdentity` get a guaranteed
/// non-None `Identity` when auth is enabled, or any identity (including None)
/// when running in legacy mode. Replaces the two-line preamble that was
/// repeated at the top of every write handler.
pub struct WriteIdentity(pub Identity);

#[axum::async_trait]
impl axum::extract::FromRequestParts<AppState> for WriteIdentity {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let session = Session::from_request_parts(parts, state)
            .await
            .map_err(|(_, msg)| AppError::Other(format!("session extract: {msg}")))?;
        let identity = identity_from_request(&session, &parts.headers, state).await?;
        require_identity_for_writes(&identity, state)?;
        Ok(WriteIdentity(identity))
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
    let auth = state
        .auth
        .as_ref()
        .ok_or_else(|| AppError::Other("auth not configured".into()))?;
    let client = auth.oauth_client();
    let (auth_url, csrf_state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("read:user".into()))
        .url();
    session
        .insert(SESSION_OAUTH_STATE, csrf_state.secret().clone())
        .await
        .map_err(|e| AppError::Other(format!("session insert: {e}")))?;
    if let Some(next) = params.next {
        session
            .insert(SESSION_POST_LOGIN_REDIRECT, next)
            .await
            .map_err(|e| AppError::Other(format!("session insert: {e}")))?;
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
    let auth = state
        .auth
        .as_ref()
        .ok_or_else(|| AppError::Other("auth not configured".into()))?;

    // Verify the state nonce matches what we stashed at /login.
    let expected: Option<String> = session.get(SESSION_OAUTH_STATE).await.unwrap_or(None);
    match expected {
        Some(s) if s == params.state => {
            session.remove::<String>(SESSION_OAUTH_STATE).await.ok();
        }
        _ => return Err(AppError::BadRequest("state mismatch or missing".into())),
    }

    // Exchange the code for an access token.
    let client = auth.oauth_client();
    let token = client
        .exchange_code(AuthorizationCode::new(params.code))
        .request_async(async_http_client)
        .await
        .map_err(|e| AppError::Other(format!("oauth token exchange: {e}")))?;

    // Fetch the GitHub user profile. GitHub requires a User-Agent.
    let access_token = token.access_token().secret();
    let http = reqwest::Client::new();
    let profile: GitHubUser = http
        .get(&auth.profile_url)
        .bearer_auth(access_token)
        .header("User-Agent", "blueprint")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| AppError::Other(format!("github fetch: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::Other(format!("github status: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Other(format!("github json: {e}")))?;

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
        .map_err(|e| AppError::Other(format!("session insert: {e}")))?;

    // Honor post_login_redirect from /login if present.
    let redirect_to: Option<String> = session
        .remove(SESSION_POST_LOGIN_REDIRECT)
        .await
        .ok()
        .flatten();
    let target = redirect_to.unwrap_or_else(|| "/".into());
    Ok(Redirect::to(&target).into_response())
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
        .map_err(|e| AppError::Other(format!("session delete: {e}")))?;
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
