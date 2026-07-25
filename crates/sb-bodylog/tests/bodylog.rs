use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension as _;
use sb_bodylog::{
    BodyEventInput, BodyEventQuery, BodyLogger, BodyLoggerConfig, CaptureMode, CaptureStage,
    GcOptions, PressureObservation, DEFAULT_KEEP_DAYS,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_root(tag: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "switchback-bodylog-{tag}-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn input(request_id: &str, body: &[u8]) -> BodyEventInput {
    BodyEventInput {
        request_id: request_id.to_string(),
        capture_stage: CaptureStage::ClientInbound,
        protocol: "http".to_string(),
        upstream: Some("http://127.0.0.1:8787".to_string()),
        model: Some("gpt-5.5".to_string()),
        status: Some(200),
        content_type: Some("application/json".to_string()),
        metadata: serde_json::json!({"source": "test"}),
        body: body.to_vec(),
    }
}

#[test]
fn default_archive_root_stays_inside_state_dir() {
    std::env::remove_var("SWITCHBACK_BODY_ARCHIVE_ROOT");
    let root = temp_root("default-root");
    let state_dir = root.join("state");
    let config = BodyLoggerConfig::from_legacy_sink(state_dir.join("tap-bodies.jsonl"));

    assert_eq!(config.state_dir, state_dir);
    assert_eq!(config.archive_root, config.state_dir.join("body/archive"));
}

#[test]
fn copies_legacy_hot_index_into_body_namespace() {
    std::env::remove_var("SWITCHBACK_BODY_ARCHIVE_ROOT");
    let root = temp_root("legacy-index");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger
        .record(input("tap_legacy", b"legacy-index-body"))
        .unwrap();
    let new_index = root.join("state/body/index.sqlite");
    let legacy_index = root.join("state/body-index.sqlite");
    fs::copy(&new_index, &legacy_index).unwrap();
    fs::remove_file(&new_index).unwrap();

    let logger = BodyLogger::new(BodyLoggerConfig::from_legacy_sink(
        root.join("state/tap-bodies.jsonl"),
    ))
    .unwrap();

    assert_eq!(logger.status().unwrap().events, 1);
    assert!(new_index.exists());
}

#[test]
fn stores_capture_segment_on_archive_and_indexes_metadata() {
    let root = temp_root("archive");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: Some(root.join("state").join("tap-bodies.jsonl")),
        inline_threshold_bytes: 16,
    })
    .unwrap();

    let record = logger
        .record(input("tap_1", br#"{"prompt":"keep me"}"#))
        .unwrap();

    assert_eq!(record.storage, "archive_segment");
    assert!(record.protected);
    assert!(record.archive_path.ends_with(".sbcap"));
    assert!(PathBuf::from(&record.archive_path).exists());
    assert_eq!(
        logger.read_blob(&record.body_sha256).unwrap(),
        br#"{"prompt":"keep me"}"#
    );

    let status = logger.status().unwrap();
    assert_eq!(status.events, 1);
    assert_eq!(status.blobs, 1);
    assert_eq!(status.spool_backlog, 0);
    assert!(status.archive_available);

    // The segment is the sole body/event artifact. Pointer/event sidecars would
    // duplicate the payload metadata and recreate write amplification.
    let day_dir = day_dir_of(&record.archive_path);
    assert!(!day_dir.join("tap-bodies.jsonl").exists());
    assert!(!day_dir.join("body-events.jsonl.zst").exists());
    // The configured legacy sink is frozen: never created or appended to.
    assert!(!root.join("state").join("tap-bodies.jsonl").exists());
}

#[test]
fn stores_each_body_once_in_a_framed_capture_segment() {
    let root = temp_root("capture-segment");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: Some(root.join("state").join("tap-bodies.jsonl")),
        inline_threshold_bytes: 16,
    })
    .unwrap();

    let body = b"byte-faithful segmented body";
    let record = logger.record(input("segment_1", body)).unwrap();
    let segment_path = PathBuf::from(&record.archive_path);

    assert_eq!(record.storage, "archive_segment");
    assert_eq!(
        segment_path.extension().and_then(|value| value.to_str()),
        Some("sbcap")
    );
    assert!(segment_path.exists());
    assert_eq!(logger.read_blob(&record.body_sha256).unwrap(), body);

    let day_dir = day_dir_of(&record.archive_path);
    assert!(
        !day_dir.join("tap-bodies.jsonl").exists(),
        "segment records replace per-event pointer JSONL writes"
    );
    assert!(
        !day_dir.join("body-events.jsonl.zst").exists(),
        "segment records replace concatenated per-event compressed logs"
    );
}

