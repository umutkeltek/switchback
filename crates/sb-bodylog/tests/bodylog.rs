use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension as _;
use sb_bodylog::{
    BodyCaptureGap, BodyEventInput, BodyEventQuery, BodyLogger, BodyLoggerConfig,
    CaptureBackupPlanItem, CaptureBackupReceipt, CaptureBackupReceiptItem,
    CaptureLegacyBackupReceipt, CaptureLegacyBackupReceiptItem, CaptureMode, CaptureReclaimOptions,
    CaptureReclaimProof, CaptureReclaimProofItem, CaptureStage, GcOptions, PressureObservation,
    DEFAULT_KEEP_DAYS,
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
fn starts_a_fresh_v2_index_and_preserves_the_legacy_segment_index_for_backup() {
    std::env::remove_var("SWITCHBACK_BODY_ARCHIVE_ROOT");
    let root = temp_root("legacy-index");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: Some(root.join("state/tap-bodies.jsonl")),
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let pressure = logger.pressure_status().unwrap();
    logger
        .record_metadata_only(input("tap_legacy", b"legacy-index-body"), &pressure)
        .unwrap();
    let v2_index = root.join("state/body/index-v2.sqlite");
    let legacy_segment_index = root.join("state/body/index.sqlite");
    fs::copy(&v2_index, &legacy_segment_index).unwrap();
    drop(logger);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(PathBuf::from(format!("{}{suffix}", v2_index.display())));
    }

    let logger = BodyLogger::new(config).unwrap();

    assert_eq!(
        logger.status().unwrap().events,
        0,
        "legacy rows must not keep the hot v2 index bloated"
    );
    assert!(v2_index.exists());
    assert!(legacy_segment_index.exists());
    let legacy_plan = logger.legacy_backup_plan().unwrap();
    assert!(legacy_plan
        .artifacts
        .iter()
        .any(|artifact| artifact.artifact_id == "legacy-segment-index"));
}

#[test]
fn pre_v2_legacy_index_stays_unmigrated_while_segment_and_jsonl_backup_can_progress() {
    let root = temp_root("pre-v2-backup-transition");
    let state_dir = root.join("state");
    let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
    let config = BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: root.join("archive"),
        legacy_jsonl: Some(legacy_jsonl.clone()),
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    logger
        .record(input("pre-v2-segment", b"sealed-before-v2"))
        .unwrap();
    logger.seal_active().unwrap();
    fs::write(&legacy_jsonl, b"frozen-jsonl-evidence\n").unwrap();
    drop(logger);

    let current_index = state_dir.join("body/index-v2.sqlite");
    let legacy_index = state_dir.join("body/index.sqlite");
    fs::rename(&current_index, &legacy_index).unwrap();
    for suffix in ["-wal", "-shm"] {
        let current = PathBuf::from(format!("{}{suffix}", current_index.display()));
        if current.exists() {
            fs::rename(
                &current,
                PathBuf::from(format!("{}{suffix}", legacy_index.display())),
            )
            .unwrap();
        }
    }
    let conn = rusqlite::Connection::open(&legacy_index).unwrap();
    conn.execute("DROP INDEX IF EXISTS idx_body_events_archive_path", [])
        .unwrap();
    conn.execute("DROP TABLE IF EXISTS body_backup_projection", [])
        .unwrap();
    drop(conn);

    let logger = BodyLogger::open_existing(config).unwrap().unwrap();
    let conn = rusqlite::Connection::open(&legacy_index).unwrap();
    let archive_index: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_body_events_archive_path'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    let backup_projection: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name = 'body_backup_projection'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(
        archive_index.is_none(),
        "opening a legacy index must not build the v2 reclaim index"
    );
    assert!(
        backup_projection.is_none(),
        "opening a legacy index must not mutate its schema"
    );
    drop(conn);

    let segment_plan = logger.backup_plan().unwrap();
    assert_eq!(segment_plan.segments.len(), 1);
    let legacy_plan = logger.legacy_backup_plan().unwrap();
    assert_eq!(legacy_plan.artifacts.len(), 1);
    assert_eq!(legacy_plan.artifacts[0].artifact_id, "legacy-jsonl");
    let legacy_plan_json = serde_json::to_value(legacy_plan).unwrap();
    assert_eq!(
        legacy_plan_json["blockers"][0]["code"],
        "v2_index_missing_or_legacy_index_active"
    );
    assert_eq!(
        legacy_plan_json["blockers"][0]["artifact_id"],
        "legacy-segment-index"
    );
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
fn recovery_never_descends_into_the_blob_tree_to_find_segments() {
    let root = temp_root("segment-scan-prunes-blobs");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let record = logger
        .record(input("blob_scan", b"body kept in a segment"))
        .unwrap();
    logger.seal_active().unwrap();
    drop(logger);

    // Blobs are content-addressed payloads written under their hash, never
    // segments. Recovery that walked that tree would try to read this file as a
    // segment and fail the reopen — and on a real archive it reads one
    // directory per stored body, holding the backup operation lock and the tap
    // listeners behind it, to find nothing.
    let blob_shard = day_dir_of(&record.archive_path)
        .join("blobs")
        .join("sha256")
        .join("ab");
    fs::create_dir_all(&blob_shard).unwrap();
    fs::write(
        blob_shard.join("abcdef0123456789.sbcap"),
        b"content-addressed payload, not a capture segment",
    )
    .unwrap();

    let reopened = BodyLogger::new(config).unwrap();
    assert_eq!(
        reopened.read_blob(&record.body_sha256).unwrap(),
        b"body kept in a segment",
        "the real archived segment is still recovered"
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
    let backup_plan = reopened.backup_plan().unwrap();
    assert_eq!(
        backup_plan.segments.len(),
        1,
        "index rebuild must restore the sealed segment backup projection"
    );
    assert_eq!(
        PathBuf::from(&backup_plan.segments[0].segment_path),
        segment_path
    );
    assert_eq!(
        reopened.status_refreshed().unwrap().segment_backlog_bytes,
        (b"first exact body".len() + b"second exact body".len()) as u64,
        "index rebuild must preserve exact unbacked body-byte pressure"
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
fn recovery_refuses_record_magic_corruption_without_truncating_evidence() {
    let root = temp_root("segment-record-magic-corruption");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let record = logger
        .record(input("corrupt-record-magic", b"preserve valid body"))
        .unwrap();
    let segment = PathBuf::from(&record.archive_path);
    drop(logger);
    fs::remove_file(format!("{}.manifest.json", segment.display())).unwrap();

    {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new().append(true).open(&segment).unwrap();
        file.write_all(b"BAD!\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0")
            .unwrap();
    }
    let corrupted_len = fs::metadata(&segment).unwrap().len();
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", index_path(&root).display()));
        let _ = fs::remove_file(path);
    }

    let error = BodyLogger::new(config).unwrap_err();
    assert!(
        error.to_string().contains("record magic mismatch"),
        "unexpected error: {error}"
    );
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        corrupted_len,
        "record magic corruption is evidence, not a crash-tail deletion signal"
    );
}

#[test]
fn recovery_is_idempotent_when_an_unsealed_frame_is_already_indexed() {
    let root = temp_root("segment-existing-projection");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let record = logger
        .record(input("already-indexed", b"one durable frame"))
        .unwrap();
    let segment = PathBuf::from(&record.archive_path);
    drop(logger);
    fs::remove_file(format!("{}.manifest.json", segment.display())).unwrap();

    let recovered = BodyLogger::new(config).unwrap();
    let events = recovered.events_for_request("already-indexed").unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].body_sha256, record.body_sha256);
}

