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
    fixture_with(gpu_up, optimize, "").await
}

/// `gpu_lane` is extra YAML for the gpu box's lane (indented 4 spaces).
async fn fixture_with(gpu_up: bool, optimize: &str, gpu_lane: &str) -> Fixture {
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
{gpu_lane}
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

/// Whole-day quiet windows (UTC), as a config reload sets them.
fn all_day_quiet(fx: &Fixture) {
    let mut cfg = (*fx.state.engine.snapshot().config).clone();
    cfg.server.optimize.quiet_windows = vec!["00:00-23:59".into(), "23:59-00:00".into()];
    fx.state.engine.reload(cfg).unwrap();
}

// A lock left by a tuner that died must not block the work a quiet window
// protects: once the window is in force, the lane serves everyone again.
#[tokio::test]
async fn a_quiet_window_sets_a_dead_tuners_lock_aside() {
    let fx = fixture(true, "    stickiness: false").await;
    let (status, lock) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a", "ttl_secs": 3600})),
    )
    .await;
    assert_eq!(status, 201, "{lock}");
    let (status, body, _) = chat_with(&fx.gw, ask("gpu-box/a"), None).await;
    assert_eq!(status, 503, "locked before the window: {body}");
    // The tuner is gone; the window begins.
    all_day_quiet(&fx);
    let (status, body, route) = chat_with(&fx.gw, ask("gpu-box/a"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["choices"][0]["message"]["content"], "ok from gpu-box");
    assert!(!route.contains("tuning run"), "{route}");
    let (_, body, _) = chat_with(&fx.gw, ask("pair"), None).await;
    assert_eq!(body["choices"][0]["message"]["content"], "ok from gpu-box");
}

// The lock, and the run's hold, end no later than the next quiet window's
// start, even when the run asked for longer.
#[tokio::test]
async fn a_lock_and_its_hold_end_at_the_next_quiet_window() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // A UTC window from 10 to 20 minutes from now.
    let start_ms = now / 60_000 * 60_000 + 10 * 60_000;
    let hhmm = |ms: i64| {
        let minute = (ms / 60_000).rem_euclid(1440);
        format!("{:02}:{:02}", minute / 60, minute % 60)
    };
    let window = format!(
        "    quiet_windows: [\"{}-{}\"]",
        hhmm(start_ms),
        hhmm(start_ms + 10 * 60_000)
    );
    let fx = fixture(true, &window).await;
    let (status, hold) = call(
        &fx.gw,
        "POST",
        "/v1/hosts/gpu-box/holds",
        ADMIN,
        Some(json!({"ttl_secs": 3600, "reason": "tune"})),
    )
    .await;
    assert_eq!(status, 201, "{hold}");
    let hold_id = hold["id"].as_str().unwrap().to_string();
    assert!(hold["expires_at_ms"].as_i64().unwrap() > start_ms);
    let (status, lock) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a", "ttl_secs": 3600, "hold_id": hold_id})),
    )
    .await;
    assert_eq!(status, 201, "{lock}");
    let expires = lock["expires_at_ms"].as_i64().unwrap();
    assert_eq!(expires, start_ms, "{lock}");
    let hold_end = lock["hold"]["expires_at_ms"].as_i64().unwrap();
    assert!(
        hold_end <= start_ms + 1_000 && hold_end > now,
        "hold {hold_end} vs window start {start_ms}: {lock}"
    );
    let (_, holds) = call(&fx.gw, "GET", "/v1/hosts/gpu-box/holds", ADMIN, None).await;
    assert_eq!(holds["data"][0]["expires_at_ms"].as_i64(), Some(hold_end));
}

