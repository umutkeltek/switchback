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

/// A host that is really off while its marker file is absent: its port
/// answers nothing (the front closes every connection at once, and the
/// ones it holds when the host goes off), as a powered-off machine gives
/// no answer at all. Counts the connections that reach it while off and
/// every health request its engine serves.
struct PoweredHost {
    marker: std::path::PathBuf,
    wakes_log: std::path::PathBuf,
    attempts_while_off: Arc<std::sync::atomic::AtomicUsize>,
    health_hits: Arc<std::sync::atomic::AtomicUsize>,
}

impl PoweredHost {
    fn wakes(&self) -> usize {
        std::fs::read_to_string(&self.wakes_log)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }
    fn attempts_while_off(&self) -> usize {
        self.attempts_while_off.load(Ordering::SeqCst)
    }
    fn health_hits(&self) -> usize {
        self.health_hits.load(Ordering::SeqCst)
    }
}

async fn spawn_powered_host(tag: &str) -> (String, PoweredHost) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let file = |what: &str| {
        std::env::temp_dir().join(format!("sb-{tag}-{}-{nanos}.{what}", std::process::id()))
    };
    let host = PoweredHost {
        marker: file("up"),
        wakes_log: file("wakes"),
        attempts_while_off: Arc::default(),
        health_hits: Arc::default(),
    };
    let engine = WakeableLane {
        marker: host.marker.clone(),
    };
    let hits = host.health_hits.clone();
    let app = Router::new()
        .route("/chat/completions", post(wakeable_chat))
        .route(
            "/health",
            get(move |state: State<WakeableLane>| {
                hits.fetch_add(1, Ordering::SeqCst);
                wakeable_health(state)
            }),
        )
        .with_state(engine);
    let back = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let back_addr = back.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(back, app).await.unwrap();
    });
    let front = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let front_addr = front.local_addr().unwrap();
    let (marker, attempts) = (host.marker.clone(), host.attempts_while_off.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut inbound, _)) = front.accept().await else {
                return;
            };
            if !marker.exists() {
                attempts.fetch_add(1, Ordering::SeqCst);
                drop(inbound);
                continue;
            }
            let marker = marker.clone();
            tokio::spawn(async move {
                let Ok(mut outbound) = tokio::net::TcpStream::connect(back_addr).await else {
                    return;
                };
                let goes_off = async {
                    while marker.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                };
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                    _ = goes_off => {}
                }
            });
        }
    });
    (format!("http://{front_addr}"), host)
}

