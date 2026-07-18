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

#[cfg(feature = "eval")]
use rusqlite::OptionalExtension;
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
#[cfg(feature = "eval")]
use sha2::{Digest, Sha256};

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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UsageEvent {
    pub request_id: String,
    pub provider_id: String,
    pub model: String,
    pub account_id: Option<String>,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    pub cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units_consumed: Option<f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Provider-served cached-prefix input tokens (Anthropic
    /// `cache_read_input_tokens` / Gemini `cachedContentTokenCount` / OpenAI
    /// `cached_tokens`). Additive column added after launch: rows written before
    /// the upgrade read back as 0, so any realized-savings attribution derived
    /// from pre-upgrade history is 0 (acceptable — those rows are not re-priced).
    #[serde(default)]
    pub cached_input_tokens: u64,
    pub latency_ms: u64,
    pub streamed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_joules: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_kwh: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_duration_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_measurement_available: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_attribution_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_kwh_consumed: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_kwh_charged: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_accounting_method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub energy_total_cost_usd: Option<f64>,
    pub created_at_ms: i64,
}

/// One metadata-only request trace, durably recorded for searchable execution
/// observability. `trace_json` is the serialized `sb_trace::TraceRecord` shape,
/// but `sb-store` deliberately keeps it opaque so the store crate stays free of
/// Switchback crate dependencies.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TraceEvent {
    pub request_id: String,
    pub revision: u64,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    pub inbound_model: String,
    pub route: String,
    #[serde(default)]
    pub selected_target: Option<String>,
    pub final_status: u16,
    pub total_latency_ms: u64,
    pub streamed: bool,
    pub cost_micros: u64,
    #[serde(default)]
    pub attempted_providers: Vec<String>,
    pub created_at_ms: i64,
    pub trace_json: String,
}

/// One metadata-only native-client history import run. This records only source
/// counts, byte counts, time ranges, and policy flags; it never stores prompt,
/// response, tool-call, token, or credential material.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NativeHistoryImportRecord {
    pub import_id: String,
    pub client_filter: String,
    pub metadata_only: bool,
    pub stores_prompts: bool,
    pub stores_responses: bool,
    pub stores_local_paths: bool,
    pub source_count: u64,
    pub existing_source_count: u64,
    pub file_count: u64,
    pub record_count: u64,
    pub parse_error_count: u64,
    pub byte_count: u64,
    pub warnings_json: String,
    pub created_at_ms: i64,
}

/// One metadata-only source snapshot captured as part of a native history
/// import. `path_id` is a stable redacted id; exact local paths stay out of the
/// durable store even when the CLI display opts into showing them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NativeHistorySourceRecord {
    pub import_id: String,
    pub source_id: String,
    pub client: String,
    pub kind: String,
    pub parser: String,
    pub path_pattern: String,
    pub path_id: String,
    pub exists: bool,
    pub truncated: bool,
    pub skipped_file_count: u64,
    pub file_count: u64,
    pub record_count: u64,
    pub parse_error_count: u64,
    pub byte_count: u64,
    pub modified_at_ms_min: Option<i64>,
    pub modified_at_ms_max: Option<i64>,
    pub observed_at_min: Option<String>,
    pub observed_at_max: Option<String>,
    pub tables_json: String,
    pub errors_json: String,
}

/// A native history import batch written atomically.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NativeHistoryImportBatch {
    pub import: NativeHistoryImportRecord,
    pub sources: Vec<NativeHistorySourceRecord>,
}

/// Outcome of a native history import write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeHistoryImportWrite {
    pub source_rows_written: u64,
}

#[cfg(feature = "eval")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct EvalEvidenceSnapshotRecord {
    pub name: String,
    pub snapshot_id: String,
    pub schema_version: String,
    pub snapshot_sha256: String,
    pub generated_at_ms: u64,
    pub published_at_ms: i64,
}

/// Filter for recent trace queries. Every field is optional and ANDed together.
#[derive(Debug, Clone, Default)]
pub struct TraceQuery {
    pub limit: usize,
    pub tenant: Option<String>,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub status: Option<u16>,
    pub since_ms: Option<i64>,
}

/// Outcome of an idempotent durable usage write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageWriteOutcome {
    Inserted,
    DuplicateIgnored,
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

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageEnergyRollup {
    pub requests_with_energy: u64,
    pub energy_joules: f64,
    pub energy_kwh: f64,
    pub duration_seconds: f64,
    pub energy_kwh_consumed: f64,
    pub energy_kwh_charged: f64,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageEnergyBucket {
    pub key: String,
    pub energy: UsageEnergyRollup,
}

/// Aggregated usage across all durably-recorded events: totals + per-provider and
/// per-model buckets. Computed in SQL so the hot path never scans rows.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct UsageRollup {
    pub requests: u64,
    pub total_cost_micros: u64,
    pub unknown_cost_requests: u64,
    pub by_provider: Vec<UsageBucket>,
    pub by_model: Vec<UsageBucket>,
    pub by_tenant: Vec<UsageBucket>,
    pub energy: UsageEnergyRollup,
    pub energy_by_provider: Vec<UsageEnergyBucket>,
    pub energy_by_model: Vec<UsageEnergyBucket>,
    pub energy_by_tenant: Vec<UsageEnergyBucket>,
}

/// One persisted outcome-scorecard aggregate row, keyed by `(target_id,
/// class)` — the router's per-target rolling-outcome evidence (registry facts
/// are priors; this is the posterior). Primitives only: `sb-store` has zero
/// `sb-*` dependencies, so `class` and `error_histogram` are opaque strings
/// (canonical values/JSON owned by `sb-runtime`'s scorecard module) rather
/// than typed enums, and `tier` is a plain `0 = Healthy` / `1 = Demoted` code.
/// Quality fields belong to the live-traffic evaluation sidecar; bodies never
/// cross this persistence seam.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScorecardRow {
    pub target_id: String,
    pub class: String,
    pub scoreable_samples: u32,
    pub success_count: u32,
    pub truncated_count: u32,
    pub target_fail_count: u32,
    pub p50_latency_ms: u32,
    pub p95_latency_ms: u32,
    pub cost_per_success_micros: u64,
    /// Opaque JSON object, keys = `ErrorClass::as_str()`.
    pub error_histogram: String,
    pub consecutive_failures: u32,
    /// `0` = Healthy, `1` = Demoted.
    pub tier: u8,
    pub demoted_since_ms: Option<i64>,
    /// Rolling response-quality EWMA for the current evaluator calibration.
    pub quality_ewma: Option<f64>,
    pub quality_samples: u32,
    pub quality_updated_at_ms: Option<i64>,
    pub quality_evaluator_id: Option<String>,
    pub updated_at_ms: i64,
    pub schema_ver: u32,
}

/// Metadata-only reservation for one live-traffic quality judgment. Request
/// and response bodies deliberately have no representation in this type or in
/// the corresponding SQLite table.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QualityJudgmentReservation {
    pub judgment_id: String,
    pub judge_request_id: String,
    pub served_request_id: String,
    pub served_target_id: String,
    pub class: String,
    pub sample_revision: u64,
    pub judge_revision: u64,
    pub evaluator_id: String,
    pub rubric_version: String,
    pub judge_target_id: Option<String>,
    pub input_chars: u32,
    pub output_chars: u32,
    pub reserved_cost_micros: u64,
    pub created_at_ms: i64,
}

/// Terminal metadata for a previously reserved quality judgment. Only a
/// scored judgment may carry `score_norm`; `reason_code` is a bounded enum-like
/// identifier, never free-form judge output.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QualityJudgmentFinalization {
    pub judgment_id: String,
    pub judge_target_id: Option<String>,
    pub status: String,
    pub score_norm: Option<f64>,
    pub reason_code: Option<String>,
    pub actual_cost_micros: Option<u64>,
    pub completed_at_ms: i64,
}

/// One metadata-only quality judgment audit/WAL row.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QualityJudgmentRecord {
    pub judgment_id: String,
    pub judge_request_id: String,
    pub served_request_id: String,
    pub served_target_id: String,
    pub class: String,
    pub sample_revision: u64,
    pub judge_revision: u64,
    pub evaluator_id: String,
    pub rubric_version: String,
    pub judge_target_id: Option<String>,
    pub status: String,
    pub score_norm: Option<f64>,
    pub reason_code: Option<String>,
    pub input_chars: u32,
    pub output_chars: u32,
    pub reserved_cost_micros: u64,
    pub actual_cost_micros: Option<u64>,
    pub created_at_ms: i64,
    pub completed_at_ms: Option<i64>,
}

/// Rolling-window usage charged to live quality evaluation. Cost is
/// conservative: every row contributes at least its reservation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QualityJudgmentBudget {
    pub attempted: u64,
    pub cost_micros: u64,
}

