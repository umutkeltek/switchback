//! Exact-input admission for the existing segment recovery owner.
use super::*;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::time::Instant;

#[derive(Debug, Serialize)]
pub struct SelectedRecoveryReport {
    pub schema: &'static str,
    pub segment_path: PathBuf,
    pub outcome: &'static str,
    pub source_device: u64,
    pub source_inode: u64,
    pub source_mtime_ns: i128,
    pub source_bytes: u64,
    pub source_sha256: String,
    pub recovered_bytes: u64,
    pub recovered_sha256: String,
    pub record_count: u64,
}
#[cfg(unix)]
struct Selection {
    path: PathBuf,
    directories: Vec<(PathBuf, u64, u64)>,
    device: u64,
    inode: u64,
    max_bytes: u64,
    deadline: Instant,
    source_fingerprint: Option<(u64, i64, i64)>,
}
#[cfg(unix)]
fn owned_regular(path: &Path) -> Result<fs::Metadata> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_file() || m.nlink() != 1 || m.uid() != unsafe { libc::geteuid() } {
        return Err(BodyLogError::new(
            "selected recovery requires an owned regular single-link file",
        ));
    }
    Ok(m)
}
#[cfg(unix)]
impl Selection {
    fn check_file(&self, file: Option<&fs::File>) -> Result<()> {
        self.check()?;
        if let Some(file) = file {
            let m = file.metadata()?;
            if !m.is_file()
                || m.nlink() != 1
                || m.uid() != unsafe { libc::geteuid() }
                || m.dev() != self.device
                || m.ino() != self.inode
                || m.len() > self.max_bytes
                || self
                    .source_fingerprint
                    .is_some_and(|expected| expected != (m.len(), m.mtime(), m.mtime_nsec()))
            {
                return Err(BodyLogError::new(
                    "selected opened source descriptor identity changed",
                ));
            }
        }
        Ok(())
    }
    fn check(&self) -> Result<()> {
        if Instant::now() >= self.deadline {
            return Err(BodyLogError::new("selected recovery budget exhausted; partial native progress is resumable with the same input"));
        }
        for (path, dev, ino) in &self.directories {
            let m = fs::symlink_metadata(path)?;
            if !m.is_dir()
                || m.dev() != *dev
                || m.ino() != *ino
                || m.uid() != unsafe { libc::geteuid() }
                || m.mode() & 0o022 != 0
            {
                return Err(BodyLogError::new(
                    "selected recovery directory identity changed",
                ));
            }
        }
        let m = owned_regular(&self.path)?;
        if self
            .source_fingerprint
            .is_some_and(|expected| expected != (m.len(), m.mtime(), m.mtime_nsec()))
        {
            return Err(BodyLogError::new(
                "selected immutable source bytes or timestamp changed",
            ));
        }
        if m.dev() != self.device || m.ino() != self.inode || m.len() > self.max_bytes {
            return Err(BodyLogError::new(
                "selected recovery source identity or size changed",
            ));
        }
        Ok(())
    }
}
#[cfg(unix)]
impl BodyLogger {
    /// Recover one explicitly named segment. Never discovers archive or spool paths.
    /// The caller opens read-only; admission precedes every mutation.
    pub fn recover_selected_segment_now(
        &self,
        path: &Path,
        max_bytes: u64,
        budget: Duration,
    ) -> Result<SelectedRecoveryReport> {
        if max_bytes == 0
            || max_bytes > 64 * 1024 * 1024
            || budget.is_zero()
            || budget > Duration::from_secs(120)
        {
            return Err(BodyLogError::new("selected recovery requires 1..64 MiB and a positive budget no greater than 120 seconds"));
        }
        if !self.uses_current_index()
            || !path.is_absolute()
            || path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || path.extension().and_then(OsStr::to_str) != Some("sbcap")
            || fs::canonicalize(path)? != path
        {
            return Err(BodyLogError::new(
                "selected recovery requires a canonical absolute current-index segment path",
            ));
        }
        let state = fs::canonicalize(&self.config.state_dir)?;
        let roots = [
            self.spool_dir.join("segments"),
            state.join("body/archive"),
            self.config.archive_root.clone(),
        ];
        let root = roots
            .iter()
            .find(|root| path.starts_with(root) && path != root.as_path())
            .ok_or_else(|| {
                BodyLogError::new("selected segment is outside this store's recovery namespaces")
            })?;
        if fs::canonicalize(root)? != *root {
            return Err(BodyLogError::new(
                "selected recovery root must be canonical and contain no links",
            ));
        }
        let mut directories = Vec::new();
        let mut directory = root.clone();
        for component in std::iter::once(None).chain(
            path.strip_prefix(root)
                .map_err(|_| BodyLogError::new("selected root changed"))?
                .parent()
                .ok_or_else(|| BodyLogError::new("selected path lacks a parent"))?
                .components()
                .map(Some),
        ) {
            if let Some(component) = component {
                directory.push(component.as_os_str());
            }
            let m = fs::symlink_metadata(&directory)?;
            if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
                return Err(BodyLogError::new(
                    "selected recovery namespace must be an owned directory without writable peers",
                ));
            }
            directories.push((directory.clone(), m.dev(), m.ino()));
        }
        let initial = owned_regular(path)?;
        let mut selected = Selection {
            path: path.to_owned(),
            directories,
            device: initial.dev(),
            inode: initial.ino(),
            max_bytes,
            deadline: Instant::now() + budget,
            source_fingerprint: None,
        };
        selected.check()?;
        let custody_path = PathBuf::from(format!("{}.custody.json", path.display()));
        owned_regular(&custody_path)?;
        let custody = custody::read(path)?
            .ok_or_else(|| BodyLogError::new("selected recovery lacks custody evidence"))?;
        if !custody.complete || !custody.owners.contains(&state) {
            return Err(BodyLogError::new(
                "selected recovery lacks complete custody for this store",
            ));
        }
        // Admission never creates a replacement writer lock or follows a lock link.
        let lock_path = segment_lock_path(path);
        let lock_identity = owned_regular(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let lock = options.open(&lock_path)?;
        if lock.metadata()?.dev() != lock_identity.dev()
            || lock.metadata()?.ino() != lock_identity.ino()
        {
            return Err(BodyLogError::new(
                "selected recovery writer lock identity changed",
            ));
        }
        if !try_lock_file(&lock)? {
            return Err(BodyLogError::new(
                "selected segment has an active writer; unchanged",
            ));
        }
        let _operation = backup::try_backup_operation_lock(&self.config.state_dir)?;
        selected.check()?;
        // Bind the report preimage after both owners have granted exclusive access.
        let initial = owned_regular(path)?;
        selected.source_fingerprint = Some((initial.len(), initial.mtime(), initial.mtime_nsec()));
        owned_regular(&self.index_path)?;
        for sidecar in [wal_path(&self.index_path), shm_path(&self.index_path)] {
            match fs::symlink_metadata(&sidecar) {
                Ok(_) => {
                    owned_regular(&sidecar)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let (source_sha256, _) =
            streaming::hash_file_checked(path, |file| selected.check_file(file))?;
        let hashed = owned_regular(path)?;
        if (hashed.len(), hashed.mtime(), hashed.mtime_nsec())
            != (initial.len(), initial.mtime(), initial.mtime_nsec())
        {
            return Err(BodyLogError::new(
                "selected source changed during preimage hashing; unchanged by recovery",
            ));
        }
        let manifest_path = segment_manifest_path(path);
        let already_sealed = match fs::symlink_metadata(&manifest_path) {
            Ok(_) => {
                if owned_regular(&manifest_path)?.len() > 65536 {
                    return Err(BodyLogError::new(
                        "selected manifest exceeds metadata bound",
                    ));
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if already_sealed {
            owned_regular(&manifest_path)?;
        }
        let mut conn = open_index_connection_with_busy_timeout(&self.index_path, 1)?;
        if !already_sealed {
            tracing::info!(stage="validate_immutable_segment",segment=%path.display(),"selected recovery progress");
            let mut summary = streaming::SegmentSummary::default();
            streaming::visit_segment_checked(
                path,
                false,
                |file| selected.check_file(file),
                |record, _| {
                    summary.record_count += 1;
                    summary.body_bytes = summary.body_bytes.saturating_add(record.body_bytes);
                    summary
                        .first_observed_at_unix_ms
                        .get_or_insert(record.observed_at_unix_ms);
                    summary.last_observed_at_unix_ms = Some(record.observed_at_unix_ms);
                    Ok(())
                },
            )?;
            selected.check()?;
            let scanned = owned_regular(path)?;
            if scanned.len() > initial.len()
                || (scanned.len() == initial.len()
                    && (scanned.mtime(), scanned.mtime_nsec())
                        != (initial.mtime(), initial.mtime_nsec()))
            {
                return Err(BodyLogError::new(
                    "selected source changed outside native tail recovery",
                ));
            }
            tracing::info!(
                stage = "replay_index",
                records = summary.record_count,
                "selected recovery progress"
            );
            let transaction = begin_write_transaction(&mut conn)?;
            streaming::visit_segment_checked(
                path,
                false,
                |file| selected.check_file(file),
                |captured, _| {
                    let mut record = captured.clone();
                    record.archive_path = path.to_string_lossy().into_owned();
                    record.storage = if path.starts_with(self.spool_dir.join("segments")) {
                        "spool_segment"
                    } else {
                        "archive_segment"
                    }
                    .into();
                    insert_recovered_record_on(&transaction, &record)
                },
            )?;
            selected.check()?;
            transaction.commit()?;
            // Existing owners retain resumable native progress: rows, seal, projection.
            write_segment_manifest_checked(path, &summary, true, |file| selected.check_file(file))?;
        }
        selected.check()?;
        let manifest =
            read_verified_segment_manifest_checked(path, |file| selected.check_file(file))?
                .ok_or_else(|| BodyLogError::new("selected recovery manifest disappeared"))?;
        if !manifest.sealed {
            return Err(BodyLogError::new(
                "selected recovery manifest is not sealed",
            ));
        }
        let transaction = begin_write_transaction(&mut conn)?;
        upsert_segment_manifest_projection_on(
            &transaction,
            path,
            if path.starts_with(self.spool_dir.join("segments")) {
                "spool_segment"
            } else {
                "archive_segment"
            },
            &manifest,
            Some(manifest.body_bytes),
        )?;
        selected.check()?;
        transaction.commit()?;
        Ok(SelectedRecoveryReport {
            schema: "switchback/selected-segment-recovery@1",
            segment_path: path.to_owned(),
            outcome: if already_sealed {
                "verified_existing_seal"
            } else {
                "recovered"
            },
            source_device: initial.dev(),
            source_inode: initial.ino(),
            source_mtime_ns: i128::from(initial.mtime()) * 1_000_000_000
                + i128::from(initial.mtime_nsec()),
            source_bytes: initial.len(),
            source_sha256,
            recovered_bytes: manifest.segment_bytes,
            recovered_sha256: manifest.segment_sha256,
            record_count: manifest.record_count,
        })
    }
}

#[cfg(not(unix))]
impl BodyLogger {
    pub fn recover_selected_segment_now(
        &self,
        _path: &Path,
        _max_bytes: u64,
        _budget: Duration,
    ) -> Result<SelectedRecoveryReport> {
        Err(BodyLogError::new(
            "selected recovery requires native Unix file identity and locks",
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        logger: BodyLogger,
        record: BodyRecord,
    }
    impl Fixture {
        fn new(abandon: bool) -> Self {
            let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
                "sb-selected-{}-{}",
                std::process::id(),
                NEXT_SEGMENT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            let logger = BodyLogger::new(BodyLoggerConfig {
                state_dir: root.join("state"),
                archive_root: root.join("archive"),
                legacy_jsonl: None,
                inline_threshold_bytes: 1,
            })
            .unwrap();
            let record = logger
                .record_segmented_at(
                    BodyEventInput {
                        request_id: "selected-fixture".into(),
                        capture_stage: CaptureStage::ClientInbound,
                        protocol: "fixture".into(),
                        upstream: None,
                        model: None,
                        status: None,
                        content_type: None,
                        metadata: serde_json::json!({}),
                        body: b"selected body survives".to_vec(),
                    },
                    now_unix_ms(),
                )
                .unwrap();
            drop(backup::backup_operation_lock(&logger.config.state_dir).unwrap());
            if abandon {
                drop(logger.segment_writer.lock().unwrap().active.take());
            }
            Self {
                root,
                logger,
                record,
            }
        }
        fn path(&self) -> &Path {
            Path::new(&self.record.archive_path)
        }
        fn recover(&self) -> Result<SelectedRecoveryReport> {
            self.logger.recover_selected_segment_now(
                self.path(),
                64 * 1024 * 1024,
                Duration::from_secs(10),
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            drop(self.logger.segment_writer.lock().unwrap().active.take());
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn selected_recovery_retries_after_unpublished_native_seal() {
        let f = Fixture::new(true);
        let before = fs::read(f.path()).unwrap();
        let summary = streaming::segment_summary(f.path(), false).unwrap();
        let prefix = format!(
            "{}.manifest.tmp-",
            f.path().file_name().unwrap().to_string_lossy()
        );
        let error = write_segment_manifest_checked(f.path(), &summary, true, |_| {
            let staged = fs::read_dir(f.path().parent().unwrap())?.any(|entry| {
                entry.is_ok_and(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            });
            if staged {
                Err(BodyLogError::new(
                    "injected stop before native seal publication",
                ))
            } else {
                Ok(())
            }
        });
        assert!(error.is_err());
        assert!(!segment_manifest_path(f.path()).exists());
        assert_eq!(fs::read(f.path()).unwrap(), before);
        assert_eq!(f.recover().unwrap().record_count, 1);
    }

    #[test]
    fn selected_recovery_native_replay_failure_rolls_back_and_retries() {
        let f = Fixture::new(true);
        let bytes = fs::read(f.path()).unwrap();
        let conn = Connection::open(&f.logger.index_path).unwrap();
        conn.execute(
            "DELETE FROM body_events WHERE event_id=?1",
            params![f.record.event_id],
        )
        .unwrap();
        conn.execute_batch("CREATE TRIGGER selected_replay_failure BEFORE INSERT ON body_events BEGIN SELECT RAISE(ABORT,'fixture pause'); END;").unwrap();
        assert!(f.recover().is_err());
        assert!(!segment_manifest_path(f.path()).exists());
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM body_events WHERE event_id=?1",
                params![f.record.event_id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        conn.execute_batch("DROP TRIGGER selected_replay_failure")
            .unwrap();
        assert_eq!(f.recover().unwrap().record_count, 1);
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM body_events WHERE event_id=?1",
                params![f.record.event_id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
    #[test]
    fn selected_recovery_refuses_dangling_manifest_and_parent_symlink() {
        let f = Fixture::new(true);
        let manifest = segment_manifest_path(f.path());
        std::os::unix::fs::symlink(f.root.join("absent"), &manifest).unwrap();
        assert!(f.recover().is_err());
        assert!(fs::symlink_metadata(&manifest)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_file(&manifest).unwrap();
        let linked = f.root.join("linked-archive");
        std::os::unix::fs::symlink(&f.logger.config.archive_root, &linked).unwrap();
        let path = linked.join(
            f.path()
                .strip_prefix(&f.logger.config.archive_root)
                .unwrap(),
        );
        assert!(f
            .logger
            .recover_selected_segment_now(&path, 1024 * 1024, Duration::from_secs(1))
            .is_err());
    }

    #[test]
    fn selected_recovery_never_discovers_unrelated_archive_or_spool() {
        let f = Fixture::new(true);
        let unrelated = f.logger.config.archive_root.join("poison.sbcap");
        fs::write(&unrelated, b"invalid historical evidence").unwrap();
        std::os::unix::fs::symlink(
            &f.logger.config.archive_root,
            f.logger.config.archive_root.join("recursive-link"),
        )
        .unwrap();
        let report = f.recover().unwrap();
        assert_eq!(report.record_count, 1);
        assert_eq!(report.outcome, "recovered");
        assert_eq!(
            f.logger.read_record_body(&f.record).unwrap(),
            b"selected body survives"
        );
        assert_eq!(fs::read(unrelated).unwrap(), b"invalid historical evidence");
    }
    #[test]
    fn selected_recovery_refuses_active_writer_without_changes() {
        let f = Fixture::new(false);
        let before = fs::read(f.path()).unwrap();
        assert!(f
            .recover()
            .unwrap_err()
            .to_string()
            .contains("active writer"));
        assert_eq!(fs::read(f.path()).unwrap(), before);
        assert!(!segment_manifest_path(f.path()).exists());
    }
    #[test]
    fn selected_recovery_preserves_existing_seal_and_resumes_projection() {
        let f = Fixture::new(true);
        let first = f.recover().unwrap();
        let manifest = segment_manifest_path(f.path());
        let before = fs::read(&manifest).unwrap();
        let metadata = fs::metadata(&manifest).unwrap();
        let conn = Connection::open(&f.logger.index_path).unwrap();
        conn.execute(
            "UPDATE body_segments SET sealed=0 WHERE segment_path=?1",
            params![f.record.archive_path],
        )
        .unwrap();
        let second = f.recover().unwrap();
        assert_eq!(second.outcome, "verified_existing_seal");
        assert_eq!(second.recovered_sha256, first.recovered_sha256);
        assert_eq!(fs::read(&manifest).unwrap(), before);
        assert_eq!(
            fs::metadata(&manifest).unwrap().mtime_nsec(),
            metadata.mtime_nsec()
        );
        assert_eq!(
            conn.query_row(
                "SELECT sealed FROM body_segments WHERE segment_path=?1",
                params![f.record.archive_path],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
    #[test]
    fn selected_recovery_refuses_link_escape_and_hardlinks() {
        let f = Fixture::new(true);
        let linked = f.logger.config.archive_root.join("linked.sbcap");
        std::os::unix::fs::symlink(f.path(), &linked).unwrap();
        assert_eq!(
            streaming::hash_file(&linked).unwrap().0,
            streaming::hash_file(f.path()).unwrap().0
        );
        assert!(streaming::hash_file_checked(&linked, |_| Ok(())).is_err());
        assert!(f
            .logger
            .recover_selected_segment_now(&linked, 1024 * 1024, Duration::from_secs(1))
            .is_err());
        let hard = f.root.join("hard.sbcap");
        fs::hard_link(f.path(), &hard).unwrap();
        assert!(f.recover().is_err());
        assert!(!segment_manifest_path(f.path()).exists());
    }
    #[test]
    fn selected_recovery_refuses_foreign_namespace_and_custody() {
        let f = Fixture::new(true);
        let foreign = f.root.join("foreign.sbcap");
        fs::write(&foreign, b"preserve me").unwrap();
        assert!(f
            .logger
            .recover_selected_segment_now(&foreign, 1024 * 1024, Duration::from_secs(1))
            .is_err());
        let custody = PathBuf::from(format!("{}.custody.json", f.path().display()));
        fs::write(&custody,br#"{"schema":"switchback/segment-custody@1","segment_file":"unrelated.sbcap","complete":true,"owners":["/synthetic/state"]}"#).unwrap();
        assert!(f.recover().is_err());
        assert_eq!(fs::read(foreign).unwrap(), b"preserve me");
    }
    #[test]
    fn selected_recovery_rejects_corruption_without_publishing_seal() {
        let f = Fixture::new(true);
        let mut bytes = fs::read(f.path()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        fs::write(f.path(), &bytes).unwrap();
        assert!(f.recover().is_err());
        assert_eq!(fs::read(f.path()).unwrap(), bytes);
        assert!(!segment_manifest_path(f.path()).exists());
    }
    #[test]
    fn selected_recovery_preserves_partial_headers_and_payloads_byte_for_byte() {
        let f = Fixture::new(true);
        let original = fs::read(f.path()).unwrap();
        let frame = &original[CAPTURE_SEGMENT_MAGIC.len()..];
        let partial_payload =
            &frame[..(CAPTURE_RECORD_HEADER_BYTES as usize + 2).min(frame.len() - 1)];
        for tail in [b"partial".as_slice(), partial_payload] {
            let mut evidence = original.clone();
            evidence.extend_from_slice(tail);
            fs::write(f.path(), &evidence).unwrap();
            for _ in 0..2 {
                assert!(f.recover().is_err());
                assert_eq!(fs::read(f.path()).unwrap(), evidence);
                assert!(!segment_manifest_path(f.path()).exists());
            }
        }
    }

    #[test]
    fn selected_recovery_refuses_byte_and_time_budgets_before_mutation() {
        let f = Fixture::new(true);
        let before = fs::read(f.path()).unwrap();
        assert!(f
            .logger
            .recover_selected_segment_now(f.path(), 1, Duration::from_secs(1))
            .is_err());
        assert!(f
            .logger
            .recover_selected_segment_now(f.path(), 1024, Duration::ZERO)
            .is_err());
        assert!(f
            .logger
            .recover_selected_segment_now(f.path(), 1024, Duration::from_secs(121))
            .is_err());
        assert_eq!(fs::read(f.path()).unwrap(), before);
        assert!(!segment_manifest_path(f.path()).exists());
    }
    #[test]
    fn selected_recovery_refuses_busy_operation_lock_without_waiting() {
        let f = Fixture::new(true);
        let _held = backup::backup_operation_lock(&f.logger.config.state_dir).unwrap();
        assert!(f.recover().is_err());
        assert!(!segment_manifest_path(f.path()).exists());
    }
    #[test]
    fn selected_scan_budget_failure_leaves_retryable_native_bytes() {
        let f = Fixture::new(true);
        let before = fs::read(f.path()).unwrap();
        let mut checks = 0;
        let failure = streaming::visit_segment_checked(
            f.path(),
            true,
            |_| {
                checks += 1;
                if checks >= 3 {
                    Err(BodyLogError::new("injected deadline"))
                } else {
                    Ok(())
                }
            },
            |_, _| Ok(()),
        );
        assert!(failure.is_err());
        assert_eq!(fs::read(f.path()).unwrap(), before);
        assert_eq!(f.recover().unwrap().record_count, 1);
    }
}
