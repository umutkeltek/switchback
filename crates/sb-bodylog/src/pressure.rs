use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::{archive_root_available, BodyLogError, Result};

pub const PRESSURE_STATE_SCHEMA: &str = "switchback/capture-pressure-state@1";
pub const CAPTURE_GAP_SCHEMA: &str = "switchback/capture-gap@1";
pub const BACKUP_RECEIPT_SCHEMA: &str = "switchback/capture-backup@2";

// Bodies are discarded only when the disk is nearly full or the writer is
// broken. A late backup or a large backlog is a warning, never a reason to
// discard. From 2026-10-11 the older rule (stop below 50 GB or 8 % free, resume
// at 100 GB, stop for a late backup or a large backlog) discarded 528,434 bodies
// on a 995 GB disk where capture itself used about 3 GB.
const BACKUP_WARN_AGE_MS: i64 = 6 * 60 * 60 * 1_000;
const FREE_WARN_BYTES: u64 = 100_000_000_000;
const FREE_WARN_BPS: u64 = 1_500;
const FREE_DEGRADE_BYTES: u64 = 10_000_000_000;
const FREE_DEGRADE_BPS: u64 = 200;
const FREE_RESUME_BYTES: u64 = FREE_DEGRADE_BYTES * 3 / 2;
const FREE_RESUME_BPS: u64 = FREE_DEGRADE_BPS * 3 / 2;
const UNBACKED_WARN_BYTES: u64 = 50_000_000_000;
const HEALTHY_BACKUP_CYCLES_TO_RESUME: u32 = 2;
const UNBACKED_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const UNBACKED_SNAPSHOT_INTERVAL_MS: i64 = 60 * 1_000;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    #[default]
    SegmentedFullWire,
    /// One healthy evaluation after a degrade. Full-wire capture is admitted
    /// again while the controller stays visibly in recovery.
    HealingProbe,
    MetadataOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressureObservation {
    pub free_bytes: u64,
    pub capacity_bytes: u64,
    pub last_backup_success_at_unix_ms: Option<i64>,
    pub backup_generation: u64,
    pub unbacked_bytes: u64,
}

/// The pressure thresholds this build enforces, published so a consumer can DERIVE a reclaim
/// target instead of hard-coding a copy of these numbers.
///
/// Publishing them is the fix for a real outage: capture degraded and could not heal because
/// free space sat between `degrade` and `resume`, while a separate storage guard — carrying its
/// own unrelated constant — considered the same disk merely "warning" and reclaimed nothing.
/// Two thresholds on one resource, neither aware of the other, and a ~63 GB band in which
/// capture was dead and the guard was content. A reclaimer that reads these cannot drift away
/// from the consumer it is protecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressureThresholds {
    /// Capture degrades to metadata-only below the LARGER of these two. The safety floor.
    pub degrade_free_bytes: u64,
    pub degrade_free_bps: u64,
    /// A degraded capture may only resume above the LARGER of these two. Reclaim must target
    /// it, not `degrade`: clearing the floor alone leaves capture stuck.
    pub resume_free_bytes: u64,
    pub resume_free_bps: u64,
    /// Consecutive healthy evaluations required before full-wire capture resumes.
    pub healthy_backup_cycles_to_resume: u32,
}