#[test]
fn reopens_by_recovering_a_crash_tail_and_rebuilding_the_index() {
    let root = temp_root("segment-rebuild");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let first = logger
        .record(input("segment_rebuild_1", b"first exact body"))
        .unwrap();
    let second = logger
        .record(input("segment_rebuild_2", b"second exact body"))
        .unwrap();
    assert_eq!(first.archive_path, second.archive_path);

    let segment_path = PathBuf::from(&first.archive_path);
    let valid_len = fs::metadata(&segment_path).unwrap().len();
    drop(logger);
    fs::remove_file(format!("{}.manifest.json", segment_path.display())).unwrap();
    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&segment_path)
            .unwrap();
        file.write_all(b"incomplete-crash-tail").unwrap();
    }

    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", index_path(&root).display()));
        let _ = fs::remove_file(path);
    }

    let reopened = BodyLogger::new(config).unwrap();
    let events = reopened.latest_events(10).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        reopened.read_blob(&first.body_sha256).unwrap(),
        b"first exact body"
    );
    assert_eq!(
        reopened.read_blob(&second.body_sha256).unwrap(),
        b"second exact body"
    );
    assert_eq!(
        fs::metadata(&segment_path).unwrap().len(),
        valid_len,
        "startup recovery must truncate only the incomplete tail"
    );
}

#[test]
fn recovery_refuses_checksum_corruption_without_truncating_evidence() {
    let root = temp_root("segment-corruption");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let record = logger
        .record(input("corrupt-me", b"checksum-protected body"))
        .unwrap();
    let segment = PathBuf::from(&record.archive_path);
    logger.seal_active().unwrap();
    drop(logger);
    fs::remove_file(format!("{}.manifest.json", segment.display())).unwrap();

    let mut bytes = fs::read(&segment).unwrap();
    let checksum_offset = 8 + 4 + 8 + 8;
    bytes[checksum_offset] ^= 0xff;
    fs::write(&segment, &bytes).unwrap();
    let corrupted_len = fs::metadata(&segment).unwrap().len();

    let error = BodyLogger::new(config).unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"));
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        corrupted_len,
        "checksum failure is evidence, not a crash-tail deletion signal"
    );
}

#[test]
fn a_second_logger_does_not_seal_a_segment_owned_by_a_live_writer() {
    let root = temp_root("segment-live-writer");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let writer = BodyLogger::new(config.clone()).unwrap();
    let first = writer
        .record(input("live-writer-1", b"first live frame"))
        .unwrap();
    let manifest = PathBuf::from(format!("{}.manifest.json", first.archive_path));
    assert!(!manifest.exists());

    let _observer = BodyLogger::new(config).unwrap();
    assert!(
        !manifest.exists(),
        "startup recovery must skip segments locked by another live logger"
    );

    let second = writer
        .record(input("live-writer-2", b"second live frame"))
        .unwrap();
    assert_eq!(first.archive_path, second.archive_path);
    assert!(!manifest.exists());
}

#[test]
fn deduplicates_body_blobs_by_sha256_but_keeps_each_event() {
    let root = temp_root("dedupe");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();

    let first = logger.record(input("tap_1", b"same-body")).unwrap();
    let second = logger.record(input("tap_2", b"same-body")).unwrap();

    assert_eq!(first.body_sha256, second.body_sha256);
    let status = logger.status().unwrap();
    assert_eq!(status.events, 2);
    assert_eq!(status.blobs, 1);
}

#[test]
fn falls_back_to_local_spool_when_archive_root_is_unavailable() {
    let root = temp_root("spool");
    let unavailable_archive = root.join("missing").join("archive");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: unavailable_archive,
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();

    let record = logger
        .record(input("tap_1", b"body that cannot leave local disk"))
        .unwrap();

    assert_eq!(record.storage, "spool_segment");
    assert!(record.archive_path.ends_with(".sbcap"));
    assert!(PathBuf::from(&record.archive_path).exists());
    assert_eq!(
        logger.read_blob(&record.body_sha256).unwrap(),
        b"body that cannot leave local disk"
    );
    let status = logger.status().unwrap();
    assert!(!status.archive_available);
    // The segment is the sole spool artifact counted for later drain.
    assert_eq!(status.spool_backlog, 1);
    assert!(status.spool_backlog_exact);
}

// D4 (falsifier 7): a large DB must report truthful MAX(rowid) approximations
// flagged approximate — never the old events=0 / blobs=100001 sentinels — and
// spool backlog must stay filesystem-exact regardless of sqlite size.
#[test]
fn large_db_status_reports_approximate_counts_not_sentinels() {
    std::env::remove_var("SWITCHBACK_BODY_ARCHIVE_ROOT");
    let root = temp_root("large-db-status");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger.record(input("tap_a", b"alpha")).unwrap();
    logger.record(input("tap_b", b"beta")).unwrap();

    // Force the large-DB path with a 1-byte precise-count threshold.
    let status = logger.status_with_precise_limit(1).unwrap();
    assert!(status.counts_approximate);
    assert_eq!(status.events, 2);
    assert_eq!(status.blobs, 2);
    assert_ne!(status.blobs, 100_001);
    assert_eq!(status.spool_backlog, 0);
    assert!(status.spool_backlog_exact);
    assert_eq!(status.status, "ok");
    assert!(status.archive_available);

    // The default (precise) path stays exact on a small DB.
    let precise = logger.status().unwrap();
    assert!(!precise.counts_approximate);
    assert_eq!(precise.events, 2);
    assert_eq!(precise.blobs, 2);
}

