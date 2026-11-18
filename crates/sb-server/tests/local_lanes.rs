//! Local text lanes end-to-end: one request shape for every local engine
//! (reasoning control translated per engine at the adapter edge) and routes
//! over local lanes whose declared order can be pinned against outcome
//! demotion. The "lane" upstream is a loopback stub that can be switched
//! between down and up, standing in for an opt-in local engine.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
struct Lane {
    up: Arc<AtomicBool>,
    last_body: Arc<Mutex<Option<Value>>>,
}

/// While "down" the stub answers 429: a scoreable target failure whose
/// account cooldown is a short backoff (2s), so the test can watch the pool
/// recover without a 30s sleep — the same trade-off `outcome_scorecard.rs`
/// makes with its `always-error` mock.
async fn lane_chat(State(lane): State<Lane>, Json(body): Json<Value>) -> Response {
    if let Ok(mut last) = lane.last_body.lock() {
        *last = Some(body);
    }
    if !lane.up.load(Ordering::SeqCst) {
        return (StatusCode::TOO_MANY_REQUESTS, "lane down").into_response();
    }
    Json(json!({
        "id": "chatcmpl-lane", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop",
                     "message":{"role":"assistant","content":"served=lane"}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
    .into_response()
}

/// The engine's health endpoint: 200 while up, 503 while down.
async fn lane_health(State(lane): State<Lane>) -> StatusCode {
    if lane.up.load(Ordering::SeqCst) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn spawn_lane(up: bool) -> (String, Lane) {
    let lane = Lane {
        up: Arc::new(AtomicBool::new(up)),
        last_body: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route("/chat/completions", post(lane_chat))
        .route("/health", get(lane_health))
        .with_state(lane.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), lane)
}

fn build_state(cfg_yaml: &str) -> sb_server::AppState {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
}

async fn spawn_switchback(cfg_yaml: &str) -> String {
    serve(build_state(cfg_yaml)).await
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

/// A route over two engines for the same model: the llama.cpp lane declared
/// first (three accounts, so one request records three failures — exactly
/// `fast_demote_streak`), a mock spare second.
fn cfg_lane_then_spare(lane_url: &str, pin_order: bool) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
  scorecard:
    demotion:
      min_samples: 8
      fast_demote_streak: 3
      fast_recover_streak: 3
providers:
  - id: lane
    type: openai_compatible
    base_url: "{lane_url}"
    engine: llama_cpp
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
      - {{ id: a2, auth: {{ kind: none }} }}
      - {{ id: a3, auth: {{ kind: none }} }}
  - id: spare
    type: mock
routes:
  - name: local-text
    match: {{ model: "local-text" }}
    pin_order: {pin_order}
    targets:
      - "lane/qwen3"
      - "spare/echo"
"#
    )
}

async fn chat(base: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn route_preview(base: &str) -> Value {
    route_preview_for(base, "local-text").await
}

async fn route_preview_for(base: &str, model: &str) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/cp/v1/route-preview"))
        .json(&json!({"model":model,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn reason_starting<'a>(preview: &'a Value, prefix: &str) -> Option<&'a str> {
    preview["decision"]["reason"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|line| line.starts_with(prefix))
}

/// Run the "lane down, then up" story and return the route preview taken
/// after the lane is back, plus the content the next real request got.
async fn lane_down_then_up(pin_order: bool) -> (Value, String) {
    let (lane_url, lane) = spawn_lane(false).await;
    let sb = spawn_switchback(&cfg_lane_then_spare(&lane_url, pin_order)).await;
    let hello = json!({"model":"local-text","messages":[{"role":"user","content":"hi"}]});

    // Lane down: all three accounts fail, the spare serves the request.
    let r = chat(&sb, hello.clone()).await;
    assert_eq!(r.status(), 200, "fallover to the spare must succeed");

    // The lane is started; its short account backoff lapses.
    lane.up.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;

    let preview = route_preview(&sb).await;
    let body: Value = chat(&sb, hello).await.json().await.unwrap();
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    (preview, content)
}

#[tokio::test]
async fn pinned_route_selects_the_lane_first_once_it_is_back_and_says_why() {
    let (preview, content) = lane_down_then_up(true).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "lane/qwen3",
        "pinned order: the declared lane is first again: {preview}"
    );
    assert_eq!(preview["decision"]["strategy"], "pinned_order");
    assert!(
        reason_starting(&preview, "order=pinned").is_some(),
        "{preview}"
    );
    let cited = reason_starting(&preview, "outcome/pinned target=lane/qwen3")
        .unwrap_or_else(|| panic!("the ignored demotion must be cited: {preview}"));
    assert!(cited.contains("reason=streak"), "{cited}");
    assert!(
        reason_starting(&preview, "outcome/demote").is_none(),
        "{preview}"
    );
    assert_eq!(content, "served=lane", "the real request hits the lane");
}

#[tokio::test]
async fn unpinned_route_keeps_todays_outcome_demotion() {
    let (preview, content) = lane_down_then_up(false).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "spare/echo",
        "today's behaviour: the scorecard keeps the lane demoted: {preview}"
    );
    let demote = reason_starting(&preview, "outcome/demote target=lane/qwen3")
        .unwrap_or_else(|| panic!("no outcome/demote line: {preview}"));
    assert!(demote.contains("reason=streak"), "{demote}");
    assert!(content.contains("echo:"), "served by the spare: {content}");
}

#[tokio::test]
async fn reasoning_effort_none_reaches_llama_cpp_as_thinking_off() {
    let (lane_url, lane) = spawn_lane(true).await;
    let sb = spawn_switchback(&cfg_lane_then_spare(&lane_url, true)).await;

    let r = chat(
        &sb,
        json!({
            "model": "local-text",
            "reasoning_effort": "none",
            "response_format": {"type": "json_object"},
            "messages": [{"role":"user","content":"hi"}]
        }),
    )
    .await;
    assert_eq!(r.status(), 200);

    let sent = lane.last_body.lock().unwrap().clone().expect("lane called");
    assert_eq!(
        sent["chat_template_kwargs"]["enable_thinking"], false,
        "{sent}"
    );
    assert!(sent.get("reasoning_effort").is_none(), "{sent}");
    assert_eq!(sent["response_format"]["type"], "json_object", "{sent}");
}

/// The same route, unpinned, with the lane declared as a managed local lane
/// (`local_executors`). With `route_health` the activator probes its health
/// endpoint for routing; without it (the default) routing never reads it.
fn cfg_managed_lane(lane_url: &str, route_health: bool) -> String {
    format!(
        r#"{}
local_executors:
  - name: lane
    base_url: "{lane_url}"
    health_endpoint: "{lane_url}/health"
    route_health: {route_health}
"#,
        cfg_lane_then_spare(lane_url, false)
    )
}

#[tokio::test]
async fn a_managed_lane_without_route_health_is_dispatched_and_counted_as_before() {
    // Opt-in: without `route_health` a managed lane is routed exactly as
    // before managed text lanes, even after a probe of it failed.
    let (lane_url, lane) = spawn_lane(false).await;
    let state = build_state(&cfg_managed_lane(&lane_url, false));
    let capacity = state.capacity.clone();
    let sb = serve(state).await;
    let hello = json!({"model":"local-text","messages":[{"role":"user","content":"hi"}]});

    // A supervisor pass sends it nothing; a direct probe does fail.
    capacity.observe_all().await;
    capacity.executor("lane").unwrap().observe().await;
    let preview = route_preview(&sb).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "lane/qwen3",
        "{preview}"
    );
    assert!(
        !preview.to_string().contains("local lane"),
        "never skipped: {preview}"
    );

    // Real traffic reaches the lane, fails over to the spare, and the
    // lane's failures are counted: the scorecard demotes it.
    let r = chat(&sb, hello).await;
    assert_eq!(r.status(), 200);
    assert!(
        lane.last_body.lock().unwrap().is_some(),
        "the lane was dispatched to"
    );
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    let preview = route_preview(&sb).await;
    let demote = reason_starting(&preview, "outcome/demote target=lane/qwen3")
        .unwrap_or_else(|| panic!("the lane's failures must count: {preview}"));
    assert!(demote.contains("reason=streak"), "{demote}");

    let report: Value = reqwest::get(format!("{sb}/v1/workloads/capacity"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        report["data"][0].get("observed_healthy").is_none(),
        "{report}"
    );
}

#[tokio::test]
async fn a_managed_lane_known_down_is_skipped_and_not_counted_as_a_failure() {
    let (lane_url, lane) = spawn_lane(false).await;
    let state = build_state(&cfg_managed_lane(&lane_url, true));
    let capacity = state.capacity.clone();
    let sb = serve(state).await;
    let hello = json!({"model":"local-text","messages":[{"role":"user","content":"hi"}]});

    // Not probed yet: unknown is not down, so routing is unchanged.
    let preview = route_preview(&sb).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "lane/qwen3",
        "{preview}"
    );

    // The supervisor's probe sees the lane down (stopped on purpose).
    capacity.observe_all().await;
    let preview = route_preview(&sb).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "spare/echo",
        "{preview}"
    );
    let rejected = preview["decision"]["rejected"].as_array().unwrap();
    let reason = rejected
        .iter()
        .find(|r| r["target_id"] == "lane/qwen3")
        .and_then(|r| r["reason"].as_str())
        .unwrap_or_else(|| panic!("lane must be rejected with a reason: {preview}"));
    assert!(reason.contains("local lane `lane` is down"), "{reason}");
    assert!(reason.contains("not counted as a failure"), "{reason}");

    // Real traffic while the lane is down never reaches it.
    for _ in 0..4 {
        let r = chat(&sb, hello.clone()).await;
        assert_eq!(r.status(), 200);
    }
    assert!(
        lane.last_body.lock().unwrap().is_none(),
        "a lane known down is never dispatched to"
    );
    let report: Value = reqwest::get(format!("{sb}/v1/workloads/capacity"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["data"][0]["observed_healthy"], false, "{report}");

    // The lane is started; the next probe sees it. Nothing was counted
    // against it, so it is first again WITHOUT pin_order, and no demotion.
    lane.up.store(true, Ordering::SeqCst);
    capacity.observe_all().await;
    let preview = route_preview(&sb).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "lane/qwen3",
        "{preview}"
    );
    assert!(
        reason_starting(&preview, "outcome/demote").is_none(),
        "{preview}"
    );
    let body: Value = chat(&sb, hello).await.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=lane");
}

#[tokio::test]
async fn a_lane_known_down_is_kept_when_it_is_the_only_candidate() {
    // Fail-open: a stale probe must never turn into a hard outage, so a
    // request with no other candidate still reaches the lane.
    let (lane_url, _lane) = spawn_lane(false).await;
    let state = build_state(&cfg_managed_lane(&lane_url, true));
    let capacity = state.capacity.clone();
    let sb = serve(state).await;
    capacity.observe_all().await;

    let preview = route_preview_for(&sb, "lane/qwen3").await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "lane/qwen3",
        "{preview}"
    );
    assert!(
        preview["decision"]["rejected"]
            .as_array()
            .is_none_or(|rejected| rejected.is_empty()),
        "{preview}"
    );
}

