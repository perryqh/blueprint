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
              delete_token TEXT NOT NULL
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

    pub fn update_blueprint_html(&self, slug: &str, html: &[u8]) -> Result<(), AppError> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute(
            "UPDATE blueprints SET html = ?1 WHERE slug = ?2",
            params![html, slug],
        )?;
        if rows == 0 {
            return Err(AppError::NotFound);
        }
        Ok(())
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
    ) -> Result<Comment, AppError> {
        let now = now_ms();
        let sel_json = serde_json::to_string(selector)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO comments (id, slug, author, body, selector, parent_id, resolved, created_at, author_user_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
            params![id, slug, author, body, sel_json, parent_id, now, author_user_id],
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

        let mut out = Vec::with_capacity(drafts.len());
        let mut parents_to_clear: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for d in drafts {
            let sel_json = serde_json::to_string(&d.selector)?;
            tx.execute(
                "INSERT INTO comments (id, slug, author, body, selector, parent_id, resolved, created_at, author_user_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
                params![
                    d.id,
                    slug,
                    d.author,
                    d.body,
                    sel_json,
                    d.parent_id,
                    now,
                    d.author_user_id
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
                        c.processing_by, c.processing_started_at, c.author_user_id, u.avatar_url
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
                    c.processing_by, c.processing_started_at, c.author_user_id, u.avatar_url
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

fn row_to_comment(r: &rusqlite::Row) -> rusqlite::Result<Comment> {
    let sel_json: String = r.get(4)?;
    let selector: TextQuoteSelector = serde_json::from_str(&sel_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let resolved: i64 = r.get(6)?;
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
    })
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