#[test]
fn locked_index_write_fails_with_bounded_wait() {
    let root = temp_root("busy-timeout");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let status = logger.status().unwrap();
    let locker = rusqlite::Connection::open(&status.index_path).unwrap();
    locker.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let started = std::time::Instant::now();
    let err = logger
        .record(input("tap_locked", b"body while index is locked"))
        .unwrap_err();

    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "body index lock wait was not bounded: {:?}",
        started.elapsed()
    );
    assert!(
        err.to_string().contains("locked") || err.to_string().contains("busy"),
        "unexpected lock error: {err}"
    );
    locker.execute_batch("ROLLBACK;").unwrap();
}

#[test]
fn open_existing_does_not_create_missing_index() {
    let root = temp_root("open-existing-missing");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };

    let logger = BodyLogger::open_existing(config).unwrap();

    assert!(logger.is_none());
    assert!(!root.join("state/body/index.sqlite").exists());
}

#[test]
fn query_events_returns_newest_first_and_filters_request() {
    let root = temp_root("query-events");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();

    logger.record(input("tap_a", b"first")).unwrap();
    logger.record(input("tap_b", b"second")).unwrap();
    logger.record(input("tap_a", b"third")).unwrap();

    let latest = logger.latest_events(2).unwrap();
    assert_eq!(latest.len(), 2);
    assert_eq!(latest[0].request_id, "tap_a");
    assert_eq!(logger.read_blob(&latest[0].body_sha256).unwrap(), b"third");

    let grouped = logger.events_for_request("tap_a").unwrap();
    assert_eq!(grouped.len(), 2);
    assert!(grouped.iter().all(|record| record.request_id == "tap_a"));
}

#[test]
fn query_events_filters_stage_and_protocol() {
    let root = temp_root("query-stage-protocol");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger.record(input("tap_1", b"request")).unwrap();
    let mut response = input("tap_1", b"response");
    response.capture_stage = CaptureStage::ClientResponse;
    response.protocol = "forward-proxy".to_string();
    logger.record(response).unwrap();

    let filtered = logger
        .query_events(BodyEventQuery {
            request_id: Some("tap_1".to_string()),
            capture_stage: Some(CaptureStage::ClientResponse),
            protocol: Some("forward-proxy".to_string()),
            limit: 10,
        })
        .unwrap();

    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].capture_stage, "client_response");
    assert_eq!(filtered[0].protocol, "forward-proxy");
    assert_eq!(
        logger.read_blob(&filtered[0].body_sha256).unwrap(),
        b"response"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle falsifiers (work_36dd586db541): GC retention, spool drain,
// tap-bodies day-routing, truthful status, guarded compaction.
// ---------------------------------------------------------------------------

const DAY_MS: i64 = 86_400_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn logger_with_archive(root: &Path) -> (BodyLogger, PathBuf) {
    std::env::remove_var("SWITCHBACK_BODY_ARCHIVE_ROOT");
    let archive = root.join("archive");
    fs::create_dir_all(&archive).unwrap();
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: archive.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    (logger, archive)
}

fn index_path(root: &Path) -> PathBuf {
    root.join("state").join("body").join("index.sqlite")
}

fn open_index(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(index_path(root)).unwrap()
}

/// Day partition dir (`archive/YYYY/MM/DD`) that produced `archive_path`.
/// Supports both the current `segments/<file>.sbcap` layout and legacy
/// `blobs/sha256/<prefix>/<sha>.zst` paths used by compatibility fixtures.
fn day_dir_of(archive_path: &str) -> PathBuf {
    let path = PathBuf::from(archive_path);
    let depth = if path.extension().and_then(|value| value.to_str()) == Some("sbcap") {
        2
    } else {
        4
    };
    path.ancestors().nth(depth).unwrap().to_path_buf()
}

fn seed_blob(conn: &rusqlite::Connection, sha: &str, created_ms: i64, storage: &str, path: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO body_blobs (
            body_sha256, body_bytes, compressed_bytes, storage, archive_path,
            protected, created_at_unix_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![sha, 3_i64, 3_i64, storage, path, 1_i64, created_ms],
    )
    .unwrap();
}

fn seed_event(
    conn: &rusqlite::Connection,
    event_id: &str,
    sha: &str,
    observed_ms: i64,
    storage: &str,
    path: &str,
) {
    conn.execute(
        "INSERT INTO body_events (
            event_id, request_id, observed_at_unix_ms, capture_stage, protocol,
            upstream, model, status, content_type, body_sha256, body_bytes,
            compressed_bytes, archive_path, storage, protected, redaction_state,
            threshold_shrunk, metadata_json
        ) VALUES (?1,'req',?2,'client_inbound','http',NULL,NULL,NULL,NULL,?3,3,3,?4,?5,1,'raw_local',0,'{}')",
        rusqlite::params![event_id, observed_ms, sha, path, storage],
    )
    .unwrap();
}

