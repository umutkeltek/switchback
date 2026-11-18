//! Background jobs (`POST /v1/jobs`): answered at once, run on the same job
//! path as `/v1/images/generations`, hold a managed lane busy while they
//! wait or run, cancel for real, are bounded per tenant, and are never
//! silently lost across a restart.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// A ComfyUI stand-in. Its prompt stays pending until `done` is set; every
/// call is recorded with the credential header it carried.
#[derive(Clone, Default)]
struct FakeComfy {
    done: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
    bodies: Arc<Mutex<Vec<(String, Value)>>>,
    /// The `/view` answer's content type (default `image/png`).
    view_type: Arc<Mutex<Option<String>>>,
}

impl FakeComfy {
    fn record(&self, path: &str, headers: &HeaderMap) {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        self.calls.lock().unwrap().push((path.to_string(), auth));
    }

    fn paths(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(path, _)| path.clone())
            .collect()
    }

    fn body_of(&self, path: &str) -> Option<Value> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .find(|(seen, _)| seen == path)
            .map(|(_, body)| body.clone())
    }
}

fn png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

async fn spawn_comfy() -> (String, FakeComfy) {
    async fn prompt(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.record("/prompt", &headers);
        fake.bodies
            .lock()
            .unwrap()
            .push(("/prompt".to_string(), body));
        Json(json!({"prompt_id": "prompt-7", "number": 3, "node_errors": {}}))
    }
    async fn history(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Path(id): Path<String>,
    ) -> Json<Value> {
        fake.record("/history", &headers);
        if !fake.done.load(Ordering::SeqCst) {
            return Json(json!({}));
        }
        Json(json!({ id: {
            "status": {"completed": true},
            "outputs": {"9": {"images": [{"filename": "out.png", "subfolder": "", "type": "output"}]}}
        }}))
    }
    async fn view(State(fake): State<FakeComfy>, headers: HeaderMap) -> impl IntoResponse {
        fake.record("/view", &headers);
        let content_type = fake
            .view_type
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "image/png".to_string());
        ([("content-type", content_type)], png())
    }
    async fn queue(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.record("/queue", &headers);
        fake.bodies
            .lock()
            .unwrap()
            .push(("/queue".to_string(), body));
        Json(json!({}))
    }
    async fn interrupt(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.record("/interrupt", &headers);
        fake.bodies
            .lock()
            .unwrap()
            .push(("/interrupt".to_string(), body));
        Json(json!({}))
    }
    async fn stats() -> Json<Value> {
        Json(json!({"system": {"os": "test"}, "devices": []}))
    }
    let fake = FakeComfy::default();
    let app = Router::new()
        .route("/prompt", post(prompt))
        .route("/history/{id}", get(history))
        .route("/view", get(view))
        .route("/queue", post(queue))
        .route("/interrupt", post(interrupt))
        .route("/system_stats", get(stats))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), fake)
}

fn comfy_provider(comfy_url: &str) -> String {
    format!(
        r#"
providers:
  - id: comfy
    type: comfyui
    base_url: "{comfy_url}"
    workflows:
      - id: txt2img
        kind: image_generation
        graph:
          "3": {{ class_type: EmptyLatentImage, inputs: {{ width: 1, height: 1 }} }}
          "6": {{ class_type: CLIPTextEncode, inputs: {{ text: "" }} }}
          "9": {{ class_type: SaveImage, inputs: {{ filename_prefix: "out" }} }}
        bindings:
          prompt: {{ path: ["6", "inputs", "text"] }}
          width: {{ path: ["3", "inputs", "width"] }}
          height: {{ path: ["3", "inputs", "height"] }}
        output_node_ids: ["9"]
"#
    )
}

fn build_state(config: &str) -> sb_server::AppState {
    let cfg = sb_core::Config::from_yaml(config).unwrap();
    sb_runtime::Engine::validate_config(&cfg).expect("valid test config");
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
}

