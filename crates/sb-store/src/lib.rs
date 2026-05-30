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

/// One executed request's usage + attributed cost, durably recorded so the
/// `/v1/usage` accounting survives a restart. Metadata only (token counts, cost,
/// latency) — never prompt/response content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageEvent {
    pub request_id: String,
    pub provider_id: String,
    pub model: String,
    pub account_id: Option<String>,
    #[serde(default)]
    pub tenant: Option<String>,
    pub cost_micros: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    pub streamed: bool,
    pub created_at_ms: i64,
}

/// A stored response for an idempotency key — captured rendered bytes so a
/// duplicate non-streaming request replays the EXACT original wire response.
/// `fingerprint` is a hash of the original request body: a reused key with a
/// different body is a client error, not a replay.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub fingerprint: String,
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub created_at_ms: i64,
}

/// A staged `/cp/v1` config draft, persisted so it survives a restart. NOTE:
/// `config_json` is the FULL proposed config including any inline secrets — a
/// deliberate choice for durable drafts (publish needs the real config), unlike
/// the metadata-only revision/usage tables.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DraftRecord {
    pub id: String,
    pub config_json: String,
    pub base_revision: u64,
    pub created_at_ms: i64,
}

/// `(key, request_count, cost_micros)` — one grouped row of the usage rollup.
pub type UsageBucket = (String, u64, u64);

