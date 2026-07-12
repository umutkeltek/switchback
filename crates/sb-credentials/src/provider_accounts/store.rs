use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags};

use super::*;

pub(crate) struct AuthorityStore {
    path: PathBuf,
    read_only: bool,
}
impl AuthorityStore {
    pub fn new(path: PathBuf, read_only: bool) -> Result<Self, AuthorityError> {
        let store = Self { path, read_only };
        if !read_only {
            store.ensure()?
        }
        Ok(store)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn exists_with_revision(&self) -> Result<bool, AuthorityError> {
        if !self.path.exists() {
            return Ok(false);
        }
        Ok(self.current_revision()?.0 > 0)
    }
    fn ensure(&self) -> Result<(), AuthorityError> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| AuthorityError::Store("database has no parent".into()))?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let conn = Connection::open(&self.path)?;
        schema(&conn)?;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    fn open(&self) -> Result<Connection, AuthorityError> {
        if self.read_only {
            if !self.path.exists() {
                return Err(AuthorityError::Absent);
            }
            Ok(Connection::open_with_flags(
                &self.path,
                OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?)
        } else {
            Ok(Connection::open(&self.path)?)
        }
    }
    pub fn current_revision(&self) -> Result<(u64, Option<String>), AuthorityError> {
        if !self.path.exists() {
            return Ok((0, None));
        }
        let conn = self.open()?;
        let row=conn.query_row("SELECT revision,input_digest FROM provider_account_revisions ORDER BY revision DESC LIMIT 1",[],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,String>(1)?)));
        match row {
            Ok(v) => Ok((v.0, Some(v.1))),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((0, None)),
            Err(e) if sqlite_missing_schema(&e) => Ok((0, None)),
            Err(e) => Err(e.into()),
        }
    }
    pub fn snapshot(&self) -> Result<ProviderAccountSnapshot, AuthorityError> {
        if !self.path.exists() {
            return Err(AuthorityError::Absent);
        }
        let conn = self.open()?;
        let json=conn.query_row("SELECT snapshot_json FROM provider_account_revisions ORDER BY revision DESC LIMIT 1",[],|r|r.get::<_,String>(0)).map_err(|e|if matches!(e,rusqlite::Error::QueryReturnedNoRows)||sqlite_missing_schema(&e){AuthorityError::Absent}else{e.into()})?;
        Ok(serde_json::from_str(&json)?)
    }
    pub fn apply(
        &self,
        plan: &crate::provider_accounts::reconcile::ReconcilePlan,
        audit_kind: &str,
    ) -> Result<(), AuthorityError> {
        if self.read_only {
            return Err(AuthorityError::ReadOnly);
        }
        let mut conn = self.open()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<u64> = tx.query_row(
            "SELECT MAX(revision) FROM provider_account_revisions",
            [],
            |r| r.get(0),
        )?;
        if current.unwrap_or(0) != plan.base_revision {
            return Err(AuthorityError::RevisionConflict);
        }
        let snapshot_json = serde_json::to_string(&plan.snapshot)?;
        tx.execute("INSERT INTO provider_account_revisions(revision,input_digest,normalization_version,policy_version,created_at_ms,snapshot_json) VALUES(?,?,?,?,?,?)",params![plan.snapshot.revision,plan.digest,NORMALIZATION_VERSION,POLICY_VERSION,plan.now_ms,snapshot_json])?;
        write_projection(&tx, &plan.snapshot)?;
        tx.execute(
            "INSERT INTO provider_account_audit(revision,event_kind,created_at_ms) VALUES(?,?,?)",
            params![plan.snapshot.revision, audit_kind, plan.now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }
}

fn sqlite_missing_schema(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::SqliteFailure(_, Some(message)) if message.contains("no such table"))
}

