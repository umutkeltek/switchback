//! Switchback's durable control-plane state store.
//!
//! A `StateStore` trait with a bundled-SQLite backend ([`SqliteStore`]). The
//! first slice persists **config revisions** (one row per published snapshot:
//! revision, config hash, source, timestamp) and an **audit log** (one row per
//! reload / runtime change). It is metadata only — no config body, so no secrets
//! land in the DB. The hot path stays in memory (the compiled snapshot); this
//! store is the authoritative *history*, the bridge to a hosted control plane.
//!
//! The trait is the seam: SQLite for local/team mode today, a Postgres backend
//! behind the same trait for hosted mode later.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

/// Unix epoch milliseconds now — the timestamp every record is stamped with.
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A store operation error. Kept as a string so the trait stays backend-agnostic
/// (no `rusqlite` types leak through the public seam).
#[derive(Debug, Clone)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "state store error: {}", self.0)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// One published config revision. Metadata only — the `config_hash` is a stable
/// fingerprint of the full config (so drift is detectable) but the body is not
/// stored. `source` is how the revision came to be: `bootstrap` | `reload` |
/// `runtime_patch`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RevisionRecord {
    pub revision: u64,
    pub config_hash: String,
    pub source: String,
    pub created_at_ms: i64,
}

/// One audit-log entry: a control-plane change, the revision it produced, a
/// short human/machine-readable detail, and when it happened.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub revision: u64,
    pub action: String,
    pub detail: String,
    pub created_at_ms: i64,
}

/// The persistence seam. Backends: [`SqliteStore`] (local/team), a future
/// Postgres backend (hosted) — both behind this one trait so the runtime never
/// knows which it's talking to. Writes are best-effort from the runtime's view
/// (a store error must not take down request serving), but the trait surfaces
/// the error so the caller can log it.
pub trait StateStore: Send + Sync {
    fn record_revision(&self, rec: &RevisionRecord) -> Result<()>;
    fn list_revisions(&self, limit: usize) -> Result<Vec<RevisionRecord>>;
    fn get_revision(&self, revision: u64) -> Result<Option<RevisionRecord>>;
    fn record_audit(&self, entry: &AuditEntry) -> Result<()>;
    fn list_audit(&self, limit: usize) -> Result<Vec<AuditEntry>>;
}

/// SQLite-backed store (bundled SQLite — no system dependency). The connection
/// is guarded by a `Mutex`; control-plane writes are infrequent (one per
/// publish), so contention is a non-issue and a pool would be premature.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (or create) a SQLite file and run migrations.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = SqliteStore {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// An ephemeral in-memory store (tests / persistence-disabled-but-present).
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = SqliteStore {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().expect("state store mutex");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS revisions (
                 revision    INTEGER PRIMARY KEY,
                 config_hash TEXT    NOT NULL,
                 source      TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS audit (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 revision    INTEGER NOT NULL,
                 action      TEXT    NOT NULL,
                 detail      TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS audit_by_time ON audit(created_at);",
        )?;
        Ok(())
    }
}

impl StateStore for SqliteStore {
    fn record_revision(&self, rec: &RevisionRecord) -> Result<()> {
        let conn = self.conn.lock().expect("state store mutex");
        // A runtime-knob change bumps the revision with the same config_hash; a
        // revision number is never reused, so OR REPLACE is just belt-and-braces.
        conn.execute(
            "INSERT OR REPLACE INTO revisions (revision, config_hash, source, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![rec.revision as i64, rec.config_hash, rec.source, rec.created_at_ms],
        )?;
        Ok(())
    }

    fn list_revisions(&self, limit: usize) -> Result<Vec<RevisionRecord>> {
        let conn = self.conn.lock().expect("state store mutex");
        let mut stmt = conn.prepare(
            "SELECT revision, config_hash, source, created_at
             FROM revisions ORDER BY revision DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok(RevisionRecord {
                    revision: row.get::<_, i64>(0)? as u64,
                    config_hash: row.get(1)?,
                    source: row.get(2)?,
                    created_at_ms: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_revision(&self, revision: u64) -> Result<Option<RevisionRecord>> {
        let conn = self.conn.lock().expect("state store mutex");
        let mut stmt = conn.prepare(
            "SELECT revision, config_hash, source, created_at
             FROM revisions WHERE revision = ?1",
        )?;
        let mut rows = stmt.query_map([revision as i64], |row| {
            Ok(RevisionRecord {
                revision: row.get::<_, i64>(0)? as u64,
                config_hash: row.get(1)?,
                source: row.get(2)?,
                created_at_ms: row.get(3)?,
            })
        })?;
        match rows.next() {
            Some(rec) => Ok(Some(rec?)),
            None => Ok(None),
        }
    }

    fn record_audit(&self, entry: &AuditEntry) -> Result<()> {
        let conn = self.conn.lock().expect("state store mutex");
        conn.execute(
            "INSERT INTO audit (revision, action, detail, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![entry.revision as i64, entry.action, entry.detail, entry.created_at_ms],
        )?;
        Ok(())
    }

    fn list_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self.conn.lock().expect("state store mutex");
        let mut stmt = conn.prepare(
            "SELECT revision, action, detail, created_at
             FROM audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok(AuditEntry {
                    revision: row.get::<_, i64>(0)? as u64,
                    action: row.get(1)?,
                    detail: row.get(2)?,
                    created_at_ms: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_and_audit_round_trip() {
        let store = SqliteStore::in_memory().unwrap();

        store
            .record_revision(&RevisionRecord {
                revision: 1,
                config_hash: "abc".into(),
                source: "bootstrap".into(),
                created_at_ms: 1000,
            })
            .unwrap();
        store
            .record_audit(&AuditEntry {
                revision: 1,
                action: "bootstrap".into(),
                detail: "from config/x.yaml".into(),
                created_at_ms: 1000,
            })
            .unwrap();
        store
            .record_revision(&RevisionRecord {
                revision: 2,
                config_hash: "def".into(),
                source: "reload".into(),
                created_at_ms: 2000,
            })
            .unwrap();

        let revs = store.list_revisions(10).unwrap();
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[0].revision, 2, "newest first");
        assert_eq!(revs[0].source, "reload");
        assert_eq!(revs[1].revision, 1);

        let one = store.get_revision(1).unwrap().unwrap();
        assert_eq!(one.config_hash, "abc");
        assert!(store.get_revision(99).unwrap().is_none());

        let audit = store.list_audit(10).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].action, "bootstrap");
    }
}
