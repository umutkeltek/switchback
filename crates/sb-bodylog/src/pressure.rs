use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{archive_root_available, BodyLogError, Result};

pub const PRESSURE_STATE_SCHEMA: &str = "switchback/capture-pressure-state@1";
pub const CAPTURE_GAP_SCHEMA: &str = "switchback/capture-gap@1";
pub const BACKUP_RECEIPT_SCHEMA: &str = "switchback/capture-backup@2";

const BACKUP_WARN_AGE_MS: i64 = 6 * 60 * 60 * 1_000;
const BACKUP_DEGRADE_AGE_MS: i64 = 24 * 60 * 60 * 1_000;
const FREE_WARN_BYTES: u64 = 100_000_000_000;
const FREE_DEGRADE_BYTES: u64 = 50_000_000_000;
const FREE_WARN_BPS: u64 = 1_500;
const FREE_DEGRADE_BPS: u64 = 800;
const UNBACKED_DEGRADE_BYTES: u64 = 50_000_000_000;
const UNBACKED_RESUME_BYTES: u64 = 10_000_000_000;
const HEALTHY_BACKUP_CYCLES_TO_RESUME: u32 = 2;
const UNBACKED_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const UNBACKED_SNAPSHOT_INTERVAL_MS: i64 = 60 * 1_000;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    #[default]
    SegmentedFullWire,
    /// One healthy, non-empty backup generation has landed after degradation.
    /// Full-wire capture is admitted so the second required generation can be
    /// produced, while the controller remains visibly in recovery.
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
    /// Below this, capture degrades to metadata-only. The safety floor.
    pub degrade_free_bytes: u64,
    pub degrade_free_bps: u64,
    /// A degraded capture may only resume ABOVE this. Reclaim must target it, not `degrade`:
    /// clearing the floor alone leaves capture permanently stuck.
    pub resume_free_bytes: u64,
    pub resume_free_bps: u64,
    /// Consecutive healthy backup cycles required before full-wire capture resumes.
    pub healthy_backup_cycles_to_resume: u32,
}

