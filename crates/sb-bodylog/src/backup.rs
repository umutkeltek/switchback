use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    begin_write_transaction, day_floor_ms, format_day_ms, now_unix_ms, open_index_connection,
    open_index_connection_for_maintenance, pressure, read_verified_segment_manifest,
    segment_manifest_path, sha256_hex, sync_directory, BodyLogError, BodyLogger, Result,
    SegmentManifest,
};

pub const BACKUP_PLAN_SCHEMA: &str = "switchback/capture-backup-plan@1";
pub const LEGACY_BACKUP_PLAN_SCHEMA: &str = "switchback/capture-legacy-backup-plan@1";
pub const LEGACY_BACKUP_RECEIPT_SCHEMA: &str = "switchback/capture-legacy-backup@1";

static NEXT_BACKUP_TEMP_ID: AtomicU64 = AtomicU64::new(1);
const MAX_RECEIPT_FUTURE_SKEW_MS: i64 = 5 * 60 * 1_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureBackupPlan {
    pub schema: String,
    pub next_generation: u64,
    pub segments: Vec<CaptureBackupPlanItem>,
    pub total_segment_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureBackupPlanItem {
    pub segment_file: String,
    pub segment_path: String,
    pub manifest_path: String,
    pub segment_sha256: String,
    pub manifest_sha256: String,
    pub segment_bytes: u64,
    pub record_count: u64,
    pub first_observed_at_unix_ms: i64,
    pub last_observed_at_unix_ms: i64,
    pub utc_day: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureBackupReceipt {
    pub schema: String,
    pub generation: u64,
    pub completed_at_unix_ms: i64,
    pub verified_through_day: Option<String>,
    pub remote_root: String,
    pub segments: Vec<CaptureBackupReceiptItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureBackupReceiptItem {
    pub segment_sha256: String,
    pub manifest_sha256: String,
    pub remote_path: String,
    pub remote_manifest_path: String,
    pub remote_checksum_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureLegacyBackupPlan {
    pub schema: String,
    pub artifacts: Vec<CaptureLegacyBackupPlanItem>,
    pub total_artifact_bytes: u64,
    #[serde(default)]
    pub blockers: Vec<CaptureLegacyBackupBlocker>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureLegacyBackupPlanItem {
    pub artifact_id: String,
    pub kind: String,
    pub file_name: String,
    pub local_path: String,
    pub sha256: String,
    pub bytes: u64,
    pub modified_at_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureLegacyBackupBlocker {
    pub code: String,
    pub artifact_id: String,
    pub local_path: String,
    #[serde(default)]
    pub remediation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureLegacyBackupReceipt {
    pub schema: String,
    pub completed_at_unix_ms: i64,
    pub remote_root: String,
    pub artifacts: Vec<CaptureLegacyBackupReceiptItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureLegacyBackupReceiptItem {
    pub artifact_id: String,
    pub kind: String,
    pub local_path: String,
    pub sha256: String,
    pub bytes: u64,
    pub modified_at_unix_ms: i64,
    pub remote_path: String,
    pub remote_checksum_verified: bool,
}

#[derive(Debug)]
struct SegmentProjection {
    path: PathBuf,
    segment_sha256: Option<String>,
    body_bytes: u64,
    sealed: bool,
}

pub(super) struct BackupOperationGuard(fs::File);

impl Drop for BackupOperationGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            // Best effort during Drop; closing the descriptor also releases it.
            let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

pub(super) fn backup_operation_lock(state_dir: &Path) -> Result<BackupOperationGuard> {
    let directory = backup_dir(state_dir);
    ensure_private_dir(&directory)?;
    let path = directory.join("operation.lock");
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(BackupOperationGuard(file))
}

impl BodyLogger {
    /// Build a transfer plan from the segment projection and sealed manifests.
    /// It never walks the archive tree and never includes the disposable index.
    pub fn backup_plan(&self) -> Result<CaptureBackupPlan> {
        let _operation = backup_operation_lock(&self.config.state_dir)?;
        self.backup_plan_unlocked()
    }

    fn backup_plan_unlocked(&self) -> Result<CaptureBackupPlan> {
        let conn = open_index_connection(&self.index_path)?;
        let (verified, latest_generation) = if self.uses_current_index() {
            verified_backup_projection_state(&conn)?
        } else {
            verified_receipt_state(&backup_dir(&self.config.state_dir))?
        };
        let projections = verified_segment_projection(&conn, &verified, self.uses_current_index())?;
        let mut segments = Vec::new();
        let mut total_segment_bytes = 0u64;
        let mut planned_hashes = HashSet::new();

        for (projection, manifest) in projections {
            let Some(manifest) = manifest else {
                continue;
            };
            if verified.contains(&manifest.segment_sha256) {
                continue;
            }
            if !planned_hashes.insert(manifest.segment_sha256.clone()) {
                return Err(BodyLogError::new(format!(
                    "sealed segment checksum is duplicated in the projection: {}",
                    manifest.segment_sha256
                )));
            }
            let Some(first_observed_at_unix_ms) = manifest.first_observed_at_unix_ms else {
                return Err(BodyLogError::new(format!(
                    "sealed segment manifest has no first record timestamp: {}",
                    projection.path.display()
                )));
            };
            let segment_file = projection
                .path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_string();
            if segment_file.is_empty() {
                return Err(BodyLogError::new(format!(
                    "sealed segment path has no file name: {}",
                    projection.path.display()
                )));
            }
            total_segment_bytes = total_segment_bytes.saturating_add(manifest.segment_bytes);
            let manifest_path = segment_manifest_path(&projection.path);
            let manifest_sha256 = sha256_hex(&fs::read(&manifest_path)?);
            let last_observed_at_unix_ms = manifest.last_observed_at_unix_ms.ok_or_else(|| {
                BodyLogError::new(format!(
                    "sealed segment manifest has no last record timestamp: {}",
                    projection.path.display()
                ))
            })?;
            segments.push(CaptureBackupPlanItem {
                segment_file,
                segment_path: projection.path.to_string_lossy().into_owned(),
                manifest_path: manifest_path.to_string_lossy().into_owned(),
                segment_sha256: manifest.segment_sha256,
                manifest_sha256,
                segment_bytes: manifest.segment_bytes,
                record_count: manifest.record_count,
                first_observed_at_unix_ms,
                last_observed_at_unix_ms,
                utc_day: format_day_ms(day_floor_ms(first_observed_at_unix_ms)),
            });
        }
        segments.sort_by(|left, right| {
            left.utc_day
                .cmp(&right.utc_day)
                .then_with(|| left.segment_path.cmp(&right.segment_path))
        });
        Ok(CaptureBackupPlan {
            schema: BACKUP_PLAN_SCHEMA.to_string(),
            next_generation: latest_generation.saturating_add(1),
            segments,
            total_segment_bytes,
        })
    }

    /// Build the one-time checksum plan for frozen pre-segment evidence.
    ///
    /// This is deliberately separate from segment backup generations so
    /// copying legacy artifacts cannot count as a healthy capture generation
    /// or influence pressure-healing state.
    pub fn legacy_backup_plan(&self) -> Result<CaptureLegacyBackupPlan> {
        let _operation = backup_operation_lock(&self.config.state_dir)?;
        self.legacy_backup_plan_unlocked()
    }

    fn legacy_backup_plan_unlocked(&self) -> Result<CaptureLegacyBackupPlan> {
        let verified = verified_legacy_artifacts(&self.config.state_dir)?;
        let cached = read_cached_legacy_plan(&self.config.state_dir)?;
        let mut artifacts = Vec::new();
        let mut total_artifact_bytes = 0u64;
        let (candidates, mut blockers) = self.legacy_artifact_candidates()?;
        for candidate in candidates {
            let directory_fingerprint = if candidate.kind == "directory" {
                Some(legacy_directory_fingerprint(&candidate.path)?)
            } else {
                None
            };
            let before = match &directory_fingerprint {
                Some((stat, _)) => *stat,
                None => legacy_artifact_stat(&candidate.path)?,
            };
            let local_path = candidate.path.to_string_lossy().into_owned();
            if verified.iter().any(|item| {
                item.artifact_id == candidate.artifact_id
                    && item.kind == candidate.kind
                    && item.local_path == local_path
                    && item.bytes == before.bytes
                    && item.modified_at_unix_ms == before.modified_at_unix_ms
                    && directory_fingerprint
                        .as_ref()
                        .map_or(true, |(_, sha256)| item.sha256 == *sha256)
            }) {
                continue;
            }
            let cached_item = cached.as_ref().and_then(|plan| {
                plan.artifacts.iter().find(|item| {
                    item.artifact_id == candidate.artifact_id
                        && item.kind == candidate.kind
                        && item.local_path == local_path
                        && item.bytes == before.bytes
                        && item.modified_at_unix_ms == before.modified_at_unix_ms
                })
            });
            let sha256 = match (directory_fingerprint, cached_item) {
                (Some((_, sha256)), _) => sha256,
                (None, Some(item)) => item.sha256.clone(),
                (None, None) => sha256_file_stable(&candidate.path, before)?,
            };
            total_artifact_bytes = total_artifact_bytes.saturating_add(before.bytes);
            artifacts.push(CaptureLegacyBackupPlanItem {
                artifact_id: candidate.artifact_id,
                kind: candidate.kind,
                file_name: candidate
                    .path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| BodyLogError::new("legacy artifact has no UTF-8 file name"))?
                    .to_string(),
                local_path,
                sha256,
                bytes: before.bytes,
                modified_at_unix_ms: before.modified_at_unix_ms,
            });
        }
        artifacts.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        blockers.sort_by(|left, right| left.artifact_id.cmp(&right.artifact_id));
        let plan = CaptureLegacyBackupPlan {
            schema: LEGACY_BACKUP_PLAN_SCHEMA.to_string(),
            artifacts,
            total_artifact_bytes,
            blockers,
        };
        atomic_write_private(
            &legacy_backup_dir(&self.config.state_dir).join("pending-plan.json"),
            &serde_json::to_vec_pretty(&plan)?,
        )?;
        Ok(plan)
    }

    /// Accept remote truth for frozen legacy evidence after exact checksum
    /// verification. Acceptance records proof only; it never deletes or
    /// proposes deletion of the local artifacts.
    pub fn accept_legacy_backup_receipt(&self, receipt: CaptureLegacyBackupReceipt) -> Result<()> {
        let _operation = backup_operation_lock(&self.config.state_dir)?;
        if receipt.schema != LEGACY_BACKUP_RECEIPT_SCHEMA {
            return Err(BodyLogError::new(format!(
                "unsupported legacy backup receipt schema {}",
                receipt.schema
            )));
        }
        validate_receipt_completion_time(receipt.completed_at_unix_ms, "legacy backup receipt")?;
        validate_remote_root(&receipt.remote_root)?;
        if receipt.artifacts.is_empty() {
            return Err(BodyLogError::new(
                "legacy backup receipt contains no artifacts",
            ));
        }
        if receipt
            .artifacts
            .iter()
            .any(|artifact| !artifact.remote_checksum_verified)
        {
            return Err(BodyLogError::new(
                "legacy backup receipt lacks remote checksum proof",
            ));
        }
        let mut receipt_ids = HashSet::with_capacity(receipt.artifacts.len());
        for artifact in &receipt.artifacts {
            if !receipt_ids.insert(artifact.artifact_id.clone()) {
                return Err(BodyLogError::new(format!(
                    "legacy backup receipt duplicates artifact {}",
                    artifact.artifact_id
                )));
            }
            validate_legacy_remote_relative_path(
                &artifact.remote_path,
                "legacy backup receipt remote path",
            )?;
        }

        let plan = self.legacy_backup_plan_unlocked()?;
        let pending_ids = plan
            .artifacts
            .iter()
            .map(|artifact| artifact.artifact_id.clone())
            .collect::<HashSet<_>>();
        if receipt_ids != pending_ids {
            return Err(BodyLogError::new(
                "legacy backup receipt must cover the exact pending artifact set",
            ));
        }
        for artifact in &receipt.artifacts {
            let planned = plan
                .artifacts
                .iter()
                .find(|planned| planned.artifact_id == artifact.artifact_id)
                .ok_or_else(|| {
                    BodyLogError::new(format!(
                        "legacy backup receipt artifact {} is not pending",
                        artifact.artifact_id
                    ))
                })?;
            if artifact.kind != planned.kind
                || artifact.local_path != planned.local_path
                || artifact.sha256 != planned.sha256
                || artifact.bytes != planned.bytes
                || artifact.modified_at_unix_ms != planned.modified_at_unix_ms
            {
                return Err(BodyLogError::new(format!(
                    "legacy backup receipt does not match planned artifact {}",
                    artifact.artifact_id
                )));
            }
            let expected_remote_path = format!(
                "legacy/{}/{}/{}",
                planned.artifact_id, planned.sha256, planned.file_name
            );
            if artifact.remote_path != expected_remote_path {
                return Err(BodyLogError::new(format!(
                    "legacy backup receipt remote path does not match planned artifact {}",
                    artifact.artifact_id
                )));
            }
        }

        let bytes = serde_json::to_vec_pretty(&receipt)?;
        let receipt_digest = sha256_hex(&bytes);
        let receipt_path = legacy_backup_dir(&self.config.state_dir)
            .join("receipts")
            .join(format!(
                "{:020}-{}.json",
                receipt.completed_at_unix_ms,
                &receipt_digest[..12]
            ));
        atomic_write_private(&receipt_path, &bytes)?;
        atomic_write_private(
            &legacy_backup_dir(&self.config.state_dir).join("latest-receipt.json"),
            &bytes,
        )
    }

    fn legacy_artifact_candidates(
        &self,
    ) -> Result<(
        Vec<LegacyArtifactCandidate>,
        Vec<CaptureLegacyBackupBlocker>,
    )> {
        let canonical_state = fs::canonicalize(&self.config.state_dir)?;
        let mut candidates = Vec::new();
        let mut blockers = Vec::new();
        if let Some(path) = self.config.legacy_jsonl.as_ref() {
            push_legacy_candidate(
                &mut candidates,
                &canonical_state,
                "legacy-jsonl",
                "jsonl",
                path,
            )?;
        }

        for (legacy_index, artifact_id) in [
            (
                self.config.state_dir.join("body").join("index.sqlite"),
                "legacy-segment-index",
            ),
            (
                self.config.state_dir.join("body-index.sqlite"),
                "legacy-body-index",
            ),
        ] {
            if !legacy_index.exists() {
                continue;
            }
            if self.index_path == legacy_index {
                blockers.push(CaptureLegacyBackupBlocker {
                    code: "v2_index_missing_or_legacy_index_active".to_string(),
                    artifact_id: artifact_id.to_string(),
                    local_path: legacy_index.to_string_lossy().into_owned(),
                    remediation: "restart capture writers onto the v2 body logger, then rerun legacy backup proof".to_string(),
                });
                continue;
            }
            push_legacy_candidate(
                &mut candidates,
                &canonical_state,
                artifact_id,
                "sqlite",
                &legacy_index,
            )?;
            for (suffix, kind) in [("-wal", "sqlite_wal"), ("-shm", "sqlite_shm")] {
                let sidecar = PathBuf::from(format!("{}{}", legacy_index.display(), suffix));
                push_legacy_candidate(
                    &mut candidates,
                    &canonical_state,
                    &format!("{artifact_id}{suffix}"),
                    kind,
                    &sidecar,
                )?;
            }
        }

        let legacy_blobs = self.config.archive_root.join("blobs");
        if self.uses_current_index() {
            push_legacy_directory_candidate(&mut candidates, "legacy-blob-archive", &legacy_blobs)?;
        } else if legacy_blobs.exists() {
            blockers.push(CaptureLegacyBackupBlocker {
                code: "v2_index_missing_or_legacy_index_active".to_string(),
                artifact_id: "legacy-blob-archive".to_string(),
                local_path: legacy_blobs.to_string_lossy().into_owned(),
                remediation: "restart capture writers onto the v2 body logger, then rerun legacy backup proof".to_string(),
            });
        }
        Ok((candidates, blockers))
    }

    /// Accept backup truth only after the transfer agent has verified every
    /// remote segment checksum. The accepted receipt is the sole authority
    /// used by future plans and pressure healing.
    pub fn accept_backup_receipt(&self, receipt: CaptureBackupReceipt) -> Result<()> {
        let _operation = backup_operation_lock(&self.config.state_dir)?;
        if receipt.schema != pressure::BACKUP_RECEIPT_SCHEMA {
            return Err(BodyLogError::new(format!(
                "unsupported backup receipt schema {}",
                receipt.schema
            )));
        }
        validate_receipt_completion_time(receipt.completed_at_unix_ms, "backup receipt")?;
        if receipt.remote_root.trim().is_empty() {
            return Err(BodyLogError::new("backup receipt remote_root is empty"));
        }
        validate_remote_root(&receipt.remote_root)?;
        if receipt.segments.is_empty() {
            return Err(BodyLogError::new(
                "empty backup receipts cannot advance capture freshness",
            ));
        }
        if receipt
            .segments
            .iter()
            .any(|segment| !segment.remote_checksum_verified)
        {
            return Err(BodyLogError::new(
                "backup receipt contains a segment without remote checksum proof",
            ));
        }
        if receipt.segments.iter().any(|segment| {
            segment.remote_path.trim().is_empty() || segment.remote_manifest_path.trim().is_empty()
        }) {
            return Err(BodyLogError::new(
                "backup receipt contains an empty remote artifact path",
            ));
        }
        for segment in &receipt.segments {
            validate_remote_relative_path(&segment.remote_path, "backup receipt remote path")?;
            validate_remote_relative_path(
                &segment.remote_manifest_path,
                "backup receipt remote manifest path",
            )?;
        }

        let mut receipt_hashes = HashSet::with_capacity(receipt.segments.len());
        for segment in &receipt.segments {
            if !receipt_hashes.insert(segment.segment_sha256.clone()) {
                return Err(BodyLogError::new(format!(
                    "backup receipt contains duplicate segment {}",
                    segment.segment_sha256
                )));
            }
        }

        let plan = self.backup_plan_unlocked()?;
        let pending_hashes = plan
            .segments
            .iter()
            .map(|segment| segment.segment_sha256.clone())
            .collect::<HashSet<_>>();
        if receipt_hashes != pending_hashes {
            return Err(BodyLogError::new(
                "backup receipt must cover the exact pending sealed segment set",
            ));
        }
        let expected_verified_through_day =
            plan.segments.last().map(|segment| segment.utc_day.clone());
        if receipt.verified_through_day != expected_verified_through_day {
            return Err(BodyLogError::new(format!(
                "backup receipt verified_through_day {:?} does not match pending segment day {:?}",
                receipt.verified_through_day, expected_verified_through_day
            )));
        }

        let backup_dir = backup_dir(&self.config.state_dir);
        ensure_private_dir(&backup_dir)?;
        if receipt.generation != plan.next_generation {
            return Err(BodyLogError::new(format!(
                "backup receipt generation {} does not match planned generation {}",
                receipt.generation, plan.next_generation
            )));
        }
        for segment in &receipt.segments {
            let planned = plan
                .segments
                .iter()
                .find(|planned| planned.segment_sha256 == segment.segment_sha256)
                .ok_or_else(|| {
                    BodyLogError::new(format!(
                        "backup receipt segment {} is not in the pending plan",
                        segment.segment_sha256
                    ))
                })?;
            if segment.manifest_sha256 != planned.manifest_sha256 {
                return Err(BodyLogError::new(format!(
                    "backup receipt manifest checksum does not match pending segment {}",
                    segment.segment_sha256
                )));
            }
        }

        let bytes = serde_json::to_vec_pretty(&receipt)?;
        let receipt_path = backup_dir
            .join("receipts")
            .join(format!("{:020}.json", receipt.generation));
        let receipt_parent = receipt_path
            .parent()
            .ok_or_else(|| BodyLogError::new("backup receipt path has no parent"))?;
        ensure_private_dir(receipt_parent)?;
        self.record_accepted_backup(&receipt, &plan)?;
        atomic_write_private(&receipt_path, &bytes)?;
        atomic_write_private(
            &pressure::backup_receipt_path(&self.config.state_dir),
            &bytes,
        )?;
        let unbacked_bytes = if self.uses_current_index() {
            self.record_backup_projection(&receipt)?;
            self.projected_unbacked_bytes()?
        } else {
            self.backup_plan_unlocked()?.total_segment_bytes
        };
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .reconcile_unbacked_bytes(unbacked_bytes, receipt.completed_at_unix_ms)
    }

    pub(super) fn projected_unbacked_bytes(&self) -> Result<u64> {
        let conn = open_index_connection(&self.index_path)?;
        let unbacked = conn.query_row(
            "SELECT COALESCE(SUM(segments.body_bytes), 0)
             FROM body_segments AS segments
             LEFT JOIN body_backup_projection AS backups
               ON backups.segment_sha256 = segments.segment_sha256
             WHERE backups.segment_sha256 IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(unbacked.max(0) as u64)
    }

    pub(super) fn rebuild_backup_projection(&self) -> Result<()> {
        if !self.uses_current_index() {
            return Ok(());
        }
        let conn = open_index_connection(&self.index_path)?;
        let current_generation = conn.query_row(
            "SELECT COALESCE(MAX(receipt_generation), 0)
             FROM body_backup_projection",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let expected_generation = latest_receipt_generation(&self.config.state_dir)?;
        if current_generation.max(0) as u64 == expected_generation {
            return Ok(());
        }
        drop(conn);

        let receipts = verified_receipts(&backup_dir(&self.config.state_dir))?;
        let mut conn = open_index_connection_for_maintenance(&self.index_path)?;
        let transaction = begin_write_transaction(&mut conn)?;
        transaction.execute("DELETE FROM body_backup_projection", [])?;
        for receipt in receipts {
            if receipt.segments.is_empty() {
                continue;
            }
            for segment in receipt.segments {
                transaction.execute(
                    "INSERT INTO body_backup_projection (
                        segment_sha256, receipt_generation, accepted_at_unix_ms
                     ) VALUES (?1, ?2, ?3)
                     ON CONFLICT(segment_sha256) DO UPDATE SET
                        receipt_generation = MAX(
                            body_backup_projection.receipt_generation,
                            excluded.receipt_generation
                        ),
                        accepted_at_unix_ms = MAX(
                            body_backup_projection.accepted_at_unix_ms,
                            excluded.accepted_at_unix_ms
                        )",
                    params![
                        segment.segment_sha256,
                        receipt.generation as i64,
                        receipt.completed_at_unix_ms,
                    ],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn record_backup_projection(&self, receipt: &CaptureBackupReceipt) -> Result<()> {
        let mut conn = open_index_connection_for_maintenance(&self.index_path)?;
        let transaction = begin_write_transaction(&mut conn)?;
        for segment in &receipt.segments {
            transaction.execute(
                "INSERT INTO body_backup_projection (
                    segment_sha256, receipt_generation, accepted_at_unix_ms
                 ) VALUES (?1, ?2, ?3)
                 ON CONFLICT(segment_sha256) DO UPDATE SET
                    receipt_generation = excluded.receipt_generation,
                    accepted_at_unix_ms = excluded.accepted_at_unix_ms",
                params![
                    segment.segment_sha256,
                    receipt.generation as i64,
                    receipt.completed_at_unix_ms,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}

pub(super) fn validate_remote_root(value: &str) -> Result<()> {
    let (endpoint, root) = value.split_once(':').ok_or_else(|| {
        BodyLogError::new("backup receipt remote_root must include endpoint:/absolute/path")
    })?;
    if endpoint.is_empty()
        || !endpoint
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-@".contains(&byte))
    {
        return Err(BodyLogError::new(
            "backup receipt remote_root endpoint contains unsupported characters",
        ));
    }
    validate_posix_parts(root, true, "backup receipt remote_root")
}

pub(super) fn validate_remote_relative_path(value: &str, label: &str) -> Result<()> {
    validate_posix_parts(value, false, label)?;
    if value.split('/').next() != Some("segments") {
        return Err(BodyLogError::new(format!(
            "{label} must stay below the segments namespace"
        )));
    }
    Ok(())
}

fn validate_legacy_remote_relative_path(value: &str, label: &str) -> Result<()> {
    validate_posix_parts(value, false, label)?;
    if value.split('/').next() != Some("legacy") {
        return Err(BodyLogError::new(format!(
            "{label} must stay below the legacy namespace"
        )));
    }
    Ok(())
}

fn validate_posix_parts(value: &str, absolute: bool, label: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('/') != absolute
        || value.ends_with('/')
        || value.contains('\\')
    {
        return Err(BodyLogError::new(format!(
            "{label} is not a normalized POSIX path"
        )));
    }
    let relative = if absolute { &value[1..] } else { value };
    if relative.is_empty()
        || relative.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-@".contains(&byte))
        })
    {
        return Err(BodyLogError::new(format!(
            "{label} is not a normalized POSIX path"
        )));
    }
    Ok(())
}

fn segment_projection_rows(conn: &Connection) -> Result<Vec<SegmentProjection>> {
    let mut statement = conn.prepare(
        "SELECT segment_path, segment_sha256, body_bytes, sealed
         FROM body_segments
         ORDER BY first_observed_at_unix_ms, segment_path",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(SegmentProjection {
            path: PathBuf::from(row.get::<_, String>(0)?),
            segment_sha256: row.get(1)?,
            body_bytes: row.get::<_, i64>(2)?.max(0) as u64,
            sealed: row.get::<_, i64>(3)? != 0,
        })
    })?;
    let mut projections = Vec::new();
    for row in rows {
        projections.push(row?);
    }
    Ok(projections)
}

fn verified_segment_projection(
    conn: &Connection,
    verified: &HashSet<String>,
    update_projection: bool,
) -> Result<Vec<(SegmentProjection, Option<SegmentManifest>)>> {
    let mut result = Vec::new();
    for mut projection in segment_projection_rows(conn)? {
        if projection
            .segment_sha256
            .as_ref()
            .is_some_and(|sha256| verified.contains(sha256))
        {
            continue;
        }
        let manifest = read_verified_segment_manifest(&projection.path)?;
        let Some(manifest) = manifest else {
            if projection.sealed {
                return Err(BodyLogError::new(format!(
                    "sealed segment manifest is missing: {}",
                    segment_manifest_path(&projection.path).display()
                )));
            }
            result.push((projection, None));
            continue;
        };
        if !manifest.sealed {
            if projection.sealed {
                return Err(BodyLogError::new(format!(
                    "sealed segment manifest is not marked sealed: {}",
                    segment_manifest_path(&projection.path).display()
                )));
            }
            result.push((projection, None));
            continue;
        }
        if update_projection
            && (!projection.sealed
                || projection.segment_sha256.as_deref() != Some(manifest.segment_sha256.as_str()))
        {
            conn.execute(
                "UPDATE body_segments
                 SET sealed = 1, segment_sha256 = ?1, segment_bytes = ?2,
                     record_count = ?3,
                     body_bytes = CASE WHEN ?4 > 0 THEN ?4 ELSE body_bytes END
                 WHERE segment_path = ?5",
                params![
                    manifest.segment_sha256,
                    manifest.segment_bytes as i64,
                    manifest.record_count as i64,
                    manifest.body_bytes as i64,
                    projection.path.to_string_lossy()
                ],
            )?;
            projection.sealed = true;
            projection.segment_sha256 = Some(manifest.segment_sha256.clone());
            if manifest.body_bytes > 0 {
                projection.body_bytes = manifest.body_bytes;
            }
        }
        result.push((projection, Some(manifest)));
    }
    Ok(result)
}

fn verified_backup_projection_state(conn: &Connection) -> Result<(HashSet<String>, u64)> {
    let mut statement = conn.prepare(
        "SELECT segment_sha256, receipt_generation
         FROM body_backup_projection",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut verified = HashSet::new();
    let mut latest_generation = 0u64;
    for row in rows {
        let (sha256, generation) = row?;
        verified.insert(sha256);
        latest_generation = latest_generation.max(generation.max(0) as u64);
    }
    Ok((verified, latest_generation))
}

pub(super) fn verified_receipt_state(backup_dir: &Path) -> Result<(HashSet<String>, u64)> {
    let mut verified = HashSet::new();
    let mut latest_generation = 0u64;
    for receipt in verified_receipts(backup_dir)? {
        if receipt.segments.is_empty() {
            continue;
        }
        latest_generation = latest_generation.max(receipt.generation);
        for segment in receipt.segments {
            verified.insert(segment.segment_sha256);
        }
    }
    Ok((verified, latest_generation))
}

fn verified_receipts(backup_dir: &Path) -> Result<Vec<CaptureBackupReceipt>> {
    let receipts_dir = backup_dir.join("receipts");
    let entries = match fs::read_dir(receipts_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(err) => return Err(err.into()),
    };
    let mut receipts = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let receipt: CaptureBackupReceipt = serde_json::from_slice(&fs::read(entry.path())?)?;
        validate_stored_receipt(&receipt)?;
        receipts.push(receipt);
    }
    Ok(receipts)
}

pub(super) fn reconcile_latest_backup_receipt(state_dir: &Path) -> Result<()> {
    let receipts_dir = backup_dir(state_dir).join("receipts");
    let entries = match fs::read_dir(&receipts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            paths.push(entry.path());
        }
    }
    paths.sort_by(|left, right| right.file_name().cmp(&left.file_name()));
    let mut latest: Option<CaptureBackupReceipt> = None;
    for path in paths {
        let receipt = read_verified_receipt(&path)?;
        if receipt.segments.is_empty() {
            continue;
        }
        latest = Some(receipt);
        break;
    }
    let path = pressure::backup_receipt_path(state_dir);
    let Some(latest) = latest else {
        match fs::remove_file(&path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        return Ok(());
    };
    let desired = serde_json::to_vec_pretty(&latest)?;
    let current = fs::read(&path).unwrap_or_default();
    if trim_trailing_ascii_whitespace(&current) != trim_trailing_ascii_whitespace(&desired) {
        atomic_write_private(&path, &desired)?;
    }
    Ok(())
}

fn latest_receipt_generation(state_dir: &Path) -> Result<u64> {
    let path = pressure::backup_receipt_path(state_dir);
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let receipt: CaptureBackupReceipt = serde_json::from_slice(&bytes)?;
    validate_stored_receipt(&receipt)?;
    if receipt.segments.is_empty() {
        return Ok(0);
    }
    Ok(receipt.generation)
}

fn read_verified_receipt(path: &Path) -> Result<CaptureBackupReceipt> {
    let receipt: CaptureBackupReceipt = serde_json::from_slice(&fs::read(path)?)?;
    validate_stored_receipt(&receipt)?;
    Ok(receipt)
}

fn validate_stored_receipt(receipt: &CaptureBackupReceipt) -> Result<()> {
    if receipt.schema != pressure::BACKUP_RECEIPT_SCHEMA {
        return Err(BodyLogError::new(format!(
            "unsupported stored backup receipt schema {}",
            receipt.schema
        )));
    }
    validate_receipt_completion_time(receipt.completed_at_unix_ms, "stored backup receipt")?;
    if receipt
        .segments
        .iter()
        .any(|segment| !segment.remote_checksum_verified)
    {
        return Err(BodyLogError::new(
            "stored backup receipt lacks remote checksum proof",
        ));
    }
    Ok(())
}

fn validate_receipt_completion_time(completed_at_unix_ms: i64, label: &str) -> Result<()> {
    let now = now_unix_ms();
    if completed_at_unix_ms <= 0
        || completed_at_unix_ms > now.saturating_add(MAX_RECEIPT_FUTURE_SKEW_MS)
    {
        return Err(BodyLogError::new(format!(
            "{label} completion time is invalid"
        )));
    }
    Ok(())
}

fn trim_trailing_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[..end]
}

#[derive(Debug)]
struct LegacyArtifactCandidate {
    artifact_id: String,
    kind: String,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegacyArtifactStat {
    bytes: u64,
    modified_at_unix_ms: i64,
}

fn push_legacy_candidate(
    candidates: &mut Vec<LegacyArtifactCandidate>,
    canonical_state: &Path,
    artifact_id: &str,
    kind: &str,
    path: &Path,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(BodyLogError::new(format!(
            "legacy artifact is not a regular file: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path)?;
    if !canonical.starts_with(canonical_state) {
        return Err(BodyLogError::new(format!(
            "legacy artifact escapes Switchback state: {}",
            path.display()
        )));
    }
    candidates.push(LegacyArtifactCandidate {
        artifact_id: artifact_id.to_string(),
        kind: kind.to_string(),
        path: canonical,
    });
    Ok(())
}

fn push_legacy_directory_candidate(
    candidates: &mut Vec<LegacyArtifactCandidate>,
    artifact_id: &str,
    path: &Path,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(BodyLogError::new(format!(
            "legacy artifact is not a real directory: {}",
            path.display()
        )));
    }
    candidates.push(LegacyArtifactCandidate {
        artifact_id: artifact_id.to_string(),
        kind: "directory".to_string(),
        path: fs::canonicalize(path)?,
    });
    Ok(())
}

fn legacy_artifact_stat(path: &Path) -> Result<LegacyArtifactStat> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(BodyLogError::new(format!(
            "legacy artifact is not a regular file: {}",
            path.display()
        )));
    }
    let modified_at_unix_ms = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BodyLogError::new("legacy artifact mtime predates the Unix epoch"))?
        .as_millis()
        .try_into()
        .map_err(|_| BodyLogError::new("legacy artifact mtime exceeds i64"))?;
    Ok(LegacyArtifactStat {
        bytes: metadata.len(),
        modified_at_unix_ms,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyTreeFile {
    path: PathBuf,
    relative_path: String,
    stat: LegacyArtifactStat,
}

fn legacy_directory_fingerprint(path: &Path) -> Result<(LegacyArtifactStat, String)> {
    let before = collect_legacy_tree_files(path)?;
    let mut digest = Sha256::new();
    let mut total_bytes = 0u64;
    let mut latest_modified_at_unix_ms = 0i64;
    for file in &before {
        let sha256 = sha256_file_stable(&file.path, file.stat)?;
        digest.update(sha256.as_bytes());
        digest.update(b"\t");
        digest.update(file.stat.bytes.to_string().as_bytes());
        digest.update(b"\t");
        digest.update(file.relative_path.as_bytes());
        digest.update(b"\n");
        total_bytes = total_bytes.saturating_add(file.stat.bytes);
        latest_modified_at_unix_ms = latest_modified_at_unix_ms.max(file.stat.modified_at_unix_ms);
    }
    let after = collect_legacy_tree_files(path)?;
    if before != after {
        return Err(BodyLogError::new(format!(
            "legacy artifact directory changed while hashing: {}",
            path.display()
        )));
    }
    if before.is_empty() {
        latest_modified_at_unix_ms = fs::metadata(path)?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(|_| BodyLogError::new("legacy artifact mtime predates Unix epoch"))?
            .as_millis()
            .try_into()
            .map_err(|_| BodyLogError::new("legacy artifact mtime exceeds i64"))?;
    }
    Ok((
        LegacyArtifactStat {
            bytes: total_bytes,
            modified_at_unix_ms: latest_modified_at_unix_ms,
        },
        format!("{:x}", digest.finalize()),
    ))
}

fn collect_legacy_tree_files(root: &Path) -> Result<Vec<LegacyTreeFile>> {
    let mut pending = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut entries = fs::read_dir(&directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for path in entries {
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(BodyLogError::new(format!(
                    "legacy artifact tree contains a symlink: {}",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                pending.push(path);
                continue;
            }
            if !metadata.is_file() {
                return Err(BodyLogError::new(format!(
                    "legacy artifact tree contains a non-file: {}",
                    path.display()
                )));
            }
            let relative = path.strip_prefix(root).map_err(|_| {
                BodyLogError::new(format!(
                    "legacy artifact escaped directory root: {}",
                    path.display()
                ))
            })?;
            let relative_path = relative
                .components()
                .map(|component| component.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| BodyLogError::new("legacy artifact path is not UTF-8"))?
                .join("/");
            validate_posix_parts(&relative_path, false, "legacy artifact tree relative path")?;
            files.push(LegacyTreeFile {
                path: path.clone(),
                relative_path,
                stat: legacy_artifact_stat(&path)?,
            });
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(files)
}

fn sha256_file_stable(path: &Path, before: LegacyArtifactStat) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let after = legacy_artifact_stat(path)?;
    if before != after {
        return Err(BodyLogError::new(format!(
            "legacy artifact changed while checksumming: {}",
            path.display()
        )));
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn legacy_backup_dir(state_dir: &Path) -> PathBuf {
    backup_dir(state_dir).join("legacy")
}

fn read_cached_legacy_plan(state_dir: &Path) -> Result<Option<CaptureLegacyBackupPlan>> {
    let path = legacy_backup_dir(state_dir).join("pending-plan.json");
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let plan: CaptureLegacyBackupPlan = serde_json::from_slice(&bytes)?;
    if plan.schema != LEGACY_BACKUP_PLAN_SCHEMA {
        return Err(BodyLogError::new(format!(
            "unsupported cached legacy backup plan schema {}",
            plan.schema
        )));
    }
    Ok(Some(plan))
}

fn verified_legacy_artifacts(state_dir: &Path) -> Result<Vec<CaptureLegacyBackupReceiptItem>> {
    let receipts_dir = legacy_backup_dir(state_dir).join("receipts");
    let entries = match fs::read_dir(receipts_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut artifacts = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let receipt: CaptureLegacyBackupReceipt = serde_json::from_slice(&fs::read(entry.path())?)?;
        if receipt.schema != LEGACY_BACKUP_RECEIPT_SCHEMA {
            return Err(BodyLogError::new(format!(
                "unsupported stored legacy backup receipt schema {}",
                receipt.schema
            )));
        }
        validate_remote_root(&receipt.remote_root)?;
        for artifact in receipt.artifacts {
            if !artifact.remote_checksum_verified {
                return Err(BodyLogError::new(
                    "stored legacy backup receipt lacks remote checksum proof",
                ));
            }
            validate_legacy_remote_relative_path(
                &artifact.remote_path,
                "stored legacy backup receipt remote path",
            )?;
            artifacts.push(artifact);
        }
    }
    Ok(artifacts)
}

pub(super) fn backup_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("body").join("backup")
}

pub(super) fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| BodyLogError::new("backup receipt has no parent directory"))?;
    ensure_private_dir(parent)?;
    let temp = parent.join(format!(
        ".capture-backup.tmp-{}-{}",
        std::process::id(),
        NEXT_BACKUP_TEMP_ID.fetch_add(1, Ordering::Relaxed)
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
    sync_directory(parent)?;
    Ok(())
}
