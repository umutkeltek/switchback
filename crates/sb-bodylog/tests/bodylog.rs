use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sb_bodylog::{BodyEventInput, BodyEventQuery, BodyLogger, BodyLoggerConfig, CaptureStage};

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

    // D3: the tap-bodies record is day-routed into the archive day partition,
    // NOT the configured (now frozen) legacy sink.
    let day_dir = PathBuf::from(&record.archive_path)
        .ancestors()
        .nth(4)
        .unwrap()
        .to_path_buf();
    let routed = fs::read_to_string(day_dir.join("tap-bodies.jsonl")).unwrap();
    assert!(routed.contains("\"archive_path\""));
    assert!(!routed.contains("keep me"));
    // The configured legacy sink is frozen: never created or appended to.
    assert!(!root.join("state").join("tap-bodies.jsonl").exists());
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
    // D4: filesystem-exact backlog counts the spooled blob file AND the
    // spooled tap-bodies day-file (archive down -> tap record day-routes to
    // spool too), both of which a later drain must move.
    assert_eq!(status.spool_backlog, 2);
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