fn count_rows(root: &Path, table: &str) -> u64 {
    open_index(root)
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn collect_files_with_extension(root: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_with_extension(&path, extension, out);
        } else if path.extension().and_then(|value| value.to_str()) == Some(extension) {
            out.push(path);
        }
    }
}

fn blob_exists(root: &Path, sha: &str) -> bool {
    open_index(root)
        .query_row(
            "SELECT 1 FROM body_blobs WHERE body_sha256 = ?1",
            rusqlite::params![sha],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .unwrap()
        .is_some()
}

fn blob_storage(root: &Path, sha: &str) -> Option<String> {
    open_index(root)
        .query_row(
            "SELECT storage FROM body_blobs WHERE body_sha256 = ?1",
            rusqlite::params![sha],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

fn event_storage(root: &Path, sha: &str) -> Option<String> {
    open_index(root)
        .query_row(
            "SELECT storage FROM body_events WHERE body_sha256 = ?1",
            rusqlite::params![sha],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

// Falsifier 1: archive unmounted -> GC/drain refuse, mutate nothing.
#[test]
fn gc_refuses_when_archive_root_is_unmounted() {
    let root = temp_root("gc-unmounted");
    let archive = PathBuf::from(format!(
        "/Volumes/switchback-nonexistent-{}/archive",
        std::process::id()
    ));
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: archive,
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    {
        let conn = open_index(&root);
        seed_event(
            &conn,
            "old",
            "shaOld",
            now_ms() - 40 * DAY_MS,
            "archive",
            "x",
        );
        seed_blob(&conn, "shaOld", now_ms() - 40 * DAY_MS, "archive", "x");
    }
    let events_before = count_rows(&root, "body_events");
    let blobs_before = count_rows(&root, "body_blobs");

    let dry = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: false,
            drain_only: false,
            batch_size: 8,
        })
        .unwrap();
    assert!(dry.refused.is_some(), "dry-run must refuse when unmounted");

    let confirmed = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: true,
            drain_only: false,
            batch_size: 8,
        })
        .unwrap();
    assert!(confirmed.refused.is_some());
    assert_eq!(confirmed.events_deleted, 0);
    assert_eq!(confirmed.blobs_deleted, 0);
    assert_eq!(confirmed.spool_blobs_drained, 0);
    assert_eq!(count_rows(&root, "body_events"), events_before);
    assert_eq!(count_rows(&root, "body_blobs"), blobs_before);
}

// Falsifier 2: present day dir -> kept; absent day dir -> batched delete + idempotent.
#[test]
fn gc_deletes_absent_day_events_and_is_idempotent() {
    let root = temp_root("gc-absent-day");
    let (logger, _archive) = logger_with_archive(&root);

    // Present day: real record, dir kept.
    let keep = logger
        .record_at(input("keep", b"kept-body"), now_ms() - 40 * DAY_MS)
        .unwrap();
    // Absent day: real record, then seed 4 more, then prune the dir.
    let del = logger
        .record_at(input("del", b"absent-body"), now_ms() - 45 * DAY_MS)
        .unwrap();
    let absent_day = day_dir_of(&del.archive_path);
    {
        let conn = open_index(&root);
        for i in 0..4 {
            let sha = format!("shaDel{i}");
            let path = absent_day.join(format!("blobs/{sha}.zst"));
            let path = path.to_string_lossy();
            seed_event(
                &conn,
                &format!("del_{i}"),
                &sha,
                now_ms() - 45 * DAY_MS,
                "archive",
                &path,
            );
            seed_blob(&conn, &sha, now_ms() - 45 * DAY_MS, "archive", &path);
        }
    }
    fs::remove_dir_all(&absent_day).unwrap();
    assert!(!absent_day.exists());
    assert!(day_dir_of(&keep.archive_path).exists(), "present day kept");

    let before_events = count_rows(&root, "body_events");
    assert_eq!(before_events, 6);

    // Dry-run: reports the absent day (5 rows), keeps the present day, mutates nothing.
    let dry = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: false,
            drain_only: false,
            batch_size: 2,
        })
        .unwrap();
    assert!(dry.refused.is_none());
    assert!(dry.candidate_days.iter().any(|c| c.event_rows == 5));
    assert_eq!(dry.events_deleted, 0);
    assert_eq!(count_rows(&root, "body_events"), 6);

    // Confirm: absent day deleted in batches, present day survives.
    let run = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: true,
            drain_only: false,
            batch_size: 2,
        })
        .unwrap();
    assert_eq!(run.events_deleted, 5);
    assert_eq!(count_rows(&root, "body_events"), 1);
    assert!(blob_exists(&root, &keep.body_sha256), "present blob kept");

    // Idempotent: re-run deletes nothing.
    let again = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: true,
            drain_only: false,
            batch_size: 2,
        })
        .unwrap();
    assert_eq!(again.events_deleted, 0);
    assert_eq!(again.blobs_deleted, 0);
}