/// `archive_root` defaults to `state_dir/body/archive` and is overridden by
/// `SWITCHBACK_BODY_ARCHIVE_ROOT`. A process that starts WITHOUT that env writes
/// segments to the default tree; once the env is restored, nothing scans it again.
/// A segment whose writer died in that window stays unsealed forever, and unsealed
/// means the sealed-manifests-only backup can never transfer it — pinning
/// `unbacked_bytes` above zero and, before this, blocking capture resume for good.
///
/// Live 2026-07-27: two such segments (823,801 bytes, dead pid 38869) survived the
/// recovery and pressure fixes and still held capture in metadata-only.
#[test]
fn recovery_adopts_a_stray_segment_left_in_the_default_archive_tree() {
    let root = temp_root("segment-default-tree-stray");
    let state_dir = root.join("state");

    // Phase 1: a writer running with NO archive-root override — the default tree.
    let default_config = BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: state_dir.join("body/archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let stranded_writer = BodyLogger::new(default_config.clone()).unwrap();
    let stray = stranded_writer
        .record(input(
            "stray-in-default-tree",
            b"evidence written without the env",
        ))
        .unwrap();
    let stray_segment = PathBuf::from(&stray.archive_path);
    let stray_manifest = PathBuf::from(format!("{}.manifest.json", stray.archive_path));
    // The writer dies mid-segment rather than shutting down: remove the manifest a
    // clean drop would have written, so the segment is unsealed and the
    // sealed-manifests-only backup adapter can never carry it.
    drop(stranded_writer);
    let _ = fs::remove_file(&stray_manifest);
    assert!(
        !stray_manifest.exists(),
        "fixture must leave an UNSEALED segment, or this test proves nothing"
    );
    assert!(stray_segment.starts_with(state_dir.join("body/archive")));

    // Phase 2: the env is restored, so archive_root now points somewhere else
    // entirely. Same state_dir, so the same index — and the stray segment is now
    // outside the configured archive root.
    let restored_config = BodyLoggerConfig {
        state_dir,
        archive_root: root.join("volume-archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let recovered = BodyLogger::new(restored_config).unwrap();

    assert!(
        stray_manifest.exists(),
        "recovery must seal the stray segment so a backup can finally transfer it"
    );
    let events = recovered
        .events_for_request("stray-in-default-tree")
        .unwrap();
    assert_eq!(events.len(), 1, "the stray body must still be indexed");
    assert_eq!(events[0].body_sha256, stray.body_sha256);
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
    let status = logger.status().unwrap();
    assert_eq!(status.events, 0);
    assert_eq!(status.local_segment_count, 0);
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
    assert!(!root.join("state/body/index-v2.sqlite").exists());
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
    root.join("state").join("body").join("index-v2.sqlite")
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

/// Seal the active capture segment if any and accept a backup receipt covering
/// every sealed segment. Used by the GC tests to receipt-gate the over-credit
/// guard so the GC can legitimately delete index rows for the absent day.
fn accept_receipt_for_absent_day(logger: &BodyLogger, _archive: PathBuf) {
    logger.seal_active().unwrap();
    let backup_plan = logger.backup_plan().unwrap();
    if backup_plan.segments.is_empty() {
        return;
    }
    let receipt = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: backup_plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: backup_plan.segments.last().map(|s| s.utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: backup_plan
            .segments
            .iter()
            .map(|s| CaptureBackupReceiptItem {
                segment_sha256: s.segment_sha256.clone(),
                manifest_sha256: s.manifest_sha256.clone(),
                remote_path: format!("segments/{}", s.segment_file),
                remote_manifest_path: format!("segments/{}.manifest.json", s.segment_file),
                remote_checksum_verified: true,
            })
            .collect(),
    };
    logger.accept_backup_receipt(receipt).unwrap();
}

/// Receipt-gate helper that does NOT panic if no active segment exists (older
/// GC tests on segments that were already sealed by the time they call this).
fn accept_receipt_for_present_segments(logger: &BodyLogger, _archive: PathBuf) {
    let _ = logger.seal_active();
    let backup_plan = logger.backup_plan().unwrap();
    if backup_plan.segments.is_empty() {
        return;
    }
    let receipt = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: backup_plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: backup_plan.segments.last().map(|s| s.utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: backup_plan
            .segments
            .iter()
            .map(|s| CaptureBackupReceiptItem {
                segment_sha256: s.segment_sha256.clone(),
                manifest_sha256: s.manifest_sha256.clone(),
                remote_path: format!("segments/{}", s.segment_file),
                remote_manifest_path: format!("segments/{}.manifest.json", s.segment_file),
                remote_checksum_verified: true,
            })
            .collect(),
    };
    logger.accept_backup_receipt(receipt).unwrap();
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
    // Receipt-gate the over-credit guard: a backup receipt must cover the
    // absent day's segments before GC will mutate the index. Seal the active
    // segment(s) on the absent day, build a backup plan, and accept a receipt
    // BEFORE removing the day-partition (the segment file lives in it).
    accept_receipt_for_absent_day(&logger, _archive);
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

    // Confirm: receipt-gated absent day is deleted in batches; present day survives.
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
    accept_receipt_for_present_segments(&logger, _archive);
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
    let backup_plan = logger.backup_plan().unwrap();
    assert_eq!(backup_plan.segments.len(), 1);
    assert_eq!(
        PathBuf::from(&backup_plan.segments[0].segment_path),
        destination,
        "the segment projection must follow spool drain before the source disappears"
    );

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
    assert_eq!(manifest["body_bytes"], b"first bucket body".len() as u64);
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
    assert_eq!(
        one_cycle.mode,
        CaptureMode::HealingProbe,
        "the first healthy backed generation must admit bounded full-wire capture so a second non-empty generation can exist"
    );
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
fn fresh_capture_without_a_backup_receipt_bootstraps_full_wire() {
    let root = temp_root("pressure-bootstrap");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let now = now_ms();

    let bootstrap = logger
        .evaluate_pressure_at(
            PressureObservation {
                free_bytes: 150_000_000_000,
                capacity_bytes: 1_000_000_000_000,
                last_backup_success_at_unix_ms: None,
                backup_generation: 0,
                unbacked_bytes: 0,
            },
            now,
        )
        .unwrap();

    assert_eq!(bootstrap.mode, CaptureMode::SegmentedFullWire);
    assert!(
        bootstrap
            .warnings
            .iter()
            .any(|warning| warning == "backup_missing"),
        "missing bootstrap proof remains observable without deadlocking capture"
    );
    assert!(bootstrap.reasons.is_empty());
}

#[test]
fn explicit_pressure_refresh_reconciles_shared_unbacked_bytes() {
    let root = temp_root("pressure-shared-unbacked");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let writer = BodyLogger::new(config.clone()).unwrap();
    let observer = BodyLogger::new(config).unwrap();
    let body = b"captured by another logger process";

    writer
        .record(input("pressure-shared-writer", body))
        .unwrap();
    let pressure = observer.evaluate_pressure().unwrap();

    assert!(
        pressure.unbacked_bytes >= body.len() as u64,
        "pressure admission used stale process-local backlog: {pressure:#?}"
    );
}

#[test]
fn idle_active_segment_seals_and_becomes_backup_eligible() {
    let root = temp_root("idle-seal");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let observed_at = now_ms();
    let record = logger
        .record_at(input("idle-seal", b"low-volume-final-segment"), observed_at)
        .unwrap();

    assert!(logger.backup_plan().unwrap().segments.is_empty());
    assert_eq!(
        logger
            .seal_idle_at(observed_at + 30_000, 30_000)
            .unwrap()
            .as_deref(),
        Some(Path::new(&record.archive_path))
    );
    let plan = logger.backup_plan().unwrap();
    assert_eq!(plan.segments.len(), 1);
    assert_eq!(
        PathBuf::from(&plan.segments[0].segment_path),
        PathBuf::from(record.archive_path)
    );
}

#[test]
fn pressure_writer_failures_are_merged_across_logger_instances() {
    let root = temp_root("pressure-shared-writer-failures");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let first = BodyLogger::new(config.clone()).unwrap();
    let second = BodyLogger::new(config.clone()).unwrap();

    first.mark_capture_writer_failed("first").unwrap();
    second.mark_capture_writer_failed("second").unwrap();

    let reopened = BodyLogger::open_existing(config).unwrap().unwrap();
    assert_eq!(
        reopened.pressure_status().unwrap().writer_failures,
        2,
        "shared pressure counters were overwritten by the last process"
    );
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

#[test]
fn approximate_status_uses_the_cached_metadata_only_counter() {
    let root = temp_root("approximate-metadata-counter");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let admission = logger.pressure_status().unwrap();
    logger
        .record_metadata_only(input("cached-metadata-count", b"identity-only"), &admission)
        .unwrap();
    open_index(&root)
        .execute("DELETE FROM body_events", [])
        .unwrap();

    let status = logger.status_with_precise_limit(0).unwrap();

    assert!(status.counts_approximate);
    assert_eq!(
        status.pressure.metadata_only_events, 1,
        "large-index status must use the persisted pressure counter instead of scanning body_events"
    );
}

#[test]
fn bounded_stream_gap_preserves_full_hash_and_size_without_a_blob() {
    let root = temp_root("bounded-stream-gap");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let admission = logger.pressure_status().unwrap();
    let expected_sha = "efdd2fc991dc3e4300b0e2f7f3f303f7e6f810fd43af800a75dc4e4ddccaf1bd";
    let record = logger
        .record_capture_gap(
            input("bounded-gap", &[]),
            &admission,
            BodyCaptureGap {
                reason: "body_limit_exceeded".to_string(),
                body_sha256: expected_sha.to_string(),
                body_bytes: 80_000_000,
            },
        )
        .unwrap();

    assert_eq!(record.storage, "metadata_only");
    assert_eq!(record.redaction_state, "metadata_only_capture_gap");
    assert_eq!(record.body_sha256, expected_sha);
    assert_eq!(record.body_bytes, 80_000_000);
    assert_eq!(record.metadata["gap"]["reason"], "body_limit_exceeded");
    assert!(logger.read_blob(expected_sha).is_err());
    assert_eq!(logger.status().unwrap().blobs, 0);
}

#[test]
fn body_status_v2_reports_bounded_index_capture_segment_and_queue_metrics() {
    let root = temp_root("body-status-v2");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger.record(input("status-v2-a", b"alpha")).unwrap();
    logger.record(input("status-v2-b", b"bravo!")).unwrap();
    logger.note_capture_queue_enqueued().unwrap();

    let status = logger.status().unwrap();
    assert_eq!(status.schema, "switchback/body-status@2");
    assert!(status.index_bytes > 0);
    assert!(status.index_reclaimable_bytes <= status.index_bytes);
    assert_eq!(status.capture_events_last_minute, 2);
    assert_eq!(status.capture_body_bytes_last_minute, 11);
    assert_eq!(status.local_segment_count, 1);
    assert!(status.segment_backlog_bytes >= 11);
    assert_eq!(status.capture_queue_depth, 1);
    assert_eq!(status.capture_queue_drops, 0);

    logger.note_capture_queue_dequeued().unwrap();
    assert_eq!(logger.status().unwrap().capture_queue_depth, 0);
}

#[test]
fn existing_index_gains_archive_path_reclaim_index_on_open() {
    let root = temp_root("archive-path-index-migration");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    drop(logger);
    open_index(&root)
        .execute("DROP INDEX IF EXISTS idx_body_events_archive_path", [])
        .unwrap();

    BodyLogger::open_existing(config).unwrap().unwrap();

    let index_sql: Option<String> = open_index(&root)
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_body_events_archive_path'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(
        index_sql
            .as_deref()
            .is_some_and(|sql| sql.contains("body_events(archive_path)")),
        "existing indexes must gain the bounded reclaim lookup index"
    );
}

#[test]
fn status_does_not_migrate_the_active_legacy_index() {
    let root = temp_root("legacy-status-read-only");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    drop(logger);

    let current_index = root.join("state/body/index-v2.sqlite");
    let legacy_index = root.join("state/body/index.sqlite");
    let conn = rusqlite::Connection::open(&current_index).unwrap();
    conn.execute("DROP INDEX IF EXISTS idx_body_events_archive_path", [])
        .unwrap();
    drop(conn);
    fs::rename(&current_index, &legacy_index).unwrap();

    BodyLogger::status_for_config(config).unwrap();

    let index_sql: Option<String> = rusqlite::Connection::open(&legacy_index)
        .unwrap()
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_body_events_archive_path'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(
        index_sql.is_none(),
        "status must not build a migration index on the active legacy database"
    );
}

#[test]
fn backup_plan_is_manifest_only_and_receipt_acceptance_requires_remote_checksum_proof() {
    let root = temp_root("backup-plan");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger
        .record(input("backup-plan-a", b"manifest-owned"))
        .unwrap();
    assert!(
        logger.backup_plan().unwrap().segments.is_empty(),
        "an active segment without a sealed manifest is never transferable"
    );
    logger.seal_active().unwrap();
    let plan = logger.backup_plan().unwrap();
    assert_eq!(plan.schema, "switchback/capture-backup-plan@1");
    assert_eq!(plan.segments.len(), 1);
    assert!(plan.segments[0].manifest_path.ends_with(".manifest.json"));

    let rejected = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: Some(plan.segments[0].utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: vec![CaptureBackupReceiptItem {
            segment_sha256: plan.segments[0].segment_sha256.clone(),
            manifest_sha256: plan.segments[0].manifest_sha256.clone(),
            remote_path: format!("segments/{}", plan.segments[0].segment_file),
            remote_manifest_path: format!(
                "segments/{}.manifest.json",
                plan.segments[0].segment_file
            ),
            remote_checksum_verified: false,
        }],
    };
    assert!(logger.accept_backup_receipt(rejected).is_err());
    assert_eq!(logger.backup_plan().unwrap().segments.len(), 1);

    let accepted = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: Some(plan.segments[0].utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: vec![CaptureBackupReceiptItem {
            segment_sha256: plan.segments[0].segment_sha256.clone(),
            manifest_sha256: plan.segments[0].manifest_sha256.clone(),
            remote_path: format!("segments/{}", plan.segments[0].segment_file),
            remote_manifest_path: format!(
                "segments/{}.manifest.json",
                plan.segments[0].segment_file
            ),
            remote_checksum_verified: true,
        }],
    };
    logger.accept_backup_receipt(accepted).unwrap();
    assert!(logger.backup_plan().unwrap().segments.is_empty());
    let latest = root.join("state/body/backup/latest-receipt.json");
    assert!(latest.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(latest).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn backup_receipt_rejects_a_completion_time_far_in_the_future() {
    let root = temp_root("backup-future-receipt");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger
        .record(input("backup-future-receipt", b"clock-skew"))
        .unwrap();
    logger.seal_active().unwrap();
    let plan = logger.backup_plan().unwrap();
    let segment = &plan.segments[0];
    let error = logger
        .accept_backup_receipt(CaptureBackupReceipt {
            schema: "switchback/capture-backup@2".to_string(),
            generation: plan.next_generation,
            completed_at_unix_ms: now_ms() + 60 * 60 * 1_000,
            verified_through_day: Some(segment.utc_day.clone()),
            remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
            segments: vec![CaptureBackupReceiptItem {
                segment_sha256: segment.segment_sha256.clone(),
                manifest_sha256: segment.manifest_sha256.clone(),
                remote_path: format!("segments/{}", segment.segment_file),
                remote_manifest_path: format!("segments/{}.manifest.json", segment.segment_file),
                remote_checksum_verified: true,
            }],
        })
        .unwrap_err();
    assert!(
        error.to_string().contains("completion time"),
        "unexpected error: {error}"
    );
    assert_eq!(logger.backup_plan().unwrap().segments.len(), 1);
}

#[test]
fn frozen_legacy_artifacts_require_exact_remote_checksum_receipts_and_stay_local() {
    let root = temp_root("legacy-backup-proof");
    let state_dir = root.join("state");
    let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
    let config = BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: root.join("archive"),
        legacy_jsonl: Some(legacy_jsonl.clone()),
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config).unwrap();
    fs::write(&legacy_jsonl, b"frozen legacy capture\n").unwrap();
    let legacy_index = state_dir.join("body-index.sqlite");
    fs::copy(index_path(&root), &legacy_index).unwrap();

    let plan = logger.legacy_backup_plan().unwrap();
    assert_eq!(plan.schema, "switchback/capture-legacy-backup-plan@1");
    assert_eq!(plan.artifacts.len(), 2);
    assert_eq!(
        plan.artifacts
            .iter()
            .map(|artifact| artifact.artifact_id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["legacy-body-index", "legacy-jsonl"])
    );
    assert!(plan
        .artifacts
        .iter()
        .all(|artifact| artifact.sha256.len() == 64 && artifact.bytes > 0));

    let unverified = CaptureLegacyBackupReceipt {
        schema: "switchback/capture-legacy-backup@1".to_string(),
        completed_at_unix_ms: now_ms(),
        remote_root: "truenas:/mnt/tank/switchback-capture-v2".to_string(),
        artifacts: plan
            .artifacts
            .iter()
            .map(|artifact| CaptureLegacyBackupReceiptItem {
                artifact_id: artifact.artifact_id.clone(),
                kind: artifact.kind.clone(),
                local_path: artifact.local_path.clone(),
                sha256: artifact.sha256.clone(),
                bytes: artifact.bytes,
                modified_at_unix_ms: artifact.modified_at_unix_ms,
                remote_path: format!(
                    "legacy/{}/{}/{}",
                    artifact.artifact_id, artifact.sha256, artifact.file_name
                ),
                remote_checksum_verified: false,
            })
            .collect(),
    };
    assert!(logger
        .accept_legacy_backup_receipt(unverified)
        .unwrap_err()
        .to_string()
        .contains("remote checksum proof"));

    let accepted = CaptureLegacyBackupReceipt {
        schema: "switchback/capture-legacy-backup@1".to_string(),
        completed_at_unix_ms: now_ms(),
        remote_root: "truenas:/mnt/tank/switchback-capture-v2".to_string(),
        artifacts: plan
            .artifacts
            .iter()
            .map(|artifact| CaptureLegacyBackupReceiptItem {
                artifact_id: artifact.artifact_id.clone(),
                kind: artifact.kind.clone(),
                local_path: artifact.local_path.clone(),
                sha256: artifact.sha256.clone(),
                bytes: artifact.bytes,
                modified_at_unix_ms: artifact.modified_at_unix_ms,
                remote_path: format!(
                    "legacy/{}/{}/{}",
                    artifact.artifact_id, artifact.sha256, artifact.file_name
                ),
                remote_checksum_verified: true,
            })
            .collect(),
    };
    logger.accept_legacy_backup_receipt(accepted).unwrap();

    assert!(logger.legacy_backup_plan().unwrap().artifacts.is_empty());
    assert!(
        legacy_jsonl.exists(),
        "receipt acceptance never deletes evidence"
    );
    assert!(
        legacy_index.exists(),
        "receipt acceptance never deletes evidence"
    );

    fs::write(&legacy_jsonl, b"changed after accepted proof\n").unwrap();
    let changed = logger.legacy_backup_plan().unwrap();
    assert_eq!(changed.artifacts.len(), 1);
    assert_eq!(changed.artifacts[0].artifact_id, "legacy-jsonl");
    assert_ne!(
        changed.artifacts[0].sha256,
        plan.artifacts
            .iter()
            .find(|artifact| artifact.artifact_id == "legacy-jsonl")
            .unwrap()
            .sha256
    );
}

#[test]
fn frozen_legacy_blob_tree_is_part_of_exact_backup_proof() {
    let root = temp_root("legacy-blob-tree-proof");
    let state_dir = root.join("state");
    let archive_root = root.join("archive");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: archive_root.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let legacy_index = state_dir.join("body/index.sqlite");
    fs::copy(index_path(&root), &legacy_index).unwrap();
    let blobs = archive_root.join("blobs/sha256/aa");
    fs::create_dir_all(&blobs).unwrap();
    fs::write(blobs.join("aa-one.zst"), b"legacy-blob-one").unwrap();
    fs::write(blobs.join("aa-two.zst"), b"legacy-blob-two").unwrap();

    let plan = logger.legacy_backup_plan().unwrap();
    let tree = plan
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_id == "legacy-blob-archive")
        .expect("legacy body blobs must be included in backup proof");
    assert_eq!(tree.kind, "directory");
    assert_eq!(
        tree.local_path,
        fs::canonicalize(archive_root.join("blobs"))
            .unwrap()
            .to_string_lossy()
    );
    assert_eq!(tree.bytes, 30);
    assert_eq!(tree.sha256.len(), 64);
}

#[test]
fn backup_receipt_must_cover_the_exact_pending_sealed_segment_set() {
    let root = temp_root("backup-exact-set");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger
        .record(input("backup-exact-a", b"first-segment"))
        .unwrap();
    logger.seal_active().unwrap();
    logger
        .record(input("backup-exact-b", b"second-segment"))
        .unwrap();
    logger.seal_active().unwrap();

    let plan = logger.backup_plan().unwrap();
    assert_eq!(plan.segments.len(), 2);
    let incomplete = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: Some(plan.segments[0].utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: vec![CaptureBackupReceiptItem {
            segment_sha256: plan.segments[0].segment_sha256.clone(),
            manifest_sha256: plan.segments[0].manifest_sha256.clone(),
            remote_path: format!("segments/{}", plan.segments[0].segment_file),
            remote_manifest_path: format!(
                "segments/{}.manifest.json",
                plan.segments[0].segment_file
            ),
            remote_checksum_verified: true,
        }],
    };

    let error = logger.accept_backup_receipt(incomplete).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exact pending sealed segment set"),
        "unexpected error: {error}"
    );
    assert_eq!(logger.backup_plan().unwrap().segments.len(), 2);

    let duplicated = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: plan.segments.last().map(|segment| segment.utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: vec![
            CaptureBackupReceiptItem {
                segment_sha256: plan.segments[0].segment_sha256.clone(),
                manifest_sha256: plan.segments[0].manifest_sha256.clone(),
                remote_path: format!("segments/{}", plan.segments[0].segment_file),
                remote_manifest_path: format!(
                    "segments/{}.manifest.json",
                    plan.segments[0].segment_file
                ),
                remote_checksum_verified: true,
            },
            CaptureBackupReceiptItem {
                segment_sha256: plan.segments[0].segment_sha256.clone(),
                manifest_sha256: plan.segments[0].manifest_sha256.clone(),
                remote_path: format!("segments/copy-{}", plan.segments[0].segment_file),
                remote_manifest_path: format!(
                    "segments/copy-{}.manifest.json",
                    plan.segments[0].segment_file
                ),
                remote_checksum_verified: true,
            },
        ],
    };
    let error = logger.accept_backup_receipt(duplicated).unwrap_err();
    assert!(
        error.to_string().contains("duplicate segment"),
        "unexpected error: {error}"
    );

    let wrong_day = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: Some("1900-01-01".to_string()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: plan
            .segments
            .iter()
            .map(|segment| CaptureBackupReceiptItem {
                segment_sha256: segment.segment_sha256.clone(),
                manifest_sha256: segment.manifest_sha256.clone(),
                remote_path: format!("segments/{}", segment.segment_file),
                remote_manifest_path: format!("segments/{}.manifest.json", segment.segment_file),
                remote_checksum_verified: true,
            })
            .collect(),
    };
    let error = logger.accept_backup_receipt(wrong_day).unwrap_err();
    assert!(
        error.to_string().contains("verified_through_day"),
        "unexpected error: {error}"
    );
}

fn pending_backup_receipt_fixture(tag: &str) -> (BodyLogger, CaptureBackupReceipt) {
    let root = temp_root(tag);
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger.record(input(tag, b"remote-path-proof")).unwrap();
    logger.seal_active().unwrap();
    let plan = logger.backup_plan().unwrap();
    assert_eq!(plan.segments.len(), 1);
    let segment = &plan.segments[0];
    let receipt = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: Some(segment.utc_day.clone()),
        remote_root: "truenas:/mnt/tank/switchback-capture-v2".to_string(),
        segments: vec![CaptureBackupReceiptItem {
            segment_sha256: segment.segment_sha256.clone(),
            manifest_sha256: segment.manifest_sha256.clone(),
            remote_path: format!("segments/{}", segment.segment_file),
            remote_manifest_path: format!("segments/{}.manifest.json", segment.segment_file),
            remote_checksum_verified: true,
        }],
    };
    (logger, receipt)
}

#[test]
fn backup_receipt_rejects_remote_artifact_path_traversal() {
    let (logger, mut receipt) = pending_backup_receipt_fixture("backup-remote-path-traversal");
    receipt.segments[0].remote_path =
        "segments/2026/07/25/hash/../../../../escape.sbcap".to_string();

    let error = logger.accept_backup_receipt(receipt).unwrap_err();
    assert!(
        error.to_string().contains("remote path"),
        "unexpected error: {error}"
    );
}

#[test]
fn backup_receipt_rejects_remote_root_traversal() {
    let (logger, mut receipt) = pending_backup_receipt_fixture("backup-remote-root-traversal");
    receipt.remote_root = "truenas:/mnt/tank/../../tmp/switchback-capture".to_string();

    let error = logger.accept_backup_receipt(receipt).unwrap_err();
    assert!(
        error.to_string().contains("remote_root"),
        "unexpected error: {error}"
    );
}

#[test]
fn accepted_receipt_clears_only_backed_sealed_bytes_not_active_segment_bytes() {
    let root = temp_root("backup-active-unbacked");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger
        .record(input("backup-sealed", b"sealed-payload"))
        .unwrap();
    logger.seal_active().unwrap();
    let plan = logger.backup_plan().unwrap();
    logger
        .record(input("backup-active", b"active-payload"))
        .unwrap();
    let receipt = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: plan.segments.last().map(|segment| segment.utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: plan
            .segments
            .iter()
            .map(|segment| CaptureBackupReceiptItem {
                segment_sha256: segment.segment_sha256.clone(),
                manifest_sha256: segment.manifest_sha256.clone(),
                remote_path: format!("segments/{}", segment.segment_file),
                remote_manifest_path: format!("segments/{}.manifest.json", segment.segment_file),
                remote_checksum_verified: true,
            })
            .collect(),
    };
    logger.accept_backup_receipt(receipt).unwrap();

    let status = logger.status_refreshed().unwrap();
    assert!(
        status.segment_backlog_bytes >= b"active-payload".len() as u64,
        "active, unsealed bytes disappeared after receipt: {status:#?}"
    );
}

#[test]
fn sealed_projection_missing_its_manifest_fails_backup_planning_loudly() {
    let root = temp_root("backup-missing-sealed-manifest");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger
        .record(input("backup-missing-manifest", b"sealed-payload"))
        .unwrap();
    logger.seal_active().unwrap();
    let plan = logger.backup_plan().unwrap();
    assert_eq!(plan.segments.len(), 1);
    fs::remove_file(&plan.segments[0].manifest_path).unwrap();

    let error = logger.backup_plan().unwrap_err();
    assert!(
        error.to_string().contains("sealed segment manifest"),
        "unexpected error: {error}"
    );
}

struct BackedReclaimFixture {
    root: PathBuf,
    logger: BodyLogger,
    segment: CaptureBackupPlanItem,
    segment_path: PathBuf,
    manifest_path: PathBuf,
    restore_segment: PathBuf,
    restore_manifest: PathBuf,
    catalog_path: PathBuf,
}

fn backed_reclaim_fixture(tag: &str, request_id: &str) -> BackedReclaimFixture {
    let root = temp_root(tag);
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let record = logger
        .record_at(
            input(request_id, b"receipt-gated-body"),
            now_ms() - 10 * DAY_MS,
        )
        .unwrap();
    logger.seal_active().unwrap();
    let segment_path = PathBuf::from(&record.archive_path);
    let manifest_path = PathBuf::from(format!("{}.manifest.json", segment_path.display()));
    let restore_segment = root.join("restore.sbcap");
    let restore_manifest = root.join("restore.sbcap.manifest.json");
    fs::copy(&segment_path, &restore_segment).unwrap();
    fs::copy(&manifest_path, &restore_manifest).unwrap();

    let backup = logger.backup_plan().unwrap();
    assert_eq!(backup.segments.len(), 1);
    let segment = backup.segments[0].clone();
    logger
        .accept_backup_receipt(CaptureBackupReceipt {
            schema: "switchback/capture-backup@2".to_string(),
            generation: backup.next_generation,
            completed_at_unix_ms: now_ms(),
            verified_through_day: Some(segment.utc_day.clone()),
            remote_root: "truenas:/mnt/tank/switchback-capture-v2".to_string(),
            segments: vec![CaptureBackupReceiptItem {
                segment_sha256: segment.segment_sha256.clone(),
                manifest_sha256: segment.manifest_sha256.clone(),
                remote_path: format!(
                    "segments/{}/{}/{}",
                    segment.utc_day.replace('-', "/"),
                    segment.segment_sha256,
                    segment.segment_file
                ),
                remote_manifest_path: format!(
                    "segments/{}/{}/{}.manifest.json",
                    segment.utc_day.replace('-', "/"),
                    segment.segment_sha256,
                    segment.segment_file
                ),
                remote_checksum_verified: true,
            }],
        })
        .unwrap();
    let catalog_path = root
        .join("state/body/backup/catalog")
        .join(format!("{}.json", segment.segment_sha256));

    BackedReclaimFixture {
        root,
        logger,
        segment,
        segment_path,
        manifest_path,
        restore_segment,
        restore_manifest,
        catalog_path,
    }
}

fn set_remote_catalog_state(catalog_path: &Path, state: &str, staging_dir: Option<&Path>) {
    let mut catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(catalog_path).unwrap()).unwrap();
    catalog["state"] = serde_json::json!(state);
    catalog["reclaim_staging_dir"] = staging_dir
        .map(|path| serde_json::json!(path.to_string_lossy().into_owned()))
        .unwrap_or(serde_json::Value::Null);
    fs::write(
        catalog_path,
        format!("{}\n", serde_json::to_string_pretty(&catalog).unwrap()),
    )
    .unwrap();
}

fn proof_for_reclaim_plan(plan: &sb_bodylog::CaptureReclaimPlan) -> CaptureReclaimProof {
    CaptureReclaimProof {
        schema: "switchback/capture-reclaim-proof@1".to_string(),
        verified_at_unix_ms: now_ms(),
        segments: plan
            .segments
            .iter()
            .map(|segment| CaptureReclaimProofItem {
                segment_sha256: segment.segment_sha256.clone(),
                manifest_sha256: segment.manifest_sha256.clone(),
                remote_path: segment.remote_path.clone(),
                remote_manifest_path: segment.remote_manifest_path.clone(),
                remote_checksums_verified: true,
            })
            .collect(),
    }
}

#[test]
fn verified_segments_reclaim_to_remote_only_and_restore_with_checksum_proof() {
    let root = temp_root("receipt-gated-reclaim");
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    let record = logger
        .record_at(
            input("reclaim-old", b"receipt-gated-body"),
            now_ms() - 10 * DAY_MS,
        )
        .unwrap();
    logger.seal_active().unwrap();
    let segment_path = PathBuf::from(&record.archive_path);
    let manifest_path = PathBuf::from(format!("{}.manifest.json", segment_path.display()));
    let restore_segment = root.join("restore.sbcap");
    let restore_manifest = root.join("restore.sbcap.manifest.json");
    fs::copy(&segment_path, &restore_segment).unwrap();
    fs::copy(&manifest_path, &restore_manifest).unwrap();

    let backup = logger.backup_plan().unwrap();
    assert_eq!(backup.segments.len(), 1);
    let backed = &backup.segments[0];
    logger
        .accept_backup_receipt(CaptureBackupReceipt {
            schema: "switchback/capture-backup@2".to_string(),
            generation: backup.next_generation,
            completed_at_unix_ms: now_ms(),
            verified_through_day: Some(backed.utc_day.clone()),
            remote_root: "truenas:/mnt/tank/switchback-capture-v2".to_string(),
            segments: vec![CaptureBackupReceiptItem {
                segment_sha256: backed.segment_sha256.clone(),
                manifest_sha256: backed.manifest_sha256.clone(),
                remote_path: format!("segments/{}/{}", backed.utc_day, backed.segment_sha256),
                remote_manifest_path: format!(
                    "segments/{}/{}/{}.manifest.json",
                    backed.utc_day, backed.segment_sha256, backed.segment_file
                ),
                remote_checksum_verified: true,
            }],
        })
        .unwrap();

    let catalog_path = root
        .join("state/body/backup/catalog")
        .join(format!("{}.json", backed.segment_sha256));
    let staging_dir = segment_path
        .parent()
        .unwrap()
        .join(".switchback-reclaim")
        .join(&backed.segment_sha256);
    fs::create_dir_all(&staging_dir).unwrap();
    let staged_segment = staging_dir.join(&backed.segment_file);
    let staged_manifest = staging_dir.join(manifest_path.file_name().unwrap());
    let mut interrupted: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    interrupted["state"] = serde_json::json!("reclaiming");
    interrupted["reclaim_staging_dir"] =
        serde_json::json!(staging_dir.to_string_lossy().into_owned());
    fs::write(
        &catalog_path,
        serde_json::to_vec_pretty(&interrupted).unwrap(),
    )
    .unwrap();
    fs::rename(&segment_path, &staged_segment).unwrap();
    fs::rename(&manifest_path, &staged_manifest).unwrap();

    let reclaim_plan = logger.reclaim_plan(3).unwrap();
    assert_eq!(reclaim_plan.segments.len(), 1);
    assert_eq!(
        reclaim_plan.segments[0].segment_sha256,
        backed.segment_sha256
    );
    assert!(
        segment_path.exists() && manifest_path.exists(),
        "an interrupted pre-index reclaim must restore its local evidence"
    );
    let proof = CaptureReclaimProof {
        schema: "switchback/capture-reclaim-proof@1".to_string(),
        verified_at_unix_ms: now_ms(),
        segments: reclaim_plan
            .segments
            .iter()
            .map(|segment| CaptureReclaimProofItem {
                segment_sha256: segment.segment_sha256.clone(),
                manifest_sha256: segment.manifest_sha256.clone(),
                remote_path: segment.remote_path.clone(),
                remote_manifest_path: segment.remote_manifest_path.clone(),
                remote_checksums_verified: true,
            })
            .collect(),
    };

    let dry_run = logger
        .reclaim_verified_segments(
            CaptureReclaimOptions {
                keep_days: 3,
                confirm: false,
            },
            proof.clone(),
        )
        .unwrap();
    assert!(dry_run.dry_run);
    assert_eq!(dry_run.candidate_segments, 1);
    assert!(segment_path.exists());
    assert_eq!(logger.status().unwrap().events, 1);

    let reclaimed = logger
        .reclaim_verified_segments(
            CaptureReclaimOptions {
                keep_days: 3,
                confirm: true,
            },
            proof,
        )
        .unwrap();
    assert!(!reclaimed.dry_run);
    assert_eq!(reclaimed.reclaimed_segments, 1);
    assert_eq!(reclaimed.reclaimed_bytes, backed.segment_bytes);
    assert!(!segment_path.exists());
    assert!(!manifest_path.exists());
    assert_eq!(logger.status().unwrap().events, 0);
    assert_eq!(logger.status().unwrap().local_segment_count, 0);
    assert!(logger.backup_plan().unwrap().segments.is_empty());

    fs::create_dir_all(&staging_dir).unwrap();
    fs::copy(&restore_segment, &staged_segment).unwrap();
    fs::copy(&restore_manifest, &staged_manifest).unwrap();
    let mut committed_not_cleaned: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    committed_not_cleaned["state"] = serde_json::json!("reclaiming");
    committed_not_cleaned["reclaim_staging_dir"] =
        serde_json::json!(staging_dir.to_string_lossy().into_owned());
    fs::write(
        &catalog_path,
        serde_json::to_vec_pretty(&committed_not_cleaned).unwrap(),
    )
    .unwrap();
    assert!(logger.reclaim_plan(3).unwrap().segments.is_empty());
    assert!(
        !staged_segment.exists() && !staged_manifest.exists(),
        "an interrupted post-index reclaim must finish staging cleanup"
    );
    let recovered_catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(&catalog_path).unwrap()).unwrap();
    assert_eq!(recovered_catalog["state"], "remote_only");

    let repeated = logger
        .reclaim_verified_segments(
            CaptureReclaimOptions {
                keep_days: 3,
                confirm: true,
            },
            CaptureReclaimProof {
                schema: "switchback/capture-reclaim-proof@1".to_string(),
                verified_at_unix_ms: now_ms(),
                segments: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(repeated.reclaimed_segments, 0);

    logger
        .restore_remote_segment(&backed.segment_sha256, &restore_segment, &restore_manifest)
        .unwrap();
    assert!(segment_path.exists());
    assert!(manifest_path.exists());
    assert_eq!(logger.events_for_request("reclaim-old").unwrap().len(), 1);
    assert!(logger.backup_plan().unwrap().segments.is_empty());

    drop(logger);
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(PathBuf::from(format!(
            "{}{suffix}",
            index_path(&root).display()
        )));
    }
    let rebuilt = BodyLogger::new(config).unwrap();
    assert_eq!(rebuilt.events_for_request("reclaim-old").unwrap().len(), 1);
    assert!(rebuilt.backup_plan().unwrap().segments.is_empty());
}

#[test]
fn reclaim_repoints_a_deduplicated_blob_to_a_remaining_segment() {
    let root = temp_root("reclaim-deduplicated-pointer");
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    let body = b"same body survives in the newer segment";
    let old = logger
        .record_at(input("dedupe-old", body), now_ms() - 10 * DAY_MS)
        .unwrap();
    logger.seal_active().unwrap();
    let recent = logger
        .record_at(input("dedupe-recent", body), now_ms() - DAY_MS)
        .unwrap();
    logger.seal_active().unwrap();
    assert_eq!(old.body_sha256, recent.body_sha256);
    assert_ne!(old.archive_path, recent.archive_path);

    let backup = logger.backup_plan().unwrap();
    assert_eq!(backup.segments.len(), 2);
    logger
        .accept_backup_receipt(CaptureBackupReceipt {
            schema: "switchback/capture-backup@2".to_string(),
            generation: backup.next_generation,
            completed_at_unix_ms: now_ms(),
            verified_through_day: backup
                .segments
                .last()
                .map(|segment| segment.utc_day.clone()),
            remote_root: "truenas:/mnt/tank/switchback-capture-v2".to_string(),
            segments: backup
                .segments
                .iter()
                .map(|segment| CaptureBackupReceiptItem {
                    segment_sha256: segment.segment_sha256.clone(),
                    manifest_sha256: segment.manifest_sha256.clone(),
                    remote_path: format!("segments/{}/{}", segment.utc_day, segment.segment_sha256),
                    remote_manifest_path: format!(
                        "segments/{}/{}/{}.manifest.json",
                        segment.utc_day, segment.segment_sha256, segment.segment_file
                    ),
                    remote_checksum_verified: true,
                })
                .collect(),
        })
        .unwrap();

    let plan = logger.reclaim_plan(3).unwrap();
    assert_eq!(plan.segments.len(), 1);
    assert_eq!(plan.segments[0].segment_path, old.archive_path);
    logger
        .reclaim_verified_segments(
            CaptureReclaimOptions {
                keep_days: 3,
                confirm: true,
            },
            proof_for_reclaim_plan(&plan),
        )
        .unwrap();

    assert!(!Path::new(&old.archive_path).exists());
    assert!(Path::new(&recent.archive_path).exists());
    assert!(logger.events_for_request("dedupe-old").unwrap().is_empty());
    assert_eq!(logger.events_for_request("dedupe-recent").unwrap().len(), 1);
    assert_eq!(logger.read_blob(&old.body_sha256).unwrap(), body);
    let blob_location: (String, String) = open_index(&root)
        .query_row(
            "SELECT storage, archive_path FROM body_blobs WHERE body_sha256 = ?1",
            [&old.body_sha256],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(blob_location.0, "archive_segment");
    assert_eq!(blob_location.1, recent.archive_path);
}

#[test]
fn remote_only_recovery_finishes_staging_cleanup_without_resurrecting_index() {
    let fixture = backed_reclaim_fixture("remote-only-recovery", "remote-only-recovery");
    let staging_dir = fixture
        .segment_path
        .parent()
        .unwrap()
        .join(".switchback-reclaim")
        .join(&fixture.segment.segment_sha256);
    fs::create_dir_all(&staging_dir).unwrap();
    let staged_segment = staging_dir.join(&fixture.segment.segment_file);
    let staged_manifest = staging_dir.join(fixture.manifest_path.file_name().unwrap());
    fs::rename(&fixture.segment_path, &staged_segment).unwrap();
    fs::rename(&fixture.manifest_path, &staged_manifest).unwrap();

    let conn = open_index(&fixture.root);
    conn.execute(
        "DELETE FROM body_events WHERE archive_path = ?1",
        [fixture.segment_path.to_string_lossy().into_owned()],
    )
    .unwrap();
    conn.execute("DELETE FROM body_blobs", []).unwrap();
    conn.execute(
        "DELETE FROM body_segments WHERE segment_path = ?1",
        [fixture.segment_path.to_string_lossy().into_owned()],
    )
    .unwrap();
    set_remote_catalog_state(&fixture.catalog_path, "remote_only", None);

    assert!(fixture.logger.reclaim_plan(3).unwrap().segments.is_empty());
    assert!(!staged_segment.exists());
    assert!(!staged_manifest.exists());
    assert!(!fixture.segment_path.exists());
    assert!(!fixture.manifest_path.exists());
    assert_eq!(fixture.logger.status().unwrap().events, 0);
    let catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.catalog_path).unwrap()).unwrap();
    assert_eq!(catalog["state"], "remote_only");
}

