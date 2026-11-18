//! First multimodal workload slice: jobs/artifacts/workflows exist as a
//! metadata-safe API surface before real provider adapters land.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
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
    /// History calls answered so far.
    history_calls: Arc<Mutex<usize>>,
    /// Answer `{}` (the job is still queued or running) to this many history
    /// calls before the job's entry appears.
    pending_history_calls: usize,
    /// The job's entry reports a failure in ComfyUI instead of outputs.
    fail: bool,
}

#[derive(Clone)]
struct FakeFalState {
    artifact_url: String,
    auth_headers: Arc<Mutex<Vec<String>>>,
    status_calls: Arc<Mutex<usize>>,
    fail: bool,
    slow: bool,
    cancel_calls: Arc<Mutex<usize>>,
}

fn capture_fal_auth(state: &FakeFalState, headers: &HeaderMap) {
    let value = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    state.auth_headers.lock().unwrap().push(value);
}

async fn fake_fal_submit(
    State(state): State<FakeFalState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    capture_fal_auth(&state, &headers);
    Json(json!({
        "request_id": "fal-request-123",
        "status": "IN_QUEUE"
    }))
}

async fn fake_fal_status(
    State(state): State<FakeFalState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    capture_fal_auth(&state, &headers);
    let mut calls = state.status_calls.lock().unwrap();
    *calls += 1;
    let status = match (state.slow, state.fail, *calls) {
        (true, _, _) => "IN_PROGRESS",
        (false, true, 1) => "IN_PROGRESS",
        (false, true, _) => "FAILED",
        (false, false, 1) => "IN_QUEUE",
        (false, false, 2) => "IN_PROGRESS",
        (false, false, _) => "COMPLETED",
    };
    Json(json!({"status": status, "queue_position": 1}))
}

async fn fake_fal_result(
    State(state): State<FakeFalState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    capture_fal_auth(&state, &headers);
    Json(json!({
        "images": [{
            "url": state.artifact_url,
            "content_type": "image/png",
            "width": 1000,
            "height": 1000
        }],
        "seed": 123
    }))
}

async fn fake_fal_artifact() -> impl IntoResponse {
    ([("content-type", "image/png")], mock_png())
}

async fn fake_fal_cancel(
    State(state): State<FakeFalState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    capture_fal_auth(&state, &headers);
    *state.cancel_calls.lock().unwrap() += 1;
    Json(json!({"status": "CANCELLED"}))
}

async fn spawn_fake_fal() -> (String, FakeFalState) {
    spawn_fake_fal_with_mode(false, false).await
}

async fn spawn_fake_fal_with_failure(fail: bool) -> (String, FakeFalState) {
    spawn_fake_fal_with_mode(fail, false).await
}

async fn spawn_slow_fake_fal() -> (String, FakeFalState) {
    spawn_fake_fal_with_mode(false, true).await
}

async fn spawn_fake_fal_with_mode(fail: bool, slow: bool) -> (String, FakeFalState) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let state = FakeFalState {
        artifact_url: format!("{base_url}/artifact.png?signature=never-store-this"),
        auth_headers: Arc::new(Mutex::new(Vec::new())),
        status_calls: Arc::new(Mutex::new(0)),
        fail,
        slow,
        cancel_calls: Arc::new(Mutex::new(0)),
    };
    let app = Router::new()
        .route("/fal-ai/qwen-image", post(fake_fal_submit))
        .route(
            "/fal-ai/qwen-image/requests/{request_id}/status",
            get(fake_fal_status),
        )
        .route(
            "/fal-ai/qwen-image/requests/{request_id}",
            get(fake_fal_result),
        )
        .route("/artifact.png", get(fake_fal_artifact))
        .route(
            "/fal-ai/qwen-image/requests/{request_id}/cancel",
            put(fake_fal_cancel),
        )
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (base_url, state)
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

async fn fake_comfy_history(
    State(state): State<FakeComfyState>,
    Path(prompt_id): Path<String>,
) -> Json<serde_json::Value> {
    let calls = {
        let mut calls = state.history_calls.lock().unwrap();
        *calls += 1;
        *calls
    };
    if calls <= state.pending_history_calls {
        return Json(json!({}));
    }
    if state.fail {
        return Json(json!({
            prompt_id.clone(): {
                "status": {
                    "status_str": "error",
                    "completed": false,
                    "messages": [
                        ["execution_start", {"prompt_id": prompt_id}],
                        ["execution_error", {
                            "node_type": "KSampler",
                            "exception_type": "torch.OutOfMemoryError",
                            "exception_message": "quoted job input that must not leak"
                        }]
                    ]
                },
                "outputs": {}
            }
        }));
    }
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
    spawn_fake_comfy_with(FakeComfyState::default()).await
}

