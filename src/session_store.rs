//! A `tower_sessions::SessionStore` backed by the daemon's SQLite database.
//!
//! Sessions used to live in a `MemoryStore`, which is only correct for a
//! long-lived server. This daemon is the opposite: every `blueprint` command
//! may respawn it, an unanswered 500ms health probe gets it SIGTERM'd and
//! replaced, and `POST /api/shutdown-if-empty` stops it outright once the last
//! blueprint is gone. Process-local sessions meant two user-visible bugs:
//!
//!   1. A restart between `GET /login` and GitHub's callback threw away the
//!      CSRF nonce, so the round trip dead-ended on `state mismatch or missing`
//!      — a restart during the seconds a user spends on the consent screen is
//!      routine, not exotic.
//!   2. Every restart silently logged the reviewer out. Since owner-vs-guest is
//!      derived from the session (`auth::role_for`), the owner quietly became a
//!      `guest` and their comments stopped tripping a plan edit.
//!
//! Writing records to the SQLite file the daemon already opens fixes both. The
//! `Record` is stored as JSON — its shape belongs to tower-sessions and we
//! don't interpret it — while the expiry is lifted into its own column so SQL
//! can enforce and sweep it.

use crate::store::Store;
use std::sync::Arc;
use tower_sessions::SessionStore;
use tower_sessions::session::{Id, Record};
use tower_sessions::session_store::{Error as SessionError, Result as SessionResult};

/// How many times to re-roll a colliding session id before giving up. A
/// collision means two 128-bit random ids matched; one retry is already
/// theatre, but the loop keeps `create` honest instead of overwriting a live
/// session belonging to someone else.
const MAX_ID_COLLISION_RETRIES: usize = 8;

#[derive(Clone)]
pub struct SqliteSessionStore {
    store: Arc<Store>,
}

impl std::fmt::Debug for SqliteSessionStore {
    // `SessionStore` requires Debug; `Store` wraps a rusqlite Connection that
    // has nothing useful to print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqliteSessionStore")
    }
}

impl SqliteSessionStore {
    pub fn new(store: Arc<Store>) -> Self {
        SqliteSessionStore { store }
    }
}

fn encode(record: &Record) -> SessionResult<String> {
    serde_json::to_string(record).map_err(|e| SessionError::Encode(e.to_string()))
}

fn decode(raw: &str) -> SessionResult<Record> {
    serde_json::from_str(raw).map_err(|e| SessionError::Decode(e.to_string()))
}

fn backend(e: crate::error::AppError) -> SessionError {
    SessionError::Backend(e.to_string())
}

/// `Record::expiry_date` in wall-clock milliseconds, to match the units every
/// other timestamp column in the store uses.
fn expires_at_ms(record: &Record) -> i64 {
    (record.expiry_date.unix_timestamp_nanos() / 1_000_000) as i64
}

#[async_trait::async_trait]
impl SessionStore for SqliteSessionStore {
    async fn create(&self, record: &mut Record) -> SessionResult<()> {
        for _ in 0..MAX_ID_COLLISION_RETRIES {
            let inserted = self
                .store
                .insert_session(
                    &record.id.to_string(),
                    &encode(record)?,
                    expires_at_ms(record),
                )
                .map_err(backend)?;
            if inserted {
                return Ok(());
            }
            record.id = Id::default();
        }
        Err(SessionError::Backend(
            "could not find an unused session id".into(),
        ))
    }

    async fn save(&self, record: &Record) -> SessionResult<()> {
        self.store
            .save_session(
                &record.id.to_string(),
                &encode(record)?,
                expires_at_ms(record),
            )
            .map_err(backend)
    }

    /// A row we can't make sense of is reported as a *miss*, not an error.
    /// Returning `Err` here propagates out of `Session::insert` and 500s every
    /// `/login` from the holder of that cookie, with nothing short of clearing
    /// cookies to break out; a miss makes tower-sessions mint a fresh session
    /// and the next request just works. The bad row is dropped on the way out so
    /// the condition can't recur.
    async fn load(&self, session_id: &Id) -> SessionResult<Option<Record>> {
        let key = session_id.to_string();
        let Some(raw) = self.store.load_session(&key).map_err(backend)? else {
            return Ok(None);
        };
        let discard = |reason: &str| {
            tracing::warn!(session_id = %key, reason, "discarding unreadable session row");
            if let Err(e) = self.store.delete_session(&key) {
                tracing::warn!(%e, "could not delete unreadable session row");
            }
        };
        let Ok(record) = decode(&raw) else {
            discard("record did not decode");
            return Ok(None);
        };
        // The id is both the primary key and a field inside the record. They can
        // only disagree if a row was corrupted or hand-edited, and trusting the
        // record's copy would hand back a session under the wrong key.
        if record.id != *session_id {
            discard("record id disagrees with its key");
            return Ok(None);
        }
        Ok(Some(record))
    }