impl PressureThresholds {
    pub fn current() -> Self {
        Self {
            degrade_free_bytes: FREE_DEGRADE_BYTES,
            degrade_free_bps: FREE_DEGRADE_BPS,
            resume_free_bytes: FREE_WARN_BYTES,
            resume_free_bps: FREE_WARN_BPS,
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
    pub healthy_backup_cycles: u32,
    pub last_backup_generation: u64,
    pub writer_failures: u64,
    pub metadata_only_events: u64,
    pub queue_depth: u64,
    pub queue_drops: u64,
    pub queue_high_watermark: u64,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedPressureState {
    schema: String,
    mode: CaptureMode,
    reasons: Vec<String>,
    healthy_backup_cycles: u32,
    last_backup_generation: u64,
    writer_failures: u64,
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
            healthy_backup_cycles: 0,
            last_backup_generation: 0,
            writer_failures: 0,
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
    bytes_since_snapshot: u64,
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
            bytes_since_snapshot: 0,
            latest_verified_through_day: None,
            latest_limiting_filesystem: None,
            local_queue_depth: 0,
            local_queue_high_watermark: 0,
        }
    }

    pub(crate) fn status(&self) -> PressureStatus {
        self.latest.clone()
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
        let free_bps = ratio_bps(observation.free_bytes, observation.capacity_bytes);
        let backup_age_ms = observation
            .last_backup_success_at_unix_ms
            .map(|completed| now_unix_ms.saturating_sub(completed).max(0));

        let mut severe = Vec::new();
        if backup_age_ms.is_some_and(|age| age > BACKUP_DEGRADE_AGE_MS)
            || (backup_age_ms.is_none() && self.state.last_backup_generation > 0)
        {
            severe.push("backup_stale".to_string());
        }
        if observation.free_bytes < FREE_DEGRADE_BYTES || free_bps < FREE_DEGRADE_BPS {
            severe.push("free_bytes".to_string());
        }
        if observation.unbacked_bytes > UNBACKED_DEGRADE_BYTES {
            severe.push("unbacked_bytes".to_string());
        }

        let mut warnings = Vec::new();
        if backup_age_ms.is_some_and(|age| age > BACKUP_WARN_AGE_MS) {
            warnings.push("backup_stale".to_string());
        } else if backup_age_ms.is_none() {
            warnings.push("backup_missing".to_string());
        }
        if observation.free_bytes < FREE_WARN_BYTES || free_bps < FREE_WARN_BPS {
            warnings.push("free_bytes".to_string());
        }

        if !severe.is_empty() {
            self.state.mode = CaptureMode::MetadataOnly;
            self.state.reasons = severe;
            self.state.healthy_backup_cycles = 0;
        } else if backup_age_ms.is_none() && observation.backup_generation == 0 {
            // Nothing has ever been backed up here, so there is no degraded
            // backup to re-earn trust from. The healing path below cannot serve
            // this state: it waits for a backup generation to advance, a backup
            // cycle needs sealed segments, and metadata-only capture writes no
            // segments — so waiting strands a fresh or freshly migrated install
            // in metadata-only permanently, silently dropping every body.
            // `severe` above still owns real pressure (free space, unbacked
            // bytes, and a backup that existed and then went stale — which is
            // why that check requires `last_backup_generation > 0`). Draw the
            // same line here: never-backed-up is not a stale backup.
            self.state.mode = CaptureMode::SegmentedFullWire;
            self.state.reasons.clear();
            self.state.healthy_backup_cycles = 0;
        } else if matches!(
            self.state.mode,
            CaptureMode::MetadataOnly | CaptureMode::HealingProbe
        ) {
            let healthy_for_resume = backup_age_ms.is_some_and(|age| age <= BACKUP_WARN_AGE_MS)
                && observation.free_bytes >= FREE_WARN_BYTES
                && free_bps >= FREE_WARN_BPS
                && observation.unbacked_bytes < UNBACKED_RESUME_BYTES;
            // A backup cycle with NOTHING to transfer is still a HEALTHY cycle. Requiring the
            // generation to advance conflates "the backup made progress" with "the backup is
            // healthy", and that conflation is a deadlock: resuming needs a generation bump, a
            // bump needs sealed segments, and metadata-only capture writes none. `435872e`
            // fixed this for a never-backed-up install (`backup_generation == 0`); an install
            // that HAD backed up fell straight through it.
            //
            // Observed live 2026-07-26: free space healthy (17.3%), backup fresh, generation 4,
            // unbacked_bytes 0 — and `healthy_backup_cycles` pinned at 0 while every request
            // payload was discarded. A manually triggered cycle returned
            // `accepted:false no_op:true transferred_segments:0`, so it wrote no receipt and
            // the generation the controller reads never moved.
            //
            // `unbacked_bytes == 0` is the honest signal here: there is no un-backed-up data,
            // so there is nothing for a transfer to prove.
            let nothing_left_to_back_up = observation.unbacked_bytes == 0;
            if healthy_for_resume
                && (observation.backup_generation > self.state.last_backup_generation
                    || nothing_left_to_back_up)
            {
                self.state.healthy_backup_cycles =
                    self.state.healthy_backup_cycles.saturating_add(1);
            } else if !healthy_for_resume {
                self.state.healthy_backup_cycles = 0;
            }

            if self.state.healthy_backup_cycles >= HEALTHY_BACKUP_CYCLES_TO_RESUME {
                self.state.mode = CaptureMode::SegmentedFullWire;
                self.state.reasons.clear();
                self.state.healthy_backup_cycles = 0;
            } else if healthy_for_resume && self.state.healthy_backup_cycles == 1 {
                self.state.mode = CaptureMode::HealingProbe;
                self.state.reasons = vec!["healing_backup_probe".to_string()];
            } else {
                self.state.mode = CaptureMode::MetadataOnly;
                self.state.reasons = vec!["healing_backup_cycles".to_string()];
            }
        } else {
            self.state.reasons.clear();
            self.state.healthy_backup_cycles = 0;
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
            healthy_backup_cycles: self.state.healthy_backup_cycles,
            last_backup_generation: self.state.last_backup_generation,
            writer_failures: self.state.writer_failures,
            metadata_only_events: self.latest.metadata_only_events,
            queue_depth: self.local_queue_depth,
            queue_drops: self.state.queue_drops,
            queue_high_watermark: self.local_queue_high_watermark,
            unbacked_bytes: observation.unbacked_bytes,
            free_bytes: Some(observation.free_bytes),
            capacity_bytes: Some(observation.capacity_bytes),
            limiting_filesystem: self.latest_limiting_filesystem.clone(),
            last_backup_success_at_unix_ms: observation.last_backup_success_at_unix_ms,
            backup_age_ms,
            verified_through_day: self.latest_verified_through_day.clone(),
            updated_at_unix_ms: now_unix_ms,
            thresholds: PressureThresholds::current(),
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
        let snapshot_due = self.bytes_since_snapshot > 0
            && (self.bytes_since_snapshot >= UNBACKED_SNAPSHOT_BYTES
                || now_unix_ms.saturating_sub(self.last_persisted_at_unix_ms)
                    >= UNBACKED_SNAPSHOT_INTERVAL_MS);
        if persisted_changed(&previous, &self.state) || snapshot_due {
            self.persist()?;
            self.bytes_since_snapshot = 0;
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
        self.state.mode = CaptureMode::MetadataOnly;
        self.state.reasons = vec![format!("writer_failed:{reason}")];
        self.state.healthy_backup_cycles = 0;
        self.state.writer_failures = self.state.writer_failures.saturating_add(1);
        self.state.updated_at_unix_ms = now_unix_ms;
        self.latest = self.status_from_shared_state();
        self.persist()?;
        self.bytes_since_snapshot = 0;
        self.last_persisted_at_unix_ms = now_unix_ms;
        Ok(self.latest.clone())
    }

    pub(crate) fn note_full_capture(&mut self, body_bytes: u64) {
        self.state.unbacked_bytes = self.state.unbacked_bytes.saturating_add(body_bytes);
        self.latest.unbacked_bytes = self.state.unbacked_bytes;
        self.bytes_since_snapshot = self.bytes_since_snapshot.saturating_add(body_bytes);
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
        self.bytes_since_snapshot = 0;
        self.last_persisted_at_unix_ms = now_unix_ms;
        Ok(())
    }

    pub(crate) fn set_metadata_only_events(&mut self, events: u64) {
        self.latest.metadata_only_events = events;
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

fn ratio_bps(free_bytes: u64, capacity_bytes: u64) -> u64 {
    if capacity_bytes == 0 {
        return 0;
    }
    free_bytes.saturating_mul(10_000) / capacity_bytes
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
        healthy_backup_cycles: state.healthy_backup_cycles,
        last_backup_generation: state.last_backup_generation,
        writer_failures: state.writer_failures,
        metadata_only_events: 0,
        queue_depth: state.queue_depth,
        queue_drops: state.queue_drops,
        queue_high_watermark: state.queue_high_watermark,
        unbacked_bytes: state.unbacked_bytes,
        free_bytes: None,
        capacity_bytes: None,
        limiting_filesystem: None,
        last_backup_success_at_unix_ms: None,
        backup_age_ms: None,
        verified_through_day: None,
        updated_at_unix_ms: state.updated_at_unix_ms,
        thresholds: PressureThresholds::current(),
    }
}

fn persisted_changed(before: &PersistedPressureState, after: &PersistedPressureState) -> bool {
    before.mode != after.mode
        || before.reasons != after.reasons
        || before.healthy_backup_cycles != after.healthy_backup_cycles
        || before.last_backup_generation != after.last_backup_generation
        || before.writer_failures != after.writer_failures
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
    let free_bps = ratio_bps(free_bytes, capacity_bytes);
    let severity = if free_bytes < FREE_DEGRADE_BYTES || free_bps < FREE_DEGRADE_BPS {
        2
    } else if free_bytes < FREE_WARN_BYTES || free_bps < FREE_WARN_BPS {
        1
    } else {
        0
    };
    let (free_threshold, ratio_threshold) = if severity == 2 {
        (FREE_DEGRADE_BYTES, FREE_DEGRADE_BPS)
    } else {
        (FREE_WARN_BYTES, FREE_WARN_BPS)
    };
    let free_margin = free_bytes
        .saturating_mul(10_000)
        .checked_div(free_threshold)
        .unwrap_or(u64::MAX);
    let ratio_margin = free_bps
        .saturating_mul(10_000)
        .checked_div(ratio_threshold)
        .unwrap_or(u64::MAX);
    (
        severity,
        10_000u64.saturating_sub(free_margin.min(ratio_margin)),
    )
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

        let status = controller
            .evaluate(
                PressureObservation {
                    free_bytes: FREE_WARN_BYTES * 2,
                    capacity_bytes: FREE_WARN_BYTES * 4,
                    last_backup_success_at_unix_ms: None,
                    backup_generation: 0,
                    unbacked_bytes: 0,
                },
                1_785_000_000_000,
            )
            .unwrap();

        // Waiting for a healthy backup cycle here never terminates: a cycle needs
        // sealed segments, and metadata-only capture writes none. Disk and
        // unbacked-byte pressure still degrade capture on their own.
        assert_eq!(
            status.mode,
            CaptureMode::SegmentedFullWire,
            "reasons: {:?}",
            status.reasons
        );
        assert!(status.reasons.is_empty(), "reasons: {:?}", status.reasons);
    }

    /// REGRESSION (live outage 2026-07-26): an install that HAS backed up before could never
    /// leave metadata-only. Resuming required the backup generation to advance; advancing it
    /// required sealed segments; metadata-only writes none. `435872e` broke that loop only for
    /// `backup_generation == 0`, so an already-backed-up install stayed deadlocked — discarding
    /// every request payload for hours with free space and backups both healthy.
    #[test]
    fn an_already_backed_up_install_heals_when_there_is_nothing_left_to_back_up() {
        let body_dir = std::env::temp_dir().join(format!(
            "switchback-pressure-healed-{}",
            std::process::id()
        ));
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

        let mut status = controller.evaluate(observation.clone(), now).unwrap();
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

    /// The escape hatch must not become a bypass: with data still un-backed-up, a static
    /// generation means the backup genuinely is not keeping up, and capture must stay degraded.
    #[test]
    fn unbacked_data_still_blocks_healing_when_the_generation_is_static() {
        let body_dir = std::env::temp_dir().join(format!(
            "switchback-pressure-unbacked-{}",
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
            // Below UNBACKED_RESUME_BYTES so `healthy_for_resume` still holds, but NOT zero:
            // there is real data a transfer would have to prove it moved.
            unbacked_bytes: 1_000_000_000,
        };

        let mut status = controller.evaluate(observation.clone(), now).unwrap();
        status = controller.evaluate(observation, now + 1).unwrap();
        assert_eq!(
            status.mode,
            CaptureMode::MetadataOnly,
            "un-backed-up data with a static generation must NOT heal; reasons: {:?}",
            status.reasons
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
        assert_eq!(t.resume_free_bytes, FREE_WARN_BYTES);
        assert_eq!(t.resume_free_bps, FREE_WARN_BPS);
        assert_eq!(
            t.healthy_backup_cycles_to_resume,
            HEALTHY_BACKUP_CYCLES_TO_RESUME
        );
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
    }
}

