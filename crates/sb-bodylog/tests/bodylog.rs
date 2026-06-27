use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sb_bodylog::{BodyEventInput, BodyLogger, BodyLoggerConfig, CaptureStage};

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
fn stores_compressed_blob_on_archive_and_indexes_metadata() {
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

    assert_eq!(record.storage, "archive");
    assert!(record.protected);
    assert!(record.archive_path.ends_with(".zst"));
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

    let legacy = fs::read_to_string(root.join("state").join("tap-bodies.jsonl")).unwrap();
    assert!(legacy.contains("\"archive_path\""));
    assert!(!legacy.contains("keep me"));
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

    assert_eq!(record.storage, "spool");
    assert!(PathBuf::from(&record.archive_path).exists());
    assert_eq!(
        logger.read_blob(&record.body_sha256).unwrap(),
        b"body that cannot leave local disk"
    );
    let status = logger.status().unwrap();
    assert!(!status.archive_available);
    assert_eq!(status.spool_backlog, 1);
}

#[test]
fn large_index_without_storage_index_reports_unknown_spool_backlog() {
    let root = temp_root("large-index");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let status = logger.status().unwrap();
    let conn = rusqlite::Connection::open(&status.index_path).unwrap();
    conn.execute(
        "INSERT INTO body_blobs (
            rowid,
            body_sha256,
            body_bytes,
            compressed_bytes,
            storage,
            archive_path,
            protected,
            created_at_unix_ms
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            100_001_i64,
            "large-spool-sha",
            1_i64,
            1_i64,
            "spool",
            "spool:large-spool-sha",
            1_i64,
            1_i64
        ],
    )
    .unwrap();

    let status = logger.status().unwrap();
    assert_eq!(status.blobs, 100_001);
    assert_eq!(status.spool_backlog, 0);
    assert!(!status.spool_backlog_exact);
    assert_eq!(status.status, "ok_spool_unverified");
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
