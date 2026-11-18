//! Generated media (job artifacts) with a bounded footprint.
//!
//! Bodies live on disk under the artifact directory, content addressed by the
//! sha256 the record already carries (`blobs/sha256/<ab>/<sha256>`), with one
//! metadata file per artifact (`records/<artifact_id>.json`) so artifacts
//! survive a restart. A deployment with no writable state directory keeps
//! them in process memory instead. Either way a retention policy runs when an
//! artifact is written and at start-up: artifacts past the age cap go, then
//! the oldest go until the total is under the size cap.
//!
//! Metadata only: a record holds the [`ArtifactRecord`] and the tenant and
//! project that may read it. No prompt, graph or credential is written here.
//! Directories are created `0700` and files `0600` (Unix).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use sb_core::{ArtifactRecord, ArtifactStorage, ArtifactsConfig};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How many removed artifact ids are remembered, so a fetch of one answers
/// "evicted" instead of "not found" (process lifetime only).
const KEPT_TOMBSTONES: usize = 4096;

/// The principal allowed to read an artifact: the tenant and project of the
/// job that made it. Both `None` for an unattributed (single-key or open)
/// gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

impl Owner {
    pub fn new(tenant: Option<String>, project: Option<String>) -> Self {
        Self { tenant, project }
    }
}

/// Size and age caps, applied oldest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub max_bytes: u64,
    pub max_age_ms: u64,
}

impl RetentionPolicy {
    pub fn from_config(cfg: &ArtifactsConfig) -> Self {
        Self {
            max_bytes: cfg.effective_max_bytes(),
            max_age_ms: cfg.max_age_secs.saturating_mul(1000),
        }
    }
}

#[derive(Debug)]
pub enum ArtifactError {
    /// The artifact alone is larger than the store's size cap.
    TooLarge {
        max_bytes: u64,
    },
    Io(String),
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { max_bytes } => write!(
                f,
                "artifact is larger than the artifact store's size cap of {max_bytes} bytes (server.artifacts.max_bytes)"
            ),
            Self::Io(message) => write!(f, "artifact store: {message}"),
        }
    }
}

/// Why a fetch found no artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    NotFound,
    /// Removed by the retention policy; carries the owner so a caller that
    /// may not read it still gets a plain "not found".
    Evicted(Owner),
}

/// An artifact's body, ready to stream.
pub enum ArtifactBody {
    File(std::fs::File),
    Memory(Arc<Vec<u8>>),
}

/// One artifact found by [`ArtifactStore::open`].
pub struct OpenedArtifact {
    pub record: ArtifactRecord,
    pub owner: Owner,
    pub body: ArtifactBody,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredMeta {
    record: ArtifactRecord,
    #[serde(default)]
    owner: Owner,
}

struct Entry {
    meta: StoredMeta,
    /// Write order, the tie-break for artifacts made in the same millisecond.
    seq: u64,
}

struct Blob {
    refs: u32,
    bytes: u64,
    /// The body, in memory mode.
    body: Option<Arc<Vec<u8>>>,
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Entry>,
    blobs: HashMap<String, Blob>,
    total_bytes: u64,
    next_seq: u64,
    tombstones: HashMap<String, Owner>,
    tombstone_order: VecDeque<String>,
}

enum Backend {
    Disk { root: PathBuf },
    Memory,
}

/// The artifact store. Cheap to clone; clones share one index.
#[derive(Clone)]
pub struct ArtifactStore {
    index: Arc<Mutex<Index>>,
    backend: Arc<Backend>,
    policy: RetentionPolicy,
}

impl ArtifactStore {
    /// A store in process memory with `policy`.
    pub fn memory(policy: RetentionPolicy) -> Self {
        Self {
            index: Arc::default(),
            backend: Arc::new(Backend::Memory),
            policy,
        }
    }