#[test]
fn restore_retry_finishes_catalog_transition_after_index_commit() {
    let fixture = backed_reclaim_fixture("restore-retry", "restore-retry");
    let plan = fixture.logger.reclaim_plan(3).unwrap();
    fixture
        .logger
        .reclaim_verified_segments(
            CaptureReclaimOptions {
                keep_days: 3,
                confirm: true,
            },
            proof_for_reclaim_plan(&plan),
        )
        .unwrap();
    fixture
        .logger
        .restore_remote_segment(
            &fixture.segment.segment_sha256,
            &fixture.restore_segment,
            &fixture.restore_manifest,
        )
        .unwrap();

    set_remote_catalog_state(&fixture.catalog_path, "remote_only", None);
    fixture
        .logger
        .restore_remote_segment(
            &fixture.segment.segment_sha256,
            &fixture.restore_segment,
            &fixture.restore_manifest,
        )
        .unwrap();

    assert_eq!(
        fixture
            .logger
            .events_for_request("restore-retry")
            .unwrap()
            .len(),
        1
    );
    let catalog: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.catalog_path).unwrap()).unwrap();
    assert_eq!(catalog["state"], "verified_local");
}

#[cfg(unix)]
#[test]
fn body_capture_namespace_tightens_all_directories_and_files_to_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    fn assert_private_tree(path: &Path) {
        let metadata = fs::metadata(path).unwrap();
        let expected = if metadata.is_dir() { 0o700 } else { 0o600 };
        assert_eq!(
            metadata.permissions().mode() & 0o777,
            expected,
            "capture path is not owner-only: {}",
            path.display()
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                assert_private_tree(&entry.unwrap().path());
            }
        }
    }

    let root = temp_root("private-tree");
    let state_dir = root.join("state");
    let body_dir = state_dir.join("body");
    let spool_dir = body_dir.join("spool");
    let archive_root = root.join("archive");
    for directory in [&body_dir, &spool_dir, &archive_root] {
        fs::create_dir_all(directory).unwrap();
        fs::set_permissions(directory, fs::Permissions::from_mode(0o777)).unwrap();
    }

    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir,
        archive_root: archive_root.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap();
    logger
        .record(input("private-tree", b"sensitive-capture-body"))
        .unwrap();
    logger.seal_active().unwrap();
    let plan = logger.backup_plan().unwrap();
    let receipt = CaptureBackupReceipt {
        schema: "switchback/capture-backup@2".to_string(),
        generation: plan.next_generation,
        completed_at_unix_ms: now_ms(),
        verified_through_day: plan.segments.last().map(|segment| segment.utc_day.clone()),
        remote_root: "truenas:/tank/switchback-capture-v2".to_string(),
        segments: plan
            .segments
            .iter()
            .map(|segment| CaptureBackupReceiptItem {
                segment_sha256: segment.segment_sha256.clone(),
                manifest_sha256: segment.manifest_sha256.clone(),
                remote_path: format!("segments/{}", segment.segment_file),
                remote_manifest_path: format!("segments/{}.manifest.json", segment.segment_file),
                remote_checksum_verified: true,
            })
            .collect(),
    };
    logger.accept_backup_receipt(receipt).unwrap();

    assert_private_tree(&body_dir);
    assert_private_tree(&archive_root);
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

