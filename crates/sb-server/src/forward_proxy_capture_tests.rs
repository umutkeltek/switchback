use super::*;
use crate::test_temp::TempRoot;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use futures::StreamExt;
use sb_bodylog::{BodyLogger, BodyLoggerConfig, BodyRecord, CaptureStage};
use sb_core::ForwardProxyConfig;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

const LARGE_BYTES: usize = 16_881_691;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A body logger whose state folder is removed when the value goes out of scope.
struct TestLogger {
    logger: BodyLogger,
    _root: TempRoot,
}

impl std::ops::Deref for TestLogger {
    type Target = BodyLogger;

    fn deref(&self) -> &BodyLogger {
        &self.logger
    }
}

fn logger(tag: &str) -> TestLogger {
    let root = TempRoot::new(format!(
        "sb-moded-{tag}-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let logger = BodyLogger::new(BodyLoggerConfig {
        state_dir: root.join("state"),
        archive_root: root.join("archive"),
        legacy_jsonl: None,
        inline_threshold_bytes: 1,
    })
    .unwrap();
    TestLogger {
        logger,
        _root: root,
    }
}

#[test]
fn moded_logger_folder_is_removed_when_the_test_ends() {
    let logger = logger("self-clean");
    let path = logger._root.to_path_buf();
    assert!(path.is_dir());
    drop(logger);
    assert!(
        !path.exists(),
        "capture test folder left behind: {}",
        path.display()
    );
}

fn input() -> BodyEventInput {
    BodyEventInput {
        request_id: "mode-d-reader-fixture".into(),
        capture_stage: CaptureStage::ClientInbound,
        protocol: "forward-proxy".into(),
        upstream: Some("upstream.invalid".into()),
        model: None,
        status: None,
        content_type: None,
        metadata: serde_json::json!({}),
        body: Vec::new(),
    }
}

async fn events(logger: &BodyLogger, count: usize) -> Vec<BodyRecord> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let records = logger.latest_events(20).unwrap();
            if records.len() >= count {
                return records;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("capture publication deadline")
}

async fn settled(logger: &BodyLogger) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while logger.capture_spill_status().unwrap().reserved_bytes != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("spill cleanup deadline");
}