impl PressureThresholds {
    pub fn current() -> Self {
        Self {
            degrade_free_bytes: FREE_DEGRADE_BYTES,
            degrade_free_bps: FREE_DEGRADE_BPS,
            resume_free_bytes: FREE_RESUME_BYTES,
            resume_free_bps: FREE_RESUME_BPS,
            healthy_backup_cycles_to_resume: HEALTHY_BACKUP_CYCLES_TO_RESUME,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressureStatus {
    pub schema: String,
    pub mode: CaptureMode,
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
    /// Which resume gate is holding a degraded capture in metadata-only.
    /// Empty while full-wire, and empty while degraded for a reason `reasons`
    /// already names (free space below the degrade line).
    ///
    /// `healthy_backup_cycles: 0` alone is not a diagnosis: it is the OUTCOME of
    /// a gate that failed, and without the gate name the state reads as a broken
    /// counter.
    #[serde(default)]
    pub resume_blockers: Vec<String>,
    pub healthy_backup_cycles: u32,
    pub last_backup_generation: u64,
    pub writer_failures: u64,
    pub metadata_only_events: u64,
    pub queue_depth: u64,
    pub queue_drops: u64,
    pub queue_high_watermark: u64,
    /// Count of capture jobs that the tap worker couldn't enqueue in-process
    /// (queue at capacity) and instead spooled to disk to preserve the
    /// never-drop invariant. Persisted in-process state of the tap; the
    /// shared state file does not include this number on purpose — a process
    /// restart whose live state does not match disk must not silently clear
    /// the spool evidence.
    #[serde(default)]
    pub queue_full_spools: u64,
    /// The longest a single submit waited for queue room, in milliseconds.
    /// Process-local; cleared on restart. Health probes use this to detect a
    /// worker that's starved (rising = bad).
    #[serde(default)]
    pub queue_wait_ms_last: u64,
    /// Highest `queue_wait_ms_last` observed this process; lets a probe
    /// catch transient starvation that already drained back to 0 ms.
    #[serde(default)]
    pub queue_wait_ms_high_watermark: u64,
    pub unbacked_bytes: u64,
    pub free_bytes: Option<u64>,
    pub capacity_bytes: Option<u64>,
    pub limiting_filesystem: Option<String>,
    pub last_backup_success_at_unix_ms: Option<i64>,
    pub backup_age_ms: Option<i64>,
    pub verified_through_day: Option<String>,
    pub updated_at_unix_ms: i64,
    /// The thresholds this build enforces. Always populated.
    pub thresholds: PressureThresholds,
    /// The free-space lines resolved for `capacity_bytes`, so a consumer reads
    /// them instead of deriving them again. Below the warn line is a warning
    /// only; below the degrade line capture discards bodies; a degraded capture
    /// resumes at or above the resume line. `None` until a capacity is observed.
    #[serde(default)]
    pub free_warn_line_bytes: Option<u64>,
    #[serde(default)]
    pub free_degrade_line_bytes: Option<u64>,
    #[serde(default)]
    pub free_resume_line_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedPressureState {
    schema: String,
    mode: CaptureMode,
    reasons: Vec<String>,
    #[serde(default)]
    resume_blockers: Vec<String>,
    healthy_backup_cycles: u32,
    last_backup_generation: u64,
    writer_failures: u64,
    /// True while capture is off only because a write to the index failed.
    ///
    /// The disk was healthy when that happened, so the free-space margin that
    /// guards a recovery from pressure says nothing about whether the writer
    /// works again. `evaluate` reads this to keep it out of a writer recovery.
    #[serde(default)]
    writer_failure_recovery: bool,
    #[serde(default)]
    queue_depth: u64,
    #[serde(default)]
    queue_drops: u64,
    #[serde(default)]
    queue_high_watermark: u64,
    unbacked_bytes: u64,
    updated_at_unix_ms: i64,
}

impl Default for PersistedPressureState {
    fn default() -> Self {
        Self {
            schema: PRESSURE_STATE_SCHEMA.to_string(),
            mode: CaptureMode::SegmentedFullWire,
            reasons: Vec::new(),
            resume_blockers: Vec::new(),
            healthy_backup_cycles: 0,
            last_backup_generation: 0,
            writer_failures: 0,
            writer_failure_recovery: false,
            queue_depth: 0,
            queue_drops: 0,
            queue_high_watermark: 0,
            unbacked_bytes: 0,
            updated_at_unix_ms: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PressureController {
    path: PathBuf,
    lock_path: PathBuf,
    state: PersistedPressureState,
    latest: PressureStatus,
    last_persisted_at_unix_ms: i64,
    latest_verified_through_day: Option<String>,
    latest_limiting_filesystem: Option<String>,
    local_queue_depth: u64,
    local_queue_high_watermark: u64,
}

impl PressureController {
    pub(crate) fn load(body_dir: &Path) -> Self {
        let path = body_dir.join("pressure-state.json");
        let state = read_persisted_state(&path);
        let latest = status_from_state(&state);
        Self {
            path,
            lock_path: body_dir.join("pressure-state.lock"),
            last_persisted_at_unix_ms: state.updated_at_unix_ms,
            state,
            latest,
            latest_verified_through_day: None,
            latest_limiting_filesystem: None,
            local_queue_depth: 0,
            local_queue_high_watermark: 0,
        }
    }

    /// Seed the cached `metadata_only_events` counter from the index. Done
    /// once at open time so we never pay for `COUNT(*)` on the capture hot
    /// path; the in-memory counter is incremented as events are recorded.
    ///
    /// Errors are swallowed because a missing/unreadable index is the normal
    /// state for fresh installs and the counter will simply start at 0;
    /// capture cannot run without the index anyway, so callers will surface a
    /// clearer error from the path that actually needs it.
    pub(crate) fn seed_metadata_only_count(&mut self, conn: &Connection) {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM body_events WHERE storage = 'metadata_only'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        self.latest.metadata_only_events = count.max(0) as u64;
    }

    pub(crate) fn status(&self) -> PressureStatus {
        self.latest.clone()
    }

    /// Records a submit-time spill to disk because the in-process queue was
    /// at capacity. Increments `queue_full_spools` and updates the queue-wait
    /// high watermark so a probe can detect starvation without parsing
    /// `metadata_only_events`.
    pub(crate) fn note_queue_full_spool(&mut self, wait_ms: u64) {
        self.latest.queue_full_spools = self.latest.queue_full_spools.saturating_add(1);
        self.latest.queue_wait_ms_last = wait_ms;
        self.latest.queue_wait_ms_high_watermark =
            self.latest.queue_wait_ms_high_watermark.max(wait_ms);
    }

    /// Records the measured submit wait, even when the queue had room. Lets
    /// a probe distinguish "queue healthy" from "queue recovers after a
    /// burst" without a per-event scan.
    pub(crate) fn note_queue_wait(&mut self, wait_ms: u64) {
        self.latest.queue_wait_ms_last = wait_ms;
        self.latest.queue_wait_ms_high_watermark =
            self.latest.queue_wait_ms_high_watermark.max(wait_ms);
    }

    pub(crate) fn observe(
        &mut self,
        state_dir: &Path,
        archive_root: &Path,
    ) -> Result<PressureObservation> {
        let mut filesystems = vec![(
            state_dir.to_string_lossy().into_owned(),
            filesystem_capacity(state_dir)?,
        )];
        if archive_root_available(archive_root) {
            filesystems.push((
                archive_root.to_string_lossy().into_owned(),
                filesystem_capacity(archive_root)?,
            ));
        }
        let (limiting_filesystem, (free_bytes, capacity_bytes)) =
            most_constrained_filesystem(filesystems)
                .ok_or_else(|| BodyLogError::new("no capture filesystem was observable"))?;
        self.latest_limiting_filesystem = Some(limiting_filesystem);
        let receipt = read_latest_backup_receipt(&backup_receipt_path(state_dir))?;
        self.latest_verified_through_day = receipt
            .as_ref()
            .and_then(|value| value.verified_through_day.clone());
        let backup_generation = receipt
            .as_ref()
            .map_or(self.state.last_backup_generation, |value| value.generation);
        Ok(PressureObservation {
            free_bytes,
            capacity_bytes,
            last_backup_success_at_unix_ms: receipt
                .as_ref()
                .and_then(|value| value.completed_at_unix_ms),
            backup_generation,
            unbacked_bytes: self.state.unbacked_bytes,
        })
    }

    pub(crate) fn evaluate(
        &mut self,
        observation: PressureObservation,
        now_unix_ms: i64,
    ) -> Result<PressureStatus> {
        let _shared = lock_pressure_state(&self.lock_path)?;
        self.reload_shared();
        let previous = self.state.clone();
        let backup_age_ms = observation
            .last_backup_success_at_unix_ms
            .map(|completed| now_unix_ms.saturating_sub(completed).max(0));
        let capacity_bytes = observation.capacity_bytes;
        let warn_line = warn_free_threshold_bytes(capacity_bytes);
        let degrade_line = degrade_free_threshold_bytes(capacity_bytes);
        let resume_line = resume_free_threshold_bytes(capacity_bytes);

        let mut severe = Vec::new();
        if observation.free_bytes < degrade_line {
            severe.push("free_bytes".to_string());
        }

        let mut warnings = Vec::new();
        if backup_age_ms.is_some_and(|age| age > BACKUP_WARN_AGE_MS) {
            warnings.push("backup_stale".to_string());
        } else if backup_age_ms.is_none() {
            warnings.push("backup_missing".to_string());
        }
        if observation.free_bytes < warn_line {
            warnings.push("free_bytes".to_string());
        }
        if observation.unbacked_bytes > UNBACKED_WARN_BYTES {
            warnings.push("unbacked_bytes".to_string());
        }

        if !severe.is_empty() {
            self.state.mode = CaptureMode::MetadataOnly;
            self.state.reasons = severe;
            self.state.resume_blockers.clear();
            self.state.healthy_backup_cycles = 0;
            // Real pressure now owns the degrade, whatever started it. The
            // free-space margin applies in full.
            self.state.writer_failure_recovery = false;
        } else if matches!(
            self.state.mode,
            CaptureMode::MetadataOnly | CaptureMode::HealingProbe
        ) {
            // A failed index write turns capture off with no pressure on the
            // disk. The free-space margin keeps capture from flapping around the
            // degrade line; it says nothing about whether the writer works again,
            // so it may not hold a writer recovery.
            //
            // Live 2026-11-12: a `database is locked` write failure turned capture
            // off with 107 GB free, and the free-space margin then held the resume
            // for eleven and a half hours with a healthy disk and a writer that
            // had recovered within the second.
            let writer_recovery = self.state.writer_failure_recovery;
            // Name the gate that holds the heal. A degraded controller publishes
            // `healthy_backup_cycles: 0`, which is an outcome, not a cause.
            let mut resume_blockers = Vec::new();
            if !writer_recovery && observation.free_bytes < resume_line {
                resume_blockers.push("free_bytes_below_resume".to_string());
            }
            let healthy_for_resume = resume_blockers.is_empty();
            self.state.resume_blockers = resume_blockers;
            if healthy_for_resume {
                self.state.healthy_backup_cycles =
                    self.state.healthy_backup_cycles.saturating_add(1);
            } else {
                self.state.healthy_backup_cycles = 0;
            }

            if self.state.healthy_backup_cycles >= HEALTHY_BACKUP_CYCLES_TO_RESUME {
                self.state.mode = CaptureMode::SegmentedFullWire;
                self.state.reasons.clear();
                self.state.resume_blockers.clear();
                self.state.healthy_backup_cycles = 0;
                self.state.writer_failure_recovery = false;
            } else if healthy_for_resume {
                self.state.mode = CaptureMode::HealingProbe;
                self.state.reasons = vec![if writer_recovery {
                    "healing_writer_probe".to_string()
                } else {
                    "healing_probe".to_string()
                }];
            } else {
                self.state.mode = CaptureMode::MetadataOnly;
                self.state.reasons = self.state.resume_blockers.clone();
            }
        } else {
            self.state.reasons.clear();
            self.state.resume_blockers.clear();
            self.state.healthy_backup_cycles = 0;
            self.state.writer_failure_recovery = false;
        }

        self.state.last_backup_generation = self
            .state
            .last_backup_generation
            .max(observation.backup_generation);
        self.state.unbacked_bytes = observation.unbacked_bytes;
        self.state.updated_at_unix_ms = now_unix_ms;
        self.latest = PressureStatus {
            schema: PRESSURE_STATE_SCHEMA.to_string(),
            mode: self.state.mode,
            reasons: self.state.reasons.clone(),
            warnings,
            resume_blockers: self.state.resume_blockers.clone(),
            healthy_backup_cycles: self.state.healthy_backup_cycles,
            last_backup_generation: self.state.last_backup_generation,
            writer_failures: self.state.writer_failures,
            metadata_only_events: self.latest.metadata_only_events,
            queue_depth: self.local_queue_depth,
            queue_drops: self.state.queue_drops,
            queue_high_watermark: self.local_queue_high_watermark,
            queue_full_spools: self.latest.queue_full_spools,
            queue_wait_ms_last: self.latest.queue_wait_ms_last,
            queue_wait_ms_high_watermark: self.latest.queue_wait_ms_high_watermark,
            unbacked_bytes: observation.unbacked_bytes,
            free_bytes: Some(observation.free_bytes),
            capacity_bytes: Some(observation.capacity_bytes),
            limiting_filesystem: self.latest_limiting_filesystem.clone(),
            last_backup_success_at_unix_ms: observation.last_backup_success_at_unix_ms,
            backup_age_ms,
            verified_through_day: self.latest_verified_through_day.clone(),
            updated_at_unix_ms: now_unix_ms,
            thresholds: PressureThresholds::current(),
            free_warn_line_bytes: Some(warn_line),
            free_degrade_line_bytes: Some(degrade_line),
            free_resume_line_bytes: Some(resume_line),
        };
        // Leaving full-wire capture means every payload from here on is thrown
        // away. That is the most consequential thing this process can decide, and
        // it used to happen with no output at all: a degraded controller looked
        // exactly like a healthy one, and eleven hours of provider bodies were
        // discarded before anyone noticed. Say it, at a level that carries.
        if self.state.mode != previous.mode {
            match self.state.mode {
                CaptureMode::SegmentedFullWire => tracing::info!(
                    left_reasons = ?previous.reasons,
                    "capture resumed full-wire body capture"
                ),
                // A probe captures full bodies again; only metadata-only discards.
                CaptureMode::HealingProbe => tracing::info!(
                    reasons = ?self.state.reasons,
                    free_bytes = observation.free_bytes,
                    unbacked_bytes = observation.unbacked_bytes,
                    "capture is probing full-wire body capture again"
                ),
                degraded => tracing::warn!(
                    mode = ?degraded,
                    reasons = ?self.state.reasons,
                    free_bytes = observation.free_bytes,
                    unbacked_bytes = observation.unbacked_bytes,
                    backup_age_ms = ?backup_age_ms,
                    "capture left full-wire mode: request and response payloads are now being DISCARDED"
                ),
            }
        }
        // A held resume is the long outage: it discards every body for as long
        // as a gate stays closed, and it used to do so without one line of
        // output. Name the gates whenever the set of them changes.
        if self.state.mode == CaptureMode::MetadataOnly
            && !self.state.resume_blockers.is_empty()
            && self.state.resume_blockers != previous.resume_blockers
        {
            tracing::warn!(
                resume_blockers = ?self.state.resume_blockers,
                free_bytes = observation.free_bytes,
                free_resume_line_bytes = resume_line,
                unbacked_bytes = observation.unbacked_bytes,
                backup_age_ms = ?backup_age_ms,
                "capture resume is held: request and response payloads stay DISCARDED until these gates clear"
            );
        }
        // `unbacked_bytes` is deliberately outside `persisted_changed`: it moves
        // with every captured body, and persisting each move would rewrite the
        // state file on every evaluation. It used to be a byte counter fed only
        // by `note_full_capture`, which never runs in metadata-only mode, so a
        // degraded controller with a held resume reached a fixed point and
        // stopped persisting. Live 2026-08-22: the file froze for 3h45m holding
        // `unbacked_bytes: 0` while the projection read 26.4 GB.
        //
        // Measure the drift against what is actually on disk. Same write budget,
        // and no mode can freeze the file while the number it publishes moves.
        let unbacked_drift = self.state.unbacked_bytes.abs_diff(previous.unbacked_bytes);
        let snapshot_due = unbacked_drift > 0
            && (unbacked_drift >= UNBACKED_SNAPSHOT_BYTES
                || now_unix_ms.saturating_sub(previous.updated_at_unix_ms)
                    >= UNBACKED_SNAPSHOT_INTERVAL_MS);
        if persisted_changed(&previous, &self.state) || snapshot_due {
            self.persist()?;
            self.last_persisted_at_unix_ms = now_unix_ms;
        }
        Ok(self.latest.clone())
    }

    pub(crate) fn mark_writer_failure(
        &mut self,
        now_unix_ms: i64,
        reason: &str,
    ) -> Result<PressureStatus> {
        let _shared = lock_pressure_state(&self.lock_path)?;
        self.reload_shared();
        // Only a failure that takes capture out of full-wire (or out of its own
        // probe) is a writer recovery. A failure during a pressure degrade leaves
        // the pressure gates in charge.
        let left_full_wire = self.state.mode == CaptureMode::SegmentedFullWire;
        self.state.writer_failure_recovery = left_full_wire || self.state.writer_failure_recovery;
        if left_full_wire {
            tracing::warn!(
                reason,
                "capture left full-wire mode after a failed capture write: request and response payloads are now being DISCARDED"
            );
        }
        self.state.mode = CaptureMode::MetadataOnly;
        self.state.reasons = vec![format!("writer_failed:{reason}")];
        self.state.resume_blockers.clear();
        self.state.healthy_backup_cycles = 0;
        self.state.writer_failures = self.state.writer_failures.saturating_add(1);
        self.state.updated_at_unix_ms = now_unix_ms;
        self.latest = self.status_from_shared_state();
        self.persist()?;
        self.last_persisted_at_unix_ms = now_unix_ms;
        Ok(self.latest.clone())
    }

    pub(crate) fn note_full_capture(&mut self, body_bytes: u64) {
        self.state.unbacked_bytes = self.state.unbacked_bytes.saturating_add(body_bytes);
        self.latest.unbacked_bytes = self.state.unbacked_bytes;
    }

    pub(crate) fn reconcile_unbacked_bytes(
        &mut self,
        unbacked_bytes: u64,
        now_unix_ms: i64,
    ) -> Result<()> {
        let _shared = lock_pressure_state(&self.lock_path)?;
        self.reload_shared();
        self.state.unbacked_bytes = unbacked_bytes;
        self.state.updated_at_unix_ms = now_unix_ms;
        self.latest.unbacked_bytes = unbacked_bytes;
        self.latest.updated_at_unix_ms = now_unix_ms;
        self.persist()?;
        self.last_persisted_at_unix_ms = now_unix_ms;
        Ok(())
    }

    pub(crate) fn set_metadata_only_events(&mut self, events: u64) {
        self.latest.metadata_only_events = events;
    }

    /// Increment the cached metadata-only event counter by one.
    ///
    /// The capture hot path used to issue a full `COUNT(*) WHERE
    /// storage='metadata_only'` scan after every recorded gap, turning each
    /// `metadata_only` event into a ~4.5s O(n) read on a 900k-row store
    /// (2026-08-21 outage). We now keep `latest.metadata_only_events` in
    /// memory as the source of truth for the running counter, seeded once at
    /// startup and incremented on every recorded gap. Persistence is
    /// unnecessary — this is a process-local observe metric, like
    /// `local_queue_depth`.
    pub(crate) fn note_metadata_only_recorded(&mut self) {
        self.latest.metadata_only_events = self.latest.metadata_only_events.saturating_add(1);
    }

    pub(crate) fn note_queue_enqueued(&mut self) {
        self.local_queue_depth = self.local_queue_depth.saturating_add(1);
        self.local_queue_high_watermark =
            self.local_queue_high_watermark.max(self.local_queue_depth);
        self.latest.queue_depth = self.local_queue_depth;
        self.latest.queue_high_watermark = self.local_queue_high_watermark;
    }

    pub(crate) fn note_queue_dequeued(&mut self) {
        self.local_queue_depth = self.local_queue_depth.saturating_sub(1);
        self.latest.queue_depth = self.local_queue_depth;
    }

    pub(crate) fn note_queue_drop(&mut self, now_unix_ms: i64) -> Result<()> {
        let _shared = lock_pressure_state(&self.lock_path)?;
        self.reload_shared();
        self.state.queue_drops = self.state.queue_drops.saturating_add(1);
        self.state.updated_at_unix_ms = now_unix_ms;
        self.latest.queue_drops = self.state.queue_drops;
        self.latest.updated_at_unix_ms = now_unix_ms;
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        atomic_write_private(&self.path, &serde_json::to_vec_pretty(&self.state)?)
    }

    fn reload_shared(&mut self) {
        self.state = read_persisted_state(&self.path);
        self.state.queue_depth = 0;
        self.state.queue_high_watermark = 0;
    }

    fn status_from_shared_state(&self) -> PressureStatus {
        let mut status = status_from_state(&self.state);
        status.metadata_only_events = self.latest.metadata_only_events;
        status.queue_depth = self.local_queue_depth;
        status.queue_high_watermark = self.local_queue_high_watermark;
        status.queue_full_spools = self.latest.queue_full_spools;
        status.queue_wait_ms_last = self.latest.queue_wait_ms_last;
        status.queue_wait_ms_high_watermark = self.latest.queue_wait_ms_high_watermark;
        status.limiting_filesystem = self.latest_limiting_filesystem.clone();
        status
    }
}

pub(crate) fn backup_receipt_path(state_dir: &Path) -> PathBuf {
    state_dir
        .join("body")
        .join("backup")
        .join("latest-receipt.json")
}

#[derive(Debug, Deserialize)]
struct LatestBackupReceipt {
    schema: String,
    generation: u64,
    completed_at_unix_ms: Option<i64>,
    verified_through_day: Option<String>,
}

fn read_latest_backup_receipt(path: &Path) -> Result<Option<LatestBackupReceipt>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let receipt: LatestBackupReceipt = serde_json::from_slice(&bytes)?;
    if receipt.schema != BACKUP_RECEIPT_SCHEMA {
        return Err(BodyLogError::new(format!(
            "unsupported backup receipt schema {}",
            receipt.schema
        )));
    }
    Ok(Some(receipt))
}

fn percent_of_capacity(capacity_bytes: u64, bps: u64) -> u64 {
    bps.saturating_mul(capacity_bytes) / 10_000
}

/// Below this, capture discards bodies: the LARGER of 10 GB and 2 % of the
/// filesystem. An unknown (zero) capacity leaves the absolute 10 GB.
fn degrade_free_threshold_bytes(capacity_bytes: u64) -> u64 {
    FREE_DEGRADE_BYTES.max(percent_of_capacity(capacity_bytes, FREE_DEGRADE_BPS))
}

/// A degraded capture resumes at or above this: half again the degrade line,
/// so capture does not flap around it.
fn resume_free_threshold_bytes(capacity_bytes: u64) -> u64 {
    degrade_free_threshold_bytes(capacity_bytes).saturating_mul(3) / 2
}

/// Below this, free space is a warning only: the SMALLER of 100 GB and 15 % of
/// the filesystem, and never below the resume line. A disk guard reclaims to
/// this line, so it must sit at or above the point where a degraded capture
/// resumes; on a disk above about 3.3 TB, or below 100 GB, the plain
/// `min(100 GB, 15 %)` falls under the resume line.
fn warn_free_threshold_bytes(capacity_bytes: u64) -> u64 {
    FREE_WARN_BYTES
        .min(percent_of_capacity(capacity_bytes, FREE_WARN_BPS))
        .max(resume_free_threshold_bytes(capacity_bytes))
}

#[cfg(unix)]
fn filesystem_counter_to_u64<T: Into<u64>>(value: T) -> u64 {
    value.into()
}

#[cfg(unix)]
fn filesystem_capacity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt as _;

    let probe_path = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .ok_or_else(|| BodyLogError::new("capture filesystem has no existing ancestor"))?;
    let path = CString::new(probe_path.as_os_str().as_bytes())
        .map_err(|_| BodyLogError::new("state path contains a NUL byte"))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stats` points to
    // writable memory for one `statvfs` result.
    let rc = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: `statvfs` returned success and initialized `stats`.
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize;
    Ok((
        filesystem_counter_to_u64(stats.f_bavail).saturating_mul(block_size),
        filesystem_counter_to_u64(stats.f_blocks).saturating_mul(block_size),
    ))
}

#[cfg(not(unix))]
fn filesystem_capacity(_path: &Path) -> Result<(u64, u64)> {
    Err(BodyLogError::new(
        "filesystem pressure observation is unsupported on this platform",
    ))
}

fn status_from_state(state: &PersistedPressureState) -> PressureStatus {
    PressureStatus {
        schema: PRESSURE_STATE_SCHEMA.to_string(),
        mode: state.mode,
        reasons: state.reasons.clone(),
        warnings: Vec::new(),
        resume_blockers: state.resume_blockers.clone(),
        healthy_backup_cycles: state.healthy_backup_cycles,
        last_backup_generation: state.last_backup_generation,
        writer_failures: state.writer_failures,
        metadata_only_events: 0,
        queue_depth: state.queue_depth,
        queue_drops: state.queue_drops,
        queue_high_watermark: state.queue_high_watermark,
        queue_full_spools: 0,
        queue_wait_ms_last: 0,
        queue_wait_ms_high_watermark: 0,
        unbacked_bytes: state.unbacked_bytes,
        free_bytes: None,
        capacity_bytes: None,
        limiting_filesystem: None,
        last_backup_success_at_unix_ms: None,
        backup_age_ms: None,
        verified_through_day: None,
        updated_at_unix_ms: state.updated_at_unix_ms,
        thresholds: PressureThresholds::current(),
        free_warn_line_bytes: None,
        free_degrade_line_bytes: None,
        free_resume_line_bytes: None,
    }
}

fn persisted_changed(before: &PersistedPressureState, after: &PersistedPressureState) -> bool {
    before.mode != after.mode
        || before.reasons != after.reasons
        || before.resume_blockers != after.resume_blockers
        || before.healthy_backup_cycles != after.healthy_backup_cycles
        || before.last_backup_generation != after.last_backup_generation
        || before.writer_failures != after.writer_failures
        || before.writer_failure_recovery != after.writer_failure_recovery
        || before.queue_drops != after.queue_drops
}

fn read_persisted_state(path: &Path) -> PersistedPressureState {
    match fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<PersistedPressureState>(&bytes) {
            Ok(mut state) if state.schema == PRESSURE_STATE_SCHEMA => {
                // Queue depth is process-local and must not survive a crash or be
                // overwritten by another tap process.
                state.queue_depth = 0;
                state.queue_high_watermark = 0;
                state
            }
            Ok(_) | Err(_) => PersistedPressureState {
                mode: CaptureMode::MetadataOnly,
                reasons: vec!["pressure_state_invalid".to_string()],
                ..PersistedPressureState::default()
            },
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => PersistedPressureState::default(),
        Err(_) => PersistedPressureState {
            mode: CaptureMode::MetadataOnly,
            reasons: vec!["pressure_state_unreadable".to_string()],
            ..PersistedPressureState::default()
        },
    }
}

fn most_constrained_filesystem(
    filesystems: Vec<(String, (u64, u64))>,
) -> Option<(String, (u64, u64))> {
    filesystems.into_iter().max_by(|left, right| {
        filesystem_pressure_score(left.1)
            .cmp(&filesystem_pressure_score(right.1))
            .then_with(|| right.1 .0.cmp(&left.1 .0))
    })
}

fn filesystem_pressure_score((free_bytes, capacity_bytes): (u64, u64)) -> (u8, u64) {
    let degrade_line = degrade_free_threshold_bytes(capacity_bytes);
    let warn_line = warn_free_threshold_bytes(capacity_bytes);
    let (severity, line) = if free_bytes < degrade_line {
        (2, degrade_line)
    } else if free_bytes < warn_line {
        (1, warn_line)
    } else {
        (0, warn_line)
    };
    let margin = free_bytes
        .saturating_mul(10_000)
        .checked_div(line)
        .unwrap_or(u64::MAX);
    (severity, 10_000u64.saturating_sub(margin))
}

struct PressureStateGuard(fs::File);

impl Drop for PressureStateGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // Best effort in Drop; closing the descriptor also releases flock.
            let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn lock_pressure_state(path: &Path) -> Result<PressureStateGuard> {
    let parent = path
        .parent()
        .ok_or_else(|| BodyLogError::new("pressure lock has no parent directory"))?;
    super::ensure_private_directory(parent)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    super::set_private_file(path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(PressureStateGuard(file))
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| BodyLogError::new("pressure state has no parent directory"))?;
    super::ensure_private_directory(parent)?;
    let temp = parent.join(format!(
        ".pressure-state.tmp-{}-{}",
        std::process::id(),
        NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    if let Err(err) = (|| -> std::io::Result<()> {
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()
    })() {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    fs::rename(&temp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        most_constrained_filesystem, CaptureMode, PressureController, PressureObservation,
        FREE_WARN_BYTES,
    };
    use std::fs;

    #[test]
    fn a_never_backed_up_install_captures_full_bodies_instead_of_waiting_for_a_backup_cycle() {
        let body_dir = std::env::temp_dir().join(format!(
            "switchback-pressure-cold-start-{}",
            std::process::id()
        ));
        fs::create_dir_all(&body_dir).unwrap();
        // Exactly what a fresh or freshly migrated install persists: degraded,
        // with no backup generation ever accepted.
        fs::write(
            body_dir.join("pressure-state.json"),
            br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "metadata_only",
  "reasons": ["healing_backup_cycles"],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 0,
  "writer_failures": 0,
  "unbacked_bytes": 0,
  "updated_at_unix_ms": 1
}"#,
        )
        .unwrap();

        let mut controller = PressureController::load(&body_dir);
        assert_eq!(
            controller.status().mode,
            CaptureMode::MetadataOnly,
            "fixture must load as degraded, or this test proves nothing"
        );

        let never_backed_up = PressureObservation {
            free_bytes: FREE_WARN_BYTES * 2,
            capacity_bytes: FREE_WARN_BYTES * 4,
            last_backup_success_at_unix_ms: None,
            backup_generation: 0,
            unbacked_bytes: 0,
        };
        // Waiting for a backup cycle here would never end: a cycle needs sealed
        // segments, and metadata-only capture writes none. The first healthy
        // evaluation already captures full bodies again.
        let first = controller
            .evaluate(never_backed_up, 1_785_000_000_000)
            .unwrap();
        assert_eq!(
            first.mode,
            CaptureMode::HealingProbe,
            "reasons: {:?}",
            first.reasons
        );
        let status = controller
            .evaluate(never_backed_up, 1_785_000_000_001)
            .unwrap();
        assert_eq!(
            status.mode,
            CaptureMode::SegmentedFullWire,
            "reasons: {:?}",
            status.reasons
        );
        assert!(status.reasons.is_empty(), "reasons: {:?}", status.reasons);
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning == "backup_missing"),
            "warnings: {:?}",
            status.warnings
        );
    }

    /// REGRESSION (live outage 2026-07-26): an install that HAD backed up before could not
    /// leave metadata-only while its backup generation stood still. A no-op backup cycle
    /// writes no receipt, so the generation never moves; the heal must not depend on it.
    #[test]
    fn an_already_backed_up_install_heals_when_there_is_nothing_left_to_back_up() {
        let body_dir =
            std::env::temp_dir().join(format!("switchback-pressure-healed-{}", std::process::id()));
        fs::create_dir_all(&body_dir).unwrap();
        // The EXACT live state: degraded, generation 4 already accepted, nothing unbacked.
        fs::write(
            body_dir.join("pressure-state.json"),
            br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "metadata_only",
  "reasons": ["healing_backup_cycles"],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 4,
  "writer_failures": 0,
  "unbacked_bytes": 0,
  "updated_at_unix_ms": 1
}"#,
        )
        .unwrap();

        let mut controller = PressureController::load(&body_dir);
        assert_eq!(
            controller.status().mode,
            CaptureMode::MetadataOnly,
            "fixture must load as degraded, or this test proves nothing"
        );

        let now = 1_785_000_000_000_i64;
        // Healthy disk, fresh backup, NOTHING unbacked — and the generation never moves,
        // because a no-op cycle writes no receipt.
        let observation = PressureObservation {
            free_bytes: FREE_WARN_BYTES * 2,
            capacity_bytes: FREE_WARN_BYTES * 4,
            last_backup_success_at_unix_ms: Some(now - 60_000),
            backup_generation: 4,
            unbacked_bytes: 0,
        };

        let mut status = controller.evaluate(observation, now).unwrap();
        // First healthy cycle promotes to the probe, not straight to full wire.
        assert_ne!(
            status.mode,
            CaptureMode::MetadataOnly,
            "a healthy cycle must count even with a static generation; reasons: {:?}",
            status.reasons
        );

        status = controller.evaluate(observation, now + 1).unwrap();
        assert_eq!(
            status.mode,
            CaptureMode::SegmentedFullWire,
            "two healthy cycles must resume full-wire capture; reasons: {:?}",
            status.reasons
        );
        assert!(status.reasons.is_empty(), "reasons: {:?}", status.reasons);
    }

    /// REGRESSION (live 2026-08-07): a static generation with a small, nonzero
    /// `unbacked_bytes` (stores a transfer cannot cover) kept capture degraded
    /// while the backup ran fine. Neither may hold the heal.
    #[test]
    fn fresh_receipt_heals_even_with_static_generation_and_unbacked_data() {
        let body_dir = std::env::temp_dir().join(format!(
            "switchback-pressure-fresh-receipt-{}",
            std::process::id()
        ));
        fs::create_dir_all(&body_dir).unwrap();
        fs::write(
            body_dir.join("pressure-state.json"),
            br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "metadata_only",
  "reasons": ["healing_backup_cycles"],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 4,
  "writer_failures": 0,
  "unbacked_bytes": 0,
  "updated_at_unix_ms": 1
}"#,
        )
        .unwrap();

        let mut controller = PressureController::load(&body_dir);
        let now = 1_785_000_000_000_i64;
        let observation = PressureObservation {
            free_bytes: FREE_WARN_BYTES * 2,
            capacity_bytes: FREE_WARN_BYTES * 4,
            last_backup_success_at_unix_ms: Some(now - 60_000),
            backup_generation: 4,
            // NOT zero: residual stores the transfer cannot cover keep this nonzero forever.
            unbacked_bytes: 1_000_000_000,
        };

        controller.evaluate(observation, now).unwrap();
        let mut status = controller.evaluate(observation, now + 1).unwrap();
        assert_ne!(
            status.mode,
            CaptureMode::MetadataOnly,
            "a fresh receipt must count as a healthy cycle even with a static generation; reasons: {:?}",
            status.reasons
        );
        status = controller.evaluate(observation, now + 2).unwrap();
        assert_eq!(
            status.mode,
            CaptureMode::SegmentedFullWire,
            "two fresh-receipt cycles must resume full-wire capture; reasons: {:?}",
            status.reasons
        );
        assert!(status.reasons.is_empty(), "reasons: {:?}", status.reasons);
    }

    /// A late backup is a warning, not a resume gate: with data still pending and
    /// no generation advance, a degraded capture on a healthy disk still heals.
    #[test]
    fn a_stale_receipt_with_pending_data_does_not_hold_the_heal() {
        let body_dir = std::env::temp_dir().join(format!(
            "switchback-pressure-stale-receipt-{}",
            std::process::id()
        ));
        fs::create_dir_all(&body_dir).unwrap();
        fs::write(
            body_dir.join("pressure-state.json"),
            br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "metadata_only",
  "reasons": ["healing_backup_cycles"],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 4,
  "writer_failures": 0,
  "unbacked_bytes": 0,
  "updated_at_unix_ms": 1
}"#,
        )
        .unwrap();

        let mut controller = PressureController::load(&body_dir);
        let now = 1_785_000_000_000_i64;
        let observation = PressureObservation {
            free_bytes: FREE_WARN_BYTES * 2,
            capacity_bytes: FREE_WARN_BYTES * 4,
            last_backup_success_at_unix_ms: Some(now - 30 * 60 * 60 * 1_000),
            backup_generation: 4,
            unbacked_bytes: 1_000_000_000,
        };

        let first = controller.evaluate(observation, now).unwrap();
        assert_eq!(
            first.mode,
            CaptureMode::HealingProbe,
            "resume_blockers: {:?}",
            first.resume_blockers
        );
        assert_eq!(first.reasons, vec!["healing_probe".to_string()]);
        assert!(first.resume_blockers.is_empty());
        let status = controller.evaluate(observation, now + 1).unwrap();
        assert_eq!(
            status.mode,
            CaptureMode::SegmentedFullWire,
            "reasons: {:?}",
            status.reasons
        );
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning == "backup_stale"),
            "the late backup is still reported: {:?}",
            status.warnings
        );
    }

    #[test]
    fn selects_the_most_constrained_capture_filesystem() {
        let selected = most_constrained_filesystem(vec![
            ("state".to_string(), (150_000_000_000, 1_000_000_000_000)),
            ("archive".to_string(), (40_000_000_000, 2_000_000_000_000)),
        ])
        .unwrap();

        assert_eq!(selected.0, "archive");
        assert_eq!(selected.1, (40_000_000_000, 2_000_000_000_000));
    }
}

#[cfg(test)]
mod published_threshold_tests {
    use super::*;

    /// The published thresholds MUST be the constants the controller actually enforces.
    /// Publishing them exists so a reclaimer can target `resume` instead of carrying its own
    /// copy; a published value that drifts from the enforced one would recreate the exact
    /// two-thresholds-that-disagree outage this was written to prevent, only harder to see.
    #[test]
    fn published_thresholds_match_enforced_constants() {
        let t = PressureThresholds::current();
        assert_eq!(t.degrade_free_bytes, FREE_DEGRADE_BYTES);
        assert_eq!(t.degrade_free_bps, FREE_DEGRADE_BPS);
        assert_eq!(t.resume_free_bytes, FREE_RESUME_BYTES);
        assert_eq!(t.resume_free_bps, FREE_RESUME_BPS);
        assert_eq!(
            t.healthy_backup_cycles_to_resume,
            HEALTHY_BACKUP_CYCLES_TO_RESUME
        );
        // The wire values themselves, so a retune is a visible decision.
        assert_eq!(t.degrade_free_bytes, 10_000_000_000);
        assert_eq!(t.degrade_free_bps, 200);
        assert_eq!(t.resume_free_bytes, 15_000_000_000);
        assert_eq!(t.resume_free_bps, 300);
    }

    /// Resume must sit at or above degrade. If it ever fell below, a lane could resume into a
    /// state the controller immediately degrades again — a flap loop instead of hysteresis.
    #[test]
    fn resume_is_never_below_degrade() {
        let t = PressureThresholds::current();
        assert!(t.resume_free_bytes >= t.degrade_free_bytes);
        assert!(t.resume_free_bps >= t.degrade_free_bps);
    }

    /// A reclaimer reads these off the wire, so they must survive serialization.
    #[test]
    fn thresholds_serialize_on_status() {
        let status = status_from_state(&PersistedPressureState::default());
        let json = serde_json::to_string(&status).expect("status serializes");
        assert!(json.contains("resume_free_bytes"), "json: {json}");
        assert!(json.contains("degrade_free_bytes"), "json: {json}");
        assert!(
            json.contains("\"free_warn_line_bytes\":null"),
            "json: {json}"
        );
        assert!(
            json.contains("\"free_degrade_line_bytes\":null"),
            "json: {json}"
        );
        assert!(
            json.contains("\"free_resume_line_bytes\":null"),
            "json: {json}"
        );
    }

    /// A status written by an engine from before the resolved lines existed
    /// still reads; the lines are simply absent.
    #[test]
    fn a_status_without_resolved_lines_still_deserializes() {
        let mut json = serde_json::to_value(status_from_state(&PersistedPressureState::default()))
            .expect("status serializes");
        let object = json.as_object_mut().unwrap();
        object.remove("free_warn_line_bytes");
        object.remove("free_degrade_line_bytes");
        object.remove("free_resume_line_bytes");
        let status: PressureStatus = serde_json::from_value(json).expect("old status reads");
        assert_eq!(status.free_degrade_line_bytes, None);
    }

    fn degraded_controller_with_prior_backup(
        tag: &str,
    ) -> (PressureController, std::path::PathBuf) {
        let body_dir =
            std::env::temp_dir().join(format!("switchback-pressure-{tag}-{}", std::process::id()));
        fs::create_dir_all(&body_dir).unwrap();
        // The live 2026-07-27 state: degraded, and this install HAS backed up
        // before (generation 6).
        fs::write(
            body_dir.join("pressure-state.json"),
            br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "metadata_only",
  "reasons": ["healing_backup_cycles"],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 6,
  "writer_failures": 0,
  "unbacked_bytes": 16148020,
  "updated_at_unix_ms": 1
}"#,
        )
        .unwrap();
        let controller = PressureController::load(&body_dir);
        (controller, body_dir)
    }

    /// The disk the 2026-11-12 outage ran on: 995 GB. 25 GB free is above the
    /// degrade line (2 % here, 19.9 GB) and below the resume line (29.8 GB).
    fn between_the_degrade_floor_and_the_resume_line(now: i64) -> PressureObservation {
        PressureObservation {
            free_bytes: 25_000_000_000,
            capacity_bytes: 994_662_584_320,
            last_backup_success_at_unix_ms: Some(now - 2 * 60 * 60 * 1_000),
            backup_generation: 6,
            unbacked_bytes: 95_258_991,
        }
    }

    fn full_wire_controller_with_prior_backup(
        tag: &str,
    ) -> (PressureController, std::path::PathBuf) {
        let body_dir =
            std::env::temp_dir().join(format!("switchback-pressure-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&body_dir);
        fs::create_dir_all(&body_dir).unwrap();
        // Written by an engine from before `writer_failure_recovery` existed.
        fs::write(
            body_dir.join("pressure-state.json"),
            br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "segmented_full_wire",
  "reasons": [],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 6,
  "writer_failures": 0,
  "unbacked_bytes": 95258991,
  "updated_at_unix_ms": 1
}"#,
        )
        .unwrap();
        let controller = PressureController::load(&body_dir);
        (controller, body_dir)
    }

    /// The 2026-11-12 outage. A failed index write turned capture off while the
    /// disk sat between the degrade floor and the resume line. The free-space
    /// margin, which has nothing to do with a failed write, then held the resume
    /// for eleven and a half hours.
    #[test]
    fn a_failed_write_does_not_wait_for_the_free_space_margin_to_resume() {
        let (mut controller, dir) = full_wire_controller_with_prior_backup("writer-recovery");
        let now = 1_794_000_000_000i64;
        let observation = between_the_degrade_floor_and_the_resume_line(now);
        assert_eq!(
            controller.evaluate(observation, now).unwrap().mode,
            CaptureMode::SegmentedFullWire,
            "this disk is healthy enough to capture on"
        );

        let failed = controller
            .mark_writer_failure(now + 1, "full_wire_persistence")
            .unwrap();
        assert_eq!(failed.mode, CaptureMode::MetadataOnly);
        assert_eq!(failed.writer_failures, 1);

        // Another process of the same store reads the shared state, as the capture
        // service and its maintenance commands do.
        let mut other = PressureController::load(&dir);
        let first = other.evaluate(observation, now + 2).unwrap();
        assert_eq!(
            first.mode,
            CaptureMode::HealingProbe,
            "a writer recovery must not be held by pressure gates: {:?}",
            first.resume_blockers
        );
        assert_eq!(first.reasons, vec!["healing_writer_probe".to_string()]);
        assert!(first.resume_blockers.is_empty());
        let second = controller.evaluate(observation, now + 3).unwrap();
        assert_eq!(second.mode, CaptureMode::SegmentedFullWire);
        assert!(second.reasons.is_empty(), "reasons: {:?}", second.reasons);
        let _ = fs::remove_dir_all(dir);
    }

    /// A late backup, or bytes that wait for the next cycle, do not hold a
    /// writer recovery either.
    #[test]
    fn a_failed_write_does_not_wait_for_the_next_backup_to_resume() {
        let (mut controller, dir) =
            full_wire_controller_with_prior_backup("writer-recovery-backup");
        let now = 1_794_000_000_000i64;
        let observation = PressureObservation {
            free_bytes: FREE_WARN_BYTES * 2,
            capacity_bytes: FREE_WARN_BYTES * 4,
            // 13h old: a warning only. And 20 GB wait for the next cycle.
            last_backup_success_at_unix_ms: Some(now - 13 * 60 * 60 * 1_000),
            backup_generation: 6,
            unbacked_bytes: 20_000_000_000,
        };
        assert_eq!(
            controller.evaluate(observation, now).unwrap().mode,
            CaptureMode::SegmentedFullWire
        );
        controller
            .mark_writer_failure(now + 1, "full_wire_persistence")
            .unwrap();
        assert_eq!(
            controller.evaluate(observation, now + 2).unwrap().mode,
            CaptureMode::HealingProbe
        );
        assert_eq!(
            controller.evaluate(observation, now + 3).unwrap().mode,
            CaptureMode::SegmentedFullWire
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// The guard on the fix above. A degrade that pressure caused keeps the
    /// free-space margin, on the very disk that must not hold a writer recovery.
    #[test]
    fn a_pressure_degrade_still_waits_for_the_free_space_margin() {
        let (mut controller, dir) = degraded_controller_with_prior_backup("pressure-keeps-margin");
        let now = 1_794_000_000_000i64;
        let observation = between_the_degrade_floor_and_the_resume_line(now);
        for step in 0..3 {
            let status = controller.evaluate(observation, now + step).unwrap();
            assert_eq!(status.mode, CaptureMode::MetadataOnly);
            assert_eq!(
                status.resume_blockers,
                vec!["free_bytes_below_resume".to_string()]
            );
        }
        // A failed write on top of a pressure degrade changes nothing about that.
        controller
            .mark_writer_failure(now + 10, "full_wire_persistence")
            .unwrap();
        for step in 11..14 {
            let status = controller.evaluate(observation, now + step).unwrap();
            assert_eq!(status.mode, CaptureMode::MetadataOnly);
            assert_eq!(
                status.resume_blockers,
                vec!["free_bytes_below_resume".to_string()]
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// Real pressure takes a writer recovery over. Once the disk has been below
    /// the degrade floor, the free-space margin is needed again, however the
    /// degrade began.
    #[test]
    fn real_pressure_takes_over_a_writer_recovery() {
        let (mut controller, dir) = full_wire_controller_with_prior_backup("pressure-takes-over");
        let now = 1_794_000_000_000i64;
        let between = between_the_degrade_floor_and_the_resume_line(now);
        controller.evaluate(between, now).unwrap();
        controller
            .mark_writer_failure(now + 1, "full_wire_persistence")
            .unwrap();

        let full = PressureObservation {
            free_bytes: 15_000_000_000,
            ..between
        };
        let status = controller.evaluate(full, now + 2).unwrap();
        assert_eq!(status.mode, CaptureMode::MetadataOnly);
        assert_eq!(status.reasons, vec!["free_bytes".to_string()]);

        for step in 3..6 {
            let status = controller.evaluate(between, now + step).unwrap();
            assert_eq!(
                status.mode,
                CaptureMode::MetadataOnly,
                "a disk that was full must clear the resume line first"
            );
            assert_eq!(
                status.resume_blockers,
                vec!["free_bytes_below_resume".to_string()]
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    /// The second half of the 2026-07-27 outage. Once nothing is left to back up,
    /// every backup cycle is a legitimate `no_op` that writes no receipt, so the
    /// backup age grows because the system is clean. That age must not hold the
    /// resume.
    #[test]
    fn a_clean_install_resumes_even_though_no_op_cycles_let_the_backup_age_go_stale() {
        let (mut controller, _dir) = degraded_controller_with_prior_backup("stale-but-clean");
        let now = 1_785_000_000_000i64;
        // 13h stale: where the live system sat while resume was unreachable.
        let stale = now - (13 * 60 * 60 * 1_000);

        let observation = PressureObservation {
            free_bytes: FREE_WARN_BYTES * 2,
            capacity_bytes: FREE_WARN_BYTES * 4,
            last_backup_success_at_unix_ms: Some(stale),
            backup_generation: 6,
            unbacked_bytes: 0,
        };

        let first = controller.evaluate(observation, now).unwrap();
        assert_eq!(
            first.mode,
            CaptureMode::HealingProbe,
            "a clean install must start earning trust back, reasons: {:?}",
            first.reasons
        );

        let second = controller.evaluate(observation, now + 1).unwrap();
        assert_eq!(
            second.mode,
            CaptureMode::SegmentedFullWire,
            "nothing to transfer must not keep capture degraded, reasons: {:?}",
            second.reasons
        );
        assert!(second.reasons.is_empty(), "reasons: {:?}", second.reasons);
    }

    /// A backup age, however old, is never a reason to degrade.
    #[test]
    fn a_clean_install_is_not_severely_degraded_by_a_no_op_backup_age() {
        let (mut controller, _dir) = degraded_controller_with_prior_backup("severe-but-clean");
        let now = 1_785_000_000_000i64;
        let very_stale = now - (30 * 60 * 60 * 1_000);

        let status = controller
            .evaluate(
                PressureObservation {
                    free_bytes: FREE_WARN_BYTES * 2,
                    capacity_bytes: FREE_WARN_BYTES * 4,
                    last_backup_success_at_unix_ms: Some(very_stale),
                    backup_generation: 6,
                    unbacked_bytes: 0,
                },
                now,
            )
            .unwrap();

        assert!(
            !status.reasons.iter().any(|r| r == "backup_stale"),
            "nothing to transfer is not a stale backup, reasons: {:?}",
            status.reasons
        );
    }

    /// REGRESSION (the 2026-08-21 stub): resume used 100 GB AND 15 % as
    /// independent gates, so on a 1 TB disk 110 GB free could not resume
    /// capture even though the disk was comfortable.
    #[test]
    fn a_one_tb_disk_resumes_at_110gb_free() {
        let body_dir = std::env::temp_dir().join(format!(
            "switchback-pressure-1tb-resume-{}",
            std::process::id()
        ));
        fs::create_dir_all(&body_dir).unwrap();
        // Degraded install with a prior backup so we exercise the resume branch.
        fs::write(
            body_dir.join("pressure-state.json"),
            br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "metadata_only",
  "reasons": ["healing_backup_cycles"],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 6,
  "writer_failures": 0,
  "unbacked_bytes": 0,
  "updated_at_unix_ms": 1
}"#,
        )
        .unwrap();
        let mut controller = PressureController::load(&body_dir);
        let now = 1_785_000_000_000_i64;
        // 994 GB capacity (a 1 TB raw drive), 110 GB free: far above the
        // resume line (about 29.8 GB on this disk).
        let observation = PressureObservation {
            free_bytes: 110_000_000_000,
            capacity_bytes: 994_000_000_000,
            last_backup_success_at_unix_ms: Some(now - 60_000),
            backup_generation: 6,
            unbacked_bytes: 0,
        };

        controller.evaluate(observation, now).unwrap();
        let status = controller.evaluate(observation, now + 1).unwrap();

        assert_ne!(
            status.mode,
            CaptureMode::MetadataOnly,
            "a 1 TB-class disk with 110 GB free must not be stuck in metadata-only; \
             reasons: {:?}",
            status.reasons
        );
    }

    /// The warn line is `min(100 GB, 15 % of capacity)`: on a 1 TB disk the
    /// absolute line rules, on smaller disks the percent line rules.
    #[test]
    fn warn_free_threshold_uses_min_of_absolute_and_percent() {
        // 1 TB (994 GB formatted): 15 % = ~149 GB → min(100, 149) = 100 GB.
        assert_eq!(warn_free_threshold_bytes(994_000_000_000), FREE_WARN_BYTES);
        // 100 GB disk: 15 % = 15 GB → min(100, 15) = 15 GB.
        assert_eq!(
            warn_free_threshold_bytes(100_000_000_000),
            (FREE_WARN_BPS * 100_000_000_000) / 10_000
        );
        // 500 GB disk: 15 % = 75 GB → min(100, 75) = 75 GB.
        assert_eq!(
            warn_free_threshold_bytes(500_000_000_000),
            (FREE_WARN_BPS * 500_000_000_000) / 10_000
        );
    }
}

/// Bodies are discarded only when the disk is nearly full. A late backup or a
/// large backlog is a warning and never takes capture out of full-wire mode.
#[cfg(test)]
mod keep_bodies_tests {
    use super::*;

    /// The 995 GB disk the discard counts were measured on.
    const DISK: u64 = 994_662_584_320;
    const NOW: i64 = 1_794_000_000_000;
    const HOUR_MS: i64 = 60 * 60 * 1_000;

    fn controller(tag: &str, state: Option<&[u8]>) -> (PressureController, std::path::PathBuf) {
        let body_dir = std::env::temp_dir().join(format!(
            "switchback-pressure-keep-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&body_dir);
        fs::create_dir_all(&body_dir).unwrap();
        if let Some(state) = state {
            fs::write(body_dir.join("pressure-state.json"), state).unwrap();
        }
        (PressureController::load(&body_dir), body_dir)
    }

    fn with_fresh_backup(free_bytes: u64) -> PressureObservation {
        PressureObservation {
            free_bytes,
            capacity_bytes: DISK,
            last_backup_success_at_unix_ms: Some(NOW - HOUR_MS),
            backup_generation: 6,
            unbacked_bytes: 95_258_991,
        }
    }

    const FULL_WIRE_AFTER_SIX_BACKUPS: &[u8] = br#"{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "segmented_full_wire",
  "reasons": [],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 6,
  "writer_failures": 0,
  "unbacked_bytes": 95258991,
  "updated_at_unix_ms": 1
}"#;

    fn has(list: &[String], value: &str) -> bool {
        list.iter().any(|item| item == value)
    }

    /// 43.6 GB free is the lowest this disk reached. The old floor (8 %, 79.6 GB)
    /// discarded bodies there while capture itself used about 3 GB.
    #[test]
    fn the_lowest_measured_free_space_still_captures_full_bodies() {
        let (mut controller, dir) = controller("measured-low", None);
        let status = controller
            .evaluate(with_fresh_backup(43_600_000_000), NOW)
            .unwrap();
        assert_eq!(
            status.mode,
            CaptureMode::SegmentedFullWire,
            "reasons: {:?}",
            status.reasons
        );
        assert!(status.reasons.is_empty(), "reasons: {:?}", status.reasons);
        assert!(
            has(&status.warnings, "free_bytes"),
            "low space is still reported: {:?}",
            status.warnings
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// Capture stops below the degrade line, holds until free space clears the
    /// resume line, and then heals in two evaluations.
    #[test]
    fn capture_stops_only_near_full_and_resumes_above_the_resume_line() {
        let (mut controller, dir) = controller("near-full", None);

        let full = controller
            .evaluate(with_fresh_backup(19_000_000_000), NOW)
            .unwrap();
        assert_eq!(full.mode, CaptureMode::MetadataOnly);
        assert_eq!(full.reasons, vec!["free_bytes".to_string()]);

        let held = controller
            .evaluate(with_fresh_backup(25_000_000_000), NOW + 1)
            .unwrap();
        assert_eq!(held.mode, CaptureMode::MetadataOnly);
        assert_eq!(held.reasons, vec!["free_bytes_below_resume".to_string()]);
        assert_eq!(
            held.resume_blockers,
            vec!["free_bytes_below_resume".to_string()]
        );

        let probe = controller
            .evaluate(with_fresh_backup(31_000_000_000), NOW + 2)
            .unwrap();
        assert_eq!(probe.mode, CaptureMode::HealingProbe);
        assert_eq!(probe.reasons, vec!["healing_probe".to_string()]);
        assert!(probe.resume_blockers.is_empty());

        let resumed = controller
            .evaluate(with_fresh_backup(31_000_000_000), NOW + 3)
            .unwrap();
        assert_eq!(resumed.mode, CaptureMode::SegmentedFullWire);
        assert!(resumed.reasons.is_empty(), "reasons: {:?}", resumed.reasons);
        assert!(resumed.resume_blockers.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_backup_five_days_late_warns_and_never_discards() {
        let (mut controller, dir) = controller("late-backup", Some(FULL_WIRE_AFTER_SIX_BACKUPS));
        let late = PressureObservation {
            free_bytes: 500_000_000_000,
            capacity_bytes: DISK,
            last_backup_success_at_unix_ms: Some(NOW - 5 * 24 * HOUR_MS),
            backup_generation: 6,
            unbacked_bytes: 20_000_000_000,
        };
        for step in 0..4 {
            let status = controller.evaluate(late, NOW + step).unwrap();
            assert_eq!(
                status.mode,
                CaptureMode::SegmentedFullWire,
                "evaluation {step}: reasons {:?}",
                status.reasons
            );
            assert!(status.reasons.is_empty(), "reasons: {:?}", status.reasons);
            assert!(
                has(&status.warnings, "backup_stale"),
                "warnings: {:?}",
                status.warnings
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_receipt_after_earlier_backups_warns_and_never_discards() {
        let (mut controller, dir) =
            controller("missing-receipt", Some(FULL_WIRE_AFTER_SIX_BACKUPS));
        let missing = PressureObservation {
            free_bytes: 500_000_000_000,
            capacity_bytes: DISK,
            last_backup_success_at_unix_ms: None,
            backup_generation: 6,
            unbacked_bytes: 20_000_000_000,
        };
        for step in 0..2 {
            let status = controller.evaluate(missing, NOW + step).unwrap();
            assert_eq!(
                status.mode,
                CaptureMode::SegmentedFullWire,
                "reasons: {:?}",
                status.reasons
            );
            assert!(
                has(&status.warnings, "backup_missing"),
                "warnings: {:?}",
                status.warnings
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_free_space_lines_for_three_disk_sizes() {
        assert_eq!(degrade_free_threshold_bytes(DISK), 19_893_251_686);
        assert_eq!(resume_free_threshold_bytes(DISK), 29_839_877_529);
        assert_eq!(warn_free_threshold_bytes(DISK), 100_000_000_000);

        assert_eq!(
            degrade_free_threshold_bytes(256_000_000_000),
            10_000_000_000
        );
        assert_eq!(resume_free_threshold_bytes(256_000_000_000), 15_000_000_000);

        assert_eq!(
            degrade_free_threshold_bytes(4_000_000_000_000),
            80_000_000_000
        );
        assert_eq!(
            resume_free_threshold_bytes(4_000_000_000_000),
            120_000_000_000
        );
    }

    /// A disk guard reclaims to the warn line. If it sat below the resume line,
    /// the guard would stop reclaiming while a degraded capture still waits.
    #[test]
    fn the_warn_line_never_sits_below_the_resume_line() {
        for capacity in [
            0,
            50_000_000_000,
            100_000_000_000,
            256_000_000_000,
            DISK,
            4_000_000_000_000,
            8_000_000_000_000,
        ] {
            let warn = warn_free_threshold_bytes(capacity);
            let degrade = degrade_free_threshold_bytes(capacity);
            let resume = resume_free_threshold_bytes(capacity);
            assert!(degrade >= FREE_DEGRADE_BYTES, "capacity {capacity}");
            assert!(resume > degrade, "capacity {capacity}");
            assert!(warn >= resume, "capacity {capacity}: {warn} < {resume}");
        }
        assert_eq!(
            warn_free_threshold_bytes(4_000_000_000_000),
            120_000_000_000
        );
    }

    #[test]
    fn the_status_publishes_the_resolved_lines() {
        let (mut controller, dir) = controller("published-lines", None);
        assert_eq!(controller.status().free_degrade_line_bytes, None);
        let status = controller
            .evaluate(with_fresh_backup(500_000_000_000), NOW)
            .unwrap();
        assert_eq!(status.free_warn_line_bytes, Some(100_000_000_000));
        assert_eq!(status.free_degrade_line_bytes, Some(19_893_251_686));
        assert_eq!(status.free_resume_line_bytes, Some(29_839_877_529));
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["free_resume_line_bytes"], 29_839_877_529_u64);

        let small = controller
            .evaluate(
                PressureObservation {
                    capacity_bytes: 256_000_000_000,
                    ..with_fresh_backup(200_000_000_000)
                },
                NOW + 1,
            )
            .unwrap();
        assert_eq!(small.free_degrade_line_bytes, Some(10_000_000_000));
        assert_eq!(small.free_resume_line_bytes, Some(15_000_000_000));
        let _ = fs::remove_dir_all(dir);
    }

    /// State files written by an older engine carry reason strings this engine
    /// no longer produces. They must still load and heal.
    #[test]
    fn an_old_state_with_retired_reasons_loads_and_heals() {
        for (tag, reason) in [
            ("old-cycles", "healing_backup_cycles"),
            ("old-probe", "healing_backup_probe"),
        ] {
            let state = format!(
                r#"{{
  "schema": "switchback/capture-pressure-state@1",
  "mode": "metadata_only",
  "reasons": ["{reason}"],
  "resume_blockers": ["backup_stale_for_resume", "unbacked_bytes_above_resume"],
  "healthy_backup_cycles": 0,
  "last_backup_generation": 6,
  "writer_failures": 0,
  "unbacked_bytes": 20000000000,
  "updated_at_unix_ms": 1
}}"#
            );
            let (mut controller, dir) = controller(tag, Some(state.as_bytes()));
            let loaded = controller.status();
            assert_eq!(loaded.mode, CaptureMode::MetadataOnly);
            assert_eq!(loaded.reasons, vec![reason.to_string()]);
            let probe = controller
                .evaluate(with_fresh_backup(500_000_000_000), NOW)
                .unwrap();
            assert_eq!(probe.mode, CaptureMode::HealingProbe);
            let resumed = controller
                .evaluate(with_fresh_backup(500_000_000_000), NOW + 1)
                .unwrap();
            assert_eq!(resumed.mode, CaptureMode::SegmentedFullWire);
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn a_large_backlog_warns_and_never_discards() {
        let (mut controller, dir) = controller("large-backlog", Some(FULL_WIRE_AFTER_SIX_BACKUPS));
        let backlog = PressureObservation {
            unbacked_bytes: 60_000_000_000,
            ..with_fresh_backup(500_000_000_000)
        };
        let status = controller.evaluate(backlog, NOW).unwrap();
        assert_eq!(
            status.mode,
            CaptureMode::SegmentedFullWire,
            "reasons: {:?}",
            status.reasons
        );
        assert!(status.reasons.is_empty(), "reasons: {:?}", status.reasons);
        assert!(
            has(&status.warnings, "unbacked_bytes"),
            "warnings: {:?}",
            status.warnings
        );
        let _ = fs::remove_dir_all(dir);
    }
}