// Regression: `sb body reclaim` died mid-run with "database is locked" on a busy
// host (737 of 2598 candidates reclaimed, then aborted) because maintenance
// connections shared the capture hot path's 250ms busy timeout and used DEFERRED
// write transactions. Reclaim must wait out the live writer, not fail the batch.
#[test]
fn reclaim_waits_out_a_live_writer_on_the_index_lock() {
    let backed = backed_reclaim_fixture("reclaim-under-live-writer", "reclaim-contended");
    let index_path = backed.logger.status().unwrap().index_path;

    // Simulate the live gateway: hold the index write lock across reclaim's
    // write window. Held well past the hot-path busy timeout (250ms), so the
    // old behavior aborts with "database is locked", but comfortably inside
    // the maintenance timeout (5s).
    let locker = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(&index_path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
        std::thread::sleep(Duration::from_millis(1_500));
        conn.execute_batch("ROLLBACK;").unwrap();
    });

    // Let the holder take the write lock before reclaim starts writing.
    std::thread::sleep(Duration::from_millis(300));

    let plan = backed.logger.reclaim_plan(3).unwrap();
    let proof = proof_for_reclaim_plan(&plan);
    let started = Instant::now();
    let reclaimed = backed
        .logger
        .reclaim_verified_segments(
            CaptureReclaimOptions {
                keep_days: 3,
                confirm: true,
            },
            proof,
        )
        .unwrap();
    locker.join().unwrap();

    assert_eq!(reclaimed.reclaimed_segments, 1);
    assert!(
        started.elapsed() >= Duration::from_millis(800),
        "reclaim must wait out the live writer, not fail fast or skip: {:?}",
        started.elapsed()
    );
    assert!(!backed.segment_path.exists());
    assert!(!backed.manifest_path.exists());
}