/// The whole power cycle of a machine the gateway wakes, over HTTP: a
/// request wakes it, the idle power-off runs (its command takes the
/// machine down), one probe confirms it off, then supervisor passes and
/// routing leave it alone (no connection at all). The next request wakes
/// it again and is answered by the lane; a machine started by hand is
/// used by the next request without a wake.
#[tokio::test]
async fn a_machine_known_off_is_left_alone_until_a_request_wakes_it_again() {
    let (lane_url, host) = spawn_powered_host("lane-cycle").await;
    let cfg = format!(
        r#"{}
local_executors:
  - name: lane
    base_url: "{lane_url}"
    health_endpoint: "{lane_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 10
    wake_command: "echo wake >> '{wakes}'; touch '{marker}'"
    poweroff_command: "rm -f '{marker}'"
    idle_timeout_secs: 1
    boot_timeout_secs: 10
    health_poll_interval_ms: 20
"#,
        cfg_lane_then_spare(&lane_url, false),
        wakes = host.wakes_log.display(),
        marker = host.marker.display(),
    );
    let state = build_state(&cfg);
    let capacity = state.capacity.clone();
    let sb = serve(state).await;
    let hello = json!({"model":"local-text","messages":[{"role":"user","content":"hi"}]});
    let lane = capacity.executor("lane").unwrap();
    async fn answered_by(sb: &str, body: &Value) -> String {
        let response = chat(sb, body.clone()).await;
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
    // The idle power-off: wait out the 1 s idle timeout, then one pass.
    let idle = std::time::Duration::from_millis(1_200);

    // 1. Off at the start; a request wakes it and the lane answers.
    capacity.observe_all().await;
    assert_eq!(host.attempts_while_off(), 1, "the first look");
    assert_eq!(answered_by(&sb, &hello).await, "served=lane");
    assert_eq!(host.wakes(), 1);

    // 2. The idle power-off runs; its command takes the machine down.
    tokio::time::sleep(idle).await;
    capacity.tick_all().await;
    // The host's open connections close as it goes down.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!host.marker.exists(), "the power-off command ran");
    assert_eq!(
        serde_json::to_value(capacity.reports()[0].state).unwrap(),
        "offline"
    );

    // 3. One probe confirms it off.
    let attempts = host.attempts_while_off();
    capacity.observe_all().await;
    assert_eq!(
        host.attempts_while_off(),
        attempts + 1,
        "one confirming probe"
    );
    assert!(lane.machine_known_off());

    // 4. Supervisor passes and routing previews: no connection at all.
    let (attempts, hits) = (host.attempts_while_off(), host.health_hits());
    for _ in 0..5 {
        capacity.observe_all().await;
        capacity.tick_all().await;
        let preview = route_preview(&sb).await;
        assert_eq!(
            preview["decision"]["selected"]["target_id"], "spare/echo",
            "known down, and a preview never wakes: {preview}"
        );
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        host.attempts_while_off(),
        attempts,
        "no connection while known off"
    );
    assert_eq!(host.health_hits(), hits, "no health request");

    // 5. A non-streaming request wakes it again and the lane answers.
    assert_eq!(answered_by(&sb, &hello).await, "served=lane");
    assert_eq!(host.wakes(), 2, "the second wake");

    // 6. Off again; then someone starts it by hand: the next request is
    // answered by the lane with no new wake.
    tokio::time::sleep(idle).await;
    capacity.tick_all().await;
    // The host's open connections close as it goes down.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!host.marker.exists());
    capacity.observe_all().await;
    assert!(lane.machine_known_off());
    std::fs::write(&host.marker, b"").unwrap();
    assert_eq!(answered_by(&sb, &hello).await, "served=lane");
    assert_eq!(host.wakes(), 2, "no new wake");

    let _ = std::fs::remove_file(&host.marker);
    let _ = std::fs::remove_file(&host.wakes_log);
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

// --- One machine, one GPU: a text lane and a ComfyUI lane -----------------

fn tiny_png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

/// A ComfyUI stand-in: accepts a prompt, reports it complete, serves one
/// image, and answers its health endpoint.
async fn spawn_comfy() -> String {
    use axum::extract::Path;
    async fn prompt() -> Json<Value> {
        Json(json!({"prompt_id": "prompt-1", "number": 1, "node_errors": {}}))
    }
    async fn history(Path(prompt_id): Path<String>) -> Json<Value> {
        Json(json!({ prompt_id: {
            "status": {"completed": true},
            "outputs": {"9": {"images": [{"filename": "out.png", "subfolder": "", "type": "output"}]}}
        }}))
    }
    async fn view() -> Response {
        ([("content-type", "image/png")], tiny_png()).into_response()
    }
    async fn stats() -> Json<Value> {
        Json(json!({"system": {"os": "test"}, "devices": []}))
    }
    let app = Router::new()
        .route("/prompt", post(prompt))
        .route("/history/{prompt_id}", get(history))
        .route("/view", get(view))
        .route("/system_stats", get(stats));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn lines_in(path: &std::path::Path, line: &str) -> usize {
    std::fs::read_to_string(path)
        .map(|text| text.lines().filter(|l| *l == line).count())
        .unwrap_or(0)
}

/// One host with a text lane and a ComfyUI lane sharing GPU group `gpu0`.
/// A media job that arrives while a text stream is open waits for it; when
/// the client goes away the text lane's release command runs once and the
/// job is served, its decision saying so. The next text request then takes
/// the GPU back from ComfyUI with one release command.
#[tokio::test]
async fn a_text_lane_and_a_comfy_lane_take_turns_on_one_gpu() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir();
    let up = dir.join(format!("sb-gpu-up-{}-{nanos}.marker", std::process::id()));
    let released = dir.join(format!("sb-gpu-release-{}-{nanos}.log", std::process::id()));
    std::fs::write(&up, "up\n").unwrap();
    let (lane_url, _lane) = spawn_wakeable_lane(up.clone()).await;
    let comfy_url = spawn_comfy().await;
    let released_path = released.display();
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
  - id: comfy
    type: comfyui
    base_url: "{comfy_url}"
    workflows:
      - id: txt2img
        kind: image_generation
        version: test
        graph:
          "3": {{ class_type: EmptyLatentImage, inputs: {{ width: 1, height: 1, batch_size: 1 }} }}
          "4": {{ class_type: KSampler, inputs: {{ seed: 0 }} }}
          "6": {{ class_type: CLIPTextEncode, inputs: {{ text: "" }} }}
          "9": {{ class_type: SaveImage, inputs: {{ filename_prefix: "switchback" }} }}
        bindings:
          prompt: {{ path: ["6", "inputs", "text"] }}
          seed: {{ path: ["4", "inputs", "seed"] }}
          width: {{ path: ["3", "inputs", "width"] }}
          height: {{ path: ["3", "inputs", "height"] }}
        output_node_ids: ["9"]
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
    wake_command: "true"
    exclusive_group: gpu0
    release_command: "echo release-text >> '{released_path}'"
    group_wait_secs: 20
  - name: comfy
    host: gpu-box
    base_url: "{comfy_url}"
    health_endpoint: "{comfy_url}/system_stats"
    exclusive_group: gpu0
    release_command: "echo release-comfy >> '{released_path}'"
    group_wait_secs: 20
"#
    );
    let parsed = sb_core::Config::from_yaml(&cfg).unwrap();
    sb_runtime::Engine::validate_config(&parsed).expect("the example shape is valid");
    let state = build_state(&cfg);
    let capacity = state.capacity.clone();
    let traces = state.traces.clone();
    let sb = serve(state).await;
    let client = reqwest::Client::new();

    // A long text stream takes the GPU (nobody held it yet: ComfyUI is
    // released once).
    let mut stream = client
        .post(format!("{sb}/v1/chat/completions"))
        .json(&json!({"model":"local-text","stream":true,
                      "messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let mut seen = String::new();
    while !seen.contains("lane-token") {
        let chunk = stream.chunk().await.unwrap().expect("stream open");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert_eq!(lines_in(&released, "release-comfy"), 1);

    // A media job arrives while the stream is open: it waits.
    let job = tokio::spawn({
        let client = client.clone();
        let sb = sb.clone();
        async move {
            client
                .post(format!("{sb}/v1/images/generations"))
                .json(&json!({"model":"comfy/txt2img","prompt":"a lighthouse",
                              "size":"768x512","n":1,"response_format":"url"}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(!job.is_finished(), "the job waits for the open stream");
    assert_eq!(lines_in(&released, "release-text"), 0);
    let report: Value = reqwest::get(format!("{sb}/v1/workloads/capacity"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lanes = report["data"].as_array().unwrap();
    assert!(
        lanes
            .iter()
            .all(|lane| lane["host"] == "gpu-box" && lane["gpu_group"] == "gpu0"),
        "{report}"
    );

    drop(stream); // the client goes away: the last stream ends
    let created = tokio::time::timeout(std::time::Duration::from_secs(10), job)
        .await
        .expect("the job is served once the stream ends")
        .unwrap();
    assert_eq!(created["job"]["status"], "succeeded", "{created}");
    let job_id = created["job"]["id"].as_str().unwrap();
    let record: Value = reqwest::get(format!("{sb}/v1/jobs/{job_id}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reasons = record["route_decision"]["reason"].to_string();
    assert!(
        reasons.contains("local lane `comfy` waited")
            && reasons.contains("took it from lane `lane`"),
        "{reasons}"
    );
    assert_eq!(
        lines_in(&released, "release-text"),
        1,
        "one release for the switch"
    );

    // The next text request takes the GPU back with one release command.
    let response = chat(
        &sb,
        json!({"model":"local-text","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(lines_in(&released, "release-comfy"), 2);
    let trace = traces.recent(1).pop().expect("trace recorded");
    assert!(
        trace
            .decision
            .reason
            .iter()
            .any(|r| r.starts_with("local lane `lane` waited")
                && r.ends_with("took it from lane `comfy`")),
        "{:?}",
        trace.decision.reason
    );
    let text = capacity.executor("lane").unwrap().report();
    assert_eq!(text.holds_gpu, Some(true));
    let _ = std::fs::remove_file(&up);
    let _ = std::fs::remove_file(&released);
}

/// After a gateway restart while the GPU machine is off, nobody is known to
/// hold the machine's GPU group. A text request must wake the machine, not
/// first run the other lane's release command against a machine that is
/// off (which fails like a remote shell to an unreachable host, exit 255,
/// and used to answer a 503 every retry, without ever waking it).
async fn restart_with_gpu_machine_off(
    observe_first: bool,
) -> (reqwest::StatusCode, String, usize, usize) {
    let (url, host) = spawn_powered_host(if observe_first {
        "gpu-off-seen"
    } else {
        "gpu-off"
    })
    .await;
    let released = host.marker.with_extension("released");
    let marker = host.marker.display();
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: text
    type: openai_compatible
    base_url: "{url}"
    engine: llama_cpp
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
  - id: media
    type: openai_compatible
    base_url: "{url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
routes:
  - name: large
    match: {{ model: "large" }}
    pin_order: true
    targets:
      - "text/qwen3"
local_executors:
  - name: text
    host: gpu-box
    base_url: "{url}"
    health_endpoint: "{url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 10
    adopt_external_wake: true
    wake_command: "echo wake >> '{wakes}'; touch '{marker}'"
    boot_timeout_secs: 10
    health_poll_interval_ms: 20
    exclusive_group: gpu0
    release_command: "test -e '{marker}' || exit 255; echo release-text >> '{released}'"
    group_wait_secs: 5
  - name: media
    host: gpu-box
    base_url: "{url}"
    health_endpoint: "{url}/health"
    exclusive_group: gpu0
    release_command: "test -e '{marker}' || exit 255; echo release-media >> '{released}'"
    group_wait_secs: 5
"#,
        wakes = host.wakes_log.display(),
        released = released.display(),
    );
    let state = build_state(&cfg);
    let capacity = state.capacity.clone();
    let sb = serve(state).await;
    if observe_first {
        // A supervisor pass ran before the first request.
        capacity.observe_all().await;
    }
    let response = chat(
        &sb,
        json!({"model":"large","messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let releases = lines_in(&released, "release-media");
    let wakes = host.wakes();
    let _ = std::fs::remove_file(&host.marker);
    let _ = std::fs::remove_file(&host.wakes_log);
    let _ = std::fs::remove_file(&released);
    (status, body, wakes, releases)
}

#[tokio::test]
async fn after_a_restart_with_the_gpu_machine_off_a_text_request_wakes_it() {
    let (status, body, wakes, releases) = restart_with_gpu_machine_off(false).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("served=lane"), "{body}");
    assert_eq!(wakes, 1, "one wake");
    assert!(releases <= 1, "at most one release, on the woken machine");
}

#[tokio::test]
async fn after_a_restart_and_a_supervisor_pass_with_the_gpu_machine_off_a_text_request_wakes_it() {
    let (status, body, wakes, releases) = restart_with_gpu_machine_off(true).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("served=lane"), "{body}");
    assert_eq!(wakes, 1, "one wake");
    assert!(releases <= 1, "at most one release, on the woken machine");
}