    /// The store `server.artifacts` asks for. `default_root` is the runtime
    /// state directory, used when `dir` is unset; without either (a library
    /// caller such as a test) the store is in memory. A disk store that
    /// cannot be opened falls back to memory with a warning: generated media
    /// then still works, capped, rather than failing the gateway's start.
    pub fn from_config(cfg: &ArtifactsConfig, default_root: Option<&Path>) -> Self {
        let policy = RetentionPolicy::from_config(cfg);
        let memory_policy = RetentionPolicy {
            max_bytes: policy
                .max_bytes
                .min(sb_core::DEFAULT_ARTIFACT_MEMORY_MAX_BYTES),
            ..policy
        };
        if cfg.storage == ArtifactStorage::Memory {
            return Self::memory(policy);
        }
        let dir = match (&cfg.dir, default_root) {
            (Some(dir), _) => PathBuf::from(dir),
            (None, Some(root)) => root.join("artifacts"),
            (None, None) => return Self::memory(memory_policy),
        };
        match Self::open_disk(&dir, policy) {
            Ok(store) => {
                tracing::info!(dir = %dir.display(), max_bytes = policy.max_bytes, max_age_secs = cfg.max_age_secs, "artifact store on disk");
                store
            }
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, max_bytes = memory_policy.max_bytes, "artifact store directory unusable; keeping artifacts in process memory");
                Self::memory(memory_policy)
            }
        }
    }

    /// Open (or create) a disk store at `root`: drop half-written files,
    /// load the records, delete bodies no record names, and apply the
    /// retention policy.
    pub fn open_disk(root: &Path, policy: RetentionPolicy) -> std::io::Result<Self> {
        for dir in [
            root.to_path_buf(),
            root.join("blobs"),
            root.join("blobs").join("sha256"),
            root.join("records"),
            root.join("jobs"),
            root.join("tmp"),
        ] {
            create_private_dir(&dir)?;
        }
        for entry in std::fs::read_dir(root.join("tmp"))? {
            let path = entry?.path();
            if path.is_file() {
                remove_quietly(&path);
            }
        }
        let mut loaded = Vec::new();
        for entry in std::fs::read_dir(root.join("records"))? {
            let path = entry?.path();
            match load_record(root, &path) {
                Some((meta, bytes)) => loaded.push((meta, bytes)),
                None => {
                    tracing::warn!(file = %path.display(), "artifact record unreadable or its body is missing; removed");
                    remove_quietly(&path);
                }
            }
        }
        loaded.sort_by(|(a, _), (b, _)| {
            (a.record.created_at_ms, &a.record.artifact_id)
                .cmp(&(b.record.created_at_ms, &b.record.artifact_id))
        });
        let mut index = Index::default();
        for (meta, bytes) in loaded {
            index.add(meta, bytes, None);
        }
        let blobs_root = root.join("blobs").join("sha256");
        for shard in std::fs::read_dir(&blobs_root)? {
            let shard = shard?.path();
            if !shard.is_dir() {
                continue;
            }
            for blob in std::fs::read_dir(&shard)? {
                let blob = blob?.path();
                let known = blob
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| index.blobs.contains_key(name));
                if !known {
                    remove_quietly(&blob);
                }
            }
        }
        let store = Self {
            index: Arc::new(Mutex::new(index)),
            backend: Arc::new(Backend::Disk {
                root: root.to_path_buf(),
            }),
            policy,
        };
        {
            let mut index = store.lock();
            let removed = index.enforce(&store.policy, now_ms());
            store.unlink(removed);
        }
        Ok(store)
    }

    /// `disk` or `process_memory`: the `retention` label on each record.
    pub fn storage_label(&self) -> &'static str {
        match self.backend.as_ref() {
            Backend::Disk { .. } => "disk",
            Backend::Memory => "process_memory",
        }
    }

    pub fn policy(&self) -> RetentionPolicy {
        self.policy
    }

    /// Bytes held now (each distinct body counted once).
    pub fn total_bytes(&self) -> u64 {
        self.lock().total_bytes
    }

    /// Start writing one artifact. `record` carries everything but the
    /// body's size, digest and storage reference, which [`ArtifactWriter::finish`]
    /// fills in.
    pub fn writer(
        &self,
        record: ArtifactRecord,
        owner: Owner,
    ) -> Result<ArtifactWriter, ArtifactError> {
        let sink = match self.backend.as_ref() {
            Backend::Disk { root } => {
                let path = root
                    .join("tmp")
                    .join(format!("{}.part", sb_core::new_id("w")));
                let file =
                    create_private_file(&path).map_err(|e| ArtifactError::Io(e.to_string()))?;
                Sink::File {
                    file,
                    path: Some(path),
                }
            }
            Backend::Memory => Sink::Memory(Vec::new()),
        };
        Ok(ArtifactWriter {
            store: self.clone(),
            record,
            owner,
            hasher: Sha256::new(),
            bytes: 0,
            sink,
        })
    }

    /// Store a body that is already in memory.
    pub fn put_bytes(
        &self,
        record: ArtifactRecord,
        owner: Owner,
        bytes: &[u8],
    ) -> Result<ArtifactRecord, ArtifactError> {
        let mut writer = self.writer(record, owner)?;
        writer.write(bytes)?;
        writer.finish()
    }

    /// The record and owner of a stored artifact.
    pub fn record(&self, id: &str) -> Result<(ArtifactRecord, Owner), Missing> {
        let index = self.lock();
        match index.entries.get(id) {
            Some(entry) => Ok((entry.meta.record.clone(), entry.meta.owner.clone())),
            None => Err(index.missing(id)),
        }
    }

    /// Open an artifact for reading. A body removed between the lookup and
    /// the open reads as evicted.
    pub fn open(&self, id: &str) -> Result<OpenedArtifact, Missing> {
        let (meta, body) = {
            let index = self.lock();
            let Some(entry) = index.entries.get(id) else {
                return Err(index.missing(id));
            };
            let meta = entry.meta.clone();
            let body = index
                .blobs
                .get(&meta.record.sha256)
                .and_then(|blob| blob.body.clone());
            (meta, body)
        };
        let body = match (self.backend.as_ref(), body) {
            (Backend::Memory, Some(body)) => ArtifactBody::Memory(body),
            (Backend::Memory, None) => return Err(Missing::Evicted(meta.owner)),
            (Backend::Disk { root }, _) => {
                match std::fs::File::open(blob_path(root, &meta.record.sha256)) {
                    Ok(file) => ArtifactBody::File(file),
                    Err(_) => return Err(Missing::Evicted(meta.owner)),
                }
            }
        };
        Ok(OpenedArtifact {
            record: meta.record,
            owner: meta.owner,
            body,
        })
    }

    /// Read a whole artifact into memory (inputs uploaded to a provider).
    pub fn read_all(&self, id: &str) -> Result<(ArtifactRecord, Owner, Vec<u8>), Missing> {
        let opened = self.open(id)?;
        let bytes = match opened.body {
            ArtifactBody::Memory(body) => body.as_ref().clone(),
            ArtifactBody::File(mut file) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|_| Missing::Evicted(opened.owner.clone()))?;
                bytes
            }
        };
        Ok((opened.record, opened.owner, bytes))
    }

    /// Keep a job record next to the artifacts (disk store only), so a
    /// restart knows which jobs did not finish. Metadata only: a job record
    /// holds no prompt, graph or credential. A failed write is logged, not
    /// fatal: the job still runs.
    pub fn persist_job(&self, job: &sb_core::JobRecord) {
        let Backend::Disk { root } = self.backend.as_ref() else {
            return;
        };
        if !is_safe_id(&job.id) {
            return;
        }
        let written = serde_json::to_vec(job)
            .map_err(std::io::Error::other)
            .and_then(|body| {
                let tmp = root.join("tmp").join(format!("{}.job.part", job.id));
                let target = root.join("jobs").join(format!("{}.json", job.id));
                let result = create_private_file(&tmp)
                    .and_then(|mut file| file.write_all(&body))
                    .and_then(|()| std::fs::rename(&tmp, &target));
                if result.is_err() {
                    remove_quietly(&tmp);
                }
                result
            });
        if let Err(error) = written {
            tracing::warn!(job = %job.id, %error, "job record not written to the artifact store");
        }
    }

    /// Forget a job record (disk store only).
    pub fn forget_job(&self, id: &str) {
        if let Backend::Disk { root } = self.backend.as_ref() {
            if is_safe_id(id) {
                remove_quietly(&root.join("jobs").join(format!("{id}.json")));
            }
        }
    }

    /// Every job record a disk store keeps (none in memory). An unreadable
    /// record is removed.
    pub fn load_jobs(&self) -> Vec<sb_core::JobRecord> {
        let Backend::Disk { root } = self.backend.as_ref() else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(root.join("jobs")) else {
            return Vec::new();
        };
        let mut jobs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let job = std::fs::read(&path)
                .ok()
                .and_then(|body| serde_json::from_slice::<sb_core::JobRecord>(&body).ok())
                .filter(|job| {
                    is_safe_id(&job.id)
                        && path.file_stem().and_then(|stem| stem.to_str()) == Some(job.id.as_str())
                });
            match job {
                Some(job) => jobs.push(job),
                None => remove_quietly(&path),
            }
        }
        jobs
    }

    fn lock(&self) -> MutexGuard<'_, Index> {
        // A panic while holding the lock leaves a consistent index (every
        // mutation is a few map operations); keep serving.
        self.index
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Commit a finished body under the index lock: move it into place
    /// (or drop it when the same body is already stored), write its record,
    /// and apply retention. File work stays under the lock so a body is
    /// never deleted between "already stored" and "referenced".
    fn commit(
        &self,
        mut record: ArtifactRecord,
        owner: Owner,
        bytes: u64,
        finished: Finished,
    ) -> Result<ArtifactRecord, ArtifactError> {
        record.bytes = bytes;
        record.retention = self.storage_label().to_string();
        let mut index = self.lock();
        let body = match (self.backend.as_ref(), finished) {
            (Backend::Disk { root }, Finished::File(tmp)) => {
                record.storage_ref = format!("blob:sha256/{}", record.sha256);
                let target = blob_path(root, &record.sha256);
                if index.blobs.contains_key(&record.sha256) || target.is_file() {
                    remove_quietly(&tmp);
                } else {
                    if let Some(parent) = target.parent() {
                        create_private_dir(parent).map_err(|e| ArtifactError::Io(e.to_string()))?;
                    }
                    std::fs::rename(&tmp, &target).map_err(|e| {
                        remove_quietly(&tmp);
                        ArtifactError::Io(format!("store body: {e}"))
                    })?;
                }
                let meta = StoredMeta {
                    record: record.clone(),
                    owner: owner.clone(),
                };
                if let Err(error) = write_record(root, &meta) {
                    if !index.blobs.contains_key(&record.sha256) {
                        remove_quietly(&target);
                    }
                    return Err(error);
                }
                None
            }
            (Backend::Memory, Finished::Memory(body)) => {
                record.storage_ref = format!("memory://{}", record.artifact_id);
                Some(Arc::new(body))
            }
            (_, finished) => {
                if let Finished::File(tmp) = finished {
                    remove_quietly(&tmp);
                }
                return Err(ArtifactError::Io(
                    "writer does not match the store".to_string(),
                ));
            }
        };
        index.add(
            StoredMeta {
                record: record.clone(),
                owner,
            },
            bytes,
            body,
        );
        let removed = index.enforce(&self.policy, now_ms());
        self.unlink(removed);
        Ok(record)
    }

    fn unlink(&self, removed: Vec<Removed>) {
        let Backend::Disk { root } = self.backend.as_ref() else {
            return;
        };
        for removed in removed {
            remove_quietly(&record_path(root, &removed.artifact_id));
            if let Some(sha) = removed.blob {
                remove_quietly(&blob_path(root, &sha));
            }
        }
    }
}

