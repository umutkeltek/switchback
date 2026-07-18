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
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{Month, OffsetDateTime};

static NEXT_EVENT_ID: AtomicU64 = AtomicU64::new(1);

const DEFAULT_INLINE_THRESHOLD_BYTES: u64 = 256 * 1024;
/// Above this DB size, exact `COUNT(*)` is too expensive, so `status()` reports
/// `MAX(rowid)` approximations (flagged approximate) instead.
const PRECISE_STATUS_DB_SIZE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 250;
const ZSTD_LEVEL: i32 = 3;
const DAY_MS: i64 = 86_400_000;

/// Default retention window: keep this many recent UTC days locally. Older days
/// whose archive day dir is absent (exported + pruned) are GC candidates.
pub const DEFAULT_KEEP_DAYS: u64 = 14;
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

#[derive(Debug, Clone, Serialize)]
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
    pub status: String,
    pub index_path: String,
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
    /// UTC day (YYYY-MM-DD) at/below which days become retention candidates.
    pub retention_cutoff_day: String,
    /// Count of local archive day dirs (`YYYY/MM/DD`) currently present.
    pub local_archive_day_dirs: u64,
    /// Oldest local archive day dir (YYYY-MM-DD), if any.
    pub oldest_local_day_dir: Option<String>,
    /// Size in bytes of the frozen legacy jsonl artifact, if present.
    pub legacy_jsonl_bytes: Option<u64>,
    pub protected_paths: Vec<String>,
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

impl BodyLogger {
    pub fn new(config: BodyLoggerConfig) -> Result<Self> {
        if config.inline_threshold_bytes == 0 {
            return Err(BodyLogError::new(
                "inline_threshold_bytes must be greater than zero",
            ));
        }
        fs::create_dir_all(&config.state_dir)?;
        let body_dir = config.state_dir.join("body");
        fs::create_dir_all(&body_dir)?;
        let spool_dir = body_dir.join("spool");
        fs::create_dir_all(&spool_dir)?;
        if let Some(path) = config.legacy_jsonl.as_ref().and_then(|p| p.parent()) {
            fs::create_dir_all(path)?;
        }
        let index_path = body_dir.join("index.sqlite");
        copy_legacy_index_if_needed(&config.state_dir, &index_path)?;
        let logger = Self {
            config,
            index_path,
            spool_dir,
        };
        logger.init_db()?;
        Ok(logger)
    }

    pub fn from_legacy_sink(path: PathBuf) -> Result<Self> {
        Self::new(BodyLoggerConfig::from_legacy_sink(path))
    }

    pub fn open_existing(config: BodyLoggerConfig) -> Result<Option<Self>> {
        let body_dir = config.state_dir.join("body");
        let current_index = body_dir.join("index.sqlite");
        let legacy_index = config.state_dir.join("body-index.sqlite");
        let index_path = if current_index.exists() || !legacy_index.exists() {
            current_index
        } else {
            legacy_index
        };
        if !index_path.exists() {
            return Ok(None);
        }
        Ok(Some(Self {
            config,
            index_path,
            spool_dir: body_dir.join("spool"),
        }))
    }

    pub fn record(&self, input: BodyEventInput) -> Result<BodyRecord> {
        self.record_at(input, now_unix_ms())
    }

