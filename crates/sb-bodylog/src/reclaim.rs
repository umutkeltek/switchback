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
    begin_write_transaction, copy_file_verified, insert_record_on, now_unix_ms,
    open_index_connection, open_index_connection_for_maintenance, read_verified_segment_manifest,
    retention_cutoff_ms, scan_segment, segment_lock_path, segment_manifest_path, sha256_hex,
    sync_directory, try_acquire_segment_lock, upsert_segment_manifest_projection_on, BodyLogError,
    BodyLogger, Result,
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
            self.validate_local_catalog_entry(&entry)?;
            total_segment_bytes = total_segment_bytes.saturating_add(entry.segment_bytes);
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
        };
        if !options.confirm {
            return Ok(report);
        }
        for candidate in plan.segments {
            self.reclaim_one(&candidate)?;
            report.reclaimed_segments += 1;
            report.reclaimed_bytes = report
                .reclaimed_bytes
                .saturating_add(candidate.segment_bytes);
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
        if entry.state == RemoteSegmentState::VerifiedLocal {
            self.validate_local_catalog_entry(&entry)?;
            return Ok(());
        }
        if entry.state != RemoteSegmentState::RemoteOnly {
            return Err(BodyLogError::new(format!(
                "segment {} is not remote-only",
                segment_sha256
            )));
        }

        let target_segment = PathBuf::from(&entry.segment_path);
        let target_manifest = PathBuf::from(&entry.manifest_path);
        self.validate_restore_target(&target_segment)?;
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
        for frame in frames {
            let mut record = frame.record;
            record.archive_path = target_segment.to_string_lossy().into_owned();
            record.storage = storage.to_string();
            insert_record_on(&transaction, &record)?;
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
        ensure_private_dir(parent)?;
        let canonical_parent = fs::canonicalize(parent)?;
        let canonical_root = fs::canonicalize(allowed)?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(BodyLogError::new(
                "restore target escapes Switchback capture root",
            ));
        }
        Ok(())
    }

    fn reclaim_one(&self, candidate: &CaptureReclaimPlanItem) -> Result<()> {
        let mut entry = read_catalog_entry(&self.config.state_dir, &candidate.segment_sha256)?;
        self.validate_local_catalog_entry(&entry)?;
        let segment = PathBuf::from(&entry.segment_path);
        let manifest = PathBuf::from(&entry.manifest_path);
        let lock = try_acquire_segment_lock(&segment)?.ok_or_else(|| {
            BodyLogError::new(format!(
                "refusing to reclaim locked capture segment {}",
                segment.display()
            ))
        })?;
        let staging_dir = segment
            .parent()
            .ok_or_else(|| BodyLogError::new("capture segment has no parent"))?
            .join(".switchback-reclaim")
            .join(&entry.segment_sha256);
        ensure_private_dir(&staging_dir)?;
        let staged_segment = staging_dir.join(&entry.segment_file);
        let manifest_file = manifest
            .file_name()
            .ok_or_else(|| BodyLogError::new("capture manifest has no file name"))?;
        let staged_manifest = staging_dir.join(manifest_file);
        if staged_segment.exists() || staged_manifest.exists() {
            return Err(BodyLogError::new(
                "reclaim staging path is not empty; recovery is required",
            ));
        }

        let conn = open_index_connection(&self.index_path)?;
        let projected: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM body_segments
                 WHERE segment_path = ?1 AND sealed = 1 LIMIT 1",
                params![segment.to_string_lossy().into_owned()],
                |row| row.get(0),
            )
            .optional()?;
        if projected.is_none() {
            return Err(BodyLogError::new(
                "refusing to reclaim a local segment missing its sealed index projection",
            ));
        }

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

            let mut conn = open_index_connection_for_maintenance(&self.index_path)?;
            let transaction = begin_write_transaction(&mut conn)?;
            let body_hashes = {
                let mut statement = transaction.prepare(
                    "SELECT DISTINCT body_sha256 FROM body_events WHERE archive_path = ?1",
                )?;
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
        })();
        if let Err(error) = index_result {
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
        let _ = fs::remove_file(segment_lock_path(&segment));
        Ok(())
    }

    fn finish_remote_only_cleanup(&self, entry: &mut RemoteSegmentEntry) -> Result<()> {
        self.validate_restore_target(Path::new(&entry.segment_path))?;
        let (staging_dir, staged_segment, staged_manifest) = reclaim_staging_paths(entry)?;
        let segment = PathBuf::from(&entry.segment_path);
        let manifest = PathBuf::from(&entry.manifest_path);
        remove_verified_artifact(&segment, &entry.segment_sha256)?;
        remove_verified_artifact(&manifest, &entry.manifest_sha256)?;
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
        for mut entry in read_catalog_entries(&self.config.state_dir)? {
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
            self.validate_restore_target(&segment)?;
            let (staging_dir, staged_segment, staged_manifest) = reclaim_staging_paths(&entry)?;
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

fn reclaim_staging_paths(entry: &RemoteSegmentEntry) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let segment = PathBuf::from(&entry.segment_path);
    let segment_parent = segment
        .parent()
        .ok_or_else(|| BodyLogError::new("capture segment has no parent"))?;
    let reclaim_root = segment_parent.join(".switchback-reclaim");
    let staging_dir = reclaim_root.join(&entry.segment_sha256);
    if entry
        .reclaim_staging_dir
        .as_deref()
        .is_some_and(|stored| Path::new(stored) != staging_dir)
    {
        return Err(BodyLogError::new(
            "reclaim staging path does not match the deterministic capture path",
        ));
    }
    for directory in [&reclaim_root, &staging_dir] {
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