/// What retention removed: a record, and its body when nothing else names it.
struct Removed {
    artifact_id: String,
    blob: Option<String>,
}

impl Index {
    fn add(&mut self, meta: StoredMeta, bytes: u64, body: Option<Arc<Vec<u8>>>) {
        let blob = self
            .blobs
            .entry(meta.record.sha256.clone())
            .or_insert_with(|| Blob {
                refs: 0,
                bytes,
                body: None,
            });
        if blob.refs == 0 {
            self.total_bytes = self.total_bytes.saturating_add(blob.bytes);
        }
        blob.refs += 1;
        if blob.body.is_none() {
            blob.body = body;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.tombstones.remove(&meta.record.artifact_id);
        self.entries
            .insert(meta.record.artifact_id.clone(), Entry { meta, seq });
    }

    fn missing(&self, id: &str) -> Missing {
        match self.tombstones.get(id) {
            Some(owner) => Missing::Evicted(owner.clone()),
            None => Missing::NotFound,
        }
    }

    /// Apply the policy: everything older than the age cap, then the oldest
    /// until the total fits the size cap.
    fn enforce(&mut self, policy: &RetentionPolicy, now_ms: u64) -> Vec<Removed> {
        let mut order: Vec<(u64, u64, String)> = self
            .entries
            .iter()
            .map(|(id, entry)| (entry.meta.record.created_at_ms, entry.seq, id.clone()))
            .collect();
        order.sort();
        let cutoff = now_ms.saturating_sub(policy.max_age_ms);
        let mut removed = Vec::new();
        for (created_at_ms, _, id) in order {
            if created_at_ms >= cutoff && self.total_bytes <= policy.max_bytes {
                continue;
            }
            if let Some(gone) = self.evict(&id) {
                removed.push(gone);
            }
        }
        removed
    }

    fn evict(&mut self, id: &str) -> Option<Removed> {
        let entry = self.entries.remove(id)?;
        let sha = entry.meta.record.sha256.clone();
        let mut blob_gone = None;
        if let Some(blob) = self.blobs.get_mut(&sha) {
            blob.refs = blob.refs.saturating_sub(1);
            if blob.refs == 0 {
                self.total_bytes = self.total_bytes.saturating_sub(blob.bytes);
                self.blobs.remove(&sha);
                blob_gone = Some(sha);
            }
        }
        self.tombstones.insert(id.to_string(), entry.meta.owner);
        self.tombstone_order.push_back(id.to_string());
        while self.tombstone_order.len() > KEPT_TOMBSTONES {
            if let Some(oldest) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&oldest);
            }
        }
        Some(Removed {
            artifact_id: id.to_string(),
            blob: blob_gone,
        })
    }
}

