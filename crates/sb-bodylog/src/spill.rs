//! Durable HTTP capture scratch under the existing body spool owner.
//! A partial capture is retained as partial. Only ready.json admits full replay.
use super::*;

pub const HTTP_SPILL_MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;
const SCRATCH_QUOTA_BYTES: u64 = 1024 * 1024 * 1024;
const RESERVATION_BYTES: u64 = 66 * 1024 * 1024;
const MAX_PENDING_CAPTURES: usize = 64;
const SCHEMA: &str = "switchback/http-capture-spill@1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Intent {
    schema: String,
    event_id: String,
    observed_at_unix_ms: i64,
    input: BodyEventInput,
    body_bytes: Option<u64>,
    body_sha256: Option<String>,
}

/// This handle owns one bounded reservation. Dropping it preserves the partial.
#[derive(Debug)]
pub struct CaptureSpill {
    descriptor: SpillDescriptor,
    file: fs::File,
    written: u64,
}

#[derive(Debug)]
struct SpillDescriptor {
    directory: PathBuf,
    intent: Intent,
    _lease: fs::File,
}

#[derive(Debug)]
pub struct ReadyCapture {
    descriptor: SpillDescriptor,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureSpillStatus {
    pub partial_captures: u64,
    pub ready_captures: u64,
    pub reserved_bytes: u64,
    pub quota_bytes: u64,
    pub oldest_pending_at_unix_ms: Option<i64>,
    pub invalid_captures: u64,
}

fn owned_file(path: &Path, create: bool) -> Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    set_owner_only(&mut options);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(BodyLogError::new("capture spill requires a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::getuid() } {
            return Err(BodyLogError::new("capture spill owner changed"));
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn directories(root: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name() == "resource.lock" {
            continue;
        }
        if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
            return Err(BodyLogError::new("unexpected capture spill resource entry"));
        }
        result.push(entry.path());
        if result.len() > MAX_PENDING_CAPTURES {
            return Err(BodyLogError::new("capture spill inventory exceeds bound"));
        }
    }
    result.sort();
    Ok(result)
}

fn input_bytes(intent: &Intent) -> Result<Vec<u8>> {
    if !intent.input.body.is_empty() {
        return Err(BodyLogError::new(
            "spill metadata must not contain a body buffer",
        ));
    }
    streaming::metadata_json(intent)
}

fn read_intent(path: &Path) -> Result<Intent> {
    let file = owned_file(path, false)?;
    if file.metadata()?.len() > streaming::IO_CHUNK_BYTES as u64 {
        return Err(BodyLogError::new("capture spill metadata exceeds 64 KiB"));
    }
    let mut bytes = Vec::new();
    file.take(streaming::IO_CHUNK_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > streaming::IO_CHUNK_BYTES {
        return Err(BodyLogError::new("capture spill metadata exceeds 64 KiB"));
    }
    let intent: Intent = serde_json::from_slice(&bytes)?;
    if intent.schema != SCHEMA || !intent.input.body.is_empty() {
        return Err(BodyLogError::new("capture spill intent schema mismatch"));
    }
    Ok(intent)
}

// Fixed, owned temporary names keep every interrupted metadata write inside the
// same reservation. The per-capture lease serializes writers across processes.
fn write_intent(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.len() > streaming::IO_CHUNK_BYTES {
        return Err(BodyLogError::new("capture spill metadata exceeds bound"));
    }
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| BodyLogError::new("capture spill metadata name invalid"))?;
    if !["seed.json", "ready.json", "append.json"].contains(&name) {
        return Err(BodyLogError::new("capture spill metadata target invalid"));
    }
    let temporary = path.with_file_name(format!(".{name}.tmp"));
    let mut file = owned_file(&temporary, true)?;
    file.set_len(0)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_directory(
        path.parent()
            .ok_or_else(|| BodyLogError::new("missing spill parent"))?,
    )
}

fn validate_intent(directory: &Path, intent: &Intent) -> Result<()> {
    if directory.file_name().and_then(OsStr::to_str) != Some(intent.event_id.as_str())
        || intent
            .body_bytes
            .is_none_or(|n| n > HTTP_SPILL_MAX_BODY_BYTES)
        || intent
            .body_sha256
            .as_ref()
            .is_none_or(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(BodyLogError::new("capture spill ready identity mismatch"));
    }
    Ok(())
}

impl CaptureSpill {
    pub(super) fn begin(root: &Path, input: BodyEventInput) -> Result<Self> {
        #[cfg(not(unix))]
        return Err(BodyLogError::new(
            "cross-process capture spill reservations require Unix",
        ));
        let intent = Intent {
            schema: SCHEMA.into(),
            event_id: new_event_id(now_unix_ms()),
            observed_at_unix_ms: now_unix_ms(),
            input,
            body_bytes: None,
            body_sha256: None,
        };
        let seed = input_bytes(&intent)?;
        let quota = owned_file(&root.join("resource.lock"), true)?;
        if !try_lock_file(&quota)? {
            return Err(BodyLogError::new("capture spill resource owner is busy"));
        }
        // Fixed reservations count even empty/crashed partial directories.
        // No disk error, timeout or restart silently acknowledges that evidence.
        let pending = directories(root)?.len();
        if pending >= MAX_PENDING_CAPTURES
            || (pending as u64 + 1) * RESERVATION_BYTES > SCRATCH_QUOTA_BYTES
        {
            return Err(BodyLogError::new("capture spill scratch quota exhausted"));
        }
        let directory = root.join(&intent.event_id);
        fs::create_dir(&directory)?;
        ensure_private_directory(&directory)?;
        let lease = owned_file(&directory.join("active.lock"), true)?;
        if !try_lock_file(&lease)? {
            return Err(BodyLogError::new("new capture spill lease is unavailable"));
        }
        write_intent(&directory.join("seed.json"), &seed)?;
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        set_owner_only(&mut options);
        let file = options.open(directory.join("body.raw"))?;
        file.sync_all()?;
        sync_directory(&directory)?;
        sync_directory(root)?;
        drop(quota);
        Ok(Self {
            descriptor: SpillDescriptor {
                directory,
                intent,
                _lease: lease,
            },
            file,
            written: 0,
        })
    }

