//! Switchback's durable control-plane state store.
//!
//! A `StateStore` trait with a bundled-SQLite backend ([`SqliteStore`]). The
//! first slice persists **config revisions** (one row per published snapshot:
//! revision, config hash, source, timestamp) and an **audit log** (one row per
//! reload / runtime change). Revision/audit/usage rows are metadata only. Other
//! tables can persist bodies (idempotency replay) or draft configs only when the
//! server layer explicitly opts into those policies. The hot path stays in memory
//! (the compiled snapshot); this store is the authoritative *history*, the bridge
//! to a hosted control plane.
//!
//! The trait is the seam: SQLite for local/team mode today, a Postgres backend
//! behind the same trait for hosted mode later.

use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, Transaction, TransactionBehavior};

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
/// stored. `source` is how the revision came to be: `bootstrap` |
/// `file_reload` | `draft_publish` | `runtime_patch` or another caller-owned
/// source label.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RevisionRecord {
    pub revision: u64,
    pub config_hash: String,
    pub source: String,
    pub created_at_ms: i64,
}

/// One audit-log entry: a control-plane change, the actor/source/object context
/// behind it, the revision it produced, a short human/machine-readable detail,
/// and when it happened.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    pub revision: u64,
    pub action: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_tenant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_project: Option<String>,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_id: Option<String>,
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub fingerprint: String,
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub created_at_ms: i64,
}

/// Result of atomically beginning an idempotent request. This combines durable
/// replay lookup with cross-process single-flight locking, so two gateway
/// processes sharing the same store cannot both execute the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyBegin {
    Claimed,
    InProgress,
    Mismatch,
    Replay(IdempotencyRecord),
}