enum Sink {
    File {
        file: std::fs::File,
        /// The half-written file; `None` once committed.
        path: Option<PathBuf>,
    },
    Memory(Vec<u8>),
}

/// A finished body handed to [`ArtifactStore::commit`].
enum Finished {
    File(PathBuf),
    Memory(Vec<u8>),
}

/// Writes one artifact's body as it arrives (a large video never sits in
/// memory whole on a disk store), hashing it on the way. Dropped before
/// [`Self::finish`] (a cancelled job), it deletes its half-written file.
pub struct ArtifactWriter {
    store: ArtifactStore,
    record: ArtifactRecord,
    owner: Owner,
    hasher: Sha256,
    bytes: u64,
    sink: Sink,
}

impl ArtifactWriter {
    pub fn write(&mut self, chunk: &[u8]) -> Result<(), ArtifactError> {
        self.bytes = self.bytes.saturating_add(chunk.len() as u64);
        if self.bytes > self.store.policy.max_bytes {
            return Err(ArtifactError::TooLarge {
                max_bytes: self.store.policy.max_bytes,
            });
        }
        self.hasher.update(chunk);
        match &mut self.sink {
            Sink::File { file, .. } => file
                .write_all(chunk)
                .map_err(|e| ArtifactError::Io(format!("write body: {e}"))),
            Sink::Memory(body) => {
                body.extend_from_slice(chunk);
                Ok(())
            }
        }
    }

