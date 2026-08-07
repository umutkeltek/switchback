//! Protected raw body evidence storage for Switchback.
//!
//! This crate is intentionally separate from `sb-trace`: traces stay
//! metadata-only, while body-bearing records are explicit, protected, hashed,
//! compressed, and indexed here.
//!
//! Lifecycle (added for internal-SSD growth bounding):
//! - `index.sqlite` is metadata-only and grows without bound; [`BodyLogger::gc`]
//!   gives it fail-closed, batched retention for UTC days that have verifiably
//!   left the local archive (day dir absent under a *mounted* archive root).
//! - New `tap-bodies.jsonl` records are day-routed into the archive day
//!   partition (or a spool day-file when the archive is unavailable) so they
//!   ride the existing NAS sync-then-prune path instead of growing one flat
//!   local file forever. The configured legacy sink is frozen (never appended).
//! - [`BodyLogger::status`] reports spool -> archive completeness truthfully,
//!   with filesystem-exact spool backlog independent of the sqlite size.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Month, OffsetDateTime};

mod backup;
mod pressure;
mod reclaim;

pub use backup::{
    CaptureBackupPlan, CaptureBackupPlanItem, CaptureBackupReceipt, CaptureBackupReceiptItem,
    CaptureLegacyBackupBlocker, CaptureLegacyBackupPlan, CaptureLegacyBackupPlanItem,
    CaptureLegacyBackupReceipt, CaptureLegacyBackupReceiptItem,
};
pub use pressure::{CaptureMode, PressureObservation, PressureStatus, CAPTURE_GAP_SCHEMA};
pub use reclaim::{
    CaptureReclaimOptions, CaptureReclaimPlan, CaptureReclaimPlanItem, CaptureReclaimProof,
    CaptureReclaimProofItem, CaptureReclaimReport,
};

static NEXT_EVENT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_SEGMENT_ID: AtomicU64 = AtomicU64::new(1);

const DEFAULT_INLINE_THRESHOLD_BYTES: u64 = 256 * 1024;
/// Above this DB size, exact `COUNT(*)` is too expensive, so `status()` reports
/// `MAX(rowid)` approximations (flagged approximate) instead.
const PRECISE_STATUS_DB_SIZE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
/// Hot-path busy timeout. Capture writes must stay bounded: when the index is
/// locked it is better to fail fast (and spool or drop) than to stall request
/// processing behind another writer.
const SQLITE_BUSY_TIMEOUT_MS: u64 = 250;
/// Maintenance busy timeout. Reclaim and receipt projection run against a live
/// gateway that writes capture continuously; 250ms was short enough that
/// `sb body reclaim` died mid-run with "database is locked" on a busy host.
/// Background maintenance can afford to wait its turn for the writer instead.
const SQLITE_MAINTENANCE_BUSY_TIMEOUT_MS: u64 = 5_000;
const ZSTD_LEVEL: i32 = 3;
const DAY_MS: i64 = 86_400_000;
const CAPTURE_SEGMENT_SCHEMA: &str = "switchback/capture-segment@1";
const CAPTURE_SEGMENT_MAGIC: &[u8; 8] = b"SBCAP001";
const CAPTURE_RECORD_MAGIC: &[u8; 4] = b"REC1";
const CAPTURE_RECORD_HEADER_BYTES: u64 = 4 + 8 + 8 + 4;
const CAPTURE_SEGMENT_MAX_BYTES: u64 = 64 * 1024 * 1024;
const CAPTURE_SEGMENT_ROTATE_MS: i64 = 15 * 60 * 1_000;
const CAPTURE_RECORD_MAX_BYTES: u64 = 512 * 1024 * 1024;
const CURRENT_INDEX_FILE: &str = "index-v2.sqlite";
const LEGACY_SEGMENT_INDEX_FILE: &str = "index.sqlite";
const LEGACY_ROOT_INDEX_FILE: &str = "body-index.sqlite";
/// Directory holding content-addressed body payloads, under both the spool and
/// each archive day. Named so segment traversal can prune it — see
/// [`collect_segment_files`].
const BLOB_DIR_NAME: &str = "blobs";

/// Default retention window: keep this many recent UTC days locally. Older days
/// whose archive day dir is absent (exported + pruned) are GC candidates.
pub const DEFAULT_KEEP_DAYS: u64 = 3;
/// Default bounded-batch size for retention deletes (never one giant txn).
pub const DEFAULT_GC_BATCH_SIZE: u64 = 20_000;
/// Env override for the retention window.
pub const KEEP_DAYS_ENV: &str = "SWITCHBACK_BODY_KEEP_DAYS";

pub type Result<T> = std::result::Result<T, BodyLogError>;

#[derive(Debug)]
pub struct BodyLogError(String);

impl BodyLogError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for BodyLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "body log error: {}", self.0)
    }
}

impl std::error::Error for BodyLogError {}

impl From<std::io::Error> for BodyLogError {
    fn from(value: std::io::Error) -> Self {
        BodyLogError(value.to_string())
    }
}

impl From<rusqlite::Error> for BodyLogError {
    fn from(value: rusqlite::Error) -> Self {
        BodyLogError(value.to_string())
    }
}