// The tuner's own hold asks to end by the next quiet window's start, so a
// run cut off before it took its lock leaves no hold reaching into the
// window. A hold that does not ask keeps the time it asked for.
#[tokio::test]
async fn a_hold_that_asks_ends_at_the_next_quiet_window() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let start_ms = now / 60_000 * 60_000 + 10 * 60_000;
    let hhmm = |ms: i64| {
        let minute = (ms / 60_000).rem_euclid(1440);
        format!("{:02}:{:02}", minute / 60, minute % 60)
    };
    let window = format!(
        "    quiet_windows: [\"{}-{}\"]",
        hhmm(start_ms),
        hhmm(start_ms + 10 * 60_000)
    );
    let fx = fixture(true, &window).await;
    let (status, hold) = call(
        &fx.gw,
        "POST",
        "/v1/hosts/gpu-box/holds",
        ADMIN,
        Some(json!({"ttl_secs": 3600, "reason": "tune", "until_quiet_window": true})),
    )
    .await;
    assert_eq!(status, 201, "{hold}");
    let end = hold["expires_at_ms"].as_i64().unwrap();
    assert!(
        end <= start_ms + 1_000 && end > now,
        "hold {end} vs window start {start_ms}: {hold}"
    );
    let (status, plain) = call(
        &fx.gw,
        "POST",
        "/v1/hosts/gpu-box/holds",
        ADMIN,
        Some(json!({"ttl_secs": 3600, "reason": "other"})),
    )
    .await;
    assert_eq!(status, 201, "{plain}");
    assert!(plain["expires_at_ms"].as_i64().unwrap() > start_ms + 1_000);
}

// Tuning locks keep embeddings and audio off the lane too.
#[tokio::test]
async fn a_tuning_lock_covers_embeddings_and_audio() {
    let fx = fixture(true, "    stickiness: false").await;
    let (status, lock) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a", "ttl_secs": 600})),
    )
    .await;
    assert_eq!(status, 201, "{lock}");
    for (path, body) in [
        (
            "/v1/embeddings",
            json!({"model": "gpu-box/a", "input": "hi"}),
        ),
        (
            "/v1/audio/speech",
            json!({"model": "gpu-box/a", "input": "hi", "voice": "x"}),
        ),
    ] {
        let (status, answer) = call(&fx.gw, "POST", path, ADMIN, Some(body)).await;
        assert_eq!(status, 503, "{path}: {answer}");
        assert!(
            answer.to_string().contains("tuning run"),
            "{path}: {answer}"
        );
    }
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

// REREVIEW: the tuner's heartbeat (PATCH, ttl = budget left + 900 s) must
// keep the lock capped at the next quiet window's start, as POST does.
#[tokio::test]
async fn rereview_a_heartbeat_keeps_the_lock_inside_the_quiet_window_cap() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let start_ms = now / 60_000 * 60_000 + 10 * 60_000;
    let hhmm = |ms: i64| {
        let minute = (ms / 60_000).rem_euclid(1440);
        format!("{:02}:{:02}", minute / 60, minute % 60)
    };
    let window = format!(
        "    quiet_windows: [\"{}-{}\"]",
        hhmm(start_ms),
        hhmm(start_ms + 10 * 60_000)
    );
    let fx = fixture(true, &window).await;
    let (status, lock) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a", "ttl_secs": 3600})),
    )
    .await;
    assert_eq!(status, 201, "{lock}");
    assert_eq!(lock["expires_at_ms"].as_i64().unwrap(), start_ms);
    let id = lock["id"].as_str().unwrap().to_string();
    let (status, beat) = call(
        &fx.gw,
        "PATCH",
        &format!("/v1/optimize/locks/{id}"),
        ADMIN,
        Some(json!({"ttl_secs": 3600})),
    )
    .await;
    assert_eq!(status, 200, "{beat}");
    let expires = beat["expires_at_ms"].as_i64().unwrap();
    assert!(
        expires <= start_ms,
        "heartbeat moved the lock {} s past the quiet window start: {beat}",
        (expires - start_ms) / 1000
    );
}

