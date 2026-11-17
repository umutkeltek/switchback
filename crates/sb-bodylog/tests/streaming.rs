use std::fs;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use sb_bodylog::{BodyEventInput, BodyLogger, BodyLoggerConfig, CaptureStage};
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct BoundedSink {
    bytes: u64,
    hash: Sha256,
    largest_chunk: usize,
}

impl Write for BoundedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > 64 * 1024 {
            return Err(io::Error::other("unbounded body-output write"));
        }
        self.hash.update(bytes);
        self.bytes += bytes.len() as u64;
        self.largest_chunk = self.largest_chunk.max(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn streams_the_observed_oversize_length_and_recovers_without_collecting_bodies() {
    let root = std::env::temp_dir().join(format!(
        "sb-streaming-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).unwrap();
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 256 * 1024,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    // Deterministic incompressible bytes, matching the actual missing length.
    let mut state = 0x291a_87f4_1028_5a76u64;
    let body: Vec<u8> = (0..19_361_818)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect();
    let expected_hash = format!("{:x}", Sha256::digest(&body));
    let record = logger
        .record(BodyEventInput {
            request_id: "oversize-stream".into(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "http".into(),
            upstream: Some("https://upstream.invalid".into()),
            model: None,
            status: Some(200),
            content_type: Some("application/octet-stream".into()),
            metadata: serde_json::json!({"transport_complete":true}),
            body,
        })
        .unwrap();
    logger.seal_active().unwrap();
    assert_eq!(logger.record_durability(&record).unwrap(), "sealed_local");
    drop(logger);
    let logger = BodyLogger::new(config).unwrap();
    let mut sink = BoundedSink {
        bytes: 0,
        hash: Sha256::new(),
        largest_chunk: 0,
    };
    logger.read_record_body_to(&record, &mut sink).unwrap();
    assert_eq!(sink.bytes, 19_361_818);
    assert_eq!(format!("{:x}", sink.hash.finalize()), expected_hash);
    assert!(sink.largest_chunk <= 64 * 1024);
    fs::remove_dir_all(root).unwrap();
}

fn spill_fixture(tag: &str) -> (std::path::PathBuf, BodyLoggerConfig, BodyLogger) {
    let root = std::env::temp_dir().join(format!(
        "sb-http-spill-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).unwrap();
    let config = BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 256 * 1024,
    };
    let logger = BodyLogger::new(config.clone()).unwrap();
    (root, config, logger)
}

fn seed(request: &str) -> BodyEventInput {
    BodyEventInput {
        request_id: request.into(),
        capture_stage: CaptureStage::ClientInbound,
        protocol: "http".into(),
        upstream: Some("https://upstream.invalid".into()),
        model: None,
        status: Some(200),
        content_type: Some("application/octet-stream".into()),
        metadata: serde_json::json!({"transport_complete":false}),
        body: Vec::new(),
    }
}

#[test]
fn a_ready_spill_preserves_the_observed_length_hash_and_identity_across_reopen() {
    let (root, config, logger) = spill_fixture("ready");
    let input = seed("ready-request");
    let mut spill = logger.begin_capture_spill(input.clone()).unwrap();
    let mut remaining = 19_361_818usize;
    let mut random = 0x8475_291a_65de_9871u64;
    let mut expected = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    while remaining != 0 {
        let count = remaining.min(buffer.len());
        for byte in &mut buffer[..count] {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            *byte = random as u8;
        }
        spill.write_all(&buffer[..count]).unwrap();
        expected.update(&buffer[..count]);
        remaining -= count;
    }
    let ready = spill.finish(input).unwrap();
    assert_eq!(ready.body_bytes(), 19_361_818);
    assert_eq!(ready.body_sha256(), format!("{:x}", expected.finalize()));
    let identity = ready.event_id().to_string();
    let status = logger.capture_spill_status().unwrap();
    assert_eq!(status.partial_captures, 0);
    assert_eq!(status.ready_captures, 1);
    assert!(
        logger.ready_capture_spills(2).unwrap().is_empty(),
        "a live lease cannot be replayed"
    );
    drop(ready);
    drop(logger);
    let logger = BodyLogger::new(config).unwrap();
    let restored = logger.ready_capture_spills(2).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].event_id(), identity);
    assert_eq!(restored[0].body_bytes(), 19_361_818);
    assert_eq!(
        logger.status().unwrap().events,
        0,
        "ready bytes are not yet indexed full capture"
    );
    let record = logger.record_capture_spill(&restored[0]).unwrap();
    assert_eq!(record.event_id, identity);
    assert_eq!(logger.status().unwrap().events, 1);
    let mut sink = BoundedSink {
        bytes: 0,
        hash: Sha256::new(),
        largest_chunk: 0,
    };
    logger.read_record_body_to(&record, &mut sink).unwrap();
    assert_eq!(sink.bytes, 19_361_818);
    assert_eq!(
        format!("{:x}", sink.hash.finalize()),
        restored[0].body_sha256()
    );
    assert_eq!(logger.capture_spill_status().unwrap().reserved_bytes, 0);
    assert_eq!(
        logger.record_capture_spill(&restored[0]).unwrap().event_id,
        identity
    );
    drop(restored);
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn partial_prefix_and_reservation_survive_a_dropped_capture() {
    let (root, config, logger) = spill_fixture("partial");
    let mut spill = logger
        .begin_capture_spill(seed("interrupted-request"))
        .unwrap();
    spill.write_all(b"truthful captured prefix").unwrap();
    drop(spill);
    drop(logger);
    let logger = BodyLogger::new(config).unwrap();
    let status = logger.capture_spill_status().unwrap();
    assert_eq!(status.partial_captures, 1);
    assert_eq!(status.ready_captures, 0);
    assert!(status.reserved_bytes > 0);
    let recovered = logger.ready_capture_spills(2).unwrap();
    assert_eq!(recovered.len(), 1);
    let record = logger.record_capture_spill(&recovered[0]).unwrap();
    assert_eq!(
        logger.read_record_body(&record).unwrap(),
        b"truthful captured prefix"
    );
    assert_eq!(record.metadata["transport_complete"], false);
    assert_eq!(record.metadata["capture_prefix_only"], true);
    assert_eq!(
        record.metadata["capture_recovery_reason"],
        "abandoned_capture_prefix"
    );
    assert_eq!(logger.capture_spill_status().unwrap().reserved_bytes, 0);
    drop(recovered);
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn spill_quota_is_persistent_and_shared_with_another_process() {
    let (root, _, logger) = spill_fixture("quota");
    let mut active = Vec::new();
    loop {
        match logger.begin_capture_spill(seed("quota-request")) {
            Ok(spill) => active.push(spill),
            Err(error) => {
                assert!(error.to_string().contains("quota exhausted"));
                break;
            }
        }
        assert!(active.len() <= 64);
    }
    let status = logger.capture_spill_status().unwrap();
    assert!(status.reserved_bytes <= status.quota_bytes);
    assert_eq!(status.partial_captures as usize, active.len());
    drop(active);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "spill_quota_child", "--nocapture"])
        .env("SB_SPILL_TEST_ROOT", &root)
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stdout)
    );
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn spill_quota_child() {
    let Some(root) = std::env::var_os("SB_SPILL_TEST_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 256 * 1024,
    })
    .unwrap();
    assert!(logger
        .begin_capture_spill(seed("child-request"))
        .unwrap_err()
        .to_string()
        .contains("quota exhausted"));
}

#[test]
fn oversized_metadata_and_changed_final_provenance_never_publish_ready_bytes() {
    let (root, _, logger) = spill_fixture("provenance");
    let mut oversized = seed("metadata");
    oversized.metadata = serde_json::json!({"large": "x".repeat(1024 * 1024)});
    assert!(logger.begin_capture_spill(oversized).is_err());
    assert_eq!(logger.capture_spill_status().unwrap().partial_captures, 0);
    let mut spill = logger
        .begin_capture_spill(seed("original-request"))
        .unwrap();
    spill.write_all(b"preserved bytes").unwrap();
    assert!(spill.finish(seed("different-request")).is_err());
    let status = logger.capture_spill_status().unwrap();
    assert_eq!(status.partial_captures, 1);
    assert_eq!(status.ready_captures, 0);
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn interrupted_ready_rename_preserves_completion_and_fixed_metadata_temp_bound() {
    let (root, _, logger) = spill_fixture("ready-rename");
    let mut input = seed("ready-rename");
    input.metadata = serde_json::json!({"transport_complete":true});
    let mut spill = logger.begin_capture_spill(input.clone()).unwrap();
    spill.write_all(b"complete body").unwrap();
    let ready = spill.finish(input).unwrap();
    let id = ready.event_id().to_string();
    let directory = root.join("state/body/spool/capture-queue").join(&id);
    fs::rename(
        directory.join("ready.json"),
        directory.join(".ready.json.tmp"),
    )
    .unwrap();
    drop(ready);
    let replay = logger.ready_capture_spills(2).unwrap();
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].event_id(), id);
    let record = logger.record_capture_spill(&replay[0]).unwrap();
    assert_eq!(record.metadata["transport_complete"], true);
    assert!(record.metadata.get("capture_prefix_only").is_none());
    assert!(!directory.exists());
    drop(replay);
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn corrupt_ready_does_not_starve_valid_or_claim_completion() {
    let (root, _, logger) = spill_fixture("corrupt-ready");
    let mut bad = logger.begin_capture_spill(seed("corrupt")).unwrap();
    bad.write_all(b"held evidence").unwrap();
    let ready = bad.finish(seed("corrupt")).unwrap();
    let directory = root
        .join("state/body/spool/capture-queue")
        .join(ready.event_id());
    drop(ready);
    fs::write(directory.join("ready.json"), b"{broken").unwrap();
    let mut good = logger.begin_capture_spill(seed("valid")).unwrap();
    good.write_all(b"valid evidence").unwrap();
    let ready = good.finish(seed("valid")).unwrap();
    drop(ready);
    assert_eq!(logger.recover_capture_spills().unwrap(), 1);
    assert_eq!(logger.events_for_request("valid").unwrap().len(), 1);
    assert!(logger.events_for_request("corrupt").unwrap().is_empty());
    let status = logger.status().unwrap();
    assert_eq!(status.capture_spill.ready_captures, 1);
    assert_eq!(status.capture_spill.invalid_captures, 1);
    assert_eq!(status.status, "capture_spill_pending");
    assert!(directory.join("body.raw").exists());
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn ready_write_and_append_errors_preserve_bytes_and_refuse_symlinks() {
    use std::os::unix::fs::symlink;
    let (root, _, logger) = spill_fixture("io-errors");
    let mut spill = logger.begin_capture_spill(seed("io-errors")).unwrap();
    spill.write_all(b"retained after error").unwrap();
    let queue = root.join("state/body/spool/capture-queue");
    let directory = fs::read_dir(&queue)
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_type().unwrap().is_dir())
        .unwrap()
        .path();
    fs::create_dir(directory.join("ready.json")).unwrap();
    assert!(
        spill.finish(seed("io-errors")).is_err(),
        "rename onto a directory must fail"
    );
    assert_eq!(
        fs::read(directory.join("body.raw")).unwrap(),
        b"retained after error"
    );
    fs::remove_dir(directory.join("ready.json")).unwrap();
    let capture = logger.ready_capture_spills(2).unwrap().remove(0);
    let outside = root.join("outside");
    fs::write(&outside, b"unchanged").unwrap();
    symlink(&outside, directory.join("frame.tmp")).unwrap();
    assert!(logger.record_capture_spill(&capture).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
    assert_eq!(logger.status().unwrap().events, 0);
    fs::remove_file(directory.join("frame.tmp")).unwrap();
    // Same-sized source mutation must be detected by the streamed encoder.
    fs::write(directory.join("body.raw"), b"mutated after error!").unwrap();
    assert!(logger.record_capture_spill(&capture).is_err());
    assert_eq!(logger.status().unwrap().events, 0);
    drop(capture);
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn hostile_expanded_size_and_crc_fail_before_emitting_unverified_bytes() {
    let (root, _, logger) = spill_fixture("hostile-frame");
    let mut input = seed("hostile-frame");
    input.body = vec![b'x'; 512 * 1024];
    let record = logger.record(input).unwrap();
    logger.seal_active().unwrap();
    let original = fs::read(&record.archive_path).unwrap();
    for size in [4u64, u64::MAX] {
        let mut bytes = original.clone();
        bytes[20..28].copy_from_slice(&size.to_be_bytes());
        fs::write(&record.archive_path, bytes).unwrap();
        let mut sink = BoundedSink {
            bytes: 0,
            hash: Sha256::new(),
            largest_chunk: 0,
        };
        assert!(logger.read_record_body_to(&record, &mut sink).is_err());
        assert_eq!(sink.bytes, 0);
    }
    let mut corrupt = original.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&record.archive_path, corrupt).unwrap();
    let mut sink = BoundedSink {
        bytes: 0,
        hash: Sha256::new(),
        largest_chunk: 0,
    };
    assert!(logger.read_record_body_to(&record, &mut sink).is_err());
    assert_eq!(sink.bytes, 0);
    fs::write(&record.archive_path, original).unwrap();
    logger
        .read_record_body_to(&record, &mut std::io::sink())
        .unwrap();
    drop(logger);
    fs::remove_dir_all(root).unwrap();
}
