use crate::error::AppError;
use crate::selector::TextQuoteSelector;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blueprint {
    pub slug: String,
    pub created_at: i64,
    #[serde(skip_serializing)]
    pub delete_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlueprintSummary {
    pub slug: String,
    pub created_at: i64,
    pub comment_count: u32,
    pub unresolved_count: u32,
    pub last_activity_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_cwd: Option<String>,
}

/// Provenance tag attached to every comment. Determined server-side at write
/// time from the request's `Identity`, then read by the agent + the frontend
/// to decide rendering and whether a comment should trip a plan edit.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthorRole {
    /// Logged-in session whose login matches `BLUEPRINT_OWNER_GITHUB_LOGIN`.
    /// Only role that should trip an HTML edit in the skill.
    Owner,
    /// Logged-in session that isn't the owner. Includes the CLI bearer path
    /// (agent traffic) — Claude posts as `user` and the orthogonal `is_agent`
    /// flag distinguishes its replies in the UI.
    User,
    /// No session, no bearer. Anonymous browser commenter.
    Guest,
}

impl AuthorRole {
    fn as_str(self) -> &'static str {
        match self {
            AuthorRole::Owner => "owner",
            AuthorRole::User => "user",
            AuthorRole::Guest => "guest",
        }
    }
}

/// Read/write the role as SQLite text, so call sites say `r.get(n)?` and
/// `params![role]` instead of hand-marshalling through `&str` at four places.
///
/// The unknown-value arm is a `warn!` rather than silence: this column decides
/// whether a comment trips a plan edit, so a value we don't recognise is
/// *reducing privilege*, and doing that invisibly is how you lose an afternoon.
impl rusqlite::types::FromSql for AuthorRole {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match v.as_str()? {
            "owner" => Ok(AuthorRole::Owner),
            "user" => Ok(AuthorRole::User),
            "guest" => Ok(AuthorRole::Guest),
            other => {
                tracing::warn!(role = other, "unknown author role in database; using guest");
                Ok(AuthorRole::Guest)
            }
        }
    }
}

impl rusqlite::ToSql for AuthorRole {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: String,
    pub slug: String,
    pub author: String,
    pub body: String,
    pub selector: TextQuoteSelector,
    pub parent_id: Option<String>,
    pub resolved: bool,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub processing_started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_user_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_avatar_url: Option<String>,
    pub role: AuthorRole,
    pub is_agent: bool,
    /// Blueprint version this comment was authored against. `None` for legacy
    /// comments written before version history existed. Read by the frontend
    /// to badge comments made on a superseded version and to fetch the
    /// historical snapshot for re-anchoring.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blueprint_version: Option<i64>,
}

