use crate::error::AppError;
use crate::selector::TextQuoteSelector;
use crate::slug;
use crate::store::{BlueprintSummary, Comment, CommentDraft, Store};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, broadcast};

/// 5-minute TTL on a slug-level batch-processing entry. If the skill crashes
/// between `start` and the last reply (and so never DELETE'd), the entry is
/// lazily evicted on the next read. Mirrors the per-comment processing TTL on
/// the frontend (`PROCESSING_TTL_MS` in `frontend/app.js`).
const BATCH_PROCESSING_TTL_MS: i64 = 5 * 60 * 1000;

#[derive(RustEmbed)]
#[folder = "frontend/"]
struct Assets;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub events: broadcast::Sender<Event>,
    pub finish_signals: Arc<Mutex<HashMap<String, broadcast::Sender<()>>>>,
    pub blueprint_versions: Arc<Mutex<HashMap<String, u64>>>,
    /// Per-slug "the agent is working on this batch" state, in-memory only.
    /// Set by `POST /api/blueprints/:slug/batch-processing` when the skill
    /// wakes on a Submit-all batch, cleared either explicitly via DELETE or
    /// implicitly when the last comment in `pending_parents` receives a reply.
    /// See `BatchProcessing` below.
    pub batch_processing: Arc<Mutex<HashMap<String, BatchProcessing>>>,
    /// None when auth env vars aren't set (legacy local-trust mode).
    pub auth: Option<Arc<crate::auth::AuthConfig>>,
    /// Notified by `POST /api/shutdown-if-empty` when there are no blueprints left.
    /// `daemon::run_foreground` selects on this alongside ctrl-c/SIGTERM so
    /// the HTTP-triggered shutdown reuses axum's graceful-shutdown path.
    pub shutdown: Arc<Notify>,
}

/// Slug-level "Claude is working on N comments" state. Lives only in memory
/// on `AppState`; surviving a daemon restart isn't worth the schema cost —
/// worst case the indicator reappears stale for at most `BATCH_PROCESSING_TTL_MS`.
#[derive(Clone, Debug, Serialize)]
pub struct BatchProcessing {
    pub author: String,
    /// Original batch size, displayed verbatim by the sidebar pill. Stays
    /// constant for the lifetime of the entry — the goal of the indicator is
    /// "is Claude working" not a per-reply progress meter, so a stable label
    /// reads more cleanly than one that ticks down.
    pub count: u32,
    pub started_at: i64,
    /// Set of parent comment IDs that haven't yet received a reply. Removed
    /// from on each matching reply-insert; when empty, the entry is cleared
    /// and a `BatchProcessingChanged` event is broadcast (auto-clear path).
    /// Skipped in serialization — the wire payload only needs `author`,
    /// `count`, `started_at`.
    #[serde(skip)]
    pub pending_parents: HashSet<String>,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub slug: String,
    pub kind: EventKind,
}

#[derive(Clone, Debug)]
pub enum EventKind {
    CommentAdded(String),
    /// All comments from a single Submit-all batch landed atomically. Carries
    /// every new comment id so subscribers can decide to batch their reactions
    /// (e.g. the blueprint skill makes one HTML edit pass for the whole batch
    /// instead of one per comment).
    CommentBatchAdded(Vec<String>),
    BlueprintUpdated,
    BlueprintDeleted,
    /// The slug's batch-processing entry was set, cleared, or auto-cleared
    /// after the last pending reply. The actual payload is read off
    /// `AppState::batch_processing`; this is just the wake signal for
    /// long-pollers and the comment-stream.
    BatchProcessingChanged,
}

impl AppState {
    pub fn new(store: Arc<Store>) -> Self {
        Self::with_auth(store, None)
    }

    pub fn with_auth(store: Arc<Store>, auth: Option<Arc<crate::auth::AuthConfig>>) -> Self {
        let (tx, _) = broadcast::channel(64);
        AppState {
            store,
            events: tx,
            finish_signals: Arc::new(Mutex::new(HashMap::new())),
            blueprint_versions: Arc::new(Mutex::new(HashMap::new())),
            batch_processing: Arc::new(Mutex::new(HashMap::new())),
            auth,
            shutdown: Arc::new(Notify::new()),
        }
    }

