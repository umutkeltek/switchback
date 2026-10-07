use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use rusqlite::{params, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use super::backup::{
    atomic_write_private, backup_dir, backup_operation_lock, ensure_private_dir,
    validate_remote_relative_path, validate_remote_root, verified_receipt_state, CaptureBackupPlan,
    CaptureBackupReceipt,
};
use super::{
    begin_write_transaction, copy_file_verified, day_floor_ms, insert_record_on, now_unix_ms,
    open_index_connection, open_index_connection_for_maintenance, read_verified_segment_manifest,
    retention_cutoff_ms, scan_segment, segment_manifest_path, sha256_hex, sync_directory,
    try_acquire_segment_lock, upsert_segment_manifest_projection_on, BodyLogError, BodyLogger,
    Result,
};

const RECLAIM_PLAN_SCHEMA: &str = "switchback/capture-reclaim-plan@1";
const RECLAIM_PROOF_SCHEMA: &str = "switchback/capture-reclaim-proof@1";
const RECLAIM_REPORT_SCHEMA: &str = "switchback/capture-reclaim-report@1";
const REMOTE_SEGMENT_SCHEMA: &str = "switchback/remote-segment@1";
const RECLAIM_PROOF_MAX_AGE_MS: i64 = 15 * 60 * 1_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReclaimPlan {
    pub schema: String,
    pub keep_days: u64,
    pub cutoff_unix_ms: i64,
    pub segments: Vec<CaptureReclaimPlanItem>,
    pub total_segment_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReclaimPlanItem {
    pub segment_file: String,
    pub segment_path: String,
    pub manifest_path: String,
    pub segment_sha256: String,
    pub manifest_sha256: String,
    pub segment_bytes: u64,
    pub utc_day: String,
    pub remote_root: String,
    pub remote_path: String,
    pub remote_manifest_path: String,
    #[serde(default)]
    pub local_pair_absent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReclaimProof {
    pub schema: String,
    pub verified_at_unix_ms: i64,
    pub segments: Vec<CaptureReclaimProofItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureReclaimProofItem {
    pub segment_sha256: String,
    pub manifest_sha256: String,
    pub remote_path: String,
    pub remote_manifest_path: String,
    pub remote_checksums_verified: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct CaptureReclaimOptions {
    pub keep_days: u64,
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureReclaimReport {
    pub schema: String,
    pub dry_run: bool,
    pub keep_days: u64,
    pub candidate_segments: u64,
    pub candidate_bytes: u64,
    pub reclaimed_segments: u64,
    pub reclaimed_bytes: u64,
    pub reconciled_segments: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RemoteSegmentState {
    VerifiedLocal,
    Reclaiming,
    RemoteOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RemoteSegmentEntry {
    schema: String,
    state: RemoteSegmentState,
    receipt_generation: u64,
    segment_file: String,
    segment_path: String,
    manifest_path: String,
    segment_sha256: String,
    manifest_sha256: String,
    segment_bytes: u64,
    record_count: u64,
    first_observed_at_unix_ms: i64,
    last_observed_at_unix_ms: i64,
    utc_day: String,
    remote_root: String,
    remote_path: String,
    remote_manifest_path: String,
    #[serde(default)]
    reclaim_staging_dir: Option<String>,
}

impl BodyLogger {
    pub(super) fn record_accepted_backup(
        &self,
        receipt: &CaptureBackupReceipt,
        plan: &CaptureBackupPlan,
    ) -> Result<()> {
        for planned in &plan.segments {
            let remote = receipt
                .segments
                .iter()
                .find(|item| item.segment_sha256 == planned.segment_sha256)
                .ok_or_else(|| {
                    BodyLogError::new(format!(
                        "accepted receipt lacks planned segment {}",
                        planned.segment_sha256
                    ))
                })?;
            let entry = RemoteSegmentEntry {
                schema: REMOTE_SEGMENT_SCHEMA.to_string(),
                state: RemoteSegmentState::VerifiedLocal,
                receipt_generation: receipt.generation,
                segment_file: planned.segment_file.clone(),
                segment_path: planned.segment_path.clone(),
                manifest_path: planned.manifest_path.clone(),
                segment_sha256: planned.segment_sha256.clone(),
                manifest_sha256: planned.manifest_sha256.clone(),
                segment_bytes: planned.segment_bytes,
                record_count: planned.record_count,
                first_observed_at_unix_ms: planned.first_observed_at_unix_ms,
                last_observed_at_unix_ms: planned.last_observed_at_unix_ms,
                utc_day: planned.utc_day.clone(),
                remote_root: receipt.remote_root.clone(),
                remote_path: remote.remote_path.clone(),
                remote_manifest_path: remote.remote_manifest_path.clone(),
                reclaim_staging_dir: None,
            };
            write_catalog_entry(&self.config.state_dir, &entry)?;
        }
        Ok(())
    }

    pub fn reclaim_plan(&self, keep_days: u64) -> Result<CaptureReclaimPlan> {
        let _operation = backup_operation_lock(&self.config.state_dir)?;
        self.reclaim_plan_unlocked(keep_days)
    }

    fn reclaim_plan_unlocked(&self, keep_days: u64) -> Result<CaptureReclaimPlan> {
        self.recover_reclaim_intents()?;
        let cutoff_unix_ms = retention_cutoff_ms(now_unix_ms(), keep_days);
        let (verified, _) = verified_receipt_state(&backup_dir(&self.config.state_dir))?;
        let mut segments = Vec::new();
        let mut total_segment_bytes = 0u64;
        for entry in read_catalog_entries(&self.config.state_dir)? {
            if entry.state != RemoteSegmentState::VerifiedLocal
                || entry.last_observed_at_unix_ms >= cutoff_unix_ms
                || !verified.contains(&entry.segment_sha256)
            {
                continue;
            }
            let local_pair_absent = self.local_pair_absent(&entry)?;
            if local_pair_absent {
                self.validate_accepted_catalog_receipt(&entry)?;
            } else {
                total_segment_bytes = total_segment_bytes.saturating_add(entry.segment_bytes);
            }
            segments.push(CaptureReclaimPlanItem {
                segment_file: entry.segment_file,
                segment_path: entry.segment_path,
                manifest_path: entry.manifest_path,
                segment_sha256: entry.segment_sha256,
                manifest_sha256: entry.manifest_sha256,
                segment_bytes: entry.segment_bytes,
                utc_day: entry.utc_day,
                remote_root: entry.remote_root,
                remote_path: entry.remote_path,
                remote_manifest_path: entry.remote_manifest_path,
                local_pair_absent,
            });
        }
        segments.sort_by(|left, right| {
            left.utc_day
                .cmp(&right.utc_day)
                .then_with(|| left.segment_path.cmp(&right.segment_path))
        });
        Ok(CaptureReclaimPlan {
            schema: RECLAIM_PLAN_SCHEMA.to_string(),
            keep_days,
            cutoff_unix_ms,
            segments,
            total_segment_bytes,
        })
    }

    pub fn reclaim_verified_segments(
        &self,
        options: CaptureReclaimOptions,
        proof: CaptureReclaimProof,
    ) -> Result<CaptureReclaimReport> {
        let _operation = backup_operation_lock(&self.config.state_dir)?;
        let plan = self.reclaim_plan_unlocked(options.keep_days)?;
        validate_reclaim_proof(&plan, &proof)?;
        let mut report = CaptureReclaimReport {
            schema: RECLAIM_REPORT_SCHEMA.to_string(),
            dry_run: !options.confirm,
            keep_days: options.keep_days,
            candidate_segments: plan.segments.len() as u64,
            candidate_bytes: plan.total_segment_bytes,
            reclaimed_segments: 0,
            reclaimed_bytes: 0,
            reconciled_segments: 0,
        };
        if !options.confirm {
            return Ok(report);
        }
        for candidate in plan.segments {
            if self.reclaim_one(&candidate, &proof)? {
                report.reclaimed_segments += 1;
                report.reclaimed_bytes = report
                    .reclaimed_bytes
                    .saturating_add(candidate.segment_bytes);
            } else {
                report.reconciled_segments += 1;
            }
        }
        Ok(report)
    }

    pub fn restore_remote_segment(
        &self,
        segment_sha256: &str,
        source_segment: &Path,
        source_manifest: &Path,
    ) -> Result<()> {
        let _operation = backup_operation_lock(&self.config.state_dir)?;
        self.recover_reclaim_intents()?;
        let mut entry = read_catalog_entry(&self.config.state_dir, segment_sha256)?;
        let source_segment_bytes = fs::read(source_segment)?;
        if sha256_hex(&source_segment_bytes) != entry.segment_sha256 {
            return Err(BodyLogError::new(
                "restore segment checksum does not match remote catalog",
            ));
        }
        let source_manifest_bytes = fs::read(source_manifest)?;
        if sha256_hex(&source_manifest_bytes) != entry.manifest_sha256 {
            return Err(BodyLogError::new(
                "restore manifest checksum does not match remote catalog",
            ));
        }
        if !matches!(
            entry.state,
            RemoteSegmentState::VerifiedLocal | RemoteSegmentState::RemoteOnly
        ) {
            return Err(BodyLogError::new(format!(
                "segment {} is not remote-only",
                segment_sha256
            )));
        }

        let target_segment = PathBuf::from(&entry.segment_path);
        let target_manifest = PathBuf::from(&entry.manifest_path);
        self.validate_restore_target(&target_segment)?;
        let _segment_lock = try_acquire_segment_lock(&target_segment)?
            .ok_or_else(|| BodyLogError::new("refusing restore of locked capture segment"))?;
        self.validate_accepted_catalog_receipt(&entry)?;
        self.refuse_shared_staging(&entry)?;
        let absent = self.classify_local_pair(&entry, true)?;
        let projection = open_index_connection(&self.index_path)?;
        let projected = validate_sealed_projection_on(&projection, &entry, true)?;
        if entry.state == RemoteSegmentState::VerifiedLocal && !absent && projected {
            return Ok(());
        }
        if target_manifest != segment_manifest_path(&target_segment) {
            return Err(BodyLogError::new(
                "remote catalog manifest path does not match segment",
            ));
        }
        copy_file_verified(source_segment, &target_segment, Some(&entry.segment_sha256))?;
        copy_file_verified(
            source_manifest,
            &target_manifest,
            Some(&entry.manifest_sha256),
        )?;

        let manifest = read_verified_segment_manifest(&target_segment)?
            .ok_or_else(|| BodyLogError::new("restored segment manifest is missing"))?;
        if manifest.segment_sha256 != entry.segment_sha256 {
            return Err(BodyLogError::new(
                "restored segment manifest does not match remote catalog",
            ));
        }
        let frames = scan_segment(&target_segment, false)?;
        let mut conn = open_index_connection_for_maintenance(&self.index_path)?;
        let transaction = begin_write_transaction(&mut conn)?;
        let storage = if target_segment.starts_with(&self.spool_dir) {
            "spool_segment"
        } else {
            "archive_segment"
        };
        let body_bytes = frames
            .iter()
            .map(|frame| frame.record.body_bytes)
            .fold(0u64, u64::saturating_add);
        // A peer's retirement or a crashed restore can leave this owner's rows intact.
        if !validate_sealed_projection_on(&transaction, &entry, true)? {
            for frame in frames {
                let mut record = frame.record;
                record.archive_path = target_segment.to_string_lossy().into_owned();
                record.storage = storage.to_string();
                insert_record_on(&transaction, &record)?;
            }
        }
        upsert_segment_manifest_projection_on(
            &transaction,
            &target_segment,
            storage,
            &manifest,
            Some(body_bytes),
        )?;
        transaction.commit()?;

        entry.state = RemoteSegmentState::VerifiedLocal;
        entry.reclaim_staging_dir = None;
        write_catalog_entry(&self.config.state_dir, &entry)
    }

    // Caller holds the store's backup-operation lock. Relocation changes physical
    // coordinates only: both accepted hashes and all remote custody proof survive.
    pub(super) fn relocate_backed_spool_segment(
        &self,
        source: &Path,
        destination: &Path,
        segment_sha256: &str,
    ) -> Result<()> {
        let bytes = match fs::read(catalog_path(&self.config.state_dir, segment_sha256)?) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let mut entry: RemoteSegmentEntry = serde_json::from_slice(&bytes)?;
        validate_catalog_entry(&entry)?;
        if entry.state != RemoteSegmentState::VerifiedLocal
            || entry.segment_sha256 != segment_sha256
            || (Path::new(&entry.segment_path) != source
                && Path::new(&entry.segment_path) != destination)
        {
            return Err(BodyLogError::new(
                "spool relocation conflicts with remote custody",
            ));
        }
        if source.exists() && Path::new(&entry.segment_path) == source {
            self.validate_local_catalog_entry(&entry)?;
        }
        entry.segment_path = destination.to_string_lossy().into_owned();
        entry.manifest_path = segment_manifest_path(destination)
            .to_string_lossy()
            .into_owned();
        self.validate_local_catalog_entry(&entry)?;
        write_catalog_entry(&self.config.state_dir, &entry)
    }

    fn validate_local_catalog_entry(&self, entry: &RemoteSegmentEntry) -> Result<()> {
        let segment = PathBuf::from(&entry.segment_path);
        let manifest = PathBuf::from(&entry.manifest_path);
        validate_regular_contained(
            &segment,
            &[self.config.archive_root.as_path(), self.spool_dir.as_path()],
        )?;
        validate_regular_contained(
            &manifest,
            &[self.config.archive_root.as_path(), self.spool_dir.as_path()],
        )?;
        if manifest != segment_manifest_path(&segment) {
            return Err(BodyLogError::new(
                "remote catalog manifest path does not match segment",
            ));
        }
        let verified = read_verified_segment_manifest(&segment)?
            .ok_or_else(|| BodyLogError::new("verified local segment manifest is missing"))?;
        if verified.segment_sha256 != entry.segment_sha256
            || sha256_hex(&fs::read(&manifest)?) != entry.manifest_sha256
        {
            return Err(BodyLogError::new(
                "verified local segment differs from the accepted remote catalog",
            ));
        }
        Ok(())
    }

    fn validate_restore_target(&self, target: &Path) -> Result<()> {
        if target
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        {
            return Err(BodyLogError::new(
                "restore target contains non-normal path components",
            ));
        }
        let allowed = if target.starts_with(&self.config.archive_root) {
            self.config.archive_root.as_path()
        } else if target.starts_with(&self.spool_dir) {
            self.spool_dir.as_path()
        } else {
            return Err(BodyLogError::new(
                "restore target is outside Switchback capture roots",
            ));
        };
        let parent = target
            .parent()
            .ok_or_else(|| BodyLogError::new("restore target has no parent"))?;
        let canonical_root = canonical_capture_root(allowed)?;
        let mut ancestor = parent;
        while !ancestor.try_exists()? {
            ancestor = ancestor
                .parent()
                .ok_or_else(|| BodyLogError::new("restore target lacks an existing ancestor"))?;
        }
        if !fs::canonicalize(ancestor)?.starts_with(&canonical_root) {
            return Err(BodyLogError::new(
                "restore parent escapes the available capture root",
            ));
        }
        ensure_private_dir(parent)?;
        let canonical_parent = fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(BodyLogError::new(
                "restore target escapes Switchback capture root",
            ));
        }
        Ok(())
    }

    // No mkdir or custody writes: absence is meaningful only beneath an existing
    // archive parent, never beneath a detached volume or an interrupted reclaim.
    fn local_pair_absent(&self, entry: &RemoteSegmentEntry) -> Result<bool> {
        self.classify_local_pair(entry, false)
    }

    fn classify_local_pair(&self, entry: &RemoteSegmentEntry, restoring: bool) -> Result<bool> {
        let segment = Path::new(&entry.segment_path);
        let manifest = Path::new(&entry.manifest_path);
        if !segment.is_absolute()
            || segment
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            || manifest != segment_manifest_path(segment)
            || segment.file_name().and_then(|n| n.to_str()) != Some(entry.segment_file.as_str())
        {
            return Err(BodyLogError::new(
                "capture pair paths are not normalized and exact",
            ));
        }
        let present = |path: &Path| -> Result<bool> {
            match fs::symlink_metadata(path) {
                Ok(metadata)
                    if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
                {
                    Ok(true)
                }
                Ok(_) => Err(BodyLogError::new(
                    "capture pair contains a non-regular target",
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            }
        };
        self.refuse_shared_staging(entry)?;
        match (present(segment)?, present(manifest)?) {
            (true, true) => {
                self.validate_local_catalog_entry(entry)?;
                Ok(false)
            }
            (false, false) => {
                let allowed = if segment.starts_with(&self.config.archive_root) {
                    self.config.archive_root.as_path()
                } else if restoring && segment.starts_with(&self.spool_dir) {
                    self.spool_dir.as_path()
                } else {
                    return Err(BodyLogError::new(
                        "missing capture pair is not in the shared archive",
                    ));
                };
                let root = canonical_capture_root(allowed)?;
                let parent = fs::canonicalize(
                    segment
                        .parent()
                        .ok_or_else(|| BodyLogError::new("capture pair has no parent"))?,
                )?;
                if !parent.starts_with(&root) {
                    return Err(BodyLogError::new(
                        "missing capture pair escapes archive root",
                    ));
                }
                self.refuse_shared_staging(entry)?;
                Ok(true)
            }
            _ => Err(BodyLogError::new("capture pair is only partially absent")),
        }
    }

    fn refuse_shared_staging(&self, entry: &RemoteSegmentEntry) -> Result<()> {
        let (staging, _, _) = reclaim_staging_paths(entry, &self.config.state_dir)?;
        let shared = staging
            .parent()
            .ok_or_else(|| BodyLogError::new("reclaim stage has no shared root"))?;
        match fs::read_dir(shared) {
            Ok(mut entries) => {
                if let Some(entry) = entries.next() {
                    entry?;
                    return Err(BodyLogError::new(
                        "shared capture reclaim staging requires owner recovery",
                    ));
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn validate_accepted_catalog_receipt(&self, entry: &RemoteSegmentEntry) -> Result<()> {
        let path = backup_dir(&self.config.state_dir)
            .join("receipts")
            .join(format!("{:020}.json", entry.receipt_generation));
        validate_regular_contained(&path, &[backup_dir(&self.config.state_dir).as_path()])?;
        let receipt: CaptureBackupReceipt = serde_json::from_slice(&fs::read(path)?)?;
        let matches = receipt
            .segments
            .iter()
            .filter(|item| item.segment_sha256 == entry.segment_sha256)
            .collect::<Vec<_>>();
        if receipt.schema != "switchback/capture-backup@2"
            || receipt.generation != entry.receipt_generation
            || receipt.remote_root != entry.remote_root
            || matches.len() != 1
            || !matches[0].remote_checksum_verified
            || matches[0].manifest_sha256 != entry.manifest_sha256
            || matches[0].remote_path != entry.remote_path
            || matches[0].remote_manifest_path != entry.remote_manifest_path
        {
            return Err(BodyLogError::new(
                "catalog differs from its exact accepted backup receipt",
            ));
        }
        Ok(())
    }

    fn retire_segment_projection(
        &self,
        entry: &RemoteSegmentEntry,
        allow_retired: bool,
    ) -> Result<()> {
        let mut conn = open_index_connection_for_maintenance(&self.index_path)?;
        let transaction = begin_write_transaction(&mut conn)?;
        validate_sealed_projection_on(&transaction, entry, allow_retired)?;
        let segment = Path::new(&entry.segment_path);
        let body_hashes = {
            let mut statement = transaction
                .prepare("SELECT DISTINCT body_sha256 FROM body_events WHERE archive_path = ?1")?;
            let rows = statement
                .query_map(params![segment.to_string_lossy().into_owned()], |row| {
                    row.get::<_, String>(0)
                })?;
            let mut hashes = Vec::new();
            for row in rows {
                hashes.push(row?);
            }
            hashes
        };
        transaction.execute(
            "DELETE FROM body_events WHERE archive_path = ?1",
            params![segment.to_string_lossy().into_owned()],
        )?;
        for hash in body_hashes {
            let replacement: Option<(String, String)> = transaction
                .query_row(
                    "SELECT storage, archive_path
                     FROM body_events
                     WHERE body_sha256 = ?1 AND archive_path <> ''
                     ORDER BY observed_at_unix_ms DESC, event_id DESC
                     LIMIT 1",
                    params![&hash],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match replacement {
                Some((storage, archive_path)) => {
                    transaction.execute(
                        "UPDATE body_blobs
                         SET storage = ?2, archive_path = ?3
                         WHERE body_sha256 = ?1",
                        params![hash, storage, archive_path],
                    )?;
                }
                None => {
                    transaction.execute(
                        "DELETE FROM body_blobs WHERE body_sha256 = ?1",
                        params![hash],
                    )?;
                }
            }
        }
        transaction.execute(
            "DELETE FROM body_segments WHERE segment_path = ?1 AND sealed = 1",
            params![segment.to_string_lossy().into_owned()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn reclaim_one(
        &self,
        candidate: &CaptureReclaimPlanItem,
        proof: &CaptureReclaimProof,
    ) -> Result<bool> {
        let segment = PathBuf::from(&candidate.segment_path);
        let manifest = PathBuf::from(&candidate.manifest_path);
        let lock = try_acquire_segment_lock(&segment)?.ok_or_else(|| {
            BodyLogError::new(format!(
                "refusing to reclaim locked capture segment {}",
                segment.display()
            ))
        })?;
        let mut entry = read_catalog_entry(&self.config.state_dir, &candidate.segment_sha256)?;
        if entry.state != RemoteSegmentState::VerifiedLocal
            || entry.segment_file != candidate.segment_file
            || entry.segment_path != candidate.segment_path
            || entry.manifest_path != candidate.manifest_path
            || entry.segment_sha256 != candidate.segment_sha256
            || entry.manifest_sha256 != candidate.manifest_sha256
            || entry.segment_bytes != candidate.segment_bytes
            || entry.utc_day != candidate.utc_day
            || entry.remote_root != candidate.remote_root
            || entry.remote_path != candidate.remote_path
            || entry.remote_manifest_path != candidate.remote_manifest_path
        {
            return Err(BodyLogError::new(
                "catalog changed after planning; refresh exact remote proof",
            ));
        }
        self.validate_accepted_catalog_receipt(&entry)?;
        let absent = self.local_pair_absent(&entry)?;
        if absent != candidate.local_pair_absent {
            return Err(BodyLogError::new(
                "capture pair changed after planning; refresh proof",
            ));
        }
        let single_plan = CaptureReclaimPlan {
            schema: RECLAIM_PLAN_SCHEMA.to_string(),
            keep_days: 0,
            cutoff_unix_ms: 0,
            segments: vec![candidate.clone()],
            total_segment_bytes: candidate.segment_bytes,
        };
        let single_proof = CaptureReclaimProof {
            schema: proof.schema.clone(),
            verified_at_unix_ms: proof.verified_at_unix_ms,
            segments: proof
                .segments
                .iter()
                .filter(|item| item.segment_sha256 == candidate.segment_sha256)
                .cloned()
                .collect(),
        };
        validate_reclaim_proof(&single_plan, &single_proof)?;
        if absent {
            self.validate_accepted_catalog_receipt(&entry)?;
            self.retire_segment_projection(&entry, true)?;
            entry.state = RemoteSegmentState::RemoteOnly;
            entry.reclaim_staging_dir = None;
            write_catalog_entry(&self.config.state_dir, &entry)?;
            return Ok(false);
        }
        let (staging_dir, staged_segment, staged_manifest) =
            reclaim_staging_paths(&entry, &self.config.state_dir)?;
        ensure_private_dir(&staging_dir)?;
        if staged_segment.exists() || staged_manifest.exists() {
            return Err(BodyLogError::new(
                "reclaim staging path is not empty; recovery is required",
            ));
        }

        let conn = open_index_connection(&self.index_path)?;
        validate_sealed_projection_on(&conn, &entry, false)?;
        entry.state = RemoteSegmentState::Reclaiming;
        entry.reclaim_staging_dir = Some(staging_dir.to_string_lossy().into_owned());
        write_catalog_entry(&self.config.state_dir, &entry)?;
        let index_result = (|| -> Result<()> {
            fs::rename(&segment, &staged_segment)?;
            fs::rename(&manifest, &staged_manifest)?;
            sync_directory(&staging_dir)?;
            if let Some(parent) = segment.parent() {
                sync_directory(parent)?;
            }

            self.retire_segment_projection(&entry, false)?;
            Ok(())
        })();
        if let Err(error) = index_result {
            drop(lock);
            if let Err(recovery_error) = self.recover_reclaim_intents() {
                return Err(BodyLogError::new(format!(
                    "{error}; reclaim recovery also failed: {recovery_error}"
                )));
            }
            return Err(error);
        }

        // SQLite commit is the point of no return. Any later failure must
        // converge to remote-only cleanup rather than resurrecting unindexed
        // local files.
        entry.state = RemoteSegmentState::RemoteOnly;
        write_catalog_entry(&self.config.state_dir, &entry)?;
        self.finish_remote_only_cleanup(&mut entry)?;
        drop(lock);
        // Keep the shared lock inode: a peer may already hold its descriptor.
        Ok(true)
    }

    fn finish_remote_only_cleanup(&self, entry: &mut RemoteSegmentEntry) -> Result<()> {
        self.validate_restore_target(Path::new(&entry.segment_path))?;
        let (staging_dir, staged_segment, staged_manifest) =
            reclaim_staging_paths(entry, &self.config.state_dir)?;
        let segment = PathBuf::from(&entry.segment_path);
        // Canonical files may be a peer restore. Only our staged artifacts
        // belong to this cleanup intent.
        remove_verified_artifact(&staged_segment, &entry.segment_sha256)?;
        remove_verified_artifact(&staged_manifest, &entry.manifest_sha256)?;
        if let Some(parent) = segment.parent() {
            sync_directory(parent)?;
        }
        remove_empty_reclaim_dirs(&staging_dir)?;

        entry.state = RemoteSegmentState::RemoteOnly;
        entry.reclaim_staging_dir = None;
        write_catalog_entry(&self.config.state_dir, entry)
    }

    fn recover_reclaim_intents(&self) -> Result<()> {
        let projection = open_index_connection(&self.index_path)?;
        for mut entry in read_catalog_entries(&self.config.state_dir)? {
            if entry.state == RemoteSegmentState::VerifiedLocal {
                let source = PathBuf::from(&entry.segment_path);
                if source.starts_with(self.spool_dir.join("segments")) {
                    let destination = self
                        .day_dir(day_floor_ms(entry.first_observed_at_unix_ms))
                        .join("segments")
                        .join(&entry.segment_file);
                    // Recover historical drains only from the exact sealed index
                    // projection, then re-verify the segment AND manifest bytes.
                    let projected: Option<i64> = projection
                        .query_row(
                            "SELECT 1 FROM body_segments WHERE segment_path = ?1
                         AND segment_sha256 = ?2 AND sealed = 1 LIMIT 1",
                            params![destination.to_string_lossy(), entry.segment_sha256],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if projected.is_some() {
                        self.relocate_backed_spool_segment(
                            &source,
                            &destination,
                            &entry.segment_sha256,
                        )?;
                    }
                }
                continue;
            }
            if entry.state == RemoteSegmentState::RemoteOnly && entry.reclaim_staging_dir.is_none()
            {
                continue; // completed retirement is not a pending deletion intent
            }
            if !matches!(
                entry.state,
                RemoteSegmentState::Reclaiming | RemoteSegmentState::RemoteOnly
            ) {
                continue;
            }
            if entry.state == RemoteSegmentState::Reclaiming && entry.reclaim_staging_dir.is_none()
            {
                return Err(BodyLogError::new(
                    "reclaiming catalog entry lacks staging path",
                ));
            }
            let segment = PathBuf::from(&entry.segment_path);
            let manifest = PathBuf::from(&entry.manifest_path);
            let (staging_dir, staged_segment, staged_manifest) =
                reclaim_staging_paths(&entry, &self.config.state_dir)?;
            self.validate_restore_target(&segment)?;
            let _lock = try_acquire_segment_lock(&segment)?
                .ok_or_else(|| BodyLogError::new("refusing recovery of locked capture segment"))?;
            let conn = open_index_connection(&self.index_path)?;
            let projected: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM body_segments WHERE segment_path = ?1 LIMIT 1",
                    params![segment.to_string_lossy().into_owned()],
                    |row| row.get(0),
                )
                .optional()?;
            if projected.is_some() {
                restore_staged_artifact(&staged_segment, &segment, &entry.segment_sha256)?;
                restore_staged_artifact(&staged_manifest, &manifest, &entry.manifest_sha256)?;
                self.validate_local_catalog_entry(&entry)?;
                remove_empty_reclaim_dirs(&staging_dir)?;
                entry.state = RemoteSegmentState::VerifiedLocal;
                entry.reclaim_staging_dir = None;
                write_catalog_entry(&self.config.state_dir, &entry)?;
            } else {
                entry.state = RemoteSegmentState::RemoteOnly;
                entry.reclaim_staging_dir = Some(staging_dir.to_string_lossy().into_owned());
                write_catalog_entry(&self.config.state_dir, &entry)?;
                self.finish_remote_only_cleanup(&mut entry)?;
            }
        }
        Ok(())
    }
}

fn canonical_capture_root(root: &Path) -> Result<PathBuf> {
    if let Some(anchor) = super::volume_anchor(root) {
        validate_volume_anchor_mount(&anchor)?;
    }
    let canonical = fs::canonicalize(root)?;
    if !fs::metadata(&canonical)?.is_dir() {
        return Err(BodyLogError::new(
            "capture root is not an available directory",
        ));
    }
    Ok(canonical)
}

fn validate_volume_anchor_mount(anchor: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(anchor)?;
        let parent = anchor
            .parent()
            .ok_or_else(|| BodyLogError::new("volume anchor has no parent"))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.dev() == fs::metadata(parent)?.dev()
        {
            return Err(BodyLogError::new(
                "capture volume anchor is not mounted; refusing custody mutation",
            ));
        }
    }
    Ok(())
}

fn validate_sealed_projection_on(
    conn: &rusqlite::Connection,
    entry: &RemoteSegmentEntry,
    allow_retired: bool,
) -> Result<bool> {
    let projection: Option<(String, u64, u64, bool)> = conn.query_row(
        "SELECT segment_sha256, segment_bytes, record_count, sealed FROM body_segments WHERE segment_path = ?1",
        params![entry.segment_path], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).optional()?;
    match projection {
        Some((hash, bytes, records, true))
            if hash == entry.segment_sha256
                && bytes == entry.segment_bytes
                && records == entry.record_count =>
        {
            Ok(true)
        }
        None if allow_retired => {
            let leftovers: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM body_events WHERE archive_path = ?1)
                   OR EXISTS(SELECT 1 FROM body_blobs WHERE archive_path = ?1)",
                params![entry.segment_path],
                |row| row.get(0),
            )?;
            if leftovers {
                return Err(BodyLogError::new("retired segment has orphaned index rows"));
            }
            Ok(false)
        }
        _ => Err(BodyLogError::new(
            "capture segment lacks its exact sealed checksum projection",
        )),
    }
}

fn reclaim_staging_paths(
    entry: &RemoteSegmentEntry,
    state_dir: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let segment = PathBuf::from(&entry.segment_path);
    let segment_parent = segment
        .parent()
        .ok_or_else(|| BodyLogError::new("capture segment has no parent"))?;
    let reclaim_root = segment_parent.join(".switchback-reclaim");
    let shared_dir = reclaim_root.join(&entry.segment_sha256);
    let owner = sha256_hex(fs::canonicalize(state_dir)?.as_os_str().as_encoded_bytes());
    let staging_dir = shared_dir.join(owner);
    if entry
        .reclaim_staging_dir
        .as_deref()
        .is_some_and(|stored| Path::new(stored) != staging_dir)
    {
        return Err(BodyLogError::new(
            "reclaim staging intent lacks exact owner binding; legacy shared intents require governed recovery",
        ));
    }
    for directory in [&reclaim_root, &shared_dir, &staging_dir] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            }
            Ok(_) => {
                return Err(BodyLogError::new(format!(
                    "reclaim staging path is not a regular directory: {}",
                    directory.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let segment_file = Path::new(&entry.segment_file);
    if segment_file.components().count() != 1
        || !matches!(segment_file.components().next(), Some(Component::Normal(_)))
    {
        return Err(BodyLogError::new(
            "remote catalog segment file is not a plain file name",
        ));
    }
    let manifest = PathBuf::from(&entry.manifest_path);
    let manifest_file = manifest
        .file_name()
        .ok_or_else(|| BodyLogError::new("capture manifest has no file name"))?;
    Ok((
        staging_dir.clone(),
        staging_dir.join(segment_file),
        staging_dir.join(manifest_file),
    ))
}

fn remove_verified_artifact(path: &Path, expected_sha256: &str) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(BodyLogError::new(format!(
            "refusing to remove non-regular capture artifact {}",
            path.display()
        )));
    }
    if sha256_hex(&fs::read(path)?) != expected_sha256 {
        return Err(BodyLogError::new(format!(
            "refusing to remove capture artifact with an unexpected checksum {}",
            path.display()
        )));
    }
    fs::remove_file(path)?;
    Ok(())
}

fn restore_staged_artifact(staged: &Path, target: &Path, expected_sha256: &str) -> Result<()> {
    if target.exists() {
        let metadata = fs::symlink_metadata(target)?;
        if !metadata.file_type().is_file()
            || metadata.file_type().is_symlink()
            || sha256_hex(&fs::read(target)?) != expected_sha256
        {
            return Err(BodyLogError::new(format!(
                "reclaim recovery target differs from catalog proof: {}",
                target.display()
            )));
        }
        remove_verified_artifact(staged, expected_sha256)?;
        return Ok(());
    }
    let metadata = fs::symlink_metadata(staged).map_err(|error| {
        BodyLogError::new(format!(
            "cannot recover staged capture artifact {}: {error}",
            staged.display()
        ))
    })?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || sha256_hex(&fs::read(staged)?) != expected_sha256
    {
        return Err(BodyLogError::new(format!(
            "staged capture artifact differs from catalog proof: {}",
            staged.display()
        )));
    }
    fs::rename(staged, target)?;
    if let Some(parent) = target.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn validate_reclaim_proof(plan: &CaptureReclaimPlan, proof: &CaptureReclaimProof) -> Result<()> {
    if proof.schema != RECLAIM_PROOF_SCHEMA {
        return Err(BodyLogError::new(format!(
            "unsupported reclaim proof schema {}",
            proof.schema
        )));
    }
    let now = now_unix_ms();
    if proof.verified_at_unix_ms > now.saturating_add(60_000)
        || now.saturating_sub(proof.verified_at_unix_ms) > RECLAIM_PROOF_MAX_AGE_MS
    {
        return Err(BodyLogError::new(
            "reclaim proof is stale or has a future timestamp",
        ));
    }
    let mut proof_by_hash = HashMap::new();
    for item in &proof.segments {
        if !item.remote_checksums_verified {
            return Err(BodyLogError::new(
                "reclaim proof contains an unverified remote artifact",
            ));
        }
        if proof_by_hash
            .insert(item.segment_sha256.as_str(), item)
            .is_some()
        {
            return Err(BodyLogError::new(format!(
                "reclaim proof duplicates segment {}",
                item.segment_sha256
            )));
        }
    }
    let planned_hashes = plan
        .segments
        .iter()
        .map(|item| item.segment_sha256.as_str())
        .collect::<HashSet<_>>();
    if proof_by_hash.keys().copied().collect::<HashSet<_>>() != planned_hashes {
        return Err(BodyLogError::new(
            "reclaim proof must cover the exact retention candidate set",
        ));
    }
    for planned in &plan.segments {
        let proven = proof_by_hash[planned.segment_sha256.as_str()];
        if proven.manifest_sha256 != planned.manifest_sha256
            || proven.remote_path != planned.remote_path
            || proven.remote_manifest_path != planned.remote_manifest_path
        {
            return Err(BodyLogError::new(format!(
                "reclaim proof does not match remote catalog for {}",
                planned.segment_sha256
            )));
        }
    }
    Ok(())
}

fn catalog_dir(state_dir: &Path) -> PathBuf {
    backup_dir(state_dir).join("catalog")
}

fn catalog_path(state_dir: &Path, segment_sha256: &str) -> Result<PathBuf> {
    if segment_sha256.len() != 64
        || !segment_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(BodyLogError::new("invalid catalog segment checksum"));
    }
    Ok(catalog_dir(state_dir).join(format!("{segment_sha256}.json")))
}

fn read_catalog_entry(state_dir: &Path, segment_sha256: &str) -> Result<RemoteSegmentEntry> {
    let path = catalog_path(state_dir, segment_sha256)?;
    let entry: RemoteSegmentEntry = serde_json::from_slice(&fs::read(&path)?)?;
    validate_catalog_entry(&entry)?;
    Ok(entry)
}

fn read_catalog_entries(state_dir: &Path) -> Result<Vec<RemoteSegmentEntry>> {
    let directory = catalog_dir(state_dir);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut result = Vec::new();
    for item in entries {
        let item = item?;
        if !item.file_type()?.is_file()
            || item.path().extension().and_then(|value| value.to_str()) != Some("json")
        {
            continue;
        }
        let entry: RemoteSegmentEntry = serde_json::from_slice(&fs::read(item.path())?)?;
        validate_catalog_entry(&entry)?;
        result.push(entry);
    }
    Ok(result)
}

fn validate_catalog_entry(entry: &RemoteSegmentEntry) -> Result<()> {
    if entry.schema != REMOTE_SEGMENT_SCHEMA {
        return Err(BodyLogError::new(format!(
            "unsupported remote segment schema {}",
            entry.schema
        )));
    }
    if entry.segment_sha256.len() != 64 || entry.manifest_sha256.len() != 64 {
        return Err(BodyLogError::new(
            "remote segment catalog has an invalid checksum",
        ));
    }
    validate_remote_root(&entry.remote_root)?;
    validate_remote_relative_path(&entry.remote_path, "remote segment catalog path")?;
    validate_remote_relative_path(
        &entry.remote_manifest_path,
        "remote segment catalog manifest path",
    )?;
    Ok(())
}

fn write_catalog_entry(state_dir: &Path, entry: &RemoteSegmentEntry) -> Result<()> {
    validate_catalog_entry(entry)?;
    let path = catalog_path(state_dir, &entry.segment_sha256)?;
    let bytes = serde_json::to_vec_pretty(entry)?;
    atomic_write_private(&path, &bytes)
}

fn validate_regular_contained(path: &Path, roots: &[&Path]) -> Result<()> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(BodyLogError::new(format!(
            "capture artifact path is not normalized: {}",
            path.display()
        )));
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(BodyLogError::new(format!(
            "capture artifact is not a regular file: {}",
            path.display()
        )));
    }
    let canonical = fs::canonicalize(path)?;
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let canonical_root = fs::canonicalize(root)?;
        if canonical.starts_with(&canonical_root) && canonical != canonical_root {
            return Ok(());
        }
    }
    Err(BodyLogError::new(format!(
        "capture artifact escapes Switchback roots: {}",
        path.display()
    )))
}

fn remove_empty_reclaim_dirs(staging_dir: &Path) -> Result<()> {
    if staging_dir.is_dir() && fs::read_dir(staging_dir)?.next().is_none() {
        fs::remove_dir(staging_dir)?;
    }
    if let Some(parent) = staging_dir.parent() {
        if parent.is_dir() && fs::read_dir(parent)?.next().is_none() {
            fs::remove_dir(parent)?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod mount_tests {
    use super::*;

    #[test]
    fn capture_segment_lock_refuses_symlinks_without_changing_the_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "switchback-lock-link-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let victim = root.join("unrelated");
        fs::write(&victim, b"unrelated-evidence").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).unwrap();
        let segment = root.join("capture.sbcap");
        symlink(&victim, super::super::segment_lock_path(&segment)).unwrap();
        assert!(
            try_acquire_segment_lock(&segment).is_err(),
            "lock must not follow a symlink"
        );
        assert_eq!(
            fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(&victim).unwrap(), b"unrelated-evidence");
    }

    #[test]
    fn reclaim_refuses_a_proof_bound_to_a_different_current_catalog_path() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "switchback-catalog-binding-{}-{nonce}",
            std::process::id()
        ));
        let logger = BodyLogger::new(super::super::BodyLoggerConfig {
            state_dir: root.join("state"),
            archive_root: root.join("archive"),
            legacy_jsonl: None,
            inline_threshold_bytes: 16,
        })
        .unwrap();
        logger
            .record_at(
                super::super::BodyEventInput {
                    request_id: "catalog-binding".into(),
                    capture_stage: super::super::CaptureStage::ClientInbound,
                    protocol: "http".into(),
                    upstream: None,
                    model: None,
                    status: Some(200),
                    content_type: None,
                    metadata: serde_json::json!({}),
                    body: b"binding-body".to_vec(),
                },
                now_unix_ms() - 10 * 86_400_000,
            )
            .unwrap();
        logger.seal_active().unwrap();
        let plan = logger.backup_plan().unwrap();
        let segment = &plan.segments[0];
        logger
            .accept_backup_receipt(CaptureBackupReceipt {
                schema: "switchback/capture-backup@2".into(),
                generation: plan.next_generation,
                completed_at_unix_ms: now_unix_ms(),
                verified_through_day: Some(segment.utc_day.clone()),
                remote_root: "nas.example:/srv/capture".into(),
                segments: vec![super::super::CaptureBackupReceiptItem {
                    segment_sha256: segment.segment_sha256.clone(),
                    manifest_sha256: segment.manifest_sha256.clone(),
                    remote_path: format!("segments/{}", segment.segment_file),
                    remote_manifest_path: format!(
                        "segments/{}.manifest.json",
                        segment.segment_file
                    ),
                    remote_checksum_verified: true,
                }],
            })
            .unwrap();
        let mut candidate = logger.reclaim_plan(3).unwrap().segments.remove(0);
        candidate.remote_path = "segments/a-different-receipt-target.sbcap".into();
        let proof = CaptureReclaimProof {
            schema: RECLAIM_PROOF_SCHEMA.into(),
            verified_at_unix_ms: now_unix_ms(),
            segments: vec![CaptureReclaimProofItem {
                segment_sha256: candidate.segment_sha256.clone(),
                manifest_sha256: candidate.manifest_sha256.clone(),
                remote_path: candidate.remote_path.clone(),
                remote_manifest_path: candidate.remote_manifest_path.clone(),
                remote_checksums_verified: true,
            }],
        };
        let before =
            fs::read(catalog_path(&logger.config.state_dir, &candidate.segment_sha256).unwrap())
                .unwrap();
        // The public operation lock excludes cooperative writers. This deliberately
        // probes the private under-segment-lock check against uncooperative drift.
        let error = logger.reclaim_one(&candidate, &proof).unwrap_err();
        assert!(error.to_string().contains("catalog changed"), "{error}");
        assert_eq!(
            fs::read(catalog_path(&logger.config.state_dir, &candidate.segment_sha256).unwrap())
                .unwrap(),
            before
        );
        assert!(Path::new(&candidate.segment_path).is_file());
        assert_eq!(logger.latest_events(10).unwrap().len(), 1);
    }

    #[test]
    fn existing_unmounted_anchor_is_not_capture_storage() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let parent = std::env::temp_dir().join(format!(
            "switchback-volume-anchor-{}-{nonce}",
            std::process::id()
        ));
        let anchor = parent.join("detached");
        fs::create_dir_all(&anchor).unwrap();
        assert!(anchor.is_dir());
        assert!(validate_volume_anchor_mount(&anchor).is_err());
        fs::remove_dir(anchor).unwrap();
        fs::remove_dir(parent).unwrap();
    }
}