fn quality_row(run_id: &str) -> Value {
    json!({
        "run_id": run_id, "target": "gpu-box/a", "suite": "s1", "suite_version": "abc123def456",
        "model_file": "sha256:aa", "engine_build": "b1", "settings_hash": "0123456789abcdef",
        "settings_revision": 2, "status": "complete", "score": 0.8, "se": 0.03,
        "per_case": {"c01": [1.0, 1.0], "c02": [0.5, 0.0]},
        "hard_fails": {"cut": 1}, "interval": [0.74, 0.86],
        "speed": {"median_s": 1.2, "p90_s": 2.5}
    })
}

// Test 19: the quality ledger keeps scores and ids, never text; client
// keys may neither write nor read it.
#[tokio::test]
async fn the_quality_ledger_refuses_text_and_round_trips_scores() {
    let fx = fixture(true, "    stickiness: false").await;
    // A key named like text, at any depth, is refused.
    for (i, row) in [
        {
            let mut row = quality_row("q_text1");
            row["answer"] = json!("x");
            row
        },
        {
            let mut row = quality_row("q_text2");
            row["detail"] = json!({"cases": [{"id": "c01", "Answer_text": "x"}]});
            row
        },
        {
            let mut row = quality_row("q_text3");
            row["speed"]["prompt_tokens"] = json!(10);
            row
        },
    ]
    .into_iter()
    .enumerate()
    {
        let (status, body) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(row)).await;
        assert_eq!(
            (status, body["error"]["reason"].as_str()),
            (400, Some("text_not_allowed")),
            "row {i}: {body}"
        );
    }
    // A string value over 200 characters is refused; exactly 200 is fine.
    let mut long = quality_row("q_long");
    long["note"] = json!("n".repeat(201));
    let (status, body) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(long)).await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (400, Some("text_not_allowed")),
        "{body}"
    );
    // Case ids are ids, whatever word they start with; anything else
    // under a case-id map is refused.
    let mut ids = quality_row("q_ids");
    ids["per_case"] = json!({"text01": [1.0], "answer-2": [0.0]});
    ids["case_detail"] = json!({"text01": [{"findings": 2, "passed": 1}]});
    let (status, body) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(ids)).await;
    assert_eq!(status, 201, "{body}");
    let mut sneaky = quality_row("q_sneaky");
    sneaky["per_case"] = json!({"a case id with spaces": [1.0]});
    let (status, body) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(sneaky)).await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (400, Some("text_not_allowed")),
        "{body}"
    );
    let mut nested = quality_row("q_nested");
    nested["case_detail"] = json!({"c01": [{"quote": 1}]});
    let (status, body) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(nested)).await;
    assert_eq!(
        (status, body["error"]["reason"].as_str()),
        (400, Some("text_not_allowed")),
        "{body}"
    );
    let mut edge = quality_row("q_edge");
    edge["note"] = json!("n".repeat(200));
    let (status, body) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(edge)).await;
    assert_eq!(status, 201, "{body}");
    // A malformed row is refused: score out of range, no per_case.
    let mut bad = quality_row("q_bad");
    bad["score"] = json!(1.5);
    let (status, _) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(bad)).await;
    assert_eq!(status, 400);
    let mut bad = quality_row("q_bad2");
    bad.as_object_mut().unwrap().remove("per_case");
    let (status, _) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(bad)).await;
    assert_eq!(status, 400);
    // A valid row round-trips; a second one with its run id is refused.
    let (status, body) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/quality",
        ADMIN,
        Some(quality_row("q_ok")),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["durable"], false);
    let (status, _) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/quality",
        ADMIN,
        Some(quality_row("q_ok")),
    )
    .await;
    assert_eq!(status, 409);
    let mut partial = quality_row("q_partial");
    partial["suite"] = json!("s2");
    partial["status"] = json!("partial");
    partial["score"] = Value::Null;
    partial["se"] = Value::Null;
    let (status, body) = call(&fx.gw, "POST", "/v1/optimize/quality", ADMIN, Some(partial)).await;
    assert_eq!(status, 201, "{body}");
    let (status, list) = call(
        &fx.gw,
        "GET",
        "/v1/optimize/quality?model=gpu-box/a&suite=s1",
        ADMIN,
        None,
    )
    .await;
    assert_eq!(status, 200, "{list}");
    let rows = list["data"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["run_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["q_ok", "q_edge", "q_ids"]
    );
    assert_eq!(rows[0]["per_case"]["c02"], json!([0.5, 0.0]));
    assert_eq!(rows[0]["score"], json!(0.8));
    assert!(rows[0]["at_ms"].as_i64().unwrap() > 0);
    let (_, all) = call(&fx.gw, "GET", "/v1/optimize/quality", ADMIN, None).await;
    assert_eq!(all["data"].as_array().unwrap().len(), 4);
    // Client keys: 403 both ways.
    let (status, _) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/quality",
        CLIENT,
        Some(quality_row("q_client")),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = call(&fx.gw, "GET", "/v1/optimize/quality", CLIENT, None).await;
    assert_eq!(status, 403);
}