async fn spawn_fake_comfy_with(state: FakeComfyState) -> (String, FakeComfyState) {
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

    let usage: serde_json::Value =
        authed(&client, reqwest::Method::GET, format!("{base}/v1/usage"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(usage["requests"], 1);
    assert_eq!(usage["unknown_cost_requests"], 1);
    assert_eq!(usage["recent"][0]["provider_id"], "switchback-mock");
    assert!(usage["recent"][0]["cost_micros"].is_null());
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

    let usage: serde_json::Value =
        authed(&client, reqwest::Method::GET, format!("{base}/v1/usage"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(usage["requests"], 1);
    assert_eq!(usage["unknown_cost_requests"], 1);
    assert_eq!(usage["recent"][0]["provider_id"], "comfy");
    assert!(usage["recent"][0]["cost_micros"].is_null());
}

/// A gateway with one ComfyUI workflow, `comfy/txt2img`, at `comfy_url`.
fn one_comfy_workflow(comfy_url: &str) -> String {
    format!(
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
        graph:
          "6":
            class_type: CLIPTextEncode
            inputs: {{ text: "" }}
          "9":
            class_type: SaveImage
            inputs: {{ filename_prefix: "switchback" }}
        bindings:
          prompt: {{ path: ["6", "inputs", "text"] }}
        output_node_ids: ["9"]
"#
    )
}

async fn generate_one_image(base: &str) -> reqwest::Response {
    authed(
        &reqwest::Client::new(),
        reqwest::Method::POST,
        format!("{base}/v1/images/generations"),
    )
    .json(&json!({"model": "comfy/txt2img", "prompt": "a lighthouse"}))
    .send()
    .await
    .unwrap()
}

#[tokio::test]
async fn comfyui_job_running_longer_than_thirty_seconds_still_delivers_its_image() {
    // ComfyUI answers `{}` while a job is queued or running. 65 such answers
    // at the 500 ms poll are about 33 s, as long as a cold model load after a
    // wake; the poll used to give up after 60 answers and fail the request
    // while ComfyUI kept rendering.
    let (comfy_url, comfy) = spawn_fake_comfy_with(FakeComfyState {
        pending_history_calls: 65,
        ..FakeComfyState::default()
    })
    .await;
    let base = spawn_with_config(&one_comfy_workflow(&comfy_url)).await;

    let response = generate_one_image(&base).await;
    assert_eq!(response.status(), 200);
    let created: serde_json::Value = response.json().await.unwrap();
    assert_eq!(created["job"]["status"], "succeeded");
    assert_eq!(created["data"].as_array().unwrap().len(), 1);
    assert_eq!(*comfy.history_calls.lock().unwrap(), 66);
}

#[tokio::test]
async fn comfyui_job_that_failed_in_comfyui_ends_the_request_at_once() {
    let (comfy_url, comfy) = spawn_fake_comfy_with(FakeComfyState {
        fail: true,
        ..FakeComfyState::default()
    })
    .await;
    let base = spawn_with_config(&one_comfy_workflow(&comfy_url)).await;

    let started = std::time::Instant::now();
    let response = generate_one_image(&base).await;
    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("failed in ComfyUI (KSampler: torch.OutOfMemoryError)"),
        "{message}"
    );
    assert!(!message.contains("quoted job input"), "{message}");
    assert_eq!(*comfy.history_calls.lock().unwrap(), 1);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "waited {:?} for a job ComfyUI had already failed",
        started.elapsed()
    );
}

#[tokio::test]
async fn fal_image_generation_runs_queue_lifecycle_and_captures_safe_provenance() {
    let (fal_url, fal) = spawn_fake_fal().await;
    let cost_map = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../config/provider-registry.json")
        .canonicalize()
        .unwrap();
    let config = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  block_private_networks: false
  cost_map: "{}"
api_keys:
  - key: "sk-operator"
    tenant: test
    project: fal-project
    role: operator
tenants:
  - id: test
providers:
  - id: fal
    type: fal
    base_url: "{fal_url}"
    platform_base_url: "{fal_url}"
    accounts:
      - id: test
        auth: {{ kind: api_key, inline: "fal-test-secret" }}
"#,
        cost_map.display()
    );
    let base = spawn_with_config(&config).await;
    let client = reqwest::Client::new();

    let created: serde_json::Value = authed(
        &client,
        reqwest::Method::POST,
        format!("{base}/v1/images/generations"),
    )
    .json(&json!({
        "model": "fal/fal-ai/qwen-image",
        "prompt": "a private prompt that must not enter metadata",
        "size": "1x1",
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

    assert_eq!(created["job"]["status"], "succeeded");
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

    let events: Vec<_> = job["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            (
                event["event"].as_str().unwrap(),
                event["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        events,
        vec![
            ("accepted", "accepted"),
            ("queued", "queued"),
            ("running", "running"),
            ("artifact_ready", "artifact_ready"),
            ("succeeded", "succeeded"),
        ]
    );
    assert!(job["route_decision"]["reason"]
        .as_array()
        .unwrap()
        .iter()
        .any(|reason| reason == "adapter=fal"));

    let artifact = &job["artifacts"][0];
    assert_eq!(artifact["provenance"]["provider"], "fal");
    assert_eq!(artifact["provenance"]["model"], "fal-ai/qwen-image");
    assert_eq!(
        artifact["provenance"]["route_decision_id"],
        job["route_decision"]["request_id"]
    );

    let serialized_job = serde_json::to_string(&job).unwrap();
    assert!(!serialized_job.contains("private prompt"));
    assert!(!serialized_job.contains("fal-test-secret"));
    assert!(!serialized_job.contains("never-store-this"));
    assert!(fal
        .auth_headers
        .lock()
        .unwrap()
        .iter()
        .all(|header| header == "Key fal-test-secret"));

    let usage: serde_json::Value =
        authed(&client, reqwest::Method::GET, format!("{base}/v1/usage"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(usage["requests"], 1);
    let row = &usage["recent"][0];
    assert_eq!(row["provider_id"], "fal");
    assert_eq!(row["model"], "fal-ai/qwen-image");
    assert_eq!(row["pricing_unit"], "per_megapixel");
    assert_eq!(row["units_consumed"], 1.0);
    assert_eq!(row["cost_micros"], 20_000);
    assert_eq!(row["tenant"], "test");
    assert_eq!(row["project"], "fal-project");
}

#[tokio::test]
async fn fal_failure_persists_a_metadata_safe_failed_job() {
    let (fal_url, _fal) = spawn_fake_fal_with_failure(true).await;
    let config = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  block_private_networks: false
api_keys:
  - key: "sk-operator"
    tenant: test
    role: operator
tenants:
  - id: test
providers:
  - id: fal
    type: fal
    base_url: "{fal_url}"
    platform_base_url: "{fal_url}"
    accounts:
      - id: test
        auth: {{ kind: api_key, inline: "fal-test-secret" }}
"#
    );
    let base = spawn_with_config(&config).await;
    let client = reqwest::Client::new();

    let response = authed(
        &client,
        reqwest::Method::POST,
        format!("{base}/v1/images/generations"),
    )
    .json(&json!({
        "model": "fal/fal-ai/qwen-image",
        "prompt": "do not retain this failed prompt",
        "n": 1
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_GATEWAY);

    let jobs: serde_json::Value = authed(&client, reqwest::Method::GET, format!("{base}/v1/jobs"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let failed = jobs["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["status"] == "failed")
        .expect("failed fal job is retained");
    let event_names: Vec<_> = failed["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert_eq!(event_names, vec!["accepted", "queued", "running", "failed"]);
    let serialized = serde_json::to_string(failed).unwrap();
    assert!(!serialized.contains("do not retain"));
    assert!(!serialized.contains("fal-test-secret"));

    let usage: serde_json::Value =
        authed(&client, reqwest::Method::GET, format!("{base}/v1/usage"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(usage["requests"], 1);
    assert_eq!(usage["unknown_cost_requests"], 1);
    assert!(usage["recent"][0]["cost_micros"].is_null());
}

#[tokio::test]
async fn fal_running_job_can_be_cancelled_through_the_public_job_route() {
    let (fal_url, fal) = spawn_slow_fake_fal().await;
    let config = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  block_private_networks: false
api_keys:
  - key: "sk-operator"
    tenant: test
    role: operator
tenants:
  - id: test
providers:
  - id: fal
    type: fal
    base_url: "{fal_url}"
    platform_base_url: "{fal_url}"
    accounts:
      - id: test
        auth: {{ kind: api_key, inline: "fal-test-secret" }}
"#
    );
    let base = spawn_with_config(&config).await;
    let client = reqwest::Client::new();

    let submit_client = client.clone();
    let submit_base = base.clone();
    let submit = tokio::spawn(async move {
        authed(
            &submit_client,
            reqwest::Method::POST,
            format!("{submit_base}/v1/images/generations"),
        )
        .json(&json!({
            "model": "fal/fal-ai/qwen-image",
            "prompt": "cancel this",
            "n": 1
        }))
        .send()
        .await
        .unwrap()
    });

    let mut running_job = None;
    for _ in 0..100 {
        let jobs: serde_json::Value =
            authed(&client, reqwest::Method::GET, format!("{base}/v1/jobs"))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
        running_job = jobs["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["status"] == "running")
            .and_then(|job| job["id"].as_str())
            .map(str::to_string);
        if running_job.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let job_id = running_job.expect("fal job becomes observable while running");

    let cancelled: serde_json::Value = authed(
        &client,
        reqwest::Method::POST,
        format!("{base}/v1/jobs/{job_id}/cancel"),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(*fal.cancel_calls.lock().unwrap(), 1);

    let submit_response = submit.await.unwrap();
    assert_eq!(submit_response.status(), reqwest::StatusCode::OK);
    let submit_body: serde_json::Value = submit_response.json().await.unwrap();
    assert_eq!(submit_body["job"]["status"], "cancelled");

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
        job["events"].as_array().unwrap().last().unwrap()["event"],
        "cancelled"
    );
    assert_eq!(
        job["events"].as_array().unwrap().last().unwrap()["status"],
        "cancelled"
    );

    let usage: serde_json::Value =
        authed(&client, reqwest::Method::GET, format!("{base}/v1/usage"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(usage["requests"], 1);
    assert_eq!(usage["unknown_cost_requests"], 1);
    assert!(usage["recent"][0]["cost_micros"].is_null());
}

fn mock_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

// --- ComfyUI behind a token proxy -------------------------------------------

const COMFY_SECRET: &str = "comfy-proxy-secret-7f3a91";

/// How the token proxy in front of the fake ComfyUI wants the credential.
#[derive(Clone, Copy, Debug)]
enum ProxyAuth {
    Bearer,
    Header,
    Query,
}

#[derive(Clone)]
struct TokenProxy {
    wants: ProxyAuth,
    /// Paths of the calls that carried the credential, and of those refused.
    passed: Arc<Mutex<Vec<String>>>,
    refused: Arc<Mutex<Vec<String>>>,
}

async fn require_comfy_credential(
    State(proxy): State<TokenProxy>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let headers = request.headers();
    let ok = match proxy.wants {
        ProxyAuth::Bearer => {
            headers.get("authorization").and_then(|v| v.to_str().ok())
                == Some(&format!("Bearer {COMFY_SECRET}"))
        }
        ProxyAuth::Header => {
            headers.get("x-comfy-key").and_then(|v| v.to_str().ok()) == Some(COMFY_SECRET)
        }
        ProxyAuth::Query => request.uri().query().is_some_and(|query| {
            query
                .split('&')
                .any(|pair| pair == format!("token={COMFY_SECRET}"))
        }),
    };
    let path = request.uri().path().to_string();
    if ok {
        proxy.passed.lock().unwrap().push(path);
        next.run(request).await
    } else {
        proxy.refused.lock().unwrap().push(path);
        axum::http::StatusCode::UNAUTHORIZED.into_response()
    }
}

async fn spawn_comfy_behind_proxy(wants: ProxyAuth) -> (String, TokenProxy) {
    let proxy = TokenProxy {
        wants,
        passed: Arc::new(Mutex::new(Vec::new())),
        refused: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/prompt", post(fake_comfy_prompt))
        .route("/history/{prompt_id}", get(fake_comfy_history))
        .route("/view", get(fake_comfy_view))
        .route("/system_stats", get(fake_comfy_system_stats))
        .with_state(FakeComfyState::default())
        .layer(axum::middleware::from_fn_with_state(
            proxy.clone(),
            require_comfy_credential,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), proxy)
}

/// A gateway whose `comfy` provider sits behind the proxy, managed as a
/// lane probed at the proxy's `/system_stats`. `auth` is the provider's
/// credential settings (empty: none).
fn comfy_behind_proxy_config(comfy_url: &str, auth: &str) -> String {
    let with_auth = one_comfy_workflow(comfy_url)
        .replace("    type: comfyui\n", &format!("    type: comfyui\n{auth}"));
    format!(
        r#"{with_auth}local_executors:
  - name: comfy
    base_url: "{comfy_url}"
    health_endpoint: "{comfy_url}/system_stats"
"#
    )
}

async fn capacity_json(base: &str) -> String {
    authed(
        &reqwest::Client::new(),
        reqwest::Method::GET,
        format!("{base}/v1/workloads/capacity"),
    )
    .send()
    .await
    .unwrap()
    .text()
    .await
    .unwrap()
}

/// A log sink for one test: everything the gateway logs on this thread.
#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLogs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn comfyui_behind_a_token_proxy_gets_the_credential_on_every_call() {
    let logs = CapturedLogs::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .finish();
    // Scoped to this test: `#[tokio::test]` runs the gateway, the fake
    // ComfyUI and the client on this one thread. A global subscriber would
    // collect every other test's lines for the rest of the run, at trace
    // level. With a single dispatcher registered, tracing computes a log
    // site's interest with the default of the thread that hits it first
    // (another test's, which takes nothing), so a second one is kept alive,
    // and the interest cache is rebuilt for the log sites other tests
    // registered before.
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::new());
    let _logs = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    for (wants, auth_scheme) in [
        (ProxyAuth::Bearer, ""),
        (
            ProxyAuth::Header,
            "    auth_scheme: { kind: header, name: x-comfy-key }\n",
        ),
        (
            ProxyAuth::Query,
            "    auth_scheme: { kind: query, name: token }\n",
        ),
    ] {
        let env = format!("SB_TEST_COMFY_PROXY_KEY_{wants:?}").to_uppercase();
        std::env::set_var(&env, COMFY_SECRET);
        let (comfy_url, proxy) = spawn_comfy_behind_proxy(wants).await;
        let auth = format!("    api_key_env: {env}\n{auth_scheme}");
        let base = spawn_with_config(&comfy_behind_proxy_config(&comfy_url, &auth)).await;

        let response = generate_one_image(&base).await;
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "{wants:?}: {body}");
        let created: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["job"]["status"], "succeeded", "{wants:?}");
        assert!(!body.contains(COMFY_SECRET), "{wants:?}: secret in the job");

        let passed = proxy.passed.lock().unwrap().clone();
        for path in ["/system_stats", "/prompt", "/history/prompt-123", "/view"] {
            assert!(
                passed.iter().any(|p| p == path),
                "{wants:?}: {path} in {passed:?}"
            );
        }
        assert!(proxy.refused.lock().unwrap().is_empty(), "{wants:?}");

        let capacity = capacity_json(&base).await;
        assert!(!capacity.contains(COMFY_SECRET), "{wants:?}: {capacity}");
        let capacity: serde_json::Value = serde_json::from_str(&capacity).unwrap();
        let lane = &capacity["data"][0];
        assert_eq!(lane["state"], "idle_countdown", "probe healthy: {lane}");
        assert!(lane.get("last_error").is_none(), "{lane}");
    }
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(!logged.is_empty(), "the gateway logged on this thread");
    assert!(!logged.contains(COMFY_SECRET), "secret in the logs");
}

#[tokio::test]
async fn comfyui_behind_a_token_proxy_without_the_credential_fails_as_an_auth_failure() {
    let (comfy_url, proxy) = spawn_comfy_behind_proxy(ProxyAuth::Bearer).await;

    // Managed lane: the probe is refused, so the job fails at once and says
    // why, instead of waiting out a boot timeout.
    let base = spawn_with_config(&comfy_behind_proxy_config(&comfy_url, "")).await;
    let started = std::time::Instant::now();
    let response = generate_one_image(&base).await;
    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("health endpoint answered 401: the engine refuses the credential"),
        "{message}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let capacity: serde_json::Value = serde_json::from_str(&capacity_json(&base).await).unwrap();
    assert_eq!(capacity["data"][0]["level"], "fail", "{capacity}");
    assert!(proxy.passed.lock().unwrap().is_empty());

    // Unmanaged provider: the first call is refused, with the same reading.
    let base = spawn_with_config(&one_comfy_workflow(&comfy_url)).await;
    let response = generate_one_image(&base).await;
    assert_eq!(response.status(), 502);
    let body: serde_json::Value = response.json().await.unwrap();
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("queue comfyui prompt failed with status 401 Unauthorized: ComfyUI refused the credential"),
        "{message}"
    );
}

// A ComfyUI job that failed after it was accepted shows in `/v1/jobs` with
// its failed state (it used to be missing), metadata only.
#[tokio::test]
async fn a_failed_comfyui_job_is_listed_with_its_failed_state() {
    let (comfy_url, _comfy) = spawn_fake_comfy_with(FakeComfyState {
        fail: true,
        ..FakeComfyState::default()
    })
    .await;
    let base = spawn_with_config(&one_comfy_workflow(&comfy_url)).await;
    assert_eq!(generate_one_image(&base).await.status(), 502);

    let jobs = authed(
        &reqwest::Client::new(),
        reqwest::Method::GET,
        format!("{base}/v1/jobs"),
    )
    .send()
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    assert!(!jobs.contains("quoted job input"), "{jobs}");
    assert!(
        !jobs.contains("a lighthouse"),
        "the prompt is never kept: {jobs}"
    );
    let jobs: serde_json::Value = serde_json::from_str(&jobs).unwrap();
    let job = &jobs["data"][0];
    assert_eq!(job["status"], "failed", "{job}");
    assert_eq!(job["target"], "comfy/txt2img");
    let failed = job["events"].as_array().unwrap().last().unwrap();
    assert_eq!(failed["event"], "failed");
    assert!(
        failed["detail"]["error"]
            .as_str()
            .unwrap()
            .contains("failed in ComfyUI (KSampler: torch.OutOfMemoryError)"),
        "{failed}"
    );
}

/// A second origin that records every request it gets: what a redirect
/// from ComfyUI (or a proxy in front of it) would hand the credential to.
async fn spawn_recording_origin() -> (String, Arc<Mutex<Vec<String>>>) {
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = seen.clone();
    let app = Router::new().fallback(move |request: axum::extract::Request| {
        let log = log.clone();
        async move {
            log.lock()
                .unwrap()
                .push(format!("{} {:?}", request.uri(), request.headers()));
            axum::http::StatusCode::NOT_FOUND
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), seen)
}

// A ComfyUI (or its proxy) that redirects to another origin must not get
// the credential forwarded there: a credentialed call follows no redirect,
// as the text adapters' calls do not.
#[tokio::test]
async fn a_redirect_never_carries_the_comfyui_credential_to_another_origin() {
    let (other, seen) = spawn_recording_origin().await;
    let target = format!("{other}/prompt");
    let app = Router::new().route(
        "/prompt",
        post(move || {
            let target = target.clone();
            async move {
                (
                    axum::http::StatusCode::TEMPORARY_REDIRECT,
                    [(axum::http::header::LOCATION, target)],
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let comfy_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    std::env::set_var("SB_TEST_COMFY_REDIRECT_KEY", COMFY_SECRET);
    let config = one_comfy_workflow(&comfy_url).replace(
        "    type: comfyui\n",
        "    type: comfyui\n    api_key_env: SB_TEST_COMFY_REDIRECT_KEY\n    auth_scheme: { kind: header, name: x-comfy-key }\n",
    );
    let base = spawn_with_config(&config).await;
    let response = generate_one_image(&base).await;
    let body = response.text().await.unwrap();
    assert!(!body.contains(COMFY_SECRET), "{body}");
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|request| !request.contains(COMFY_SECRET)),
        "the other origin got the credential: {seen:?}"
    );
}

// With a `query` scheme the URL carries the secret: a failed call's error
// text, the failed job in `/v1/jobs` and the reply never quote it.
#[tokio::test]
async fn a_failed_call_with_a_query_credential_never_shows_the_secret() {
    // A port that refuses connections: bind one, then let it go.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    std::env::set_var("SB_TEST_COMFY_QUERY_KEY", COMFY_SECRET);
    let config = one_comfy_workflow(&format!("http://{closed}")).replace(
        "    type: comfyui\n",
        "    type: comfyui\n    api_key_env: SB_TEST_COMFY_QUERY_KEY\n    auth_scheme: { kind: query, name: token }\n",
    );
    let base = spawn_with_config(&config).await;
    let response = generate_one_image(&base).await;
    assert_eq!(response.status(), 502);
    let body = response.text().await.unwrap();
    assert!(body.contains("queue comfyui prompt"), "{body}");
    assert!(!body.contains(COMFY_SECRET), "{body}");
    let jobs = authed(
        &reqwest::Client::new(),
        reqwest::Method::GET,
        format!("{base}/v1/jobs"),
    )
    .send()
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    assert!(
        jobs.contains("\"failed\""),
        "the failed job is listed: {jobs}"
    );
    assert!(!jobs.contains(COMFY_SECRET), "{jobs}");
}