struct HashWriter {
    hash: Sha256,
    bytes: usize,
}
impl std::io::Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.hash.update(bytes);
        self.bytes += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn assert_body(logger: &BodyLogger, record: &BodyRecord, byte: u8, count: usize) {
    let mut writer = HashWriter {
        hash: Sha256::new(),
        bytes: 0,
    };
    logger.read_record_body_to(record, &mut writer).unwrap();
    let expected = format!("{:x}", Sha256::digest(vec![byte; count]));
    assert_eq!(writer.bytes, count);
    assert_eq!(record.body_bytes, count as u64);
    assert_eq!(record.body_sha256, expected);
    assert_eq!(format!("{:x}", writer.hash.finalize()), expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_blocking_upload_keeps_ready_lease_until_reader_closes() {
    let logger = logger("cancelled-reader");
    let worker = CaptureWorker::new(logger.clone()).unwrap();
    let mut source: &[u8] = b"durable request fixture";
    let upload = capture_request_body(worker, input(), &mut source, 23, None)
        .await
        .unwrap();
    let reader = UploadReader {
        file: Some(upload.ready.as_ref().unwrap().open_body_reader().unwrap()),
        upload: upload.clone(),
        remaining: 23,
    };
    drop(upload);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let job = tokio::spawn(async move {
        bounded_file_io(move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            drop(reader);
        })
        .await
    });
    started_rx.await.unwrap();
    job.abort();
    let _ = job.await;
    assert!(
        logger.ready_capture_spills(4).unwrap().is_empty(),
        "live reader owns the lease"
    );
    assert!(
        logger.latest_events(4).unwrap().is_empty(),
        "publication must wait for the reader"
    );
    assert!(logger.capture_spill_status().unwrap().reserved_bytes > 0);
    release_tx.send(()).unwrap();
    let records = events(&logger, 1).await;
    settled(&logger).await;
    assert_eq!(
        logger.read_record_body(&records[0]).unwrap(),
        b"durable request fixture"
    );
    assert_eq!(logger.capture_spill_status().unwrap().reserved_bytes, 0);
}

#[tokio::test]
async fn incomplete_request_publishes_only_actual_prefix() {
    let logger = logger("incomplete-request");
    let worker = CaptureWorker::new(logger.clone()).unwrap();
    let mut source: &[u8] = b"prefix";
    let result = capture_request_body(worker, input(), &mut source, 100, None).await;
    assert!(matches!(result, Err(CaptureBodyFailure::Client(_))));
    let records = events(&logger, 1).await;
    assert_eq!(logger.read_record_body(&records[0]).unwrap(), b"prefix");
    assert_eq!(records[0].metadata["transport_complete"], false);
    assert_eq!(records[0].metadata["capture_prefix_only"], true);
    assert_eq!(records[0].metadata["expected_body_bytes"], 100);
}

#[tokio::test]
async fn exhausted_spool_refuses_before_consuming_client_body() {
    let logger = logger("admission");
    let worker = CaptureWorker::new(logger.clone()).unwrap();
    let mut reservations = Vec::new();
    while let Ok(spill) = logger.begin_capture_spill(input()) {
        reservations.push(spill);
    }
    assert!(!reservations.is_empty());
    let before = logger.capture_spill_status().unwrap().reserved_bytes;
    let mut source: &[u8] = b"must remain unread";
    let result = capture_request_body(worker, input(), &mut source, 18, None).await;
    assert!(matches!(result, Err(CaptureBodyFailure::Admission(_))));
    assert_eq!(source, b"must remain unread");
    assert_eq!(
        logger.capture_spill_status().unwrap().reserved_bytes,
        before
    );
    let records = events(&logger, 1).await;
    assert_eq!(records[0].storage, "metadata_only");
    assert_eq!(records[0].body_bytes, 0);
}

struct Fixture {
    logger: TestLogger,
    client: reqwest::Client,
    url: String,
    handles: Vec<tokio::task::AbortHandle>,
    // the proxy reads its CA files from here; removed after the handles are aborted
    _ca_root: TempRoot,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}
async fn fixture(tls: bool, policies: bool) -> Fixture {
    let upstream = Router::new()
        .route(
            "/v1/messages",
            post(|headers: HeaderMap, body: Body| async move {
                for name in [
                    "x-switchback-launch-profile",
                    "x-switchback-capture-policy",
                    "x-switchback-conformance-revision",
                    "x-switchback-account-attempt",
                ] {
                    assert!(
                        headers.get(name).is_none(),
                        "private correlation reached upstream: {name}"
                    );
                }
                assert_eq!(headers["authorization"], "Bearer synthetic-fixture");
                let mut stream = body.into_data_stream();
                let mut hash = Sha256::new();
                let mut count = 0;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.unwrap();
                    count += chunk.len();
                    hash.update(&chunk);
                }
                assert_eq!(count, LARGE_BYTES);
                assert_eq!(
                    format!("{:x}", hash.finalize()),
                    format!("{:x}", Sha256::digest(vec![b'q'; LARGE_BYTES]))
                );
                let broken = headers.contains_key("x-fixture-break-response");
                let chunks = futures::stream::unfold(
                    (LARGE_BYTES, false),
                    move |(remaining, failed)| async move {
                        if remaining == 0 || failed {
                            return None;
                        }
                        if broken && remaining < LARGE_BYTES {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            return Some((
                                Err(std::io::Error::other("synthetic upstream interruption")),
                                (0, true),
                            ));
                        }
                        let count = remaining.min(CHUNK_BYTES);
                        Some((
                            Ok::<_, std::io::Error>(Bytes::from(vec![b'r'; count])),
                            (remaining - count, false),
                        ))
                    },
                );
                Body::from_stream(chunks)
            }),
        )
        .layer(DefaultBodyLimit::disable());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let logger = logger("wire");
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let ca_root = TempRoot::new(format!(
        "sb-moded-ca-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let cfg: ForwardProxyConfig = serde_json::from_value(serde_json::json!({
        "id":"moded-spill-fixture", "bind":"127.0.0.1:0", "capture_bodies":true,
        "intercept_hosts":["upstream.invalid"], "tunnel_unknown_hosts":false,
        "capture_http_upstreams":[origin],
        "ca_cert_path":ca_root.join("ca.pem"), "ca_key_path":ca_root.join("ca.key"),
        "upstream_overrides":{"upstream.invalid":origin}
    }))
    .unwrap();
    let authority = crate::tap::CaptureProfileAuthority::from_entries(if policies {
        vec![
            (
                "metadata-fixture",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "metadata_only",
            ),
            (
                "off-fixture",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "off",
            ),
        ]
    } else {
        vec![]
    })
    .unwrap();
    let handle = crate::forward_proxy::spawn_forward_proxy_listener_with_authority_and_logger(
        cfg,
        proxy,
        None,
        authority,
        Some(logger.clone()),
    )
    .await
    .unwrap();
    let ca =
        reqwest::Certificate::from_pem(&std::fs::read(ca_root.join("ca.pem")).unwrap()).unwrap();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy_address}")).unwrap())
        .add_root_certificate(ca)
        .timeout(Duration::from_secs(60))
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .build()
        .unwrap();
    Fixture {
        logger,
        client,
        url: if tls {
            "https://upstream.invalid/v1/messages".into()
        } else {
            format!("{origin}/v1/messages")
        },
        handles: vec![server.abort_handle(), handle.abort_handle()],
        _ca_root: ca_root,
    }
}
async fn exchange(fixture: &Fixture, policy: Option<(&str, &str, &str)>) {
    let mut request = fixture
        .client
        .post(&fixture.url)
        .header("authorization", "Bearer synthetic-fixture")
        .header("content-type", "application/octet-stream")
        .header("x-switchback-account-attempt", "synthetic-attempt")
        .body(vec![b'q'; LARGE_BYTES]);
    if let Some((profile, revision, policy)) = policy {
        request = request
            .header("x-switchback-launch-profile", profile)
            .header("x-switchback-conformance-revision", revision)
            .header("x-switchback-capture-policy", policy);
    }
    let response = request.send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut chunks = response.bytes_stream();
    let mut hash = Sha256::new();
    let mut count = 0;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.unwrap();
        count += chunk.len();
        hash.update(&chunk);
    }
    assert_eq!(count, LARGE_BYTES);
    assert_eq!(
        format!("{:x}", hash.finalize()),
        format!("{:x}", Sha256::digest(vec![b'r'; LARGE_BYTES]))
    );
}
async fn large_wire(tls: bool) {
    let fixture = fixture(tls, false).await;
    exchange(&fixture, None).await;
    let records = events(&fixture.logger, 2).await;
    settled(&fixture.logger).await;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].request_id, records[1].request_id);
    for record in &records {
        assert_eq!(record.protocol, "forward-proxy");
        assert_eq!(record.metadata["transport_complete"], true);
        assert_ne!(record.storage, "metadata_only");
        assert_body(
            &fixture.logger,
            record,
            if record.capture_stage == "client_inbound" {
                b'q'
            } else {
                b'r'
            },
            LARGE_BYTES,
        );
    }
    assert_eq!(
        fixture
            .logger
            .capture_spill_status()
            .unwrap()
            .reserved_bytes,
        0
    );
}
#[tokio::test]
async fn tls_connect_preserves_exact_16881691_byte_request_and_reply() {
    large_wire(true).await;
}
#[tokio::test]
async fn enrolled_http_preserves_exact_16881691_byte_request_and_reply() {
    large_wire(false).await;
}
#[tokio::test]
async fn large_metadata_and_off_policies_never_spill_payload() {
    let fixture = fixture(true, true).await;
    exchange(
        &fixture,
        Some((
            "metadata-fixture",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "metadata_only",
        )),
    )
    .await;
    let records = events(&fixture.logger, 2).await;
    settled(&fixture.logger).await;
    assert_eq!(records.len(), 2);
    assert!(
        records
            .iter()
            .all(|record| record.storage == "metadata_only"
                && record.body_bytes == LARGE_BYTES as u64)
    );
    assert_eq!(fixture.logger.status().unwrap().blobs, 0);
    assert_eq!(
        fixture
            .logger
            .capture_spill_status()
            .unwrap()
            .reserved_bytes,
        0
    );
    exchange(
        &fixture,
        Some((
            "off-fixture",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "off",
        )),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fixture.logger.latest_events(20).unwrap().len(), 2);
    assert_eq!(fixture.logger.status().unwrap().blobs, 0);
    assert_eq!(
        fixture
            .logger
            .capture_spill_status()
            .unwrap()
            .reserved_bytes,
        0
    );
}