// Falsifier 3: blob kept while a newer event references it; removed when orphaned.
#[test]
fn gc_keeps_blob_referenced_by_a_newer_event() {
    let root = temp_root("gc-dedup-safety");
    let (logger, _archive) = logger_with_archive(&root);

    // sha X: old event (absent day) + newer event (within keep window) share the blob.
    let x_old = logger
        .record_at(input("x", b"shared-body-x"), now_ms() - 45 * DAY_MS)
        .unwrap();
    let x_new = logger
        .record_at(input("x", b"shared-body-x"), now_ms() - DAY_MS)
        .unwrap();
    // sha Y: only an old event (absent day).
    let y_old = logger
        .record_at(input("y", b"only-old-body-y"), now_ms() - 45 * DAY_MS)
        .unwrap();
    assert_eq!(x_old.body_sha256, x_new.body_sha256);

    // Prune the absent day dir (export + prune).
    fs::remove_dir_all(day_dir_of(&x_old.archive_path)).unwrap();

    let run = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: true,
            drain_only: false,
            batch_size: 8,
        })
        .unwrap();
    assert_eq!(run.events_deleted, 2, "both old events deleted");
    assert_eq!(run.blobs_deleted, 1, "only the orphaned blob removed");
    assert!(
        blob_exists(&root, &x_old.body_sha256),
        "blob still referenced by the newer event survives"
    );
    assert!(
        !blob_exists(&root, &y_old.body_sha256),
        "blob whose only reference was deleted is removed"
    );
}

// Falsifier 4: spool drain moves blob into today's partition, updates rows,
// merges spool day-files without clobber, and status flips to exact-backlog ok.
#[test]
fn spool_drain_moves_blob_and_flips_status() {
    let root = temp_root("spool-drain");
    let (logger, archive) = logger_with_archive(&root);
    let spool = root.join("state").join("body").join("spool");

    // A spooled blob file + its index rows (storage=spool).
    let sha = "abcdef0123456789";
    let src = spool
        .join("blobs")
        .join("sha256")
        .join(&sha[..2])
        .join(format!("{sha}.zst"));
    fs::create_dir_all(src.parent().unwrap()).unwrap();
    fs::write(&src, b"spooled-blob-bytes").unwrap();
    {
        let conn = open_index(&root);
        seed_blob(&conn, sha, now_ms(), "spool", &src.to_string_lossy());
        seed_event(
            &conn,
            "spool_evt",
            sha,
            now_ms(),
            "spool",
            &src.to_string_lossy(),
        );
    }

    // A spooled day-file for 2026-07-02, plus a pre-existing archive day file
    // (drain must append-merge, never clobber).
    let day_file = spool.join("tap-bodies-20260702.jsonl");
    fs::write(&day_file, b"{\"spooled\":true}\n").unwrap();
    let archive_day = archive.join("2026").join("07").join("02");
    fs::create_dir_all(&archive_day).unwrap();
    fs::write(
        archive_day.join("tap-bodies.jsonl"),
        b"{\"preexisting\":true}\n",
    )
    .unwrap();

    let before = logger.status().unwrap();
    assert_eq!(
        before.spool_backlog, 2,
        "one blob file + one spool day-file"
    );
    assert!(before.spool_backlog_exact);
    assert_eq!(before.status, "spooling");

    let run = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: true,
            drain_only: true,
            batch_size: 8,
        })
        .unwrap();
    assert!(run.refused.is_none());
    assert_eq!(run.spool_blobs_drained, 1);
    assert_eq!(run.spool_day_files_drained, 1);
    assert_eq!(run.events_deleted, 0, "drain-only skips retention");

    assert!(!src.exists(), "spool blob file moved");
    assert_eq!(blob_storage(&root, sha).as_deref(), Some("archive"));
    assert_eq!(event_storage(&root, sha).as_deref(), Some("archive"));

    let merged = fs::read_to_string(archive_day.join("tap-bodies.jsonl")).unwrap();
    assert!(merged.contains("preexisting"), "existing content preserved");
    assert!(merged.contains("spooled"), "spooled content appended");

    let after = logger.status().unwrap();
    assert_eq!(after.spool_backlog, 0);
    assert!(after.spool_backlog_exact);
    assert_eq!(after.status, "ok");
}

