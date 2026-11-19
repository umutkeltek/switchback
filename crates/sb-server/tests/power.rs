//! The power endpoint over HTTP: `POST /v1/hosts/{host}/power`.
//!
//! An operator (or an admin) can power a managed machine off now, through
//! the same guard command and the same power-off command the idle timer
//! uses, and wake it again. A client key may not: a machine's power is the
//! operator's business.
//!
//! Every refusal is a 409 with a typed `reason`: a hold is active, a lane
//! has work in flight or queued, the guard says the machine is busy, the
//! machine has not been up long enough, or it has no power-off command at
//! all. A machine without a power-off command is never powered off.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const CLIENT: &str = "sk-test-client-a";
const OPERATOR: &str = "sk-test-operator";
const ADMIN: &str = "sk-test-admin";

/// A path under the temp dir for a marker file a fake command writes.
fn marker(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sb-power-{name}-{}-{nanos}", std::process::id()))
}

/// The config of one managed machine. `lane_extra` is appended to the lane
/// (its commands), so each test can give the machine its own behaviour.
fn config(lane_extra: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
  max_hold_secs: 7200
api_keys:
  - {{ key: "{CLIENT}", tenant: team-a }}
  - {{ key: "{OPERATOR}", tenant: ops, role: operator }}
  - {{ key: "{ADMIN}", tenant: ops, role: admin }}
tenants:
  - id: team-a
  - id: ops
providers:
  - id: text
    type: openai_compatible
    base_url: "http://127.0.0.1:9/v1"
local_executors:
  - name: text
    host: gpu-box
    base_url: "http://127.0.0.1:9"
    health_endpoint: "http://127.0.0.1:9/health"
{lane_extra}
"#
    )
}

async fn serve(config: &str) -> String {
    let cfg = sb_core::Config::from_yaml(config).unwrap();
    sb_runtime::Engine::validate_config(&cfg).expect("valid test config");
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

async fn power(base: &str, key: &str, host: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/hosts/{host}/power"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn take_hold(base: &str, key: &str, reason: &str) -> Value {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/hosts/gpu-box/holds"))
        .bearer_auth(key)
        .json(&json!({"ttl_secs": 3600, "reason": reason}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    response.json().await.unwrap()
}

// The operator powers the machine off now: the power-off command runs (the
// marker file says so) and the answer names the machine.
#[tokio::test]
async fn an_operator_powers_a_machine_off_now() {
    let marker = marker("off");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;

    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["host"], "gpu-box");
    assert_eq!(body["action"], "off");
    assert_eq!(body["powered_off"], true);
    assert_eq!(body["released_holds"], json!([]));

    let ran = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(ran.contains("off"), "the power-off command ran: {ran:?}");
    let _ = std::fs::remove_file(&marker);
}

// A second power-off on a machine this gateway powered off is a no-op, not
// a failed power-off: a command that reaches a sleeping machine fails, and
// a false failure would escalate the lane and alert.
#[tokio::test]
async fn a_repeat_power_off_is_a_no_op() {
    let marker = marker("repeat");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;

    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 200, "{body}");
    let first = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(first.lines().count(), 1, "{first:?}");

    // The machine is off: the second request is a no-op, not a failure.
    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["powered_off"], true);
    let second = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(
        second.lines().count(),
        1,
        "the command ran again: {second:?}"
    );
    let _ = std::fs::remove_file(&marker);
}

// A client key may not touch a machine's power: 403, and the machine keeps
// running.
#[tokio::test]
async fn a_client_key_cannot_power_a_machine() {
    let marker = marker("client");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;

    let (status, body) = power(&base, CLIENT, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["type"], "permission_error");

    let (status, body) = power(&base, CLIENT, "gpu-box", json!({"action": "on"})).await;
    assert_eq!(status, 403, "{body}");

    // The power-off command never ran.
    assert!(!marker.exists(), "a client key powered a machine off");
}

// An admin key may, like an operator.
#[tokio::test]
async fn an_admin_key_powers_a_machine_off_now() {
    let marker = marker("admin");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;
    let (status, body) = power(&base, ADMIN, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["powered_off"], true);
    let _ = std::fs::remove_file(&marker);
}

// A hold keeps the machine on: the power-off is refused with a typed
// reason, and the machine's power-off command never runs.
#[tokio::test]
async fn a_hold_keeps_the_machine_on() {
    let marker = marker("hold");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;
    let hold = take_hold(&base, CLIENT, "a benchmark").await;

    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["reason"], "hold_active");
    assert_eq!(body["error"]["type"], "power_refused");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains(hold["id"].as_str().unwrap()),
        "the refusal names the hold: {body}"
    );
    assert!(!marker.exists(), "the machine was powered off under a hold");
}

// With `release_holds` the holds end first, and the answer says which.
#[tokio::test]
async fn release_holds_lets_the_machine_go() {
    let marker = marker("release");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;
    let hold = take_hold(&base, CLIENT, "a benchmark").await;
    let hold_id = hold["id"].as_str().unwrap().to_string();

    let (status, body) = power(
        &base,
        OPERATOR,
        "gpu-box",
        json!({"action": "off", "release_holds": true}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["powered_off"], true);
    assert_eq!(body["released_holds"], json!([hold_id]));
    assert!(marker.exists(), "the power-off command ran");

    // The hold is gone: the machine's holds list is empty.
    let listed: Value = reqwest::Client::new()
        .get(format!("{base}/v1/hosts/gpu-box/holds"))
        .bearer_auth(OPERATOR)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["data"], json!([]), "{listed}");
    let _ = std::fs::remove_file(&marker);
}