#[tokio::test]
async fn interrupted_upstream_keeps_actual_prefix_without_synthetic_502() {
    let fixture = fixture(true, false).await;
    let response = fixture
        .client
        .post(&fixture.url)
        .header("authorization", "Bearer synthetic-fixture")
        .header("x-fixture-break-response", "yes")
        .body(vec![b'q'; LARGE_BYTES])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert!(response.bytes().await.is_err());
    let records = events(&fixture.logger, 2).await;
    settled(&fixture.logger).await;
    assert_eq!(records.len(), 2);
    let response = records
        .iter()
        .find(|record| record.capture_stage == "upstream_response")
        .unwrap();
    assert_eq!(response.status, Some(200));
    assert_eq!(response.metadata["transport_complete"], false);
    assert_eq!(response.metadata["capture_prefix_only"], true);
    assert_body(&fixture.logger, response, b'r', CHUNK_BYTES);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_large_exchanges_keep_independent_capture_identity() {
    let fixture = fixture(true, false).await;
    tokio::join!(
        exchange(&fixture, None),
        exchange(&fixture, None),
        exchange(&fixture, None)
    );
    let records = events(&fixture.logger, 6).await;
    settled(&fixture.logger).await;
    assert_eq!(records.len(), 6);
    let mut grouped = std::collections::BTreeMap::new();
    for record in &records {
        grouped
            .entry(&record.request_id)
            .or_insert_with(Vec::new)
            .push(record);
        assert_eq!(record.body_bytes, LARGE_BYTES as u64);
        assert_ne!(record.storage, "metadata_only");
    }
    assert_eq!(grouped.len(), 3);
    for pair in grouped.values() {
        assert_eq!(pair.len(), 2);
        assert_ne!(pair[0].capture_stage, pair[1].capture_stage);
    }
    assert_eq!(
        fixture
            .logger
            .capture_spill_status()
            .unwrap()
            .reserved_bytes,
        0
    );
}