    /// Finish the body and commit it; returns the stored record.
    pub fn finish(mut self) -> Result<ArtifactRecord, ArtifactError> {
        if let Sink::File { file, .. } = &mut self.sink {
            file.flush()
                .and_then(|()| file.sync_all())
                .map_err(|e| ArtifactError::Io(format!("flush body: {e}")))?;
        }
        let digest = std::mem::take(&mut self.hasher).finalize();
        let mut record = self.record.clone();
        record.sha256 = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        let finished = match &mut self.sink {
            Sink::File { path, .. } => match path.take() {
                Some(path) => Finished::File(path),
                None => return Err(ArtifactError::Io("writer already used".to_string())),
            },
            Sink::Memory(body) => Finished::Memory(std::mem::take(body)),
        };
        self.store
            .commit(record, self.owner.clone(), self.bytes, finished)
    }
}

impl Drop for ArtifactWriter {
    fn drop(&mut self) {
        if let Sink::File {
            path: Some(path), ..
        } = &self.sink
        {
            remove_quietly(path);
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn blob_path(root: &Path, sha: &str) -> PathBuf {
    root.join("blobs")
        .join("sha256")
        .join(&sha[..2.min(sha.len())])
        .join(sha)
}

fn record_path(root: &Path, artifact_id: &str) -> PathBuf {
    root.join("records").join(format!("{artifact_id}.json"))
}

/// A record file and its body's size, or `None` when either is unusable.
fn load_record(root: &Path, path: &Path) -> Option<(StoredMeta, u64)> {
    let stem = path.file_stem()?.to_str()?;
    if path.extension()?.to_str()? != "json" {
        return None;
    }
    let meta: StoredMeta = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    if meta.record.artifact_id != stem || !is_safe_id(stem) || !is_sha256_hex(&meta.record.sha256) {
        return None;
    }
    let bytes = std::fs::metadata(blob_path(root, &meta.record.sha256))
        .ok()
        .filter(|metadata| metadata.is_file())?
        .len();
    (bytes == meta.record.bytes).then_some((meta, bytes))
}

fn write_record(root: &Path, meta: &StoredMeta) -> Result<(), ArtifactError> {
    let body = serde_json::to_vec(meta).map_err(|e| ArtifactError::Io(e.to_string()))?;
    let tmp = root
        .join("tmp")
        .join(format!("{}.json.part", meta.record.artifact_id));
    let target = record_path(root, &meta.record.artifact_id);
    let written = create_private_file(&tmp)
        .and_then(|mut file| {
            file.write_all(&body)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, &target));
    written.map_err(|e| {
        remove_quietly(&tmp);
        ArtifactError::Io(format!("write record: {e}"))
    })
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn remove_quietly(path: &Path) {
    if let Err(error) = std::fs::remove_file(path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(file = %path.display(), %error, "artifact store could not remove a file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "sb-artifacts-{name}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn record(id: &str, created_at_ms: u64) -> ArtifactRecord {
        ArtifactRecord {
            artifact_id: id.to_string(),
            job_id: "job_test".to_string(),
            kind: sb_core::ArtifactKind::Image,
            media_type: "image/png".to_string(),
            bytes: 0,
            sha256: String::new(),
            storage_ref: String::new(),
            width: None,
            height: None,
            duration_ms: None,
            fps: None,
            created_at_ms,
            retention: String::new(),
            provenance: BTreeMap::new(),
        }
    }

    fn policy(max_bytes: u64) -> RetentionPolicy {
        RetentionPolicy {
            max_bytes,
            max_age_ms: 3_600_000,
        }
    }

    fn owner() -> Owner {
        Owner::new(Some("team".to_string()), None)
    }

    fn body(store: &ArtifactStore, id: &str) -> Vec<u8> {
        store.read_all(id).map(|(_, _, bytes)| bytes).unwrap()
    }

    #[test]
    fn artifacts_survive_a_restart_of_the_store() {
        let root = temp_root("restart");
        let now = now_ms();
        let stored = {
            let store = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
            store
                .put_bytes(record("art_one", now), owner(), b"first body")
                .unwrap()
        };
        assert_eq!(stored.bytes, 10);
        assert_eq!(stored.retention, "disk");
        assert!(stored.storage_ref.starts_with("blob:sha256/"));
        assert!(!stored.storage_ref.contains(root.to_str().unwrap()));

        let reopened = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
        let (found, found_owner) = reopened.record("art_one").unwrap();
        assert_eq!(found.sha256, stored.sha256);
        assert_eq!(found_owner, owner());
        assert_eq!(body(&reopened, "art_one"), b"first body");
        assert_eq!(reopened.total_bytes(), 10);
    }

    #[test]
    fn the_size_cap_evicts_the_oldest_first() {
        let root = temp_root("size");
        let store = ArtifactStore::open_disk(&root, policy(25)).unwrap();
        let now = now_ms();
        store
            .put_bytes(record("art_a", now), owner(), &[1; 10])
            .unwrap();
        store
            .put_bytes(record("art_b", now), owner(), &[2; 10])
            .unwrap();
        store
            .put_bytes(record("art_c", now), owner(), &[3; 10])
            .unwrap();
        assert_eq!(
            store.record("art_a").unwrap_err(),
            Missing::Evicted(owner())
        );
        assert!(store.record("art_b").is_ok());
        assert!(store.record("art_c").is_ok());
        assert_eq!(store.total_bytes(), 20);
        // The evicted body and record are gone from disk too.
        let records = std::fs::read_dir(root.join("records")).unwrap().count();
        assert_eq!(records, 2);
    }

    #[test]
    fn the_age_cap_removes_old_artifacts_at_write_and_at_start_up() {
        let root = temp_root("age");
        let now = now_ms();
        {
            let store = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
            store
                .put_bytes(record("art_old", now - 10_000), owner(), b"old")
                .unwrap();
            store
                .put_bytes(record("art_new", now), owner(), b"new")
                .unwrap();
        }
        let tight = RetentionPolicy {
            max_bytes: 1 << 20,
            max_age_ms: 5_000,
        };
        let store = ArtifactStore::open_disk(&root, tight).unwrap();
        assert_eq!(
            store.record("art_old").unwrap_err(),
            Missing::Evicted(owner())
        );
        assert!(store.record("art_new").is_ok());
        store
            .put_bytes(record("art_stale", now - 60_000), owner(), b"stale")
            .ok();
        assert!(store.record("art_stale").is_err(), "too old at write time");
    }

    #[test]
    fn memory_only_has_a_hard_cap_oldest_first() {
        let cfg = ArtifactsConfig {
            storage: ArtifactStorage::Memory,
            dir: None,
            max_bytes: Some(15),
            max_age_secs: 60,
        };
        let store = ArtifactStore::from_config(&cfg, Some(Path::new("/nonexistent-root")));
        assert_eq!(store.storage_label(), "process_memory");
        let now = now_ms();
        let first = store
            .put_bytes(record("art_1", now), owner(), &[1; 10])
            .unwrap();
        assert_eq!(first.retention, "process_memory");
        assert!(first.storage_ref.starts_with("memory://"));
        store
            .put_bytes(record("art_2", now), owner(), &[2; 10])
            .unwrap();
        assert!(matches!(store.record("art_1"), Err(Missing::Evicted(_))));
        assert_eq!(body(&store, "art_2"), vec![2; 10]);
        let too_big = store.put_bytes(record("art_3", now), owner(), &[3; 16]);
        assert!(matches!(
            too_big,
            Err(ArtifactError::TooLarge { max_bytes: 15 })
        ));
        assert!(store.total_bytes() <= 15);

        // Memory storage without its own cap gets the 256 MiB default.
        let defaults = ArtifactsConfig {
            storage: ArtifactStorage::Memory,
            ..ArtifactsConfig::default()
        };
        assert_eq!(
            ArtifactStore::from_config(&defaults, None)
                .policy()
                .max_bytes,
            256 * 1024 * 1024
        );
    }

    #[test]
    fn an_unknown_artifact_is_not_found_and_an_evicted_one_says_so() {
        let store = ArtifactStore::memory(policy(10));
        let now = now_ms();
        store
            .put_bytes(record("art_x", now), owner(), &[1; 8])
            .unwrap();
        store
            .put_bytes(record("art_y", now), owner(), &[2; 8])
            .unwrap();
        assert_eq!(store.record("art_never").unwrap_err(), Missing::NotFound);
        assert!(matches!(store.open("art_x"), Err(Missing::Evicted(_))));
    }

    #[cfg(unix)]
    #[test]
    fn files_and_directories_are_private_to_the_gateway_user() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("perms");
        let store = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
        let stored = store
            .put_bytes(record("art_p", now_ms()), owner(), b"private")
            .unwrap();
        let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(root.clone()), 0o700);
        assert_eq!(mode(root.join("records")), 0o700);
        assert_eq!(mode(blob_path(&root, &stored.sha256)), 0o600);
        assert_eq!(mode(record_path(&root, "art_p")), 0o600);
    }

    #[test]
    fn identical_bodies_share_one_file_and_count_once() {
        let root = temp_root("dedup");
        let store = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
        let now = now_ms();
        let a = store
            .put_bytes(record("art_a", now), owner(), b"same")
            .unwrap();
        let b = store
            .put_bytes(record("art_b", now), owner(), b"same")
            .unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(store.total_bytes(), 4);
    }

    #[test]
    fn a_dropped_writer_leaves_no_half_written_file() {
        let root = temp_root("drop");
        let store = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
        let mut writer = store.writer(record("art_d", now_ms()), owner()).unwrap();
        writer.write(b"partial").unwrap();
        drop(writer);
        assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
        assert_eq!(store.record("art_d").unwrap_err(), Missing::NotFound);
    }

    #[test]
    fn start_up_drops_bodies_and_records_that_do_not_match() {
        let root = temp_root("orphans");
        {
            let store = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
            store
                .put_bytes(record("art_keep", now_ms()), owner(), b"kept")
                .unwrap();
        }
        let orphan = blob_path(&root, &"ab".repeat(32));
        std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
        std::fs::write(&orphan, b"orphan").unwrap();
        std::fs::write(root.join("records").join("art_bad.json"), b"not json").unwrap();
        std::fs::write(root.join("tmp").join("leftover.part"), b"half").unwrap();
        let store = ArtifactStore::open_disk(&root, policy(1 << 20)).unwrap();
        assert!(!orphan.exists());
        assert!(!root.join("records").join("art_bad.json").exists());
        assert_eq!(std::fs::read_dir(root.join("tmp")).unwrap().count(), 0);
        assert_eq!(body(&store, "art_keep"), b"kept");
    }

    #[test]
    fn concurrent_writes_and_reads_stay_consistent() {
        let root = temp_root("concurrent");
        let store = ArtifactStore::open_disk(&root, policy(64 * 100)).unwrap();
        let threads: Vec<_> = (0..8)
            .map(|t: u32| {
                let store = store.clone();
                std::thread::spawn(move || {
                    for n in 0..40u32 {
                        let id = format!("art_{t}_{n}");
                        let bytes = vec![(t * 40 + n) as u8; 64];
                        store
                            .put_bytes(record(&id, now_ms()), Owner::default(), &bytes)
                            .unwrap();
                        match store.read_all(&id) {
                            Ok((_, _, read)) => assert_eq!(read, bytes),
                            Err(Missing::Evicted(_)) => {}
                            Err(other) => panic!("{other:?}"),
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!(store.total_bytes() <= 64 * 100);
        let reopened = ArtifactStore::open_disk(&root, policy(64 * 100)).unwrap();
        assert_eq!(reopened.total_bytes(), store.total_bytes());
    }
}
