//! Protected raw body evidence storage for Switchback.
//!
//! This crate is intentionally separate from `sb-trace`: traces stay
//! metadata-only, while body-bearing records are explicit, protected, hashed,
//! compressed, and indexed here.

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
const PRECISE_SPOOL_BACKLOG_ROW_LIMIT: u64 = 100_000;
const PRECISE_STATUS_DB_SIZE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;
const SQLITE_BUSY_TIMEOUT_MS: u64 = 250;
const ZSTD_LEVEL: i32 = 3;

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
    pub spool_backlog: u64,
    pub spool_backlog_exact: bool,
    pub last_event_at_unix_ms: Option<i64>,
    pub protected_paths: Vec<String>,
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

    pub fn record(&self, input: BodyEventInput) -> Result<BodyRecord> {
        let now_ms = now_unix_ms();
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
        self.append_legacy_event(&record)?;
        if location.archive_available {
            if let Some(day_dir) = location.day_dir {
                self.append_archive_event(&day_dir, &record)?;
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

    pub fn status(&self) -> Result<BodyStatus> {
        let conn = open_index_connection(&self.index_path)?;
        let has_storage_index = index_exists(&conn, "idx_body_blobs_storage")?;
        let large_unindexed = !has_storage_index
            && fs::metadata(&self.index_path)
                .map(|metadata| metadata.len() > PRECISE_STATUS_DB_SIZE_LIMIT_BYTES)
                .unwrap_or(false);
        let events = if large_unindexed {
            0
        } else {
            append_only_rows(&conn, "body_events")?
        };
        let blobs = if large_unindexed {
            PRECISE_SPOOL_BACKLOG_ROW_LIMIT + 1
        } else {
            append_only_rows(&conn, "body_blobs")?
        };
        let spool_backlog_exact =
            !large_unindexed && (blobs <= PRECISE_SPOOL_BACKLOG_ROW_LIMIT || has_storage_index);
        let spool_backlog = if spool_backlog_exact {
            conn.query_row(
                "SELECT COUNT(*) FROM body_blobs WHERE storage = 'spool'",
                [],
                |row| row.get::<_, u64>(0),
            )?
        } else {
            0
        };
        let last_event_at_unix_ms = if large_unindexed {
            None
        } else {
            conn.query_row(
                "SELECT MAX(observed_at_unix_ms) FROM body_events",
                [],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten()
        };
        let archive_available = archive_root_available(&self.config.archive_root);
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
            spool_backlog,
            spool_backlog_exact,
            last_event_at_unix_ms,
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
                spool_backlog: 0,
                spool_backlog_exact: true,
                last_event_at_unix_ms: None,
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
            let (yyyy, mm, dd) = date_parts(observed_at_unix_ms);
            let day_dir = self
                .config
                .archive_root
                .join(format!("{yyyy:04}"))
                .join(format!("{mm:02}"))
                .join(format!("{dd:02}"));
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

    fn append_legacy_event(&self, record: &BodyRecord) -> Result<()> {
        let Some(path) = &self.config.legacy_jsonl else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{}", serde_json::to_string(record)?)?;
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

fn append_only_rows(conn: &Connection, table: &str) -> Result<u64> {
    let sql = format!("SELECT COALESCE(MAX(rowid), 0) FROM {table}");
    Ok(conn.query_row(&sql, [], |row| row.get::<_, u64>(0))?)
}

fn index_exists(conn: &Connection, name: &str) -> Result<bool> {
    let found = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(found)
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