#[tokio::test]
async fn a_lane_that_comes_back_is_used_again_without_waiting_for_the_supervisor() {
    // Stale window: the last probe said down, then the engine came up. The
    // request that skips the lane starts a background re-probe (the request
    // itself never waits on it), so the next requests reach the lane again
    // well before the next 10s supervisor probe.
    let (lane_url, lane) = spawn_lane(false).await;
    let state = build_state(&cfg_managed_lane(&lane_url, true));
    let capacity = state.capacity.clone();
    let sb = serve(state).await;
    let hello = json!({"model":"local-text","messages":[{"role":"user","content":"hi"}]});
    capacity.observe_all().await;
    lane.up.store(true, Ordering::SeqCst);
    // Past the minimum gap between probes.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    let body: Value = chat(&sb, hello.clone()).await.json().await.unwrap();
    let first = body["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        first.contains("echo:"),
        "stale evidence: the spare serves: {first}"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let preview = route_preview(&sb).await;
        if preview["decision"]["selected"]["target_id"] == "lane/qwen3" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the lane was not re-probed: {preview}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let body: Value = chat(&sb, hello).await.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=lane");
}

/// An engine on a powered-off host: it answers only once a wake command has
/// run (the command appends a line to a marker file, so the test can also
/// count the wakes), and a streamed answer keeps sending a token every 50 ms
/// until the client goes away.
#[derive(Clone)]
struct WakeableLane {
    marker: std::path::PathBuf,
}

impl WakeableLane {
    fn up(&self) -> bool {
        self.marker.exists()
    }
    fn wakes(&self) -> usize {
        std::fs::read_to_string(&self.marker)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }
}

async fn wakeable_health(State(lane): State<WakeableLane>) -> StatusCode {
    if lane.up() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn wakeable_chat(State(lane): State<WakeableLane>, Json(body): Json<Value>) -> Response {
    if !lane.up() {
        return (StatusCode::TOO_MANY_REQUESTS, "host off").into_response();
    }
    if body["stream"] != true {
        return Json(json!({
            "id": "chatcmpl-lane", "object": "chat.completion",
            "choices": [{"index":0,"finish_reason":"stop",
                         "message":{"role":"assistant","content":"served=lane"}}]
        }))
        .into_response();
    }
    let tokens = futures::stream::unfold(0u32, |n| async move {
        if n >= 600 {
            return None;
        }
        if n > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let chunk = json!({
            "id": "chatcmpl-lane", "object": "chat.completion.chunk",
            "choices": [{"index":0,"delta":{"content":"lane-token "},"finish_reason":null}]
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

async fn spawn_wakeable_lane(marker: std::path::PathBuf) -> (String, WakeableLane) {
    let lane = WakeableLane { marker };
    let app = Router::new()
        .route("/chat/completions", post(wakeable_chat))
        .route("/health", get(wakeable_health))
        .with_state(lane.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), lane)
}

#[tokio::test]
async fn a_request_wakes_a_down_lane_streams_from_it_and_a_disconnect_frees_it() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let marker = std::env::temp_dir().join(format!(
        "sb-lane-wake-{}-{nanos}.marker",
        std::process::id()
    ));
    let (lane_url, lane) = spawn_wakeable_lane(marker.clone()).await;
    let cfg = format!(
        r#"{}
local_executors:
  - name: lane
    base_url: "{lane_url}"
    health_endpoint: "{lane_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 10
    wake_command: "echo wake >> '{}'"
    boot_timeout_secs: 10
    health_poll_interval_ms: 20
"#,
        cfg_lane_then_spare(&lane_url, false),
        marker.display()
    );
    let state = build_state(&cfg);
    let capacity = state.capacity.clone();
    let traces = state.traces.clone();
    let sb = serve(state).await;
    capacity.observe_all().await; // the supervisor saw the host off
    let preview = route_preview(&sb).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "spare/echo",
        "known down, and a preview never wakes: {preview}"
    );
    assert_eq!(lane.wakes(), 0);

    // The request finds its first choice down: it wakes the host, waits for
    // health, then streams from the lane rather than the spare.
    let mut response = reqwest::Client::new()
        .post(format!("{sb}/v1/chat/completions"))
        .json(&json!({"model":"local-text","stream":true,
                      "messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut seen = String::new();
    while !seen.contains("lane-token") {
        let chunk = response.chunk().await.unwrap().expect("stream open");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert_eq!(lane.wakes(), 1, "one wake");
    let report = &capacity.reports()[0];
    assert_eq!(report.in_flight, 1, "the open stream holds the lane");
    assert_eq!(
        serde_json::to_value(report.state).unwrap(),
        "draining",
        "{report:?}"
    );

    // The client goes away mid-stream: the lane is released and its idle
    // countdown starts.
    drop(response);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while capacity.reports()[0].in_flight != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a client disconnect must release the lane"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        serde_json::to_value(capacity.reports()[0].state).unwrap(),
        "idle_countdown"
    );

    let trace = traces.recent(1).pop().expect("trace recorded");
    assert_eq!(trace.final_status, 499, "client aborted");
    assert_eq!(
        trace.decision.selected.as_ref().unwrap().target_id,
        "lane/qwen3"
    );
    assert!(
        trace
            .decision
            .reason
            .iter()
            .any(|r| r.starts_with("local lane `lane` woken for this request, healthy after")),
        "{:?}",
        trace.decision.reason
    );
    assert!(trace.ttft_ms.is_some(), "a token was produced");
    let _ = std::fs::remove_file(&marker);
}

/// A route whose only target is a `wake_on_request` lane that cannot come
/// up (its host is off and its wake command fails) is refused before
/// the first byte with a retryable 503 in the repo's error shape, on every
/// ingress, instead of being dispatched into the host it just probed down.
#[tokio::test]
async fn a_lane_that_cannot_wake_and_is_the_only_target_answers_a_retryable_503() {
    let (lane_url, lane) = spawn_lane(false).await;
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: lane
    type: openai_compatible
    base_url: "{lane_url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
routes:
  - name: local-only
    match: {{ model: "local-only" }}
    targets:
      - "lane/qwen3"
local_executors:
  - name: lane
    base_url: "{lane_url}"
    health_endpoint: "{lane_url}/health"
    route_health: true
    wake_on_request: true
    wake_command: "exit 3"
    wake_retry_interval_secs: 20
"#
    );
    let state = build_state(&cfg);
    let capacity = state.capacity.clone();
    let traces = state.traces.clone();
    let sb = serve(state).await;
    capacity.observe_all().await; // the supervisor saw the host off

    let client = reqwest::Client::new();
    let requests = [
        (
            "/v1/chat/completions",
            json!({"model":"local-only","messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"local-only","stream":true,
                   "messages":[{"role":"user","content":"hi"}]}),
        ),
        (
            "/v1/messages",
            json!({"model":"local-only","max_tokens":16,
                   "messages":[{"role":"user","content":"hi"}]}),
        ),
    ];
    for (path, body) in requests {
        let response = client
            .post(format!("{sb}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503, "{path} {body}");
        let retry_after: u64 = response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(retry_after, 20, "the lane's wake re-send gap");
        assert!(response.headers().contains_key("x-switchback-route"));
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"]["type"], "local_lane_unavailable", "{error}");
        let message = error["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("wake command failed: exit 3"),
            "says why: {message}"
        );
    }
    assert!(
        lane.last_body.lock().unwrap().is_none(),
        "never dispatched into the lane"
    );
    let trace = traces.recent(1).pop().expect("trace recorded");
    assert_eq!(trace.final_status, 503);
    assert!(
        trace
            .decision
            .reason
            .iter()
            .any(|r| r.starts_with("local_lane_unavailable: local lane `lane` is not ready")),
        "{:?}",
        trace.decision.reason
    );
}

#[tokio::test]
async fn a_lane_added_by_a_reload_is_used_without_a_restart() {
    let (lane_url, _lane) = spawn_lane(false).await;
    let state = build_state(&cfg_lane_then_spare(&lane_url, false));
    let capacity = state.capacity.clone();
    let engine = state.engine.clone();
    let sb = serve(state).await;
    assert!(capacity.executor("lane").is_none());

    let with_lane = sb_core::Config::from_yaml(&cfg_managed_lane(&lane_url, true)).unwrap();
    engine.reload(with_lane).unwrap();
    assert!(
        capacity.executor("lane").is_some(),
        "the reload added the lane"
    );
    capacity.observe_all().await;
    let preview = route_preview(&sb).await;
    assert_eq!(
        preview["decision"]["selected"]["target_id"], "spare/echo",
        "routing reads the added lane: {preview}"
    );
    let report: Value = reqwest::get(format!("{sb}/v1/workloads/capacity"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(report["data"][0]["name"], "lane", "{report}");
}
