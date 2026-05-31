//! End-to-end observability: a request produces one queryable trace tying the
//! route decision + the account attempt + cost together, and the response
//! carries the request id so a client can correlate it with `/v1/traces/{id}`.

use std::sync::Arc;

const CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
"#;

async fn spawn() -> String {
    let cfg = sb_core::Config::from_yaml(CFG).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn request_produces_a_queryable_trace_with_request_id_header() {
    let base = spawn().await;
    let client = reqwest::Client::new();

    // A non-streaming request. The response must carry x-switchback-request-id.
    let resp = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    let req_id = resp
        .headers()
        .get("x-switchback-request-id")
        .expect("response must carry x-switchback-request-id")
        .to_str()
        .unwrap()
        .to_string();
    assert!(!req_id.is_empty());
    let _ = resp.json::<serde_json::Value>().await.unwrap();

    // The recent-traces ring exposes that request end-to-end.
    let traces: serde_json::Value = client
        .get(format!("{base}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(traces["count"], 1, "exactly one trace so far");
    let t = &traces["traces"][0];
    assert_eq!(
        t["request_id"],
        serde_json::json!(req_id),
        "trace keyed by request id"
    );
    assert_eq!(t["route"], "direct");
    assert_eq!(t["inbound_model"], "mock/echo");
    assert_eq!(t["final_status"], 200);
    assert_eq!(t["streamed"], false);
    // One successful attempt, on the default egress.
    assert_eq!(t["attempts"].as_array().unwrap().len(), 1, "one attempt");
    assert_eq!(t["attempts"][0]["outcome"], "success");
    assert_eq!(t["attempts"][0]["egress"], "direct");
    assert_eq!(t["attempts"][0]["provider_id"], "mock");
    // The explainable decision rode along.
    assert!(t["decision"]["selected"]["target_id"]
        .as_str()
        .unwrap()
        .contains("mock"));

    // Fetch the same trace by id.
    let one: serde_json::Value = client
        .get(format!("{base}/v1/traces/{req_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one["request_id"], serde_json::json!(req_id));

    // Unknown id → 404.
    let missing = client
        .get(format!("{base}/v1/traces/does-not-exist"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn streaming_request_is_traced_after_the_stream_completes() {
    let base = spawn().await;
    let client = reqwest::Client::new();

    // Drain a streamed response fully so the meter/trace completion fires.
    let text = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({"model":"mock/echo","stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("[DONE]"), "stream did not complete: {text}");

    let traces: serde_json::Value = client
        .get(format!("{base}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(traces["count"], 1);
    assert_eq!(
        traces["traces"][0]["streamed"], true,
        "streamed trace recorded"
    );
    assert_eq!(traces["traces"][0]["attempts"][0]["outcome"], "success");
}