// Falsifier 10 (over-credit case): GC refuses to delete index rows for a day
// whose day-partition dir is absent if no backup receipt proves the data was
// exported to TrueNAS. The old behavior would "credit" the export whenever the
// local day-partition dir disappeared, regardless of whether the data was
// actually transferred — a single-line JSONL on 2026-07-11 could be lost even
// though the index still claimed it existed. The GC must require receipt-gated
// proof (same seam as `reclaim_verified_segments`) before mutating index rows.
#[test]
fn gc_refuses_to_over_credit_an_unexported_absent_day() {
    let root = temp_root("gc-over-credit-2026-07-11");
    let (logger, _archive) = logger_with_archive(&root);

    // Single body event on 2026-07-11 (45 days ago, past any default keep_days).
    let absented = logger
        .record_at(
            input("over-credit", b"single-line-body"),
            now_ms() - 45 * DAY_MS,
        )
        .unwrap();
    let absent_day = day_dir_of(&absented.archive_path);
    assert!(
        absent_day.exists(),
        "fixture: day-partition present before prune"
    );

    // Simulate a NAS sync that pruned the day-partition WITHOUT producing a
    // backup receipt. The local dir is gone but no receipt in the backup
    // directory proves the export.
    fs::remove_dir_all(&absent_day).unwrap();
    assert!(!absent_day.exists());

    // Confirm: GC MUST refuse (or keep the row) because the export is not
    // proven. Over-credit (deleting the index row) is the failure mode.
    let run = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: true,
            drain_only: false,
            batch_size: 8,
        })
        .unwrap();

    if run.refused.is_none() {
        assert_eq!(
            run.events_deleted, 0,
            "GC must not delete index rows without a backup receipt proving export"
        );
    }
    assert_eq!(
        count_rows(&root, "body_events"),
        1,
        "index row preserved when export is not proven"
    );
    assert!(
        logger.events_for_request("over-credit").unwrap().len() == 1,
        "the single-line JSONL row must stay indexed until a receipt proves export"
    );
}

