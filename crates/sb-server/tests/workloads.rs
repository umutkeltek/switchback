//! First multimodal workload slice: jobs/artifacts/workflows exist as a
//! metadata-safe API surface before real provider adapters land.

use std::sync::Arc;

use serde_json::json;

const CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
api_keys:
  - key: "sk-operator"
    tenant: test
    role: operator
tenants:
  - id: test
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets: ["mock/echo"]
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

fn authed(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
) -> reqwest::RequestBuilder {
    client
        .request(method, url)
        .header("authorization", "Bearer sk-operator")
}

#[tokio::test]
async fn workload_endpoints_are_auth_gated() {
    let base = spawn().await;
    let client = reqwest::Client::new();

    for path in [
        "/v1/jobs",
        "/v1/jobs/job_missing",
        "/v1/jobs/job_missing/events",
        "/v1/artifacts/art_missing",
        "/v1/artifacts/art_missing/thumb",
        "/v1/workflows",
    ] {
        let status = client
            .get(format!("{base}{path}"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED, "{path}");
    }

    let status = client
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model":"mock/image","prompt":"test"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn image_generation_creates_job_and_artifact_metadata() {
    let base = spawn().await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = authed(
        &client,
        reqwest::Method::POST,
        format!("{base}/v1/images/generations"),
    )
    .json(&json!({
        "model": "mock/image",
        "prompt": "draw a switchback route board",
        "size": "512x512",
        "n": 1,
        "response_format": "url"
    }))
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();

    assert_eq!(created["object"], "image.generation");
    assert_eq!(created["model"], "mock/image");
    assert_eq!(created["job"]["kind"], "image_generation");
    assert_eq!(created["job"]["status"], "succeeded");
    assert_eq!(created["data"].as_array().unwrap().len(), 1);
    assert!(created["data"][0]["artifact_id"]
        .as_str()
        .unwrap()
        .starts_with("art_"));
    assert!(created["data"][0]["url"]
        .as_str()
        .unwrap()
        .contains("/v1/artifacts/art_"));

    let job_id = created["job"]["id"].as_str().unwrap();
    assert!(job_id.starts_with("job_"));

    let job: serde_json::Value = authed(
        &client,
        reqwest::Method::GET,
        format!("{base}/v1/jobs/{job_id}"),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();

    assert_eq!(job["id"], job_id);
    assert_eq!(job["kind"], "image_generation");
    assert_eq!(job["status"], "succeeded");
    assert_eq!(job["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(job["route_decision"]["selected"]["target_id"], "mock/image");
    assert_eq!(job["prompt_stored"], false);

    let artifact_id = job["artifacts"][0]["artifact_id"].as_str().unwrap();
    let artifact = authed(
        &client,
        reqwest::Method::GET,
        format!("{base}/v1/artifacts/{artifact_id}"),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert_eq!(artifact.headers()["content-type"], "image/png");
    assert!(artifact.bytes().await.unwrap().len() > 16);

    let events = authed(
        &client,
        reqwest::Method::GET,
        format!("{base}/v1/jobs/{job_id}/events"),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .text()
    .await
    .unwrap();
    assert!(events.contains("event: accepted"));
    assert!(events.contains("event: artifact_ready"));
    assert!(events.contains("event: succeeded"));
}

#[tokio::test]
async fn workflow_registry_exposes_named_mock_image_workflow() {
    let base = spawn().await;
    let client = reqwest::Client::new();

    let workflows: serde_json::Value = authed(
        &client,
        reqwest::Method::GET,
        format!("{base}/v1/workflows"),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();

    assert_eq!(workflows["object"], "list");
    assert_eq!(workflows["data"][0]["id"], "mock-image");
    assert_eq!(workflows["data"][0]["kind"], "image_generation");
    assert_eq!(workflows["data"][0]["provider"], "switchback-mock");
}
