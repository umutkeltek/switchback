use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension as _;
use sb_bodylog::{
    BodyEventInput, BodyEventQuery, BodyLogger, BodyLoggerConfig, CaptureStage, GcOptions,
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
fn day_dir_of(archive_path: &str) -> PathBuf {
    PathBuf::from(archive_path)
        .ancestors()
        .nth(4)
        .unwrap()
        .to_path_buf()
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

// Falsifier 5: tap-bodies records day-route into their UTC day partition and
// the configured legacy sink is never appended (frozen; bytes untouched).
#[test]
fn tap_body_records_day_route_and_freeze_legacy() {
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

    let f1 = day_dir_of(&r1.archive_path).join("tap-bodies.jsonl");
    let f2 = day_dir_of(&r2.archive_path).join("tap-bodies.jsonl");
    assert_ne!(f1, f2, "different UTC days land in different files");
    let c1 = fs::read_to_string(&f1).unwrap();
    assert!(c1.contains(&r1.body_sha256));
    assert!(!c1.contains(&r2.body_sha256));
    let c2 = fs::read_to_string(&f2).unwrap();
    assert!(c2.contains(&r2.body_sha256));

    // Legacy sink is byte-for-byte untouched.
    assert_eq!(fs::read(&legacy).unwrap(), frozen);
}

// Falsifier 6: archive unavailable -> record day-routes into a spool day-file,
// legacy stays frozen (never created).
#[test]
fn archive_unavailable_routes_tap_body_into_spool_day_file() {
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
    assert_eq!(r.storage, "spool");

    let spool = root.join("state").join("body").join("spool");
    let day_file = fs::read_dir(&spool)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("tap-bodies-") && n.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .expect("spool day-file created");
    let content = fs::read_to_string(&day_file).unwrap();
    assert!(content.contains(&r.body_sha256));
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
