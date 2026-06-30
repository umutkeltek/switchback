//! First multimodal workload slice: jobs/artifacts/workflows exist as a
//! metadata-safe API surface before real provider adapters land.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
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
    spawn_with_config(CFG).await
}

async fn spawn_with_config(config: &str) -> String {
    let cfg = sb_core::Config::from_yaml(config).unwrap();
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

#[derive(Clone, Default)]
struct FakeComfyState {
    prompt_body: Arc<Mutex<Option<serde_json::Value>>>,
}

async fn fake_comfy_prompt(
    State(state): State<FakeComfyState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    *state.prompt_body.lock().unwrap() = Some(body);
    Json(json!({
        "prompt_id": "prompt-123",
        "number": 1,
        "node_errors": {}
    }))
}

async fn fake_comfy_history(Path(prompt_id): Path<String>) -> Json<serde_json::Value> {
    Json(json!({
        prompt_id: {
            "status": {"completed": true},
            "outputs": {
                "9": {
                    "images": [{
                        "filename": "switchback_00001_.png",
                        "subfolder": "",
                        "type": "output"
                    }]
                }
            }
        }
    }))
}

async fn fake_comfy_view(Query(_query): Query<HashMap<String, String>>) -> impl IntoResponse {
    ([("content-type", "image/png")], mock_png())
}

async fn fake_comfy_system_stats() -> Json<serde_json::Value> {
    Json(json!({"system": {"os": "test"}, "devices": []}))
}

async fn spawn_fake_comfy() -> (String, FakeComfyState) {
    let state = FakeComfyState::default();
    let app = Router::new()
        .route("/prompt", post(fake_comfy_prompt))
        .route("/history/{prompt_id}", get(fake_comfy_history))
        .route("/view", get(fake_comfy_view))
        .route("/system_stats", get(fake_comfy_system_stats))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
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

#[tokio::test]
async fn workflow_registry_exposes_configured_comfyui_workflow() {
    let (comfy_url, _comfy) = spawn_fake_comfy().await;
    let config = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  api_keys:
    - key: "sk-operator"
      tenant: test
      role: operator
tenants:
  - id: test
providers:
  - id: comfy
    type: comfyui
    base_url: "{comfy_url}"
    workflows:
      - id: txt2img
        kind: image_generation
        version: test
        graph: {{"6": {{"class_type": "CLIPTextEncode", "inputs": {{"text": ""}}}}, "9": {{"class_type": "SaveImage", "inputs": {{"filename_prefix": "switchback"}}}}}}
        bindings:
          prompt: {{ path: ["6", "inputs", "text"] }}
        output_node_ids: ["9"]
"#
    );
    let base = spawn_with_config(&config).await;
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

    let data = workflows["data"].as_array().unwrap();
    assert!(data.iter().any(|workflow| {
        workflow["id"] == "comfy/txt2img"
            && workflow["provider"] == "comfy"
            && workflow["kind"] == "image_generation"
    }));
}

#[tokio::test]
async fn comfyui_image_generation_submits_bound_graph_and_captures_output_artifact() {
    let (comfy_url, comfy) = spawn_fake_comfy().await;
    let config = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  api_keys:
    - key: "sk-operator"
      tenant: test
      role: operator
tenants:
  - id: test
providers:
  - id: comfy
    type: comfyui
    base_url: "{comfy_url}"
    workflows:
      - id: txt2img
        kind: image_generation
        version: test
        graph:
          "3":
            class_type: EmptyLatentImage
            inputs: {{ width: 1, height: 1, batch_size: 1 }}
          "4":
            class_type: KSampler
            inputs: {{ seed: 0 }}
          "6":
            class_type: CLIPTextEncode
            inputs: {{ text: "" }}
          "9":
            class_type: SaveImage
            inputs: {{ filename_prefix: "switchback" }}
        bindings:
          prompt: {{ path: ["6", "inputs", "text"] }}
          seed: {{ path: ["4", "inputs", "seed"] }}
          width: {{ path: ["3", "inputs", "width"] }}
          height: {{ path: ["3", "inputs", "height"] }}
        output_node_ids: ["9"]
"#
    );
    let base = spawn_with_config(&config).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = authed(
        &client,
        reqwest::Method::POST,
        format!("{base}/v1/images/generations"),
    )
    .json(&json!({
        "model": "comfy/txt2img",
        "prompt": "a glass switchback router",
        "size": "768x512",
        "seed": 123,
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
    assert_eq!(created["model"], "comfy/txt2img");
    assert_eq!(created["job"]["status"], "succeeded");
    assert_eq!(created["data"].as_array().unwrap().len(), 1);

    let submitted = comfy.prompt_body.lock().unwrap().clone().unwrap();
    assert_eq!(
        submitted["prompt"]["6"]["inputs"]["text"],
        "a glass switchback router"
    );
    assert_eq!(submitted["prompt"]["4"]["inputs"]["seed"], 123);
    assert_eq!(submitted["prompt"]["3"]["inputs"]["width"], 768);
    assert_eq!(submitted["prompt"]["3"]["inputs"]["height"], 512);
    assert!(submitted["client_id"]
        .as_str()
        .unwrap()
        .starts_with("switchback-"));

    let job_id = created["job"]["id"].as_str().unwrap();
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
    assert_eq!(
        job["route_decision"]["selected"]["target_id"],
        "comfy/txt2img"
    );
    assert!(job["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["event"] == "history_polled"));

    let artifact_id = created["data"][0]["artifact_id"].as_str().unwrap();
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
    assert_eq!(artifact.bytes().await.unwrap(), mock_png());
}

fn mock_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}
