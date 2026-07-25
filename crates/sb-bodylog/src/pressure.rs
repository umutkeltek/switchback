use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use super::{BodyLogError, Result};

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
    pub unbacked_bytes: u64,
    pub free_bytes: Option<u64>,
    pub capacity_bytes: Option<u64>,
    pub last_backup_success_at_unix_ms: Option<i64>,
    pub updated_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedPressureState {
    schema: String,
    mode: CaptureMode,
    reasons: Vec<String>,
    healthy_backup_cycles: u32,
    last_backup_generation: u64,
    writer_failures: u64,
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
            unbacked_bytes: 0,
            updated_at_unix_ms: 0,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PressureController {
    path: PathBuf,
    state: PersistedPressureState,
    latest: PressureStatus,
    bytes_since_snapshot: u64,
    last_persisted_at_unix_ms: i64,
}

impl PressureController {
    pub(crate) fn load(body_dir: &Path) -> Self {
        let path = body_dir.join("pressure-state.json");
        let state = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedPressureState>(&bytes) {
                Ok(state) if state.schema == PRESSURE_STATE_SCHEMA => state,
                Ok(_) | Err(_) => PersistedPressureState {
                    mode: CaptureMode::MetadataOnly,
                    reasons: vec!["pressure_state_invalid".to_string()],
                    ..PersistedPressureState::default()
                },
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                PersistedPressureState::default()
            }
            Err(_) => PersistedPressureState {
                mode: CaptureMode::MetadataOnly,
                reasons: vec!["pressure_state_unreadable".to_string()],
                ..PersistedPressureState::default()
            },
        };
        let latest = status_from_state(&state);
        Self {
            path,
            last_persisted_at_unix_ms: state.updated_at_unix_ms,
            state,
            latest,
            bytes_since_snapshot: 0,
        }
    }

    pub(crate) fn status(&self) -> PressureStatus {
        self.latest.clone()
    }

    pub(crate) fn observe(&self, state_dir: &Path) -> Result<PressureObservation> {
        let (free_bytes, capacity_bytes) = filesystem_capacity(state_dir)?;
        let receipt = read_latest_backup_receipt(&backup_receipt_path(state_dir))?;
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
            unbacked_bytes: if backup_generation > self.state.last_backup_generation {
                0
            } else {
                self.state.unbacked_bytes
            },
        })
    }

    pub(crate) fn evaluate(
        &mut self,
        observation: PressureObservation,
        now_unix_ms: i64,
    ) -> Result<PressureStatus> {
        let previous = self.state.clone();
        let free_bps = ratio_bps(observation.free_bytes, observation.capacity_bytes);
        let backup_age_ms = observation
            .last_backup_success_at_unix_ms
            .map(|completed| now_unix_ms.saturating_sub(completed).max(0));

        let mut severe = Vec::new();
        if backup_age_ms.map_or(true, |age| age > BACKUP_DEGRADE_AGE_MS) {
            severe.push("backup_stale".to_string());
        }
        if observation.free_bytes < FREE_DEGRADE_BYTES || free_bps < FREE_DEGRADE_BPS {
            severe.push("free_bytes".to_string());
        }
        if observation.unbacked_bytes > UNBACKED_DEGRADE_BYTES {
            severe.push("unbacked_bytes".to_string());
        }

        let mut warnings = Vec::new();
        if backup_age_ms.map_or(true, |age| age > BACKUP_WARN_AGE_MS) {
            warnings.push("backup_stale".to_string());
        }
        if observation.free_bytes < FREE_WARN_BYTES || free_bps < FREE_WARN_BPS {
            warnings.push("free_bytes".to_string());
        }

        if !severe.is_empty() {
            self.state.mode = CaptureMode::MetadataOnly;
            self.state.reasons = severe;
            self.state.healthy_backup_cycles = 0;
        } else if self.state.mode == CaptureMode::MetadataOnly {
            let healthy_for_resume = backup_age_ms.is_some_and(|age| age <= BACKUP_WARN_AGE_MS)
                && observation.free_bytes >= FREE_WARN_BYTES
                && free_bps >= FREE_WARN_BPS
                && observation.unbacked_bytes < UNBACKED_RESUME_BYTES;
            if healthy_for_resume
                && observation.backup_generation > self.state.last_backup_generation
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
            } else {
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
            unbacked_bytes: observation.unbacked_bytes,
            free_bytes: Some(observation.free_bytes),
            capacity_bytes: Some(observation.capacity_bytes),
            last_backup_success_at_unix_ms: observation.last_backup_success_at_unix_ms,
            updated_at_unix_ms: now_unix_ms,
        };
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
        self.state.mode = CaptureMode::MetadataOnly;
        self.state.reasons = vec![format!("writer_failed:{reason}")];
        self.state.healthy_backup_cycles = 0;
        self.state.writer_failures = self.state.writer_failures.saturating_add(1);
        self.state.updated_at_unix_ms = now_unix_ms;
        self.latest = status_from_state(&self.state);
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

    pub(crate) fn set_metadata_only_events(&mut self, events: u64) {
        self.latest.metadata_only_events = events;
    }

    fn persist(&self) -> Result<()> {
        atomic_write_private(&self.path, &serde_json::to_vec_pretty(&self.state)?)
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
fn filesystem_capacity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt as _;

    let path = CString::new(path.as_os_str().as_bytes())
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
        (stats.f_bavail as u64).saturating_mul(block_size),
        (stats.f_blocks as u64).saturating_mul(block_size),
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
        unbacked_bytes: state.unbacked_bytes,
        free_bytes: None,
        capacity_bytes: None,
        last_backup_success_at_unix_ms: None,
        updated_at_unix_ms: state.updated_at_unix_ms,
    }
}

fn persisted_changed(before: &PersistedPressureState, after: &PersistedPressureState) -> bool {
    before.mode != after.mode
        || before.reasons != after.reasons
        || before.healthy_backup_cycles != after.healthy_backup_cycles
        || before.last_backup_generation != after.last_backup_generation
        || before.writer_failures != after.writer_failures
        || before.unbacked_bytes != after.unbacked_bytes
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| BodyLogError::new("pressure state has no parent directory"))?;
    fs::create_dir_all(parent)?;
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