fn schema(conn: &Connection) -> Result<(), AuthorityError> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS provider_account_revisions(revision INTEGER PRIMARY KEY,input_digest TEXT NOT NULL,normalization_version TEXT NOT NULL,policy_version TEXT NOT NULL,created_at_ms INTEGER NOT NULL,snapshot_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS provider_account_enrollments(account_id TEXT PRIMARY KEY,provider TEXT NOT NULL,state TEXT NOT NULL,updated_revision INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS provider_account_aliases(account_id TEXT NOT NULL,scheme TEXT NOT NULL,normalized_value TEXT NOT NULL,source TEXT NOT NULL,source_record_key TEXT NOT NULL,binding_state TEXT NOT NULL,PRIMARY KEY(account_id,scheme,normalized_value,source,source_record_key));
CREATE TABLE IF NOT EXISTS provider_account_credentials(account_id TEXT NOT NULL,pointer_json TEXT NOT NULL,readiness TEXT NOT NULL,PRIMARY KEY(account_id,pointer_json));
CREATE TABLE IF NOT EXISTS provider_account_active_clients(client TEXT PRIMARY KEY,state_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS provider_account_capacity_windows(account_id TEXT NOT NULL,sample_key TEXT NOT NULL,state_json TEXT NOT NULL,PRIMARY KEY(account_id,sample_key));
CREATE TABLE IF NOT EXISTS provider_account_sources(source TEXT PRIMARY KEY,status_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS provider_account_observations(idempotency_key TEXT PRIMARY KEY,revision INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS provider_account_audit(id INTEGER PRIMARY KEY AUTOINCREMENT,revision INTEGER NOT NULL,event_kind TEXT NOT NULL,created_at_ms INTEGER NOT NULL);")?;
    Ok(())
}
fn write_projection(
    tx: &rusqlite::Transaction<'_>,
    s: &ProviderAccountSnapshot,
) -> Result<(), AuthorityError> {
    for table in [
        "provider_account_enrollments",
        "provider_account_aliases",
        "provider_account_credentials",
        "provider_account_active_clients",
        "provider_account_capacity_windows",
        "provider_account_sources",
    ] {
        tx.execute(&format!("DELETE FROM {table}"), [])?;
    }
    for a in &s.accounts {
        tx.execute(
            "INSERT INTO provider_account_enrollments VALUES(?,?,?,?)",
            params![
                a.id.0,
                a.provider.0,
                format!("{:?}", a.state),
                a.updated_revision
            ],
        )?;
        for b in &a.aliases {
            tx.execute(
                "INSERT INTO provider_account_aliases VALUES(?,?,?,?,?,?)",
                params![
                    a.id.0,
                    b.scheme.as_str(),
                    b.normalized_value,
                    format!("{:?}", b.source),
                    b.source_record_key,
                    format!("{:?}", b.binding_state)
                ],
            )?;
            let key = crate::provider_accounts::reconcile::observation_key(
                b.source,
                &b.source_record_key,
                &b.normalized_value,
            );
            tx.execute(
                "INSERT OR IGNORE INTO provider_account_observations VALUES(?,?)",
                params![key, s.revision],
            )?;
        }
    }
    for c in &s.credentials {
        let p = serde_json::to_string(&c.pointer)?;
        tx.execute(
            "INSERT INTO provider_account_credentials VALUES(?,?,?)",
            params![c.account_id.0, p, format!("{:?}", c.readiness)],
        )?;
    }
    for a in &s.active_clients {
        tx.execute(
            "INSERT INTO provider_account_active_clients VALUES(?,?)",
            params![format!("{:?}", a.client), serde_json::to_string(a)?],
        )?;
    }
    for (i, c) in s.capacity.iter().enumerate() {
        tx.execute(
            "INSERT INTO provider_account_capacity_windows VALUES(?,?,?)",
            params![
                c.account_id.0,
                format!("{}:{}", c.sampled_at_ms, i),
                serde_json::to_string(c)?
            ],
        )?;
    }
    for source in &s.sources {
        tx.execute(
            "INSERT INTO provider_account_sources VALUES(?,?)",
            params![
                format!("{:?}", source.source),
                serde_json::to_string(source)?
            ],
        )?;
    }
    Ok(())
}
