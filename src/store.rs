use crate::error::AppError;
use crate::selector::TextQuoteSelector;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
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

    fn from_str(s: &str) -> Self {
        match s {
            "owner" => AuthorRole::Owner,
            "user" => AuthorRole::User,
            _ => AuthorRole::Guest,
        }
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

/// Input shape for `Store::add_comments_batch`. Mirrors `add_comment`'s
/// parameter list but groups everything per-comment so callers can pass a
/// slice of drafts and get back a Vec of fully-formed Comments.
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
    pub fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;

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
            "#,
        )?;

        // Idempotent migrations for columns added after the initial schema.
        // PRAGMA table_info returns one row per column; we just check whether the
        // column exists and ALTER if not.
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
            // rows default to 1 so a pre-migration blueprint reads as v1.
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
            // `watch`, 0 once one has consumed it. Persisting the *pending* bit
            // (rather than just comparing timestamps) is what makes the click
            // durable — a reviewer can click with no watcher running at all and
            // the next `watch` to connect still sees it.
            (
                "blueprints",
                "finish_pending",
                "ALTER TABLE blueprints ADD COLUMN finish_pending INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
                    params![table, col],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                conn.execute(ddl, [])?;
            }
        }

        // Atomic ALTER + backfill for the `role` column. If we crashed
        // between the ALTER and the UPDATE on a non-transactional migration,
        // the next startup would see the column present and skip the backfill
        // entirely, stranding every pre-existing logged-in comment as `guest`.
        // Wrap both in a single transaction so it's all-or-nothing.
        let role_exists: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('comments') WHERE name = 'role'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !role_exists {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "ALTER TABLE comments ADD COLUMN role TEXT NOT NULL DEFAULT 'guest'",
                [],
            )?;
            tx.execute(
                "UPDATE comments SET role = 'user' WHERE author_user_id IS NOT NULL",
                [],
            )?;
            tx.commit()?;
        }

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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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

    pub fn insert_blueprint(
        &self,
        slug: &str,
        html: &[u8],
        delete_token: &str,
        client_cwd: Option<&str>,
    ) -> Result<Blueprint, AppError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
    pub fn mark_finished(&self, slug: &str) -> Result<i64, AppError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE blueprints SET finished_at = ?1, finish_pending = 1 WHERE slug = ?2",
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
    /// The clear is a single conditional UPDATE, so it's atomic — with several
    /// waiters parked on one slug exactly one of them claims the finish, and a
    /// later review round sees a lowered latch and parks correctly.
    pub fn claim_finish(&self, slug: &str) -> Result<Option<i64>, AppError> {
        let conn = self.conn.lock().unwrap();
        let claimed = conn.execute(
            "UPDATE blueprints SET finish_pending = 0 WHERE slug = ?1 AND finish_pending = 1",
            params![slug],
        )? == 1;
        if !claimed {
            // Distinguish "nothing pending" from "no such blueprint".
            let exists = read_version(&conn, slug)?.is_some();
            return if exists { Ok(None) } else { Err(AppError::NotFound) };
        }
        let stamp: Option<Option<i64>> = conn
            .query_row(
                "SELECT finished_at FROM blueprints WHERE slug = ?1",
                params![slug],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        match stamp {
            // The row vanished between the claim and this read — the blueprint
            // was deleted concurrently. Nothing left to finish.
            None => Err(AppError::NotFound),
            // We lowered the latch, so a finish *was* claimed and must be
            // reported. `mark_finished` always writes both columns together, so
            // a NULL stamp means a corrupted row; substituting `now` keeps the
            // wakeup rather than dropping it and parking the waiter forever.
            Some(ts) => Ok(Some(ts.unwrap_or_else(now_ms))),
        }
    }

    /// When this blueprint was last finished, regardless of whether a waiter has
    /// claimed it. `Ok(None)` means never finished; `Err(NotFound)` means the
    /// slug doesn't exist. Drives the reviewer header's durable finished state.
    pub fn finished_at(&self, slug: &str) -> Result<Option<i64>, AppError> {
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        // One query: the archive holds every superseded version and the live
        // row holds the current one. UNION dedups; ORDER BY sorts. The live
        // version is always the max, so `current` falls out of the last row —
        // no separate lookup. Empty result ⇒ unknown slug.
        let mut stmt = conn.prepare(
            "SELECT version FROM blueprint_versions WHERE slug = ?1
             UNION SELECT version FROM blueprints WHERE slug = ?1
             ORDER BY version ASC",
        )?;
        let versions: Vec<u64> = stmt
            .query_map(params![slug], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?
            .into_iter()
            .map(|v| v as u64)
            .collect();
        let current = *versions.last().ok_or(AppError::NotFound)?;
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        let n = conn.execute("DELETE FROM blueprints WHERE slug = ?1", params![slug])?;
        Ok(n > 0)
    }

    pub fn list_blueprints(&self) -> Result<Vec<Blueprint>, AppError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT slug, created_at, delete_token, client_cwd FROM blueprints ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Blueprint {
                slug: r.get(0)?,
                created_at: r.get(1)?,
                delete_token: r.get(2)?,
                client_cwd: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn list_blueprint_summaries(&self) -> Result<Vec<BlueprintSummary>, AppError> {
        let conn = self.conn.lock().unwrap();
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
        let rows = stmt.query_map([], |r| {
            Ok(BlueprintSummary {
                slug: r.get(0)?,
                created_at: r.get(1)?,
                client_cwd: r.get(2)?,
                comment_count: r.get::<_, i64>(3)? as u32,
                unresolved_count: r.get::<_, i64>(4)? as u32,
                last_activity_at: r.get(5)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

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
        let now = now_ms();
        let sel_json = serde_json::to_string(selector)?;
        let conn = self.conn.lock().unwrap();
        // Stamp the version the comment is being written against, read under
        // the same lock so it can't race an interleaved update.
        let blueprint_version = read_version(&conn, slug)?;
        conn.execute(
            "INSERT INTO comments (id, slug, author, body, selector, parent_id, resolved, created_at, author_user_id, role, is_agent, blueprint_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8, ?9, ?10, ?11)",
            params![id, slug, author, body, sel_json, parent_id, now, author_user_id, role.as_str(), is_agent as i64, blueprint_version],
        )?;
        // If this comment is a reply, clear processing state on its parent — the agent
        // that was working on it has now produced its response.
        if let Some(pid) = parent_id {
            conn.execute(
                "UPDATE comments SET processing_by = NULL, processing_started_at = NULL
                 WHERE slug = ?1 AND id = ?2",
                params![slug, pid],
            )?;
        }
        Ok(Comment {
            id: id.into(),
            slug: slug.into(),
            author: author.into(),
            body: body.into(),
            selector: selector.clone(),
            parent_id: parent_id.map(|s| s.into()),
            resolved: false,
            created_at: now,
            processing_by: None,
            processing_started_at: None,
            author_user_id,
            author_avatar_url,
            role,
            is_agent,
            blueprint_version,
        })
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
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;

        // First pass: validate every parent_id either points at an existing
        // DB row OR an earlier draft in this batch. Reject the whole batch
        // if anything is bogus — partial inserts on a batch are worse than
        // a clean failure the client can correct and retry.
        let batch_ids: std::collections::HashSet<&str> =
            drafts.iter().map(|d| d.id.as_str()).collect();
        for d in drafts {
            if let Some(pid) = &d.parent_id
                && !batch_ids.contains(pid.as_str())
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
                    d.role.as_str(),
                    d.is_agent as i64,
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
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.slug, c.author, c.body, c.selector, c.parent_id, c.resolved, c.created_at,
                    c.processing_by, c.processing_started_at, c.author_user_id, u.avatar_url, c.role, c.is_agent, c.blueprint_version
             FROM comments c LEFT JOIN users u ON c.author_user_id = u.id
             WHERE c.slug = ?1 AND c.created_at > ?2 ORDER BY c.created_at ASC",
        )?;
        let rows = stmt.query_map(params![slug, since.unwrap_or(0)], row_to_comment)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Mark a comment as currently being processed by `author`. Returns whether the
    /// comment exists; if not, returns false without changing anything.
    pub fn set_processing(&self, slug: &str, id: &str, author: &str) -> Result<bool, AppError> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE comments SET processing_by = NULL, processing_started_at = NULL
             WHERE slug = ?1 AND id = ?2",
            params![slug, id],
        )?;
        Ok(())
    }

    pub fn set_resolved(&self, slug: &str, id: &str, resolved: bool) -> Result<bool, AppError> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE comments SET resolved = ?1 WHERE slug = ?2 AND id = ?3",
            params![resolved as i64, slug, id],
        )?;
        Ok(n > 0)
    }
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

fn row_to_comment(r: &rusqlite::Row) -> rusqlite::Result<Comment> {
    let sel_json: String = r.get(4)?;
    let selector: TextQuoteSelector = serde_json::from_str(&sel_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let resolved: i64 = r.get(6)?;
    let role_str: String = r.get(12)?;
    let is_agent_int: i64 = r.get(13)?;
    Ok(Comment {
        id: r.get(0)?,
        slug: r.get(1)?,
        author: r.get(2)?,
        body: r.get(3)?,
        selector,
        parent_id: r.get(5)?,
        resolved: resolved != 0,
        created_at: r.get(7)?,
        processing_by: r.get(8)?,
        processing_started_at: r.get(9)?,
        author_user_id: r.get(10)?,
        author_avatar_url: r.get(11)?,
        role: AuthorRole::from_str(&role_str),
        is_agent: is_agent_int != 0,
        blueprint_version: r.get(14)?,
    })
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