    async fn delete(&self, session_id: &Id) -> SessionResult<()> {
        self.store
            .delete_session(&session_id.to_string())
            .map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_sessions::cookie::time::{Duration, OffsetDateTime};

    fn store() -> (tempfile::TempDir, SqliteSessionStore) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&tmp.path().join("t.db")).unwrap());
        (tmp, SqliteSessionStore::new(store))
    }

    fn record(offset: Duration) -> Record {
        let mut data = std::collections::HashMap::new();
        data.insert("oauth_state".to_string(), serde_json::json!("nonce"));
        Record {
            id: Id::default(),
            data,
            expiry_date: OffsetDateTime::now_utc() + offset,
        }
    }

    /// A row that won't decode must read as a miss and be swept, not raise —
    /// an `Err` here 500s every `/login` from that cookie holder for good.
    #[tokio::test]
    async fn undecodable_row_reads_as_a_miss_and_is_dropped() {
        let (_tmp, s) = store();
        let mut r = record(Duration::days(1));
        s.create(&mut r).await.unwrap();
        s.store
            .save_session(&r.id.to_string(), "{ not json at all", i64::MAX)
            .unwrap();

        assert!(
            s.load(&r.id).await.is_ok_and(|r| r.is_none()),
            "a corrupt row must degrade to a miss, not an error"
        );
        assert!(
            s.store.load_session(&r.id.to_string()).unwrap().is_none(),
            "the bad row should be gone so the condition can't recur"
        );
    }

    /// The id is stored twice — as the primary key and inside the record. If
    /// they disagree the row is untrustworthy, so don't hand it back.
    #[tokio::test]
    async fn row_whose_record_id_disagrees_with_its_key_is_rejected() {
        let (_tmp, s) = store();
        let mut r = record(Duration::days(1));
        s.create(&mut r).await.unwrap();
        let mut impostor = record(Duration::days(1));
        impostor.id = Id::default();
        s.store
            .save_session(
                &r.id.to_string(),
                &serde_json::to_string(&impostor).unwrap(),
                i64::MAX,
            )
            .unwrap();

        assert!(s.load(&r.id).await.unwrap().is_none());
        assert!(s.store.load_session(&r.id.to_string()).unwrap().is_none());
    }

    #[tokio::test]
    async fn round_trips_a_record() {
        let (_tmp, s) = store();
        let mut r = record(Duration::days(1));
        s.create(&mut r).await.unwrap();
        let loaded = s.load(&r.id).await.unwrap().expect("record present");
        assert_eq!(loaded.data["oauth_state"], "nonce");
        assert_eq!(loaded.id, r.id);
    }

    #[tokio::test]
    async fn create_re_rolls_a_colliding_id() {
        let (_tmp, s) = store();
        let mut first = record(Duration::days(1));
        s.create(&mut first).await.unwrap();

        // Force the collision: a second record claiming the same id must be
        // given a new one rather than overwrite the first.
        let mut second = record(Duration::days(1));
        second.id = first.id;
        second
            .data
            .insert("oauth_state".into(), serde_json::json!("other"));
        s.create(&mut second).await.unwrap();

        assert_ne!(second.id, first.id, "colliding id should be re-rolled");
        assert_eq!(
            s.load(&first.id).await.unwrap().unwrap().data["oauth_state"],
            "nonce"
        );
        assert_eq!(
            s.load(&second.id).await.unwrap().unwrap().data["oauth_state"],
            "other"
        );
    }

    #[tokio::test]
    async fn expired_record_reads_as_absent_and_sweeps() {
        let (_tmp, s) = store();
        let mut r = record(Duration::seconds(-1));
        s.create(&mut r).await.unwrap();
        assert!(s.load(&r.id).await.unwrap().is_none());
        assert_eq!(s.store.delete_expired_sessions().unwrap(), 1);
    }

    #[tokio::test]
    async fn delete_removes_the_record() {
        let (_tmp, s) = store();
        let mut r = record(Duration::days(1));
        s.create(&mut r).await.unwrap();
        s.delete(&r.id).await.unwrap();
        assert!(s.load(&r.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn save_updates_an_existing_record() {
        let (_tmp, s) = store();
        let mut r = record(Duration::days(1));
        s.create(&mut r).await.unwrap();
        r.data.insert("user_id".into(), serde_json::json!(7));
        s.save(&r).await.unwrap();
        let loaded = s.load(&r.id).await.unwrap().unwrap();
        assert_eq!(loaded.data["user_id"], 7);
    }
}