    pub fn finish(self, input: BodyEventInput) -> Result<ReadyCapture> {
        self.descriptor.finish(self.file, input)
    }
}

impl Write for CaptureSpill {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.written.saturating_add(bytes.len() as u64) > HTTP_SPILL_MAX_BODY_BYTES {
            return Err(std::io::Error::other(
                "HTTP capture body exceeds 32 MiB spill limit",
            ));
        }
        let count = self.file.write(bytes)?;
        self.written += count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl SpillDescriptor {
    fn finish(mut self, mut file: fs::File, input: BodyEventInput) -> Result<ReadyCapture> {
        if input.request_id != self.intent.input.request_id
            || input.capture_stage != self.intent.input.capture_stage
            || input.protocol != self.intent.input.protocol
        {
            return Err(BodyLogError::new("capture spill provenance changed"));
        }
        if file.metadata()?.len() > HTTP_SPILL_MAX_BODY_BYTES {
            return Err(BodyLogError::new(
                "HTTP capture body exceeds 32 MiB spill limit",
            ));
        }
        let path = self.directory.join("body.raw");
        let path_metadata = fs::symlink_metadata(&path)?;
        if !path_metadata.file_type().is_file() {
            return Err(BodyLogError::new("capture spill body is not regular"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file.metadata()?;
            if (opened.dev(), opened.ino()) != (path_metadata.dev(), path_metadata.ino()) {
                return Err(BodyLogError::new("capture spill body identity changed"));
            }
        }
        file.flush()?;
        file.sync_all()?;
        file.seek(std::io::SeekFrom::Start(0))?;
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0u8; streaming::IO_CHUNK_BYTES];
        let mut bounded = file.take(HTTP_SPILL_MAX_BODY_BYTES + 1);
        loop {
            let count = bounded.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            bytes += count as u64;
        }
        if bytes > HTTP_SPILL_MAX_BODY_BYTES {
            return Err(BodyLogError::new(
                "HTTP capture body exceeds 32 MiB spill limit",
            ));
        }
        self.intent.input = input;
        self.intent.body_bytes = Some(bytes);
        self.intent.body_sha256 = Some(format!("{:x}", hash.finalize()));
        let ready = input_bytes(&self.intent)?;
        write_intent(&self.directory.join("ready.json"), &ready)?;
        sync_directory(&self.directory)?;
        Ok(ReadyCapture { descriptor: self })
    }
}

impl ReadyCapture {
    pub fn event_id(&self) -> &str {
        &self.descriptor.intent.event_id
    }
    pub fn body_bytes(&self) -> u64 {
        self.descriptor.intent.body_bytes.unwrap_or(0)
    }
    pub fn body_sha256(&self) -> &str {
        self.descriptor.intent.body_sha256.as_deref().unwrap_or("")
    }

    fn open(directory: &Path, logger: &BodyLogger) -> Result<Option<Self>> {
        let quota = owned_file(&logger.capture_spill_root().join("resource.lock"), false)?;
        if !try_lock_file(&quota)? {
            return Ok(None);
        }
        // Admission and cleanup also hold the resource lock. It is now safe to
        // recreate a missing lease left by an interrupted committed cleanup.
        let lease = owned_file(&directory.join("active.lock"), true)?;
        if !try_lock_file(&lease)? {
            return Ok(None);
        }
        drop(quota);
        let ready_path = directory.join("ready.json");
        if !ready_path.try_exists()? {
            // A ready write may have been synced immediately before a crash
            // interrupted rename. Never downgrade valid completion evidence.
            let staged = directory.join(".ready.json.tmp");
            if staged.try_exists()? {
                if let Ok(intent) = read_intent(&staged) {
                    validate_intent(directory, &intent)?;
                    fs::rename(&staged, &ready_path)?;
                    sync_directory(directory)?;
                }
            }
        }
        if !ready_path.try_exists()? {
            let id = directory
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| BodyLogError::new("capture spill directory identity invalid"))?;
            let conn = read_only_index_connection(&logger.index_path)?;
            if let Some(record) = existing_event(&conn, id)? {
                // A crash during cleanup can remove every metadata file.
                // Only an indexed event with readable, verified body evidence
                // admits cleanup of that same owned event directory.
                logger.read_record_body_to(&record, &mut std::io::sink())?;
                let intent = Intent {
                    schema: SCHEMA.into(),
                    event_id: record.event_id,
                    observed_at_unix_ms: record.observed_at_unix_ms,
                    body_bytes: Some(record.body_bytes),
                    body_sha256: Some(record.body_sha256),
                    input: BodyEventInput {
                        request_id: record.request_id,
                        capture_stage: serde_json::from_value(serde_json::Value::String(
                            record.capture_stage,
                        ))?,
                        protocol: record.protocol,
                        upstream: record.upstream,
                        model: record.model,
                        status: record.status,
                        content_type: record.content_type,
                        metadata: record.metadata,
                        body: Vec::new(),
                    },
                };
                return Ok(Some(Self {
                    descriptor: SpillDescriptor {
                        directory: directory.into(),
                        intent,
                        _lease: lease,
                    },
                }));
            }
            drop(conn);
            if directory.join("append.json").try_exists()? {
                return Err(BodyLogError::new(
                    "capture append journal lost ready authority",
                ));
            }
            let seed_path = directory.join("seed.json");
            if !seed_path.try_exists()? {
                let staged = directory.join(".seed.json.tmp");
                let intent = read_intent(&staged)?;
                if directory.file_name().and_then(OsStr::to_str) != Some(intent.event_id.as_str()) {
                    return Err(BodyLogError::new("capture spill seed identity mismatch"));
                }
                fs::rename(staged, &seed_path)?;
                sync_directory(directory)?;
            }
            let mut intent = read_intent(&seed_path)?;
            if directory.file_name().and_then(OsStr::to_str) != Some(intent.event_id.as_str())
                || intent.body_bytes.is_some()
                || intent.body_sha256.is_some()
            {
                return Err(BodyLogError::new("capture spill seed identity mismatch"));
            }
            let metadata = if intent.input.metadata.get("capture_metadata").is_some() {
                intent
                    .input
                    .metadata
                    .get_mut("capture_metadata")
                    .expect("checked metadata")
            } else {
                &mut intent.input.metadata
            };
            if !metadata.is_object() {
                *metadata = serde_json::json!({});
            }
            let object = metadata.as_object_mut().expect("object metadata");
            object.insert("transport_complete".into(), false.into());
            object.insert("capture_prefix_only".into(), true.into());
            object.insert("capture_stream_incomplete".into(), true.into());
            object.insert(
                "capture_recovery_reason".into(),
                "abandoned_capture_prefix".into(),
            );
            let descriptor = SpillDescriptor {
                directory: directory.into(),
                intent: intent.clone(),
                _lease: lease,
            };
            let file = owned_file(&directory.join("body.raw"), true)?;
            return descriptor.finish(file, intent.input).map(Some);
        }
        let intent = read_intent(&ready_path)?;
        validate_intent(directory, &intent)?;
        Ok(Some(Self {
            descriptor: SpillDescriptor {
                directory: directory.into(),
                intent,
                _lease: lease,
            },
        }))
    }
}

impl BodyLogger {
    fn capture_spill_root(&self) -> PathBuf {
        self.spool_dir.join("capture-queue")
    }