#[test]
fn spool_drain_moves_sealed_segment_to_its_archive_day_and_updates_the_index() {
    let root = temp_root("segment-spool-drain");
    let mount = root.join("archive-mount");
    fs::write(&mount, b"offline").unwrap();
    let archive = mount.join("capture");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: archive.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let observed_at = now_ms() - 5 * DAY_MS;
    let record = logger
        .record_at(input("spooled-segment", b"spooled exact body"), observed_at)
        .unwrap();
    let source = PathBuf::from(&record.archive_path);
    let spool_segments = root.join("state/body/spool/segments");
    let relative = source.strip_prefix(&spool_segments).unwrap();
    let expected_destination = archive
        .join(relative.parent().unwrap())
        .join("segments")
        .join(relative.file_name().unwrap());

    assert_eq!(record.storage, "spool_segment");
    assert!(source.exists());
    assert_eq!(logger.status().unwrap().spool_backlog, 1);

    fs::remove_file(&mount).unwrap();
    fs::create_dir_all(&archive).unwrap();

    let run = logger
        .gc(GcOptions {
            keep_days: 3,
            confirm: true,
            drain_only: true,
            batch_size: 8,
        })
        .unwrap();

    assert!(run.refused.is_none());
    assert_eq!(run.spool_segments_drained, 1);
    assert!(!source.exists());
    let updated = logger.events_for_request("spooled-segment").unwrap();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].storage, "archive_segment");
    let destination = PathBuf::from(&updated[0].archive_path);
    assert_eq!(destination, expected_destination);
    assert!(destination.exists());
    assert!(
        PathBuf::from(format!("{}.manifest.json", destination.display())).exists(),
        "drain seals and moves the segment manifest with the segment"
    );
    assert_eq!(
        logger.read_blob(&record.body_sha256).unwrap(),
        b"spooled exact body"
    );
    assert_eq!(logger.status().unwrap().spool_backlog, 0);

    drop(logger);
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", index_path(&root).display()));
        let _ = fs::remove_file(path);
    }
    let rebuilt = BodyLogger::new(config).unwrap();
    let rebuilt_event = rebuilt.events_for_request("spooled-segment").unwrap();
    assert_eq!(rebuilt_event.len(), 1);
    assert_eq!(rebuilt_event[0].storage, "archive_segment");
    assert_eq!(PathBuf::from(&rebuilt_event[0].archive_path), destination);
}

#[test]
fn gc_never_treats_a_spool_segment_as_an_archive_gc_candidate() {
    let root = temp_root("segment-spool-gc-safety");
    let mount = root.join("archive-mount");
    fs::write(&mount, b"offline").unwrap();
    let archive = mount.join("capture");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: archive.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let record = logger
        .record_at(
            input("old-spool", b"must survive gc"),
            now_ms() - 40 * DAY_MS,
        )
        .unwrap();
    let source = PathBuf::from(&record.archive_path);

    fs::remove_file(&mount).unwrap();
    fs::create_dir_all(&archive).unwrap();
    let dry = logger
        .gc(GcOptions {
            keep_days: 3,
            confirm: false,
            drain_only: false,
            batch_size: 8,
        })
        .unwrap();

    assert!(
        dry.candidate_days
            .iter()
            .all(|candidate| candidate.event_rows == 0),
        "spool segments are drain candidates, never retention-delete candidates"
    );
    assert_eq!(count_rows(&root, "body_events"), 1);
    assert!(source.exists());
}

#[test]
fn time_rotation_seals_a_checksum_manifest_for_the_previous_segment() {
    let root = temp_root("segment-manifest");
    let (logger, _archive) = logger_with_archive(&root);
    let first_at = now_ms() - 30 * 60 * 1_000;
    let second_at = first_at + 16 * 60 * 1_000;
    let first = logger
        .record_at(input("first-bucket", b"first bucket body"), first_at)
        .unwrap();
    let second = logger
        .record_at(input("second-bucket", b"second bucket body"), second_at)
        .unwrap();

    assert_ne!(first.archive_path, second.archive_path);
    let manifest_path = PathBuf::from(format!("{}.manifest.json", first.archive_path));
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["schema_version"], "switchback/capture-segment@1");
    assert_eq!(manifest["sealed"], true);
    assert_eq!(manifest["record_count"], 1);
    assert_eq!(manifest["first_observed_at_unix_ms"], first_at);
    assert_eq!(manifest["last_observed_at_unix_ms"], first_at);
    assert_eq!(
        manifest["segment_bytes"].as_u64().unwrap(),
        fs::metadata(&first.archive_path).unwrap().len()
    );
    assert_eq!(
        manifest["segment_sha256"].as_str().unwrap().len(),
        64,
        "manifest carries a full SHA-256 checksum"
    );
    assert!(
        !PathBuf::from(format!("{}.manifest.json", second.archive_path)).exists(),
        "the current appendable segment stays unsealed"
    );
}