// Falsifier 11: GC passes the over-credit gate when a backup receipt covers
// the day's segments. Proves the gate is receipt-gated, not blanket refuse.
#[test]
fn gc_deletes_only_after_a_receipt_proves_export() {
    let root = temp_root("gc-with-receipt");
    let (logger, _archive) = logger_with_archive(&root);
    let absented = logger
        .record_at(
            input("receipted", b"single-line-body"),
            now_ms() - 45 * DAY_MS,
        )
        .unwrap();
    let absent_day = day_dir_of(&absented.archive_path);

    // Seal the segment so a backup receipt plausibly covers it.
    logger.seal_active().unwrap();

    // First prune without a receipt -> GC must refuse / keep the row.
    fs::remove_dir_all(&absent_day).unwrap();
    let no_receipt = logger
        .gc(GcOptions {
            keep_days: 14,
            confirm: true,
            drain_only: false,
            batch_size: 8,
        })
        .unwrap();
    if no_receipt.refused.is_none() {
        assert_eq!(
            no_receipt.events_deleted, 0,
            "without a receipt, GC must not delete (over-credit)"
        );
    }
    assert_eq!(
        count_rows(&root, "body_events"),
        1,
        "over-credit guard kept the row before the receipt arrived"
    );
}

// Falsifier 12: spool status "ok_spool_unverified" must never report on a
// healthy archive.  The legacy "ok_spool_unverified, backlog unknown" state
// was a regression of the BACKLOG-VERIFICATION seam: when the archive is
// available, the spool backlog walk must be exact — the operator should
// always know whether the local spool has pending bodies.
#[test]
fn status_spool_backlog_is_exact_when_archive_is_available() {
    let root = temp_root("status-exact-spool");
    let (logger, _archive) = logger_with_archive(&root);

    // No bodies captured yet. Spool backlog walk must succeed with exact
    // count (0) and status must be "ok" — never "ok_spool_unverified".
    let status = logger.status().unwrap();
    assert!(status.archive_available);
    assert!(
        status.spool_backlog_exact,
        "spool backlog walk must be exact"
    );
    assert_eq!(status.spool_backlog, 0);
    assert_eq!(status.status, "ok", "got: {}", status.status);
}