    /// Read the active batch-processing entry for a slug, applying the TTL.
    /// If the recorded `started_at` is older than `BATCH_PROCESSING_TTL_MS`,
    /// the entry is evicted on-the-fly and `None` is returned — saves a
    /// background sweep for what's a rare crash-recovery path.
    pub async fn current_batch_processing(&self, slug: &str) -> Option<BatchProcessing> {
        let mut m = self.batch_processing.lock().await;
        let entry = m.get(slug)?.clone();
        let age = crate::store::now_ms() - entry.started_at;
        if age > BATCH_PROCESSING_TTL_MS {
            m.remove(slug);
            return None;
        }
        Some(entry)
    }

    /// Single-reply version of `note_replies_in_batch`. Inline so the common
    /// `create_comment` / `create_reply` path doesn't need to build a slice.
    pub async fn note_reply_in_batch(&self, slug: &str, parent_id: &str) {
        self.note_replies_in_batch(slug, &[parent_id]).await;
    }

    /// Mark a set of comments in this slug's active batch (if any) as having
    /// received their replies. If the batch's `pending_parents` set empties
    /// as a result, the entry is cleared and a `BatchProcessingChanged` event
    /// broadcast so the next 1.5s sidebar poll hides the indicator. No-op if
    /// there's no active batch or none of the parents are in it. Takes the
    /// mutex once for the whole slice.
    pub async fn note_replies_in_batch(&self, slug: &str, parent_ids: &[&str]) {
        if parent_ids.is_empty() {
            return;
        }
        let cleared = {
            let mut m = self.batch_processing.lock().await;
            if let Some(entry) = m.get_mut(slug) {
                for pid in parent_ids {
                    entry.pending_parents.remove(*pid);
                }
                if entry.pending_parents.is_empty() {
                    m.remove(slug);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if cleared {
            let _ = self.events.send(Event {
                slug: slug.to_string(),
                kind: EventKind::BatchProcessingChanged,
            });
        }
    }

    pub async fn finish_sender(&self, slug: &str) -> broadcast::Sender<()> {
        let mut m = self.finish_signals.lock().await;
        m.entry(slug.to_string())
            .or_insert_with(|| broadcast::channel(8).0)
            .clone()
    }

    pub async fn bump_version(&self, slug: &str) -> u64 {
        let mut m = self.blueprint_versions.lock().await;
        let v = m.entry(slug.to_string()).or_insert(0);
        *v += 1;
        *v
    }

    pub async fn blueprint_version(&self, slug: &str) -> u64 {
        *self.blueprint_versions.lock().await.get(slug).unwrap_or(&0)
    }
}

pub fn router(state: AppState) -> Router {
    use tower_sessions::{
        Expiry, MemoryStore, SessionManagerLayer, cookie::SameSite, cookie::time::Duration,
    };
    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false) // localhost = http
        .with_same_site(SameSite::Lax)
        .with_name("ps_session")
        .with_expiry(Expiry::OnInactivity(Duration::days(7)));

    Router::new()
        .route("/", get(root))
        .route("/b/:slug", get(reviewer_page))
        .route("/api/health", get(health))
        .route("/api/blueprints", post(create_blueprint))
        .route("/api/blueprints", get(list_blueprints))
        .route("/api/shutdown-if-empty", post(shutdown_if_empty))
        .route(
            "/api/blueprints/:slug",
            delete(delete_blueprint).put(update_blueprint),
        )
        .route("/api/blueprints/:slug/raw", get(get_blueprint_raw))
        .route(
            "/api/blueprints/:slug/comments",
            get(list_comments).post(create_comment),
        )
        .route(
            "/api/blueprints/:slug/comments/batch",
            post(create_comments_batch),
        )
        .route(
            "/api/blueprints/:slug/comments/:id/replies",
            post(create_reply),
        )
        .route(
            "/api/blueprints/:slug/comments/:id/processing",
            post(set_processing),
        )
        .route(
            "/api/blueprints/:slug/comments/:id/resolve",
            post(set_resolved),
        )
        .route("/api/blueprints/:slug/finish", post(finish_review))
        .route("/api/blueprints/:slug/wait", get(wait_for_finish))
        .route("/api/blueprints/:slug/wait-comment", get(wait_for_comment))
        .route(
            "/api/blueprints/:slug/batch-processing",
            post(start_batch_processing).delete(end_batch_processing),
        )
        // Auth surface
        .route("/login", get(crate::auth::login))
        .route("/auth/github/callback", get(crate::auth::callback))
        .route("/logout", post(crate::auth::logout))
        .route("/api/me", get(crate::auth::me))
        .layer(session_layer)
        .route("/static/*path", get(static_asset))
        .with_state(state)
}

async fn root() -> Response {
    serve_asset("index.html").await
}

async fn reviewer_page(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    if state.store.get_blueprint(&slug).ok().flatten().is_none() {
        return (StatusCode::NOT_FOUND, "blueprint not found").into_response();
    }
    serve_asset("reviewer.html").await
}

async fn static_asset(Path(path): Path<String>) -> Response {
    serve_asset(&path).await
}

async fn serve_asset(name: &str) -> Response {
    match Assets::get(name) {
        Some(file) => {
            let mime = mime_guess::from_path(name).first_or_octet_stream();
            let mut resp = Response::new(file.data.into_owned().into());
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(mime.as_ref())
                    .unwrap_or(HeaderValue::from_static("application/octet-stream")),
            );
            // Frontend assets are embedded in the binary; they change whenever the binary is
            // rebuilt. Tell the browser never to cache so users always get the assets that
            // match the running daemon.
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        None => (StatusCode::NOT_FOUND, "asset not found").into_response(),
    }
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct CreateBlueprintInput {
    html: String,
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Serialize)]
struct CreateBlueprintOutput {
    slug: String,
    url: String,
    created_at: i64,
}

async fn create_blueprint(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    headers: HeaderMap,
    Json(input): Json<CreateBlueprintInput>,
) -> Result<Json<CreateBlueprintOutput>, AppError> {
    if input.html.is_empty() {
        return Err(AppError::BadRequest("html field is required".into()));
    }
    let slug = match input.slug {
        Some(s) if !s.is_empty() => s,
        _ => slug::random(),
    };
    let token = slug::delete_token();
    let client_cwd = headers
        .get("x-client-cwd")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let blueprint =
        state
            .store
            .insert_blueprint(&slug, input.html.as_bytes(), &token, client_cwd)?;
    state.bump_version(&blueprint.slug).await;
    Ok(Json(CreateBlueprintOutput {
        slug: blueprint.slug.clone(),
        url: format!("/b/{}", blueprint.slug),
        created_at: blueprint.created_at,
    }))
}

/// Shut the daemon down — but only if no blueprints remain. The store mutex
/// serializes this count against any concurrent `insert_blueprint`, so the
/// decision can't read stale state. If a publish lands *after* we signal,
/// axum's graceful shutdown still serves it before exiting, and the next
/// CLI call respawns the daemon. See `unpublish` in `cli.rs`.
async fn shutdown_if_empty(State(state): State<AppState>) -> Result<StatusCode, AppError> {
    let count = state.store.count_blueprints()?;
    if count == 0 {
        state.shutdown.notify_one();
        Ok(StatusCode::NO_CONTENT)
    } else {
        Ok(StatusCode::CONFLICT)
    }
}

async fn list_blueprints(
    State(state): State<AppState>,
) -> Result<Json<Vec<BlueprintSummary>>, AppError> {
    let blueprints = state.store.list_blueprint_summaries()?;
    Ok(Json(blueprints))
}

#[derive(Deserialize)]
struct UpdateBlueprintInput {
    html: String,
}

async fn update_blueprint(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    Path(slug): Path<String>,
    Json(input): Json<UpdateBlueprintInput>,
) -> Result<StatusCode, AppError> {
    state
        .store
        .update_blueprint_html(&slug, input.html.as_bytes())?;
    state.bump_version(&slug).await;
    let _ = state.events.send(Event {
        slug: slug.clone(),
        kind: EventKind::BlueprintUpdated,
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn get_blueprint_raw(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    match state.store.get_blueprint_html(&slug) {
        Ok(Some(bytes)) => {
            let mut resp = Response::new(bytes.into());
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Ok(None) => (StatusCode::NOT_FOUND, "blueprint not found").into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_blueprint(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    Path(slug): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = state.store.delete_blueprint(&slug)?;
    if !removed {
        return Err(AppError::NotFound);
    }
    let _ = state.events.send(Event {
        slug: slug.clone(),
        kind: EventKind::BlueprintDeleted,
    });
    let sender = state.finish_sender(&slug).await;
    let _ = sender.send(());
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CommentQuery {
    #[serde(default)]
    since: Option<i64>,
}

async fn list_comments(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(q): Query<CommentQuery>,
) -> Result<Json<CommentsResponse>, AppError> {
    if state.store.get_blueprint(&slug)?.is_none() {
        return Err(AppError::NotFound);
    }
    let comments = state.store.list_comments(&slug, q.since)?;
    Ok(Json(build_comments_response(&state, &slug, comments).await))
}

/// Stamp comments with the trio of slug-scoped facts that every comments-list
/// response carries: `server_ts`, `blueprint_version`, and `batch_processing`.
/// Used by `list_comments` and by every return-arm of `wait_for_comment`.
async fn build_comments_response(
    state: &AppState,
    slug: &str,
    comments: Vec<Comment>,
) -> CommentsResponse {
    let server_ts = crate::store::now_ms();
    let blueprint_version = state.blueprint_version(slug).await;
    let batch_processing = state.current_batch_processing(slug).await;
    CommentsResponse {
        comments,
        server_ts,
        blueprint_version,
        batch_processing,
    }
}

#[derive(Serialize)]
struct CommentsResponse {
    comments: Vec<Comment>,
    server_ts: i64,
    blueprint_version: u64,
    /// `Some(_)` while a Submit-all batch is being worked on by the agent;
    /// `None` otherwise. Drives the slug-level "Claude is working on N
    /// comments" pill in the sidebar. Field is `skip_serializing_if = None`
    /// so older clients that don't know about it see the same shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_processing: Option<BatchProcessing>,
}

#[derive(Deserialize)]
struct CreateCommentInput {
    author: String,
    body: String,
    selector: TextQuoteSelector,
    #[serde(default)]
    parent_id: Option<String>,
}

async fn create_comment(
    State(state): State<AppState>,
    crate::auth::WriteIdentity(identity): crate::auth::WriteIdentity,
    Path(slug): Path<String>,
    Json(input): Json<CreateCommentInput>,
) -> Result<Json<Comment>, AppError> {
    if state.store.get_blueprint(&slug)?.is_none() {
        return Err(AppError::NotFound);
    }
    if input.body.trim().is_empty() {
        return Err(AppError::BadRequest("body is required".into()));
    }
    if let Some(pid) = &input.parent_id
        && state.store.find_comment(&slug, pid)?.is_none()
    {
        return Err(AppError::BadRequest("parent comment not found".into()));
    }
    let (author, author_user_id, author_avatar_url) = match &identity {
        crate::auth::Identity::SessionUser(u) => {
            (u.login.clone(), Some(u.id), u.avatar_url.clone())
        }
        _ => (sanitize_author(&input.author), None, None),
    };
    let role = crate::auth::role_for(&identity, &state);
    let is_agent = crate::auth::is_agent(&identity);
    let id = slug::comment_id();
    let comment = state.store.add_comment(
        &slug,
        &id,
        &author,
        &input.body,
        &input.selector,
        input.parent_id.as_deref(),
        author_user_id,
        author_avatar_url,
        role,
        is_agent,
    )?;
    if let Some(pid) = &input.parent_id {
        state.note_reply_in_batch(&slug, pid).await;
    }
    let _ = state.events.send(Event {
        slug: slug.clone(),
        kind: EventKind::CommentAdded(id),
    });
    Ok(Json(comment))
}

/// Normalize an unauthenticated author string. Empty / whitespace → `"anonymous"`
/// so the wire shape is never blank. Server-side mirror of the frontend default
/// at `frontend/app.js` — defense in depth for guests submitting without filling
/// the legacy author input.
fn sanitize_author(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "anonymous".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Deserialize)]
struct BatchCommentInput {
    author: String,
    body: String,
    selector: TextQuoteSelector,
    #[serde(default)]
    parent_id: Option<String>,
}

#[derive(Serialize)]
struct BatchCreateOutput {
    comments: Vec<Comment>,
}

/// Atomically create N comments from a single "Submit all" click in the
/// reviewer UI. One transaction, one broadcast event — so an agent watching
/// `/wait-comment` wakes once for the whole batch instead of N times.
///
/// Bodies are trimmed and checked; if any one is empty, the whole batch
/// 400s without persisting anything. Author/avatar identity is taken from
/// the session if present, mirroring `create_comment` so impersonation
/// isn't possible via the batch route either.
async fn create_comments_batch(
    State(state): State<AppState>,
    crate::auth::WriteIdentity(identity): crate::auth::WriteIdentity,
    Path(slug): Path<String>,
    Json(inputs): Json<Vec<BatchCommentInput>>,
) -> Result<Json<BatchCreateOutput>, AppError> {
    if state.store.get_blueprint(&slug)?.is_none() {
        return Err(AppError::NotFound);
    }
    if inputs.is_empty() {
        return Err(AppError::BadRequest(
            "batch must contain at least one comment".into(),
        ));
    }
    for (i, input) in inputs.iter().enumerate() {
        if input.body.trim().is_empty() {
            return Err(AppError::BadRequest(format!(
                "comment {i}: body is required"
            )));
        }
    }

    let (session_author, session_user_id, session_avatar) = match &identity {
        crate::auth::Identity::SessionUser(u) => {
            (Some(u.login.clone()), Some(u.id), u.avatar_url.clone())
        }
        _ => (None, None, None),
    };
    let role = crate::auth::role_for(&identity, &state);
    let is_agent = crate::auth::is_agent(&identity);

    let drafts: Vec<CommentDraft> = inputs
        .into_iter()
        .map(|input| {
            let author = session_author
                .clone()
                .unwrap_or_else(|| sanitize_author(&input.author));
            CommentDraft {
                id: slug::comment_id(),
                author,
                body: input.body,
                selector: input.selector,
                parent_id: input.parent_id,
                author_user_id: session_user_id,
                author_avatar_url: session_avatar.clone(),
                role,
                is_agent,
            }
        })
        .collect();

    let comments = state.store.add_comments_batch(&slug, &drafts)?;
    // Any drafts in this batch that ARE replies (parent_id set) may settle
    // pending entries on this slug's active batch-processing record. The user-
    // facing Submit-all flow doesn't mix replies into a batch — but the
    // /batch endpoint is generic, so still call through for correctness.
    // One acquisition for the whole batch instead of N.
    let parent_ids: Vec<&str> = comments
        .iter()
        .filter_map(|c| c.parent_id.as_deref())
        .collect();
    if !parent_ids.is_empty() {
        state.note_replies_in_batch(&slug, &parent_ids).await;
    }
    let ids: Vec<String> = comments.iter().map(|c| c.id.clone()).collect();
    let _ = state.events.send(Event {
        slug: slug.clone(),
        kind: EventKind::CommentBatchAdded(ids),
    });
    Ok(Json(BatchCreateOutput { comments }))
}

#[derive(Deserialize)]
struct CreateReplyInput {
    author: String,
    body: String,
}

async fn create_reply(
    State(state): State<AppState>,
    crate::auth::WriteIdentity(identity): crate::auth::WriteIdentity,
    Path((slug, parent_id)): Path<(String, String)>,
    Json(input): Json<CreateReplyInput>,
) -> Result<Json<Comment>, AppError> {
    let parent = state
        .store
        .find_comment(&slug, &parent_id)?
        .ok_or(AppError::NotFound)?;
    let (author, author_user_id, author_avatar_url) = match &identity {
        crate::auth::Identity::SessionUser(u) => {
            (u.login.clone(), Some(u.id), u.avatar_url.clone())
        }
        _ => (sanitize_author(&input.author), None, None),
    };
    let role = crate::auth::role_for(&identity, &state);
    let is_agent = crate::auth::is_agent(&identity);
    let id = slug::comment_id();
    let comment = state.store.add_comment(
        &slug,
        &id,
        &author,
        &input.body,
        &parent.selector,
        Some(&parent_id),
        author_user_id,
        author_avatar_url,
        role,
        is_agent,
    )?;
    state.note_reply_in_batch(&slug, &parent_id).await;
    let _ = state.events.send(Event {
        slug: slug.clone(),
        kind: EventKind::CommentAdded(id),
    });
    Ok(Json(comment))
}

async fn finish_review(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    Path(slug): Path<String>,
) -> Result<StatusCode, AppError> {
    if state.store.get_blueprint(&slug)?.is_none() {
        return Err(AppError::NotFound);
    }
    let sender = state.finish_sender(&slug).await;
    let _ = sender.send(());
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetProcessingInput {
    author: String,
}

/// Mark a comment as "an agent is currently working on a reply." Auto-clears when a
/// reply is posted to this comment, or when it ages past 5 min (frontend-enforced).
async fn set_processing(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    Path((slug, id)): Path<(String, String)>,
    Json(input): Json<SetProcessingInput>,
) -> Result<StatusCode, AppError> {
    if input.author.trim().is_empty() {
        return Err(AppError::BadRequest("author is required".into()));
    }
    let updated = state.store.set_processing(&slug, &id, &input.author)?;
    if !updated {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetResolvedInput {
    resolved: bool,
}

async fn set_resolved(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    Path((slug, id)): Path<(String, String)>,
    Json(input): Json<SetResolvedInput>,
) -> Result<StatusCode, AppError> {
    let updated = state.store.set_resolved(&slug, &id, input.resolved)?;
    if !updated {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn wait_for_finish(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> Result<StatusCode, AppError> {
    if state.store.get_blueprint(&slug)?.is_none() {
        return Err(AppError::NotFound);
    }
    let sender = state.finish_sender(&slug).await;
    let mut rx = sender.subscribe();
    let _ = rx.recv().await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct StartBatchProcessingInput {
    author: String,
    /// Comment IDs the agent is about to work through. Their count drives
    /// the sidebar pill's "N comments" wording (displayed verbatim, never
    /// decremented). As replies land for each ID, the entry's pending set
    /// shrinks; when empty, the entry is auto-cleared without an explicit
    /// DELETE from the agent.
    parent_ids: Vec<String>,
}

/// `POST /api/blueprints/:slug/batch-processing` — stamp the slug-level
/// "Claude is working on N comments" state on `AppState`. Called by the
/// `/blueprint` skill at the top of its triage pass on a Submit-all batch.
async fn start_batch_processing(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    Path(slug): Path<String>,
    Json(input): Json<StartBatchProcessingInput>,
) -> Result<Json<BatchProcessing>, AppError> {
    if state.store.get_blueprint(&slug)?.is_none() {
        return Err(AppError::NotFound);
    }
    if input.author.trim().is_empty() {
        return Err(AppError::BadRequest("author is required".into()));
    }
    if input.parent_ids.is_empty() {
        return Err(AppError::BadRequest(
            "parent_ids must contain at least one comment id".into(),
        ));
    }
    let pending_parents: HashSet<String> = input.parent_ids.iter().cloned().collect();
    let entry = BatchProcessing {
        author: input.author.trim().to_string(),
        count: pending_parents.len() as u32,
        started_at: crate::store::now_ms(),
        pending_parents,
    };
    {
        let mut m = state.batch_processing.lock().await;
        m.insert(slug.clone(), entry.clone());
    }
    let _ = state.events.send(Event {
        slug: slug.clone(),
        kind: EventKind::BatchProcessingChanged,
    });
    Ok(Json(entry))
}

/// `DELETE /api/blueprints/:slug/batch-processing` — explicit clear. Mostly
/// belt-and-braces now that the auto-clear path handles the success case;
/// useful when the skill exits early (no edits AND no replies needed).
async fn end_batch_processing(
    State(state): State<AppState>,
    _w: crate::auth::WriteIdentity,
    Path(slug): Path<String>,
) -> Result<StatusCode, AppError> {
    let removed = {
        let mut m = state.batch_processing.lock().await;
        m.remove(&slug).is_some()
    };
    if removed {
        let _ = state.events.send(Event {
            slug: slug.clone(),
            kind: EventKind::BatchProcessingChanged,
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Long-poll endpoint that blocks until a new comment arrives on this blueprint.
/// Returns immediately if comments exist after `since`. Returns empty after ~30s
/// so clients can reconnect cleanly. Subscribes to the event stream BEFORE the
/// "anything already there?" check to avoid race-condition drops.
async fn wait_for_comment(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(q): Query<CommentQuery>,
) -> Result<Json<CommentsResponse>, AppError> {
    if state.store.get_blueprint(&slug)?.is_none() {
        return Err(AppError::NotFound);
    }

    let since = q.since.unwrap_or(0);
    let mut rx = state.events.subscribe();

    // Fast path: comments since `since` already exist.
    let existing = state.store.list_comments(&slug, Some(since))?;
    if !existing.is_empty() {
        return Ok(Json(build_comments_response(&state, &slug, existing).await));
    }

    // Slow path: wait for the next CommentAdded event for this slug.
    let timeout = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            _ = &mut timeout => {
                return Ok(Json(build_comments_response(&state, &slug, vec![]).await));
            }
            ev = rx.recv() => {
                match ev {
                    Ok(event) if event.slug == slug => {
                        match event.kind {
                            EventKind::CommentAdded(_) | EventKind::CommentBatchAdded(_) => {
                                let comments = state.store.list_comments(&slug, Some(since))?;
                                if !comments.is_empty() {
                                    return Ok(Json(build_comments_response(&state, &slug, comments).await));
                                }
                            }
                            EventKind::BlueprintDeleted => return Err(AppError::NotFound),
                            EventKind::BlueprintUpdated => {}
                            EventKind::BatchProcessingChanged => {}
                        }
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return Ok(Json(build_comments_response(&state, &slug, vec![]).await));
                    }
                }
            }
        }
    }
}