#[test]
fn default_hot_retention_is_three_days() {
    assert_eq!(DEFAULT_KEEP_DAYS, 3);
}

#[test]
fn pressure_hysteresis_persists_and_requires_two_healthy_backup_cycles_to_resume() {
    let root = temp_root("pressure-hysteresis");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let now = now_ms();
    let logger = BodyLogger::new(config.clone()).unwrap();
    let degraded = logger
        .evaluate_pressure_at(
            PressureObservation {
                free_bytes: 40_000_000_000,
                capacity_bytes: 1_000_000_000_000,
                last_backup_success_at_unix_ms: Some(now - 25 * 60 * 60 * 1_000),
                backup_generation: 10,
                unbacked_bytes: 12_000_000_000,
            },
            now,
        )
        .unwrap();
    assert_eq!(degraded.mode, CaptureMode::MetadataOnly);
    assert!(degraded.reasons.iter().any(|reason| reason == "free_bytes"));
    assert!(degraded
        .reasons
        .iter()
        .any(|reason| reason == "backup_stale"));
    drop(logger);

    let reopened = BodyLogger::new(config).unwrap();
    assert_eq!(
        reopened.status().unwrap().pressure.mode,
        CaptureMode::MetadataOnly,
        "degradation survives a process restart"
    );
    let one_cycle = reopened
        .evaluate_pressure_at(
            PressureObservation {
                free_bytes: 150_000_000_000,
                capacity_bytes: 1_000_000_000_000,
                last_backup_success_at_unix_ms: Some(now),
                backup_generation: 11,
                unbacked_bytes: 1_000_000_000,
            },
            now,
        )
        .unwrap();
    assert_eq!(one_cycle.mode, CaptureMode::MetadataOnly);
    assert_eq!(one_cycle.healthy_backup_cycles, 1);

    let two_cycles = reopened
        .evaluate_pressure_at(
            PressureObservation {
                free_bytes: 150_000_000_000,
                capacity_bytes: 1_000_000_000_000,
                last_backup_success_at_unix_ms: Some(now),
                backup_generation: 12,
                unbacked_bytes: 1_000_000_000,
            },
            now,
        )
        .unwrap();
    assert_eq!(two_cycles.mode, CaptureMode::SegmentedFullWire);
    assert!(two_cycles.reasons.is_empty());
}

#[test]
fn metadata_only_pressure_record_keeps_a_gap_without_writing_payload_bytes() {
    let root = temp_root("metadata-only-gap");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let now = now_ms();
    let admission = logger
        .evaluate_pressure_at(
            PressureObservation {
                free_bytes: 40_000_000_000,
                capacity_bytes: 1_000_000_000_000,
                last_backup_success_at_unix_ms: Some(now),
                backup_generation: 1,
                unbacked_bytes: 0,
            },
            now,
        )
        .unwrap();
    let body = b"must not be written under pressure";
    let record = logger
        .record_metadata_only(input("pressure-gap", body), &admission)
        .unwrap();

    assert_eq!(record.storage, "metadata_only");
    assert_eq!(record.redaction_state, "metadata_only_pressure");
    assert!(record.archive_path.is_empty());
    assert_eq!(record.body_bytes, body.len() as u64);
    assert!(logger.read_blob(&record.body_sha256).is_err());
    let status = logger.status().unwrap();
    assert_eq!(status.events, 1);
    assert_eq!(status.blobs, 0);
    assert_eq!(status.pressure.metadata_only_events, 1);
    let mut segments = Vec::new();
    collect_files_with_extension(&root, "sbcap", &mut segments);
    assert!(segments.is_empty());
}

// Falsifier 5: segments route into their UTC day partition and the configured
// legacy sink is never appended (frozen; bytes untouched).
#[test]
fn capture_segments_route_by_day_and_freeze_legacy() {
    let root = temp_root("day-route");
    std::env::remove_var("SWITCHBACK_BODY_ARCHIVE_ROOT");
    let archive = root.join("archive");
    fs::create_dir_all(&archive).unwrap();
    let legacy = root.join("state").join("tap-bodies.jsonl");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let frozen = b"FROZEN-LEGACY-LINE\n";
    fs::write(&legacy, frozen).unwrap();

    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: archive,
        legacy_jsonl: Some(legacy.clone()),
        inline_threshold_bytes: 16,
    })
    .unwrap();

    let r1 = logger
        .record_at(input("r1", b"day-one-body"), now_ms() - 10 * DAY_MS)
        .unwrap();
    let r2 = logger
        .record_at(input("r2", b"day-two-body"), now_ms() - 20 * DAY_MS)
        .unwrap();

    let d1 = day_dir_of(&r1.archive_path);
    let d2 = day_dir_of(&r2.archive_path);
    assert_ne!(d1, d2, "different UTC days land in different partitions");
    assert!(PathBuf::from(&r1.archive_path).exists());
    assert!(PathBuf::from(&r2.archive_path).exists());
    assert!(!d1.join("tap-bodies.jsonl").exists());
    assert!(!d2.join("tap-bodies.jsonl").exists());

    // Legacy sink is byte-for-byte untouched.
    assert_eq!(fs::read(&legacy).unwrap(), frozen);
}