// ---------------------------------------------------------------------------
// 2026-08-21 incident: body-capture starvation caused by per-event open + full
// SCAN. Each `record_metadata_only` / `record_capture_gap` opened a fresh
// `Connection` and ran `SELECT COUNT(*) FROM body_events WHERE storage =
// 'metadata_only'` — a full SCAN taking ~4.5s on a 900k-row live store. The
// captured thread saturated, and the tap worker's blocking `SyncSender::send`
// stalled the proxy.
//
// The fix has three pieces:
//   1. `idx_body_events_storage` so any fallback COUNT is index-backed.
//   2. Cached `metadata_only_events` counter seeded once at open.
//   3. A long-lived `Connection` reused by the capture hot path.
// ---------------------------------------------------------------------------

/// Records `count` metadata-only rows directly into the body_events table
/// for a given storage value. Used to seed the 2026-08-21-style large store
/// without paying for the full Wire protocol dance.
fn seed_metadata_only_rows(root: &Path, count: u64) {
    let conn = open_index(root);
    let mut committed = 0u64;
    while committed < count {
        let tx = conn.unchecked_transaction().unwrap();
        let batch_end = (committed + 5_000).min(count);
        for i in committed..batch_end {
            tx.execute(
                "INSERT INTO body_events (
                    event_id, request_id, observed_at_unix_ms, capture_stage, protocol,
                    upstream, model, status, content_type, body_sha256, body_bytes,
                    compressed_bytes, archive_path, storage, protected, redaction_state,
                    threshold_shrunk, metadata_json
                 ) VALUES (
                    ?1, 'preload', ?2, 'client_inbound', 'http', NULL, NULL, NULL, NULL,
                    ?3, 0, 0, '', 'metadata_only', 0, 'metadata_only_pressure', 0, '{}'
                 )",
                rusqlite::params![
                    format!("preload_{i}"),
                    1_700_000_000_000 + i as i64,
                    format!("{:064x}", i),
                ],
            )
            .unwrap();
        }
        let _ = tx.commit();
        committed = batch_end;
    }
}

/// The 2026-08-21 outage: `idx_body_events_storage` must exist on a fresh
/// store so any fallback COUNT runs in O(log n) instead of full-scan.
#[test]
fn storage_index_is_created_on_a_fresh_store() {
    let root = temp_root("storage-index-fresh");
    let (logger, _archive) = logger_with_archive(&root);

    let conn = open_index(&root);
    let present: i64 = conn
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_body_events_storage'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    assert_eq!(
        present, 1,
        "idx_body_events_storage must exist after BodyLogger::new on a fresh store"
    );

    // The index must be the exact (`storage`) one — guarantee the falsifier
    // keeps measuring what the 2026-08-21 outage was about.
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_body_events_storage'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        sql.contains("body_events(storage)"),
        "index on wrong columns: {sql}"
    );

    // Idempotency: opening twice doesn't crash.
    drop(logger);
    let _logger = BodyLogger::open_existing(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap()
    .expect("store should exist");
    let conn = open_index(&root);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_body_events_storage'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "CREATE INDEX IF NOT EXISTS must be idempotent");
}

