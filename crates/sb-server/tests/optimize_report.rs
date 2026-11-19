//! The speed report and the tuning plumbing end to end: the report never
//! wakes a machine or calls an engine, a client key gets its own view,
//! a tuning lock keeps other requests off its lane (and refuses a busy lane,
//! a quiet window and a second run), the experiment ledger round-trips, and
//! a settings write keeps its note in the history.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
struct Machine {
    name: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
    up: PathBuf,
}

fn temp(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sb-optimize-report-{name}-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ))
}

impl Machine {
    fn new(name: &'static str, up: bool) -> Self {
        let path = temp(&format!("{name}-up"));
        if up {
            std::fs::write(&path, b"up").unwrap();
        }
        Machine {
            name,
            calls: Arc::default(),
            up: path,
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

async fn health(State(m): State<Machine>) -> StatusCode {
    if m.up.exists() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn models(State(m): State<Machine>) -> Json<Value> {
    m.calls.lock().unwrap().push("GET /models".into());
    Json(json!({"data": [{"id": "a", "status": {"value": "loaded"}}]}))
}

async fn chat(State(m): State<Machine>, Json(body): Json<Value>) -> Json<Value> {
    m.calls.lock().unwrap().push("chat".into());
    if body.to_string().contains("slow please") {
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let thinking_off = body["chat_template_kwargs"]["enable_thinking"] == json!(false);
    let (content, finish) = if thinking_off {
        (format!("ok from {}", m.name), "stop")
    } else {
        (String::new(), "length")
    };
    Json(json!({
        "id": "x", "object": "chat.completion", "model": body["model"],
        "choices": [{"index": 0, "finish_reason": finish,
                     "message": {"role": "assistant", "content": content,
                                 "reasoning_content": "hmm"}}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
        "timings": {"prompt_n": 10, "prompt_ms": 5.0, "predicted_n": 5, "predicted_ms": 50.0}
    }))
}

async fn listen(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn serve(machine: &Machine) -> String {
    listen(
        Router::new()
            .route("/health", get(health))
            .route("/models", get(models))
            .route("/v1/chat/completions", post(chat))
            .with_state(machine.clone()),
    )
    .await
}

struct Fixture {
    gw: String,
    state: sb_server::AppState,
    gpu: Machine,
    wakes: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.gpu.up);
        let _ = std::fs::remove_file(&self.wakes);
    }
}

const ADMIN: &str = "sk-admin-test";
const CLIENT: &str = "sk-client-test";

async fn fixture(gpu_up: bool, optimize: &str) -> Fixture {
    let gpu = Machine::new("gpu-box", gpu_up);
    let small = Machine::new("small-host", true);
    let gpu_url = serve(&gpu).await;
    let small_url = serve(&small).await;
    let wakes = temp("wakes");
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  retry: {{ max_retries: 0 }}
  optimize:
{optimize}
tenants:
  - id: ops
  - id: t1
api_keys:
  - {{ key: "{ADMIN}", tenant: ops, role: admin }}
  - {{ key: "{CLIENT}", tenant: t1, role: client }}
providers:
  - id: gpu-box
    type: openai_compatible
    engine: llama_cpp
    base_url: "{gpu_url}/v1"
    accounts: [{{ id: a, auth: {{ kind: none }} }}]
  - id: small-host
    type: openai_compatible
    engine: llama_cpp
    base_url: "{small_url}/v1"
    accounts: [{{ id: a, auth: {{ kind: none }} }}]
routes:
  - name: pair
    match: {{ model: "pair" }}
    targets: ["gpu-box/a", "small-host/m"]
local_executors:
  - name: gpu-box
    base_url: "{gpu_url}"
    health_endpoint: "{gpu_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 0
    wake_command: "echo wake >> '{wakes}'"
    boot_timeout_secs: 5
    max_in_flight: 4
    engine:
      kind: llama_cpp_router
      settings_command: "cat > /dev/null"
  - name: small-host
    base_url: "{small_url}"
    health_endpoint: "{small_url}/health"
"#,
        wakes = wakes.display(),
    );
    let cfg = sb_core::Config::from_yaml(&cfg).unwrap();
    sb_runtime::Engine::validate_config(&cfg).expect("valid test config");
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    let gw = listen(sb_server::build_app(state.clone())).await;
    state.capacity.observe_all().await;
    Fixture {
        gw,
        state,
        gpu,
        wakes,
    }
}

async fn call(gw: &str, method: &str, path: &str, key: &str, body: Option<Value>) -> (u16, Value) {
    let client = reqwest::Client::new();
    let mut request = match method {
        "GET" => client.get(format!("{gw}{path}")),
        "POST" => client.post(format!("{gw}{path}")),
        "PUT" => client.put(format!("{gw}{path}")),
        "PATCH" => client.patch(format!("{gw}{path}")),
        _ => client.delete(format!("{gw}{path}")),
    }
    .bearer_auth(key);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let r = request.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn chat_with(gw: &str, body: Value, lock: Option<&str>) -> (u16, Value, String) {
    let mut request = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth(ADMIN)
        .json(&body);
    if let Some(lock) = lock {
        request = request.header("x-switchback-tune-lock", lock);
    }
    let r = request.send().await.unwrap();
    let route = r
        .headers()
        .get("x-switchback-route")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    (
        r.status().as_u16(),
        r.json().await.unwrap_or(Value::Null),
        route,
    )
}

fn ask(model: &str) -> Value {
    json!({"model": model, "max_tokens": 50,
           "chat_template_kwargs": {"enable_thinking": false},
           "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn the_report_never_wakes_or_calls_an_engine_and_names_findings() {
    let fx = fixture(false, "    stickiness: false").await;
    // An empty answer after thinking on the small host.
    let (status, _, _) = chat_with(
        &fx.gw,
        json!({"model": "small-host/m", "max_tokens": 50,
               "messages": [{"role": "user", "content": "think"}]}),
        None,
    )
    .await;
    assert_eq!(status, 200);
    let before = fx.gpu.calls().len();
    let (status, report) = call(&fx.gw, "GET", "/v1/optimize/report", ADMIN, None).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["view"], "operator");
    let targets: Vec<&str> = report["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["target"].as_str().unwrap())
        .collect();
    assert!(
        targets.contains(&"gpu-box/a") && targets.contains(&"small-host/m"),
        "{targets:?}"
    );
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "empty_after_thinking")
        .expect("finding");
    assert_eq!(finding["target"], "small-host/m");
    // The gateway's own evidence agrees.
    let obs = fx.state.engine.optimize().observations();
    assert_eq!(obs["small-host/m"].empty_after_thinking, 1);
    // The sleeping GPU box was neither woken nor called.
    assert_eq!(fx.gpu.calls().len(), before);
    assert!(!fx.wakes.exists(), "the report woke a machine");
    // A client key sees its own view, without findings or gateway counters.
    let (status, client) = call(&fx.gw, "GET", "/v1/optimize/report", CLIENT, None).await;
    assert_eq!(status, 200, "{client}");
    assert_eq!(client["view"], "client");
    assert!(client["findings"].as_array().unwrap().is_empty());
    // A client key may not take a tuning lock.
    let (status, _) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        CLIENT,
        Some(json!({"target": "gpu-box/a"})),
    )
    .await;
    assert_eq!(status, 403);
}

#[tokio::test]
async fn a_tuning_lock_keeps_other_requests_off_its_lane() {
    let fx = fixture(true, "    stickiness: false").await;
    let (status, lock) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a", "ttl_secs": 600, "reason": "test"})),
    )
    .await;
    assert_eq!(status, 201, "{lock}");
    let id = lock["id"].as_str().unwrap().to_string();
    // Another request goes elsewhere, and says why.
    let (status, body, route) = chat_with(&fx.gw, ask("pair"), None).await;
    assert_eq!(status, 200);
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "ok from small-host"
    );
    assert!(route.contains("selected=small-host/m"), "{route}");
    // Only the gpu box: a 503 to retry.
    let (status, body, _) = chat_with(&fx.gw, ask("gpu-box/a"), None).await;
    assert_eq!(status, 503, "{body}");
    assert!(body.to_string().contains("tuning run"), "{body}");
    // The run's own requests use the lane.
    let (status, body, _) = chat_with(&fx.gw, ask("pair"), Some(&id)).await;
    assert_eq!(status, 200);
    assert_eq!(body["choices"][0]["message"]["content"], "ok from gpu-box");
    // A second run is refused.
    let (status, body) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (409, Some("locked"))
    );
    let (status, _) = call(
        &fx.gw,
        "DELETE",
        &format!("/v1/optimize/locks/{id}"),
        ADMIN,
        None,
    )
    .await;
    assert_eq!(status, 200);
    let (_, body, _) = chat_with(&fx.gw, ask("pair"), None).await;
    assert_eq!(body["choices"][0]["message"]["content"], "ok from gpu-box");
}

#[tokio::test]
async fn a_lock_is_refused_on_a_busy_lane() {
    let fx = fixture(true, "    stickiness: false").await;
    let gw = fx.gw.clone();
    let slow = tokio::spawn(async move {
        chat_with(
            &gw,
            json!({"model": "gpu-box/a", "max_tokens": 50,
                   "chat_template_kwargs": {"enable_thinking": false},
                   "messages": [{"role": "user", "content": "slow please"}]}),
            None,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(250)).await;
    let (status, body) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (409, Some("lane_busy")),
        "{body}"
    );
    slow.await.unwrap();
}

#[tokio::test]
async fn a_lock_is_refused_inside_a_quiet_window() {
    let fx = fixture(
        true,
        "    quiet_windows: [\"00:00-23:59\", \"23:59-00:00\"]",
    )
    .await;
    let (status, body) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a"})),
    )
    .await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (409, Some("quiet_window")),
        "{body}"
    );
}

#[tokio::test]
async fn quiet_windows_name_their_time_zone_and_reach_a_running_lock() {
    // A whole-day window in an IANA zone: refused, and the zone is named.
    let fx = fixture(
        true,
        "    timezone: Asia/Tokyo\n    quiet_windows: [\"00:00-23:59\", {start: \"23:59\", end: \"00:00\", tz: UTC}]",
    )
    .await;
    let (status, body) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a"})),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Asia/Tokyo") || message.contains("UTC"),
        "{message}"
    );
    // A lock taken before a window begins: its heartbeat (PATCH) says the
    // window is in force, so the run stops at its next safe point.
    let fx = fixture(true, "    stickiness: false").await;
    let (status, lock) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a", "ttl_secs": 60})),
    )
    .await;
    assert_eq!(status, 201, "{lock}");
    let id = lock["id"].as_str().unwrap().to_string();
    let path = format!("/v1/optimize/locks/{id}");
    let (status, beat) = call(&fx.gw, "PATCH", &path, ADMIN, Some(json!({"ttl_secs": 60}))).await;
    assert_eq!(status, 200, "{beat}");
    assert_eq!(beat["quiet_window"], Value::Null);
    let mut cfg = (*fx.state.engine.snapshot().config).clone();
    cfg.server.optimize.quiet_windows = vec![
        sb_core::QuietWindow::Spec(sb_core::QuietWindowSpec {
            start: "00:00".into(),
            end: "23:59".into(),
            tz: Some("Asia/Tokyo".into()),
        }),
        sb_core::QuietWindow::Spec(sb_core::QuietWindowSpec {
            start: "23:59".into(),
            end: "00:00".into(),
            tz: Some("Asia/Tokyo".into()),
        }),
    ];
    fx.state.engine.reload(cfg).unwrap();
    let (status, beat) = call(&fx.gw, "PATCH", &path, ADMIN, Some(json!({"ttl_secs": 60}))).await;
    assert_eq!(status, 200, "{beat}");
    assert!(
        beat["quiet_window"]
            .as_str()
            .is_some_and(|w| w.ends_with("Asia/Tokyo")),
        "{beat}"
    );
    // A lock the gateway no longer has (a restart): 404 on the heartbeat.
    let (status, _) = call(&fx.gw, "DELETE", &path, ADMIN, None).await;
    assert_eq!(status, 200);
    let (status, _) = call(&fx.gw, "PATCH", &path, ADMIN, Some(json!({"ttl_secs": 60}))).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn an_unknown_time_zone_is_a_config_error() {
    let cfg = sb_core::Config::from_yaml(
        r#"
server:
  bind: "127.0.0.1:0"
  optimize:
    quiet_windows: [{start: "00:30", end: "03:00", tz: "Mars/Base"}]
"#,
    )
    .unwrap();
    let e = sb_runtime::Engine::validate_config(&cfg).unwrap_err();
    assert!(e.contains("Mars/Base"), "{e}");
}