// Falsifier 6: archive unavailable -> record is captured in a spool segment;
// the legacy/pointer sinks stay frozen.
#[test]
fn archive_unavailable_routes_into_spool_segment_without_pointer_files() {
    let root = temp_root("spool-day-route");
    let archive = PathBuf::from(format!(
        "/Volumes/switchback-nonexistent-{}/archive",
        std::process::id()
    ));
    let legacy = root.join("state").join("tap-bodies.jsonl");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: archive,
        legacy_jsonl: Some(legacy.clone()),
        inline_threshold_bytes: 16,
    })
    .unwrap();

    let r = logger.record(input("r", b"offline-body")).unwrap();
    assert_eq!(r.storage, "spool_segment");

    let spool = root.join("state").join("body").join("spool");
    assert!(PathBuf::from(&r.archive_path).starts_with(spool.join("segments")));
    assert!(PathBuf::from(&r.archive_path).exists());
    assert!(
        fs::read_dir(&spool)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .all(|path| !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("tap-bodies-"))),
        "segment capture does not create spool pointer files"
    );
    assert!(!legacy.exists(), "legacy sink stays frozen (never created)");
}

// Falsifier 8: compaction refuses when a holder is present, succeeds (identical
// row counts, atomic replace) when none, and requires --confirm.
#[test]
fn compact_refuses_with_holder_and_succeeds_without() {
    let root = temp_root("compact");
    let (logger, _archive) = logger_with_archive(&root);
    logger.record(input("a", b"alpha")).unwrap();
    logger.record(input("b", b"beta")).unwrap();

    let unconfirmed = logger.compact(false).unwrap();
    assert!(unconfirmed.refused.as_deref().unwrap().contains("confirm"));

    let held = logger
        .compact_with_holder_probe(true, || Ok(vec![u32::MAX]))
        .unwrap();
    assert!(held
        .refused
        .as_ref()
        .unwrap()
        .contains(u32::MAX.to_string().as_str()));
    assert_eq!(held.events_after, held.events_before);
    assert_eq!(held.blobs_after, held.blobs_before);

    let ok = logger
        .compact_with_holder_probe(true, || Ok(vec![]))
        .unwrap();
    assert!(ok.refused.is_none());
    assert_eq!(ok.events_after, ok.events_before);
    assert_eq!(ok.blobs_after, ok.blobs_before);

    // The replaced index is still usable and consistent.
    let status = logger.status().unwrap();
    assert_eq!(status.events, 2);
    assert_eq!(status.blobs, 2);
}

// Falsifier 9: dry-run (no --confirm) reports candidates but mutates nothing.
#[test]
fn gc_dry_run_default_mutates_nothing() {
    let root = temp_root("gc-dry-run");
    let (logger, _archive) = logger_with_archive(&root);
    let del = logger
        .record_at(input("d", b"old-body"), now_ms() - 45 * DAY_MS)
        .unwrap();
    let absent_day = day_dir_of(&del.archive_path);
    {
        let conn = open_index(&root);
        for i in 0..2 {
            let sha = format!("dry{i}");
            seed_event(
                &conn,
                &format!("dry_{i}"),
                &sha,
                now_ms() - 45 * DAY_MS,
                "archive",
                "p",
            );
            seed_blob(&conn, &sha, now_ms() - 45 * DAY_MS, "archive", "p");
        }
    }
    fs::remove_dir_all(&absent_day).unwrap();

    let events_before = count_rows(&root, "body_events");
    let blobs_before = count_rows(&root, "body_blobs");

    let dry = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: false,
            drain_only: false,
            batch_size: 8,
        })
        .unwrap();
    assert!(dry.refused.is_none());
    assert_eq!(dry.events_deleted, 0);
    assert_eq!(dry.blobs_deleted, 0);
    let reported: u64 = dry.candidate_days.iter().map(|c| c.event_rows).sum();
    assert!(reported >= 3, "reports nonzero candidates: {reported}");

    assert_eq!(count_rows(&root, "body_events"), events_before);
    assert_eq!(count_rows(&root, "body_blobs"), blobs_before);
}