    /// Record a capture with an explicit observed-at timestamp.
    ///
    /// Production always calls [`BodyLogger::record`] (which stamps "now"); this
    /// seam exists so lifecycle tests can place records on specific UTC days
    /// through the real write path (blob placement + day-routing) instead of
    /// hand-crafting rows.
    #[doc(hidden)]
    pub fn record_at(&self, input: BodyEventInput, observed_at_unix_ms: i64) -> Result<BodyRecord> {
        let now_ms = observed_at_unix_ms;
        let body_sha256 = sha256_hex(&input.body);
        let compressed = zstd::stream::encode_all(input.body.as_slice(), ZSTD_LEVEL)?;
        let location = self.blob_location(now_ms, &body_sha256);

        if let Some(parent) = location.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !location.path.exists() {
            fs::write(&location.path, &compressed)?;
        }

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

    pub fn read_blob(&self, body_sha256: &str) -> Result<Vec<u8>> {
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
        Ok(BodyStatus {
            status: body_status_text(archive_available, spool_backlog, spool_backlog_exact)
                .to_string(),
            index_path: self.index_path.to_string_lossy().into_owned(),
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
            retention_cutoff_day: format_day_ms(cutoff_ms),
            local_archive_day_dirs,
            oldest_local_day_dir,
            legacy_jsonl_bytes,
            protected_paths,
        })
    }

    pub fn status_for_config(config: BodyLoggerConfig) -> Result<BodyStatus> {
        let body_dir = config.state_dir.join("body");
        let current_index = body_dir.join("index.sqlite");
        let legacy_index = config.state_dir.join("body-index.sqlite");
        let index_path = if current_index.exists() || !legacy_index.exists() {
            current_index
        } else {
            legacy_index
        };
        let spool_dir = body_dir.join("spool");
        if !index_path.exists() {
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
                status: body_status_text(archive_available, 0, true).to_string(),
                index_path: index_path.to_string_lossy().into_owned(),
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
                retention_cutoff_day: format_day_ms(cutoff_ms),
                local_archive_day_dirs,
                oldest_local_day_dir,
                legacy_jsonl_bytes,
                protected_paths,
            });
        }
        BodyLogger {
            config,
            index_path,
            spool_dir,
        }
        .status()
    }

    /// Fail-closed retention GC for `index.sqlite` plus spool drain.
    ///
    /// Dry-run by default; mutates only with `opts.confirm`. Refuses entirely
    /// (mutating nothing) unless the archive root is mounted — absence of a day
    /// dir must never be conflated with an unmounted volume.
    pub fn gc(&self, opts: GcOptions) -> Result<GcReport> {
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
        let conn = open_index_connection(&self.index_path)?;

        if !opts.drain_only {
            let candidate_days = self.collect_candidate_days(&conn, cutoff_ms, &mut report)?;
            if opts.confirm && !candidate_days.is_empty() {
                report.events_deleted =
                    self.delete_candidate_events(&conn, &candidate_days, batch)?;
                report.blobs_deleted =
                    self.delete_candidate_blobs(&conn, &candidate_days, batch)?;
            }
        }

        if opts.confirm {
            self.drain_spool(&conn, now_ms, &mut report)?;
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
                       AND storage <> 'spool'",
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
                         AND storage <> 'spool' \
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
                if storage != "archive" {
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

    /// Move every spool blob file into today's archive day partition and every
    /// spool day-file into its own day partition, updating index rows. Requires
    /// the archive to be mounted (checked by the caller).
    fn drain_spool(&self, conn: &Connection, now_ms: i64, report: &mut GcReport) -> Result<()> {
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
        conn.execute_batch(
            "
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
            CREATE INDEX IF NOT EXISTS idx_body_events_request_id
              ON body_events(request_id);
            CREATE INDEX IF NOT EXISTS idx_body_events_observed_at
              ON body_events(observed_at_unix_ms);
            CREATE INDEX IF NOT EXISTS idx_body_events_hash
              ON body_events(body_sha256);
            ",
        )?;
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
                fs::create_dir_all(day_dir)?;
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
        fs::create_dir_all(day_dir)?;
        let path = day_dir.join("body-events.jsonl.zst");
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        let compressed = zstd::stream::encode_all(line.as_slice(), ZSTD_LEVEL)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
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

fn copy_legacy_index_if_needed(state_dir: &Path, index_path: &Path) -> Result<()> {
    if index_path.exists() {
        return Ok(());
    }
    let legacy = state_dir.join("body-index.sqlite");
    if legacy.exists() {
        fs::copy(legacy, index_path)?;
    }
    Ok(())
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
    let mut count = 0u64;
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
        fs::create_dir_all(parent)?;
    }
    if dest.exists() {
        fs::remove_file(src)?;
        return Ok(());
    }
    if fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    // Cross-device: copy + fsync + unlink.
    let data = fs::read(src)?;
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    set_owner_only(&mut opts);
    let mut file = opts.open(dest)?;
    file.write_all(&data)?;
    file.sync_all()?;
    fs::remove_file(src)?;
    Ok(())
}

/// Append `src`'s bytes to `dest` (create 0600), never clobbering existing content.
fn append_merge_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = fs::read(src)?;
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    set_owner_only(&mut opts);
    let mut file = opts.open(dest)?;
    file.write_all(&data)?;
    Ok(())
}

fn append_line_0600(path: &Path, line: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    set_owner_only(&mut opts);
    let mut file = opts.open(path)?;
    writeln!(file, "{line}")?;
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

fn open_index_connection(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))?;
    Ok(conn)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_event_id(now_ms: i64) -> String {
    let seq = NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    format!("body_{now_ms}_{seq}")
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