impl From<serde_json::Error> for BodyLogError {
    fn from(value: serde_json::Error) -> Self {
        BodyLogError(value.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct BodyLoggerConfig {
    pub state_dir: PathBuf,
    pub archive_root: PathBuf,
    pub legacy_jsonl: Option<PathBuf>,
    pub inline_threshold_bytes: u64,
}

impl BodyLoggerConfig {
    pub fn from_legacy_sink(path: PathBuf) -> Self {
        let state_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            archive_root: default_archive_root(&state_dir),
            state_dir,
            legacy_jsonl: Some(path),
            inline_threshold_bytes: DEFAULT_INLINE_THRESHOLD_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BodyLogger {
    config: BodyLoggerConfig,
    index_path: PathBuf,
    spool_dir: PathBuf,
    segment_writer: Arc<Mutex<SegmentWriterState>>,
    pressure: Arc<Mutex<pressure::PressureController>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureStage {
    ClientInbound,
    HeadroomInbound,
    HeadroomOutbound,
    UpstreamResponse,
    ClientResponse,
    ClientSession,
    DerivedFact,
}

impl CaptureStage {
    fn as_str(self) -> &'static str {
        match self {
            CaptureStage::ClientInbound => "client_inbound",
            CaptureStage::HeadroomInbound => "headroom_inbound",
            CaptureStage::HeadroomOutbound => "headroom_outbound",
            CaptureStage::UpstreamResponse => "upstream_response",
            CaptureStage::ClientResponse => "client_response",
            CaptureStage::ClientSession => "client_session",
            CaptureStage::DerivedFact => "derived_fact",
        }
    }
}

#[derive(Debug, Clone)]
pub struct BodyEventInput {
    pub request_id: String,
    pub capture_stage: CaptureStage,
    pub protocol: String,
    pub upstream: Option<String>,
    pub model: Option<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub metadata: serde_json::Value,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BodyCaptureGap {
    pub reason: String,
    pub body_sha256: String,
    pub body_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BodyRecord {
    pub event_id: String,
    pub request_id: String,
    pub observed_at_unix_ms: i64,
    pub capture_stage: String,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub body_sha256: String,
    pub body_bytes: u64,
    pub compressed_bytes: u64,
    pub archive_path: String,
    pub storage: String,
    pub protected: bool,
    pub redaction_state: String,
    pub threshold_shrunk: bool,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct BodyStatus {
    pub schema: String,
    pub status: String,
    pub index_path: String,
    pub index_bytes: u64,
    pub index_reclaimable_bytes: u64,
    pub state_dir: String,
    pub archive_root: String,
    pub legacy_jsonl: Option<String>,
    pub archive_available: bool,
    pub events: u64,
    pub blobs: u64,
    /// True when `events`/`blobs` are `MAX(rowid)` approximations (large DB),
    /// false when they are exact `COUNT(*)`.
    pub counts_approximate: bool,
    pub spool_backlog: u64,
    pub spool_backlog_exact: bool,
    pub last_event_at_unix_ms: Option<i64>,
    pub capture_events_last_minute: u64,
    pub capture_body_bytes_last_minute: u64,
    pub local_segment_count: u64,
    pub segment_backlog_bytes: u64,
    pub capture_queue_depth: u64,
    pub capture_queue_drops: u64,
    pub backup_age_ms: Option<i64>,
    pub verified_through_day: Option<String>,
    /// UTC day (YYYY-MM-DD) at/below which days become retention candidates.
    pub retention_cutoff_day: String,
    /// Count of local archive day dirs (`YYYY/MM/DD`) currently present.
    pub local_archive_day_dirs: u64,
    /// Oldest local archive day dir (YYYY-MM-DD), if any.
    pub oldest_local_day_dir: Option<String>,
    /// Size in bytes of the frozen legacy jsonl artifact, if present.
    pub legacy_jsonl_bytes: Option<u64>,
    pub protected_paths: Vec<String>,
    pub pressure: PressureStatus,
}

#[derive(Debug, Clone, Default)]
pub struct BodyEventQuery {
    pub request_id: Option<String>,
    pub capture_stage: Option<CaptureStage>,
    pub protocol: Option<String>,
    pub limit: usize,
}

/// Options for [`BodyLogger::gc`]. Dry-run by default: mutations require
/// `confirm = true` (no way to mutate without it).
#[derive(Debug, Clone)]
pub struct GcOptions {
    pub keep_days: u64,
    /// Must be true to mutate; false = dry-run (report candidates only).
    pub confirm: bool,
    /// Only drain the spool into day partitions; skip retention deletes.
    pub drain_only: bool,
    pub batch_size: u64,
}

impl Default for GcOptions {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_KEEP_DAYS,
            confirm: false,
            drain_only: false,
            batch_size: DEFAULT_GC_BATCH_SIZE,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GcDayCandidate {
    pub day: String,
    pub event_rows: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    /// `Some` reason means the command refused (fail-closed) and mutated nothing.
    pub refused: Option<String>,
    pub dry_run: bool,
    pub drain_only: bool,
    pub keep_days: u64,
    pub cutoff_day: String,
    pub archive_available: bool,
    pub candidate_days: Vec<GcDayCandidate>,
    pub events_deleted: u64,
    pub blobs_deleted: u64,
    /// In dry-run these are "would drain" counts; with `confirm` they are actual.
    pub spool_blobs_drained: u64,
    pub spool_segments_drained: u64,
    pub spool_day_files_drained: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactReport {
    /// `Some` reason means compaction refused (guard held or unconfirmed).
    pub refused: Option<String>,
    pub events_before: u64,
    pub blobs_before: u64,
    pub events_after: u64,
    pub blobs_after: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

#[derive(Debug, Clone)]
struct BlobLocation {
    path: PathBuf,
    day_dir: Option<PathBuf>,
    storage: &'static str,
    archive_available: bool,
}

#[derive(Debug, Default)]
struct SegmentWriterState {
    active: Option<ActiveSegment>,
}

impl Drop for SegmentWriterState {
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            let _ = seal_active_segment(active);
        }
    }
}

#[derive(Debug, Clone)]
struct ActiveSegment {
    path: PathBuf,
    storage: &'static str,
    bucket_start_ms: i64,
    last_write_at_unix_ms: i64,
    lock_file: Arc<fs::File>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SegmentManifest {
    schema_version: String,
    segment_file: String,
    segment_sha256: String,
    segment_bytes: u64,
    record_count: u64,
    #[serde(default)]
    body_bytes: u64,
    first_observed_at_unix_ms: Option<i64>,
    last_observed_at_unix_ms: Option<i64>,
    sealed: bool,
}

#[derive(Debug)]
struct SegmentFrame {
    record: BodyRecord,
    body: Vec<u8>,
}

impl BodyLogger {
    pub fn new(config: BodyLoggerConfig) -> Result<Self> {
        if config.inline_threshold_bytes == 0 {
            return Err(BodyLogError::new(
                "inline_threshold_bytes must be greater than zero",
            ));
        }
        fs::create_dir_all(&config.state_dir)?;
        let body_dir = config.state_dir.join("body");
        ensure_private_directory(&body_dir)?;
        backup::reconcile_latest_backup_receipt(&config.state_dir)?;
        let spool_dir = body_dir.join("spool");
        ensure_private_directory_tree(&body_dir, &spool_dir)?;
        if config.archive_root.is_dir() {
            ensure_private_directory(&config.archive_root)?;
        }
        if let Some(path) = config.legacy_jsonl.as_ref().and_then(|p| p.parent()) {
            fs::create_dir_all(path)?;
        }
        let index_path = body_dir.join(CURRENT_INDEX_FILE);
        if index_path.exists() {
            set_private_file(&index_path)?;
        }
        let rebuild_index = !index_path.exists();
        let logger = Self {
            config,
            index_path,
            spool_dir,
            segment_writer: Arc::new(Mutex::new(SegmentWriterState::default())),
            pressure: Arc::new(Mutex::new(pressure::PressureController::load(&body_dir))),
        };
        logger.init_db()?;
        {
            let _operation = backup::backup_operation_lock(&logger.config.state_dir)?;
            logger.recover_segments(rebuild_index)?;
            logger.rebuild_backup_projection()?;
        }
        Ok(logger)
    }

    pub fn from_legacy_sink(path: PathBuf) -> Result<Self> {
        Self::new(BodyLoggerConfig::from_legacy_sink(path))
    }

    pub fn open_existing(config: BodyLoggerConfig) -> Result<Option<Self>> {
        let body_dir = config.state_dir.join("body");
        if body_dir.is_dir() {
            ensure_private_directory(&body_dir)?;
        }
        backup::reconcile_latest_backup_receipt(&config.state_dir)?;
        let index_path = existing_index_path(&config.state_dir);
        if !index_path.exists() {
            return Ok(None);
        }
        set_private_file(&index_path)?;
        let spool_dir = body_dir.join("spool");
        if spool_dir.is_dir() {
            ensure_private_directory_tree(&body_dir, &spool_dir)?;
        }
        if config.archive_root.is_dir() {
            ensure_private_directory(&config.archive_root)?;
        }
        let logger = Self {
            config,
            index_path,
            spool_dir,
            segment_writer: Arc::new(Mutex::new(SegmentWriterState::default())),
            pressure: Arc::new(Mutex::new(pressure::PressureController::load(&body_dir))),
        };
        if logger.uses_current_index() {
            logger.init_db()?;
            let _operation = backup::backup_operation_lock(&logger.config.state_dir)?;
            logger.rebuild_backup_projection()?;
        }
        Ok(Some(logger))
    }

    pub(crate) fn uses_current_index(&self) -> bool {
        self.index_path == self.config.state_dir.join("body").join(CURRENT_INDEX_FILE)
    }

    pub fn record(&self, input: BodyEventInput) -> Result<BodyRecord> {
        self.record_at(input, now_unix_ms())
    }

    /// Evaluate the live local-disk and backup receipt state before accepting
    /// body bytes into an asynchronous capture queue.
    pub fn evaluate_pressure(&self) -> Result<PressureStatus> {
        let now = now_unix_ms();
        // The segment projection is shared by every tap process. Recompute from
        // it on every admission so one process cannot hide another's backlog.
        let unbacked_bytes = self.projected_unbacked_bytes()?;
        let mut pressure = self
            .pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?;
        let mut observation =
            pressure.observe(&self.config.state_dir, &self.config.archive_root)?;
        observation.unbacked_bytes = unbacked_bytes;
        pressure.evaluate(observation, now)
    }

    /// Deterministic pressure seam for tests and external health probes.
    #[doc(hidden)]
    pub fn evaluate_pressure_at(
        &self,
        observation: PressureObservation,
        now_unix_ms: i64,
    ) -> Result<PressureStatus> {
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .evaluate(observation, now_unix_ms)
    }

    pub fn pressure_status(&self) -> Result<PressureStatus> {
        Ok(self
            .pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .status())
    }

    /// Persist an explicit capture gap without persisting payload bytes.
    pub fn record_metadata_only(
        &self,
        input: BodyEventInput,
        admission: &PressureStatus,
    ) -> Result<BodyRecord> {
        let gap = BodyCaptureGap {
            reason: "pressure_policy".to_string(),
            body_sha256: sha256_hex(&input.body),
            body_bytes: input.body.len() as u64,
        };
        self.record_gap_inner(input, admission, gap, "metadata_only_pressure")
    }

    /// Persist a typed capture gap produced by a bounded streaming observer.
    /// The full payload never enters this API; its incremental hash and exact
    /// byte count preserve identity without widening memory or queue bounds.
    pub fn record_capture_gap(
        &self,
        input: BodyEventInput,
        admission: &PressureStatus,
        gap: BodyCaptureGap,
    ) -> Result<BodyRecord> {
        self.record_gap_inner(input, admission, gap, "metadata_only_capture_gap")
    }

    fn record_gap_inner(
        &self,
        input: BodyEventInput,
        admission: &PressureStatus,
        gap: BodyCaptureGap,
        redaction_state: &str,
    ) -> Result<BodyRecord> {
        let observed_at_unix_ms = now_unix_ms();
        let record = BodyRecord {
            event_id: new_event_id(observed_at_unix_ms),
            request_id: input.request_id,
            observed_at_unix_ms,
            capture_stage: input.capture_stage.as_str().to_string(),
            protocol: input.protocol,
            upstream: input.upstream,
            model: input.model,
            status: input.status,
            content_type: input.content_type,
            body_sha256: gap.body_sha256.clone(),
            body_bytes: gap.body_bytes,
            compressed_bytes: 0,
            archive_path: String::new(),
            storage: "metadata_only".to_string(),
            protected: false,
            redaction_state: redaction_state.to_string(),
            threshold_shrunk: false,
            metadata: serde_json::json!({
                "schema": CAPTURE_GAP_SCHEMA,
                "pressure_reasons": admission.reasons,
                "gap": gap,
                "capture_metadata": input.metadata,
            }),
        };
        let conn = open_index_connection(&self.index_path)?;
        insert_event_only_on(&conn, &record)?;
        let metadata_only_events = metadata_only_event_count(&conn)?;
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .set_metadata_only_events(metadata_only_events);
        Ok(record)
    }

    /// Fail closed for future captures while allowing inference traffic to
    /// continue when the full-wire writer fails.
    pub fn mark_capture_writer_failed(&self, reason: &str) -> Result<PressureStatus> {
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .mark_writer_failure(now_unix_ms(), reason)
    }

    pub fn note_capture_queue_enqueued(&self) -> Result<()> {
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .note_queue_enqueued();
        Ok(())
    }

    pub fn note_capture_queue_dequeued(&self) -> Result<()> {
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .note_queue_dequeued();
        Ok(())
    }

    pub fn note_capture_queue_drop(&self) -> Result<()> {
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .note_queue_drop(now_unix_ms())
    }

    /// Seal the segment currently owned by this logger, if any. A sealed
    /// segment has an immutable checksum manifest and is eligible for backup or
    /// spool drain. The next record starts a fresh segment.
    pub fn seal_active(&self) -> Result<Option<PathBuf>> {
        let active = self
            .segment_writer
            .lock()
            .map_err(|_| BodyLogError::new("capture segment writer lock poisoned"))?
            .active
            .take();
        let Some(active) = active else {
            return Ok(None);
        };
        let path = active.path.clone();
        seal_active_segment(active)?;
        Ok(Some(path))
    }

    /// Seal the current appendable segment once it has been idle for the
    /// configured interval. This is the low-volume rotation path used by the
    /// capture worker so a final segment does not wait forever for another
    /// request before it becomes checksum-stable and backup-eligible.
    #[doc(hidden)]
    pub fn seal_idle_at(&self, now_unix_ms: i64, minimum_idle_ms: i64) -> Result<Option<PathBuf>> {
        let mut writer = self
            .segment_writer
            .lock()
            .map_err(|_| BodyLogError::new("capture segment writer lock poisoned"))?;
        let should_seal = writer.active.as_ref().is_some_and(|active| {
            now_unix_ms.saturating_sub(active.last_write_at_unix_ms) >= minimum_idle_ms.max(0)
        });
        if !should_seal {
            return Ok(None);
        }
        let active = writer
            .active
            .take()
            .ok_or_else(|| BodyLogError::new("capture segment disappeared before idle seal"))?;
        let path = active.path.clone();
        seal_active_segment(active)?;
        Ok(Some(path))
    }

    /// Record a capture with an explicit observed-at timestamp.
    ///
    /// Production always calls [`BodyLogger::record`] (which stamps "now"); this
    /// seam exists so lifecycle tests can place records on specific UTC days
    /// through the real write path (blob placement + day-routing) instead of
    /// hand-crafting rows.
    #[doc(hidden)]
    pub fn record_at(&self, input: BodyEventInput, observed_at_unix_ms: i64) -> Result<BodyRecord> {
        self.record_segmented_at(input, observed_at_unix_ms)
    }

    #[allow(dead_code)]
    fn record_legacy_at(
        &self,
        input: BodyEventInput,
        observed_at_unix_ms: i64,
    ) -> Result<BodyRecord> {
        let now_ms = observed_at_unix_ms;
        let body_sha256 = sha256_hex(&input.body);
        let compressed = zstd::stream::encode_all(input.body.as_slice(), ZSTD_LEVEL)?;
        let location = self.blob_location(now_ms, &body_sha256);

        if let Some(parent) = location.path.parent() {
            let base = location
                .day_dir
                .as_deref()
                .map(|_| self.config.archive_root.as_path())
                .unwrap_or(self.spool_dir.as_path());
            ensure_private_directory_tree(base, parent)?;
        }
        if !location.path.exists() {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            set_owner_only(&mut options);
            let mut file = options.open(&location.path)?;
            file.write_all(&compressed)?;
        }
        set_private_file(&location.path)?;

        let record = BodyRecord {
            event_id: new_event_id(now_ms),
            request_id: input.request_id,
            observed_at_unix_ms: now_ms,
            capture_stage: input.capture_stage.as_str().to_string(),
            protocol: input.protocol,
            upstream: input.upstream,
            model: input.model,
            status: input.status,
            content_type: input.content_type,
            body_sha256,
            body_bytes: input.body.len() as u64,
            compressed_bytes: compressed.len() as u64,
            archive_path: location.path.to_string_lossy().into_owned(),
            storage: location.storage.to_string(),
            protected: true,
            redaction_state: "raw_local".to_string(),
            threshold_shrunk: (input.body.len() as u64) > self.config.inline_threshold_bytes,
            metadata: input.metadata,
        };

        self.insert_record(&record)?;
        self.route_tap_body_event(&record, &location)?;
        if location.archive_available {
            if let Some(day_dir) = &location.day_dir {
                self.append_archive_event(day_dir, &record)?;
            }
        }
        Ok(record)
    }

    fn record_segmented_at(
        &self,
        input: BodyEventInput,
        observed_at_unix_ms: i64,
    ) -> Result<BodyRecord> {
        // Lock SQLite before appending. A transient DB lock therefore creates
        // no duplicate frame when the lossless capture worker retries.
        let mut conn = open_index_connection(&self.index_path)?;
        let transaction =
            conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut record = BodyRecord {
            event_id: new_event_id(observed_at_unix_ms),
            request_id: input.request_id,
            observed_at_unix_ms,
            capture_stage: input.capture_stage.as_str().to_string(),
            protocol: input.protocol,
            upstream: input.upstream,
            model: input.model,
            status: input.status,
            content_type: input.content_type,
            body_sha256: sha256_hex(&input.body),
            body_bytes: input.body.len() as u64,
            compressed_bytes: 0,
            archive_path: String::new(),
            storage: String::new(),
            protected: true,
            redaction_state: "raw_local".to_string(),
            threshold_shrunk: (input.body.len() as u64) > self.config.inline_threshold_bytes,
            metadata: input.metadata,
        };
        self.append_segment_frame(&mut record, &input.body)?;
        insert_record_on(&transaction, &record)?;
        upsert_segment_projection_on(&transaction, &record)?;
        transaction.commit()?;
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .note_full_capture(record.body_bytes);
        Ok(record)
    }

    pub fn read_blob(&self, body_sha256: &str) -> Result<Vec<u8>> {
        let conn = open_index_connection(&self.index_path)?;
        let segment_location: Option<(String, String)> = conn
            .query_row(
                "SELECT storage, archive_path FROM body_blobs WHERE body_sha256 = ?1",
                params![body_sha256],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((storage, path)) = segment_location {
            if storage == "archive_segment" || storage == "spool_segment" {
                return read_body_from_segment(Path::new(&path), body_sha256);
            }
        }
        let conn = open_index_connection(&self.index_path)?;
        let path: Option<String> = conn
            .query_row(
                "SELECT archive_path FROM body_blobs WHERE body_sha256 = ?1",
                params![body_sha256],
                |row| row.get(0),
            )
            .optional()?;
        let path = path.ok_or_else(|| BodyLogError::new("body blob not indexed"))?;
        let compressed = fs::read(path)?;
        Ok(zstd::stream::decode_all(compressed.as_slice())?)
    }

    pub fn events_for_request(&self, request_id: &str) -> Result<Vec<BodyRecord>> {
        self.query_events(BodyEventQuery {
            request_id: Some(request_id.to_string()),
            limit: 100,
            ..BodyEventQuery::default()
        })
    }

    pub fn latest_events(&self, limit: usize) -> Result<Vec<BodyRecord>> {
        self.query_events(BodyEventQuery {
            limit,
            ..BodyEventQuery::default()
        })
    }

    pub fn query_events(&self, query: BodyEventQuery) -> Result<Vec<BodyRecord>> {
        let limit = query.limit.clamp(1, 1000) as i64;
        let conn = open_index_connection(&self.index_path)?;
        match (
            query.request_id.as_deref(),
            query.capture_stage,
            query.protocol.as_deref(),
        ) {
            (Some(request_id), Some(stage), Some(protocol)) => query_records(
                &conn,
                "WHERE request_id = ?1 AND capture_stage = ?2 AND protocol = ?3",
                params![request_id, stage.as_str(), protocol, limit],
            ),
            (Some(request_id), Some(stage), None) => query_records(
                &conn,
                "WHERE request_id = ?1 AND capture_stage = ?2",
                params![request_id, stage.as_str(), limit],
            ),
            (Some(request_id), None, Some(protocol)) => query_records(
                &conn,
                "WHERE request_id = ?1 AND protocol = ?2",
                params![request_id, protocol, limit],
            ),
            (Some(request_id), None, None) => {
                query_records(&conn, "WHERE request_id = ?1", params![request_id, limit])
            }
            (None, Some(stage), Some(protocol)) => query_records(
                &conn,
                "WHERE capture_stage = ?1 AND protocol = ?2",
                params![stage.as_str(), protocol, limit],
            ),
            (None, Some(stage), None) => query_records(
                &conn,
                "WHERE capture_stage = ?1",
                params![stage.as_str(), limit],
            ),
            (None, None, Some(protocol)) => {
                query_records(&conn, "WHERE protocol = ?1", params![protocol, limit])
            }
            (None, None, None) => query_records(&conn, "", params![limit]),
        }
    }

    pub fn status(&self) -> Result<BodyStatus> {
        self.status_with_precise_limit(PRECISE_STATUS_DB_SIZE_LIMIT_BYTES)
    }

    pub fn status_refreshed(&self) -> Result<BodyStatus> {
        self.evaluate_pressure()?;
        self.status()
    }

    /// Status with an explicit "precise counts" DB-size threshold. Above the
    /// threshold, `events`/`blobs` are `MAX(rowid)` approximations flagged
    /// `counts_approximate`; below it they are exact `COUNT(*)`. Production uses
    /// [`BodyLogger::status`]; the threshold is exposed so tests can exercise
    /// the large-DB path without a multi-hundred-MB fixture.
    pub fn status_with_precise_limit(&self, precise_size_limit_bytes: u64) -> Result<BodyStatus> {
        let conn = open_index_connection(&self.index_path)?;
        let db_bytes = fs::metadata(&self.index_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let page_size = conn.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?;
        let freelist_pages =
            conn.query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0))?;
        let index_reclaimable_bytes = (page_size.max(0) as u64)
            .saturating_mul(freelist_pages.max(0) as u64)
            .min(db_bytes);
        let counts_approximate = db_bytes > precise_size_limit_bytes;
        let (events, blobs) = if counts_approximate {
            (
                append_only_rows(&conn, "body_events")?,
                append_only_rows(&conn, "body_blobs")?,
            )
        } else {
            (
                exact_rows(&conn, "body_events")?,
                exact_rows(&conn, "body_blobs")?,
            )
        };
        let last_event_at_unix_ms = conn
            .query_row(
                "SELECT MAX(observed_at_unix_ms) FROM body_events",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let minute_cutoff = now_unix_ms().saturating_sub(60 * 1_000);
        let (capture_events_last_minute, capture_body_bytes_last_minute) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(body_bytes), 0)
             FROM body_events
             WHERE observed_at_unix_ms >= ?1",
            params![minute_cutoff],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let local_segment_count =
            conn.query_row("SELECT COUNT(*) FROM body_segments", [], |row| {
                row.get::<_, i64>(0)
            })?;

        // Spool backlog is a cheap filesystem walk, exact and independent of the
        // sqlite size. `exact` only becomes false if the walk itself errors.
        let (spool_backlog, spool_backlog_exact) = match count_spool_backlog(&self.spool_dir) {
            Ok(count) => (count, true),
            Err(_) => (0, false),
        };

        let archive_available = archive_root_available(&self.config.archive_root);
        let keep_days = env_keep_days();
        let cutoff_ms = retention_cutoff_ms(now_unix_ms(), keep_days);
        let (local_archive_day_dirs, oldest_local_day_dir) = if archive_available {
            count_local_day_dirs(&self.config.archive_root)
        } else {
            (0, None)
        };
        let legacy_jsonl_bytes = self
            .config
            .legacy_jsonl
            .as_ref()
            .and_then(|path| fs::metadata(path).ok())
            .map(|metadata| metadata.len());

        let mut protected_paths = vec![
            self.index_path.to_string_lossy().into_owned(),
            self.spool_dir.to_string_lossy().into_owned(),
            self.config.archive_root.to_string_lossy().into_owned(),
        ];
        if let Some(path) = &self.config.legacy_jsonl {
            protected_paths.push(path.to_string_lossy().into_owned());
        }
        let mut pressure = self
            .pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?;
        if !counts_approximate {
            pressure.set_metadata_only_events(metadata_only_event_count(&conn)?);
        }
        let pressure = pressure.status();
        let segment_backlog_bytes = pressure.unbacked_bytes;
        let capture_queue_depth = pressure.queue_depth;
        let capture_queue_drops = pressure.queue_drops;
        let backup_age_ms = pressure.backup_age_ms;
        let verified_through_day = pressure.verified_through_day.clone();

        Ok(BodyStatus {
            schema: "switchback/body-status@2".to_string(),
            status: body_status_text(archive_available, spool_backlog, spool_backlog_exact)
                .to_string(),
            index_path: self.index_path.to_string_lossy().into_owned(),
            index_bytes: db_bytes,
            index_reclaimable_bytes,
            state_dir: self.config.state_dir.to_string_lossy().into_owned(),
            archive_root: self.config.archive_root.to_string_lossy().into_owned(),
            legacy_jsonl: self
                .config
                .legacy_jsonl
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            archive_available,
            events,
            blobs,
            counts_approximate,
            spool_backlog,
            spool_backlog_exact,
            last_event_at_unix_ms,
            capture_events_last_minute: capture_events_last_minute.max(0) as u64,
            capture_body_bytes_last_minute: capture_body_bytes_last_minute.max(0) as u64,
            local_segment_count: local_segment_count.max(0) as u64,
            segment_backlog_bytes,
            capture_queue_depth,
            capture_queue_drops,
            backup_age_ms,
            verified_through_day,
            retention_cutoff_day: format_day_ms(cutoff_ms),
            local_archive_day_dirs,
            oldest_local_day_dir,
            legacy_jsonl_bytes,
            protected_paths,
            pressure,
        })
    }

    pub fn status_for_config(config: BodyLoggerConfig) -> Result<BodyStatus> {
        let body_dir = config.state_dir.join("body");
        let index_path = existing_index_path(&config.state_dir);
        let spool_dir = body_dir.join("spool");
        if !index_path.exists() {
            let pressure = pressure::PressureController::load(&body_dir).status();
            let archive_available = archive_root_available(&config.archive_root);
            let keep_days = env_keep_days();
            let cutoff_ms = retention_cutoff_ms(now_unix_ms(), keep_days);
            let (local_archive_day_dirs, oldest_local_day_dir) = if archive_available {
                count_local_day_dirs(&config.archive_root)
            } else {
                (0, None)
            };
            let legacy_jsonl_bytes = config
                .legacy_jsonl
                .as_ref()
                .and_then(|path| fs::metadata(path).ok())
                .map(|metadata| metadata.len());
            let mut protected_paths = vec![
                index_path.to_string_lossy().into_owned(),
                spool_dir.to_string_lossy().into_owned(),
                config.archive_root.to_string_lossy().into_owned(),
            ];
            if let Some(path) = &config.legacy_jsonl {
                protected_paths.push(path.to_string_lossy().into_owned());
            }
            return Ok(BodyStatus {
                schema: "switchback/body-status@2".to_string(),
                status: body_status_text(archive_available, 0, true).to_string(),
                index_path: index_path.to_string_lossy().into_owned(),
                index_bytes: 0,
                index_reclaimable_bytes: 0,
                state_dir: config.state_dir.to_string_lossy().into_owned(),
                archive_root: config.archive_root.to_string_lossy().into_owned(),
                legacy_jsonl: config
                    .legacy_jsonl
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                archive_available,
                events: 0,
                blobs: 0,
                counts_approximate: false,
                spool_backlog: 0,
                spool_backlog_exact: true,
                last_event_at_unix_ms: None,
                capture_events_last_minute: 0,
                capture_body_bytes_last_minute: 0,
                local_segment_count: 0,
                segment_backlog_bytes: pressure.unbacked_bytes,
                capture_queue_depth: pressure.queue_depth,
                capture_queue_drops: pressure.queue_drops,
                backup_age_ms: pressure.backup_age_ms,
                verified_through_day: pressure.verified_through_day.clone(),
                retention_cutoff_day: format_day_ms(cutoff_ms),
                local_archive_day_dirs,
                oldest_local_day_dir,
                legacy_jsonl_bytes,
                protected_paths,
                pressure,
            });
        }
        let logger = BodyLogger {
            config,
            index_path,
            spool_dir,
            segment_writer: Arc::new(Mutex::new(SegmentWriterState::default())),
            pressure: Arc::new(Mutex::new(pressure::PressureController::load(&body_dir))),
        };
        if logger.uses_current_index() {
            logger.init_db()?;
        }
        logger.status_refreshed()
    }

    /// Fail-closed retention GC for `index.sqlite` plus spool drain.
    ///
    /// Dry-run by default; mutates only with `opts.confirm`. Refuses entirely
    /// (mutating nothing) unless the archive root is mounted — absence of a day
    /// dir must never be conflated with an unmounted volume.
    pub fn gc(&self, opts: GcOptions) -> Result<GcReport> {
        let _operation = backup::backup_operation_lock(&self.config.state_dir)?;
        let now_ms = now_unix_ms();
        let archive_available = archive_root_available(&self.config.archive_root);
        let cutoff_ms = retention_cutoff_ms(now_ms, opts.keep_days);
        let mut report = GcReport {
            refused: None,
            dry_run: !opts.confirm,
            drain_only: opts.drain_only,
            keep_days: opts.keep_days,
            cutoff_day: format_day_ms(cutoff_ms),
            archive_available,
            candidate_days: Vec::new(),
            events_deleted: 0,
            blobs_deleted: 0,
            spool_blobs_drained: 0,
            spool_segments_drained: 0,
            spool_day_files_drained: 0,
        };
        if !archive_available {
            report.refused = Some(format!(
                "archive root not mounted at {}; refusing GC/drain (fail-closed)",
                self.config.archive_root.display()
            ));
            return Ok(report);
        }

        let batch = opts.batch_size.max(1);
        let mut conn = open_index_connection(&self.index_path)?;

        if !opts.drain_only {
            let candidate_days = self.collect_candidate_days(&conn, cutoff_ms, &mut report)?;
            if opts.confirm && !candidate_days.is_empty() {
                // Over-credit guard: a missing day-partition dir is NOT proof of
                // export. The index row may only be pruned when a backup receipt
                // proves the day's segments reached TrueNAS (the prune side of
                // the existing sync-then-prune seam). Days without receipt-gated
                // proof are kept on disk — the over-credit case.
                let verified = backup::verified_receipt_state(&backup::backup_dir(
                    &self.config.state_dir,
                ))?
                .0;
                let receipt_gated = self
                    .filter_receipt_gated_candidate_days(&conn, &candidate_days, &verified)?;
                report.events_deleted =
                    self.delete_candidate_events(&conn, &receipt_gated, batch)?;
                report.blobs_deleted =
                    self.delete_candidate_blobs(&conn, &receipt_gated, batch)?;
            }
        }

        if opts.confirm {
            self.drain_spool(&mut conn, now_ms, &mut report)?;
        } else {
            self.count_spool_pending(&mut report)?;
        }

        Ok(report)
    }

    /// Candidate UTC days: strictly older than the cutoff AND whose archive day
    /// dir is absent under the (mounted) archive root. Returns the set of day
    /// starts (unix ms) and populates `report.candidate_days` (rows > 0 only).
    fn collect_candidate_days(
        &self,
        conn: &Connection,
        cutoff_ms: i64,
        report: &mut GcReport,
    ) -> Result<HashSet<i64>> {
        let min_obs: Option<i64> = conn.query_row(
            "SELECT MIN(observed_at_unix_ms) FROM body_events",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let Some(min_obs) = min_obs else {
            return Ok(HashSet::new());
        };
        let mut candidates = HashSet::new();
        let mut day_start = day_floor_ms(min_obs);
        while day_start < cutoff_ms {
            if !self.day_dir(day_start).exists() {
                let day_end = day_start + DAY_MS;
                let rows: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM body_events \
                     WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms < ?2 \
                       AND storage NOT IN ('spool', 'spool_segment')",
                    params![day_start, day_end],
                    |row| row.get(0),
                )?;
                if rows > 0 {
                    report.candidate_days.push(GcDayCandidate {
                        day: format_day_ms(day_start),
                        event_rows: rows,
                    });
                }
                candidates.insert(day_start);
            }
            day_start += DAY_MS;
        }
        Ok(candidates)
    }

    /// Over-credit guard: keep only candidate days whose segments are
    /// provably exported to TrueNAS via a backup receipt. Empty `verified` set
    /// yields an empty result, which renders the GC a no-op for retention.
    /// The day-partition dir absence is necessary but not sufficient: the
    /// 2026-07-11 single-line JSONL case shows that the local dir can be
    /// pruned out-of-band without the data ever having left the Mac.
    fn filter_receipt_gated_candidate_days(
        &self,
        conn: &Connection,
        candidate_days: &HashSet<i64>,
        verified_segment_sha256: &HashSet<String>,
    ) -> Result<HashSet<i64>> {
        let mut kept = HashSet::new();
        for &day_start in candidate_days {
            let day_end = day_start + DAY_MS;
            // Collect the segment sha256s covering this candidate day. A
            // day is only safe to GC when every covering segment is recorded
            // in a backup receipt.
            let mut segment_stmt = conn.prepare(
                "SELECT DISTINCT segment_sha256 FROM body_segments \
                 WHERE first_observed_at_unix_ms < ?1 \
                   AND last_observed_at_unix_ms >= ?2 \
                   AND segment_sha256 IS NOT NULL \
                   AND segment_sha256 != ''",
            )?;
            let mut all_covered = true;
            let mut has_any = false;
            let rows = segment_stmt.query_map(params![day_end, day_start], |row| {
                row.get::<_, String>(0)
            })?;
            for row in rows {
                let sha = row?;
                if !verified_segment_sha256.contains(&sha) {
                    all_covered = false;
                    break;
                }
                has_any = true;
            }
            if all_covered && has_any {
                kept.insert(day_start);
            }
        }
        Ok(kept)
    }

    fn delete_candidate_events(
        &self,
        conn: &Connection,
        candidate_days: &HashSet<i64>,
        batch: u64,
    ) -> Result<u64> {
        let mut total = 0u64;
        for &day_start in candidate_days {
            let day_end = day_start + DAY_MS;
            loop {
                // Bounded batch: rowid subquery avoids the compile-time
                // SQLITE_ENABLE_UPDATE_DELETE_LIMIT dependency of `DELETE ... LIMIT`.
                let deleted = conn.execute(
                    "DELETE FROM body_events WHERE rowid IN (\
                       SELECT rowid FROM body_events \
                       WHERE observed_at_unix_ms >= ?1 AND observed_at_unix_ms < ?2 \
                         AND storage NOT IN ('spool', 'spool_segment') \
                       LIMIT ?3)",
                    params![day_start, day_end, batch as i64],
                )? as u64;
                total += deleted;
                if deleted < batch {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(total)
    }

    fn delete_candidate_blobs(
        &self,
        conn: &Connection,
        candidate_days: &HashSet<i64>,
        batch: u64,
    ) -> Result<u64> {
        let max_rowid: i64 = conn.query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM body_blobs",
            [],
            |row| row.get(0),
        )?;
        let mut total = 0u64;
        let mut lo: i64 = 0;
        while lo <= max_rowid {
            let hi = lo.saturating_add(batch as i64);
            let batch_rows: Vec<(i64, String, String, i64)> = {
                let mut stmt = conn.prepare(
                    "SELECT rowid, body_sha256, storage, created_at_unix_ms \
                     FROM body_blobs WHERE rowid >= ?1 AND rowid < ?2",
                )?;
                let mapped = stmt.query_map(params![lo, hi], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?;
                let mut rows = Vec::new();
                for row in mapped {
                    rows.push(row?);
                }
                rows
            };
            let mut deleted_this_batch = false;
            for (rowid, sha, storage, created_at) in batch_rows {
                // Never touch spool rows; only archive rows on candidate days.
                // Segment files are shared by many records, so GC removes only
                // orphaned index rows here and never unlinks a segment file.
                if storage != "archive" && storage != "archive_segment" {
                    continue;
                }
                if !candidate_days.contains(&day_floor_ms(created_at)) {
                    continue;
                }
                // Dedup safety: keep the blob if any surviving event references it.
                let still_referenced: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM body_events WHERE body_sha256 = ?1 LIMIT 1",
                        params![sha],
                        |row| row.get(0),
                    )
                    .optional()?;
                if still_referenced.is_none() {
                    conn.execute("DELETE FROM body_blobs WHERE rowid = ?1", params![rowid])?;
                    total += 1;
                    deleted_this_batch = true;
                }
            }
            lo = hi;
            if deleted_this_batch {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        Ok(total)
    }

    /// Move sealed spool segments into their original archive day partition,
    /// legacy spool blobs into today's partition, and legacy day-files into
    /// their own partition. The archive mount is checked by the caller.
    fn drain_spool(&self, conn: &mut Connection, now_ms: i64, report: &mut GcReport) -> Result<()> {
        // Seal the segment owned by this logger before looking for drainable
        // work. Segments owned by another process have no manifest yet and are
        // skipped until that writer seals or restarts.
        let active_spool = {
            let mut writer = self
                .segment_writer
                .lock()
                .map_err(|_| BodyLogError::new("capture segment writer lock poisoned"))?;
            if writer
                .active
                .as_ref()
                .is_some_and(|active| active.storage == "spool_segment")
            {
                writer.active.take()
            } else {
                None
            }
        };
        if let Some(active) = active_spool {
            seal_active_segment(active)?;
        }

        let mut segments = Vec::new();
        collect_segment_files(&self.spool_dir.join("segments"), &mut segments)?;
        segments.sort();
        for src in segments {
            let Some(manifest) = read_verified_segment_manifest(&src)? else {
                continue;
            };
            if !manifest.sealed {
                continue;
            }
            let Some(first_at) = manifest.first_observed_at_unix_ms else {
                continue;
            };
            let file_name = src
                .file_name()
                .ok_or_else(|| BodyLogError::new("spool segment has no file name"))?;
            let dest = self
                .day_dir(day_floor_ms(first_at))
                .join("segments")
                .join(file_name);
            let src_manifest = segment_manifest_path(&src);
            let dest_manifest = segment_manifest_path(&dest);

            copy_file_verified(&src, &dest, Some(&manifest.segment_sha256))?;
            copy_file_verified(&src_manifest, &dest_manifest, None)?;

            let src_str = src.to_string_lossy().into_owned();
            let dest_str = dest.to_string_lossy().into_owned();
            let transaction = conn.transaction()?;
            transaction.execute(
                "UPDATE body_blobs
                 SET storage = 'archive_segment', archive_path = ?1
                 WHERE storage = 'spool_segment' AND archive_path = ?2",
                params![dest_str, src_str],
            )?;
            transaction.execute(
                "UPDATE body_events
                 SET storage = 'archive_segment', archive_path = ?1
                 WHERE storage = 'spool_segment' AND archive_path = ?2",
                params![dest_str, src_str],
            )?;
            transaction.execute(
                "UPDATE body_segments
                 SET segment_path = ?1, storage = 'archive_segment',
                     segment_bytes = ?2, record_count = ?3, sealed = 1
                 WHERE segment_path = ?4",
                params![
                    dest_str,
                    manifest.segment_bytes as i64,
                    manifest.record_count as i64,
                    src_str
                ],
            )?;
            transaction.commit()?;

            fs::remove_file(&src)?;
            fs::remove_file(&src_manifest)?;
            report.spool_segments_drained += 1;
        }

        // Blob files: spool/blobs/sha256/<2>/<sha>.zst -> archive/<today>/blobs/...
        let blobs_root = self.spool_dir.join("blobs").join("sha256");
        if blobs_root.is_dir() {
            let today_dir = self.day_dir(day_floor_ms(now_ms));
            for prefix in read_dir_sorted(&blobs_root)? {
                if !prefix.is_dir() {
                    continue;
                }
                for src in read_dir_sorted(&prefix)? {
                    if src.extension().and_then(OsStr::to_str) != Some("zst") {
                        continue;
                    }
                    let Some(sha) = src.file_stem().and_then(OsStr::to_str) else {
                        continue;
                    };
                    let sha = sha.to_string();
                    let two = sha.get(..2).unwrap_or("xx");
                    let dest = today_dir
                        .join("blobs")
                        .join("sha256")
                        .join(two)
                        .join(format!("{sha}.zst"));
                    move_file(&src, &dest)?;
                    let dest_str = dest.to_string_lossy().into_owned();
                    conn.execute(
                        "UPDATE body_blobs SET storage = 'archive', archive_path = ?1 \
                         WHERE body_sha256 = ?2",
                        params![dest_str, sha],
                    )?;
                    conn.execute(
                        "UPDATE body_events SET storage = 'archive', archive_path = ?1 \
                         WHERE body_sha256 = ?2",
                        params![dest_str, sha],
                    )?;
                    report.spool_blobs_drained += 1;
                }
            }
        }

        // Spool day-files: spool/tap-bodies-YYYYMMDD.jsonl -> archive/<day>/tap-bodies.jsonl
        if self.spool_dir.is_dir() {
            for src in read_dir_sorted(&self.spool_dir)? {
                let Some(day_ms) = spool_day_file_day(&src) else {
                    continue;
                };
                let dest = self.day_dir(day_ms).join("tap-bodies.jsonl");
                append_merge_file(&src, &dest)?;
                fs::remove_file(&src)?;
                report.spool_day_files_drained += 1;
            }
        }
        Ok(())
    }

    /// Fill `report` with the would-drain counts without mutating (dry-run).
    fn count_spool_pending(&self, report: &mut GcReport) -> Result<()> {
        let mut segments = Vec::new();
        collect_segment_files(&self.spool_dir.join("segments"), &mut segments)?;
        report.spool_segments_drained = segments.len() as u64;

        let blobs_root = self.spool_dir.join("blobs").join("sha256");
        if blobs_root.is_dir() {
            for prefix in read_dir_sorted(&blobs_root)? {
                if !prefix.is_dir() {
                    continue;
                }
                for src in read_dir_sorted(&prefix)? {
                    if src.extension().and_then(OsStr::to_str) == Some("zst") {
                        report.spool_blobs_drained += 1;
                    }
                }
            }
        }
        if self.spool_dir.is_dir() {
            for src in read_dir_sorted(&self.spool_dir)? {
                if spool_day_file_day(&src).is_some() {
                    report.spool_day_files_drained += 1;
                }
            }
        }
        Ok(())
    }

    /// Compact the index with `VACUUM INTO` + atomic replace, guarded so it
    /// refuses unless (a) `confirm` is set and (b) no other process has the DB
    /// open (a stale writer holding the unlinked inode would silently lose data).
    /// Not run automatically; drives `sb body gc --compact`.
    pub fn compact(&self, confirm: bool) -> Result<CompactReport> {
        let index_path = self.index_path.clone();
        self.compact_with_holder_probe(confirm, move || default_db_holders(&index_path))
    }

    /// Compaction with an injectable holder probe (for deterministic tests of
    /// the guard). The probe returns the PIDs currently holding the DB open.
    pub fn compact_with_holder_probe(
        &self,
        confirm: bool,
        probe: impl Fn() -> Result<Vec<u32>>,
    ) -> Result<CompactReport> {
        let bytes_before = fs::metadata(&self.index_path).map(|m| m.len()).unwrap_or(0);
        let (events_before, blobs_before) = {
            let conn = open_index_connection(&self.index_path)?;
            (
                exact_rows(&conn, "body_events")?,
                exact_rows(&conn, "body_blobs")?,
            )
        };
        let mut report = CompactReport {
            refused: None,
            events_before,
            blobs_before,
            events_after: events_before,
            blobs_after: blobs_before,
            bytes_before,
            bytes_after: bytes_before,
        };

        if !confirm {
            report.refused = Some("compact requires --confirm".to_string());
            return Ok(report);
        }

        let others: Vec<u32> = match probe() {
            Ok(pids) => pids
                .into_iter()
                .filter(|&pid| pid != std::process::id())
                .collect(),
            Err(err) => {
                report.refused = Some(format!(
                    "cannot prove the index has no other holders; refusing compact ({err})"
                ));
                return Ok(report);
            }
        };
        if !others.is_empty() {
            report.refused = Some(format!(
                "refusing compact: index held open by pid(s) {others:?}"
            ));
            return Ok(report);
        }

        let dir = self
            .index_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let tmp = dir.join(format!("index.compact.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&tmp);
        {
            let conn = open_index_connection(&self.index_path)?;
            let target = tmp.to_string_lossy().replace('\'', "''");
            conn.execute_batch(&format!("VACUUM INTO '{target}'"))?;
        }
        fs::rename(&tmp, &self.index_path)?;
        set_private_file(&self.index_path)?;
        // The fresh file has no WAL; drop any stale sidecars from the old inode.
        let _ = fs::remove_file(wal_path(&self.index_path));
        let _ = fs::remove_file(shm_path(&self.index_path));

        report.bytes_after = fs::metadata(&self.index_path).map(|m| m.len()).unwrap_or(0);
        let conn = open_index_connection(&self.index_path)?;
        report.events_after = exact_rows(&conn, "body_events")?;
        report.blobs_after = exact_rows(&conn, "body_blobs")?;
        Ok(report)
    }

    fn init_db(&self) -> Result<()> {
        let conn = open_index_connection(&self.index_path)?;
        conn.execute_batch(INDEX_SCHEMA_SQL)?;
        ensure_sqlite_column(
            &conn,
            "body_segments",
            "segment_sha256",
            "ALTER TABLE body_segments ADD COLUMN segment_sha256 TEXT",
        )?;
        set_private_file(&self.index_path)?;
        for sidecar in [wal_path(&self.index_path), shm_path(&self.index_path)] {
            if sidecar.exists() {
                set_private_file(&sidecar)?;
            }
        }
        Ok(())
    }

    fn insert_record(&self, record: &BodyRecord) -> Result<()> {
        let conn = open_index_connection(&self.index_path)?;
        conn.execute(
            "INSERT OR IGNORE INTO body_blobs (
              body_sha256, body_bytes, compressed_bytes, storage, archive_path,
              protected, created_at_unix_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.body_sha256,
                record.body_bytes,
                record.compressed_bytes,
                record.storage,
                record.archive_path,
                record.protected as i64,
                record.observed_at_unix_ms,
            ],
        )?;
        conn.execute(
            "INSERT INTO body_events (
              event_id, request_id, observed_at_unix_ms, capture_stage, protocol,
              upstream, model, status, content_type, body_sha256, body_bytes,
              compressed_bytes, archive_path, storage, protected, redaction_state,
              threshold_shrunk, metadata_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                record.event_id,
                record.request_id,
                record.observed_at_unix_ms,
                record.capture_stage,
                record.protocol,
                record.upstream,
                record.model,
                record.status.map(i64::from),
                record.content_type,
                record.body_sha256,
                record.body_bytes,
                record.compressed_bytes,
                record.archive_path,
                record.storage,
                record.protected as i64,
                record.redaction_state,
                record.threshold_shrunk as i64,
                serde_json::to_string(&record.metadata)?,
            ],
        )?;
        Ok(())
    }

    fn append_segment_frame(&self, record: &mut BodyRecord, body: &[u8]) -> Result<()> {
        let archive_available = archive_root_available(&self.config.archive_root);
        let storage = if archive_available {
            "archive_segment"
        } else {
            "spool_segment"
        };
        let bucket_start_ms = record
            .observed_at_unix_ms
            .div_euclid(CAPTURE_SEGMENT_ROTATE_MS)
            * CAPTURE_SEGMENT_ROTATE_MS;
        let mut writer = self
            .segment_writer
            .lock()
            .map_err(|_| BodyLogError::new("capture segment writer lock poisoned"))?;

        let active_matches = writer.active.as_ref().is_some_and(|active| {
            active.storage == storage && active.bucket_start_ms == bucket_start_ms
        });
        if !active_matches {
            if let Some(active) = writer.active.take() {
                seal_active_segment(active)?;
            }
            writer.active = Some(self.create_segment(storage, bucket_start_ms)?);
        }

        let active = writer
            .active
            .as_ref()
            .ok_or_else(|| BodyLogError::new("capture segment was not created"))?;
        record.archive_path = active.path.to_string_lossy().into_owned();
        record.storage = active.storage.to_string();
        let mut frame = encode_segment_frame(record, body)?;
        let existing_bytes = fs::metadata(&active.path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        if segment_would_exceed_limit(
            existing_bytes,
            frame.len() as u64,
            CAPTURE_SEGMENT_MAX_BYTES,
        ) {
            let active = writer
                .active
                .take()
                .ok_or_else(|| BodyLogError::new("capture segment was not created"))?;
            seal_active_segment(active)?;
            let active = self.create_segment(storage, bucket_start_ms)?;
            record.archive_path = active.path.to_string_lossy().into_owned();
            record.storage = active.storage.to_string();
            frame = encode_segment_frame(record, body)?;
            writer.active = Some(active);
        }

        let active = writer
            .active
            .as_ref()
            .ok_or_else(|| BodyLogError::new("capture segment was not created"))?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        set_owner_only(&mut options);
        let mut file = options.open(&active.path)?;
        set_private_file(&active.path)?;
        file.write_all(&frame)?;
        file.flush()?;
        if let Some(active) = writer.active.as_mut() {
            active.last_write_at_unix_ms = record.observed_at_unix_ms;
        }
        Ok(())
    }

    fn create_segment(&self, storage: &'static str, bucket_start_ms: i64) -> Result<ActiveSegment> {
        let day_ms = day_floor_ms(bucket_start_ms);
        let root = if storage == "archive_segment" {
            self.day_dir(day_ms).join("segments")
        } else {
            let (year, month, day) = date_parts(day_ms);
            self.spool_dir
                .join("segments")
                .join(format!("{year:04}"))
                .join(format!("{month:02}"))
                .join(format!("{day:02}"))
        };
        let base = if storage == "archive_segment" {
            self.config.archive_root.as_path()
        } else {
            self.spool_dir.as_path()
        };
        ensure_private_directory_tree(base, &root)?;
        let sequence = NEXT_SEGMENT_ID.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            "capture-{bucket_start_ms}-p{}-{sequence}.sbcap",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        set_owner_only(&mut options);
        let mut file = options.open(&path)?;
        set_private_file(&path)?;
        file.write_all(CAPTURE_SEGMENT_MAGIC)?;
        file.flush()?;
        let lock_path = segment_lock_path(&path);
        let mut lock_options = OpenOptions::new();
        lock_options.create(true).read(true).write(true);
        set_owner_only(&mut lock_options);
        let lock_file = Arc::new(lock_options.open(&lock_path)?);
        set_private_file(&lock_path)?;
        if !try_lock_file(&lock_file)? {
            return Err(BodyLogError::new(format!(
                "cannot lock new capture segment {}",
                path.display()
            )));
        }
        Ok(ActiveSegment {
            path,
            storage,
            bucket_start_ms,
            last_write_at_unix_ms: bucket_start_ms,
            lock_file,
        })
    }

    fn recover_segments(&self, rebuild_index: bool) -> Result<()> {
        let mut irreconcilable_frames = 0u64;
        let mut segments = Vec::new();
        collect_segment_files(&self.config.archive_root, &mut segments)?;
        collect_segment_files(&self.spool_dir.join("segments"), &mut segments)?;
        // The archive tree that lives beside the index, under `body_dir`. When the
        // archive volume is detached the archive_root of the day points here, and
        // once the volume returns nothing scans it again — so a segment whose writer
        // died while the volume was away stays unsealed forever. Unsealed means the
        // backup adapter (sealed-manifests only) can never transfer it, which pins
        // `unbacked_bytes` above zero permanently. Live 2026-07-27: exactly two such
        // segments (823,801 bytes, dead pid 38869) were the reason capture could not
        // earn its way back to full-wire even after the recovery and pressure fixes.
        //
        // Skipped when it IS the configured archive_root, so the scan stays
        // idempotent instead of double-listing every segment.
        if let Some(body_dir) = self.spool_dir.parent() {
            let local_archive = body_dir.join("archive");
            if local_archive != self.config.archive_root {
                collect_segment_files(&local_archive, &mut segments)?;
            }
        }
        segments.sort();
        segments.dedup();
        for path in segments {
            let manifest = segment_manifest_path(&path);
            if !rebuild_index && manifest.exists() {
                let verified = read_verified_segment_manifest(&path)?
                    .ok_or_else(|| BodyLogError::new("capture segment manifest disappeared"))?;
                let conn = open_index_connection(&self.index_path)?;
                upsert_segment_manifest_projection_on(
                    &conn,
                    &path,
                    if path.starts_with(self.spool_dir.join("segments")) {
                        "spool_segment"
                    } else {
                        "archive_segment"
                    },
                    &verified,
                    None,
                )?;
                continue;
            }
            let recovery_lock = if manifest.exists() {
                None
            } else {
                let Some(lock) = try_acquire_segment_lock(&path)? else {
                    // Another live logger still owns this appendable segment.
                    continue;
                };
                Some(lock)
            };
            let frames = if manifest.exists() {
                read_verified_segment_manifest(&path)?
                    .ok_or_else(|| BodyLogError::new("capture segment manifest disappeared"))?;
                scan_segment(&path, false)?
            } else {
                scan_segment(&path, true)?
            };
            if rebuild_index || !manifest.exists() {
                let conn = open_index_connection(&self.index_path)?;
                for frame in &frames {
                    let mut record = frame.record.clone();
                    record.archive_path = path.to_string_lossy().into_owned();
                    record.storage = if path.starts_with(self.spool_dir.join("segments")) {
                        "spool_segment"
                    } else {
                        "archive_segment"
                    }
                    .to_string();
                    if rebuild_index {
                        insert_record_on(&conn, &record)?;
                    } else if let Err(err) = insert_recovered_record_on(&conn, &record) {
                        // One irreconcilable HISTORICAL frame must never cost us
                        // LIVE capture. Propagating here aborts BodyLogger
                        // construction, and `sb-server::tap` turns that into a
                        // dropped capture worker for EVERY tap — trading one
                        // contested row for total body-capture loss, which is
                        // exactly the 15h outage on 2026-07-27. The segment file
                        // itself stays intact evidence; only the index row is in
                        // question. Say it loudly, count it, keep recovering.
                        irreconcilable_frames += 1;
                        tracing::error!(
                            event_id = %record.event_id,
                            segment = %path.display(),
                            error = %err,
                            "capture recovery skipped an irreconcilable frame; live capture continues"
                        );
                    }
                }
            }
            if !manifest.exists() {
                write_segment_manifest(&path, &frames, true)?;
            }
            let verified = read_verified_segment_manifest(&path)?
                .ok_or_else(|| BodyLogError::new("capture segment manifest disappeared"))?;
            let conn = open_index_connection(&self.index_path)?;
            upsert_segment_manifest_projection_on(
                &conn,
                &path,
                if path.starts_with(self.spool_dir.join("segments")) {
                    "spool_segment"
                } else {
                    "archive_segment"
                },
                &verified,
                Some(
                    frames
                        .iter()
                        .map(|frame| frame.record.body_bytes)
                        .fold(0u64, u64::saturating_add),
                ),
            )?;
            if let Some(lock) = recovery_lock {
                unlock_file(&lock)?;
                drop(lock);
                let _ = fs::remove_file(segment_lock_path(&path));
            }
        }
        if irreconcilable_frames > 0 {
            tracing::error!(
                irreconcilable_frames,
                "capture recovery completed with skipped frames; run `sb body audit` on the named event ids"
            );
        }
        Ok(())
    }

    fn blob_location(&self, observed_at_unix_ms: i64, body_sha256: &str) -> BlobLocation {
        let prefix = body_sha256.get(..2).unwrap_or("xx");
        if archive_root_available(&self.config.archive_root) {
            let day_dir = self.day_dir(day_floor_ms(observed_at_unix_ms));
            let path = day_dir
                .join("blobs")
                .join("sha256")
                .join(prefix)
                .join(format!("{body_sha256}.zst"));
            BlobLocation {
                path,
                day_dir: Some(day_dir),
                storage: "archive",
                archive_available: true,
            }
        } else {
            let path = self
                .spool_dir
                .join("blobs")
                .join("sha256")
                .join(prefix)
                .join(format!("{body_sha256}.zst"));
            BlobLocation {
                path,
                day_dir: None,
                storage: "spool",
                archive_available: false,
            }
        }
    }

    /// The archive day partition dir (`archive_root/YYYY/MM/DD`) for a UTC ms.
    fn day_dir(&self, day_ms: i64) -> PathBuf {
        let (yyyy, mm, dd) = date_parts(day_ms);
        self.config
            .archive_root
            .join(format!("{yyyy:04}"))
            .join(format!("{mm:02}"))
            .join(format!("{dd:02}"))
    }

    /// Route a `tap-bodies.jsonl` record to the day partition (archive up) or a
    /// spool day-file (archive down). The configured legacy sink is FROZEN: it
    /// is never appended to here, so the historical flat file stops growing.
    fn route_tap_body_event(&self, record: &BodyRecord, location: &BlobLocation) -> Result<()> {
        let line = serde_json::to_string(record)?;
        if location.archive_available {
            if let Some(day_dir) = &location.day_dir {
                ensure_private_directory_tree(&self.config.archive_root, day_dir)?;
                append_line_0600(&day_dir.join("tap-bodies.jsonl"), &line)?;
            }
        } else {
            let (yyyy, mm, dd) = date_parts(day_floor_ms(record.observed_at_unix_ms));
            let name = format!("tap-bodies-{yyyy:04}{mm:02}{dd:02}.jsonl");
            append_line_0600(&self.spool_dir.join(name), &line)?;
        }
        Ok(())
    }

    fn append_archive_event(&self, day_dir: &Path, record: &BodyRecord) -> Result<()> {
        ensure_private_directory_tree(&self.config.archive_root, day_dir)?;
        let path = day_dir.join("body-events.jsonl.zst");
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let compressed = zstd::stream::encode_all(line.as_slice(), ZSTD_LEVEL)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        set_owner_only(&mut options);
        let mut file = options.open(&path)?;
        set_private_file(&path)?;
        file.write_all(&compressed)?;
        Ok(())
    }
}

/// Resolve the retention window: explicit value, else env, else default.
pub fn resolve_keep_days(explicit: Option<u64>) -> u64 {
    explicit.unwrap_or_else(env_keep_days)
}

fn env_keep_days() -> u64 {
    std::env::var(KEEP_DAYS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_KEEP_DAYS)
}

fn default_archive_root(state_dir: &Path) -> PathBuf {
    std::env::var_os("SWITCHBACK_BODY_ARCHIVE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.join("body").join("archive"))
}

fn existing_index_path(state_dir: &Path) -> PathBuf {
    let body_dir = state_dir.join("body");
    for candidate in [
        body_dir.join(CURRENT_INDEX_FILE),
        body_dir.join(LEGACY_SEGMENT_INDEX_FILE),
        state_dir.join(LEGACY_ROOT_INDEX_FILE),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    body_dir.join(CURRENT_INDEX_FILE)
}

fn archive_root_available(path: &Path) -> bool {
    if let Some(anchor) = volume_anchor(path) {
        return anchor.is_dir();
    }
    path.is_dir() || path.parent().is_some_and(Path::is_dir)
}

fn volume_anchor(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match (components.next(), components.next(), components.next()) {
        (
            Some(Component::RootDir),
            Some(Component::Normal(volumes)),
            Some(Component::Normal(name)),
        ) if volumes == "Volumes" => Some(PathBuf::from("/Volumes").join(name)),
        _ => None,
    }
}

/// `MAX(rowid)` — O(1), an over-approximation of row count after deletes.
fn append_only_rows(conn: &Connection, table: &str) -> Result<u64> {
    let sql = format!("SELECT COALESCE(MAX(rowid), 0) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get::<_, u64>(0))?)
}

/// Exact `COUNT(*)` — O(n); only used on small DBs / compaction verification.
fn exact_rows(conn: &Connection, table: &str) -> Result<u64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get::<_, u64>(0))?)
}

/// Filesystem-exact spool backlog: count of spool blob files plus non-empty
/// spool day-files. Cheap and independent of the sqlite size.
fn count_spool_backlog(spool_dir: &Path) -> std::io::Result<u64> {
    let mut count = count_files_with_extension(&spool_dir.join("segments"), "sbcap")?;
    let blobs_root = spool_dir.join("blobs").join("sha256");
    if blobs_root.is_dir() {
        for prefix in fs::read_dir(&blobs_root)? {
            let prefix = prefix?.path();
            if prefix.is_dir() {
                for file in fs::read_dir(&prefix)? {
                    let file = file?.path();
                    if file.extension().and_then(OsStr::to_str) == Some("zst") {
                        count += 1;
                    }
                }
            }
        }
    }
    if spool_dir.is_dir() {
        for entry in fs::read_dir(spool_dir)? {
            let path = entry?.path();
            if is_spool_day_file(&path) && fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false)
            {
                count += 1;
            }
        }
    }
    Ok(count)
}

fn count_files_with_extension(root: &Path, extension: &str) -> std::io::Result<u64> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0u64;
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            count += count_files_with_extension(&path, extension)?;
        } else if path.extension().and_then(OsStr::to_str) == Some(extension) {
            count += 1;
        }
    }
    Ok(count)
}

/// Count local archive day dirs (`YYYY/MM/DD`) and the oldest, best-effort.
fn count_local_day_dirs(archive_root: &Path) -> (u64, Option<String>) {
    let mut count = 0u64;
    let mut oldest: Option<(i32, u8, u8)> = None;
    let Ok(years) = fs::read_dir(archive_root) else {
        return (0, None);
    };
    for year in years.flatten() {
        let year_path = year.path();
        let Some(year_num) = numeric_dir_name::<i32>(&year_path) else {
            continue;
        };
        let Ok(months) = fs::read_dir(&year_path) else {
            continue;
        };
        for month in months.flatten() {
            let month_path = month.path();
            let Some(month_num) = numeric_dir_name::<u8>(&month_path) else {
                continue;
            };
            let Ok(days) = fs::read_dir(&month_path) else {
                continue;
            };
            for day in days.flatten() {
                let day_path = day.path();
                let Some(day_num) = numeric_dir_name::<u8>(&day_path) else {
                    continue;
                };
                count += 1;
                let key = (year_num, month_num, day_num);
                if oldest.map(|current| key < current).unwrap_or(true) {
                    oldest = Some(key);
                }
            }
        }
    }
    (
        count,
        oldest.map(|(y, m, d)| format!("{y:04}-{m:02}-{d:02}")),
    )
}

fn numeric_dir_name<T: std::str::FromStr>(path: &Path) -> Option<T> {
    if !path.is_dir() {
        return None;
    }
    path.file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.parse::<T>().ok())
}

fn is_spool_day_file(path: &Path) -> bool {
    spool_day_file_day(path).is_some()
}

/// If `path` is a `tap-bodies-YYYYMMDD.jsonl` spool day-file, its day start (ms).
fn spool_day_file_day(path: &Path) -> Option<i64> {
    let name = path.file_name().and_then(OsStr::to_str)?;
    let digits = name
        .strip_prefix("tap-bodies-")
        .and_then(|rest| rest.strip_suffix(".jsonl"))?;
    if digits.len() != 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i32 = digits[0..4].parse().ok()?;
    let month: u8 = digits[4..6].parse().ok()?;
    let day: u8 = digits[6..8].parse().ok()?;
    day_start_ms(year, month, day)
}

fn read_dir_sorted(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    paths.sort();
    Ok(paths)
}

/// Move `src` to `dest`, cross-device safe. If `dest` already exists (dedup),
/// drop `src`. New files are created 0600.
fn move_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        ensure_private_directory(parent)?;
    }
    if dest.exists() {
        set_private_file(dest)?;
        fs::remove_file(src)?;
        return Ok(());
    }
    if fs::rename(src, dest).is_ok() {
        set_private_file(dest)?;
        return Ok(());
    }
    // Cross-device: copy + fsync + unlink.
    let data = fs::read(src)?;
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    set_owner_only(&mut opts);
    let mut file = opts.open(dest)?;
    set_private_file(dest)?;
    file.write_all(&data)?;
    file.sync_all()?;
    fs::remove_file(src)?;
    Ok(())
}

/// Append `src`'s bytes to `dest` (create 0600), never clobbering existing content.
fn append_merge_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        ensure_private_directory(parent)?;
    }
    let data = fs::read(src)?;
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    set_owner_only(&mut opts);
    let mut file = opts.open(dest)?;
    set_private_file(dest)?;
    file.write_all(&data)?;
    Ok(())
}

fn encode_segment_frame(record: &mut BodyRecord, body: &[u8]) -> Result<Vec<u8>> {
    let mut compressed = Vec::new();
    let mut payload_len = 0u64;
    for _ in 0..4 {
        let metadata = serde_json::to_vec(record)?;
        let metadata_len = u32::try_from(metadata.len())
            .map_err(|_| BodyLogError::new("capture segment metadata exceeds u32"))?;
        let mut payload = Vec::with_capacity(4 + metadata.len() + body.len());
        payload.extend_from_slice(&metadata_len.to_be_bytes());
        payload.extend_from_slice(&metadata);
        payload.extend_from_slice(body);
        payload_len = payload.len() as u64;
        compressed = zstd::stream::encode_all(payload.as_slice(), ZSTD_LEVEL)?;
        let compressed_len = compressed.len() as u64;
        if record.compressed_bytes == compressed_len {
            break;
        }
        record.compressed_bytes = compressed_len;
    }
    let compressed_len = compressed.len() as u64;
    if compressed_len > CAPTURE_RECORD_MAX_BYTES || payload_len > CAPTURE_RECORD_MAX_BYTES {
        return Err(BodyLogError::new(
            "capture segment record exceeds safety limit",
        ));
    }
    let checksum = crc32fast::hash(&compressed);
    let mut frame = Vec::with_capacity(CAPTURE_RECORD_HEADER_BYTES as usize + compressed.len());
    frame.extend_from_slice(CAPTURE_RECORD_MAGIC);
    frame.extend_from_slice(&compressed_len.to_be_bytes());
    frame.extend_from_slice(&payload_len.to_be_bytes());
    frame.extend_from_slice(&checksum.to_be_bytes());
    frame.extend_from_slice(&compressed);
    Ok(frame)
}

fn segment_would_exceed_limit(existing_bytes: u64, frame_bytes: u64, limit_bytes: u64) -> bool {
    existing_bytes > CAPTURE_SEGMENT_MAGIC.len() as u64
        && existing_bytes.saturating_add(frame_bytes) > limit_bytes
}

fn scan_segment(path: &Path, recover_tail: bool) -> Result<Vec<SegmentFrame>> {
    let mut options = OpenOptions::new();
    options.read(true).write(recover_tail);
    let mut file = options.open(path)?;
    set_private_file(path)?;
    let file_len = file.metadata()?.len();
    if file_len < CAPTURE_SEGMENT_MAGIC.len() as u64 {
        return Err(BodyLogError::new(format!(
            "capture segment header incomplete: {}",
            path.display()
        )));
    }
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != CAPTURE_SEGMENT_MAGIC {
        return Err(BodyLogError::new(format!(
            "capture segment magic mismatch: {}",
            path.display()
        )));
    }

    let mut frames = Vec::new();
    loop {
        let frame_start = file.stream_position()?;
        if frame_start == file_len {
            break;
        }
        let remaining = file_len.saturating_sub(frame_start);
        if remaining < CAPTURE_RECORD_HEADER_BYTES {
            if recover_tail {
                truncate_partial_segment_tail(&file, frame_start)?;
                break;
            }
            return Err(BodyLogError::new(
                "capture segment has incomplete record header",
            ));
        }
        let mut record_magic = [0u8; 4];
        file.read_exact(&mut record_magic)?;
        if &record_magic != CAPTURE_RECORD_MAGIC {
            return Err(BodyLogError::new("capture segment record magic mismatch"));
        }
        let compressed_len = read_u64_be(&mut file)?;
        let payload_len = read_u64_be(&mut file)?;
        let checksum = read_u32_be(&mut file)?;
        if compressed_len > CAPTURE_RECORD_MAX_BYTES || payload_len > CAPTURE_RECORD_MAX_BYTES {
            return Err(BodyLogError::new(
                "capture segment record exceeds safety limit",
            ));
        }
        if file_len.saturating_sub(file.stream_position()?) < compressed_len {
            if recover_tail {
                truncate_partial_segment_tail(&file, frame_start)?;
                break;
            }
            return Err(BodyLogError::new(
                "capture segment record body is incomplete",
            ));
        }
        let mut compressed = vec![0u8; compressed_len as usize];
        file.read_exact(&mut compressed)?;
        if crc32fast::hash(&compressed) != checksum {
            return Err(BodyLogError::new(
                "capture segment record checksum mismatch",
            ));
        }
        let payload = zstd::stream::decode_all(compressed.as_slice())?;
        if payload.len() as u64 != payload_len || payload.len() < 4 {
            return Err(BodyLogError::new("capture segment payload length mismatch"));
        }
        let mut metadata_len_bytes = [0u8; 4];
        metadata_len_bytes.copy_from_slice(&payload[..4]);
        let metadata_len = u32::from_be_bytes(metadata_len_bytes) as usize;
        let metadata_end = 4usize.saturating_add(metadata_len);
        if metadata_end > payload.len() {
            return Err(BodyLogError::new("capture segment metadata is incomplete"));
        }
        let record: BodyRecord = serde_json::from_slice(&payload[4..metadata_end])?;
        let body = payload[metadata_end..].to_vec();
        if record.body_bytes != body.len() as u64 || record.body_sha256 != sha256_hex(&body) {
            return Err(BodyLogError::new("capture segment body integrity mismatch"));
        }
        frames.push(SegmentFrame { record, body });
    }
    Ok(frames)
}

fn truncate_partial_segment_tail(file: &fs::File, frame_start: u64) -> Result<()> {
    file.set_len(frame_start)?;
    file.sync_all()?;
    Ok(())
}

fn read_body_from_segment(path: &Path, body_sha256: &str) -> Result<Vec<u8>> {
    for frame in scan_segment(path, false)? {
        if frame.record.body_sha256 == body_sha256 {
            return Ok(frame.body);
        }
    }
    Err(BodyLogError::new("body blob not found in capture segment"))
}

fn read_u64_be(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_u32_be(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

/// Segments only ever live in a `segments/` directory — the spool's own, and one
/// per archive day (`<archive>/<YYYY>/<MM>/<DD>/segments`). A day's sibling
/// `blobs/sha256/**` tree holds content-addressed body payloads written under
/// their hash with no extension, so it can never hold a `.sbcap` segment.
/// Descending it costs one directory read per stored body — millions, on an
/// archive volume — to find nothing, and recovery runs on the startup path
/// before the tap listeners are serving. Prune it by name so the cost of
/// recovery stays proportional to the number of segments, not bodies.
fn collect_segment_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().and_then(OsStr::to_str) == Some(BLOB_DIR_NAME) {
                continue;
            }
            collect_segment_files(&path, out)?;
        } else if path.extension().and_then(OsStr::to_str) == Some("sbcap") {
            out.push(path);
        }
    }
    Ok(())
}

fn segment_manifest_path(segment: &Path) -> PathBuf {
    let mut path = segment.as_os_str().to_os_string();
    path.push(".manifest.json");
    PathBuf::from(path)
}

fn segment_lock_path(segment: &Path) -> PathBuf {
    let mut path = segment.as_os_str().to_os_string();
    path.push(".active.lock");
    PathBuf::from(path)
}

fn try_lock_file(file: &fs::File) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: `file` owns a valid descriptor for the duration of this call;
        // `flock` neither retains the Rust reference nor accesses memory.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(false);
        }
        Err(error.into())
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(true)
    }
}

fn unlock_file(file: &fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        // SAFETY: `file` owns a valid descriptor for the duration of this call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

fn try_acquire_segment_lock(segment: &Path) -> Result<Option<fs::File>> {
    let lock_path = segment_lock_path(segment);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    set_owner_only(&mut options);
    let file = options.open(lock_path)?;
    set_private_file(&segment_lock_path(segment))?;
    if try_lock_file(&file)? {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn seal_segment(path: &Path) -> Result<()> {
    let segment = OpenOptions::new().read(true).write(true).open(path)?;
    segment.sync_all()?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    let frames = scan_segment(path, true)?;
    write_segment_manifest(path, &frames, true)
}

fn seal_active_segment(active: ActiveSegment) -> Result<()> {
    let lock_path = segment_lock_path(&active.path);
    seal_segment(&active.path)?;
    unlock_file(&active.lock_file)?;
    drop(active);
    let _ = fs::remove_file(lock_path);
    Ok(())
}

fn write_segment_manifest(path: &Path, frames: &[SegmentFrame], sealed: bool) -> Result<()> {
    let bytes = fs::read(path)?;
    let manifest = SegmentManifest {
        schema_version: CAPTURE_SEGMENT_SCHEMA.to_string(),
        segment_file: path
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or_default()
            .to_string(),
        segment_sha256: sha256_hex(&bytes),
        segment_bytes: bytes.len() as u64,
        record_count: frames.len() as u64,
        body_bytes: frames
            .iter()
            .map(|frame| frame.record.body_bytes)
            .fold(0u64, u64::saturating_add),
        first_observed_at_unix_ms: frames.first().map(|frame| frame.record.observed_at_unix_ms),
        last_observed_at_unix_ms: frames.last().map(|frame| frame.record.observed_at_unix_ms),
        sealed,
    };
    let manifest_path = segment_manifest_path(path);
    let tmp = manifest_path.with_extension(format!(
        "tmp-{}",
        NEXT_SEGMENT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    set_owner_only(&mut options);
    let mut file = options.open(&tmp)?;
    set_private_file(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(tmp, manifest_path)?;
    set_private_file(&segment_manifest_path(path))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn read_verified_segment_manifest(path: &Path) -> Result<Option<SegmentManifest>> {
    let manifest_path = segment_manifest_path(path);
    if !manifest_path.is_file() {
        return Ok(None);
    }
    let manifest: SegmentManifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    if manifest.schema_version != CAPTURE_SEGMENT_SCHEMA {
        return Err(BodyLogError::new(format!(
            "unsupported capture segment manifest schema at {}",
            manifest_path.display()
        )));
    }
    let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    if manifest.segment_file != file_name {
        return Err(BodyLogError::new(format!(
            "capture segment manifest file mismatch at {}",
            manifest_path.display()
        )));
    }
    let bytes = fs::read(path)?;
    if manifest.segment_bytes != bytes.len() as u64 || manifest.segment_sha256 != sha256_hex(&bytes)
    {
        return Err(BodyLogError::new(format!(
            "capture segment checksum proof failed at {}",
            path.display()
        )));
    }
    Ok(Some(manifest))
}

/// Copy without deleting the source. This ordering lets the caller publish the
/// new index location before unlinking the spool copy, so a crash always leaves
/// at least one path referenced by SQLite. Existing destinations are accepted
/// only when their bytes match the source checksum.
fn copy_file_verified(src: &Path, dest: &Path, expected_sha256: Option<&str>) -> Result<()> {
    let bytes = fs::read(src)?;
    let source_sha256 = sha256_hex(&bytes);
    if expected_sha256.is_some_and(|expected| expected != source_sha256) {
        return Err(BodyLogError::new(format!(
            "source checksum proof failed before copy: {}",
            src.display()
        )));
    }
    if dest.exists() {
        let existing = fs::read(dest)?;
        if sha256_hex(&existing) != source_sha256 {
            return Err(BodyLogError::new(format!(
                "refusing to replace mismatched capture artifact at {}",
                dest.display()
            )));
        }
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        ensure_private_directory(parent)?;
    }
    let tmp = dest.with_extension(format!(
        "copy-{}-{}.tmp",
        std::process::id(),
        NEXT_SEGMENT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    set_owner_only(&mut options);
    let mut file = options.open(&tmp)?;
    set_private_file(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    if dest.exists() {
        let existing = fs::read(dest)?;
        if sha256_hex(&existing) != source_sha256 {
            let _ = fs::remove_file(&tmp);
            return Err(BodyLogError::new(format!(
                "refusing to replace raced capture artifact at {}",
                dest.display()
            )));
        }
        fs::remove_file(tmp)?;
    } else {
        fs::rename(tmp, dest)?;
        set_private_file(dest)?;
    }
    Ok(())
}

fn append_line_0600(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_directory(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    set_owner_only(&mut opts);
    let mut file = opts.open(path)?;
    set_private_file(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_private_directory_tree(base: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(base).map_err(|_| {
        BodyLogError::new(format!(
            "private capture path {} is outside base {}",
            path.display(),
            base.display()
        ))
    })?;
    ensure_private_directory(base)?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        current.push(component);
        ensure_private_directory(&current)?;
    }
    Ok(())
}

fn set_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn set_owner_only(opts: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    {
        let _ = opts;
    }
}

fn wal_path(index_path: &Path) -> PathBuf {
    sidecar_path(index_path, "-wal")
}

fn shm_path(index_path: &Path) -> PathBuf {
    sidecar_path(index_path, "-shm")
}

fn sidecar_path(index_path: &Path, suffix: &str) -> PathBuf {
    let mut os = index_path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Best-effort holder detection via `lsof`. Any error (missing tool, etc.) is
/// surfaced so compaction fails closed rather than assuming "no holders".
fn default_db_holders(index_path: &Path) -> Result<Vec<u32>> {
    let mut pids: HashSet<u32> = HashSet::new();
    for path in [
        index_path.to_path_buf(),
        wal_path(index_path),
        shm_path(index_path),
    ] {
        if !path.exists() {
            continue;
        }
        let output = std::process::Command::new("lsof")
            .arg("-t")
            .arg("--")
            .arg(&path)
            .output()
            .map_err(|err| {
                BodyLogError::new(format!("lsof failed for {}: {err}", path.display()))
            })?;
        // lsof exits 1 when nothing holds the file — that is a clean "no holders",
        // not an error. Only a spawn failure (above) is fail-closed.
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(pid) = line.trim().parse::<u32>() {
                pids.insert(pid);
            }
        }
    }
    Ok(pids.into_iter().collect())
}

fn insert_record_on(conn: &Connection, record: &BodyRecord) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO body_blobs (
            body_sha256, body_bytes, compressed_bytes, storage, archive_path,
            protected, created_at_unix_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            record.body_sha256,
            record.body_bytes,
            record.compressed_bytes,
            record.storage,
            record.archive_path,
            record.protected as i64,
            record.observed_at_unix_ms,
        ],
    )?;
    conn.execute(
        "INSERT INTO body_events (
            event_id, request_id, observed_at_unix_ms, capture_stage, protocol,
            upstream, model, status, content_type, body_sha256, body_bytes,
            compressed_bytes, archive_path, storage, protected, redaction_state,
            threshold_shrunk, metadata_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            record.event_id,
            record.request_id,
            record.observed_at_unix_ms,
            record.capture_stage,
            record.protocol,
            record.upstream,
            record.model,
            record.status.map(i64::from),
            record.content_type,
            record.body_sha256,
            record.body_bytes,
            record.compressed_bytes,
            record.archive_path,
            record.storage,
            record.protected as i64,
            record.redaction_state,
            record.threshold_shrunk as i64,
            serde_json::to_string(&record.metadata)?,
        ],
    )?;
    Ok(())
}

/// The fields that make a captured body *what it is*. Two records agreeing on all
/// of these describe the same wire event, however much has been rewritten around
/// them since.
///
/// Everything outside this set is a PROJECTION — where the bytes currently live
/// and what we have since learned about them. Sealing, archiving, backing up, and
/// reclaiming all rewrite those legitimately, so a replay disagreeing there is
/// routine, not corruption. Only a different `body_sha256`/`body_bytes` (different
/// bytes) or a different request/stage/protocol (different event) is a real
/// collision.
fn recovery_identity_divergence(existing: &BodyRecord, replay: &BodyRecord) -> Vec<&'static str> {
    let mut diverged = Vec::new();
    if existing.request_id != replay.request_id {
        diverged.push("request_id");
    }
    if existing.observed_at_unix_ms != replay.observed_at_unix_ms {
        diverged.push("observed_at_unix_ms");
    }
    if existing.capture_stage != replay.capture_stage {
        diverged.push("capture_stage");
    }
    if existing.protocol != replay.protocol {
        diverged.push("protocol");
    }
    if existing.body_sha256 != replay.body_sha256 {
        diverged.push("body_sha256");
    }
    if existing.body_bytes != replay.body_bytes {
        diverged.push("body_bytes");
    }
    diverged
}

/// Point an already-indexed event at where recovery just found its bytes.
///
/// Deliberately narrow: recovery is authoritative about the segment file it is
/// scanning right now (`archive_path`, `storage` — set from the live path by the
/// caller), and about nothing else. The indexed row may carry enrichment the
/// on-disk frame predates, so overwriting the rest of the record with the frame's
/// original view would regress it.
fn reconcile_recovered_location_on(conn: &Connection, record: &BodyRecord) -> Result<()> {
    conn.execute(
        "UPDATE body_events SET archive_path = ?2, storage = ?3 WHERE event_id = ?1",
        params![record.event_id, record.archive_path, record.storage],
    )?;
    Ok(())
}

/// Replay one segment frame into the index, tolerating the events already there.
///
/// An already-durable event must be a no-op here. This runs on the STARTUP path of
/// every `BodyLogger`, and an error disables that tap's capture worker outright
/// (`sb-server::tap` logs `tap body logger disabled` and drops the worker). Live
/// 2026-07-27: one frame whose `archive_path`/`storage` had been rewritten by
/// archiving failed the old byte-equality check and took body capture down on all
/// ten taps for ~15h — no bodies AND no gap records, so the loss left no trace
/// while `sb pulse` still reported every tap "listening".
///
/// Same identity → reconcile the location and continue. Different identity → say
/// WHICH fields diverged; the old message named none, which is what turned a
/// one-line mismatch into a live archaeology session.
fn insert_recovered_record_on(conn: &Connection, record: &BodyRecord) -> Result<()> {
    let existing = conn
        .query_row(
            "SELECT
               event_id, request_id, observed_at_unix_ms, capture_stage, protocol,
               upstream, model, status, content_type, body_sha256, body_bytes,
               compressed_bytes, archive_path, storage, protected, redaction_state,
               threshold_shrunk, metadata_json
             FROM body_events
             WHERE event_id = ?1",
            params![record.event_id],
            body_record_from_row,
        )
        .optional()?;
    let Some(existing) = existing else {
        return insert_record_on(conn, record);
    };
    let diverged = recovery_identity_divergence(&existing, record);
    if !diverged.is_empty() {
        return Err(BodyLogError::new(format!(
            "capture recovery event id collision for {}: identity fields differ ({})",
            record.event_id,
            diverged.join(", ")
        )));
    }
    if existing == *record {
        return Ok(());
    }
    reconcile_recovered_location_on(conn, record)
}

fn upsert_segment_projection_on(conn: &Connection, record: &BodyRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO body_segments (
            segment_path, storage, segment_sha256, segment_bytes, record_count, body_bytes,
            first_observed_at_unix_ms, last_observed_at_unix_ms, sealed
         ) VALUES (?1, ?2, NULL, ?3, 1, ?4, ?5, ?5, 0)
         ON CONFLICT(segment_path) DO UPDATE SET
            storage = excluded.storage,
            segment_bytes = body_segments.segment_bytes + excluded.segment_bytes,
            record_count = body_segments.record_count + 1,
            body_bytes = body_segments.body_bytes + excluded.body_bytes,
            first_observed_at_unix_ms =
                MIN(body_segments.first_observed_at_unix_ms, excluded.first_observed_at_unix_ms),
            last_observed_at_unix_ms =
                MAX(body_segments.last_observed_at_unix_ms, excluded.last_observed_at_unix_ms)",
        params![
            record.archive_path,
            record.storage,
            record.compressed_bytes,
            record.body_bytes,
            record.observed_at_unix_ms,
        ],
    )?;
    Ok(())
}

fn upsert_segment_manifest_projection_on(
    conn: &Connection,
    path: &Path,
    storage: &str,
    manifest: &SegmentManifest,
    recovered_body_bytes: Option<u64>,
) -> Result<()> {
    let Some(first_observed_at_unix_ms) = manifest.first_observed_at_unix_ms else {
        return Ok(());
    };
    let Some(last_observed_at_unix_ms) = manifest.last_observed_at_unix_ms else {
        return Ok(());
    };
    let body_bytes = recovered_body_bytes.unwrap_or(manifest.body_bytes);
    conn.execute(
        "INSERT INTO body_segments (
            segment_path, storage, segment_sha256, segment_bytes, record_count, body_bytes,
            first_observed_at_unix_ms, last_observed_at_unix_ms, sealed
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(segment_path) DO UPDATE SET
            storage = excluded.storage,
            segment_sha256 = excluded.segment_sha256,
            segment_bytes = excluded.segment_bytes,
            record_count = excluded.record_count,
            body_bytes = CASE
                WHEN excluded.body_bytes > 0 THEN excluded.body_bytes
                ELSE body_segments.body_bytes
            END,
            first_observed_at_unix_ms = excluded.first_observed_at_unix_ms,
            last_observed_at_unix_ms = excluded.last_observed_at_unix_ms,
            sealed = excluded.sealed",
        params![
            path.to_string_lossy(),
            storage,
            manifest.segment_sha256,
            manifest.segment_bytes,
            manifest.record_count,
            body_bytes,
            first_observed_at_unix_ms,
            last_observed_at_unix_ms,
            manifest.sealed as i64,
        ],
    )?;
    Ok(())
}

fn insert_event_only_on(conn: &Connection, record: &BodyRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO body_events (
            event_id, request_id, observed_at_unix_ms, capture_stage, protocol,
            upstream, model, status, content_type, body_sha256, body_bytes,
            compressed_bytes, archive_path, storage, protected,
            redaction_state, threshold_shrunk, metadata_json
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, ?16, ?17, ?18
         )",
        params![
            record.event_id,
            record.request_id,
            record.observed_at_unix_ms,
            record.capture_stage,
            record.protocol,
            record.upstream,
            record.model,
            record.status.map(i64::from),
            record.content_type,
            record.body_sha256,
            record.body_bytes,
            record.compressed_bytes,
            record.archive_path,
            record.storage,
            record.protected as i64,
            record.redaction_state,
            record.threshold_shrunk as i64,
            serde_json::to_string(&record.metadata)?,
        ],
    )?;
    Ok(())
}

fn metadata_only_event_count(conn: &Connection) -> Result<u64> {
    let count = conn.query_row(
        "SELECT COUNT(*) FROM body_events WHERE storage = 'metadata_only'",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count.max(0) as u64)
}

fn ensure_sqlite_column(
    conn: &Connection,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> Result<()> {
    let mut statement = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(());
        }
    }
    conn.execute_batch(alter_sql)?;
    Ok(())
}

fn query_records<P>(conn: &Connection, where_clause: &str, params: P) -> Result<Vec<BodyRecord>>
where
    P: rusqlite::Params,
{
    let sql = format!(
        "SELECT
           event_id, request_id, observed_at_unix_ms, capture_stage, protocol,
           upstream, model, status, content_type, body_sha256, body_bytes,
           compressed_bytes, archive_path, storage, protected, redaction_state,
           threshold_shrunk, metadata_json
         FROM body_events
         {where_clause}
         ORDER BY observed_at_unix_ms DESC, rowid DESC
         LIMIT ?"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params, body_record_from_row)?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row?);
    }
    Ok(records)
}

fn body_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BodyRecord> {
    let metadata_json: String = row.get(17)?;
    let metadata = serde_json::from_str(&metadata_json).unwrap_or(serde_json::Value::Null);
    Ok(BodyRecord {
        event_id: row.get(0)?,
        request_id: row.get(1)?,
        observed_at_unix_ms: row.get(2)?,
        capture_stage: row.get(3)?,
        protocol: row.get(4)?,
        upstream: row.get(5)?,
        model: row.get(6)?,
        status: row.get::<_, Option<i64>>(7)?.map(|status| status as u16),
        content_type: row.get(8)?,
        body_sha256: row.get(9)?,
        body_bytes: row.get(10)?,
        compressed_bytes: row.get(11)?,
        archive_path: row.get(12)?,
        storage: row.get(13)?,
        protected: row.get::<_, i64>(14)? != 0,
        redaction_state: row.get(15)?,
        threshold_shrunk: row.get::<_, i64>(16)? != 0,
        metadata,
    })
}

fn body_status_text(
    archive_available: bool,
    spool_backlog: u64,
    spool_backlog_exact: bool,
) -> &'static str {
    if spool_backlog_exact {
        if spool_backlog > 0 {
            "spooling"
        } else {
            "ok"
        }
    } else if archive_available {
        "ok_spool_unverified"
    } else {
        "spooling_unverified"
    }
}

/// The index schema, shared by `init_db` and the tests so a table can never be
/// exercised in one and absent in the other.
const INDEX_SCHEMA_SQL: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    CREATE TABLE IF NOT EXISTS body_blobs (
      body_sha256 TEXT PRIMARY KEY,
      body_bytes INTEGER NOT NULL,
      compressed_bytes INTEGER NOT NULL,
      storage TEXT NOT NULL,
      archive_path TEXT NOT NULL,
      protected INTEGER NOT NULL,
      created_at_unix_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS body_events (
      event_id TEXT PRIMARY KEY,
      request_id TEXT NOT NULL,
      observed_at_unix_ms INTEGER NOT NULL,
      capture_stage TEXT NOT NULL,
      protocol TEXT NOT NULL,
      upstream TEXT,
      model TEXT,
      status INTEGER,
      content_type TEXT,
      body_sha256 TEXT NOT NULL,
      body_bytes INTEGER NOT NULL,
      compressed_bytes INTEGER NOT NULL,
      archive_path TEXT NOT NULL,
      storage TEXT NOT NULL,
      protected INTEGER NOT NULL,
      redaction_state TEXT NOT NULL,
      threshold_shrunk INTEGER NOT NULL,
      metadata_json TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS body_segments (
        segment_path TEXT PRIMARY KEY,
        storage TEXT NOT NULL,
        segment_sha256 TEXT,
        segment_bytes INTEGER NOT NULL,
        record_count INTEGER NOT NULL,
        body_bytes INTEGER NOT NULL,
        first_observed_at_unix_ms INTEGER NOT NULL,
        last_observed_at_unix_ms INTEGER NOT NULL,
        sealed INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS body_backup_projection (
        segment_sha256 TEXT PRIMARY KEY,
        receipt_generation INTEGER NOT NULL,
        accepted_at_unix_ms INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_body_events_request_id
      ON body_events(request_id);
    CREATE INDEX IF NOT EXISTS idx_body_events_observed_at
      ON body_events(observed_at_unix_ms);
    CREATE INDEX IF NOT EXISTS idx_body_events_hash
      ON body_events(body_sha256);
    CREATE INDEX IF NOT EXISTS idx_body_events_archive_path
      ON body_events(archive_path);
    ";

fn open_index_connection(path: &Path) -> Result<Connection> {
    open_index_connection_with_busy_timeout(path, SQLITE_BUSY_TIMEOUT_MS)
}

/// Index connection for background maintenance (reclaim, receipt projection).
/// Same index, longer leash than the capture hot path: maintenance must wait
/// out the live writer rather than abort a whole batch on the first contention.
fn open_index_connection_for_maintenance(path: &Path) -> Result<Connection> {
    open_index_connection_with_busy_timeout(path, SQLITE_MAINTENANCE_BUSY_TIMEOUT_MS)
}

fn open_index_connection_with_busy_timeout(path: &Path, busy_timeout_ms: u64) -> Result<Connection> {
    let conn = Connection::open(path)?;
    set_private_file(path)?;
    for sidecar in [wal_path(path), shm_path(path)] {
        if sidecar.exists() {
            set_private_file(&sidecar)?;
        }
    }
    conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
    Ok(conn)
}

/// Begin a write transaction that takes the write lock at BEGIN.
///
/// rusqlite's `Connection::transaction()` is DEFERRED: it takes a read lock at
/// BEGIN and upgrades on the first write. With live capture holding the index,
/// that upgrade fails SQLITE_BUSY — measured on a busy host, where `sb body
/// reclaim` aborted mid-batch with "database is locked" after reclaiming only
/// part of the candidates. IMMEDIATE takes the write lock at BEGIN, where the
/// connection's `busy_timeout` applies, so a maintenance transaction waits out
/// the writer instead of dying.
pub(crate) fn begin_write_transaction(conn: &mut Connection) -> Result<Transaction<'_>> {
    Ok(conn.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_event_id(now_ms: i64) -> String {
    let seq = NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    format!("body_{now_ms}_p{}_{seq}", std::process::id())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// UTC day floor (00:00:00) of a unix ms. Unix time has no leap seconds, so a
/// UTC day is exactly `DAY_MS` and day starts align to multiples of `DAY_MS`.
fn day_floor_ms(unix_ms: i64) -> i64 {
    unix_ms.div_euclid(DAY_MS) * DAY_MS
}

/// Retention cutoff (unix ms): day starts strictly below this are candidates.
fn retention_cutoff_ms(now_ms: i64, keep_days: u64) -> i64 {
    day_floor_ms(now_ms) - (keep_days as i64) * DAY_MS
}

fn day_start_ms(year: i32, month: u8, day: u8) -> Option<i64> {
    let month = month_from_number(month)?;
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    Some(date.midnight().assume_utc().unix_timestamp() * 1000)
}

fn format_day_ms(unix_ms: i64) -> String {
    let (year, month, day) = date_parts(unix_ms);
    format!("{year:04}-{month:02}-{day:02}")
}

fn date_parts(unix_ms: i64) -> (i32, u8, u8) {
    let seconds = unix_ms.div_euclid(1000);
    let dt = OffsetDateTime::from_unix_timestamp(seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    (dt.year(), month_number(dt.month()), dt.day())
}

fn month_number(month: Month) -> u8 {
    match month {
        Month::January => 1,
        Month::February => 2,
        Month::March => 3,
        Month::April => 4,
        Month::May => 5,
        Month::June => 6,
        Month::July => 7,
        Month::August => 8,
        Month::September => 9,
        Month::October => 10,
        Month::November => 11,
        Month::December => 12,
    }
}

fn month_from_number(month: u8) -> Option<Month> {
    match month {
        1 => Some(Month::January),
        2 => Some(Month::February),
        3 => Some(Month::March),
        4 => Some(Month::April),
        5 => Some(Month::May),
        6 => Some(Month::June),
        7 => Some(Month::July),
        8 => Some(Month::August),
        9 => Some(Month::September),
        10 => Some(Month::October),
        11 => Some(Month::November),
        12 => Some(Month::December),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_size_rotation_keeps_one_oversized_record_but_rotates_before_the_next() {
        assert!(
            !segment_would_exceed_limit(
                CAPTURE_SEGMENT_MAGIC.len() as u64,
                CAPTURE_SEGMENT_MAX_BYTES + 1,
                CAPTURE_SEGMENT_MAX_BYTES,
            ),
            "one record is never rejected solely because it exceeds the segment target"
        );
        assert!(segment_would_exceed_limit(
            CAPTURE_SEGMENT_MAGIC.len() as u64 + 1,
            CAPTURE_SEGMENT_MAX_BYTES,
            CAPTURE_SEGMENT_MAX_BYTES,
        ));
    }

    fn test_index() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory index");
        conn.execute_batch(INDEX_SCHEMA_SQL).expect("index schema");
        conn
    }

    fn sample_record() -> BodyRecord {
        BodyRecord {
            event_id: "body_1700000000000_p70978_25".to_string(),
            request_id: "req_abc".to_string(),
            observed_at_unix_ms: 1700000000000,
            capture_stage: "client_inbound".to_string(),
            protocol: "anthropic".to_string(),
            upstream: Some("http://127.0.0.1:8790".to_string()),
            model: Some("glm-5.2".to_string()),
            status: Some(200),
            content_type: Some("application/json".to_string()),
            body_sha256: "a".repeat(64),
            body_bytes: 588_533,
            compressed_bytes: 140_233,
            archive_path: "/archive/2026/07/26/segments/capture-1.sbcap".to_string(),
            storage: "archive_segment".to_string(),
            protected: true,
            redaction_state: "raw_local".to_string(),
            threshold_shrunk: true,
            metadata: serde_json::json!({"capture_metadata": {"tap": "zai-claude-tap"}}),
        }
    }

    fn stored_location(conn: &Connection, event_id: &str) -> (String, String) {
        conn.query_row(
            "SELECT archive_path, storage FROM body_events WHERE event_id = ?1",
            params![event_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("row")
    }

    /// The 2026-07-27 outage in one assertion: archiving rewrote a frame's
    /// location, the replay no longer matched byte-for-byte, and the resulting
    /// error disabled body capture on all ten taps. Same bytes at a new path is a
    /// reconcile, never a collision.
    #[test]
    fn recovery_reconciles_a_relocated_body_instead_of_colliding() {
        let conn = test_index();
        let original = sample_record();
        insert_record_on(&conn, &original).expect("first insert");

        let mut relocated = original.clone();
        relocated.archive_path = "/Volumes/Work/archive/2026/07/26/segments/capture-1.sbcap".into();
        relocated.storage = "spool_segment".to_string();

        insert_recovered_record_on(&conn, &relocated)
            .expect("a relocated body must reconcile, not collide");

        let (path, storage) = stored_location(&conn, &original.event_id);
        assert_eq!(
            path, relocated.archive_path,
            "recovery is authoritative about where it just found the bytes"
        );
        assert_eq!(storage, "spool_segment");
    }

    /// Recovery must not relitigate the record's content — only its location. An
    /// index row enriched after the frame was written (status/model learned on
    /// response completion) must survive a replay of the older frame.
    #[test]
    fn recovery_does_not_regress_enrichment_the_frame_predates() {
        let conn = test_index();
        let mut enriched = sample_record();
        enriched.status = Some(200);
        enriched.model = Some("glm-5.2".to_string());
        insert_record_on(&conn, &enriched).expect("insert enriched");

        let mut older_frame = enriched.clone();
        older_frame.status = None;
        older_frame.model = None;
        older_frame.archive_path = "/archive/moved.sbcap".to_string();

        insert_recovered_record_on(&conn, &older_frame).expect("reconcile");

        let (status, model): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT status, model FROM body_events WHERE event_id = ?1",
                params![enriched.event_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row");
        assert_eq!(status, Some(200), "a stale frame must not erase enrichment");
        assert_eq!(model.as_deref(), Some("glm-5.2"));
    }

    /// Different bytes under one event id is a REAL collision and must still fail —
    /// and must name the field, because the message that named nothing is what made
    /// the outage take a live archaeology session to diagnose.
    #[test]
    fn recovery_still_rejects_different_bytes_and_names_the_field() {
        let conn = test_index();
        let original = sample_record();
        insert_record_on(&conn, &original).expect("insert");

        let mut different = original.clone();
        different.body_sha256 = "b".repeat(64);

        let err = insert_recovered_record_on(&conn, &different)
            .expect_err("a different body under the same event id is a real collision");
        let message = err.to_string();
        assert!(
            message.contains("body_sha256"),
            "the error must name the diverging field, got: {message}"
        );
    }

    #[test]
    fn recovery_identity_ignores_projection_but_catches_substance() {
        let base = sample_record();

        let mut projection_only = base.clone();
        projection_only.storage = "spool_segment".to_string();
        projection_only.compressed_bytes = 1;
        projection_only.metadata = serde_json::json!({"different": true});
        assert!(
            recovery_identity_divergence(&base, &projection_only).is_empty(),
            "storage/compressed_bytes/metadata are projections, not identity"
        );

        let mut substantive = base.clone();
        substantive.capture_stage = "upstream_response".to_string();
        substantive.body_bytes = 7;
        let diverged = recovery_identity_divergence(&base, &substantive);
        assert!(diverged.contains(&"capture_stage"));
        assert!(diverged.contains(&"body_bytes"));
    }
}