    pub fn begin_capture_spill(&self, seed: BodyEventInput) -> Result<CaptureSpill> {
        let root = self.capture_spill_root();
        ensure_private_directory_tree(&self.spool_dir, &root)?;
        CaptureSpill::begin(&root, seed)
    }

    pub fn ready_capture_spills(&self, limit: usize) -> Result<Vec<ReadyCapture>> {
        let root = self.capture_spill_root();
        if !root.try_exists()? {
            return Ok(Vec::new());
        }
        let mut ready = Vec::new();
        let mut pending = directories(&root)?;
        // At most fifteen reservations exist. Rotate admission so a held or
        // failing journal cannot starve later captures across maintenance ticks.
        if !pending.is_empty() {
            let offset = (now_unix_ms().div_euclid(1000) as usize) % pending.len();
            pending.rotate_left(offset);
        }
        for directory in pending {
            match ReadyCapture::open(&directory, self) {
                Ok(Some(capture)) => {
                    ready.push(capture);
                    if ready.len() >= limit.clamp(1, 2) {
                        break;
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, "HTTP spill recovery held invalid journal"),
            }
        }
        Ok(ready)
    }

    pub fn capture_spill_status(&self) -> Result<CaptureSpillStatus> {
        CaptureSpillStatus::for_root(&self.capture_spill_root())
    }

    /// Retry a bounded batch through the same append/index owner. Remaining
    /// journals are durable and visible in status, including invalid evidence.
    pub fn recover_capture_spills(&self) -> Result<u64> {
        let mut recovered = 0;
        for capture in self.ready_capture_spills(2)? {
            match self.record_capture_spill(&capture) {
                Ok(_) => recovered += 1,
                Err(error) => tracing::warn!(event_id = capture.event_id(), %error,
                    "durable HTTP capture replay held"),
            }
        }
        Ok(recovered)
    }
}

impl CaptureSpillStatus {
    pub(super) fn for_root(root: &Path) -> Result<Self> {
        let mut status = Self {
            partial_captures: 0,
            ready_captures: 0,
            reserved_bytes: 0,
            quota_bytes: SCRATCH_QUOTA_BYTES,
            oldest_pending_at_unix_ms: None,
            invalid_captures: 0,
        };
        if !root.try_exists()? {
            return Ok(status);
        }
        for directory in directories(root)? {
            let ready = directory.join("ready.json");
            let metadata = if ready.try_exists()? {
                status.ready_captures += 1;
                read_intent(&ready).and_then(|intent| {
                    validate_intent(&directory, &intent)?;
                    Ok(intent)
                })
            } else {
                status.partial_captures += 1;
                read_intent(&directory.join("seed.json"))
            };
            match metadata {
                Ok(intent) => {
                    status.oldest_pending_at_unix_ms = Some(
                        status
                            .oldest_pending_at_unix_ms
                            .map_or(intent.observed_at_unix_ms, |old| {
                                old.min(intent.observed_at_unix_ms)
                            }),
                    )
                }
                Err(_) => status.invalid_captures += 1,
            }
            status.reserved_bytes += RESERVATION_BYTES;
        }
        Ok(status)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpillCheckpoint {
    BeforeAppend,
    AfterAppendChunk,
    AfterAppend,
    AfterIndexCommit,
}

#[derive(Serialize, Deserialize)]
struct AppendIntent {
    schema: String,
    record: BodyRecord,
    frame_start: u64,
    frame_bytes: u64,
}

fn existing_event(conn: &Connection, id: &str) -> Result<Option<BodyRecord>> {
    Ok(query_records(conn, "WHERE event_id = ?1", params![id, 1i64])?.pop())
}

fn same_capture(expected: &BodyRecord, actual: &BodyRecord) -> bool {
    recovery_identity_divergence(expected, actual).is_empty()
        && expected.event_id == actual.event_id
        && expected.status == actual.status
        && expected.upstream == actual.upstream
        && [
            "native_execution",
            "path",
            "selected_upstream",
            "response_origin",
            "transport_complete",
        ]
        .iter()
        .all(|key| {
            let evidence =
                |m: &serde_json::Value| m.get("capture_metadata").unwrap_or(m).get(key).cloned();
            evidence(&expected.metadata) == evidence(&actual.metadata)
        })
}

impl ReadyCapture {
    fn expected_record(&self, logger: &BodyLogger) -> BodyRecord {
        let intent = &self.descriptor.intent;
        let input = &intent.input;
        BodyRecord {
            event_id: intent.event_id.clone(),
            request_id: input.request_id.clone(),
            observed_at_unix_ms: intent.observed_at_unix_ms,
            capture_stage: input.capture_stage.as_str().into(),
            protocol: input.protocol.clone(),
            upstream: input.upstream.clone(),
            model: input.model.clone(),
            status: input.status,
            content_type: input.content_type.clone(),
            body_sha256: self.body_sha256().into(),
            body_bytes: self.body_bytes(),
            compressed_bytes: 0,
            archive_path: String::new(),
            storage: String::new(),
            protected: true,
            redaction_state: "raw_local".into(),
            threshold_shrunk: self.body_bytes() > logger.config.inline_threshold_bytes,
            metadata: input.metadata.clone(),
        }
    }

    fn validate_root(&self, logger: &BodyLogger) -> Result<()> {
        let directory = &self.descriptor.directory;
        let metadata = match fs::symlink_metadata(directory) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if metadata.is_some_and(|metadata| !metadata.is_dir() || metadata.file_type().is_symlink())
            || directory.file_name().and_then(OsStr::to_str) != Some(self.event_id())
            || fs::canonicalize(
                directory
                    .parent()
                    .ok_or_else(|| BodyLogError::new("spill has no parent"))?,
            )? != fs::canonicalize(logger.capture_spill_root())?
        {
            return Err(BodyLogError::new("capture spill belongs to another store"));
        }
        Ok(())
    }

    fn cleanup(&self, logger: &BodyLogger) -> Result<()> {
        self.validate_root(logger)?;
        if !self.descriptor.directory.try_exists()? {
            return Ok(());
        }
        let quota = owned_file(&logger.capture_spill_root().join("resource.lock"), false)?;
        if !try_lock_file(&quota)? {
            return Err(BodyLogError::new(
                "capture spill cleanup resource owner is busy",
            ));
        }
        let allowed = [
            "active.lock",
            "body.raw",
            "seed.json",
            "ready.json",
            "append.json",
            "frame.tmp",
            ".seed.json.tmp",
            ".ready.json.tmp",
            ".append.json.tmp",
        ];
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.descriptor.directory)? {
            let entry = entry?;
            if !allowed.iter().any(|name| entry.file_name() == *name)
                || !entry.file_type()?.is_file()
                || entry.file_type()?.is_symlink()
            {
                return Err(BodyLogError::new(
                    "capture spill cleanup preserved an unexpected artifact",
                ));
            }
            files.push(entry.path());
        }
        for file in files {
            fs::remove_file(file)?;
        }
        fs::remove_dir(&self.descriptor.directory)?;
        sync_directory(&logger.capture_spill_root())?;
        Ok(())
    }
}

impl BodyLogger {
    fn replay_spill_append(
        &self,
        capture: &ReadyCapture,
        writer: &mut SegmentWriterState,
    ) -> Result<Option<BodyRecord>> {
        let journal = capture.descriptor.directory.join("append.json");
        if !journal.try_exists()? {
            return Ok(None);
        }
        let mut file = owned_file(&journal, false)?;
        if file.metadata()?.len() > streaming::IO_CHUNK_BYTES as u64 {
            return Err(BodyLogError::new(
                "capture spill append journal exceeds bound",
            ));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(streaming::IO_CHUNK_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let append: AppendIntent = serde_json::from_slice(&bytes)?;
        let expected = capture.expected_record(self);
        if append.schema != "switchback/http-capture-append@1"
            || !same_capture(&expected, &append.record)
            || append.frame_bytes > HTTP_SPILL_MAX_BODY_BYTES + 1024 * 1024
        {
            return Err(BodyLogError::new(
                "capture spill append journal identity changed",
            ));
        }
        let path = Path::new(&append.record.archive_path);
        let meta = fs::symlink_metadata(path)?;
        if !meta.file_type().is_file() || meta.len() < append.frame_start {
            return Err(BodyLogError::new("capture spill append target changed"));
        }
        let canonical = fs::canonicalize(path)?;
        let roots = [
            self.config.archive_root.clone(),
            self.spool_dir.join("segments"),
        ];
        if !roots
            .iter()
            .any(|root| fs::canonicalize(root).is_ok_and(|root| canonical.starts_with(root)))
        {
            return Err(BodyLogError::new(
                "capture spill append target escaped store roots",
            ));
        }
        let owner = fs::canonicalize(&self.config.state_dir)?;
        if !custody::read(path)?.is_some_and(|custody| custody.owners.contains(&owner)) {
            return Err(BodyLogError::new(
                "capture spill append target lacks this store's custody",
            ));
        }
        let held_by_writer = writer
            .active
            .as_ref()
            .is_some_and(|active| active.path == path);
        let _lease = if held_by_writer {
            None
        } else {
            Some(
                try_acquire_segment_lock(path)?
                    .ok_or_else(|| BodyLogError::new("capture spill append target still active"))?,
            )
        };
        let recover_tail = !segment_manifest_path(path).try_exists()?;
        let mut found = None;
        streaming::visit_segment(path, recover_tail, |record, _| {
            if record.event_id == capture.event_id() {
                if found.is_some() || !same_capture(&expected, record) {
                    return Err(BodyLogError::new("capture spill append event collision"));
                }
                found = Some(record.clone());
            }
            Ok(())
        })?;
        let Some(mut record) = found else {
            return Ok(None);
        };
        record.archive_path = path.to_string_lossy().into_owned();
        let mut conn = open_index_connection(&self.index_path)?;
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let already_indexed = existing_event(&transaction, &record.event_id)?.is_some();
        insert_recovered_record_on(&transaction, &record)?;
        if !already_indexed {
            upsert_segment_projection_on(&transaction, &record)?;
        }
        transaction.commit()?;
        Ok(Some(record))
    }

    /// Publish a durable ready capture with its original event identity. Failed
    /// appends remain journaled; replay proves the old target before any retry.
    pub fn record_capture_spill(&self, capture: &ReadyCapture) -> Result<BodyRecord> {
        self.record_capture_spill_inner(capture, |_| Ok(()))
    }

    fn record_capture_spill_inner(
        &self,
        capture: &ReadyCapture,
        mut checkpoint: impl FnMut(SpillCheckpoint) -> Result<()>,
    ) -> Result<BodyRecord> {
        capture.validate_root(self)?;
        let expected = capture.expected_record(self);
        {
            let conn = read_only_index_connection(&self.index_path)?;
            if let Some(existing) = existing_event(&conn, capture.event_id())? {
                if !same_capture(&expected, &existing) {
                    return Err(BodyLogError::new("capture spill indexed event collision"));
                }
                self.read_record_body_to(&existing, &mut std::io::sink())?;
                capture.cleanup(self)?;
                return Ok(existing);
            }
        }
        let mut writer = self
            .segment_writer
            .lock()
            .map_err(|_| BodyLogError::new("capture segment writer lock poisoned"))?;
        if let Some(record) = self.replay_spill_append(capture, &mut writer)? {
            drop(writer);
            capture.cleanup(self)?;
            return Ok(record);
        }
        let mut source = owned_file(&capture.descriptor.directory.join("body.raw"), false)?;
        if source.metadata()?.len() != capture.body_bytes() {
            return Err(BodyLogError::new("capture spill body length changed"));
        }
        let mut frame = owned_file(&capture.descriptor.directory.join("frame.tmp"), true)?;
        let mut record = expected;
        let storage = if archive_root_available(&self.config.archive_root) {
            "archive_segment"
        } else {
            "spool_segment"
        };
        let bucket = record
            .observed_at_unix_ms
            .div_euclid(CAPTURE_SEGMENT_ROTATE_MS)
            * CAPTURE_SEGMENT_ROTATE_MS;
        if !writer
            .active
            .as_ref()
            .is_some_and(|active| active.storage == storage && active.bucket_start_ms == bucket)
        {
            if let Some(active) = writer.active.take() {
                seal_active_segment(active)?;
            }
            writer.active = Some(self.create_segment(storage, bucket)?);
        }
        let active = writer
            .active
            .as_ref()
            .ok_or_else(|| BodyLogError::new("capture segment was not created"))?;
        record.archive_path = active.path.to_string_lossy().into_owned();
        record.storage = storage.into();
        let mut frame_bytes =
            streaming::encode_frame(&mut record, &mut source, capture.body_bytes(), &mut frame)?;
        let existing_bytes = fs::metadata(&active.path)?.len();
        if segment_would_exceed_limit(existing_bytes, frame_bytes, CAPTURE_SEGMENT_MAX_BYTES) {
            let active = writer
                .active
                .take()
                .ok_or_else(|| BodyLogError::new("capture segment was not created"))?;
            seal_active_segment(active)?;
            let active = self.create_segment(storage, bucket)?;
            record.archive_path = active.path.to_string_lossy().into_owned();
            frame_bytes = streaming::encode_frame(
                &mut record,
                &mut source,
                capture.body_bytes(),
                &mut frame,
            )?;
            writer.active = Some(active);
        }
        let path = Path::new(&record.archive_path);
        let frame_start = fs::metadata(path)?.len();
        let append = AppendIntent {
            schema: "switchback/http-capture-append@1".into(),
            record: record.clone(),
            frame_start,
            frame_bytes,
        };
        write_intent(
            &capture.descriptor.directory.join("append.json"),
            &streaming::metadata_json(&append)?,
        )?;
        let mut conn = open_index_connection(&self.index_path)?;
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut options = OpenOptions::new();
        options.append(true).read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        checkpoint(SpillCheckpoint::BeforeAppend)?;
        let mut segment = options.open(path)?;
        if !segment.metadata()?.is_file() || segment.metadata()?.len() != frame_start {
            return Err(BodyLogError::new("capture segment changed at spill append"));
        }
        frame.seek(std::io::SeekFrom::Start(0))?;
        let mut buffer = [0u8; streaming::IO_CHUNK_BYTES];
        let mut copied = 0u64;
        loop {
            let count = frame.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            segment.write_all(&buffer[..count])?;
            copied += count as u64;
            checkpoint(SpillCheckpoint::AfterAppendChunk)?;
        }
        if copied != frame_bytes {
            return Err(BodyLogError::new("capture spill frame length changed"));
        }
        segment.sync_all()?;
        if let Some(active) = writer.active.as_mut() {
            active.last_write_at_unix_ms = record.observed_at_unix_ms;
        }
        checkpoint(SpillCheckpoint::AfterAppend)?;
        insert_record_on(&transaction, &record)?;
        upsert_segment_projection_on(&transaction, &record)?;
        transaction.commit()?;
        checkpoint(SpillCheckpoint::AfterIndexCommit)?;
        drop(segment);
        drop(frame);
        drop(source);
        drop(writer);
        self.pressure
            .lock()
            .map_err(|_| BodyLogError::new("capture pressure lock poisoned"))?
            .note_full_capture(record.body_bytes);
        capture.cleanup(self)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> BodyEventInput {
        BodyEventInput {
            request_id: "spill-interruption".into(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "http".into(),
            upstream: Some("https://upstream.invalid".into()),
            model: None,
            status: Some(200),
            content_type: None,
            metadata: serde_json::json!({"transport_complete":true}),
            body: Vec::new(),
        }
    }

    #[test]
    fn replay_contains_partial_append_full_append_and_committed_index_ambiguity() {
        for stop in [
            SpillCheckpoint::AfterAppendChunk,
            SpillCheckpoint::AfterAppend,
            SpillCheckpoint::AfterIndexCommit,
        ] {
            let root = std::env::temp_dir().join(format!(
                "sb-spill-cut-{}-{}-{stop:?}",
                std::process::id(),
                now_unix_ms()
            ));
            let logger = BodyLogger::new(BodyLoggerConfig {
                state_dir: root.join("state"),
                archive_root: root.join("archive"),
                legacy_jsonl: None,
                inline_threshold_bytes: 256 * 1024,
            })
            .unwrap();
            let mut spill = logger.begin_capture_spill(seed()).unwrap();
            let mut remaining = 19_361_818usize;
            let mut state = 0x1234_98fe_0921_ab78u64;
            let mut buffer = [0u8; streaming::IO_CHUNK_BYTES];
            while remaining != 0 {
                let count = remaining.min(buffer.len());
                for byte in &mut buffer[..count] {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = state as u8;
                }
                spill.write_all(&buffer[..count]).unwrap();
                remaining -= count;
            }
            let ready = spill.finish(seed()).unwrap();
            let interrupted = logger.record_capture_spill_inner(&ready, |point| {
                if point == stop {
                    Err(BodyLogError::new("synthetic interrupted publication"))
                } else {
                    Ok(())
                }
            });
            assert!(interrupted.is_err(), "{stop:?}");
            assert_eq!(logger.capture_spill_status().unwrap().ready_captures, 1);
            let record = logger.record_capture_spill(&ready).unwrap();
            assert_eq!(record.event_id, ready.event_id());
            assert_eq!(record.body_bytes, 19_361_818);
            assert_eq!(
                logger
                    .events_for_request("spill-interruption")
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                streaming::segment_summary(Path::new(&record.archive_path), false)
                    .unwrap()
                    .record_count,
                1
            );
            assert_eq!(
                logger.record_capture_spill(&ready).unwrap().event_id,
                record.event_id,
                "a lost success response is idempotent"
            );
            assert_eq!(logger.capture_spill_status().unwrap().reserved_bytes, 0);
            drop(ready);
            drop(logger);
            fs::remove_dir_all(root).unwrap();
        }
    }
    fn crash_config(root: &Path) -> BodyLoggerConfig {
        BodyLoggerConfig {
            state_dir: root.join("state"),
            archive_root: root.join("archive"),
            legacy_jsonl: None,
            inline_threshold_bytes: 256 * 1024,
        }
    }

    #[test]
    fn spill_crash_child() {
        let Some(root) = std::env::var_os("SB_SPILL_CRASH_ROOT") else {
            return;
        };
        let logger = BodyLogger::new(crash_config(Path::new(&root))).unwrap();
        let mut capture = logger.begin_capture_spill(seed()).unwrap();
        let mut random = 0x9754_987a_5b65_2acdu64;
        let mut buffer = [0u8; 64 * 1024];
        for _ in 0..32 {
            for byte in &mut buffer {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                *byte = random as u8;
            }
            capture.write_all(&buffer).unwrap();
        }
        let ready = capture.finish(seed()).unwrap();
        fs::write(Path::new(&root).join("identity"), ready.event_id()).unwrap();
        let stop = std::env::var("SB_SPILL_CRASH_POINT").unwrap();
        logger
            .record_capture_spill_inner(&ready, |point| {
                if format!("{point:?}") == stop {
                    std::process::exit(86);
                }
                Ok(())
            })
            .unwrap();
        panic!("crash checkpoint was not reached");
    }

    #[test]
    fn process_exit_replays_each_publication_boundary_with_one_identity_and_frame() {
        for point in [
            "BeforeAppend",
            "AfterAppendChunk",
            "AfterAppend",
            "AfterIndexCommit",
        ] {
            let root = std::env::temp_dir().join(format!(
                "sb-spill-process-{}-{}-{point}",
                std::process::id(),
                now_unix_ms()
            ));
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "spill::tests::spill_crash_child", "--nocapture"])
                .env("SB_SPILL_CRASH_ROOT", &root)
                .env("SB_SPILL_CRASH_POINT", point)
                .output()
                .unwrap();
            assert_eq!(child.status.code(), Some(86), "{point}: {child:?}");
            let identity = fs::read_to_string(root.join("identity")).unwrap();
            let logger = BodyLogger::new(crash_config(&root)).unwrap();
            let ready = logger.ready_capture_spills(2).unwrap();
            assert_eq!(ready.len(), 1, "{point}");
            assert_eq!(ready[0].event_id(), identity);
            let record = logger.record_capture_spill(&ready[0]).unwrap();
            assert_eq!(record.event_id, identity);
            assert_eq!(record.body_bytes, 2 * 1024 * 1024);
            logger
                .read_record_body_to(&record, &mut std::io::sink())
                .unwrap();
            assert_eq!(
                logger
                    .events_for_request("spill-interruption")
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                streaming::segment_summary(Path::new(&record.archive_path), false)
                    .unwrap()
                    .record_count,
                1
            );
            drop(ready);
            drop(logger);
            fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn committed_cleanup_recovers_after_metadata_and_lease_unlinks() {
        for empty in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "sb-spill-cleanup-{}-{}-{empty}",
                std::process::id(),
                now_unix_ms()
            ));
            let logger = BodyLogger::new(crash_config(&root)).unwrap();
            let mut spill = logger.begin_capture_spill(seed()).unwrap();
            spill.write_all(b"committed evidence").unwrap();
            let ready = spill.finish(seed()).unwrap();
            let id = ready.event_id().to_string();
            assert!(logger
                .record_capture_spill_inner(&ready, |point| {
                    if point == SpillCheckpoint::AfterIndexCommit {
                        Err(BodyLogError::new("interrupted cleanup"))
                    } else {
                        Ok(())
                    }
                })
                .is_err());
            let directory = ready.descriptor.directory.clone();
            drop(ready);
            for name in ["ready.json", "body.raw", "append.json", "frame.tmp"] {
                let path = directory.join(name);
                if path.exists() {
                    fs::remove_file(path).unwrap();
                }
            }
            if empty {
                fs::remove_file(directory.join("seed.json")).unwrap();
                fs::remove_file(directory.join("active.lock")).unwrap();
            }
            assert_eq!(logger.recover_capture_spills().unwrap(), 1);
            assert!(!directory.exists());
            let events = logger.events_for_request("spill-interruption").unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_id, id);
            assert_eq!(
                logger.read_record_body(&events[0]).unwrap(),
                b"committed evidence"
            );
            logger.seal_active().unwrap();
            drop(logger);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn repeated_failed_metadata_rename_cannot_accumulate_temporary_files() {
        let root = std::env::temp_dir().join(format!(
            "sb-spill-temp-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        fs::create_dir(&root).unwrap();
        let target = root.join("append.json");
        fs::create_dir(&target).unwrap();
        let data = vec![b'x'; streaming::IO_CHUNK_BYTES];
        for _ in 0..100 {
            assert!(write_intent(&target, &data).is_err());
            let files: Vec<_> = fs::read_dir(&root).unwrap().collect();
            assert_eq!(files.len(), 2);
            assert_eq!(
                fs::metadata(root.join(".append.json.tmp")).unwrap().len(),
                streaming::IO_CHUNK_BYTES as u64
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