/// A staged `/cp/v1` config draft, persisted so it survives a restart. The
/// server layer decides whether secret-bearing config bodies may be stored before
/// calling this trait.
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
/// knows which it's talking to. Callers decide whether a write is best-effort
/// local durability or a required control-plane invariant; the trait surfaces
/// errors for both policies.
pub trait StateStore: Send + Sync {
    fn record_revision(&self, rec: &RevisionRecord) -> Result<()>;
    /// Atomically record a revision and its audit entry. Backends should
    /// override this when they can provide a transaction; the default keeps
    /// simple test stores small.
    fn record_revision_and_audit(
        &self,
        revision: &RevisionRecord,
        audit: &AuditEntry,
    ) -> Result<()> {
        self.record_revision(revision)?;
        self.record_audit(audit)
    }
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
    /// Atomically claim an in-flight idempotency key, or return an existing
    /// replay/mismatch/in-progress state. Backends may use `ttl_ms` to clean up
    /// abandoned in-flight claims after a process crash.
    fn idempotency_begin(
        &self,
        _key: &str,
        _fingerprint: &str,
        _ttl_ms: u64,
    ) -> Result<IdempotencyBegin> {
        Err(StoreError(
            "idempotency in-flight coordination is not supported".to_string(),
        ))
    }
    /// Release an in-flight idempotency claim after the request has completed.
    fn idempotency_release(&self, _key: &str) -> Result<()> {
        Ok(())
    }
    /// Atomically acquire one tenant concurrency slot. Returns `true` if the
    /// slot was acquired, `false` if the tenant is already at `max`.
    fn tenant_slot_acquire(
        &self,
        _tenant: &str,
        _slot_id: &str,
        _max: u32,
        _ttl_ms: u64,
    ) -> Result<bool> {
        Err(StoreError(
            "tenant concurrency coordination is not supported".to_string(),
        ))
    }
    /// Release one tenant concurrency slot.
    fn tenant_slot_release(&self, _slot_id: &str) -> Result<()> {
        Ok(())
    }
    /// Count active tenant concurrency slots after expiring abandoned rows.
    fn tenant_slot_count(&self, _tenant: &str) -> Result<u32> {
        Ok(0)
    }
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

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| StoreError("state store mutex poisoned".to_string()))
    }

    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn()?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS schema_migrations (
                 version       INTEGER PRIMARY KEY,
                 name          TEXT    NOT NULL,
                 applied_at_ms INTEGER NOT NULL
             );",
        )?;
        Self::apply_migration(&mut conn, 1, "initial_control_plane_state", |tx| {
            tx.execute_batch(
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
                     actor_role  TEXT,
                     actor_tenant TEXT,
                     actor_project TEXT,
                     source      TEXT,
                     object_id   TEXT,
                     created_at  INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS audit_by_time ON audit(created_at);
                 CREATE TABLE IF NOT EXISTS usage (
                     id            INTEGER PRIMARY KEY AUTOINCREMENT,
                     request_id    TEXT    NOT NULL,
                     provider_id   TEXT    NOT NULL,
                     model         TEXT    NOT NULL,
                     account_id    TEXT,
                     cost_micros   INTEGER NOT NULL,
                     input_tokens  INTEGER NOT NULL,
                     output_tokens INTEGER NOT NULL,
                     latency_ms    INTEGER NOT NULL,
                     streamed      INTEGER NOT NULL,
                     created_at    INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS usage_by_provider ON usage(provider_id);
                 CREATE INDEX IF NOT EXISTS usage_by_model ON usage(model);
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
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 2, "usage_tenant_attribution", |tx| {
            if !Self::column_exists(tx, "usage", "tenant")? {
                tx.execute("ALTER TABLE usage ADD COLUMN tenant TEXT", [])?;
            }
            tx.execute(
                "CREATE INDEX IF NOT EXISTS usage_by_tenant ON usage(tenant)",
                [],
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 3, "audit_context", |tx| {
            for column in [
                "actor_role",
                "actor_tenant",
                "actor_project",
                "source",
                "object_id",
            ] {
                if !Self::column_exists(tx, "audit", column)? {
                    tx.execute(&format!("ALTER TABLE audit ADD COLUMN {column} TEXT"), [])?;
                }
            }
            tx.execute(
                "UPDATE audit SET source = action WHERE source IS NULL OR source = ''",
                [],
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 4, "coordination_leases", |tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS idempotency_inflight (
                     key         TEXT    PRIMARY KEY,
                     fingerprint TEXT    NOT NULL,
                     created_at  INTEGER NOT NULL,
                     expires_at  INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idempotency_inflight_expires
                   ON idempotency_inflight(expires_at);
                 CREATE TABLE IF NOT EXISTS tenant_slots (
                     slot_id    TEXT    PRIMARY KEY,
                     tenant     TEXT    NOT NULL,
                     created_at INTEGER NOT NULL,
                     expires_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS tenant_slots_by_tenant
                   ON tenant_slots(tenant, expires_at);
                 CREATE INDEX IF NOT EXISTS tenant_slots_expires
                   ON tenant_slots(expires_at);",
            )?;
            Ok(())
        })?;
        Ok(())
    }

    fn apply_migration<F>(
        conn: &mut Connection,
        version: i64,
        name: &str,
        migration: F,
    ) -> Result<()>
    where
        F: FnOnce(&Transaction<'_>) -> rusqlite::Result<()>,
    {
        let applied = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM schema_migrations WHERE version = ?1)",
            [version],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if applied {
            return Ok(());
        }

        let tx = conn.transaction()?;
        migration(&tx)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at_ms)
             VALUES (?1, ?2, ?3)",
            params![version, name, now_millis()],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            if name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn schema_versions(&self) -> Result<Vec<i64>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("SELECT version FROM schema_migrations ORDER BY version ASC")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
        let conn = self.conn()?;
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

    fn record_revision_and_audit(
        &self,
        revision: &RevisionRecord,
        audit: &AuditEntry,
    ) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO revisions (revision, config_hash, source, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                revision.revision as i64,
                revision.config_hash,
                revision.source,
                revision.created_at_ms
            ],
        )?;
        tx.execute(
            "INSERT INTO audit
                (revision, action, detail, actor_role, actor_tenant, actor_project, source, object_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                audit.revision as i64,
                audit.action,
                audit.detail,
                audit.actor_role,
                audit.actor_tenant,
                audit.actor_project,
                audit.source,
                audit.object_id,
                audit.created_at_ms
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn list_revisions(&self, limit: usize) -> Result<Vec<RevisionRecord>> {
        let conn = self.conn()?;
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
        let conn = self.conn()?;
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
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO audit
                (revision, action, detail, actor_role, actor_tenant, actor_project, source, object_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.revision as i64,
                entry.action,
                entry.detail,
                entry.actor_role,
                entry.actor_tenant,
                entry.actor_project,
                entry.source,
                entry.object_id,
                entry.created_at_ms
            ],
        )?;
        Ok(())
    }

    fn list_audit(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT revision, action, detail, actor_role, actor_tenant, actor_project,
                    COALESCE(source, action), object_id, created_at
             FROM audit ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                Ok(AuditEntry {
                    revision: row.get::<_, i64>(0)? as u64,
                    action: row.get(1)?,
                    detail: row.get(2)?,
                    actor_role: row.get(3)?,
                    actor_tenant: row.get(4)?,
                    actor_project: row.get(5)?,
                    source: row.get(6)?,
                    object_id: row.get(7)?,
                    created_at_ms: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn record_usage(&self, e: &UsageEvent) -> Result<()> {
        let conn = self.conn()?;
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
        let conn = self.conn()?;
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
        let conn = self.conn()?;
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
        let conn = self.conn()?;
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
        let conn = self.conn()?;
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

    fn idempotency_begin(
        &self,
        key: &str,
        fingerprint: &str,
        ttl_ms: u64,
    ) -> Result<IdempotencyBegin> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_millis();
        let expires = now.saturating_add(ttl_ms as i64);
        tx.execute(
            "DELETE FROM idempotency_inflight WHERE expires_at <= ?1",
            [now],
        )?;

        let existing = {
            let mut stmt = tx.prepare(
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
                Some(rec) => Some(rec?),
                None => None,
            }
        };
        if let Some(rec) = existing {
            let out = if rec.fingerprint == fingerprint {
                IdempotencyBegin::Replay(rec)
            } else {
                IdempotencyBegin::Mismatch
            };
            tx.commit()?;
            return Ok(out);
        }

        let inflight_fingerprint = {
            let mut stmt =
                tx.prepare("SELECT fingerprint FROM idempotency_inflight WHERE key = ?1")?;
            let mut rows = stmt.query_map([key], |row| row.get::<_, String>(0))?;
            match rows.next() {
                Some(fp) => Some(fp?),
                None => None,
            }
        };
        if let Some(fp) = inflight_fingerprint {
            tx.commit()?;
            return Ok(if fp == fingerprint {
                IdempotencyBegin::InProgress
            } else {
                IdempotencyBegin::Mismatch
            });
        }

        tx.execute(
            "INSERT INTO idempotency_inflight (key, fingerprint, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![key, fingerprint, now, expires],
        )?;
        tx.commit()?;
        Ok(IdempotencyBegin::Claimed)
    }

    fn idempotency_release(&self, key: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM idempotency_inflight WHERE key = ?1", [key])?;
        Ok(())
    }

    fn tenant_slot_acquire(
        &self,
        tenant: &str,
        slot_id: &str,
        max: u32,
        ttl_ms: u64,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_millis();
        let expires = now.saturating_add(ttl_ms as i64);
        tx.execute("DELETE FROM tenant_slots WHERE expires_at <= ?1", [now])?;
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM tenant_slots WHERE tenant = ?1",
            [tenant],
            |row| row.get(0),
        )?;
        if active >= max as i64 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO tenant_slots (slot_id, tenant, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![slot_id, tenant, now, expires],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn tenant_slot_release(&self, slot_id: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM tenant_slots WHERE slot_id = ?1", [slot_id])?;
        Ok(())
    }

    fn tenant_slot_count(&self, tenant: &str) -> Result<u32> {
        let conn = self.conn()?;
        let now = now_millis();
        conn.execute("DELETE FROM tenant_slots WHERE expires_at <= ?1", [now])?;
        let active: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tenant_slots WHERE tenant = ?1",
            [tenant],
            |row| row.get(0),
        )?;
        Ok(active as u32)
    }

    fn put_draft(&self, rec: &DraftRecord) -> Result<()> {
        let conn = self.conn()?;
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
        let conn = self.conn()?;
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
        let conn = self.conn()?;
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
        let conn = self.conn()?;
        conn.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_versioned() {
        let store = SqliteStore::in_memory().unwrap();

        assert_eq!(store.schema_versions().unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn migrations_upgrade_legacy_usage_table_without_tenant_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE revisions (
                 revision    INTEGER PRIMARY KEY,
                 config_hash TEXT    NOT NULL,
                 source      TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL
             );
             CREATE TABLE audit (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 revision    INTEGER NOT NULL,
                 action      TEXT    NOT NULL,
                 detail      TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL
             );
             CREATE TABLE usage (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 request_id    TEXT    NOT NULL,
                 provider_id   TEXT    NOT NULL,
                 model         TEXT    NOT NULL,
                 account_id    TEXT,
                 cost_micros   INTEGER NOT NULL,
                 input_tokens  INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 latency_ms    INTEGER NOT NULL,
                 streamed      INTEGER NOT NULL,
                 created_at    INTEGER NOT NULL
             );
             CREATE TABLE idempotency (
                 key          TEXT    PRIMARY KEY,
                 fingerprint  TEXT    NOT NULL,
                 status       INTEGER NOT NULL,
                 content_type TEXT    NOT NULL,
                 body         TEXT    NOT NULL,
                 created_at   INTEGER NOT NULL
             );
             CREATE TABLE drafts (
                 id            TEXT    PRIMARY KEY,
                 config_json   TEXT    NOT NULL,
                 base_revision INTEGER NOT NULL,
                 created_at    INTEGER NOT NULL
             );",
        )
        .unwrap();
        let store = SqliteStore {
            conn: Mutex::new(conn),
        };

        store.migrate().unwrap();

        assert_eq!(store.schema_versions().unwrap(), vec![1, 2, 3, 4]);
        let conn = store.conn.lock().unwrap();
        assert!(SqliteStore::column_exists(&conn, "usage", "tenant").unwrap());
        assert!(SqliteStore::column_exists(&conn, "audit", "source").unwrap());
    }

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
                actor_role: Some("admin".into()),
                actor_tenant: None,
                actor_project: None,
                source: "bootstrap".into(),
                object_id: Some("config/x.yaml".into()),
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
        assert_eq!(audit[0].source, "bootstrap");
        assert_eq!(audit[0].actor_role.as_deref(), Some("admin"));
        assert_eq!(audit[0].object_id.as_deref(), Some("config/x.yaml"));
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

    #[test]
    fn idempotency_begin_coordinates_inflight_and_replay() {
        let store = SqliteStore::in_memory().unwrap();

        assert_eq!(
            store.idempotency_begin("k1", "fp", 60_000).unwrap(),
            IdempotencyBegin::Claimed
        );
        assert_eq!(
            store.idempotency_begin("k1", "fp", 60_000).unwrap(),
            IdempotencyBegin::InProgress
        );
        assert_eq!(
            store.idempotency_begin("k1", "different", 60_000).unwrap(),
            IdempotencyBegin::Mismatch
        );

        store.idempotency_release("k1").unwrap();
        assert_eq!(
            store.idempotency_begin("k1", "fp", 60_000).unwrap(),
            IdempotencyBegin::Claimed
        );
        store.idempotency_release("k1").unwrap();

        let rec = IdempotencyRecord {
            key: "k1".into(),
            fingerprint: "fp".into(),
            status: 200,
            content_type: "application/json".into(),
            body: "{\"ok\":true}".into(),
            created_at_ms: 1,
        };
        assert!(store.idempotency_put(&rec).unwrap());
        assert_eq!(
            store.idempotency_begin("k1", "fp", 60_000).unwrap(),
            IdempotencyBegin::Replay(rec)
        );
        assert_eq!(
            store.idempotency_begin("k1", "different", 60_000).unwrap(),
            IdempotencyBegin::Mismatch
        );
    }

    #[test]
    fn idempotency_begin_expires_abandoned_claims() {
        let store = SqliteStore::in_memory().unwrap();

        assert_eq!(
            store.idempotency_begin("k1", "fp", 0).unwrap(),
            IdempotencyBegin::Claimed
        );
        assert_eq!(
            store.idempotency_begin("k1", "fp", 60_000).unwrap(),
            IdempotencyBegin::Claimed,
            "expired in-flight claim should not block a new process forever"
        );
    }

    #[test]
    fn tenant_slots_enforce_limit_and_release() {
        let store = SqliteStore::in_memory().unwrap();

        assert!(store
            .tenant_slot_acquire("acme", "slot-1", 1, 60_000)
            .unwrap());
        assert_eq!(store.tenant_slot_count("acme").unwrap(), 1);
        assert!(
            !store
                .tenant_slot_acquire("acme", "slot-2", 1, 60_000)
                .unwrap(),
            "second active slot is over the tenant max"
        );
        assert!(store
            .tenant_slot_acquire("globex", "slot-3", 1, 60_000)
            .unwrap());

        store.tenant_slot_release("slot-1").unwrap();
        assert_eq!(store.tenant_slot_count("acme").unwrap(), 0);
        assert!(store
            .tenant_slot_acquire("acme", "slot-4", 1, 60_000)
            .unwrap());
    }

    #[test]
    fn tenant_slots_expire_abandoned_rows() {
        let store = SqliteStore::in_memory().unwrap();

        assert!(store.tenant_slot_acquire("acme", "slot-1", 1, 0).unwrap());
        assert_eq!(
            store.tenant_slot_count("acme").unwrap(),
            0,
            "expired slot should be cleaned before counting"
        );
        assert!(store
            .tenant_slot_acquire("acme", "slot-2", 1, 60_000)
            .unwrap());
    }
}