/// Aggregated usage across all durably-recorded events: totals + per-provider and
/// per-model buckets. Computed in SQL so the hot path never scans rows.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageRollup {
    pub requests: u64,
    pub total_cost_micros: u64,
    pub by_provider: Vec<UsageBucket>,
    pub by_model: Vec<UsageBucket>,
    pub by_tenant: Vec<UsageBucket>,
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
    /// Durably append one usage event.
    fn record_usage(&self, event: &UsageEvent) -> Result<()>;
    /// Aggregate all durably-recorded usage (totals + by-provider + by-model).
    fn usage_rollup(&self) -> Result<UsageRollup>;
    /// The most recent `limit` usage events (newest first).
    fn recent_usage(&self, limit: usize) -> Result<Vec<UsageEvent>>;
    /// Look up a stored response by idempotency key.
    fn idempotency_get(&self, key: &str) -> Result<Option<IdempotencyRecord>>;
    /// Store a response under an idempotency key. First writer wins (existing
    /// keys are left untouched); returns `true` if this call inserted the record.
    fn idempotency_put(&self, rec: &IdempotencyRecord) -> Result<bool>;
    /// Stage (or replace) a `/cp/v1` config draft.
    fn put_draft(&self, rec: &DraftRecord) -> Result<()>;
    /// Fetch a staged draft by id.
    fn get_draft(&self, id: &str) -> Result<Option<DraftRecord>>;
    /// All staged drafts (newest first).
    fn list_drafts(&self) -> Result<Vec<DraftRecord>>;
    /// Remove a staged draft (e.g. after publish).
    fn delete_draft(&self, id: &str) -> Result<()>;
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
             CREATE INDEX IF NOT EXISTS audit_by_time ON audit(created_at);
             CREATE TABLE IF NOT EXISTS usage (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 request_id    TEXT    NOT NULL,
                 provider_id   TEXT    NOT NULL,
                 model         TEXT    NOT NULL,
                 account_id    TEXT,
                 tenant        TEXT,
                 cost_micros   INTEGER NOT NULL,
                 input_tokens  INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 latency_ms    INTEGER NOT NULL,
                 streamed      INTEGER NOT NULL,
                 created_at    INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS usage_by_provider ON usage(provider_id);
             CREATE INDEX IF NOT EXISTS usage_by_model ON usage(model);
             CREATE INDEX IF NOT EXISTS usage_by_tenant ON usage(tenant);
             CREATE TABLE IF NOT EXISTS idempotency (
                 key          TEXT    PRIMARY KEY,
                 fingerprint  TEXT    NOT NULL,
                 status       INTEGER NOT NULL,
                 content_type TEXT    NOT NULL,
                 body         TEXT    NOT NULL,
                 created_at   INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS drafts (
                 id            TEXT    PRIMARY KEY,
                 config_json   TEXT    NOT NULL,
                 base_revision INTEGER NOT NULL,
                 created_at    INTEGER NOT NULL
             );",
        )?;
        // Bring a usage table created before tenant attribution up to date. A
        // fresh DB already has the column (CREATE above); the ALTER errors with
        // "duplicate column name" there, which we ignore.
        if let Err(e) = conn.execute("ALTER TABLE usage ADD COLUMN tenant TEXT", []) {
            if !e.to_string().contains("duplicate column name") {
                return Err(e.into());
            }
        }
        Ok(())
    }

    /// Run the `(key, COUNT(*), SUM(cost_micros))` grouped query for one column.
    fn usage_buckets(conn: &Connection, group_col: &str) -> Result<Vec<UsageBucket>> {
        let sql = format!(
            "SELECT {group_col}, COUNT(*), COALESCE(SUM(cost_micros),0)
             FROM usage GROUP BY {group_col} ORDER BY {group_col}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
            params![
                rec.revision as i64,
                rec.config_hash,
                rec.source,
                rec.created_at_ms
            ],
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
            params![
                entry.revision as i64,
                entry.action,
                entry.detail,
                entry.created_at_ms
            ],
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

    fn record_usage(&self, e: &UsageEvent) -> Result<()> {
        let conn = self.conn.lock().expect("state store mutex");
        conn.execute(
            "INSERT INTO usage
                (request_id, provider_id, model, account_id, tenant, cost_micros,
                 input_tokens, output_tokens, latency_ms, streamed, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                e.request_id,
                e.provider_id,
                e.model,
                e.account_id,
                e.tenant,
                e.cost_micros as i64,
                e.input_tokens as i64,
                e.output_tokens as i64,
                e.latency_ms as i64,
                e.streamed as i64,
                e.created_at_ms,
            ],
        )?;
        Ok(())
    }

    fn usage_rollup(&self) -> Result<UsageRollup> {
        let conn = self.conn.lock().expect("state store mutex");
        let (requests, total_cost_micros) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(cost_micros),0) FROM usage",
            [],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?;
        // Tenant buckets skip unattributed rows (tenant IS NULL).
        let mut tenant_stmt = conn.prepare(
            "SELECT tenant, COUNT(*), COALESCE(SUM(cost_micros),0)
             FROM usage WHERE tenant IS NOT NULL GROUP BY tenant ORDER BY tenant",
        )?;
        let by_tenant = tenant_stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(UsageRollup {
            requests,
            total_cost_micros,
            by_provider: Self::usage_buckets(&conn, "provider_id")?,
            by_model: Self::usage_buckets(&conn, "model")?,
            by_tenant,
        })
    }

    fn recent_usage(&self, limit: usize) -> Result<Vec<UsageEvent>> {
        let conn = self.conn.lock().expect("state store mutex");
        let mut stmt = conn.prepare(
            "SELECT request_id, provider_id, model, account_id, tenant, cost_micros,
                    input_tokens, output_tokens, latency_ms, streamed, created_at
             FROM usage ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok(UsageEvent {
                    request_id: row.get(0)?,
                    provider_id: row.get(1)?,
                    model: row.get(2)?,
                    account_id: row.get(3)?,
                    tenant: row.get(4)?,
                    cost_micros: row.get::<_, i64>(5)? as u64,
                    input_tokens: row.get::<_, i64>(6)? as u64,
                    output_tokens: row.get::<_, i64>(7)? as u64,
                    latency_ms: row.get::<_, i64>(8)? as u64,
                    streamed: row.get::<_, i64>(9)? != 0,
                    created_at_ms: row.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn idempotency_get(&self, key: &str) -> Result<Option<IdempotencyRecord>> {
        let conn = self.conn.lock().expect("state store mutex");
        let mut stmt = conn.prepare(
            "SELECT key, fingerprint, status, content_type, body, created_at
             FROM idempotency WHERE key = ?1",
        )?;
        let mut rows = stmt.query_map([key], |row| {
            Ok(IdempotencyRecord {
                key: row.get(0)?,
                fingerprint: row.get(1)?,
                status: row.get::<_, i64>(2)? as u16,
                content_type: row.get(3)?,
                body: row.get(4)?,
                created_at_ms: row.get(5)?,
            })
        })?;
        match rows.next() {
            Some(rec) => Ok(Some(rec?)),
            None => Ok(None),
        }
    }

    fn idempotency_put(&self, rec: &IdempotencyRecord) -> Result<bool> {
        let conn = self.conn.lock().expect("state store mutex");
        // First writer wins — a concurrent racer's INSERT is ignored, so a key
        // never flips to a different stored response.
        let changed = conn.execute(
            "INSERT OR IGNORE INTO idempotency
                (key, fingerprint, status, content_type, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                rec.key,
                rec.fingerprint,
                rec.status as i64,
                rec.content_type,
                rec.body,
                rec.created_at_ms,
            ],
        )?;
        Ok(changed > 0)
    }

    fn put_draft(&self, rec: &DraftRecord) -> Result<()> {
        let conn = self.conn.lock().expect("state store mutex");
        conn.execute(
            "INSERT OR REPLACE INTO drafts (id, config_json, base_revision, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                rec.id,
                rec.config_json,
                rec.base_revision as i64,
                rec.created_at_ms
            ],
        )?;
        Ok(())
    }

    fn get_draft(&self, id: &str) -> Result<Option<DraftRecord>> {
        let conn = self.conn.lock().expect("state store mutex");
        let mut stmt = conn.prepare(
            "SELECT id, config_json, base_revision, created_at FROM drafts WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([id], |row| {
            Ok(DraftRecord {
                id: row.get(0)?,
                config_json: row.get(1)?,
                base_revision: row.get::<_, i64>(2)? as u64,
                created_at_ms: row.get(3)?,
            })
        })?;
        match rows.next() {
            Some(rec) => Ok(Some(rec?)),
            None => Ok(None),
        }
    }

    fn list_drafts(&self) -> Result<Vec<DraftRecord>> {
        let conn = self.conn.lock().expect("state store mutex");
        let mut stmt = conn.prepare(
            "SELECT id, config_json, base_revision, created_at
             FROM drafts ORDER BY created_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DraftRecord {
                    id: row.get(0)?,
                    config_json: row.get(1)?,
                    base_revision: row.get::<_, i64>(2)? as u64,
                    created_at_ms: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn delete_draft(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("state store mutex");
        conn.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
        Ok(())
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

    #[test]
    fn usage_events_record_and_roll_up() {
        let store = SqliteStore::in_memory().unwrap();
        let ev = |rid: &str, prov: &str, model: &str, tenant: &str, cost: u64| UsageEvent {
            request_id: rid.into(),
            provider_id: prov.into(),
            model: model.into(),
            account_id: Some("a".into()),
            tenant: Some(tenant.into()),
            cost_micros: cost,
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 20,
            streamed: false,
            created_at_ms: 1000,
        };
        store
            .record_usage(&ev("r1", "anthropic", "claude", "acme", 100))
            .unwrap();
        store
            .record_usage(&ev("r2", "anthropic", "claude", "acme", 200))
            .unwrap();
        store
            .record_usage(&ev("r3", "openai", "gpt", "globex", 50))
            .unwrap();

        let roll = store.usage_rollup().unwrap();
        assert_eq!(roll.requests, 3);
        assert_eq!(roll.total_cost_micros, 350);
        assert_eq!(
            roll.by_provider,
            vec![("anthropic".into(), 2, 300), ("openai".into(), 1, 50)]
        );
        assert!(roll.by_model.contains(&("claude".to_string(), 2, 300)));
        assert_eq!(
            roll.by_tenant,
            vec![("acme".into(), 2, 300), ("globex".into(), 1, 50)]
        );

        let recent = store.recent_usage(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].request_id, "r3", "newest first");
    }

    #[test]
    fn idempotency_first_writer_wins_and_replays() {
        let store = SqliteStore::in_memory().unwrap();
        let rec = |body: &str| IdempotencyRecord {
            key: "k1".into(),
            fingerprint: "fp".into(),
            status: 200,
            content_type: "application/json".into(),
            body: body.into(),
            created_at_ms: 1,
        };
        assert!(
            store.idempotency_put(&rec("first")).unwrap(),
            "first insert wins"
        );
        assert!(
            !store.idempotency_put(&rec("second")).unwrap(),
            "second insert is ignored (key already present)"
        );
        let got = store.idempotency_get("k1").unwrap().unwrap();
        assert_eq!(got.body, "first", "the original response is what replays");
        assert_eq!(got.fingerprint, "fp");
        assert!(store.idempotency_get("missing").unwrap().is_none());
    }
}