// Test 20: a lane with `experiments: false` refuses the tuning lock before
// anything else, and the capacity view says so.
#[tokio::test]
async fn a_lane_without_experiments_refuses_the_lock_and_shows_the_flag() {
    let fx = fixture_with(
        true,
        "    quiet_windows: [\"00:00-23:59\", \"23:59-00:00\"]",
        "    experiments: false",
    )
    .await;
    // Even inside a quiet window, `experiments_off` is the answer.
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
        (409, Some("experiments_off")),
        "{body}"
    );
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("local_executors[].experiments: false"));
    let (status, capacity) = call(&fx.gw, "GET", "/v1/workloads/capacity", ADMIN, None).await;
    assert_eq!(status, 200);
    let lane = |name: &str| {
        capacity["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == name)
            .cloned()
            .unwrap()
    };
    assert_eq!(lane("gpu-box")["experiments"], json!(false));
    assert_eq!(lane("small-host")["experiments"], json!(true));
    // The default lane still takes a lock.
    let fx = fixture(true, "    stickiness: false").await;
    let (status, body) = call(
        &fx.gw,
        "POST",
        "/v1/optimize/locks",
        ADMIN,
        Some(json!({"target": "gpu-box/a"})),
    )
    .await;
    assert_eq!(status, 201, "{body}");
}

// The settings view names the answer-settings hash for operators only, and
// a stored ceiling leaves it alone while a temperature change moves it.
#[tokio::test]
async fn the_settings_view_carries_the_answer_settings_hash_for_operators() {
    let fx = fixture(true, "    stickiness: false").await;
    let hash = |view: &Value| view["targets"][0]["settings_hash"].clone();
    let (status, view) = call(&fx.gw, "GET", "/v1/models/gpu-box/a/settings", ADMIN, None).await;
    assert_eq!(status, 200, "{view}");
    let empty = hash(&view);
    assert_eq!(empty.as_str().unwrap().len(), 16);
    let (status, body) = call(
        &fx.gw,
        "PUT",
        "/v1/models/gpu-box/a/settings",
        ADMIN,
        Some(json!({"revision": 0, "defaults": {"temperature": 0.2}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, view) = call(&fx.gw, "GET", "/v1/models/gpu-box/a/settings", ADMIN, None).await;
    let warm = hash(&view);
    assert_ne!(warm, empty);
    let (status, body) = call(
        &fx.gw,
        "PUT",
        "/v1/models/gpu-box/a/settings",
        ADMIN,
        Some(json!({"revision": 1, "defaults": {"temperature": 0.2},
                    "ceiling": {"decode_tps": 40.0}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (_, view) = call(&fx.gw, "GET", "/v1/models/gpu-box/a/settings", ADMIN, None).await;
    assert_eq!(view["targets"][0]["revision"], json!(2));
    assert_eq!(hash(&view), warm);
    let (status, view) = call(&fx.gw, "GET", "/v1/models/gpu-box/a/settings", CLIENT, None).await;
    if status == 200 {
        assert!(view["targets"][0].get("settings_hash").is_none(), "{view}");
    }
}