#[tokio::test]
async fn the_experiment_ledger_and_settings_notes_round_trip() {
    let fx = fixture(true, "    stickiness: false").await;
    for (run, lever) in [("tune_1", "flash_attn"), ("tune_2", "draft_max")] {
        let (status, body) = call(
            &fx.gw,
            "POST",
            "/v1/optimize/experiments",
            ADMIN,
            Some(json!({"target": "gpu-box/a", "run_id": run, "lever": lever,
                        "verdict": "rejected", "engine_build": "b1"})),
        )
        .await;
        assert_eq!(status, 201, "{body}");
    }
    let (status, list) = call(
        &fx.gw,
        "GET",
        "/v1/optimize/experiments?model=gpu-box/a",
        ADMIN,
        None,
    )
    .await;
    assert_eq!(status, 200);
    let levers: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["lever"].as_str().unwrap())
        .collect();
    assert_eq!(levers, ["draft_max", "flash_attn"]);
    // A row without its run id is refused.
    let (status, _) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/experiments",
        ADMIN,
        Some(json!({"target": "gpu-box/a"})),
    )
    .await;
    assert_eq!(status, 400);
    // A settings write keeps its note in the history.
    let (status, body) = call(
        &fx.gw,
        "PUT",
        "/v1/models/gpu-box/a/settings",
        ADMIN,
        Some(json!({"revision": 0, "load": {"flash_attn": "on"}, "note": "tune tune_2"})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, history) = call(
        &fx.gw,
        "GET",
        "/v1/models/gpu-box/a/settings/history",
        ADMIN,
        None,
    )
    .await;
    assert_eq!(
        history["history"][0]["action"], "put: tune tune_2",
        "{history}"
    );
    // The state is the gateway's: nothing else holds it.
    let _ = fx.state.engine.optimize().locks();
}