async fn serve(state: sb_server::AppState) -> String {
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn submit(base: &str, key: Option<&str>, body: Value) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/jobs"))
        .json(&body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    request.send().await.unwrap()
}

fn image_job(prompt: &str) -> Value {
    json!({
        "kind": "image_generation",
        "workflow": "comfy/txt2img",
        "inputs": {"prompt": prompt, "size": "64x32"}
    })
}

async fn job(base: &str, id: &str) -> Value {
    reqwest::get(format!("{base}/v1/jobs/{id}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_for_status(base: &str, id: &str, status: &str) -> Value {
    for _ in 0..200 {
        let current = job(base, id).await;
        if current["status"] == status {
            return current;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job {id} never reached `{status}`: {}", job(base, id).await);
}

async fn lane(base: &str, name: &str) -> Value {
    let report: Value = reqwest::get(format!("{base}/v1/workloads/capacity"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    report["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lane| lane["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no lane `{name}`: {report}"))
}

async fn cancel(base: &str, key: Option<&str>, id: &str) -> reqwest::Response {
    let mut request = reqwest::Client::new().post(format!("{base}/v1/jobs/{id}/cancel"));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    request.send().await.unwrap()
}

fn event_names(job: &Value) -> Vec<String> {
    job["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_background_job_answers_at_once_and_finishes_in_the_background() {
    let (comfy_url, fake) = spawn_comfy().await;
    let base = serve(build_state(&comfy_provider(&comfy_url))).await;

    let response = submit(&base, None, image_job("a lighthouse at dusk")).await;
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("job_"));
    assert_eq!(accepted["status"], "accepted");
    assert_eq!(accepted["kind"], "image_generation");
    assert_eq!(accepted["prompt_stored"], false);
    assert!(!accepted.to_string().contains("lighthouse"), "{accepted}");

    let running = wait_for_status(&base, &id, "running").await;
    assert!(event_names(&running).contains(&"running".to_string()));
    let prompt = fake.body_of("/prompt").unwrap();
    assert_eq!(
        prompt["prompt"]["6"]["inputs"]["text"],
        "a lighthouse at dusk"
    );
    assert_eq!(prompt["prompt"]["3"]["inputs"]["width"], 64);

    fake.done.store(true, Ordering::SeqCst);
    let done = wait_for_status(&base, &id, "succeeded").await;
    assert_eq!(
        event_names(&done),
        vec![
            "accepted",
            "running",
            "history_polled",
            "artifact_ready",
            "succeeded"
        ]
    );
    let artifact = done["artifacts"][0]["artifact_id"].as_str().unwrap();
    let body = reqwest::get(format!("{base}/v1/artifacts/{artifact}"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.as_ref(), png().as_slice());
    assert!(!done.to_string().contains("lighthouse"), "{done}");

    let listed: Value = reqwest::get(format!("{base}/v1/jobs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|listed| listed["id"] == id.as_str()));
}

fn managed_lane(comfy_url: &str, health_url: &str, extra: &str) -> String {
    format!(
        r#"{providers}
local_executors:
  - name: comfy
    base_url: "{comfy_url}"
    health_endpoint: "{health_url}"
    health_poll_interval_ms: 50
{extra}
"#,
        providers = comfy_provider(comfy_url)
    )
}

#[tokio::test]
async fn a_running_job_holds_its_lane_busy_and_a_cancel_interrupts_comfyui_and_frees_it() {
    let (comfy_url, fake) = spawn_comfy().await;
    let health = format!("{comfy_url}/system_stats");
    let base = serve(build_state(&managed_lane(&comfy_url, &health, ""))).await;

    let accepted: Value = submit(&base, None, image_job("hold the lane"))
        .await
        .json()
        .await
        .unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    let running = wait_for_status(&base, &id, "running").await;
    let waited = running["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "waiting_for_lane")
        .cloned()
        .expect("a managed job reports its wait for the lane");
    assert_eq!(waited["status"], "queued");
    assert_eq!(waited["detail"]["lane"], "comfy");
    assert_eq!(
        lane(&base, "comfy").await["in_flight"],
        1,
        "the job holds the lane"
    );

    let response = cancel(&base, None, &id).await;
    assert_eq!(response.status(), 200);
    let cancelled: Value = response.json().await.unwrap();
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    let last = cancelled["events"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["event"], "cancelled");
    assert_eq!(last["detail"]["upstream_cancel"], "sent", "{last}");
    assert_eq!(
        fake.body_of("/queue").unwrap(),
        json!({"delete": ["prompt-7"]})
    );
    assert_eq!(
        fake.body_of("/interrupt").unwrap(),
        json!({"prompt_id": "prompt-7"})
    );

    let mut freed = false;
    for _ in 0..40 {
        if lane(&base, "comfy").await["in_flight"] == 0 {
            freed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        freed,
        "the cancel releases the lane: {}",
        lane(&base, "comfy").await
    );

    // A late answer from ComfyUI cannot undo the cancel.
    fake.done.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(job(&base, &id).await["status"], "cancelled");
    assert!(!fake.paths().contains(&"/view".to_string()));
}

#[tokio::test]
async fn a_job_waiting_for_a_wake_is_queued_and_a_cancel_drops_it_from_the_lane_queue() {
    let (comfy_url, fake) = spawn_comfy().await;
    // Health on a port that refuses: the lane stays down while it "wakes".
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let config = managed_lane(
        &comfy_url,
        &format!("http://{closed}/system_stats"),
        "    wake_command: \"true\"\n    boot_timeout_secs: 60\n",
    );
    let base = serve(build_state(&config)).await;

    let accepted: Value = submit(&base, None, image_job("wait for the machine"))
        .await
        .json()
        .await
        .unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    let queued = wait_for_status(&base, &id, "queued").await;
    let waited = queued["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(waited["event"], "waiting_for_lane", "{queued}");
    let mut counted = false;
    for _ in 0..40 {
        if lane(&base, "comfy").await["queue_depth"] == 1 {
            counted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(counted, "the job waits in the lane's queue");

    let cancelled: Value = cancel(&base, None, &id).await.json().await.unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(
        cancelled["events"].as_array().unwrap().last().unwrap()["detail"]
            ["upstream_cancel_requested"],
        "false"
    );
    let mut dropped = false;
    for _ in 0..40 {
        let report = lane(&base, "comfy").await;
        if report["queue_depth"] == 0 && report["in_flight"] == 0 {
            dropped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(dropped, "{}", lane(&base, "comfy").await);
    assert!(
        fake.paths().is_empty(),
        "nothing reached ComfyUI: {:?}",
        fake.paths()
    );
}

#[tokio::test]
async fn a_tenant_over_its_pending_job_limit_gets_429() {
    let (comfy_url, _fake) = spawn_comfy().await;
    let config = format!(
        "server:\n  bind: \"127.0.0.1:0\"\n  jobs:\n    max_pending_per_tenant: 1\n{}",
        comfy_provider(&comfy_url)
    );
    let base = serve(build_state(&config)).await;
    let first = submit(&base, None, image_job("one")).await;
    assert_eq!(first.status(), 202);
    let first: Value = first.json().await.unwrap();

    let second = submit(&base, None, image_job("two")).await;
    assert_eq!(second.status(), 429);
    let error: Value = second.json().await.unwrap();
    assert_eq!(error["error"]["type"], "rate_limit_error");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("server.jobs.max_pending_per_tenant"));

    cancel(&base, None, first["id"].as_str().unwrap()).await;
    let third = submit(&base, None, image_job("three")).await;
    assert_eq!(third.status(), 202, "a cancel frees the slot");
}

#[tokio::test]
async fn a_restart_marks_an_unfinished_job_failed_with_its_reason() {
    let (comfy_url, _fake) = spawn_comfy().await;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("sb-jobs-restart-{}-{nanos}", std::process::id()));
    let config = format!(
        "server:\n  bind: \"127.0.0.1:0\"\n  artifacts:\n    dir: \"{}\"\n{}",
        dir.display(),
        comfy_provider(&comfy_url)
    );
    let first = serve(build_state(&config)).await;
    let accepted: Value = submit(&first, None, image_job("never finishes"))
        .await
        .json()
        .await
        .unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    wait_for_status(&first, &id, "running").await;

    // A second gateway on the same state: a restart.
    let second = serve(build_state(&config)).await;
    let after = job(&second, &id).await;
    assert_eq!(after["status"], "failed", "{after}");
    let last = after["events"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["detail"]["error_class"], "restart");
    assert!(last["detail"]["error"]
        .as_str()
        .unwrap()
        .contains("restarted before the job finished"));
    assert!(!after.to_string().contains("never finishes"));
}

#[tokio::test]
async fn a_job_request_is_checked_before_the_job_exists() {
    let (comfy_url, fake) = spawn_comfy().await;
    let base = serve(build_state(&comfy_provider(&comfy_url))).await;
    let cases = [
        (
            json!({"kind": "image_generation", "workflow": "comfy/nope", "inputs": {"prompt": "x"}}),
            400,
        ),
        (
            json!({"kind": "image_generation", "workflow": "comfy/txt2img", "model": "comfy/txt2img", "inputs": {"prompt": "x"}}),
            400,
        ),
        (
            json!({"kind": "video_generation", "workflow": "comfy/txt2img", "inputs": {"prompt": "x"}}),
            400,
        ),
        (
            json!({"kind": "image_generation", "workflow": "comfy/txt2img", "inputs": {"prompt": ""}}),
            400,
        ),
        (
            json!({"kind": "image_generation", "workflow": "comfy/txt2img", "inputs": {"prompt": "x", "frames": 9}}),
            400,
        ),
        (
            json!({"kind": "image_generation", "workflow": "comfy/txt2img", "inputs": {"prompt": "x", "image": "art_missing"}}),
            400,
        ),
        (
            json!({"kind": "text_generation", "workflow": "comfy/txt2img", "inputs": {"prompt": "x"}}),
            400,
        ),
        (
            json!({"kind": "image_generation", "workflow": "comfy/txt2img", "inputs": {"prompt": "x", "graph": {}}}),
            422,
        ),
        (
            json!({"kind": "image_generation", "workflow": "comfy/txt2img", "graph": {}, "inputs": {"prompt": "x"}}),
            422,
        ),
    ];
    for (body, status) in cases {
        let response = submit(&base, None, body.clone()).await;
        assert_eq!(response.status(), status, "{body}");
    }
    let listed: Value = reqwest::get(format!("{base}/v1/jobs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(listed["data"].as_array().unwrap().is_empty(), "{listed}");
    assert!(fake.paths().is_empty());

    // The mock workflow runs as a background job too.
    let accepted: Value = submit(
        &base,
        None,
        json!({"kind": "image_generation", "workflow": "mock-image", "inputs": {"prompt": "x"}}),
    )
    .await
    .json()
    .await
    .unwrap();
    wait_for_status(&base, accepted["id"].as_str().unwrap(), "succeeded").await;
}

#[tokio::test]
async fn a_client_key_submits_and_cancels_its_own_jobs_only() {
    let (comfy_url, _fake) = spawn_comfy().await;
    let config = format!(
        r#"
api_keys:
  - {{ key: "sk-test-client-a", tenant: team-a }}
  - {{ key: "sk-test-client-b", tenant: team-b }}
tenants:
  - id: team-a
  - id: team-b
{}"#,
        comfy_provider(&comfy_url)
    );
    let base = serve(build_state(&config)).await;
    let response = submit(&base, Some("sk-test-client-a"), image_job("mine")).await;
    assert_eq!(response.status(), 202);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let other = cancel(&base, Some("sk-test-client-b"), &id).await;
    assert_eq!(other.status(), 404);
    let own = cancel(&base, Some("sk-test-client-a"), &id).await;
    assert_eq!(own.status(), 200);
    assert_eq!(own.json::<Value>().await.unwrap()["status"], "cancelled");
}

#[tokio::test]
async fn a_blocking_image_request_still_answers_with_its_image() {
    let (comfy_url, fake) = spawn_comfy().await;
    fake.done.store(true, Ordering::SeqCst);
    let base = serve(build_state(&comfy_provider(&comfy_url))).await;
    let created: Value = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "comfy/txt2img", "prompt": "blocking", "n": 1}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["object"], "image.generation");
    assert_eq!(created["job"]["status"], "succeeded", "{created}");
    assert_eq!(created["data"][0]["media_type"], "image/png");
}

#[tokio::test]
async fn an_upstream_content_type_is_never_served_back_as_is() {
    let (comfy_url, fake) = spawn_comfy().await;
    *fake.view_type.lock().unwrap() = Some("text/html".to_string());
    fake.done.store(true, Ordering::SeqCst);
    let base = serve(build_state(&comfy_provider(&comfy_url))).await;

    let accepted: Value = submit(&base, None, image_job("a page"))
        .await
        .json()
        .await
        .unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    let done = wait_for_status(&base, &id, "succeeded").await;
    let artifact = &done["artifacts"][0];
    assert_eq!(artifact["media_type"], "image/png", "{done}");
    let response = reqwest::get(format!(
        "{base}/v1/artifacts/{}",
        artifact["artifact_id"].as_str().unwrap()
    ))
    .await
    .unwrap();
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
}

#[tokio::test]
async fn an_unknown_image_model_is_an_error_and_never_a_placeholder_image() {
    let (comfy_url, fake) = spawn_comfy().await;
    let base = serve(build_state(&comfy_provider(&comfy_url))).await;
    let blocking = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "comfy/no-such-workflow", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocking.status(), 400);
    let error: Value = blocking.json().await.unwrap();
    assert_eq!(error["error"]["type"], "workload_error");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown image model"),
        "{error}"
    );
    let background = submit(
        &base,
        None,
        json!({"kind": "image_generation", "model": "nobody/knows", "inputs": {"prompt": "a cat"}}),
    )
    .await;
    assert_eq!(background.status(), 400);
    // The mock stays reachable by its own name.
    let mock = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .json(&json!({"model": "mock/image", "prompt": "a cat"}))
        .send()
        .await
        .unwrap();
    assert_eq!(mock.status(), 200);
    let listed: Value = reqwest::get(format!("{base}/v1/jobs"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["data"].as_array().unwrap().len(), 1, "{listed}");
    assert!(fake.paths().is_empty());
}