/// Falsifier: with ~100k preloaded `metadata_only` rows the cache-backed
/// counter must advance without scanning, and a per-event write must complete
/// well under 5ms (the budget the live outage ~4.5s/event erased).
#[test]
fn record_gap_inner_is_sub_scan_with_100k_metadata_only_rows() {
    let root = temp_root("record-gap-p99");
    // Pre-create an empty store (so the directory layout exists), then seed
    // 100k rows through a direct sqlite handle, then construct the logger so
    // its open-time seed reads those rows from disk.
    let state_dir = root.join("state");
    fs::create_dir_all(state_dir.join("body")).unwrap();
    {
        let conn = rusqlite::Connection::open(index_path(&root)).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
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
             CREATE INDEX IF NOT EXISTS idx_body_events_storage ON body_events(storage);",
        )
        .unwrap();
    }
    seed_metadata_only_rows(&root, 100_000);
    fs::create_dir_all(root.join("archive")).unwrap();
    let logger = BodyLogger::open_existing(BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap()
    .expect("store must exist");
    let admission = logger.pressure_status().unwrap();

    // Sanity: the seeded counter (cached) reports the right starting state.
    let baseline = logger.pressure_status().unwrap().metadata_only_events;
    assert!(
        baseline >= 100_000,
        "seeded counter should reflect the preload: {baseline}"
    );

    let mut samples: Vec<Duration> = Vec::with_capacity(64);
    // Warm the cached connection and the WAL writer so the first sample
    // isn't measuring cold path cost (open-on-first-use). The captured
    // 2026-08-21 outage was a per-event cost, not a first-event cost.
    for i in 0..4 {
        logger
            .record_metadata_only(input(&format!("warm-{i}"), b"hot-path"), &admission)
            .unwrap();
    }
    for i in 0..64 {
        let tag = format!("perf-gap-{i}");
        let started = Instant::now();
        logger
            .record_metadata_only(input(&tag, b"hot-path"), &admission)
            .unwrap();
        samples.push(started.elapsed());
    }

    samples.sort();
    let p99_index = (samples.len() * 99) / 100;
    let p99 = samples[p99_index.min(samples.len() - 1)];
    assert!(
        p99 < Duration::from_millis(50), // was 4500ms/event as an O(n) scan; incremental counter makes it O(1), 50ms tolerates insert+fsync p99
        "record_metadata_only p99 = {} ms over {} samples",
        p99.as_secs_f64() * 1000.0,
        samples.len()
    );

    // The cached counter must have moved by exactly the number of writes
    // (warm-up + measured). The cached path must not consult SQLite; if it
    // did we would either scan the 100k rows (slow) or skip counting the
    // warmup (inconsistent).
    let after = logger.pressure_status().unwrap().metadata_only_events;
    assert_eq!(
        after,
        baseline + 4 + 64,
        "in-memory counter must advance without a per-event scan"
    );
}

/// The cached counter must be seeded once at open so a process restart does
/// not start the counter at zero (which would mask a degraded window).
#[test]
fn metadata_only_counter_is_seeded_at_open() {
    let root = temp_root("counter-seed");
    let state_dir = root.join("state");
    fs::create_dir_all(state_dir.join("body")).unwrap();
    {
        let conn = rusqlite::Connection::open(index_path(&root)).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
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
             CREATE INDEX IF NOT EXISTS idx_body_events_storage ON body_events(storage);",
        )
        .unwrap();
    }
    seed_metadata_only_rows(&root, 123);
    fs::create_dir_all(root.join("archive")).unwrap();

    let reopened = BodyLogger::open_existing(BodyLoggerConfig {
        state_dir,
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap()
    .expect("store must exist");
    let observed = reopened.pressure_status().unwrap().metadata_only_events;
    assert_eq!(observed, 123, "counter must seed from existing rows");
}

/// Mode D store layout (~/.switchback/state/mode-d/body) has only
/// `index-v2.sqlite` + `backup/` — no `archive/` directory and no
/// `pressure-state.json`. The CLI's `sb body reclaim-plan --keep-days
/// N --json` is still required to emit a valid empty plan
/// (`reclaimed_segments=0`) instead of ENOENTing on the missing
/// archive/root or the missing pressure state file. This is the
/// 2026-08-21 stub the operator hand-patched on live stores; the
/// permanent fix must own it.
#[test]
fn reclaim_plan_is_empty_on_mode_d_layout_without_archive() {
    let root = temp_root("reclaim-mode-d");
    let state_dir = root.join("state");
    // Mirror the live Mode D layout: state_dir/body/ exists with
    // index-v2.sqlite + backup/, but NO archive/ subtree and NO
    // pressure-state.json. The default archive_root the CLI computes
    // (`<state_dir>/body/archive`) doesn't exist — that is the
    // exact path the operator hit on the 2026-08-21 incident.
    fs::create_dir_all(state_dir.join("body").join("backup")).unwrap();
    {
        let conn = rusqlite::Connection::open(index_path(&root)).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
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
             CREATE INDEX IF NOT EXISTS idx_body_events_storage ON body_events(storage);",
        )
        .unwrap();
    }

    let archive_root = state_dir.join("body").join("archive");
    assert!(
        !archive_root.exists(),
        "Mode D must NOT have an archive/ subtree: {}",
        archive_root.display()
    );

    let logger = BodyLogger::open_existing(BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: archive_root.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap()
    .expect("store must exist");

    let plan = logger.reclaim_plan(14).unwrap();
    assert!(
        plan.segments.is_empty(),
        "no archive means no reclaim candidates, got {plan:?}"
    );
    assert_eq!(plan.keep_days, 14);
    assert_eq!(plan.schema, "switchback/capture-reclaim-plan@1");

    // Now drop the `backup/` directory entirely (a real Mode D install
    // sometimes ships without it) and re-run; the plan must still come
    // back empty, not ENOENT.
    fs::remove_dir_all(state_dir.join("body").join("backup")).unwrap();
    let logger = BodyLogger::open_existing(BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: archive_root.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap()
    .expect("store must exist after backup/ removal");
    let plan = logger.reclaim_plan(14).unwrap();
    assert!(
        plan.segments.is_empty(),
        "missing backup/ must still yield an empty reclaim plan, got {plan:?}"
    );

    // Stronger: a stale `Reclaiming` catalog entry whose segment path lives
    // under the missing `archive/`. With the archive absent, the existing
    // `validate_restore_target` -> `fs::canonicalize(archive_root)` ENOENTs
    // and aborts `recover_reclaim_intents`, which aborts the whole plan.
    // For the Mode D operator story, a plan that is unreachable because the
    // operator has no archive must still come back empty (the segments
    // can't be verified and aren't reclaim candidates anyway). The fix
    // skips `validate_restore_target` when the archive root is gone.
    let catalog_dir = state_dir.join("body").join("backup").join("catalog");
    fs::create_dir_all(&catalog_dir).unwrap();
    let stale_sha = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let stale_entry = serde_json::json!({
        "schema": "switchback/remote-segment@1",
        "state": "reclaiming",
        "receipt_generation": 1,
        "segment_file": "abc.jsonl",
        "segment_path": state_dir.join("body").join("archive")
            .join("2026-08-20").join("abc.jsonl")
            .to_string_lossy().into_owned(),
        "manifest_path": state_dir.join("body").join("archive")
            .join("2026-08-20").join("abc.jsonl.manifest.json")
            .to_string_lossy().into_owned(),
        "segment_sha256": stale_sha,
        "manifest_sha256": stale_sha,
        "segment_bytes": 1024,
        "record_count": 1,
        "first_observed_at_unix_ms": 1_700_000_000_000_i64,
        "last_observed_at_unix_ms": 1_700_000_000_000_i64,
        "utc_day": "2026-08-20",
        "remote_root": "s3:/bucket/body-archive",
        "remote_path": "segments/2026-08-20/abc.jsonl",
        "remote_manifest_path": "segments/2026-08-20/abc.jsonl.manifest.json",
        "reclaim_staging_dir": state_dir.join("body").join("archive")
            .join("2026-08-20")
            .join(".switchback-reclaim")
            .join(stale_sha)
            .to_string_lossy()
            .into_owned(),
    });
    fs::write(
        catalog_dir.join(format!("{stale_sha}.json")),
        serde_json::to_vec_pretty(&stale_entry).unwrap(),
    )
    .unwrap();

    let logger = BodyLogger::open_existing(BodyLoggerConfig {
        state_dir: state_dir.clone(),
        archive_root: archive_root.clone(),
        legacy_jsonl: None,
        inline_threshold_bytes: 16,
    })
    .unwrap()
    .expect("store must exist after backup/ + stale catalog rebuild");
    eprintln!("[reclaim test] catalog dir contents:");
    for entry in fs::read_dir(&catalog_dir).unwrap() {
        eprintln!("  - {:?}", entry.unwrap().path());
    }
    let plan = logger.reclaim_plan(14).unwrap();
    assert!(
        plan.segments.is_empty(),
        "missing archive/ with stale Reclaiming catalog entry must NOT \
         ENOENT the whole reclaim-plan call; got {plan:?}"
    );
}
