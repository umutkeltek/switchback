use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{routing::post, Router};
use sb_bodylog::{BodyLogger, BodyLoggerConfig};

struct Recorder {
    child: Child,
    root: PathBuf,
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn existing_index_serves_multiple_body_proxies_without_archive_recovery() {
    let root = std::env::temp_dir().join(format!(
        "switchback-proxy-existing-index-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let state = root.join("state");
    let archive = root.join("archive");
    let logger_config = BodyLoggerConfig {
        state_dir: state.clone(),
        archive_root: archive.clone(),
        legacy_jsonl: Some(state.join("tap-bodies.jsonl")),
        inline_threshold_bytes: 1,
    };
    let logger = BodyLogger::new(logger_config.clone()).unwrap();
    // Recovery is an explicit maintenance operation. An unrelated corrupt
    // archive segment must not disable ordinary writes to an existing index.
    std::fs::write(archive.join("corrupt.sbcap"), b"invalid archived segment").unwrap();
    let upstream = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let origin = format!("http://{}", upstream.local_addr().unwrap());
    let response_body = r#"{"output":"captured-fixture-response"}"#;
    let upstream_task = tokio::spawn(async move {
        axum::serve(
            upstream,
            Router::new().route("/v1/responses", post(move || async move { response_body })),
        )
        .await
        .unwrap();
    });
    let ports = [free_port(), free_port()];
    let gateway_port = free_port();
    let proxies: Vec<_> = ports
        .iter()
        .zip(["hermes", "prime"])
        .map(|(port, harness)| {
            serde_json::json!({
                "id": harness,
                "bind": format!("127.0.0.1:{port}"),
                "intercept_hosts": ["fixture.example"],
                "ca_cert_path": root.join("ca.pem"),
                "ca_key_path": root.join("ca.key"),
                "capture_bodies": true,
                "capture_harness": harness,
                "capture_http_upstreams": [origin],
                "tunnel_unknown_hosts": true
            })
        })
        .collect();
    let config = root.join("capture.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&serde_json::json!({
            "server": {
                "bind": format!("127.0.0.1:{gateway_port}"),
                "trace_log": state.join("traces.jsonl"),
                "forward_proxies": proxies
            },
            "providers": [],
            "routes": []
        }))
        .unwrap(),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_switchback"))
        .args(["serve", "--config"])
        .arg(&config)
        .env("SB_RUNTIME_ROOT", &root)
        .env("SWITCHBACK_RUNTIME_ROOT", &root)
        .env("SWITCHBACK_BODY_ARCHIVE_ROOT", &archive)
        .env("SWITCHBACK_BODY_FORCE_SPOOL", "1")
        .env(
            "SB_PROFILE_PROJECTION_ROOT",
            root.join("profile-conformance"),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut recorder = Recorder { child, root };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    for port in ports {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            assert!(
                recorder.child.try_wait().unwrap().is_none(),
                "recorder exited during startup"
            );
            assert!(std::time::Instant::now() < deadline, "proxy did not start");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let response = client
            .post(format!("{origin}/v1/responses"))
            .header("content-type", "application/json")
            .body(r#"{"model":"fixture/model","input":"captured-fixture-request"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), response_body);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while logger.latest_events(32).unwrap().len() < 4 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let events = logger.latest_events(32).unwrap();
    assert_eq!(
        events.len(),
        4,
        "both proxies must capture request and response bodies"
    );
    for harness in ["hermes", "prime"] {
        assert_eq!(
            events
                .iter()
                .filter(|event| event.metadata["native_execution"]["harness"] == harness)
                .count(),
            2,
            "dedicated proxy provenance must survive"
        );
    }
    for event in events {
        assert!(!logger.read_blob(&event.body_sha256).unwrap().is_empty());
    }
    upstream_task.abort();
    assert_eq!(
        std::fs::read(recorder.root.join("archive/corrupt.sbcap")).unwrap(),
        b"invalid archived segment"
    );
}