/// Input shape for `Store::add_comment_draft` and `Store::add_comments_batch`.
/// Groups the per-comment fields so a single insert names them and a batch can
/// pass a slice, instead of threading four consecutive `&str`s positionally.
#[derive(Debug, Clone)]
pub struct CommentDraft {
    pub id: String,
    pub author: String,
    pub body: String,
    pub selector: TextQuoteSelector,
    pub parent_id: Option<String>,
    pub author_user_id: Option<i64>,
    pub author_avatar_url: Option<String>,
    pub role: AuthorRole,
    pub is_agent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub github_id: i64,
    pub login: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn = Connection::open(path)?;
        // rusqlite's default busy timeout is zero, so a SQLITE_BUSY fails on the
        // spot instead of waiting. The WAL sidecars are shared across processes
        // and the daemon respawns routinely, so two daemons overlapping during a
        // restart is expected — give the loser a chance to wait rather than 500.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Owner-only, before WAL exists. A tower-sessions id is a bearer
        // credential — there's no signing and no server-side secret, so
        // whoever can read a session row can impersonate that user. The
        // database therefore needs the same 0600 that `ensure_cli_token`
        // already gives the CLI token. Done before `journal_mode = WAL` so
        // the `-wal` and `-shm` files, which carry the same rows, inherit it.
        restrict_to_owner(path)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;",
        )?;
        migrate(&mut conn)?;

        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    /// Upsert a GitHub user. Refreshes name + avatar on every login.
    /// Returns the user's row id.
    pub fn upsert_user(
        &self,
        github_id: i64,
        login: &str,
        name: Option<&str>,
        avatar_url: Option<&str>,
    ) -> Result<i64, AppError> {
        let now = now_ms();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO users (github_id, login, name, avatar_url, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(github_id) DO UPDATE SET
               login = excluded.login,
               name = excluded.name,
               avatar_url = excluded.avatar_url,
               updated_at = excluded.updated_at",
            params![github_id, login, name, avatar_url, now],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM users WHERE github_id = ?1",
            params![github_id],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn get_user(&self, id: i64) -> Result<Option<User>, AppError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT id, github_id, login, name, avatar_url FROM users WHERE id = ?1",
                params![id],
                |r| {
                    Ok(User {
                        id: r.get(0)?,
                        github_id: r.get(1)?,
                        login: r.get(2)?,
                        name: r.get(3)?,
                        avatar_url: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    // -----------------------------------------------------------------------
    // Session records. The serialized-record shape belongs to tower-sessions;
    // these are deliberately dumb key/value accessors so `crate::session_store`
    // owns all of the encoding. `expires_at` is a wall-clock ms deadline.
    // -----------------------------------------------------------------------

    /// Insert a brand-new session. Returns `false` if `id` was already taken,
    /// which is the caller's signal to re-roll the id rather than clobber a
    /// live session belonging to someone else.
    pub fn insert_session(
        &self,
        id: &str,
        record: &str,
        expires_at: i64,
    ) -> Result<bool, AppError> {
        let conn = self.conn.lock();
        let rows = conn.execute(
            "INSERT OR IGNORE INTO sessions (id, record, expires_at) VALUES (?1, ?2, ?3)",
            params![id, record, expires_at],
        )?;
        Ok(rows == 1)
    }

    /// Write a session, creating it if absent. Used for updates to a session
    /// that already exists (a fresh one goes through `insert_session`).
    pub fn save_session(&self, id: &str, record: &str, expires_at: i64) -> Result<(), AppError> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sessions (id, record, expires_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
               record = excluded.record,
               expires_at = excluded.expires_at",
            params![id, record, expires_at],
        )?;
        Ok(())
    }

    /// Load a session's serialized record, treating an expired row as absent.
    pub fn load_session(&self, id: &str) -> Result<Option<String>, AppError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT record FROM sessions WHERE id = ?1 AND expires_at > ?2",
                params![id, now_ms()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row)
    }

    pub fn delete_session(&self, id: &str) -> Result<(), AppError> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Drop every session past its expiry. Called once at daemon startup —
    /// expired rows are already invisible to `load_session`, so this is only
    /// housekeeping to keep the table from growing without bound.
    pub fn delete_expired_sessions(&self) -> Result<usize, AppError> {
        let conn = self.conn.lock();
        let rows = conn.execute(
            "DELETE FROM sessions WHERE expires_at <= ?1",
            params![now_ms()],
        )?;
        Ok(rows)
    }

    pub fn insert_blueprint(
        &self,
        slug: &str,
        html: &[u8],
        delete_token: &str,
        client_cwd: Option<&str>,
    ) -> Result<Blueprint, AppError> {
        let now = now_ms();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO blueprints (slug, html, created_at, delete_token, client_cwd) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![slug, html, now, delete_token, client_cwd],
        )?;
        Ok(Blueprint {
            slug: slug.into(),
            created_at: now,
            delete_token: delete_token.into(),
            client_cwd: client_cwd.map(|s| s.to_string()),
        })
    }

    /// Total number of blueprints. Used by `POST /api/shutdown-if-empty` to
    /// decide whether the daemon can stop. Holds the store mutex for the
    /// duration of the read, so any concurrent `insert_blueprint` serializes
    /// against it.
    pub fn count_blueprints(&self) -> Result<i64, AppError> {
        let conn = self.conn.lock();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM blueprints", [], |r| r.get(0))?;
        Ok(n)
    }

    /// Replace a blueprint's HTML, archiving the prior version first, and
    /// return the new (bumped) version. The snapshot-then-bump-then-replace
    /// runs in one transaction so a crash can't strand a half-archived state:
    /// either the old version is preserved and the new one is live, or nothing
    /// changed. Comments authored against the archived version keep resolving
    /// against the exact text they anchored to via `get_blueprint_html_at`.
    pub fn update_blueprint_html(&self, slug: &str, html: &[u8]) -> Result<u64, AppError> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        // Snapshot the CURRENT html at its CURRENT version into the archive.
        // No-op INSERT if the slug doesn't exist (the UPDATE below reports it).
        tx.execute(
            "INSERT OR IGNORE INTO blueprint_versions (slug, version, html, created_at)
             SELECT slug, version, html, ?2 FROM blueprints WHERE slug = ?1",
            params![slug, now_ms()],
        )?;
        // Bump + replace the live row.
        let rows = tx.execute(
            "UPDATE blueprints SET html = ?1, version = version + 1 WHERE slug = ?2",
            params![html, slug],
        )?;
        if rows == 0 {
            return Err(AppError::NotFound);
        }
        let new_version: i64 = tx.query_row(
            "SELECT version FROM blueprints WHERE slug = ?1",
            params![slug],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(new_version as u64)
    }

    /// Current live version of a blueprint, or `None` if the slug is unknown.
    pub fn get_blueprint_version(&self, slug: &str) -> Result<Option<u64>, AppError> {
        let conn = self.conn.lock();
        let v = conn
            .query_row(
                "SELECT version FROM blueprints WHERE slug = ?1",
                params![slug],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(v.map(|n| n as u64))
    }

    /// Stamp "the reviewer clicked Finish Review" and raise the pending latch.
    /// Returns the timestamp written. `Err(NotFound)` for an unknown slug.
    /// Re-finishing is allowed and simply re-raises the latch, so a reviewer who
    /// keeps going after finishing can end a later round too.
    ///
    /// Both columns get the same `now`: `finished_at` is the durable record the
    /// reviewer header reads and is never cleared, while `finish_pending_at` is
    /// the latch a `watch` consumes.
    pub fn mark_finished(&self, slug: &str) -> Result<i64, AppError> {
        let now = now_ms();
        let conn = self.conn.lock();
        let rows = conn.execute(
            "UPDATE blueprints SET finished_at = ?1, finish_pending_at = ?1 WHERE slug = ?2",
            params![now, slug],
        )?;
        if rows == 0 {
            return Err(AppError::NotFound);
        }
        Ok(now)
    }

    /// Claim a pending finish, if there is one: returns `Some(finished_at)` and
    /// lowers the latch, or `None` if no finish is waiting. `Err(NotFound)` for
    /// an unknown slug.
    ///
    /// One statement does the whole job, so there's no follow-up SELECT that
    /// could observe a concurrently-deleted row, and nothing to fabricate when
    /// the latch is up but the stamp is missing — a single nullable column
    /// cannot be in that state.
    ///
    /// It returns `finished_at` rather than `finish_pending_at` because SQLite's
    /// `RETURNING` reports values *after* the UPDATE, and this statement just
    /// set `finish_pending_at` to NULL — asking for it back yields NULL. The two
    /// columns are written together by `mark_finished` with the same `now`, so
    /// the unmodified `finished_at` is that same claimed timestamp.
    ///
    /// Being one conditional UPDATE is also what makes the claim exclusive:
    /// several waiters parked on a slug race on `finish_pending_at IS NOT NULL`
    /// and exactly one wins, and a later round correctly sees NULL and parks.
    pub fn claim_finish(&self, slug: &str) -> Result<Option<i64>, AppError> {
        let conn = self.conn.lock();
        let claimed: Option<i64> = conn
            .query_row(
                "UPDATE blueprints SET finish_pending_at = NULL
                 WHERE slug = ?1 AND finish_pending_at IS NOT NULL
                 RETURNING finished_at",
                params![slug],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(ts) = claimed {
            return Ok(Some(ts));
        }
        // No row matched, which is either "nothing pending" or "no such slug" —
        // the UPDATE can't tell them apart, so ask.
        if read_version(&conn, slug)?.is_some() {
            Ok(None)
        } else {
            Err(AppError::NotFound)
        }
    }

    /// When this blueprint was last finished, regardless of whether a waiter has
    /// claimed it. `Ok(None)` means never finished; `Err(NotFound)` means the
    /// slug doesn't exist. Drives the reviewer header's durable finished state.
    pub fn finished_at(&self, slug: &str) -> Result<Option<i64>, AppError> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT finished_at FROM blueprints WHERE slug = ?1",
            params![slug],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .ok_or(AppError::NotFound)
    }

    /// All version numbers for a blueprint (archived + the live one),
    /// ascending. `Err(NotFound)` for an unknown slug. Backs the reviewer's
    /// version dropdown.
    pub fn list_versions(&self, slug: &str) -> Result<(u64, Vec<u64>), AppError> {
        let conn = self.conn.lock();
        // `current` comes from the blueprints row, which is the thing that
        // *defines* it. Taking the max of the UNION instead would be correct
        // only as long as the archive never holds a version >= the live one —
        // an invariant nothing enforces, and whose violation would show the
        // reviewer's dropdown a version number that isn't the current one.
        // Doubles as the existence check: no row ⇒ unknown slug.
        let current = read_version(&conn, slug)?.ok_or(AppError::NotFound)? as u64;
        // The archive holds every superseded version and the live row holds the
        // current one. UNION dedups; ORDER BY sorts.
        let mut stmt = conn.prepare(
            "SELECT version FROM blueprint_versions WHERE slug = ?1
             UNION SELECT version FROM blueprints WHERE slug = ?1
             ORDER BY version ASC",
        )?;
        let versions = stmt
            .query_map(params![slug], |r| r.get::<_, i64>(0).map(|v| v as u64))?
            .collect::<rusqlite::Result<Vec<u64>>>()?;
        Ok((current, versions))
    }

    /// HTML for a specific version. Serves the live row when `version` matches
    /// the current version, otherwise the archived snapshot. `None` when the
    /// slug is unknown or that version was never recorded (or has been pruned).
    pub fn get_blueprint_html_at(
        &self,
        slug: &str,
        version: u64,
    ) -> Result<Option<Vec<u8>>, AppError> {
        let conn = self.conn.lock();
        // Live row first — the current version is not copied into the archive.
        let live: Option<(i64, Vec<u8>)> = conn
            .query_row(
                "SELECT version, html FROM blueprints WHERE slug = ?1",
                params![slug],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((cur, html)) = live
            && cur as u64 == version
        {
            return Ok(Some(html));
        }
        let archived = conn
            .query_row(
                "SELECT html FROM blueprint_versions WHERE slug = ?1 AND version = ?2",
                params![slug, version as i64],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(archived)
    }

    pub fn get_blueprint(&self, slug: &str) -> Result<Option<Blueprint>, AppError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT slug, created_at, delete_token, client_cwd FROM blueprints WHERE slug = ?1",
                params![slug],
                |r| {
                    Ok(Blueprint {
                        slug: r.get(0)?,
                        created_at: r.get(1)?,
                        delete_token: r.get(2)?,
                        client_cwd: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_blueprint_html(&self, slug: &str) -> Result<Option<Vec<u8>>, AppError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT html FROM blueprints WHERE slug = ?1",
                params![slug],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(row)
    }

    pub fn delete_blueprint(&self, slug: &str) -> Result<bool, AppError> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM blueprints WHERE slug = ?1", params![slug])?;
        Ok(n > 0)
    }

    pub fn list_blueprints(&self) -> Result<Vec<Blueprint>, AppError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT slug, created_at, delete_token, client_cwd FROM blueprints ORDER BY created_at DESC",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(Blueprint {
                    slug: r.get(0)?,
                    created_at: r.get(1)?,
                    delete_token: r.get(2)?,
                    client_cwd: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    pub fn list_blueprint_summaries(&self) -> Result<Vec<BlueprintSummary>, AppError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT \
                 b.slug, \
                 b.created_at, \
                 b.client_cwd, \
                 COUNT(c.id) AS comment_count, \
                 COALESCE(SUM(CASE WHEN c.resolved = 0 THEN 1 ELSE 0 END), 0) AS unresolved_count, \
                 COALESCE(MAX(c.created_at), b.created_at) AS last_activity_at \
             FROM blueprints b \
             LEFT JOIN comments c ON c.slug = b.slug \
             GROUP BY b.slug, b.created_at, b.client_cwd \
             ORDER BY b.created_at DESC",
        )?;
        let out = stmt
            .query_map([], |r| {
                Ok(BlueprintSummary {
                    slug: r.get(0)?,
                    created_at: r.get(1)?,
                    client_cwd: r.get(2)?,
                    comment_count: r.get::<_, i64>(3)? as u32,
                    unresolved_count: r.get::<_, i64>(4)? as u32,
                    last_activity_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Insert one comment. The real implementation — takes the fields grouped in
    /// a `CommentDraft` rather than as a run of four consecutive `&str`s, where
    /// swapping any two still compiles and simply writes the wrong thing.
    pub fn add_comment_draft(&self, slug: &str, draft: &CommentDraft) -> Result<Comment, AppError> {
        let now = now_ms();
        let sel_json = serde_json::to_string(&draft.selector)?;
        let conn = self.conn.lock();
        // The INSERT and the parent's processing clear go together or not at
        // all. The daemon is SIGTERM'd routinely (a missed health probe is
        // enough), and a kill landing between the two used to leave the reply
        // persisted with its parent still flagged "Claude is working on this"
        // — permanently, since nothing else clears that flag.
        let tx = conn.unchecked_transaction()?;
        // Stamp the version the comment is being written against, read inside
        // the transaction so it can't race an interleaved update.
        let blueprint_version = read_version(&tx, slug)?;
        tx.execute(
            "INSERT INTO comments (id, slug, author, body, selector, parent_id, resolved, created_at, author_user_id, role, is_agent, blueprint_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?11)",
            params![
                draft.id,
                slug,
                draft.author,
                draft.body,
                sel_json,
                draft.parent_id,
                now,
                draft.author_user_id,
                draft.role,
                draft.is_agent,
                blueprint_version,
            ],
        )?;
        // If this comment is a reply, clear processing state on its parent — the agent
        // that was working on it has now produced its response.
        if let Some(pid) = &draft.parent_id {
            tx.execute(
                "UPDATE comments SET processing_by = NULL, processing_started_at = NULL
                 WHERE slug = ?1 AND id = ?2",
                params![slug, pid],
            )?;
        }
        tx.commit()?;
        Ok(Comment {
            id: draft.id.clone(),
            slug: slug.into(),
            author: draft.author.clone(),
            body: draft.body.clone(),
            selector: draft.selector.clone(),
            parent_id: draft.parent_id.clone(),
            resolved: false,
            created_at: now,
            processing_by: None,
            processing_started_at: None,
            author_user_id: draft.author_user_id,
            author_avatar_url: draft.author_avatar_url.clone(),
            role: draft.role,
            is_agent: draft.is_agent,
            blueprint_version,
        })
    }

    /// Positional-argument wrapper over `add_comment_draft`, kept only so the
    /// existing call sites in `server.rs` keep compiling. Prefer
    /// `add_comment_draft`; this should go away once those are migrated.
    #[allow(clippy::too_many_arguments)]
    pub fn add_comment(
        &self,
        slug: &str,
        id: &str,
        author: &str,
        body: &str,
        selector: &TextQuoteSelector,
        parent_id: Option<&str>,
        author_user_id: Option<i64>,
        author_avatar_url: Option<String>,
        role: AuthorRole,
        is_agent: bool,
    ) -> Result<Comment, AppError> {
        self.add_comment_draft(
            slug,
            &CommentDraft {
                id: id.to_string(),
                author: author.to_string(),
                body: body.to_string(),
                selector: selector.clone(),
                parent_id: parent_id.map(|s| s.to_string()),
                author_user_id,
                author_avatar_url,
                role,
                is_agent,
            },
        )
    }

    /// Insert N comments atomically. Used by the batch-submit flow so one
    /// "Submit all" click in the reviewer UI lands as a single transaction
    /// and (via the server) fires a single `CommentBatchAdded` broadcast.
    ///
    /// Parents may live in the database OR be earlier entries in this same
    /// batch (a draft replying to another draft) — we validate by scanning
    /// the current batch first, then the DB, so the caller doesn't have to
    /// pre-order the inputs.
    pub fn add_comments_batch(
        &self,
        slug: &str,
        drafts: &[CommentDraft],
    ) -> Result<Vec<Comment>, AppError> {
        if drafts.is_empty() {
            return Ok(vec![]);
        }
        let now = now_ms();
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        // First pass: validate every parent_id either points at an existing DB
        // row OR an earlier draft in this batch. Reject the whole batch if
        // anything is bogus — partial inserts on a batch are worse than a clean
        // failure the client can correct and retry.
        //
        // "Earlier" is load-bearing and used not to be enforced: the seen-set was
        // built from *all* drafts up front, so a draft naming a parent that comes
        // later in the slice passed validation and then hit a raw FOREIGN KEY
        // violation on insert — a 500 where the client deserved a 400. Inserts
        // run in slice order and SQLite checks the constraint immediately, so
        // accumulating the set as we go is what makes the check match reality.
        let mut seen_ids: std::collections::HashSet<&str> =
            std::collections::HashSet::with_capacity(drafts.len());
        for d in drafts {
            if let Some(pid) = &d.parent_id
                && !seen_ids.contains(pid.as_str())
            {
                let exists: bool = tx
                    .query_row(
                        "SELECT 1 FROM comments WHERE slug = ?1 AND id = ?2",
                        params![slug, pid],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if !exists {
                    return Err(AppError::BadRequest(format!(
                        "parent comment {pid} not found"
                    )));
                }
            }
            seen_ids.insert(d.id.as_str());
        }

        // All comments in a Submit-all batch are stamped with the same version
        // — the one live when the batch landed. Read once under the tx.
        let blueprint_version = read_version(&tx, slug)?;

        let mut out = Vec::with_capacity(drafts.len());
        let mut parents_to_clear: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for d in drafts {
            let sel_json = serde_json::to_string(&d.selector)?;
            tx.execute(
                "INSERT INTO comments (id, slug, author, body, selector, parent_id, resolved, created_at, author_user_id, role, is_agent, blueprint_version)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?11)",
                params![
                    d.id,
                    slug,
                    d.author,
                    d.body,
                    sel_json,
                    d.parent_id,
                    now,
                    d.author_user_id,
                    d.role,
                    d.is_agent,
                    blueprint_version,
                ],
            )?;
            if let Some(pid) = &d.parent_id {
                parents_to_clear.insert(pid.clone());
            }
            out.push(Comment {
                id: d.id.clone(),
                slug: slug.into(),
                author: d.author.clone(),
                body: d.body.clone(),
                selector: d.selector.clone(),
                parent_id: d.parent_id.clone(),
                resolved: false,
                created_at: now,
                processing_by: None,
                processing_started_at: None,
                author_user_id: d.author_user_id,
                author_avatar_url: d.author_avatar_url.clone(),
                role: d.role,
                is_agent: d.is_agent,
                blueprint_version,
            });
        }
        // Mirror add_comment's behavior: any parent that just got a reply
        // has its processing flag cleared in the same transaction.
        for pid in &parents_to_clear {
            tx.execute(
                "UPDATE comments SET processing_by = NULL, processing_started_at = NULL
                 WHERE slug = ?1 AND id = ?2",
                params![slug, pid],
            )?;
        }
        tx.commit()?;
        Ok(out)
    }

    pub fn find_comment(&self, slug: &str, id: &str) -> Result<Option<Comment>, AppError> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT c.id, c.slug, c.author, c.body, c.selector, c.parent_id, c.resolved, c.created_at,
                        c.processing_by, c.processing_started_at, c.author_user_id, u.avatar_url, c.role, c.is_agent, c.blueprint_version
                 FROM comments c LEFT JOIN users u ON c.author_user_id = u.id
                 WHERE c.slug = ?1 AND c.id = ?2",
                params![slug, id],
                row_to_comment,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_comments(&self, slug: &str, since: Option<i64>) -> Result<Vec<Comment>, AppError> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.slug, c.author, c.body, c.selector, c.parent_id, c.resolved, c.created_at,
                    c.processing_by, c.processing_started_at, c.author_user_id, u.avatar_url, c.role, c.is_agent, c.blueprint_version
             FROM comments c LEFT JOIN users u ON c.author_user_id = u.id
             WHERE c.slug = ?1 AND c.created_at > ?2 ORDER BY c.created_at ASC",
        )?;
        let out = stmt
            .query_map(params![slug, since.unwrap_or(0)], row_to_comment)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(out)
    }

    /// Mark a comment as currently being processed by `author`. Returns whether the
    /// comment exists; if not, returns false without changing anything.
    pub fn set_processing(&self, slug: &str, id: &str, author: &str) -> Result<bool, AppError> {
        let now = now_ms();
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE comments SET processing_by = ?1, processing_started_at = ?2
             WHERE slug = ?3 AND id = ?4",
            params![author, now, slug, id],
        )?;
        Ok(n > 0)
    }

    /// Clear processing state on a comment. Called automatically when a reply is posted
    /// to the parent (see add_comment), but also exposed for explicit clears.
    pub fn clear_processing(&self, slug: &str, id: &str) -> Result<(), AppError> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE comments SET processing_by = NULL, processing_started_at = NULL
             WHERE slug = ?1 AND id = ?2",
            params![slug, id],
        )?;
        Ok(())
    }

    pub fn set_resolved(&self, slug: &str, id: &str, resolved: bool) -> Result<bool, AppError> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE comments SET resolved = ?1 WHERE slug = ?2 AND id = ?3",
            params![resolved, slug, id],
        )?;
        Ok(n > 0)
    }
}

/// Schema steps, applied in order and tracked by `PRAGMA user_version`. The
/// index into this array *is* the version: after applying `MIGRATIONS[n]` the
/// database reads `user_version = n + 1`, so `MIGRATIONS.len()` is the schema
/// this binary expects.
///
/// Steps are append-only. Editing one that has already shipped changes what a
/// fresh database gets without touching any database that already applied it,
/// which is precisely how the two diverge — add a new step instead.
type Migration = fn(&rusqlite::Transaction) -> Result<(), AppError>;

const MIGRATIONS: &[Migration] = &[
    migration_0_baseline,
    migration_1_finish_pending_at,
    migration_2_comments_parent_id_index,
];

/// Baseline: the schema as it stood when `user_version` tracking was
/// introduced, reproduced exactly.
///
/// Every clause here has to be idempotent, because the databases in the field
/// already have all of this and still read `user_version = 0` — they were built
/// by the hand-rolled loop this replaced, which never recorded a version. So
/// this step runs against them too, and must be a no-op rather than an error.
/// `CREATE TABLE IF NOT EXISTS` covers the tables; the columns added by later
/// ALTERs go through `add_column_if_missing`, the same name-based existence
/// check the old loop used.
///
/// A fresh database therefore converges on the identical schema as a migrated
/// one, which is the whole point — before this, `version` was declared both in
/// `CREATE TABLE` and as an ALTER and you couldn't tell the two apart.
fn migration_0_baseline(tx: &rusqlite::Transaction) -> Result<(), AppError> {
    tx.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS blueprints (
          slug         TEXT PRIMARY KEY,
          html         BLOB NOT NULL,
          created_at   INTEGER NOT NULL,
          delete_token TEXT NOT NULL,
          version      INTEGER NOT NULL DEFAULT 1
        );

        -- Archive of prior HTML, keyed by (slug, version). On every
        -- `update_blueprint_html` the *current* html is snapshotted here at
        -- its current version before the live row is bumped and replaced,
        -- so a comment authored against an older version can still be
        -- resolved against the exact text it anchored to. Cascades away
        -- with the blueprint.
        CREATE TABLE IF NOT EXISTS blueprint_versions (
          slug        TEXT NOT NULL REFERENCES blueprints(slug) ON DELETE CASCADE,
          version     INTEGER NOT NULL,
          html        BLOB NOT NULL,
          created_at  INTEGER NOT NULL,
          PRIMARY KEY (slug, version)
        );

        CREATE TABLE IF NOT EXISTS comments (
          id         TEXT PRIMARY KEY,
          slug       TEXT NOT NULL REFERENCES blueprints(slug) ON DELETE CASCADE,
          author     TEXT NOT NULL,
          body       TEXT NOT NULL,
          selector   TEXT NOT NULL,
          parent_id  TEXT REFERENCES comments(id) ON DELETE CASCADE,
          resolved   INTEGER NOT NULL DEFAULT 0,
          created_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_comments_slug_created
          ON comments(slug, created_at);

        CREATE TABLE IF NOT EXISTS users (
          id         INTEGER PRIMARY KEY AUTOINCREMENT,
          github_id  INTEGER UNIQUE NOT NULL,
          login      TEXT NOT NULL,
          name       TEXT,
          avatar_url TEXT,
          created_at INTEGER NOT NULL,
          updated_at INTEGER NOT NULL
        );

        -- Browser sessions, keyed by the opaque cookie id. `record` is the
        -- serialized tower-sessions Record (its own JSON shape, opaque to
        -- us); `expires_at` is that record's expiry lifted into a column so
        -- expiry can be enforced and swept in SQL. This is on disk rather
        -- than in memory because the daemon is short-lived by design — see
        -- `crate::session_store`.
        CREATE TABLE IF NOT EXISTS sessions (
          id         TEXT PRIMARY KEY,
          record     TEXT NOT NULL,
          expires_at INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_sessions_expires_at
          ON sessions(expires_at);
        "#,
    )?;

    // Columns that postdate the original `CREATE TABLE`s. A database old enough
    // to predate any of these gets it added; a current one skips every line.
    for (table, col, ddl) in [
        (
            "comments",
            "processing_by",
            "ALTER TABLE comments ADD COLUMN processing_by TEXT",
        ),
        (
            "comments",
            "processing_started_at",
            "ALTER TABLE comments ADD COLUMN processing_started_at INTEGER",
        ),
        (
            "comments",
            "author_user_id",
            "ALTER TABLE comments ADD COLUMN author_user_id INTEGER REFERENCES users(id)",
        ),
        (
            "blueprints",
            "client_cwd",
            "ALTER TABLE blueprints ADD COLUMN client_cwd TEXT",
        ),
        (
            "comments",
            "is_agent",
            "ALTER TABLE comments ADD COLUMN is_agent INTEGER NOT NULL DEFAULT 0",
        ),
        // Version the blueprint was published at when this comment was
        // written. NULL for comments predating version history — treated
        // as "current" by the anchoring resolver.
        (
            "comments",
            "blueprint_version",
            "ALTER TABLE comments ADD COLUMN blueprint_version INTEGER",
        ),
        // Persist the live version on the blueprint row itself. Existing
        // rows default to 1 so a pre-migration blueprint reads as v1. Also
        // declared in the `CREATE TABLE` above, so this only fires on a
        // database whose blueprints table predates versioning.
        (
            "blueprints",
            "version",
            "ALTER TABLE blueprints ADD COLUMN version INTEGER NOT NULL DEFAULT 1",
        ),
        // Wall-clock ms of the most recent "Finish Review" click, or NULL if
        // the blueprint has never been finished. Never cleared — it's what
        // the reviewer header shows so the finished state survives a reload.
        (
            "blueprints",
            "finished_at",
            "ALTER TABLE blueprints ADD COLUMN finished_at INTEGER",
        ),
        // Latch: 1 when a finish click is waiting to be picked up by a
        // `watch`, 0 once one has consumed it. Superseded by
        // `finish_pending_at` in the next step, which backfills from it —
        // it has to exist here so that backfill has something to read.
        (
            "blueprints",
            "finish_pending",
            "ALTER TABLE blueprints ADD COLUMN finish_pending INTEGER NOT NULL DEFAULT 0",
        ),
        // Provenance of the comment's author. Defaults to `guest`; the
        // backfill below promotes anything with a user id to `user`.
        (
            "comments",
            "role",
            "ALTER TABLE comments ADD COLUMN role TEXT NOT NULL DEFAULT 'guest'",
        ),
    ] {
        if add_column_if_missing(tx, table, col, ddl)? && (table, col) == ("comments", "role") {
            // Only worth running when the ALTER just happened: every existing
            // row is `guest` at this instant. The step is transactional, so
            // unlike the old code there's no window where the column exists
            // with the backfill skipped.
            tx.execute(
                "UPDATE comments SET role = 'user' WHERE author_user_id IS NOT NULL",
                [],
            )?;
        }
    }
    Ok(())
}

/// Replace the `finish_pending` flag with a nullable timestamp, so "a finish is
/// pending" and "this is when it happened" can't disagree.
///
/// The old pair let `finish_pending = 1` coexist with `finished_at IS NULL`, a
/// state `mark_finished` never writes but that `claim_finish` still had to
/// handle — and handled by inventing `now_ms()` and reporting a fabricated
/// review-completion time to the agent. One nullable column makes the bad state
/// unrepresentable instead of merely unlikely.
///
/// `finish_pending` is deliberately left in place rather than dropped: nothing
/// reads it after this, and an unused column is cheaper than rewriting a table
/// that holds live blueprints.
fn migration_1_finish_pending_at(tx: &rusqlite::Transaction) -> Result<(), AppError> {
    // Guarded rather than a bare ALTER: this step is gated on `user_version` so
    // it normally runs exactly once, but a database that somehow reached this
    // schema without recording the stamp would otherwise die here on "duplicate
    // column name" and refuse to open at all. Skipping the backfill in that case
    // is right — the column is already there and populated.
    if !add_column_if_missing(
        tx,
        "blueprints",
        "finish_pending_at",
        "ALTER TABLE blueprints ADD COLUMN finish_pending_at INTEGER",
    )? {
        return Ok(());
    }
    // A raised latch carries `finished_at` forward as the pending stamp; a
    // lowered one stays NULL. This is the same value the old two-column read
    // would have returned, so a finish clicked before the upgrade and not yet
    // claimed still resolves the next `watch` with the right timestamp.
    tx.execute(
        "UPDATE blueprints SET finish_pending_at = finished_at WHERE finish_pending = 1",
        [],
    )?;
    Ok(())
}

/// Index the self-referential FK. SQLite does not index foreign keys on its
/// own, so `ON DELETE CASCADE` on `parent_id` meant every comment delete — and
/// every blueprint cascade, once per comment it owns — full-scanned `comments`
/// looking for children.
///
/// Partial on `parent_id IS NOT NULL` because most comments are top-level, so
/// the index only carries the replies that the cascade can actually match.
fn migration_2_comments_parent_id_index(tx: &rusqlite::Transaction) -> Result<(), AppError> {
    tx.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_comments_parent_id
           ON comments(parent_id) WHERE parent_id IS NOT NULL;",
    )?;
    Ok(())
}

/// Bring the database up to `MIGRATIONS.len()`, one transaction per step.
///
/// Each step bumps `user_version` inside its own transaction, so a crash
/// mid-migration commits either the whole step or none of it — there's no
/// "column added, backfill skipped" window of the kind the old ad-hoc `role`
/// migration had to hand-guard against.
fn migrate(conn: &mut Connection) -> Result<(), AppError> {
    let current: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let target = MIGRATIONS.len() as u32;
    // A database written by a newer binary may have columns and constraints this
    // one has never heard of. Downgrading silently would mean writing rows that
    // violate them, so refuse rather than corrupt.
    if current > target {
        return Err(AppError::Config(format!(
            "database schema version {current} is newer than this binary supports \
             ({target}); upgrade blueprint or remove the database"
        )));
    }
    for (i, step) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        step(&tx)?;
        // `PRAGMA user_version` takes no bind parameters, hence the format!.
        // `i` is a slice index, so there's nothing injectable about it.
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}

/// `ALTER TABLE ... ADD COLUMN` guarded by a name check, returning whether the
/// column was actually added so a caller can condition a backfill on it.
///
/// Detection is by column *name* only — a column of the right name but the
/// wrong type slips through. That's tolerable here because the check exists
/// solely to let the baseline step run twice (once on a fresh database, once on
/// a field database that already has these columns); every step after this one
/// is gated on `user_version` and runs exactly once, so new columns should be
/// plain ALTERs rather than routed through here.
fn add_column_if_missing(
    tx: &rusqlite::Transaction,
    table: &str,
    col: &str,
    ddl: &str,
) -> Result<bool, AppError> {
    let exists: bool = tx
        .query_row(
            "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, col],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(false);
    }
    tx.execute(ddl, [])?;
    Ok(true)
}

/// Read a blueprint's current version under an existing connection or
/// transaction, for stamping onto a comment at write time. `None` for an
/// unknown slug. Shared by `add_comment` and `add_comments_batch` so the
/// stamp semantics live in one place.
fn read_version(conn: &Connection, slug: &str) -> Result<Option<i64>, AppError> {
    Ok(conn
        .query_row(
            "SELECT version FROM blueprints WHERE slug = ?1",
            params![slug],
            |r| r.get(0),
        )
        .optional()?)
}

/// Clamp the database — and its WAL sidecars, if they already exist — to
/// owner-only. Covers the sidecars explicitly rather than relying on SQLite to
/// inherit the mode, so a database created before this change gets fixed up on
/// the next open instead of staying readable.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    for p in [
        path.to_path_buf(),
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
    ] {
        // A sidecar that isn't there yet inherits the mode we just set on the
        // database, so a missing file is nothing to fix.
        if !p.exists() {
            continue;
        }
        let mut perms = std::fs::metadata(&p)?.permissions();
        if perms.mode() & 0o077 != 0 {
            perms.set_mode(0o600);
            std::fs::set_permissions(&p, perms)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

/// `foo.db` + `-wal` → `foo.db-wal`, matching how SQLite names its sidecars
/// (a suffix on the full filename, not a replaced extension).
#[cfg(unix)]
fn sidecar(path: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn row_to_comment(r: &rusqlite::Row) -> rusqlite::Result<Comment> {
    let sel_json: String = r.get(4)?;
    let selector: TextQuoteSelector = serde_json::from_str(&sel_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    // `bool` and `AuthorRole` both have FromSql, so no `as i64` / `!= 0` here.
    Ok(Comment {
        id: r.get(0)?,
        slug: r.get(1)?,
        author: r.get(2)?,
        body: r.get(3)?,
        selector,
        parent_id: r.get(5)?,
        resolved: r.get(6)?,
        created_at: r.get(7)?,
        processing_by: r.get(8)?,
        processing_started_at: r.get(9)?,
        author_user_id: r.get(10)?,
        author_avatar_url: r.get(11)?,
        role: r.get(12)?,
        is_agent: r.get(13)?,
        blueprint_version: r.get(14)?,
    })
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The database holds session rows, and a session id is a bearer credential.
    /// It — and the WAL sidecars carrying the same rows — must not be readable
    /// by other local users.
    #[test]
    fn database_and_wal_sidecars_are_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");
        let store = Store::open(&db).unwrap();
        // Force the WAL sidecars into existence.
        store.save_session("id", "{}", now_ms() + 60_000).unwrap();

        assert_eq!(mode_of(&db), 0o600, "database should be owner-only");
        for suffix in ["-wal", "-shm"] {
            let side = sidecar(&db, suffix);
            assert!(side.exists(), "expected {suffix} to exist");
            assert_eq!(mode_of(&side), 0o600, "{suffix} should be owner-only");
        }
    }

    /// A database created before sessions moved to disk is group/world-readable.
    /// Opening it has to tighten it rather than leave the old mode in place.
    #[test]
    fn open_tightens_a_preexisting_world_readable_database() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");
        drop(Store::open(&db).unwrap());
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(mode_of(&db), 0o644, "precondition");

        let _store = Store::open(&db).unwrap();
        assert_eq!(mode_of(&db), 0o600);
    }
}

/// Schema tests. Not gated on `unix` — nothing here touches file modes.
///
/// The stakes: these run against a real database holding blueprints and
/// comments, and the baseline step is applied to it even though it already has
/// every column. Parity between "fresh database" and "database upgraded in
/// place" is the property under test, because that's what silently diverging
/// would cost.
#[cfg(test)]
mod schema_tests {
    use super::*;

    /// Every column `row_to_comment` and the blueprint queries read, by table.
    /// Written out longhand rather than derived from the DDL so a dropped column
    /// fails here instead of being "expected" by the same bug that dropped it.
    const EXPECTED: &[(&str, &[&str])] = &[
        (
            "blueprints",
            &[
                "slug",
                "html",
                "created_at",
                "delete_token",
                "version",
                "client_cwd",
                "finished_at",
                "finish_pending_at",
            ],
        ),
        (
            "blueprint_versions",
            &["slug", "version", "html", "created_at"],
        ),
        (
            "comments",
            &[
                "id",
                "slug",
                "author",
                "body",
                "selector",
                "parent_id",
                "resolved",
                "created_at",
                "processing_by",
                "processing_started_at",
                "author_user_id",
                "is_agent",
                "blueprint_version",
                "role",
            ],
        ),
        (
            "users",
            &[
                "id",
                "github_id",
                "login",
                "name",
                "avatar_url",
                "created_at",
                "updated_at",
            ],
        ),
        ("sessions", &["id", "record", "expires_at"]),
    ];

    fn columns_of(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info(?1)")
            .unwrap();
        stmt.query_map(params![table], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn user_version(conn: &Connection) -> u32 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    fn assert_schema_complete(conn: &Connection, context: &str) {
        for (table, expected) in EXPECTED {
            let actual = columns_of(conn, table);
            assert!(
                !actual.is_empty(),
                "{context}: table {table} is missing entirely"
            );
            for col in *expected {
                assert!(
                    actual.iter().any(|c| c == col),
                    "{context}: {table}.{col} missing; has {actual:?}"
                );
            }
        }
    }

    /// A fresh database must land on the newest schema with the version stamped,
    /// so the next open has nothing to do.
    #[test]
    fn a_fresh_database_is_fully_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");
        let store = Store::open(&db).unwrap();

        let conn = store.conn.lock();
        assert_eq!(
            user_version(&conn),
            MIGRATIONS.len() as u32,
            "a fresh database should be stamped at the latest version"
        );
        assert_schema_complete(&conn, "fresh");
    }

    /// The real upgrade path. Builds the *original* pre-migration schema by hand
    /// — the `CREATE TABLE`s as they were before any column was bolted on, with
    /// `user_version = 0`, exactly like the databases in the field — puts rows in
    /// it, then opens it with `Store::open`.
    ///
    /// This is the test that would have caught a baseline step that assumed a
    /// fresh database: the ALTERs must add what's missing, and the pre-existing
    /// rows must still be there afterward.
    #[test]
    fn an_original_schema_database_upgrades_without_losing_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");

        {
            let conn = Connection::open(&db).unwrap();
            // The schema as it originally shipped: no version, client_cwd,
            // finished_at, finish_pending, role, is_agent, processing_*, or
            // blueprint_version anywhere.
            conn.execute_batch(
                "CREATE TABLE blueprints (
                   slug         TEXT PRIMARY KEY,
                   html         BLOB NOT NULL,
                   created_at   INTEGER NOT NULL,
                   delete_token TEXT NOT NULL
                 );
                 CREATE TABLE comments (
                   id         TEXT PRIMARY KEY,
                   slug       TEXT NOT NULL REFERENCES blueprints(slug) ON DELETE CASCADE,
                   author     TEXT NOT NULL,
                   body       TEXT NOT NULL,
                   selector   TEXT NOT NULL,
                   parent_id  TEXT REFERENCES comments(id) ON DELETE CASCADE,
                   resolved   INTEGER NOT NULL DEFAULT 0,
                   created_at INTEGER NOT NULL
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO blueprints (slug, html, created_at, delete_token)
                 VALUES ('legacy', X'3c703e68693c2f703e', 111, 'tok')",
                [],
            )
            .unwrap();
            conn.execute(
                r#"INSERT INTO comments (id, slug, author, body, selector, resolved, created_at)
                   VALUES ('c1', 'legacy', 'ada', 'still here', '{"exact":"hi"}', 0, 222)"#,
                [],
            )
            .unwrap();
            assert_eq!(user_version(&conn), 0, "precondition: unversioned");
        }

        let store = Store::open(&db).unwrap();
        {
            let conn = store.conn.lock();
            assert_eq!(user_version(&conn), MIGRATIONS.len() as u32);
            assert_schema_complete(&conn, "upgraded-in-place");
        }

        // The rows that were there before the upgrade are still there, and the
        // columns added around them carry their documented defaults.
        let bp = store
            .get_blueprint("legacy")
            .unwrap()
            .expect("blueprint survived");
        assert_eq!(bp.created_at, 111);
        assert_eq!(bp.delete_token, "tok");
        assert_eq!(
            store.get_blueprint_version("legacy").unwrap(),
            Some(1),
            "a pre-versioning blueprint reads as v1"
        );
        assert_eq!(
            store.finished_at("legacy").unwrap(),
            None,
            "never finished, so no timestamp"
        );

        let c = store
            .find_comment("legacy", "c1")
            .unwrap()
            .expect("comment survived");
        assert_eq!(c.body, "still here");
        assert_eq!(c.created_at, 222);
        assert!(!c.is_agent);
        assert_eq!(
            c.blueprint_version, None,
            "legacy comment predates versioning"
        );
        assert_eq!(
            c.role,
            AuthorRole::Guest,
            "no author_user_id, so the backfill leaves it guest"
        );
    }

    /// The production case, reproduced column-for-column.
    ///
    /// The live database was inspected while writing this: `user_version = 0`,
    /// and the two tables carry exactly the columns spelled out below — every
    /// post-baseline column *except* `finish_pending_at`, in the order the old
    /// hand-rolled loop appended them. Note `comments` has `role` before
    /// `blueprint_version`, which is the order the ALTERs ran in, not the order
    /// the baseline step lists them; column order is not something the migration
    /// may depend on, and this pins that.
    ///
    /// The baseline step must no-op across all of it and the finish-latch step
    /// must then add its one column.
    #[test]
    fn the_production_schema_shape_is_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (
                   id INTEGER PRIMARY KEY AUTOINCREMENT, github_id INTEGER UNIQUE NOT NULL,
                   login TEXT NOT NULL, name TEXT, avatar_url TEXT,
                   created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE blueprints (
                   slug TEXT PRIMARY KEY, html BLOB NOT NULL, created_at INTEGER NOT NULL,
                   delete_token TEXT NOT NULL, client_cwd TEXT,
                   version INTEGER NOT NULL DEFAULT 1, finished_at INTEGER,
                   finish_pending INTEGER NOT NULL DEFAULT 0
                 );
                 CREATE TABLE comments (
                   id TEXT PRIMARY KEY,
                   slug TEXT NOT NULL REFERENCES blueprints(slug) ON DELETE CASCADE,
                   author TEXT NOT NULL, body TEXT NOT NULL, selector TEXT NOT NULL,
                   parent_id TEXT REFERENCES comments(id) ON DELETE CASCADE,
                   resolved INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
                   processing_by TEXT, processing_started_at INTEGER,
                   author_user_id INTEGER REFERENCES users(id),
                   is_agent INTEGER NOT NULL DEFAULT 0,
                   role TEXT NOT NULL DEFAULT 'guest',
                   blueprint_version INTEGER
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO blueprints (slug, html, created_at, delete_token, version, finished_at, finish_pending)
                 VALUES ('prod', X'00', 1, 't', 3, 555, 1)",
                [],
            )
            .unwrap();
            conn.execute(
                r#"INSERT INTO comments (id, slug, author, body, selector, resolved, created_at, role, blueprint_version)
                   VALUES ('c1', 'prod', 'ada', 'kept', '{"exact":"hi"}', 0, 2, 'owner', 3)"#,
                [],
            )
            .unwrap();
            assert_eq!(user_version(&conn), 0, "precondition: unversioned");
        }

        let store = Store::open(&db).unwrap();
        {
            let conn = store.conn.lock();
            assert_eq!(user_version(&conn), MIGRATIONS.len() as u32);
            assert_schema_complete(&conn, "production-shape");
        }

        // Data and its meaning both survive: the version isn't reset to 1, the
        // role isn't re-defaulted to guest, and the raised latch is carried into
        // the new column so the pending finish is still claimable.
        assert_eq!(store.get_blueprint_version("prod").unwrap(), Some(3));
        let c = store.find_comment("prod", "c1").unwrap().unwrap();
        assert_eq!(
            c.role,
            AuthorRole::Owner,
            "an existing role is not clobbered"
        );
        assert_eq!(c.blueprint_version, Some(3));
        assert_eq!(
            store.claim_finish("prod").unwrap(),
            Some(555),
            "a finish pending across the upgrade is still claimable"
        );
    }

    /// Reopening is idempotent: no step reruns, nothing changes.
    #[test]
    fn reopening_an_up_to_date_database_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");
        drop(Store::open(&db).unwrap());
        let store = Store::open(&db).unwrap();
        let conn = store.conn.lock();
        assert_eq!(user_version(&conn), MIGRATIONS.len() as u32);
        assert_schema_complete(&conn, "reopened");
    }

    /// An older binary must refuse a database a newer one has already upgraded,
    /// rather than write rows that violate constraints it can't see.
    #[test]
    fn a_newer_schema_version_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");
        {
            let store = Store::open(&db).unwrap();
            let conn = store.conn.lock();
            conn.execute_batch(&format!("PRAGMA user_version = {}", MIGRATIONS.len() + 1))
                .unwrap();
        }

        // `Store` isn't `Debug`, so match rather than `expect_err`.
        let msg = match Store::open(&db) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a future schema must not open"),
        };
        assert!(
            msg.contains("newer than this binary supports"),
            "unhelpful refusal: {msg}"
        );
    }

    /// The `finish_pending` → `finish_pending_at` backfill. A raised latch has to
    /// carry `finished_at` across as the pending stamp, or a finish clicked just
    /// before the upgrade is lost and its waiter parks forever.
    #[test]
    fn a_raised_finish_latch_survives_the_backfill() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("blueprints.db");

        // Stop one step short of the backfill, so `finish_pending` is the live
        // representation and can be set the way the old code would have.
        {
            let mut conn = Connection::open(&db).unwrap();
            let tx = conn.transaction().unwrap();
            migration_0_baseline(&tx).unwrap();
            tx.execute_batch("PRAGMA user_version = 1").unwrap();
            tx.commit().unwrap();
            conn.execute(
                "INSERT INTO blueprints (slug, html, created_at, delete_token, finished_at, finish_pending)
                 VALUES ('pending', X'00', 1, 't', 777, 1),
                        ('claimed', X'00', 1, 't', 888, 0),
                        ('never',   X'00', 1, 't', NULL, 0)",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&db).unwrap();
        // The pending one is still claimable, and reports the timestamp it was
        // finished at rather than a fresh one.
        assert_eq!(store.claim_finish("pending").unwrap(), Some(777));
        // ...exactly once.
        assert_eq!(store.claim_finish("pending").unwrap(), None);
        // An already-claimed latch stays down; `finished_at` is untouched.
        assert_eq!(store.claim_finish("claimed").unwrap(), None);
        assert_eq!(store.finished_at("claimed").unwrap(), Some(888));
        assert_eq!(store.claim_finish("never").unwrap(), None);
    }

    /// `claim_finish` used to invent `now_ms()` when the latch was up but
    /// `finished_at` was NULL. With one nullable column that state can't be
    /// built, so the timestamp it reports is always the one that was stored.
    #[test]
    fn claim_finish_reports_the_stored_timestamp_and_claims_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("blueprints.db")).unwrap();
        store
            .insert_blueprint("s", b"<p>x</p>", "tok", None)
            .unwrap();

        assert_eq!(
            store.claim_finish("s").unwrap(),
            None,
            "nothing pending yet"
        );
        assert!(matches!(
            store.claim_finish("nope"),
            Err(AppError::NotFound)
        ));

        let finished_at = store.mark_finished("s").unwrap();
        assert_eq!(
            store.claim_finish("s").unwrap(),
            Some(finished_at),
            "the claim reports exactly what mark_finished wrote"
        );
        assert_eq!(store.claim_finish("s").unwrap(), None, "claimed only once");
        assert_eq!(
            store.finished_at("s").unwrap(),
            Some(finished_at),
            "finished_at outlives the claim"
        );

        // Re-finishing raises the latch again for the next round.
        let again = store.mark_finished("s").unwrap();
        assert_eq!(store.claim_finish("s").unwrap(), Some(again));
    }

    /// The reply insert and the parent's processing clear are one transaction,
    /// so a reply can never be visible with its parent still flagged.
    #[test]
    fn adding_a_reply_clears_the_parent_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("blueprints.db")).unwrap();
        store
            .insert_blueprint("s", b"<p>x</p>", "tok", None)
            .unwrap();

        let sel = crate::selector::TextQuoteSelector {
            ty: "TextQuoteSelector".into(),
            exact: "x".into(),
            prefix: None,
            suffix: None,
        };
        let parent = store
            .add_comment_draft(
                "s",
                &CommentDraft {
                    id: "p1".into(),
                    author: "ada".into(),
                    body: "question".into(),
                    selector: sel.clone(),
                    parent_id: None,
                    author_user_id: None,
                    author_avatar_url: None,
                    role: AuthorRole::Owner,
                    is_agent: false,
                },
            )
            .unwrap();
        assert!(store.set_processing("s", &parent.id, "claude").unwrap());
        assert_eq!(
            store
                .find_comment("s", "p1")
                .unwrap()
                .unwrap()
                .processing_by,
            Some("claude".into()),
            "precondition: parent is flagged"
        );

        let reply = store
            .add_comment_draft(
                "s",
                &CommentDraft {
                    id: "r1".into(),
                    author: "claude".into(),
                    body: "answer".into(),
                    selector: sel,
                    parent_id: Some("p1".into()),
                    author_user_id: None,
                    author_avatar_url: None,
                    role: AuthorRole::User,
                    is_agent: true,
                },
            )
            .unwrap();

        // Both writes landed together: the reply exists AND the parent is clear.
        assert_eq!(reply.parent_id.as_deref(), Some("p1"));
        assert!(store.find_comment("s", "r1").unwrap().is_some());
        let p = store.find_comment("s", "p1").unwrap().unwrap();
        assert_eq!(
            p.processing_by, None,
            "parent's flag cleared in the same tx"
        );
        assert_eq!(p.processing_started_at, None);
    }

    /// `list_versions` must read `current` from the blueprints row. Planting an
    /// archive row above the live version is exactly the cross-table violation
    /// the old max-of-UNION would have reported as current.
    #[test]
    fn list_versions_reads_current_from_the_live_row() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("blueprints.db")).unwrap();
        store
            .insert_blueprint("s", b"<p>v1</p>", "tok", None)
            .unwrap();
        let v2 = store.update_blueprint_html("s", b"<p>v2</p>").unwrap();
        assert_eq!(v2, 2);
        assert_eq!(store.list_versions("s").unwrap(), (2, vec![1, 2]));

        // Plant a bogus archived version above the live one.
        {
            let conn = store.conn.lock();
            conn.execute(
                "INSERT INTO blueprint_versions (slug, version, html, created_at)
                 VALUES ('s', 9, X'00', 1)",
                [],
            )
            .unwrap();
        }
        let (current, versions) = store.list_versions("s").unwrap();
        assert_eq!(
            current, 2,
            "current comes from the blueprints row, not the archive's max"
        );
        assert_eq!(versions, vec![1, 2, 9]);

        assert!(matches!(
            store.list_versions("nope"),
            Err(AppError::NotFound)
        ));
    }

    /// The partial index the cascade needs. Asserted by name because its absence
    /// is invisible — deletes stay correct, just quadratic.
    #[test]
    fn comments_parent_id_is_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path().join("blueprints.db")).unwrap();
        let conn = store.conn.lock();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name='idx_comments_parent_id'",
                [],
                |r| r.get(0),
            )
            .expect("idx_comments_parent_id should exist");
        assert!(
            sql.contains("parent_id IS NOT NULL"),
            "index should be partial: {sql}"
        );
    }
}