/// Result of atomically checking both rolling caps and inserting a reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityJudgmentReserveOutcome {
    Reserved,
    Duplicate,
    BudgetExceeded(QualityJudgmentBudget),
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
    /// Durably record one usage event. `request_id` is idempotent:
    /// first writer wins and duplicate writes leave the original event intact.
    fn record_usage(&self, event: &UsageEvent) -> Result<UsageWriteOutcome>;
    /// Aggregate all durably-recorded usage (totals + by-provider + by-model).
    fn usage_rollup(&self) -> Result<UsageRollup>;
    /// The most recent `limit` usage events (newest first).
    fn recent_usage(&self, limit: usize) -> Result<Vec<UsageEvent>>;
    /// Durably record one metadata-only trace. `request_id` is idempotent:
    /// first writer wins and duplicate writes leave the original event intact.
    fn record_trace(&self, _event: &TraceEvent) -> Result<bool> {
        Err(StoreError(
            "durable trace metadata is not supported".to_string(),
        ))
    }
    /// Query recent metadata-only traces.
    fn query_traces(&self, _query: &TraceQuery) -> Result<Vec<TraceEvent>> {
        Err(StoreError(
            "durable trace metadata is not supported".to_string(),
        ))
    }
    /// Fetch one metadata-only trace by request id.
    fn get_trace(&self, _request_id: &str) -> Result<Option<TraceEvent>> {
        Err(StoreError(
            "durable trace metadata is not supported".to_string(),
        ))
    }
    /// Durably record one metadata-only native-client history import batch.
    fn record_native_history_import(
        &self,
        _batch: &NativeHistoryImportBatch,
    ) -> Result<NativeHistoryImportWrite> {
        Err(StoreError(
            "durable native history import metadata is not supported".to_string(),
        ))
    }
    /// Recent native-client history import runs, newest first.
    fn recent_native_history_imports(
        &self,
        _limit: usize,
    ) -> Result<Vec<NativeHistoryImportRecord>> {
        Err(StoreError(
            "durable native history import metadata is not supported".to_string(),
        ))
    }
    /// Source snapshots for one native-client history import.
    fn native_history_sources(&self, _import_id: &str) -> Result<Vec<NativeHistorySourceRecord>> {
        Err(StoreError(
            "durable native history import metadata is not supported".to_string(),
        ))
    }
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
        _lease_id: &str,
        _ttl_ms: u64,
    ) -> Result<IdempotencyBegin> {
        Err(StoreError(
            "idempotency in-flight coordination is not supported".to_string(),
        ))
    }
    /// Release an in-flight idempotency claim after the request has completed.
    fn idempotency_release(&self, _key: &str, _lease_id: &str) -> Result<bool> {
        Ok(false)
    }
    /// Extend an active in-flight idempotency claim. Returns `true` when the
    /// claim still exists and was renewed, `false` when it is already gone or
    /// expired. Backends should not revive expired leases.
    fn idempotency_renew(&self, _key: &str, _lease_id: &str, _ttl_ms: u64) -> Result<bool> {
        Err(StoreError(
            "idempotency in-flight renewal is not supported".to_string(),
        ))
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
    /// Extend an active tenant concurrency slot. Returns `true` when the slot
    /// still exists and was renewed, `false` when it is already gone or expired.
    fn tenant_slot_renew(&self, _slot_id: &str, _ttl_ms: u64) -> Result<bool> {
        Err(StoreError(
            "tenant concurrency renewal is not supported".to_string(),
        ))
    }
    /// Count active tenant concurrency slots after expiring abandoned rows.
    fn tenant_slot_count(&self, _tenant: &str) -> Result<u32> {
        Ok(0)
    }
    /// Atomically acquire one global admission slot. Returns `true` if the slot
    /// was acquired, `false` if the gateway is already at `max`.
    fn admission_slot_acquire(&self, _slot_id: &str, _max: u32, _ttl_ms: u64) -> Result<bool> {
        Err(StoreError(
            "global admission coordination is not supported".to_string(),
        ))
    }
    /// Release one global admission slot.
    fn admission_slot_release(&self, _slot_id: &str) -> Result<()> {
        Ok(())
    }
    /// Extend an active global admission slot. Returns `true` when the slot
    /// still exists and was renewed, `false` when it is already gone or expired.
    fn admission_slot_renew(&self, _slot_id: &str, _ttl_ms: u64) -> Result<bool> {
        Err(StoreError(
            "global admission renewal is not supported".to_string(),
        ))
    }
    /// Count active global admission slots after expiring abandoned rows.
    fn admission_slot_count(&self) -> Result<u32> {
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
    /// Upsert outcome-scorecard aggregate rows, one transaction, keyed by
    /// `(target_id, class)` — a repeated key overwrites the previous row.
    /// Dormant until `sb-runtime`'s background flusher calls it; backends
    /// that don't persist scorecards may leave this unsupported (fail-open —
    /// the in-memory projection just never hydrates a prior).
    fn upsert_scorecard(&self, _rows: &[ScorecardRow]) -> Result<()> {
        Err(StoreError(
            "outcome scorecard persistence is not supported".to_string(),
        ))
    }
    /// Load all persisted outcome-scorecard aggregate rows (startup hydrate).
    fn load_scorecard(&self) -> Result<Vec<ScorecardRow>> {
        Err(StoreError(
            "outcome scorecard persistence is not supported".to_string(),
        ))
    }
    /// Atomically enforce the rolling count + cost caps and reserve one judge
    /// call. A duplicate judgment/request id is idempotently ignored.
    fn reserve_quality_judgment(
        &self,
        _reservation: &QualityJudgmentReservation,
        _max_judgments: u64,
        _max_cost_micros: u64,
        _since_ms: i64,
    ) -> Result<QualityJudgmentReserveOutcome> {
        Err(StoreError(
            "quality judgment persistence is not supported".to_string(),
        ))
    }
    /// Finalize a started judgment. Returns false when it was already terminal
    /// or is unknown, making repeated completion safe.
    fn finalize_quality_judgment(
        &self,
        _finalization: &QualityJudgmentFinalization,
    ) -> Result<bool> {
        Err(StoreError(
            "quality judgment persistence is not supported".to_string(),
        ))
    }
    /// Replay scored rows for one evaluator calibration, oldest first.
    fn replay_quality_judgments(
        &self,
        _evaluator_id: &str,
        _since_ms: i64,
    ) -> Result<Vec<QualityJudgmentRecord>> {
        Err(StoreError(
            "quality judgment persistence is not supported".to_string(),
        ))
    }
    /// Mark all startup-orphaned reservations abandoned while retaining their
    /// conservative budget charge.
    fn abandon_started_quality_judgments(&self, _completed_at_ms: i64) -> Result<u64> {
        Err(StoreError(
            "quality judgment persistence is not supported".to_string(),
        ))
    }
    /// Query recent audit rows, newest first.
    fn recent_quality_judgments(&self, _limit: usize) -> Result<Vec<QualityJudgmentRecord>> {
        Err(StoreError(
            "quality judgment persistence is not supported".to_string(),
        ))
    }
    /// Read rolling-window count and conservative reserved/actual cost.
    fn quality_judgment_budget(&self, _since_ms: i64) -> Result<QualityJudgmentBudget> {
        Err(StoreError(
            "quality judgment persistence is not supported".to_string(),
        ))
    }
}

/// SQLite-backed store (bundled SQLite — no system dependency). One connection
/// guarded by a `Mutex`. The store is on the hot path now (a usage write plus
/// admission/idempotency/tenant lease checks per request, not just one write per
/// config publish), so the file backend runs in **WAL mode**: across processes
/// — the multi-gateway coordination story — readers no longer block the writer
/// and vice versa (the default rollback journal takes a full-database lock), so
/// `/v1/usage` rollups and lease GC don't serialize behind every write. In a
/// single process the `Mutex` still serializes access; a read connection pool is
/// the next step if in-process read throughput becomes the bottleneck.
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open (or create) a SQLite file and run migrations.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // WAL is a persistent database setting; synchronous=NORMAL is its durable
        // pairing (only the last committed transaction is at risk on power loss,
        // never corruption). In-memory databases don't support WAL, so this lives
        // here rather than in the shared `migrate()`.
        let journal_mode: String =
            conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            tracing::warn!(
                %journal_mode,
                "sqlite WAL unavailable on this path; continuing on the default journal"
            );
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL;")?;
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
                     cost_known    INTEGER NOT NULL DEFAULT 1,
                     workload_kind TEXT,
                     pricing_unit  TEXT,
                     units_consumed REAL,
                     input_tokens  INTEGER NOT NULL,
                     output_tokens INTEGER NOT NULL,
                     cached_input_tokens INTEGER,
                     latency_ms    INTEGER NOT NULL,
                     streamed      INTEGER NOT NULL,
                     energy_joules REAL,
                     energy_kwh REAL,
                     energy_duration_seconds REAL,
                     energy_measurement_available INTEGER,
                     energy_attribution_method TEXT,
                     energy_kwh_consumed REAL,
                     energy_kwh_charged REAL,
                     energy_accounting_method TEXT,
                     energy_total_cost_usd REAL,
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
                     lease_id    TEXT,
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
        Self::apply_migration(&mut conn, 5, "global_admission_leases", |tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS admission_slots (
                     slot_id    TEXT    PRIMARY KEY,
                     created_at INTEGER NOT NULL,
                     expires_at INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS admission_slots_expires
                   ON admission_slots(expires_at);",
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 6, "idempotency_inflight_lease_owner", |tx| {
            if !Self::column_exists(tx, "idempotency_inflight", "lease_id")? {
                tx.execute(
                    "ALTER TABLE idempotency_inflight ADD COLUMN lease_id TEXT",
                    [],
                )?;
            }
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 7, "usage_request_id_unique", |tx| {
            tx.execute(
                "DELETE FROM usage
                 WHERE id NOT IN (
                     SELECT MIN(id) FROM usage GROUP BY request_id
                 )",
                [],
            )?;
            tx.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS usage_request_id_unique
                 ON usage(request_id)",
                [],
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 8, "trace_events", |tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS trace_events (
                     id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                     request_id          TEXT    NOT NULL,
                     revision            INTEGER NOT NULL,
                     tenant              TEXT,
                     project             TEXT,
                     session_id          TEXT,
                     inbound_model       TEXT    NOT NULL,
                     route               TEXT    NOT NULL,
                     selected_target     TEXT,
                     final_status        INTEGER NOT NULL,
                     total_latency_ms    INTEGER NOT NULL,
                     streamed            INTEGER NOT NULL,
                     cost_micros         INTEGER NOT NULL,
                     attempted_providers TEXT    NOT NULL,
                     trace_json          TEXT    NOT NULL,
                     created_at          INTEGER NOT NULL
                 );
                 CREATE UNIQUE INDEX IF NOT EXISTS trace_events_request_id_unique
                   ON trace_events(request_id);
                 CREATE INDEX IF NOT EXISTS trace_events_by_time
                   ON trace_events(created_at);
                 CREATE INDEX IF NOT EXISTS trace_events_by_session
                   ON trace_events(session_id, created_at);
                 CREATE INDEX IF NOT EXISTS trace_events_by_tenant
                   ON trace_events(tenant, created_at);
                 CREATE INDEX IF NOT EXISTS trace_events_by_model
                   ON trace_events(inbound_model, created_at);
                 CREATE INDEX IF NOT EXISTS trace_events_by_status
                   ON trace_events(final_status, created_at);",
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 9, "usage_project_attribution", |tx| {
            if !Self::column_exists(tx, "usage", "project")? {
                tx.execute("ALTER TABLE usage ADD COLUMN project TEXT", [])?;
            }
            tx.execute(
                "CREATE INDEX IF NOT EXISTS usage_by_project ON usage(project)",
                [],
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 10, "native_history_imports", |tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS native_history_imports (
                     import_id             TEXT    PRIMARY KEY,
                     client_filter         TEXT    NOT NULL,
                     metadata_only         INTEGER NOT NULL,
                     stores_prompts        INTEGER NOT NULL,
                     stores_responses      INTEGER NOT NULL,
                     stores_local_paths    INTEGER NOT NULL,
                     source_count          INTEGER NOT NULL,
                     existing_source_count INTEGER NOT NULL,
                     file_count            INTEGER NOT NULL,
                     record_count          INTEGER NOT NULL,
                     parse_error_count     INTEGER NOT NULL,
                     byte_count            INTEGER NOT NULL,
                     warnings_json         TEXT    NOT NULL,
                     created_at            INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS native_history_imports_by_time
                   ON native_history_imports(created_at);
                 CREATE TABLE IF NOT EXISTS native_history_sources (
                     id                    INTEGER PRIMARY KEY AUTOINCREMENT,
                     import_id             TEXT    NOT NULL,
                     source_id             TEXT    NOT NULL,
                     client                TEXT    NOT NULL,
                     kind                  TEXT    NOT NULL,
                     parser                TEXT    NOT NULL,
                     path_pattern          TEXT    NOT NULL,
                     path_id               TEXT    NOT NULL,
                     source_exists         INTEGER NOT NULL,
                     truncated             INTEGER NOT NULL,
                     skipped_file_count    INTEGER NOT NULL,
                     file_count            INTEGER NOT NULL,
                     record_count          INTEGER NOT NULL,
                     parse_error_count     INTEGER NOT NULL,
                     byte_count            INTEGER NOT NULL,
                     modified_at_ms_min    INTEGER,
                     modified_at_ms_max    INTEGER,
                     observed_at_min       TEXT,
                     observed_at_max       TEXT,
                     tables_json           TEXT    NOT NULL,
                     errors_json           TEXT    NOT NULL,
                     FOREIGN KEY(import_id) REFERENCES native_history_imports(import_id)
                       ON DELETE CASCADE
                 );
                 CREATE UNIQUE INDEX IF NOT EXISTS native_history_sources_import_source
                   ON native_history_sources(import_id, source_id);
                 CREATE INDEX IF NOT EXISTS native_history_sources_by_client
                   ON native_history_sources(client, source_id);",
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 11, "usage_energy_accounting", |tx| {
            for (column, ty) in [
                ("energy_joules", "REAL"),
                ("energy_kwh", "REAL"),
                ("energy_duration_seconds", "REAL"),
                ("energy_measurement_available", "INTEGER"),
                ("energy_attribution_method", "TEXT"),
                ("energy_kwh_consumed", "REAL"),
                ("energy_kwh_charged", "REAL"),
                ("energy_accounting_method", "TEXT"),
                ("energy_total_cost_usd", "REAL"),
            ] {
                if !Self::column_exists(tx, "usage", column)? {
                    tx.execute(&format!("ALTER TABLE usage ADD COLUMN {column} {ty}"), [])?;
                }
            }
            Ok(())
        })?;
        #[cfg(feature = "eval")]
        Self::apply_migration(&mut conn, 12, "eval_evidence_ledger", |tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS eval_cases (
                    case_id TEXT NOT NULL,
                    case_revision TEXT NOT NULL,
                    schema_version TEXT NOT NULL,
                    task_type TEXT NOT NULL,
                    privacy_level TEXT NOT NULL,
                    tags_json TEXT NOT NULL,
                    fixture_json TEXT NOT NULL,
                    fixture_uri_redacted TEXT,
                    manifest_sha256 TEXT NOT NULL,
                    manifest_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    PRIMARY KEY (case_id, case_revision)
                );
                CREATE TABLE IF NOT EXISTS eval_case_tags (
                    case_id TEXT NOT NULL,
                    case_revision TEXT NOT NULL,
                    tag TEXT NOT NULL,
                    PRIMARY KEY (case_id, case_revision, tag),
                    FOREIGN KEY(case_id, case_revision)
                        REFERENCES eval_cases(case_id, case_revision)
                        ON DELETE CASCADE
                );
                CREATE TABLE IF NOT EXISTS eval_runs (
                    run_id TEXT PRIMARY KEY,
                    source_run_id TEXT,
                    case_id TEXT NOT NULL,
                    case_revision TEXT NOT NULL,
                    harness TEXT NOT NULL,
                    harness_version TEXT,
                    strategy_id TEXT NOT NULL,
                    strategy_version TEXT,
                    status TEXT NOT NULL,
                    verdict TEXT NOT NULL,
                    latency_ms INTEGER,
                    cost_micros INTEGER,
                    retry_count INTEGER,
                    cache_status TEXT,
                    route_decision_id TEXT,
                    trace_id TEXT,
                    run_sha256 TEXT NOT NULL,
                    run_json TEXT NOT NULL,
                    started_at_ms INTEGER,
                    finished_at_ms INTEGER,
                    ingested_at INTEGER NOT NULL,
                    FOREIGN KEY(case_id, case_revision)
                        REFERENCES eval_cases(case_id, case_revision)
                );
                CREATE UNIQUE INDEX IF NOT EXISTS eval_runs_harness_source
                    ON eval_runs(harness, source_run_id)
                    WHERE source_run_id IS NOT NULL;
                CREATE INDEX IF NOT EXISTS eval_runs_report
                    ON eval_runs(harness, verdict, ingested_at);
                CREATE INDEX IF NOT EXISTS eval_runs_case
                    ON eval_runs(case_id, case_revision, harness);
                CREATE TABLE IF NOT EXISTS eval_outcomes (
                    run_id TEXT PRIMARY KEY,
                    verdict TEXT NOT NULL,
                    confidence REAL,
                    outcome_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    FOREIGN KEY(run_id) REFERENCES eval_runs(run_id)
                        ON DELETE CASCADE
                );
                CREATE TABLE IF NOT EXISTS eval_metrics (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    run_id TEXT NOT NULL,
                    name TEXT NOT NULL,
                    value REAL NOT NULL,
                    unit TEXT NOT NULL,
                    source TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    FOREIGN KEY(run_id) REFERENCES eval_runs(run_id)
                        ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS eval_metrics_run_name
                    ON eval_metrics(run_id, name);
                CREATE TABLE IF NOT EXISTS eval_artifacts (
                    artifact_id TEXT PRIMARY KEY,
                    run_id TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    reference TEXT NOT NULL,
                    sha256 TEXT,
                    size_bytes INTEGER,
                    privacy_level TEXT NOT NULL,
                    metadata_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    FOREIGN KEY(run_id) REFERENCES eval_runs(run_id)
                        ON DELETE CASCADE
                );
CREATE INDEX IF NOT EXISTS eval_artifacts_run_kind
ON eval_artifacts(run_id, kind);",
            )?;
            Ok(())
        })?;
        #[cfg(feature = "eval")]
        Self::apply_migration(&mut conn, 13, "eval_evidence_snapshots", |tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS eval_evidence_snapshots (
  name TEXT PRIMARY KEY,
  snapshot_id TEXT NOT NULL,
  schema_version TEXT NOT NULL,
  snapshot_sha256 TEXT NOT NULL,
  snapshot_json TEXT NOT NULL,
  generated_at_ms INTEGER NOT NULL,
  published_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS eval_evidence_snapshots_snapshot
ON eval_evidence_snapshots(snapshot_id, published_at_ms);",
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 14, "outcome_scorecard", |tx| {
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS scorecard (
                  target_id TEXT NOT NULL, class TEXT NOT NULL DEFAULT 'any',
                  scoreable_samples INTEGER NOT NULL, success_count INTEGER NOT NULL,
                  truncated_count INTEGER NOT NULL, target_fail_count INTEGER NOT NULL,
                  p50_latency_ms INTEGER NOT NULL, p95_latency_ms INTEGER NOT NULL,
                  cost_per_success_micros INTEGER NOT NULL,
                  error_histogram TEXT NOT NULL DEFAULT '{}',
                  consecutive_failures INTEGER NOT NULL DEFAULT 0,
                  tier INTEGER NOT NULL DEFAULT 0, demoted_since_ms INTEGER,
                  quality_ewma REAL,
                  updated_at_ms INTEGER NOT NULL, schema_ver INTEGER NOT NULL DEFAULT 1,
                  PRIMARY KEY (target_id, class)
                );",
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 15, "live_quality_judgments", |tx| {
            if !Self::column_exists(tx, "scorecard", "quality_samples")? {
                tx.execute(
                    "ALTER TABLE scorecard ADD COLUMN quality_samples INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            if !Self::column_exists(tx, "scorecard", "quality_updated_at_ms")? {
                tx.execute(
                    "ALTER TABLE scorecard ADD COLUMN quality_updated_at_ms INTEGER",
                    [],
                )?;
            }
            if !Self::column_exists(tx, "scorecard", "quality_evaluator_id")? {
                tx.execute(
                    "ALTER TABLE scorecard ADD COLUMN quality_evaluator_id TEXT",
                    [],
                )?;
            }
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS quality_judgments (
                   judgment_id TEXT PRIMARY KEY,
                   judge_request_id TEXT NOT NULL UNIQUE,
                   served_request_id TEXT NOT NULL,
                   served_target_id TEXT NOT NULL,
                   class TEXT NOT NULL,
                   sample_revision INTEGER NOT NULL,
                   judge_revision INTEGER NOT NULL,
                   evaluator_id TEXT NOT NULL,
                   rubric_version TEXT NOT NULL,
                   judge_target_id TEXT,
                   status TEXT NOT NULL CHECK(status IN
                     ('started','scored','ungradable','invalid','failed','timeout','abandoned')),
                   score_norm REAL CHECK(score_norm IS NULL OR
                     (score_norm >= 0.0 AND score_norm <= 1.0)),
                   reason_code TEXT,
                   input_chars INTEGER NOT NULL CHECK(input_chars >= 0),
                   output_chars INTEGER NOT NULL CHECK(output_chars >= 0),
                   reserved_cost_micros INTEGER NOT NULL CHECK(reserved_cost_micros >= 0),
                   actual_cost_micros INTEGER CHECK(actual_cost_micros IS NULL OR actual_cost_micros >= 0),
                   created_at_ms INTEGER NOT NULL,
                   completed_at_ms INTEGER
                 );
                 CREATE INDEX IF NOT EXISTS quality_judgments_target_evaluator_created
                   ON quality_judgments(served_target_id, evaluator_id, created_at_ms);
                 CREATE INDEX IF NOT EXISTS quality_judgments_created
                   ON quality_judgments(created_at_ms);",
            )?;
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 16, "usage_workload_pricing", |tx| {
            if !Self::table_exists(tx, "usage")? {
                return Ok(());
            }
            if !Self::column_exists(tx, "usage", "cost_known")? {
                tx.execute(
                    "ALTER TABLE usage ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 1",
                    [],
                )?;
            }
            for column in ["workload_kind", "pricing_unit"] {
                if !Self::column_exists(tx, "usage", column)? {
                    tx.execute(&format!("ALTER TABLE usage ADD COLUMN {column} TEXT"), [])?;
                }
            }
            if !Self::column_exists(tx, "usage", "units_consumed")? {
                tx.execute("ALTER TABLE usage ADD COLUMN units_consumed REAL", [])?;
            }
            Ok(())
        })?;
        Self::apply_migration(&mut conn, 17, "usage_cached_input_tokens", |tx| {
            if !Self::table_exists(tx, "usage")? {
                return Ok(());
            }
            // Additive + nullable: old rows keep NULL (read back as 0), so cache
            // savings for pre-upgrade history is 0. New rows always write a value.
            if !Self::column_exists(tx, "usage", "cached_input_tokens")? {
                tx.execute(
                    "ALTER TABLE usage ADD COLUMN cached_input_tokens INTEGER",
                    [],
                )?;
            }
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

    fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .map(|exists| exists != 0)
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

fn energy_rollup_from_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<UsageEnergyRollup> {
    Ok(UsageEnergyRollup {
        requests_with_energy: row.get::<_, i64>(offset)? as u64,
        energy_joules: row.get(offset + 1)?,
        energy_kwh: row.get(offset + 2)?,
        duration_seconds: row.get(offset + 3)?,
        energy_kwh_consumed: row.get(offset + 4)?,
        energy_kwh_charged: row.get(offset + 5)?,
    })
}

fn energy_rollup_select() -> &'static str {
    "COALESCE(SUM(CASE WHEN COALESCE(energy_measurement_available, 1) != 0
             AND (energy_joules IS NOT NULL OR energy_kwh IS NOT NULL
                  OR energy_kwh_consumed IS NOT NULL OR energy_kwh_charged IS NOT NULL)
             THEN 1 ELSE 0 END), 0),
         COALESCE(SUM(CASE WHEN COALESCE(energy_measurement_available, 1) != 0 THEN energy_joules ELSE 0 END), 0.0),
         COALESCE(SUM(CASE WHEN COALESCE(energy_measurement_available, 1) != 0 THEN energy_kwh ELSE 0 END), 0.0),
         COALESCE(SUM(CASE WHEN COALESCE(energy_measurement_available, 1) != 0 THEN energy_duration_seconds ELSE 0 END), 0.0),
         COALESCE(SUM(CASE WHEN COALESCE(energy_measurement_available, 1) != 0 THEN energy_kwh_consumed ELSE 0 END), 0.0),
         COALESCE(SUM(CASE WHEN COALESCE(energy_measurement_available, 1) != 0 THEN energy_kwh_charged ELSE 0 END), 0.0)"
}

fn energy_buckets(conn: &Connection, group_col: &str) -> Result<Vec<UsageEnergyBucket>> {
    let sql = format!(
        "SELECT {group_col}, {} FROM usage
             WHERE {group_col} IS NOT NULL
               AND COALESCE(energy_measurement_available, 1) != 0
               AND (energy_joules IS NOT NULL OR energy_kwh IS NOT NULL
                    OR energy_kwh_consumed IS NOT NULL OR energy_kwh_charged IS NOT NULL)
             GROUP BY {group_col} ORDER BY {group_col}",
        energy_rollup_select()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(UsageEnergyBucket {
                key: row.get(0)?,
                energy: energy_rollup_from_row(row, 1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn trace_event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TraceEvent> {
    let providers_json: String = row.get(12)?;
    let attempted_providers = serde_json::from_str(&providers_json).unwrap_or_else(|_| Vec::new());
    Ok(TraceEvent {
        request_id: row.get(0)?,
        revision: row.get::<_, i64>(1)? as u64,
        tenant: row.get(2)?,
        project: row.get(3)?,
        session_id: row.get(4)?,
        inbound_model: row.get(5)?,
        route: row.get(6)?,
        selected_target: row.get(7)?,
        final_status: row.get::<_, i64>(8)? as u16,
        total_latency_ms: row.get::<_, i64>(9)? as u64,
        streamed: row.get::<_, i64>(10)? != 0,
        cost_micros: row.get::<_, i64>(11)? as u64,
        attempted_providers,
        trace_json: row.get(13)?,
        created_at_ms: row.get(14)?,
    })
}

fn native_history_import_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<NativeHistoryImportRecord> {
    Ok(NativeHistoryImportRecord {
        import_id: row.get(0)?,
        client_filter: row.get(1)?,
        metadata_only: row.get::<_, i64>(2)? != 0,
        stores_prompts: row.get::<_, i64>(3)? != 0,
        stores_responses: row.get::<_, i64>(4)? != 0,
        stores_local_paths: row.get::<_, i64>(5)? != 0,
        source_count: row.get::<_, i64>(6)? as u64,
        existing_source_count: row.get::<_, i64>(7)? as u64,
        file_count: row.get::<_, i64>(8)? as u64,
        record_count: row.get::<_, i64>(9)? as u64,
        parse_error_count: row.get::<_, i64>(10)? as u64,
        byte_count: row.get::<_, i64>(11)? as u64,
        warnings_json: row.get(12)?,
        created_at_ms: row.get(13)?,
    })
}

fn native_history_source_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<NativeHistorySourceRecord> {
    Ok(NativeHistorySourceRecord {
        import_id: row.get(0)?,
        source_id: row.get(1)?,
        client: row.get(2)?,
        kind: row.get(3)?,
        parser: row.get(4)?,
        path_pattern: row.get(5)?,
        path_id: row.get(6)?,
        exists: row.get::<_, i64>(7)? != 0,
        truncated: row.get::<_, i64>(8)? != 0,
        skipped_file_count: row.get::<_, i64>(9)? as u64,
        file_count: row.get::<_, i64>(10)? as u64,
        record_count: row.get::<_, i64>(11)? as u64,
        parse_error_count: row.get::<_, i64>(12)? as u64,
        byte_count: row.get::<_, i64>(13)? as u64,
        modified_at_ms_min: row.get(14)?,
        modified_at_ms_max: row.get(15)?,
        observed_at_min: row.get(16)?,
        observed_at_max: row.get(17)?,
        tables_json: row.get(18)?,
        errors_json: row.get(19)?,
    })
}

#[cfg(feature = "eval")]
fn eval_to_store_error(err: sb_eval::EvalStoreError) -> StoreError {
    StoreError(err.0)
}

#[cfg(feature = "eval")]
fn serialize_eval_json<T: serde::Serialize>(label: &str, value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|err| StoreError(format!("serialize {label}: {err}")))
}

#[cfg(feature = "eval")]
fn deserialize_eval_json<T: for<'de> serde::Deserialize<'de>>(
    label: &str,
    value: &str,
) -> Result<T> {
    serde_json::from_str(value).map_err(|err| StoreError(format!("deserialize {label}: {err}")))
}

#[cfg(feature = "eval")]
fn sha256_hex(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(feature = "eval")]
fn redacted_eval_uri(uri: &str) -> Option<String> {
    if uri.trim().is_empty() {
        return None;
    }
    if uri.starts_with('/') || uri.starts_with("~/") {
        Some(format!("local-redacted:{}", sha256_hex(uri)))
    } else {
        Some(uri.to_string())
    }
}

#[cfg(feature = "eval")]
fn eval_status_json<T: serde::Serialize>(label: &str, value: &T) -> Result<String> {
    serialize_eval_json(label, value).map(|json| json.trim_matches('"').to_string())
}

#[cfg(feature = "eval")]
fn validate_eval_snapshot_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(StoreError(
            "eval snapshot name must not be empty".to_string(),
        ));
    }
    if trimmed != name {
        return Err(StoreError(
            "eval snapshot name must not have leading or trailing whitespace".to_string(),
        ));
    }
    if trimmed.len() > 128 {
        return Err(StoreError(
            "eval snapshot name must be 128 characters or fewer".to_string(),
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(StoreError(
            "eval snapshot name may contain only letters, numbers, '.', '_' or '-'".to_string(),
        ));
    }
    Ok(trimmed)
}

#[cfg(feature = "eval")]
impl SqliteStore {
    pub fn put_eval_case(&self, case: &sb_eval::EvalCaseManifest) -> Result<()> {
        case.validate().map_err(eval_to_store_error)?;
        let manifest_json = serialize_eval_json("eval case manifest", case)?;
        let manifest_sha256 = sha256_hex(&manifest_json);
        let tags_json = serialize_eval_json("eval case tags", &case.tags)?;
        let fixture_json = serialize_eval_json("eval case fixture", &case.fixture)?;
        let task_type = eval_status_json("eval case task_type", &case.task_type)?;
        let privacy_level = eval_status_json("eval case privacy_level", &case.privacy_level)?;
        let fixture_uri_redacted = redacted_eval_uri(&case.fixture.uri);
        let created_at = now_millis();

        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO eval_cases
                (case_id, case_revision, schema_version, task_type, privacy_level,
                 tags_json, fixture_json, fixture_uri_redacted, manifest_sha256,
                 manifest_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                case.case_id,
                case.case_revision,
                case.schema_version,
                task_type,
                privacy_level,
                tags_json,
                fixture_json,
                fixture_uri_redacted,
                manifest_sha256,
                manifest_json,
                created_at
            ],
        )?;
        tx.execute(
            "DELETE FROM eval_case_tags WHERE case_id = ?1 AND case_revision = ?2",
            params![case.case_id, case.case_revision],
        )?;
        for tag in &case.tags {
            tx.execute(
                "INSERT OR IGNORE INTO eval_case_tags
                    (case_id, case_revision, tag)
                 VALUES (?1, ?2, ?3)",
                params![case.case_id, case.case_revision, tag],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn ingest_eval_run(
        &self,
        run: &sb_eval::EvalRunIngest,
    ) -> Result<sb_eval::EvalIngestReceipt> {
        run.validate().map_err(eval_to_store_error)?;
        let run_id = run.stable_run_id();
        let status = eval_status_json("eval run status", &run.status)?;
        let verdict = eval_status_json("eval run verdict", &run.outcome.verdict)?;
        let cache_status = run
            .cache_status
            .as_ref()
            .map(|status| eval_status_json("eval cache status", status))
            .transpose()?;
        let run_json = serialize_eval_json("eval run", run)?;
        let run_sha256 = sha256_hex(&run_json);
        let outcome_json = serialize_eval_json("eval outcome", &run.outcome)?;
        let latency_ms = run.latency_ms().map(|value| value as i64);
        let cost_micros = run.cost_micros().map(|value| value as i64);
        let retry_count = run.retry_count.map(|value| value as i64);
        let started_at_ms = run.started_at_ms.map(|value| value as i64);
        let finished_at_ms = run.finished_at_ms.map(|value| value as i64);
        let ingested_at = now_millis();

        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let case_exists: i64 = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM eval_cases
                WHERE case_id = ?1 AND case_revision = ?2
            )",
            params![run.case_id, run.case_revision],
            |row| row.get(0),
        )?;
        if case_exists == 0 {
            tx.commit()?;
            return Err(StoreError(format!(
                "unknown eval case `{}` revision `{}`",
                run.case_id, run.case_revision
            )));
        }

        if let Some(source_run_id) = run
            .source_run_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
        {
            let mut stmt = tx.prepare(
                "SELECT run_id FROM eval_runs
                 WHERE harness = ?1 AND source_run_id = ?2
                 LIMIT 1",
            )?;
            let mut rows = stmt.query_map(params![run.harness, source_run_id], |row| {
                row.get::<_, String>(0)
            })?;
            if let Some(existing) = rows.next() {
                let existing = existing?;
                drop(rows);
                drop(stmt);
                tx.commit()?;
                return Ok(sb_eval::EvalIngestReceipt {
                    run_id: existing,
                    inserted: false,
                });
            }
            drop(rows);
            drop(stmt);
        }

        let run_id_exists: i64 = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM eval_runs WHERE run_id = ?1)",
            params![run_id],
            |row| row.get(0),
        )?;
        if run_id_exists != 0 {
            tx.commit()?;
            return Ok(sb_eval::EvalIngestReceipt {
                run_id,
                inserted: false,
            });
        }

        tx.execute(
            "INSERT INTO eval_runs
                (run_id, source_run_id, case_id, case_revision, harness, harness_version,
                 strategy_id, strategy_version, status, verdict, latency_ms, cost_micros,
                 retry_count, cache_status, route_decision_id, trace_id, run_sha256, run_json,
                 started_at_ms, finished_at_ms, ingested_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                     ?13, ?14, NULL, NULL, ?15, ?16, ?17, ?18, ?19)",
            params![
                run_id,
                run.source_run_id,
                run.case_id,
                run.case_revision,
                run.harness,
                run.harness_version,
                run.strategy_id,
                run.strategy_version,
                status,
                verdict,
                latency_ms,
                cost_micros,
                retry_count,
                cache_status,
                run_sha256,
                run_json,
                started_at_ms,
                finished_at_ms,
                ingested_at
            ],
        )?;
        tx.execute(
            "INSERT INTO eval_outcomes
                (run_id, verdict, confidence, outcome_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id,
                verdict,
                run.outcome.confidence.map(f64::from),
                outcome_json,
                ingested_at
            ],
        )?;
        for metric in &run.metrics {
            tx.execute(
                "INSERT INTO eval_metrics
                    (run_id, name, value, unit, source, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    run_id,
                    metric.name,
                    metric.value,
                    metric.unit,
                    metric.source,
                    ingested_at
                ],
            )?;
        }
        for (index, artifact) in run.artifacts.iter().enumerate() {
            let metadata_json = serialize_eval_json("eval artifact metadata", &artifact.metadata)?;
            let artifact_id = artifact
                .sha256
                .clone()
                .unwrap_or_else(|| format!("{}:{index}", run_id));
            let kind = eval_status_json("eval artifact kind", &artifact.kind)?;
            let privacy_level =
                eval_status_json("eval artifact privacy level", &artifact.privacy_level)?;
            tx.execute(
                "INSERT INTO eval_artifacts
                    (artifact_id, run_id, kind, reference, sha256, size_bytes,
                     privacy_level, metadata_json, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, ?8)",
                params![
                    artifact_id,
                    run_id,
                    kind,
                    artifact.reference,
                    artifact.sha256,
                    privacy_level,
                    metadata_json,
                    ingested_at
                ],
            )?;
        }

        tx.commit()?;
        Ok(sb_eval::EvalIngestReceipt {
            run_id,
            inserted: true,
        })
    }

    pub fn eval_report(&self, query: sb_eval::EvalReportQuery) -> Result<sb_eval::EvalReport> {
        let conn = self.conn()?;
        let mut cases = std::collections::BTreeMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT case_id, case_revision, manifest_json FROM eval_cases")?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (case_id, case_revision, manifest_json) = row?;
                let case: sb_eval::EvalCaseManifest =
                    deserialize_eval_json("eval case manifest", &manifest_json)?;
                cases.insert((case_id, case_revision), case);
            }
        }

        let mut runs = Vec::new();
        {
            let mut stmt =
                conn.prepare("SELECT run_id, run_json FROM eval_runs ORDER BY ingested_at ASC")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (run_id, run_json) = row?;
                let run: sb_eval::EvalRunIngest = deserialize_eval_json("eval run", &run_json)?;
                runs.push(sb_eval::StoredEvalRun { run_id, run });
            }
        }

        Ok(sb_eval::EvalReport {
            rows: sb_eval::build_report_rows(&cases, runs.iter(), &query),
        })
    }

    pub fn publish_eval_evidence_snapshot(
        &self,
        name: &str,
        snapshot: &sb_eval::EvalEvidenceSnapshot,
    ) -> Result<EvalEvidenceSnapshotRecord> {
        let name = validate_eval_snapshot_name(name)?;
        snapshot.validate().map_err(eval_to_store_error)?;
        let snapshot_json = serialize_eval_json("eval evidence snapshot", snapshot)?;
        let snapshot_sha256 = sha256_hex(&snapshot_json);
        let published_at_ms = now_millis();
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR REPLACE INTO eval_evidence_snapshots
             (name, snapshot_id, schema_version, snapshot_sha256, snapshot_json,
              generated_at_ms, published_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                name,
                &snapshot.snapshot_id,
                &snapshot.schema_version,
                snapshot_sha256,
                snapshot_json,
                snapshot.generated_at_ms as i64,
                published_at_ms,
            ],
        )?;
        Ok(EvalEvidenceSnapshotRecord {
            name: name.to_string(),
            snapshot_id: snapshot.snapshot_id.clone(),
            schema_version: snapshot.schema_version.clone(),
            snapshot_sha256,
            generated_at_ms: snapshot.generated_at_ms,
            published_at_ms,
        })
    }

    pub fn get_eval_evidence_snapshot(
        &self,
        name: &str,
    ) -> Result<Option<sb_eval::EvalEvidenceSnapshot>> {
        let name = validate_eval_snapshot_name(name)?;
        let conn = self.conn()?;
        let snapshot_json = conn
            .query_row(
                "SELECT snapshot_json FROM eval_evidence_snapshots WHERE name = ?1",
                [name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        snapshot_json
            .map(|json| deserialize_eval_json("eval evidence snapshot", &json))
            .transpose()
    }

    pub fn get_eval_evidence_snapshot_record(
        &self,
        name: &str,
    ) -> Result<Option<EvalEvidenceSnapshotRecord>> {
        let name = validate_eval_snapshot_name(name)?;
        let conn = self.conn()?;
        conn.query_row(
            "SELECT name, snapshot_id, schema_version, snapshot_sha256,
                    generated_at_ms, published_at_ms
             FROM eval_evidence_snapshots WHERE name = ?1",
            [name],
            |row| {
                Ok(EvalEvidenceSnapshotRecord {
                    name: row.get(0)?,
                    snapshot_id: row.get(1)?,
                    schema_version: row.get(2)?,
                    snapshot_sha256: row.get(3)?,
                    generated_at_ms: row.get::<_, i64>(4)? as u64,
                    published_at_ms: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_eval_evidence_snapshot_records(&self) -> Result<Vec<EvalEvidenceSnapshotRecord>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT name, snapshot_id, schema_version, snapshot_sha256,
                    generated_at_ms, published_at_ms
             FROM eval_evidence_snapshots
             ORDER BY published_at_ms DESC, name ASC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(EvalEvidenceSnapshotRecord {
                    name: row.get(0)?,
                    snapshot_id: row.get(1)?,
                    schema_version: row.get(2)?,
                    snapshot_sha256: row.get(3)?,
                    generated_at_ms: row.get::<_, i64>(4)? as u64,
                    published_at_ms: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn import_eval_llm_judge_result(
        &self,
        run_id: &str,
        result: &sb_eval::LlmJudgeResult,
    ) -> Result<sb_eval::LlmJudgeImportReceipt> {
        result.validate().map_err(eval_to_store_error)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run_json = tx
            .query_row(
                "SELECT run_json FROM eval_runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError(format!("unknown eval run `{run_id}`")))?;
        let mut run: sb_eval::EvalRunIngest = deserialize_eval_json("eval run", &run_json)?;
        let receipt = result
            .merge_into_run(run_id, &mut run)
            .map_err(eval_to_store_error)?;
        let run_json = serialize_eval_json("eval run", &run)?;
        let run_sha256 = sha256_hex(&run_json);
        let outcome_json = serialize_eval_json("eval outcome", &run.outcome)?;
        let verdict = eval_status_json("eval verdict", &run.outcome.verdict)?;
        let updated_at = now_millis();
        tx.execute(
            "UPDATE eval_runs
             SET verdict = ?2, run_sha256 = ?3, run_json = ?4
             WHERE run_id = ?1",
            params![run_id, verdict, run_sha256, run_json],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO eval_outcomes
             (run_id, verdict, confidence, outcome_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                run_id,
                verdict,
                run.outcome.confidence.map(f64::from),
                outcome_json,
                updated_at
            ],
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn eval_llm_judge_packet(
        &self,
        run_id: &str,
        options: sb_eval::LlmJudgePacketOptions,
    ) -> Result<sb_eval::LlmJudgePacket> {
        let conn = self.conn()?;
        let (case_id, case_revision, run_json) = conn
            .query_row(
                "SELECT case_id, case_revision, run_json
                 FROM eval_runs
                 WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| StoreError(format!("unknown eval run `{run_id}`")))?;
        let case_json = conn
            .query_row(
                "SELECT manifest_json
                 FROM eval_cases
                 WHERE case_id = ?1 AND case_revision = ?2",
                params![case_id, case_revision],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError(format!(
                    "unknown eval case `{case_id}` revision `{case_revision}`"
                ))
            })?;
        let case: sb_eval::EvalCaseManifest = deserialize_eval_json("eval case", &case_json)?;
        let run: sb_eval::EvalRunIngest = deserialize_eval_json("eval run", &run_json)?;
        sb_eval::LlmJudgePacket::from_case_run(run_id, &case, &run, options)
            .map_err(eval_to_store_error)
    }
}

#[cfg(feature = "eval")]
impl sb_eval::CaseStore for SqliteStore {
    fn put_case(&mut self, case: sb_eval::EvalCaseManifest) -> sb_eval::Result<()> {
        self.put_eval_case(&case)
            .map_err(|err| sb_eval::EvalStoreError(err.0))
    }
}

#[cfg(feature = "eval")]
impl sb_eval::EvalStore for SqliteStore {
    fn ingest_run(
        &mut self,
        run: sb_eval::EvalRunIngest,
    ) -> sb_eval::Result<sb_eval::EvalIngestReceipt> {
        self.ingest_eval_run(&run)
            .map_err(|err| sb_eval::EvalStoreError(err.0))
    }

    fn import_llm_judge_result(
        &mut self,
        run_id: &str,
        result: sb_eval::LlmJudgeResult,
    ) -> sb_eval::Result<sb_eval::LlmJudgeImportReceipt> {
        self.import_eval_llm_judge_result(run_id, &result)
            .map_err(|err| sb_eval::EvalStoreError(err.0))
    }

    fn llm_judge_packet(
        &self,
        run_id: &str,
        options: sb_eval::LlmJudgePacketOptions,
    ) -> sb_eval::Result<sb_eval::LlmJudgePacket> {
        self.eval_llm_judge_packet(run_id, options)
            .map_err(|err| sb_eval::EvalStoreError(err.0))
    }

    fn report(&self, query: sb_eval::EvalReportQuery) -> sb_eval::Result<sb_eval::EvalReport> {
        self.eval_report(query)
            .map_err(|err| sb_eval::EvalStoreError(err.0))
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

    fn record_usage(&self, e: &UsageEvent) -> Result<UsageWriteOutcome> {
        let conn = self.conn()?;
        let rows = conn.execute(
            "INSERT OR IGNORE INTO usage
             (request_id, provider_id, model, account_id, tenant, project, cost_micros,
              cost_known, workload_kind, pricing_unit, units_consumed,
              input_tokens, output_tokens, latency_ms, streamed, energy_joules, energy_kwh,
              energy_duration_seconds, energy_measurement_available, energy_attribution_method,
              energy_kwh_consumed, energy_kwh_charged, energy_accounting_method,
              energy_total_cost_usd, created_at, cached_input_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
            params![
                e.request_id,
                e.provider_id,
                e.model,
                e.account_id,
                e.tenant,
                e.project,
                e.cost_micros.unwrap_or(0) as i64,
                e.cost_micros.is_some() as i64,
                e.workload_kind,
                e.pricing_unit,
                e.units_consumed,
                e.input_tokens as i64,
                e.output_tokens as i64,
                e.latency_ms as i64,
                e.streamed as i64,
                e.energy_joules,
                e.energy_kwh,
                e.energy_duration_seconds,
                e.energy_measurement_available
                    .map(|available| if available { 1_i64 } else { 0_i64 }),
                e.energy_attribution_method,
                e.energy_kwh_consumed,
                e.energy_kwh_charged,
                e.energy_accounting_method,
                e.energy_total_cost_usd,
                e.created_at_ms,
                e.cached_input_tokens as i64,
            ],
        )?;
        if rows == 0 {
            Ok(UsageWriteOutcome::DuplicateIgnored)
        } else {
            Ok(UsageWriteOutcome::Inserted)
        }
    }

    fn usage_rollup(&self) -> Result<UsageRollup> {
        let conn = self.conn()?;
        let (requests, total_cost_micros, unknown_cost_requests, energy) = conn.query_row(
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(cost_micros),0),
                        COALESCE(SUM(CASE WHEN cost_known = 0 THEN 1 ELSE 0 END),0),
                        {} FROM usage",
                energy_rollup_select()
            ),
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                    energy_rollup_from_row(row, 3)?,
                ))
            },
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
            unknown_cost_requests,
            by_provider: Self::usage_buckets(&conn, "provider_id")?,
            by_model: Self::usage_buckets(&conn, "model")?,
            by_tenant,
            energy,
            energy_by_provider: energy_buckets(&conn, "provider_id")?,
            energy_by_model: energy_buckets(&conn, "model")?,
            energy_by_tenant: energy_buckets(&conn, "tenant")?,
        })
    }

    fn recent_usage(&self, limit: usize) -> Result<Vec<UsageEvent>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT request_id, provider_id, model, account_id, tenant, project, cost_micros,
                    cost_known, workload_kind, pricing_unit, units_consumed,
                    input_tokens, output_tokens, latency_ms, streamed,
                    energy_joules, energy_kwh, energy_duration_seconds,
                    energy_measurement_available, energy_attribution_method,
                    energy_kwh_consumed, energy_kwh_charged, energy_accounting_method,
                    energy_total_cost_usd, created_at, cached_input_tokens
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
                    project: row.get(5)?,
                    cost_micros: (row.get::<_, i64>(7)? != 0)
                        .then(|| row.get::<_, i64>(6).map(|cost| cost as u64))
                        .transpose()?,
                    workload_kind: row.get(8)?,
                    pricing_unit: row.get(9)?,
                    units_consumed: row.get(10)?,
                    input_tokens: row.get::<_, i64>(11)? as u64,
                    output_tokens: row.get::<_, i64>(12)? as u64,
                    latency_ms: row.get::<_, i64>(13)? as u64,
                    streamed: row.get::<_, i64>(14)? != 0,
                    energy_joules: row.get(15)?,
                    energy_kwh: row.get(16)?,
                    energy_duration_seconds: row.get(17)?,
                    energy_measurement_available: row
                        .get::<_, Option<i64>>(18)?
                        .map(|available| available != 0),
                    energy_attribution_method: row.get(19)?,
                    energy_kwh_consumed: row.get(20)?,
                    energy_kwh_charged: row.get(21)?,
                    energy_accounting_method: row.get(22)?,
                    energy_total_cost_usd: row.get(23)?,
                    created_at_ms: row.get(24)?,
                    // Nullable additive column: pre-upgrade rows are NULL -> 0.
                    cached_input_tokens: row.get::<_, Option<i64>>(25)?.unwrap_or(0) as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn record_trace(&self, e: &TraceEvent) -> Result<bool> {
        let conn = self.conn()?;
        let providers_json = serde_json::to_string(&e.attempted_providers)
            .map_err(|err| StoreError(format!("serialize trace providers: {err}")))?;
        let rows = conn.execute(
            "INSERT OR IGNORE INTO trace_events
                (request_id, revision, tenant, project, session_id, inbound_model, route,
                 selected_target, final_status, total_latency_ms, streamed, cost_micros,
                 attempted_providers, trace_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                e.request_id,
                e.revision as i64,
                e.tenant,
                e.project,
                e.session_id,
                e.inbound_model,
                e.route,
                e.selected_target,
                e.final_status as i64,
                e.total_latency_ms as i64,
                e.streamed as i64,
                e.cost_micros as i64,
                providers_json,
                e.trace_json,
                e.created_at_ms,
            ],
        )?;
        Ok(rows != 0)
    }

    fn query_traces(&self, q: &TraceQuery) -> Result<Vec<TraceEvent>> {
        let conn = self.conn()?;
        let limit = q.limit.clamp(1, 5000) as i64;
        let mut stmt = conn.prepare(
            "SELECT request_id, revision, tenant, project, session_id, inbound_model, route,
                    selected_target, final_status, total_latency_ms, streamed, cost_micros,
                    attempted_providers, trace_json, created_at
             FROM trace_events
             WHERE (?1 IS NULL OR tenant = ?1)
               AND (?2 IS NULL OR session_id = ?2)
               AND (?3 IS NULL OR inbound_model = ?3)
               AND (?4 IS NULL OR final_status = ?4)
               AND (?5 IS NULL OR created_at >= ?5)
             ORDER BY created_at DESC, id DESC
             LIMIT ?6",
        )?;
        let rows = stmt
            .query_map(
                params![
                    q.tenant.as_deref(),
                    q.session_id.as_deref(),
                    q.model.as_deref(),
                    q.status.map(|status| status as i64),
                    q.since_ms,
                    limit,
                ],
                trace_event_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn get_trace(&self, request_id: &str) -> Result<Option<TraceEvent>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT request_id, revision, tenant, project, session_id, inbound_model, route,
                    selected_target, final_status, total_latency_ms, streamed, cost_micros,
                    attempted_providers, trace_json, created_at
             FROM trace_events WHERE request_id = ?1",
        )?;
        let mut rows = stmt.query_map([request_id], trace_event_from_row)?;
        match rows.next() {
            Some(rec) => Ok(Some(rec?)),
            None => Ok(None),
        }
    }

    fn record_native_history_import(
        &self,
        batch: &NativeHistoryImportBatch,
    ) -> Result<NativeHistoryImportWrite> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO native_history_imports
                (import_id, client_filter, metadata_only, stores_prompts, stores_responses,
                 stores_local_paths, source_count, existing_source_count, file_count,
                 record_count, parse_error_count, byte_count, warnings_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                batch.import.import_id,
                batch.import.client_filter,
                batch.import.metadata_only as i64,
                batch.import.stores_prompts as i64,
                batch.import.stores_responses as i64,
                batch.import.stores_local_paths as i64,
                batch.import.source_count as i64,
                batch.import.existing_source_count as i64,
                batch.import.file_count as i64,
                batch.import.record_count as i64,
                batch.import.parse_error_count as i64,
                batch.import.byte_count as i64,
                batch.import.warnings_json,
                batch.import.created_at_ms,
            ],
        )?;
        tx.execute(
            "DELETE FROM native_history_sources WHERE import_id = ?1",
            [batch.import.import_id.as_str()],
        )?;
        let mut written = 0u64;
        for source in &batch.sources {
            tx.execute(
                "INSERT INTO native_history_sources
                    (import_id, source_id, client, kind, parser, path_pattern, path_id,
                     source_exists, truncated, skipped_file_count, file_count, record_count,
                     parse_error_count, byte_count, modified_at_ms_min, modified_at_ms_max,
                     observed_at_min, observed_at_max, tables_json, errors_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                         ?15, ?16, ?17, ?18, ?19, ?20)",
                params![
                    source.import_id,
                    source.source_id,
                    source.client,
                    source.kind,
                    source.parser,
                    source.path_pattern,
                    source.path_id,
                    source.exists as i64,
                    source.truncated as i64,
                    source.skipped_file_count as i64,
                    source.file_count as i64,
                    source.record_count as i64,
                    source.parse_error_count as i64,
                    source.byte_count as i64,
                    source.modified_at_ms_min,
                    source.modified_at_ms_max,
                    source.observed_at_min,
                    source.observed_at_max,
                    source.tables_json,
                    source.errors_json,
                ],
            )?;
            written += 1;
        }
        tx.commit()?;
        Ok(NativeHistoryImportWrite {
            source_rows_written: written,
        })
    }

    fn recent_native_history_imports(
        &self,
        limit: usize,
    ) -> Result<Vec<NativeHistoryImportRecord>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT import_id, client_filter, metadata_only, stores_prompts, stores_responses,
                    stores_local_paths, source_count, existing_source_count, file_count,
                    record_count, parse_error_count, byte_count, warnings_json, created_at
             FROM native_history_imports
             ORDER BY created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(
                [limit.clamp(1, 5000) as i64],
                native_history_import_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn native_history_sources(&self, import_id: &str) -> Result<Vec<NativeHistorySourceRecord>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT import_id, source_id, client, kind, parser, path_pattern, path_id,
                    source_exists, truncated, skipped_file_count, file_count, record_count,
                    parse_error_count, byte_count, modified_at_ms_min, modified_at_ms_max,
                    observed_at_min, observed_at_max, tables_json, errors_json
             FROM native_history_sources
             WHERE import_id = ?1
             ORDER BY client, source_id",
        )?;
        let rows = stmt
            .query_map([import_id], native_history_source_from_row)?
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
        lease_id: &str,
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
            "INSERT INTO idempotency_inflight (key, fingerprint, lease_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key, fingerprint, lease_id, now, expires],
        )?;
        tx.commit()?;
        Ok(IdempotencyBegin::Claimed)
    }

    fn idempotency_release(&self, key: &str, lease_id: &str) -> Result<bool> {
        let conn = self.conn()?;
        let changed = conn.execute(
            "DELETE FROM idempotency_inflight WHERE key = ?1 AND lease_id = ?2",
            params![key, lease_id],
        )?;
        Ok(changed > 0)
    }

    fn idempotency_renew(&self, key: &str, lease_id: &str, ttl_ms: u64) -> Result<bool> {
        let conn = self.conn()?;
        let now = now_millis();
        let expires = now.saturating_add(ttl_ms as i64);
        let changed = conn.execute(
            "UPDATE idempotency_inflight
             SET expires_at = ?1
             WHERE key = ?2 AND lease_id = ?3 AND expires_at > ?4",
            params![expires, key, lease_id, now],
        )?;
        Ok(changed > 0)
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

    fn tenant_slot_renew(&self, slot_id: &str, ttl_ms: u64) -> Result<bool> {
        let conn = self.conn()?;
        let now = now_millis();
        let expires = now.saturating_add(ttl_ms as i64);
        let changed = conn.execute(
            "UPDATE tenant_slots
             SET expires_at = ?1
             WHERE slot_id = ?2 AND expires_at > ?3",
            params![expires, slot_id, now],
        )?;
        Ok(changed > 0)
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

    fn admission_slot_acquire(&self, slot_id: &str, max: u32, ttl_ms: u64) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_millis();
        let expires = now.saturating_add(ttl_ms as i64);
        tx.execute("DELETE FROM admission_slots WHERE expires_at <= ?1", [now])?;
        let active: i64 =
            tx.query_row("SELECT COUNT(*) FROM admission_slots", [], |row| row.get(0))?;
        if active >= max as i64 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO admission_slots (slot_id, created_at, expires_at)
             VALUES (?1, ?2, ?3)",
            params![slot_id, now, expires],
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn admission_slot_release(&self, slot_id: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM admission_slots WHERE slot_id = ?1", [slot_id])?;
        Ok(())
    }

    fn admission_slot_renew(&self, slot_id: &str, ttl_ms: u64) -> Result<bool> {
        let conn = self.conn()?;
        let now = now_millis();
        let expires = now.saturating_add(ttl_ms as i64);
        let changed = conn.execute(
            "UPDATE admission_slots
             SET expires_at = ?1
             WHERE slot_id = ?2 AND expires_at > ?3",
            params![expires, slot_id, now],
        )?;
        Ok(changed > 0)
    }

    fn admission_slot_count(&self) -> Result<u32> {
        let conn = self.conn()?;
        let now = now_millis();
        conn.execute("DELETE FROM admission_slots WHERE expires_at <= ?1", [now])?;
        let active: i64 =
            conn.query_row("SELECT COUNT(*) FROM admission_slots", [], |row| row.get(0))?;
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

    fn upsert_scorecard(&self, rows: &[ScorecardRow]) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        for row in rows {
            tx.execute(
                "INSERT OR REPLACE INTO scorecard
                    (target_id, class, scoreable_samples, success_count, truncated_count,
                     target_fail_count, p50_latency_ms, p95_latency_ms,
                     cost_per_success_micros, error_histogram, consecutive_failures,
                     tier, demoted_since_ms, quality_ewma, quality_samples,
                     quality_updated_at_ms, quality_evaluator_id, updated_at_ms, schema_ver)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    row.target_id,
                    row.class,
                    row.scoreable_samples as i64,
                    row.success_count as i64,
                    row.truncated_count as i64,
                    row.target_fail_count as i64,
                    row.p50_latency_ms as i64,
                    row.p95_latency_ms as i64,
                    row.cost_per_success_micros as i64,
                    row.error_histogram,
                    row.consecutive_failures as i64,
                    row.tier as i64,
                    row.demoted_since_ms,
                    row.quality_ewma,
                    row.quality_samples as i64,
                    row.quality_updated_at_ms,
                    row.quality_evaluator_id,
                    row.updated_at_ms,
                    row.schema_ver as i64,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn load_scorecard(&self) -> Result<Vec<ScorecardRow>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT target_id, class, scoreable_samples, success_count, truncated_count,
                    target_fail_count, p50_latency_ms, p95_latency_ms,
                    cost_per_success_micros, error_histogram, consecutive_failures,
                    tier, demoted_since_ms, quality_ewma, quality_samples,
                    quality_updated_at_ms, quality_evaluator_id, updated_at_ms, schema_ver
             FROM scorecard",
        )?;
        let raw_rows = stmt
            .query_map([], |row| {
                Ok(RawScorecardRow {
                    target_id: row.get(0)?,
                    class: row.get(1)?,
                    scoreable_samples: row.get(2)?,
                    success_count: row.get(3)?,
                    truncated_count: row.get(4)?,
                    target_fail_count: row.get(5)?,
                    p50_latency_ms: row.get(6)?,
                    p95_latency_ms: row.get(7)?,
                    cost_per_success_micros: row.get(8)?,
                    error_histogram: row.get(9)?,
                    consecutive_failures: row.get(10)?,
                    tier: row.get(11)?,
                    demoted_since_ms: row.get(12)?,
                    quality_ewma: row.get(13)?,
                    quality_samples: row.get(14)?,
                    quality_updated_at_ms: row.get(15)?,
                    quality_evaluator_id: row.get(16)?,
                    updated_at_ms: row.get(17)?,
                    schema_ver: row.get(18)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let rows = raw_rows
            .into_iter()
            .filter_map(|raw| match decode_scorecard_row(raw) {
                Ok(row) => Some(row),
                Err(reason) => {
                    tracing::warn!(reason, "scorecard: rejecting corrupt persisted row on load");
                    None
                }
            })
            .collect();
        Ok(rows)
    }

    fn reserve_quality_judgment(
        &self,
        reservation: &QualityJudgmentReservation,
        max_judgments: u64,
        max_cost_micros: u64,
        since_ms: i64,
    ) -> Result<QualityJudgmentReserveOutcome> {
        validate_quality_reservation(reservation)?;
        let reserved_cost =
            quality_u64_to_i64(reservation.reserved_cost_micros, "reserved_cost_micros")?;
        let sample_revision = quality_u64_to_i64(reservation.sample_revision, "sample_revision")?;
        let judge_revision = quality_u64_to_i64(reservation.judge_revision, "judge_revision")?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM quality_judgments
             WHERE judgment_id = ?1 OR judge_request_id = ?2)",
            params![reservation.judgment_id, reservation.judge_request_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if duplicate {
            tx.commit()?;
            return Ok(QualityJudgmentReserveOutcome::Duplicate);
        }
        let budget = quality_judgment_budget_from(&tx, since_ms)?;
        let proposed_cost = budget
            .cost_micros
            .checked_add(reservation.reserved_cost_micros)
            .ok_or_else(|| StoreError("quality judgment budget cost overflow".to_string()))?;
        if budget.attempted >= max_judgments || proposed_cost > max_cost_micros {
            tx.commit()?;
            return Ok(QualityJudgmentReserveOutcome::BudgetExceeded(budget));
        }
        tx.execute(
            "INSERT INTO quality_judgments
               (judgment_id, judge_request_id, served_request_id, served_target_id, class,
                sample_revision, judge_revision, evaluator_id, rubric_version, judge_target_id,
                status, score_norm, reason_code, input_chars, output_chars,
                reserved_cost_micros, actual_cost_micros, created_at_ms, completed_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                     'started', NULL, NULL, ?11, ?12, ?13, NULL, ?14, NULL)",
            params![
                reservation.judgment_id,
                reservation.judge_request_id,
                reservation.served_request_id,
                reservation.served_target_id,
                reservation.class,
                sample_revision,
                judge_revision,
                reservation.evaluator_id,
                reservation.rubric_version,
                reservation.judge_target_id,
                reservation.input_chars as i64,
                reservation.output_chars as i64,
                reserved_cost,
                reservation.created_at_ms,
            ],
        )?;
        tx.execute(
            "DELETE FROM quality_judgments WHERE rowid NOT IN
               (SELECT rowid FROM quality_judgments ORDER BY rowid DESC LIMIT 2000)",
            [],
        )?;
        tx.commit()?;
        Ok(QualityJudgmentReserveOutcome::Reserved)
    }

    fn finalize_quality_judgment(
        &self,
        finalization: &QualityJudgmentFinalization,
    ) -> Result<bool> {
        validate_quality_finalization(finalization)?;
        let actual_cost = finalization
            .actual_cost_micros
            .map(|cost| quality_u64_to_i64(cost, "actual_cost_micros"))
            .transpose()?;
        let conn = self.conn()?;
        let changed = conn.execute(
            "UPDATE quality_judgments
             SET judge_target_id = COALESCE(?2, judge_target_id), status = ?3,
                 score_norm = ?4, reason_code = ?5, actual_cost_micros = ?6,
                 completed_at_ms = ?7
             WHERE judgment_id = ?1 AND status = 'started'",
            params![
                finalization.judgment_id,
                finalization.judge_target_id,
                finalization.status,
                finalization.score_norm,
                finalization.reason_code,
                actual_cost,
                finalization.completed_at_ms,
            ],
        )?;
        Ok(changed != 0)
    }

    fn replay_quality_judgments(
        &self,
        evaluator_id: &str,
        since_ms: i64,
    ) -> Result<Vec<QualityJudgmentRecord>> {
        validate_quality_text("evaluator_id", evaluator_id)?;
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!(
            "{} WHERE evaluator_id = ?1 AND status = 'scored' AND created_at_ms >= ?2
             ORDER BY created_at_ms ASC, rowid ASC",
            QUALITY_JUDGMENT_SELECT
        ))?;
        let rows = stmt
            .query_map(params![evaluator_id, since_ms], quality_judgment_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn abandon_started_quality_judgments(&self, completed_at_ms: i64) -> Result<u64> {
        if completed_at_ms < 0 {
            return Err(StoreError(
                "completed_at_ms must be non-negative".to_string(),
            ));
        }
        let conn = self.conn()?;
        let changed = conn.execute(
            "UPDATE quality_judgments
             SET status = 'abandoned', completed_at_ms = ?1
             WHERE status = 'started'",
            [completed_at_ms],
        )?;
        Ok(changed as u64)
    }

    fn recent_quality_judgments(&self, limit: usize) -> Result<Vec<QualityJudgmentRecord>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(&format!(
            "{} ORDER BY created_at_ms DESC, rowid DESC LIMIT ?1",
            QUALITY_JUDGMENT_SELECT
        ))?;
        let rows = stmt
            .query_map([limit.min(5000) as i64], quality_judgment_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn quality_judgment_budget(&self, since_ms: i64) -> Result<QualityJudgmentBudget> {
        let conn = self.conn()?;
        quality_judgment_budget_from(&conn, since_ms)
    }
}

const QUALITY_JUDGMENT_SELECT: &str =
    "SELECT judgment_id, judge_request_id, served_request_id, served_target_id, class,
            sample_revision, judge_revision, evaluator_id, rubric_version, judge_target_id,
            status, score_norm, reason_code, input_chars, output_chars, reserved_cost_micros,
            actual_cost_micros, created_at_ms, completed_at_ms
     FROM quality_judgments";

fn quality_judgment_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<QualityJudgmentRecord> {
    Ok(QualityJudgmentRecord {
        judgment_id: row.get(0)?,
        judge_request_id: row.get(1)?,
        served_request_id: row.get(2)?,
        served_target_id: row.get(3)?,
        class: row.get(4)?,
        sample_revision: row.get::<_, i64>(5)? as u64,
        judge_revision: row.get::<_, i64>(6)? as u64,
        evaluator_id: row.get(7)?,
        rubric_version: row.get(8)?,
        judge_target_id: row.get(9)?,
        status: row.get(10)?,
        score_norm: row.get(11)?,
        reason_code: row.get(12)?,
        input_chars: row.get::<_, i64>(13)? as u32,
        output_chars: row.get::<_, i64>(14)? as u32,
        reserved_cost_micros: row.get::<_, i64>(15)? as u64,
        actual_cost_micros: row.get::<_, Option<i64>>(16)?.map(|cost| cost as u64),
        created_at_ms: row.get(17)?,
        completed_at_ms: row.get(18)?,
    })
}

fn quality_judgment_budget_from(conn: &Connection, since_ms: i64) -> Result<QualityJudgmentBudget> {
    let (attempted, cost_micros) = conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(MAX(reserved_cost_micros, COALESCE(actual_cost_micros, 0))), 0)
         FROM quality_judgments WHERE created_at_ms >= ?1",
        [since_ms],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )?;
    Ok(QualityJudgmentBudget {
        attempted: u64::try_from(attempted)
            .map_err(|_| StoreError("quality judgment count is negative".to_string()))?,
        cost_micros: u64::try_from(cost_micros)
            .map_err(|_| StoreError("quality judgment cost is negative".to_string()))?,
    })
}

fn quality_u64_to_i64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| StoreError(format!("{label} exceeds SQLite INTEGER range")))
}

fn validate_quality_text(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StoreError(format!("{label} must not be empty")));
    }
    Ok(())
}

fn validate_quality_code(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(StoreError(format!(
            "{label} must be a lowercase enum identifier"
        )));
    }
    Ok(())
}

fn validate_quality_reservation(reservation: &QualityJudgmentReservation) -> Result<()> {
    for (label, value) in [
        ("judgment_id", reservation.judgment_id.as_str()),
        ("judge_request_id", reservation.judge_request_id.as_str()),
        ("served_request_id", reservation.served_request_id.as_str()),
        ("served_target_id", reservation.served_target_id.as_str()),
        ("class", reservation.class.as_str()),
        ("evaluator_id", reservation.evaluator_id.as_str()),
        ("rubric_version", reservation.rubric_version.as_str()),
    ] {
        validate_quality_text(label, value)?;
    }
    if let Some(target_id) = reservation.judge_target_id.as_deref() {
        validate_quality_text("judge_target_id", target_id)?;
    }
    if reservation.created_at_ms < 0 {
        return Err(StoreError("created_at_ms must be non-negative".to_string()));
    }
    Ok(())
}

fn validate_quality_finalization(finalization: &QualityJudgmentFinalization) -> Result<()> {
    validate_quality_text("judgment_id", &finalization.judgment_id)?;
    if let Some(target_id) = finalization.judge_target_id.as_deref() {
        validate_quality_text("judge_target_id", target_id)?;
    }
    if !matches!(
        finalization.status.as_str(),
        "scored" | "ungradable" | "invalid" | "failed" | "timeout"
    ) {
        return Err(StoreError("invalid terminal quality status".to_string()));
    }
    match (finalization.status.as_str(), finalization.score_norm) {
        ("scored", Some(score)) if score.is_finite() && (0.0..=1.0).contains(&score) => {}
        ("scored", _) => {
            return Err(StoreError(
                "scored quality judgment requires score_norm in [0,1]".to_string(),
            ));
        }
        (_, None) => {}
        (_, Some(_)) => {
            return Err(StoreError(
                "only scored quality judgment may carry score_norm".to_string(),
            ));
        }
    }
    if let Some(reason_code) = finalization.reason_code.as_deref() {
        validate_quality_code("reason_code", reason_code)?;
    }
    if finalization.completed_at_ms < 0 {
        return Err(StoreError(
            "completed_at_ms must be non-negative".to_string(),
        ));
    }
    Ok(())
}

/// Raw, unchecked column values as read straight off the `scorecard` table —
/// SQLite integers are always `i64`, so every numeric field here is `i64`
/// regardless of the checked/narrower type it must become in [`ScorecardRow`].
struct RawScorecardRow {
    target_id: String,
    class: String,
    scoreable_samples: i64,
    success_count: i64,
    truncated_count: i64,
    target_fail_count: i64,
    p50_latency_ms: i64,
    p95_latency_ms: i64,
    cost_per_success_micros: i64,
    error_histogram: String,
    consecutive_failures: i64,
    tier: i64,
    demoted_since_ms: Option<i64>,
    quality_ewma: Option<f64>,
    quality_samples: i64,
    quality_updated_at_ms: Option<i64>,
    quality_evaluator_id: Option<String>,
    updated_at_ms: i64,
    schema_ver: i64,
}

/// Checked decode of one raw persisted scorecard row (outcome-routing-v1
/// F13): any overflow/negative numeric conversion, an internally
/// inconsistent count (`success + truncated + target_fail != scoreable`), a
/// negative timestamp, unparseable histogram JSON, or an empty
/// `target_id`/`class` rejects the WHOLE row (`Err(<reason>)`) — the caller
/// then leaves NO map entry for it at all, rather than a default-prior
/// placeholder.
fn decode_scorecard_row(raw: RawScorecardRow) -> std::result::Result<ScorecardRow, &'static str> {
    if raw.target_id.trim().is_empty() {
        return Err("empty target_id");
    }
    if raw.class.trim().is_empty() {
        return Err("empty class");
    }
    let scoreable_samples =
        u32::try_from(raw.scoreable_samples).map_err(|_| "scoreable_samples overflow/negative")?;
    let success_count =
        u32::try_from(raw.success_count).map_err(|_| "success_count overflow/negative")?;
    let truncated_count =
        u32::try_from(raw.truncated_count).map_err(|_| "truncated_count overflow/negative")?;
    let target_fail_count =
        u32::try_from(raw.target_fail_count).map_err(|_| "target_fail_count overflow/negative")?;
    let p50_latency_ms =
        u32::try_from(raw.p50_latency_ms).map_err(|_| "p50_latency_ms overflow/negative")?;
    let p95_latency_ms =
        u32::try_from(raw.p95_latency_ms).map_err(|_| "p95_latency_ms overflow/negative")?;
    let cost_per_success_micros = u64::try_from(raw.cost_per_success_micros)
        .map_err(|_| "cost_per_success_micros overflow/negative")?;
    let consecutive_failures = u32::try_from(raw.consecutive_failures)
        .map_err(|_| "consecutive_failures overflow/negative")?;
    let tier = u8::try_from(raw.tier).map_err(|_| "tier overflow/negative")?;
    let quality_samples =
        u32::try_from(raw.quality_samples).map_err(|_| "quality_samples overflow/negative")?;
    let schema_ver = u32::try_from(raw.schema_ver).map_err(|_| "schema_ver overflow/negative")?;

    let sum = success_count
        .checked_add(truncated_count)
        .and_then(|s| s.checked_add(target_fail_count))
        .ok_or("success+truncated+target_fail overflow")?;
    if sum != scoreable_samples {
        return Err("success+truncated+target_fail != scoreable_samples");
    }
    if raw.updated_at_ms < 0 {
        return Err("updated_at_ms is negative");
    }
    if raw.demoted_since_ms.is_some_and(|ms| ms < 0) {
        return Err("demoted_since_ms is negative");
    }
    if raw.quality_updated_at_ms.is_some_and(|ms| ms < 0) {
        return Err("quality_updated_at_ms is negative");
    }
    if raw
        .quality_ewma
        .is_some_and(|quality| !quality.is_finite() || !(0.0..=1.0).contains(&quality))
    {
        return Err("quality_ewma is not finite or outside [0,1]");
    }
    if raw
        .quality_evaluator_id
        .as_deref()
        .is_some_and(|id| id.trim().is_empty())
    {
        return Err("quality_evaluator_id is empty");
    }
    if serde_json::from_str::<serde_json::Value>(&raw.error_histogram).is_err() {
        return Err("error_histogram is not valid JSON");
    }

    Ok(ScorecardRow {
        target_id: raw.target_id,
        class: raw.class,
        scoreable_samples,
        success_count,
        truncated_count,
        target_fail_count,
        p50_latency_ms,
        p95_latency_ms,
        cost_per_success_micros,
        error_histogram: raw.error_histogram,
        consecutive_failures,
        tier,
        demoted_since_ms: raw.demoted_since_ms,
        quality_ewma: raw.quality_ewma,
        quality_samples,
        quality_updated_at_ms: raw.quality_updated_at_ms,
        quality_evaluator_id: raw.quality_evaluator_id,
        updated_at_ms: raw.updated_at_ms,
        schema_ver,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "eval")]
    use sb_core::{CacheStatus, ExecutionTaskType, PrivacyClass};
    #[cfg(feature = "eval")]
    use sb_eval::{
        ArtifactKind, CaseStore, EvalArtifactRef, EvalCaseManifest, EvalEvidenceSnapshot,
        EvalFixtureRef, EvalMetric, EvalOutcome, EvalReportQuery, EvalRunIngest, EvalStore,
        EvidenceSource, PromptRef, RunStatus, SuccessCriterion, Verdict,
    };

    #[cfg(feature = "eval")]
    fn eval_case(case_id: &str, revision: &str, tags: &[&str]) -> EvalCaseManifest {
        EvalCaseManifest {
            schema_version: sb_eval::CASE_SCHEMA_VERSION.to_string(),
            case_id: case_id.to_string(),
            case_revision: revision.to_string(),
            task_type: ExecutionTaskType::Coding,
            privacy_level: PrivacyClass::Standard,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            fixture: EvalFixtureRef {
                kind: "git_repo".to_string(),
                uri: "https://example.invalid/repo.git".to_string(),
                revision: Some("abc123".to_string()),
                fingerprint: Some(format!("fixture-{case_id}-{revision}")),
            },
            prompt_ref: Some(PromptRef {
                kind: "sha256".to_string(),
                reference: format!("prompt-{case_id}-{revision}"),
                sha256: Some(format!("prompt-{case_id}-{revision}")),
            }),
            success_criteria: vec![SuccessCriterion {
                id: "tests".to_string(),
                kind: "tests_pass".to_string(),
                required: true,
                params: serde_json::json!({}),
            }],
            commands: Vec::new(),
            allowed_paths: vec!["src/**".to_string()],
            forbidden_paths: vec![".env".to_string()],
        }
    }

    #[cfg(feature = "eval")]
    fn eval_run(
        case_id: &str,
        revision: &str,
        source_run_id: &str,
        harness: &str,
        verdict: Verdict,
    ) -> EvalRunIngest {
        EvalRunIngest {
            schema_version: sb_eval::RUN_SCHEMA_VERSION.to_string(),
            run_id: None,
            source_run_id: Some(source_run_id.to_string()),
            case_id: case_id.to_string(),
            case_revision: revision.to_string(),
            harness: harness.to_string(),
            harness_version: Some("1.0.0".to_string()),
            strategy_id: "default".to_string(),
            strategy_version: Some("v1".to_string()),
            started_at_ms: Some(1_000),
            finished_at_ms: Some(3_000),
            job: None,
            receipt: None,
            harness_summary: None,
            status: RunStatus::Succeeded,
            outcome: EvalOutcome {
                verdict,
                source: EvidenceSource::MechanicalCheck,
                confidence: Some(0.9),
                checks: Vec::new(),
                evidence: Vec::new(),
            },
            metrics: vec![
                EvalMetric {
                    name: "latency_ms".to_string(),
                    value: 2_000.0,
                    unit: "ms".to_string(),
                    source: "harness".to_string(),
                },
                EvalMetric {
                    name: "cost_micros".to_string(),
                    value: 42_000.0,
                    unit: "micros_usd".to_string(),
                    source: "harness".to_string(),
                },
            ],
            artifacts: vec![EvalArtifactRef {
                kind: ArtifactKind::Trace,
                reference: format!("trace:{source_run_id}"),
                sha256: Some(format!("trace-sha-{source_run_id}")),
                privacy_level: PrivacyClass::Standard,
                metadata: serde_json::json!({ "trace_id": source_run_id }),
            }],
            human_outcomes: Vec::new(),
            retry_count: Some(1),
            cache_status: Some(CacheStatus::Hit),
        }
    }

    #[test]
    fn migrations_are_versioned() {
        let store = SqliteStore::in_memory().unwrap();

        assert_eq!(store.schema_versions().unwrap(), expected_schema_versions());
    }

    fn expected_schema_versions() -> Vec<i64> {
        #[cfg(feature = "eval")]
        {
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
        }
        #[cfg(not(feature = "eval"))]
        {
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 14, 15, 16, 17]
        }
    }

    #[cfg(feature = "eval")]
    #[test]
    fn eval_migration_adds_evidence_tables() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap();

        for table in [
            "eval_cases",
            "eval_case_tags",
            "eval_runs",
            "eval_outcomes",
            "eval_metrics",
            "eval_artifacts",
            "eval_evidence_snapshots",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "{table} table should exist");
        }
    }

    #[cfg(feature = "eval")]
    #[test]
    fn eval_case_and_run_round_trip_to_report() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .put_case(eval_case("react-bug-001", "rev-1", &["react"]))
            .unwrap();

        store
            .ingest_run(eval_run(
                "react-bug-001",
                "rev-1",
                "codex-1",
                "codex-cli",
                Verdict::Pass,
            ))
            .unwrap();
        store
            .ingest_run(eval_run(
                "react-bug-001",
                "rev-1",
                "codex-2",
                "codex-cli",
                Verdict::Fail,
            ))
            .unwrap();

        let report = store
            .report(EvalReportQuery {
                task_type: Some(ExecutionTaskType::Coding),
                tag: Some("react".to_string()),
                min_runs: 2,
                ..EvalReportQuery::default()
            })
            .unwrap();

        assert_eq!(report.rows.len(), 1);
        let row = &report.rows[0];
        assert_eq!(row.harness, "codex-cli");
        assert_eq!(row.runs, 2);
        assert_eq!(row.pass_count, 1);
        assert_eq!(row.fail_count, 1);
        assert_eq!(row.success_rate, Some(0.5));
        assert_eq!(row.median_latency_ms, Some(2_000));
        assert_eq!(row.median_cost_micros, Some(42_000));
        assert_eq!(row.retry_rate, Some(1.0));
        assert_eq!(row.cache_hit_rate, Some(1.0));
        assert!(!row.insufficient_sample);
    }

    #[cfg(feature = "eval")]
    #[test]
    fn eval_evidence_snapshot_publish_round_trips_named_current() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .put_case(eval_case("react-bug-001", "rev-1", &["react"]))
            .unwrap();
        store
            .ingest_run(eval_run(
                "react-bug-001",
                "rev-1",
                "codex-1",
                "codex-cli",
                Verdict::Pass,
            ))
            .unwrap();
        let query = EvalReportQuery {
            task_type: Some(ExecutionTaskType::Coding),
            tag: Some("react".to_string()),
            min_runs: 1,
            ..EvalReportQuery::default()
        };
        let report = store.report(query.clone()).unwrap();
        let snapshot = EvalEvidenceSnapshot::from_report(&query, report, 42_000);

        let record = store
            .publish_eval_evidence_snapshot("current", &snapshot)
            .unwrap();
        assert_eq!(record.name, "current");
        assert_eq!(record.snapshot_id, snapshot.snapshot_id);

        let loaded = store
            .get_eval_evidence_snapshot("current")
            .unwrap()
            .expect("current snapshot is published");
        assert_eq!(loaded, snapshot);
        let metadata = store
            .get_eval_evidence_snapshot_record("current")
            .unwrap()
            .expect("current metadata is published");
        assert_eq!(metadata.snapshot_id, snapshot.snapshot_id);
        assert_eq!(
            store.list_eval_evidence_snapshot_records().unwrap()[0].name,
            "current"
        );
    }

    #[cfg(feature = "eval")]
    #[test]
    fn eval_ingest_is_idempotent_by_harness_source_run_id() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .put_case(eval_case("react-bug-001", "rev-1", &["react"]))
            .unwrap();

        let first = store
            .ingest_run(eval_run(
                "react-bug-001",
                "rev-1",
                "same-source-run",
                "codex-cli",
                Verdict::Pass,
            ))
            .unwrap();
        let second = store
            .ingest_run(eval_run(
                "react-bug-001",
                "rev-1",
                "same-source-run",
                "codex-cli",
                Verdict::Fail,
            ))
            .unwrap();

        assert!(first.inserted);
        assert!(!second.inserted);
        assert_eq!(first.run_id, second.run_id);

        let report = store.report(EvalReportQuery::default()).unwrap();
        assert_eq!(report.rows[0].runs, 1);
        assert_eq!(report.rows[0].pass_count, 1);
    }

    #[cfg(feature = "eval")]
    #[test]
    fn eval_case_revision_and_tags_filter_reports() {
        let mut store = SqliteStore::in_memory().unwrap();
        store
            .put_case(eval_case("shared-case", "rev-1", &["react"]))
            .unwrap();
        store
            .put_case(eval_case("shared-case", "rev-2", &["swift"]))
            .unwrap();
        store
            .ingest_run(eval_run(
                "shared-case",
                "rev-1",
                "codex-react",
                "codex-cli",
                Verdict::Pass,
            ))
            .unwrap();
        store
            .ingest_run(eval_run(
                "shared-case",
                "rev-2",
                "codex-swift",
                "codex-cli",
                Verdict::Fail,
            ))
            .unwrap();

        let react_report = store
            .report(EvalReportQuery {
                task_type: Some(ExecutionTaskType::Coding),
                tag: Some("react".to_string()),
                min_runs: 1,
                ..EvalReportQuery::default()
            })
            .unwrap();
        assert_eq!(react_report.rows[0].runs, 1);
        assert_eq!(react_report.rows[0].pass_count, 1);

        let all_report = store.report(EvalReportQuery::default()).unwrap();
        assert_eq!(all_report.rows[0].runs, 2);
        assert_eq!(all_report.rows[0].pass_count, 1);
        assert_eq!(all_report.rows[0].fail_count, 1);
    }

    #[test]
    fn file_backed_store_runs_in_wal_mode() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "sb_store_wal_{}_{}.sqlite",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path_str = path.to_str().unwrap().to_string();
        let store = SqliteStore::open(&path_str).unwrap();
        let mode: String = store
            .conn()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            mode.to_lowercase(),
            "wal",
            "a file-backed store must run in WAL mode"
        );
        drop(store);
        let _ = std::fs::remove_file(&path_str);
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
    }

    fn scorecard_row(target_id: &str, class: &str) -> ScorecardRow {
        ScorecardRow {
            target_id: target_id.to_string(),
            class: class.to_string(),
            scoreable_samples: 42,
            success_count: 40,
            truncated_count: 1,
            target_fail_count: 1,
            p50_latency_ms: 1_200,
            p95_latency_ms: 3_100,
            cost_per_success_micros: 400,
            error_histogram: "{}".to_string(),
            consecutive_failures: 0,
            tier: 0,
            demoted_since_ms: None,
            quality_ewma: None,
            quality_samples: 0,
            quality_updated_at_ms: None,
            quality_evaluator_id: None,
            updated_at_ms: 1_700_000_000_000,
            schema_ver: 1,
        }
    }

    #[test]
    fn scorecard_migration_creates_table() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'scorecard')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "scorecard table should exist");
    }

    #[test]
    fn scorecard_round_trips_json_histogram_and_null_fields() {
        let store = SqliteStore::in_memory().unwrap();

        // Healthy row: JSON error histogram, never demoted (nulls stay null).
        let mut healthy = scorecard_row("openrouter/llama-3.3-70b", "any");
        healthy.error_histogram = "{\"timeout\":2,\"server_error\":1}".to_string();

        // Demoted row: populated demotion + coherent quality calibration.
        let mut demoted = scorecard_row("nvidia/minimax-m3", "any");
        demoted.tier = 1;
        demoted.demoted_since_ms = Some(1_700_000_500_000);
        demoted.quality_ewma = Some(0.82);
        demoted.quality_samples = 7;
        demoted.quality_updated_at_ms = Some(1_700_000_600_000);
        demoted.quality_evaluator_id = Some("quality-v1-deepseek".to_string());

        store
            .upsert_scorecard(&[healthy.clone(), demoted.clone()])
            .unwrap();

        let mut loaded = store.load_scorecard().unwrap();
        loaded.sort_by(|a, b| a.target_id.cmp(&b.target_id));
        let mut expected = vec![healthy, demoted];
        expected.sort_by(|a, b| a.target_id.cmp(&b.target_id));
        assert_eq!(loaded, expected, "round-tripped rows must match exactly");
    }

    #[test]
    fn scorecard_upsert_same_key_second_wins() {
        let store = SqliteStore::in_memory().unwrap();

        // Internally consistent rows (success+truncated+target_fail ==
        // scoreable_samples) -- F13 hardening now rejects inconsistent rows
        // on load, so the fixture must stay valid while still proving the
        // second upsert overwrites the first.
        let mut first = scorecard_row("zai/glm-4.6", "any");
        first.success_count = 10;
        first.truncated_count = 0;
        first.target_fail_count = 0;
        first.scoreable_samples = 10;
        store.upsert_scorecard(&[first]).unwrap();

        let mut second = scorecard_row("zai/glm-4.6", "any");
        second.success_count = 99;
        second.truncated_count = 0;
        second.target_fail_count = 0;
        second.scoreable_samples = 99;
        second.tier = 1;
        store.upsert_scorecard(&[second.clone()]).unwrap();

        let loaded = store.load_scorecard().unwrap();
        assert_eq!(loaded.len(), 1, "same (target_id, class) upserts in place");
        assert_eq!(loaded[0], second, "second upsert must win");
    }

    #[test]
    fn scorecard_load_rejects_corrupt_rows_and_leaves_zero_entries() {
        // F13: checked i64->u32/u64 conversions + full row validation. A
        // rejected row must leave NO trace in the loaded Vec (zero routing
        // influence), and one corrupt row must not break loading the OTHER
        // valid rows.
        let store = SqliteStore::in_memory().unwrap();

        // A normal, valid row alongside the corrupt ones -- proves a single
        // bad row doesn't poison the whole load.
        store
            .upsert_scorecard(&[scorecard_row("good/target", "any")])
            .unwrap();

        {
            let conn = store.conn.lock().unwrap();
            // Negative count (would silently wrap to u32::MAX via `as u32`).
            conn.execute(
                "INSERT INTO scorecard (target_id, class, scoreable_samples, success_count,
                    truncated_count, target_fail_count, p50_latency_ms, p95_latency_ms,
                    cost_per_success_micros, error_histogram, consecutive_failures, tier,
                    demoted_since_ms, quality_ewma, updated_at_ms, schema_ver)
                 VALUES ('negative/count', 'any', -5, 0, 0, 0, 0, 0, 0, '{}', 0, 0, NULL, NULL, 1700000000000, 1)",
                [],
            )
            .unwrap();
            // Overflows u32 (would silently truncate via `as u32`).
            conn.execute(
                "INSERT INTO scorecard (target_id, class, scoreable_samples, success_count,
                    truncated_count, target_fail_count, p50_latency_ms, p95_latency_ms,
                    cost_per_success_micros, error_histogram, consecutive_failures, tier,
                    demoted_since_ms, quality_ewma, updated_at_ms, schema_ver)
                 VALUES ('overflow/count', 'any', 10000000000, 10000000000, 0, 0, 0, 0, 0, '{}', 0, 0, NULL, NULL, 1700000000000, 1)",
                [],
            )
            .unwrap();
            // Unparseable histogram JSON.
            conn.execute(
                "INSERT INTO scorecard (target_id, class, scoreable_samples, success_count,
                    truncated_count, target_fail_count, p50_latency_ms, p95_latency_ms,
                    cost_per_success_micros, error_histogram, consecutive_failures, tier,
                    demoted_since_ms, quality_ewma, updated_at_ms, schema_ver)
                 VALUES ('bad/json', 'any', 5, 5, 0, 0, 0, 0, 0, 'not-json{{', 0, 0, NULL, NULL, 1700000000000, 1)",
                [],
            )
            .unwrap();
        }

        let loaded = store.load_scorecard().unwrap();
        let ids: Vec<&str> = loaded.iter().map(|r| r.target_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["good/target"],
            "every corrupt row is rejected (zero entries, zero influence); the valid row still loads"
        );
    }

    #[test]
    fn scorecard_migration_is_idempotent_on_existing_db() {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "sb_store_scorecard_migration_{}_{}.sqlite",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path_str = path.to_str().unwrap().to_string();

        {
            let store = SqliteStore::open(&path_str).unwrap();
            store
                .upsert_scorecard(&[scorecard_row("a/b", "any")])
                .unwrap();
        }
        // Reopening re-runs migrate(); version 14 must be a no-op the second
        // time, and previously-written data must survive it.
        let store = SqliteStore::open(&path_str).unwrap();
        assert!(store.schema_versions().unwrap().contains(&14));
        let loaded = store.load_scorecard().unwrap();
        assert_eq!(
            loaded.len(),
            1,
            "existing data survives a second migration pass"
        );

        drop(store);
        let _ = std::fs::remove_file(&path_str);
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
    }

    fn quality_reservation(
        id: usize,
        created_at_ms: i64,
        reserved_cost_micros: u64,
    ) -> QualityJudgmentReservation {
        QualityJudgmentReservation {
            judgment_id: format!("judgment-{id:04}"),
            judge_request_id: format!("judge-request-{id:04}"),
            served_request_id: format!("served-request-{id:04}"),
            served_target_id: "mock/served".into(),
            class: "any".into(),
            sample_revision: 7,
            judge_revision: 8,
            evaluator_id: "quality-v1-mock".into(),
            rubric_version: "quality-v1".into(),
            judge_target_id: None,
            input_chars: 128,
            output_chars: 256,
            reserved_cost_micros,
            created_at_ms,
        }
    }

    #[test]
    fn quality_migration_adds_metadata_only_wal_and_scorecard_columns() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("PRAGMA table_info(quality_judgments)")
            .unwrap();
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            columns,
            vec![
                "judgment_id",
                "judge_request_id",
                "served_request_id",
                "served_target_id",
                "class",
                "sample_revision",
                "judge_revision",
                "evaluator_id",
                "rubric_version",
                "judge_target_id",
                "status",
                "score_norm",
                "reason_code",
                "input_chars",
                "output_chars",
                "reserved_cost_micros",
                "actual_cost_micros",
                "created_at_ms",
                "completed_at_ms",
            ]
        );
        for forbidden in ["body", "prompt", "response", "rationale"] {
            assert!(columns.iter().all(|column| !column.contains(forbidden)));
        }
        assert!(SqliteStore::column_exists(&conn, "scorecard", "quality_samples").unwrap());
        assert!(SqliteStore::column_exists(&conn, "scorecard", "quality_updated_at_ms").unwrap());
        assert!(SqliteStore::column_exists(&conn, "scorecard", "quality_evaluator_id").unwrap());
    }

    #[test]
    fn quality_migration_decodes_pre_v15_scorecard_row_with_empty_quality() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
               version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at_ms INTEGER NOT NULL
             );
             INSERT INTO schema_migrations(version, name, applied_at_ms) VALUES
               (1,'v1',1),(2,'v2',1),(3,'v3',1),(4,'v4',1),(5,'v5',1),(6,'v6',1),
               (7,'v7',1),(8,'v8',1),(9,'v9',1),(10,'v10',1),(11,'v11',1),
               (12,'v12',1),(13,'v13',1),(14,'outcome_scorecard',1);
             CREATE TABLE scorecard (
               target_id TEXT NOT NULL, class TEXT NOT NULL DEFAULT 'any',
               scoreable_samples INTEGER NOT NULL, success_count INTEGER NOT NULL,
               truncated_count INTEGER NOT NULL, target_fail_count INTEGER NOT NULL,
               p50_latency_ms INTEGER NOT NULL, p95_latency_ms INTEGER NOT NULL,
               cost_per_success_micros INTEGER NOT NULL, error_histogram TEXT NOT NULL DEFAULT '{}',
               consecutive_failures INTEGER NOT NULL DEFAULT 0, tier INTEGER NOT NULL DEFAULT 0,
               demoted_since_ms INTEGER, quality_ewma REAL, updated_at_ms INTEGER NOT NULL,
               schema_ver INTEGER NOT NULL DEFAULT 1, PRIMARY KEY(target_id, class)
             );
             INSERT INTO scorecard VALUES
               ('mock/old','any',1,1,0,0,10,10,1,'{}',0,0,NULL,NULL,1000,1);",
        )
        .unwrap();
        let store = SqliteStore {
            conn: Mutex::new(conn),
        };
        store.migrate().unwrap();
        let row = store.load_scorecard().unwrap().pop().unwrap();
        assert_eq!(row.quality_ewma, None);
        assert_eq!(row.quality_samples, 0);
        assert_eq!(row.quality_updated_at_ms, None);
        assert_eq!(row.quality_evaluator_id, None);
    }

    #[test]
    fn quality_reserve_finalize_budget_replay_abandon_and_idempotence() {
        let store = SqliteStore::in_memory().unwrap();
        let first = quality_reservation(1, 1_000, 40);
        assert_eq!(
            store.reserve_quality_judgment(&first, 2, 100, 0).unwrap(),
            QualityJudgmentReserveOutcome::Reserved
        );
        assert_eq!(
            store.reserve_quality_judgment(&first, 2, 100, 0).unwrap(),
            QualityJudgmentReserveOutcome::Duplicate
        );
        let too_costly = quality_reservation(2, 1_001, 61);
        assert_eq!(
            store
                .reserve_quality_judgment(&too_costly, 2, 100, 0)
                .unwrap(),
            QualityJudgmentReserveOutcome::BudgetExceeded(QualityJudgmentBudget {
                attempted: 1,
                cost_micros: 40,
            })
        );
        let second = quality_reservation(2, 1_001, 60);
        assert_eq!(
            store.reserve_quality_judgment(&second, 2, 100, 0).unwrap(),
            QualityJudgmentReserveOutcome::Reserved
        );
        assert_eq!(
            store
                .reserve_quality_judgment(&quality_reservation(3, 1_002, 0), 2, 100, 0)
                .unwrap(),
            QualityJudgmentReserveOutcome::BudgetExceeded(QualityJudgmentBudget {
                attempted: 2,
                cost_micros: 100,
            })
        );

        let finalization = QualityJudgmentFinalization {
            judgment_id: first.judgment_id.clone(),
            judge_target_id: Some("mock/judge".into()),
            status: "scored".into(),
            score_norm: Some(0.75),
            reason_code: Some("pass".into()),
            actual_cost_micros: Some(25),
            completed_at_ms: 2_000,
        };
        assert!(store.finalize_quality_judgment(&finalization).unwrap());
        assert!(!store.finalize_quality_judgment(&finalization).unwrap());
        assert_eq!(
            store.quality_judgment_budget(0).unwrap(),
            QualityJudgmentBudget {
                attempted: 2,
                cost_micros: 100
            },
            "actual cost below the reservation must not release the conservative cap"
        );
        let replay = store
            .replay_quality_judgments("quality-v1-mock", 0)
            .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].score_norm, Some(0.75));
        assert_eq!(replay[0].judge_target_id.as_deref(), Some("mock/judge"));
        assert_eq!(store.abandon_started_quality_judgments(3_000).unwrap(), 1);
        let recent = store.recent_quality_judgments(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].status, "abandoned");
        assert_eq!(recent[1].status, "scored");
    }

    #[test]
    fn quality_judgments_prune_to_newest_two_thousand() {
        let store = SqliteStore::in_memory().unwrap();
        for id in 0..2_001 {
            assert_eq!(
                store
                    .reserve_quality_judgment(
                        &quality_reservation(id, id as i64, 0),
                        10_000,
                        10_000,
                        0,
                    )
                    .unwrap(),
                QualityJudgmentReserveOutcome::Reserved
            );
        }
        let rows = store.recent_quality_judgments(5_000).unwrap();
        assert_eq!(rows.len(), 2_000);
        assert_eq!(rows.first().unwrap().judgment_id, "judgment-2000");
        assert_eq!(rows.last().unwrap().judgment_id, "judgment-0001");
    }

    #[test]
    fn quality_migration_does_not_modify_eval_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
               version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_at_ms INTEGER NOT NULL
             );
             INSERT INTO schema_migrations(version, name, applied_at_ms) VALUES
               (1,'v1',1),(2,'v2',1),(3,'v3',1),(4,'v4',1),(5,'v5',1),(6,'v6',1),
               (7,'v7',1),(8,'v8',1),(9,'v9',1),(10,'v10',1),(11,'v11',1),
               (12,'v12',1),(13,'v13',1),(14,'outcome_scorecard',1);
             CREATE TABLE scorecard (
               target_id TEXT NOT NULL, class TEXT NOT NULL, scoreable_samples INTEGER NOT NULL,
               success_count INTEGER NOT NULL, truncated_count INTEGER NOT NULL,
               target_fail_count INTEGER NOT NULL, p50_latency_ms INTEGER NOT NULL,
               p95_latency_ms INTEGER NOT NULL, cost_per_success_micros INTEGER NOT NULL,
               error_histogram TEXT NOT NULL, consecutive_failures INTEGER NOT NULL,
               tier INTEGER NOT NULL, demoted_since_ms INTEGER, quality_ewma REAL,
               updated_at_ms INTEGER NOT NULL, schema_ver INTEGER NOT NULL,
               PRIMARY KEY(target_id, class)
             );
             CREATE TABLE eval_runs (sentinel TEXT PRIMARY KEY);",
        )
        .unwrap();
        let before: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='eval_runs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let store = SqliteStore {
            conn: Mutex::new(conn),
        };
        store.migrate().unwrap();
        let conn = store.conn.lock().unwrap();
        let after: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='eval_runs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, before);
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

        assert_eq!(store.schema_versions().unwrap(), expected_schema_versions());
        let conn = store.conn.lock().unwrap();
        assert!(SqliteStore::column_exists(&conn, "usage", "tenant").unwrap());
        assert!(SqliteStore::column_exists(&conn, "audit", "source").unwrap());
        assert!(SqliteStore::column_exists(&conn, "idempotency_inflight", "lease_id").unwrap());
    }

    #[test]
    fn migration_dedupes_legacy_usage_before_unique_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_migrations (
                 version       INTEGER PRIMARY KEY,
                 name          TEXT    NOT NULL,
                 applied_at_ms INTEGER NOT NULL
             );
             INSERT INTO schema_migrations (version, name, applied_at_ms)
             VALUES
                (1, 'initial_control_plane_state', 1),
                (2, 'usage_tenant_attribution', 1),
                (3, 'audit_context', 1),
                (4, 'coordination_leases', 1),
                (5, 'global_admission_leases', 1),
                (6, 'idempotency_inflight_lease_owner', 1);
             CREATE TABLE revisions (
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
                 actor_role  TEXT,
                 actor_tenant TEXT,
                 actor_project TEXT,
                 source      TEXT,
                 object_id   TEXT,
                 created_at  INTEGER NOT NULL
             );
             CREATE TABLE usage (
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
             INSERT INTO usage
                (request_id, provider_id, model, account_id, tenant, cost_micros,
                 input_tokens, output_tokens, latency_ms, streamed, created_at)
             VALUES
                ('req-1', 'mock', 'mock/echo', 'a', 'acme', 100, 1, 1, 10, 0, 1000),
                ('req-1', 'mock', 'mock/echo', 'a', 'acme', 999, 1, 1, 10, 0, 2000),
                ('req-2', 'mock', 'mock/echo', 'a', 'acme', 50, 1, 1, 10, 0, 3000);
             CREATE TABLE idempotency (
                 key          TEXT    PRIMARY KEY,
                 fingerprint  TEXT    NOT NULL,
                 status       INTEGER NOT NULL,
                 content_type TEXT    NOT NULL,
                 body         TEXT    NOT NULL,
                 created_at   INTEGER NOT NULL
             );
             CREATE TABLE idempotency_inflight (
                 key         TEXT PRIMARY KEY,
                 fingerprint TEXT NOT NULL,
                 lease_id    TEXT,
                 created_at  INTEGER NOT NULL,
                 expires_at  INTEGER NOT NULL
             );
             CREATE TABLE tenant_slots (
                 slot_id    TEXT PRIMARY KEY,
                 tenant     TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL
             );
             CREATE TABLE admission_slots (
                 slot_id    TEXT PRIMARY KEY,
                 created_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL
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

        assert_eq!(store.schema_versions().unwrap(), expected_schema_versions());
        let roll = store.usage_rollup().unwrap();
        assert_eq!(roll.requests, 2);
        assert_eq!(roll.total_cost_micros, 150);
        assert_eq!(
            roll.by_tenant,
            vec![("acme".to_string(), 2, 150)],
            "first usage event for req-1 is retained before the unique index is created"
        );
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
            project: Some(format!("{tenant}-api")),
            cost_micros: Some(cost),
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 20,
            streamed: false,
            created_at_ms: 1000,
            ..UsageEvent::default()
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
        assert_eq!(recent[0].project.as_deref(), Some("globex-api"));
    }

    #[test]
    fn usage_schema_tracks_whether_cost_is_known() {
        let store = SqliteStore::in_memory().unwrap();
        assert!(store.schema_versions().unwrap().contains(&16));
        let conn = store.conn().unwrap();
        assert!(SqliteStore::column_exists(&conn, "usage", "cost_known").unwrap());
        assert!(SqliteStore::column_exists(&conn, "usage", "workload_kind").unwrap());
        assert!(SqliteStore::column_exists(&conn, "usage", "pricing_unit").unwrap());
        assert!(SqliteStore::column_exists(&conn, "usage", "units_consumed").unwrap());
    }

    #[test]
    fn unknown_workload_cost_round_trips_as_null() {
        let store = SqliteStore::in_memory().unwrap();
        let conn = store.conn().unwrap();
        conn.execute(
            "INSERT INTO usage
             (request_id, provider_id, model, cost_micros, cost_known,
              input_tokens, output_tokens, latency_ms, streamed, created_at,
              workload_kind, pricing_unit, units_consumed)
             VALUES ('job-unknown', 'fal', 'fal-ai/unknown', 0, 0,
                     0, 0, 10, 0, 1000, 'image_generation', NULL, NULL)",
            [],
        )
        .unwrap();
        drop(conn);

        let recent = store.recent_usage(1).unwrap();
        let value = serde_json::to_value(&recent[0]).unwrap();
        assert!(value["cost_micros"].is_null());
        assert_eq!(value["workload_kind"], "image_generation");
    }

    #[test]
    fn usage_events_are_deduped_by_request_id() {
        let store = SqliteStore::in_memory().unwrap();
        let ev = |cost: u64| UsageEvent {
            request_id: "req-1".into(),
            provider_id: "mock".into(),
            model: "mock/echo".into(),
            account_id: Some("a".into()),
            tenant: Some("acme".into()),
            project: Some("api".into()),
            cost_micros: Some(cost),
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 20,
            streamed: false,
            created_at_ms: 1000,
            ..UsageEvent::default()
        };

        store.record_usage(&ev(100)).unwrap();
        store.record_usage(&ev(999)).unwrap();

        let roll = store.usage_rollup().unwrap();
        assert_eq!(
            roll.requests, 1,
            "duplicate request_id is first-writer-wins"
        );
        assert_eq!(
            roll.total_cost_micros, 100,
            "duplicate usage must not overwrite the first event"
        );
        assert_eq!(roll.by_provider, vec![("mock".into(), 1, 100)]);
        assert_eq!(roll.by_tenant, vec![("acme".into(), 1, 100)]);

        let recent = store.recent_usage(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].request_id, "req-1");
        assert_eq!(recent[0].cost_micros, Some(100));
        assert_eq!(recent[0].project.as_deref(), Some("api"));
    }

    #[test]
    fn usage_events_round_trip_energy_metadata() {
        let store = SqliteStore::in_memory().unwrap();
        let ev = UsageEvent {
            request_id: "req-energy".into(),
            provider_id: "neuralwatt".into(),
            model: "glm-5.2".into(),
            cost_micros: Some(100),
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 20,
            streamed: false,
            energy_joules: Some(5.23),
            energy_kwh: Some(0.00000145),
            energy_duration_seconds: Some(0.0183),
            energy_measurement_available: Some(true),
            energy_attribution_method: Some("time_weighted".into()),
            energy_kwh_consumed: Some(0.00000145),
            energy_kwh_charged: Some(0.00000145),
            energy_accounting_method: Some("energy".into()),
            energy_total_cost_usd: Some(0.01),
            created_at_ms: 1000,
            ..UsageEvent::default()
        };
        store.record_usage(&ev).unwrap();

        let roll = store.usage_rollup().unwrap();
        assert_eq!(roll.energy.requests_with_energy, 1);
        assert_eq!(roll.energy.energy_joules, 5.23);
        assert_eq!(roll.energy.energy_kwh, 0.00000145);
        assert_eq!(roll.energy_by_provider[0].key, "neuralwatt");

        let recent = store.recent_usage(1).unwrap();
        assert_eq!(
            recent[0].energy_attribution_method.as_deref(),
            Some("time_weighted")
        );
        assert_eq!(
            recent[0].energy_accounting_method.as_deref(),
            Some("energy")
        );
    }

    #[test]
    fn usage_events_round_trip_cached_input_tokens() {
        let store = SqliteStore::in_memory().unwrap();
        let ev = UsageEvent {
            request_id: "req-cache".into(),
            provider_id: "anthropic".into(),
            model: "claude-3-5-sonnet-latest".into(),
            cost_micros: Some(100),
            input_tokens: 1000,
            output_tokens: 200,
            cached_input_tokens: 800,
            latency_ms: 20,
            streamed: false,
            created_at_ms: 1000,
            ..UsageEvent::default()
        };
        store.record_usage(&ev).unwrap();

        let recent = store.recent_usage(1).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].cached_input_tokens, 800,
            "cached_input_tokens must survive the store round-trip"
        );
        assert_eq!(recent[0].input_tokens, 1000);
    }

    #[test]
    fn record_usage_reports_inserted_or_duplicate_ignored() {
        let store = SqliteStore::in_memory().unwrap();
        let ev = UsageEvent {
            request_id: "req-1".into(),
            provider_id: "mock".into(),
            model: "mock/echo".into(),
            account_id: Some("a".into()),
            tenant: Some("acme".into()),
            project: Some("api".into()),
            cost_micros: Some(100),
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 20,
            streamed: false,
            created_at_ms: 1000,
            ..UsageEvent::default()
        };

        assert_eq!(
            store.record_usage(&ev).unwrap(),
            UsageWriteOutcome::Inserted
        );
        assert_eq!(
            store.record_usage(&ev).unwrap(),
            UsageWriteOutcome::DuplicateIgnored
        );
    }

    #[test]
    fn trace_events_record_query_and_dedupe() {
        let store = SqliteStore::in_memory().unwrap();
        let ev = TraceEvent {
            request_id: "req-1".into(),
            revision: 7,
            tenant: Some("acme".into()),
            project: Some("api".into()),
            session_id: Some("sess-1".into()),
            inbound_model: "coding".into(),
            route: "default".into(),
            selected_target: Some("mock/echo".into()),
            final_status: 200,
            total_latency_ms: 42,
            streamed: false,
            cost_micros: 123,
            attempted_providers: vec!["mock".into()],
            created_at_ms: 2000,
            trace_json: r#"{"request_id":"req-1"}"#.into(),
        };

        assert!(store.record_trace(&ev).unwrap());
        assert!(!store.record_trace(&ev).unwrap());

        let one = store.get_trace("req-1").unwrap().expect("stored trace");
        assert_eq!(one.session_id.as_deref(), Some("sess-1"));
        assert_eq!(one.attempted_providers, vec!["mock"]);

        let hits = store
            .query_traces(&TraceQuery {
                limit: 10,
                tenant: Some("acme".into()),
                session_id: Some("sess-1".into()),
                model: Some("coding".into()),
                status: Some(200),
                since_ms: Some(1000),
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].request_id, "req-1");

        let misses = store
            .query_traces(&TraceQuery {
                limit: 10,
                tenant: Some("beta".into()),
                ..TraceQuery::default()
            })
            .unwrap();
        assert!(misses.is_empty());
    }

    #[test]
    fn native_history_imports_record_metadata_only_sources() {
        let store = SqliteStore::in_memory().unwrap();
        let import = NativeHistoryImportRecord {
            import_id: "import-1".into(),
            client_filter: "all".into(),
            metadata_only: true,
            stores_prompts: false,
            stores_responses: false,
            stores_local_paths: false,
            source_count: 2,
            existing_source_count: 1,
            file_count: 1,
            record_count: 3,
            parse_error_count: 0,
            byte_count: 120,
            warnings_json: "[]".into(),
            created_at_ms: 2000,
        };
        let import_id = import.import_id.clone();
        let source = |source_id: &str, client: &str, exists: bool| NativeHistorySourceRecord {
            import_id: import_id.clone(),
            source_id: source_id.into(),
            client: client.into(),
            kind: "jsonl".into(),
            parser: "jsonl_metadata".into(),
            path_pattern: "${HOME}/.codex/history.jsonl".into(),
            path_id: "path-redacted".into(),
            exists,
            truncated: false,
            skipped_file_count: 0,
            file_count: u64::from(exists),
            record_count: if exists { 3 } else { 0 },
            parse_error_count: 0,
            byte_count: if exists { 120 } else { 0 },
            modified_at_ms_min: Some(100),
            modified_at_ms_max: Some(200),
            observed_at_min: Some("10".into()),
            observed_at_max: Some("20".into()),
            tables_json: "[]".into(),
            errors_json: "[]".into(),
        };
        let batch = NativeHistoryImportBatch {
            import: import.clone(),
            sources: vec![
                source("codex_history_jsonl", "codex", true),
                source("claude_history_jsonl", "claude-code", false),
            ],
        };

        let write = store.record_native_history_import(&batch).unwrap();
        assert_eq!(write.source_rows_written, 2);

        let recent = store.recent_native_history_imports(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].import_id, "import-1");
        assert!(recent[0].metadata_only);
        assert!(!recent[0].stores_prompts);
        assert!(!recent[0].stores_responses);
        assert!(!recent[0].stores_local_paths);

        let sources = store.native_history_sources("import-1").unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources
            .iter()
            .all(|source| source.path_id == "path-redacted"));
        assert!(sources
            .iter()
            .all(|source| !source.path_pattern.contains("/Users/")));

        let replaced = NativeHistoryImportBatch {
            import,
            sources: vec![source("codex_history_jsonl", "codex", true)],
        };
        let write = store.record_native_history_import(&replaced).unwrap();
        assert_eq!(write.source_rows_written, 1);
        assert_eq!(
            store.native_history_sources("import-1").unwrap().len(),
            1,
            "replacing the same import id should replace its source snapshot"
        );
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
            store
                .idempotency_begin("k1", "fp", "lease-1", 60_000)
                .unwrap(),
            IdempotencyBegin::Claimed
        );
        assert_eq!(
            store
                .idempotency_begin("k1", "fp", "lease-2", 60_000)
                .unwrap(),
            IdempotencyBegin::InProgress
        );
        assert_eq!(
            store
                .idempotency_begin("k1", "different", "lease-3", 60_000)
                .unwrap(),
            IdempotencyBegin::Mismatch
        );

        assert!(store.idempotency_release("k1", "lease-1").unwrap());
        assert_eq!(
            store
                .idempotency_begin("k1", "fp", "lease-4", 60_000)
                .unwrap(),
            IdempotencyBegin::Claimed
        );
        assert!(store.idempotency_release("k1", "lease-4").unwrap());

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
            store
                .idempotency_begin("k1", "fp", "lease-5", 60_000)
                .unwrap(),
            IdempotencyBegin::Replay(rec)
        );
        assert_eq!(
            store
                .idempotency_begin("k1", "different", "lease-6", 60_000)
                .unwrap(),
            IdempotencyBegin::Mismatch
        );
    }

    #[test]
    fn idempotency_begin_expires_abandoned_claims() {
        let store = SqliteStore::in_memory().unwrap();

        assert_eq!(
            store.idempotency_begin("k1", "fp", "old", 0).unwrap(),
            IdempotencyBegin::Claimed
        );
        assert_eq!(
            store.idempotency_begin("k1", "fp", "new", 60_000).unwrap(),
            IdempotencyBegin::Claimed,
            "expired in-flight claim should not block a new process forever"
        );
    }

    #[test]
    fn idempotency_stale_release_does_not_clear_new_claim() {
        let store = SqliteStore::in_memory().unwrap();

        assert_eq!(
            store.idempotency_begin("k1", "fp", "old", 0).unwrap(),
            IdempotencyBegin::Claimed
        );
        assert_eq!(
            store.idempotency_begin("k1", "fp", "new", 60_000).unwrap(),
            IdempotencyBegin::Claimed,
            "a new process can claim after the old owner expires"
        );

        assert!(
            !store.idempotency_renew("k1", "old", 60_000).unwrap(),
            "the old owner must not be able to renew the newer claim"
        );
        assert!(
            !store.idempotency_release("k1", "old").unwrap(),
            "the old owner must not be able to release the newer claim"
        );
        assert_eq!(
            store
                .idempotency_begin("k1", "fp", "third", 60_000)
                .unwrap(),
            IdempotencyBegin::InProgress,
            "the old owner must not be able to release the newer claim"
        );
        assert!(store.idempotency_release("k1", "new").unwrap());
        assert_eq!(
            store
                .idempotency_begin("k1", "fp", "third", 60_000)
                .unwrap(),
            IdempotencyBegin::Claimed,
            "the current owner can still release its own claim"
        );
    }

    #[test]
    fn idempotency_renew_extends_only_active_claims() {
        let store = SqliteStore::in_memory().unwrap();

        assert_eq!(
            store
                .idempotency_begin("k1", "fp", "lease-1", 60_000)
                .unwrap(),
            IdempotencyBegin::Claimed
        );
        assert!(store.idempotency_renew("k1", "lease-1", 60_000).unwrap());
        assert!(store.idempotency_release("k1", "lease-1").unwrap());
        assert!(
            !store.idempotency_renew("k1", "lease-1", 60_000).unwrap(),
            "released claims should not renew"
        );

        assert_eq!(
            store.idempotency_begin("k2", "fp", "lease-2", 0).unwrap(),
            IdempotencyBegin::Claimed
        );
        assert!(
            !store.idempotency_renew("k2", "lease-2", 60_000).unwrap(),
            "expired claims should not be revived by renewal"
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

    #[test]
    fn tenant_slot_renew_extends_only_active_slots() {
        let store = SqliteStore::in_memory().unwrap();

        assert!(store
            .tenant_slot_acquire("acme", "slot-1", 1, 60_000)
            .unwrap());
        assert!(store.tenant_slot_renew("slot-1", 60_000).unwrap());
        store.tenant_slot_release("slot-1").unwrap();
        assert!(
            !store.tenant_slot_renew("slot-1", 60_000).unwrap(),
            "released slots should not renew"
        );

        assert!(store.tenant_slot_acquire("acme", "slot-2", 1, 0).unwrap());
        assert!(
            !store.tenant_slot_renew("slot-2", 60_000).unwrap(),
            "expired slots should not be revived by renewal"
        );
    }

    #[test]
    fn admission_slots_enforce_limit_and_release() {
        let store = SqliteStore::in_memory().unwrap();

        assert!(store.admission_slot_acquire("slot-1", 1, 60_000).unwrap());
        assert_eq!(store.admission_slot_count().unwrap(), 1);
        assert!(
            !store.admission_slot_acquire("slot-2", 1, 60_000).unwrap(),
            "second active slot is over the global max"
        );

        store.admission_slot_release("slot-1").unwrap();
        assert_eq!(store.admission_slot_count().unwrap(), 0);
        assert!(store.admission_slot_acquire("slot-3", 1, 60_000).unwrap());
    }

    #[test]
    fn admission_slots_expire_abandoned_rows() {
        let store = SqliteStore::in_memory().unwrap();

        assert!(store.admission_slot_acquire("slot-1", 1, 0).unwrap());
        assert_eq!(
            store.admission_slot_count().unwrap(),
            0,
            "expired slot should be cleaned before counting"
        );
        assert!(store.admission_slot_acquire("slot-2", 1, 60_000).unwrap());
    }

    #[test]
    fn admission_slot_renew_extends_only_active_slots() {
        let store = SqliteStore::in_memory().unwrap();

        assert!(store.admission_slot_acquire("slot-1", 1, 60_000).unwrap());
        assert!(store.admission_slot_renew("slot-1", 60_000).unwrap());
        store.admission_slot_release("slot-1").unwrap();
        assert!(
            !store.admission_slot_renew("slot-1", 60_000).unwrap(),
            "released slots should not renew"
        );

        assert!(store.admission_slot_acquire("slot-2", 1, 0).unwrap());
        assert!(
            !store.admission_slot_renew("slot-2", 60_000).unwrap(),
            "expired slots should not be revived by renewal"
        );
    }
}