// The guard has the last word: a busy machine stays on.
#[tokio::test]
async fn the_guard_keeps_a_busy_machine_on() {
    let marker = marker("guard");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"\n    poweroff_guard_command: \"/usr/bin/false\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;

    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["reason"], "guard_busy");
    assert!(!marker.exists(), "the machine was powered off while busy");
}

// A machine that has just booted is left alone: the operator's power-off
// waits for the minimum uptime.
#[tokio::test]
async fn a_machine_that_just_booted_is_left_alone() {
    let marker = marker("uptime");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"\n    poweroff_min_uptime_secs: 3600",
        marker.display()
    );
    let base = serve(&config(&lane)).await;

    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["reason"], "uptime_too_short");
    assert!(!marker.exists(), "the machine was powered off while young");
}

// A machine with no power-off command is always on: the gateway never
// powers it off, whoever asks.
#[tokio::test]
async fn a_machine_without_a_poweroff_command_is_never_powered_off() {
    let base = serve(&config("    wake_command: \"/usr/bin/true\"")).await;
    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["reason"], "no_poweroff_command");
}

// A machine with no wake command cannot be woken.
#[tokio::test]
async fn a_machine_without_a_wake_command_cannot_be_woken() {
    let base = serve(&config("    poweroff_command: \"/usr/bin/true\"")).await;
    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "on"})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["reason"], "no_wake_command");
}

// The wake runs in the background: the answer comes at once and the wake
// command runs after it.
#[tokio::test]
async fn on_wakes_the_machine() {
    let marker = marker("on");
    let lane = format!(
        "    wake_command: \"echo wake >> '{}'\"\n    poweroff_command: \"/usr/bin/true\"",
        marker.display()
    );
    let base = serve(&config(&lane)).await;

    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "on"})).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["host"], "gpu-box");
    assert_eq!(body["action"], "on");
    assert_eq!(body["status"], "waking");

    // The wake runs in the background.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the wake command never ran"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = std::fs::remove_file(&marker);
}

// An unknown action is a 400, and an unknown machine a 404.
#[tokio::test]
async fn a_bad_request_is_refused() {
    let base = serve(&config("    wake_command: \"/usr/bin/true\"")).await;
    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "toggle"})).await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = power(&base, OPERATOR, "no-such-box", json!({"action": "off"})).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["type"], "not_found");
}

// Work in flight on a lane of the machine keeps it on.
#[tokio::test]
async fn work_in_flight_keeps_the_machine_on() {
    let (lane_url, _lane) = spawn_lane().await;
    let marker = marker("busy");
    let lane = format!(
        "    wake_command: \"/usr/bin/true\"\n    poweroff_command: \"echo off >> '{}'\"",
        marker.display()
    );
    let config = format!(
        r#"
server:
  bind: "127.0.0.1:0"
api_keys:
  - {{ key: "{OPERATOR}", tenant: ops, role: operator }}
tenants:
  - id: ops
providers:
  - id: lane
    type: openai_compatible
    base_url: "{lane_url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
routes:
  - name: local-text
    match: {{ model: "local-text" }}
    targets:
      - "lane/qwen3"
local_executors:
  - name: lane
    host: gpu-box
    base_url: "{lane_url}"
    health_endpoint: "{lane_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 10
{lane}
"#
    );
    let base = serve(&config).await;

    // An open stream holds the lane.
    let mut response = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .bearer_auth(OPERATOR)
        .json(&json!({"model": "local-text", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut seen = String::new();
    while !seen.contains("lane-token") {
        let chunk = response.chunk().await.unwrap().expect("stream open");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }

    let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["reason"], "lane_busy");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("lane"),
        "the refusal names the busy lane: {body}"
    );
    assert!(!marker.exists(), "the machine was powered off under work");

    // The client goes away: the lane is free again and the machine goes.
    drop(response);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (status, body) = power(&base, OPERATOR, "gpu-box", json!({"action": "off"})).await;
        if status == 200 {
            assert_eq!(body["powered_off"], true);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the machine never went off: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_file(&marker);
}

// A lane that answers its health probe and streams tokens, so a request
// holds the lane while its stream is open.
#[derive(Clone)]
struct Lane;

async fn lane_health() -> StatusCode {
    StatusCode::OK
}

async fn lane_chat(Json(body): Json<Value>) -> Response {
    if body["stream"] != true {
        return Json(json!({
            "id": "chatcmpl-lane", "object": "chat.completion",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "served=lane"}}]
        }))
        .into_response();
    }
    let tokens = futures::stream::unfold(0u32, |n| async move {
        if n >= 600 {
            return None;
        }
        if n > 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let chunk = json!({
            "id": "chatcmpl-lane", "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": {"content": "lane-token "}, "finish_reason": null}]
        });
        Some((
            Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default().data(chunk.to_string()),
            ),
            n + 1,
        ))
    });
    axum::response::sse::Sse::new(tokens).into_response()
}

async fn spawn_lane() -> (String, Lane) {
    let app = Router::new()
        .route("/chat/completions", post(lane_chat))
        .route("/health", get(lane_health))
        .with_state(Lane);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), Lane)
}
