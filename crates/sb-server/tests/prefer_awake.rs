//! Wake-aware routing ("prefer awake") end to end. One route lists a target
//! on a GPU machine that sleeps when idle (`gpu-box`: a wake command brings
//! it up) and one on an always-on machine (`small-host`: no wake, its lane
//! closes when a health guard takes the memory). A second sleeping machine
//! (`gpu-two`) serves the route with two sleepers. Every engine is a
//! loopback stub; each wake command appends a line to a marker file, so a
//! test can count the wakes it caused.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// A sleeping GPU machine: up only while its `.up` file exists. A wake
/// appends one line to `marker` (the wake count) and creates it.
#[derive(Clone)]
struct GpuBox {
    name: &'static str,
    marker: std::path::PathBuf,
    served: Arc<AtomicUsize>,
    /// Every chat request that reached the engine, up or not.
    hits: Arc<AtomicUsize>,
}

impl GpuBox {
    fn new(name: &'static str, marker: std::path::PathBuf) -> Self {
        Self {
            name,
            marker,
            served: Arc::new(AtomicUsize::new(0)),
            hits: Arc::new(AtomicUsize::new(0)),
        }
    }
    fn up_file(&self) -> std::path::PathBuf {
        self.marker.with_extension("up")
    }
    fn up(&self) -> bool {
        self.up_file().exists()
    }
    fn wakes(&self) -> usize {
        std::fs::read_to_string(&self.marker)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }
    /// Power the machine off again; the wake count stays.
    fn sleep(&self) {
        let _ = std::fs::remove_file(self.up_file());
    }
}

async fn gpu_health(State(gpu): State<GpuBox>) -> StatusCode {
    if gpu.up() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn gpu_chat(State(gpu): State<GpuBox>) -> Response {
    gpu.hits.fetch_add(1, Ordering::SeqCst);
    if !gpu.up() {
        return (StatusCode::SERVICE_UNAVAILABLE, "host off").into_response();
    }
    gpu.served.fetch_add(1, Ordering::SeqCst);
    completion(&format!("served={}", gpu.name))
}

async fn gpu_embeddings(State(gpu): State<GpuBox>) -> Response {
    gpu.hits.fetch_add(1, Ordering::SeqCst);
    if !gpu.up() {
        return (StatusCode::SERVICE_UNAVAILABLE, "host off").into_response();
    }
    gpu.served.fetch_add(1, Ordering::SeqCst);
    embedding(&format!("served={}", gpu.name))
}

/// The always-on machine: `down` true = lane closed (the health guard
/// took the memory); `fail` true = the engine answers 500; `slow_ms` > 0 =
/// each answer takes that long.
#[derive(Clone, Default)]
struct SmallHost {
    down: Arc<AtomicBool>,
    fail: Arc<AtomicBool>,
    served: Arc<AtomicUsize>,
    slow_ms: Arc<AtomicU64>,
}

async fn small_health(State(small): State<SmallHost>) -> StatusCode {
    if small.down.load(Ordering::SeqCst) {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

async fn small_chat(State(small): State<SmallHost>) -> Response {
    small.served.fetch_add(1, Ordering::SeqCst);
    let slow = small.slow_ms.load(Ordering::SeqCst);
    if slow > 0 {
        tokio::time::sleep(Duration::from_millis(slow)).await;
    }
    if small.fail.load(Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "engine error").into_response();
    }
    completion("served=small-host")
}

async fn small_embeddings(State(small): State<SmallHost>) -> Response {
    small.served.fetch_add(1, Ordering::SeqCst);
    embedding("served=small-host")
}

fn completion(content: &str) -> Response {
    Json(json!({
        "id": "chatcmpl-stub", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop",
            "message":{"role":"assistant","content":content}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
    .into_response()
}

/// An embeddings answer; `model` says which machine served it.
fn embedding(served: &str) -> Response {
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
        "model": served,
        "usage": {"prompt_tokens": 1, "total_tokens": 1}
    }))
    .into_response()
}

async fn listen(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn gpu_stub(gpu: &GpuBox) -> String {
    listen(
        Router::new()
            .route("/chat/completions", post(gpu_chat))
            .route("/embeddings", post(gpu_embeddings))
            .route("/health", get(gpu_health))
            .with_state(gpu.clone()),
    )
    .await
}

struct Fixture {
    sb: String,
    gpu: GpuBox,
    gpu_two: GpuBox,
    small: SmallHost,
    state: sb_server::AppState,
    traces: Arc<sb_trace::TraceLog>,
}

/// What a test changes in the fixture's config.
#[derive(Default)]
struct Opts {
    /// `None` = `prefer_awake` not configured; `Some(enabled)` = on every
    /// route.
    prefer_awake: Option<bool>,
    /// Lines under `server:` (two-space indent).
    server: &'static str,
    /// Lines at the top level (`api_keys:`, `tenants:`).
    top: &'static str,
    /// Lines added to the small host's lane (four-space indent).
    small_lane: &'static str,
    /// Lines added to the GPU box's lane (four-space indent).
    gpu_lane: &'static str,
    /// Shell run before each wake's marker line, e.g. `sleep 0.3 && `.
    wake_delay: &'static str,
}

/// `prefer_awake`: `None` = not configured; `Some(enabled)` = on the route.
async fn fixture(prefer_awake: Option<bool>) -> Fixture {
    fixture_with(Opts {
        prefer_awake,
        ..Opts::default()
    })
    .await
}

fn marker_file(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "sb-prefer-awake-{name}-{}-{nanos}-{}.wakes",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ))
}

async fn fixture_with(opts: Opts) -> Fixture {
    let gpu = GpuBox::new("gpu-box", marker_file("gpu-box"));
    let gpu_url = gpu_stub(&gpu).await;
    let gpu_two = GpuBox::new("gpu-two", marker_file("gpu-two"));
    let gpu_two_url = gpu_stub(&gpu_two).await;
    let small = SmallHost::default();
    let small_url = listen(
        Router::new()
            .route("/chat/completions", post(small_chat))
            .route("/embeddings", post(small_embeddings))
            .route("/health", get(small_health))
            .with_state(small.clone()),
    )
    .await;
    let prefer = match opts.prefer_awake {
        None => String::new(),
        Some(enabled) => format!(
            "    prefer_awake: {{ enabled: {enabled}, wake_for_prompt_tokens: 1000, wake_for_output_tokens: 500 }}\n"
        ),
    };
    let wake = |gpu: &GpuBox| {
        format!(
            "{}echo wake >> '{}' && touch '{}'",
            opts.wake_delay,
            gpu.marker.display(),
            gpu.up_file().display()
        )
    };
    let route = |name: &str, targets: &[&str]| {
        let mut route =
            format!("  - name: {name}\n    match: {{ model: \"{name}\" }}\n{prefer}    targets:\n");
        for target in targets {
            route.push_str(&format!("      - \"{target}\"\n"));
        }
        route
    };
    let routes = [
        route("chat", &["gpu-box/big", "small-host/small"]),
        route("gpu-only", &["gpu-box/big"]),
        route("small-only", &["small-host/small"]),
        route("two-sleepers", &["gpu-box/big", "gpu-two/big"]),
        route("embed", &["gpu-box/emb", "small-host/emb"]),
        route("embed-gpu-only", &["gpu-box/emb"]),
    ]
    .concat();
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  retry:
    max_retries: 0
{server}
{top}
providers:
  - id: gpu-box
    type: openai_compatible
    base_url: "{gpu_url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
  - id: gpu-two
    type: openai_compatible
    base_url: "{gpu_two_url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
  - id: small-host
    type: openai_compatible
    base_url: "{small_url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
routes:
{routes}
local_executors:
  - name: gpu-box
    base_url: "{gpu_url}"
    health_endpoint: "{gpu_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 10
    wake_command: "{wake_gpu}"
    boot_timeout_secs: 10
    health_poll_interval_ms: 20
{gpu_lane}
  - name: gpu-two
    base_url: "{gpu_two_url}"
    health_endpoint: "{gpu_two_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 10
    wake_command: "{wake_two}"
    boot_timeout_secs: 10
    health_poll_interval_ms: 20
  - name: small-host
    base_url: "{small_url}"
    health_endpoint: "{small_url}/health"
    route_health: true
{small_lane}
"#,
        server = opts.server,
        top = opts.top,
        gpu_lane = opts.gpu_lane,
        small_lane = opts.small_lane,
        wake_gpu = wake(&gpu),
        wake_two = wake(&gpu_two),
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
    let traces = state.traces.clone();
    let sb = listen(sb_server::build_app(state.clone())).await;
    // The supervisor's first look: the GPU machines are off, the small one up.
    state.capacity.observe_all().await;
    Fixture {
        sb,
        gpu,
        gpu_two,
        small,
        state,
        traces,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for gpu in [&self.gpu, &self.gpu_two] {
            gpu.sleep();
            let _ = std::fs::remove_file(&gpu.marker);
        }
    }
}

async fn chat(sb: &str, model: &str, body: Value, wake: Option<&str>) -> reqwest::Response {
    post_to(sb, "/v1/chat/completions", model, body, wake).await
}

async fn post_to(
    sb: &str,
    path: &str,
    model: &str,
    body: Value,
    wake: Option<&str>,
) -> reqwest::Response {
    let mut body = body;
    body["model"] = json!(model);
    let mut request = reqwest::Client::new()
        .post(format!("{sb}{path}"))
        .json(&body);
    if let Some(wake) = wake {
        request = request.header("x-switchback-wake", wake);
    }
    request.send().await.unwrap()
}

fn small_request() -> Value {
    json!({"messages":[{"role":"user","content":"hi"}], "max_tokens": 64})
}

async fn served_by(response: reqwest::Response) -> (String, String) {
    assert_eq!(response.status(), 200, "{response:?}");
    let route = response.headers()["x-switchback-route"]
        .to_str()
        .unwrap()
        .to_string();
    let body: Value = response.json().await.unwrap();
    (
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .to_string(),
        route,
    )
}

fn last_reasons(traces: &sb_trace::TraceLog) -> Vec<String> {
    traces
        .recent(1)
        .pop()
        .expect("trace recorded")
        .decision
        .reason
}

/// The GPU box lane's last wake result, from the capacity report.
fn gpu_last_wake_result(f: &Fixture) -> Option<String> {
    f.state
        .capacity
        .reports()
        .into_iter()
        .find(|report| report.name == "gpu-box")
        .unwrap()
        .last_wake_result
}

#[tokio::test]
async fn a_small_request_is_served_awake_and_wakes_nothing() {
    let f = fixture(Some(true)).await;
    let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=small-host");
    assert_eq!(f.gpu.wakes(), 0, "the wake command never ran");
    assert_eq!(f.gpu.served.load(Ordering::SeqCst), 0);
    assert!(
        route.contains(
            "prefer_awake=[gpu-box asleep (wake time unmeasured), chose small-host/small]"
        ),
        "the route header says why: {route}"
    );
    assert!(
        route.contains("fallbacks=[gpu-box/big]"),
        "the sleeper stays a fallback: {route}"
    );
    let reasons = last_reasons(&f.traces);
    assert!(
        reasons
            .iter()
            .any(|r| r
                == "prefer_awake: gpu-box asleep (wake time unmeasured), chose small-host/small"),
        "{reasons:?}"
    );
    // No wake started in the background either: none was sent, and none
    // turns up a moment later.
    assert_eq!(gpu_last_wake_result(&f), None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(f.gpu.wakes(), 0, "no late background wake");
    assert_eq!(gpu_last_wake_result(&f), None);
}

#[tokio::test]
async fn header_always_wakes_the_sleeping_machine() {
    let f = fixture(Some(true)).await;
    let (content, route) =
        served_by(chat(&f.sb, "chat", small_request(), Some("always")).await).await;
    assert_eq!(content, "served=gpu-box");
    assert_eq!(f.gpu.wakes(), 1);
    assert_eq!(f.small.served.load(Ordering::SeqCst), 0);
    assert!(
        route.contains(
            "prefer_awake=[x-switchback-wake=always, route order kept, wakes gpu-box asleep"
        ),
        "{route}"
    );
}

#[tokio::test]
async fn a_big_prompt_wakes_the_sleeping_machine() {
    let f = fixture(Some(true)).await;
    // ~1250 tokens by the chars/4 estimate: over wake_for_prompt_tokens.
    let body = json!({"messages":[{"role":"user","content":"x".repeat(5_000)}]});
    let (content, route) = served_by(chat(&f.sb, "chat", body, None).await).await;
    assert_eq!(content, "served=gpu-box");
    assert_eq!(f.gpu.wakes(), 1);
    assert!(
        route.contains("prefer_awake=[large prompt (~1250 tokens, wake_for_prompt_tokens 1000), route order kept"),
        "{route}"
    );
}

#[tokio::test]
async fn a_long_output_request_wakes_the_sleeping_machine() {
    let f = fixture(Some(true)).await;
    let body = json!({"messages":[{"role":"user","content":"hi"}], "max_tokens": 4000});
    let (content, _) = served_by(chat(&f.sb, "chat", body, None).await).await;
    assert_eq!(content, "served=gpu-box");
    assert_eq!(f.gpu.wakes(), 1);
}

#[tokio::test]
async fn header_never_answers_503_when_only_sleepers_can_serve() {
    let f = fixture(Some(true)).await;
    let response = chat(&f.sb, "gpu-only", small_request(), Some("never")).await;
    assert_eq!(response.status(), 503);
    assert!(response.headers().contains_key("x-switchback-route"));
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "local_lane_asleep", "{error}");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("x-switchback-wake=never"),
        "{error}"
    );
    assert_eq!(f.gpu.wakes(), 0, "never woken");

    // A big request with `never` still never wakes: it goes to the small host.
    let body = json!({"messages":[{"role":"user","content":"x".repeat(5_000)}]});
    let (content, _) = served_by(chat(&f.sb, "chat", body, Some("never")).await).await;
    assert_eq!(content, "served=small-host");
    assert_eq!(f.gpu.wakes(), 0);
}

#[tokio::test]
async fn an_awake_target_that_fails_falls_back_to_the_sleeper_and_wakes_it() {
    let f = fixture(Some(true)).await;
    f.small.fail.store(true, Ordering::SeqCst);
    let (content, _) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=gpu-box");
    assert_eq!(f.small.served.load(Ordering::SeqCst), 1, "tried first");
    assert_eq!(f.gpu.wakes(), 1, "woken only for the fallback");
    let trace = f.traces.recent(1).pop().unwrap();
    assert!(
        trace.warnings.iter().any(|w| w.starts_with(
            "prefer_awake: fell back to gpu-box/big; local lane `gpu-box` woken for this request"
        )),
        "{:?}",
        trace.warnings
    );
}

#[tokio::test]
async fn when_the_awake_lane_is_closed_by_its_guard_the_sleeper_is_woken() {
    let f = fixture(Some(true)).await;
    f.small.down.store(true, Ordering::SeqCst);
    f.state.capacity.observe_all().await; // the small host's lane is seen closed
    let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=gpu-box");
    assert_eq!(f.gpu.wakes(), 1);
    assert_eq!(
        f.small.served.load(Ordering::SeqCst),
        0,
        "skipped, not tried"
    );
    assert!(
        route.contains("prefer_awake=[nothing awake can serve, wakes gpu-box asleep"),
        "{route}"
    );
}

/// `prefer_awake` absent and `prefer_awake.enabled: false` both route
/// exactly as before: the first target's lane is known down, so the request
/// wakes it (today's `wake_on_request` behaviour) and nothing says
/// `prefer_awake`.
#[tokio::test]
async fn prefer_awake_off_routes_exactly_as_before() {
    for prefer_awake in [None, Some(false)] {
        let f = fixture(prefer_awake).await;
        let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
        assert_eq!(content, "served=gpu-box", "{prefer_awake:?}");
        assert_eq!(f.gpu.wakes(), 1, "{prefer_awake:?}");
        assert_eq!(f.small.served.load(Ordering::SeqCst), 0);
        assert!(!route.contains("prefer_awake"), "{route}");
        // The header means nothing without `prefer_awake`.
        f.gpu.sleep();
        f.state.capacity.observe_all().await;
        let response = chat(&f.sb, "gpu-only", small_request(), Some("never")).await;
        assert_eq!(response.status(), 200, "{prefer_awake:?}");
        let reasons = last_reasons(&f.traces);
        assert!(
            !reasons.iter().any(|r| r.starts_with("prefer_awake")),
            "{reasons:?}"
        );
    }
}

#[tokio::test]
async fn a_wake_is_measured_shown_and_used_in_the_reason_line() {
    // The wake takes at least 300 ms, so the measure has a floor that does
    // not come from the measure itself.
    let f = fixture_with(Opts {
        prefer_awake: Some(true),
        wake_delay: "sleep 0.3 && ",
        ..Opts::default()
    })
    .await;
    let (content, _) = served_by(chat(&f.sb, "chat", small_request(), Some("always")).await).await;
    assert_eq!(content, "served=gpu-box");

    let report = f
        .state
        .capacity
        .reports()
        .into_iter()
        .find(|report| report.name == "gpu-box")
        .unwrap();
    let measured = report.wake_duration_ms.expect("the wake was measured");
    assert!(measured >= 300, "the wake took at least 300 ms: {measured}");
    assert!(measured < 10_000, "{measured}");
    assert_eq!(report.wake_samples, 1);
    assert_eq!(report.wake_duration_ewma_ms, Some(measured));
    let small = f
        .state
        .capacity
        .reports()
        .into_iter()
        .find(|report| report.name == "small-host")
        .unwrap();
    assert!(small.wake_duration_ms.is_none(), "never woken");

    let capacity: Value = reqwest::get(format!("{}/v1/workloads/capacity", f.sb))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lane = capacity["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lane| lane["name"] == "gpu-box")
        .unwrap();
    assert_eq!(lane["wake_duration_ms"], json!(measured), "{capacity}");
    assert_eq!(lane["wake_duration_ewma_ms"], json!(measured));

    let stats: Value = reqwest::get(format!("{}/v1/targets/stats", f.sb))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        stats["wake"],
        json!([
            {"host": "gpu-box", "lanes": ["gpu-box"], "last_wake_ms": measured,
             "wake_ewma_ms": measured, "samples": 1},
            {"host": "gpu-two", "lanes": ["gpu-two"], "last_wake_ms": null,
             "wake_ewma_ms": null, "samples": 0}
        ]),
        "{stats}"
    );

    // The machine sleeps again: the next small request's reason line now
    // carries the measured wake time, which is not zero.
    f.gpu.sleep();
    f.state.capacity.observe_all().await;
    let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=small-host");
    let shown = route
        .split("prefer_awake=[gpu-box asleep (wake ~")
        .nth(1)
        .and_then(|rest| rest.split("s), chose small-host/small]").next())
        .unwrap_or_else(|| panic!("the reason line shows the wake time: {route}"));
    let seconds: f64 = shown.parse().unwrap_or_else(|_| panic!("{route}"));
    assert!(seconds >= 0.3, "a non-zero wake time: {route}");
    assert_eq!(f.gpu.wakes(), 1, "no second wake");
}

/// The small host's lane takes one request at a time; the first request
/// holds it (slow engine) while the second arrives. Returns the second
/// request's answer and the first one's task.
async fn second_while_small_host_busy(
    f: &Fixture,
    hold_ms: u64,
) -> (
    reqwest::Response,
    tokio::task::JoinHandle<reqwest::Response>,
) {
    f.small.slow_ms.store(hold_ms, Ordering::SeqCst);
    let sb = f.sb.clone();
    let first = tokio::spawn(async move { chat(&sb, "chat", small_request(), None).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = chat(&f.sb, "chat", small_request(), None).await;
    (second, first)
}

/// Review finding: with the default `on_full: spill`, a full awake lane
/// spilled a small request onto the sleeping machine and woke it. Now the
/// request waits in the full lane's queue and is served awake.
#[tokio::test]
async fn review_full_awake_lane_spills_into_a_wake() {
    let f = fixture_with(Opts {
        prefer_awake: Some(true),
        small_lane: "    max_in_flight: 1\n",
        ..Opts::default()
    })
    .await;
    let (second, first) = second_while_small_host_busy(&f, 1_500).await;
    let (content, route) = served_by(second).await;
    let (first_content, _) = served_by(first.await.unwrap()).await;
    assert_eq!(first_content, "served=small-host");
    assert_eq!(content, "served=small-host", "{route}");
    assert_eq!(
        f.gpu.wakes(),
        0,
        "a small request must not wake the GPU because the awake lane was briefly full: {route}"
    );
    assert_eq!(f.gpu.hits.load(Ordering::SeqCst), 0);
    assert!(
        route.contains(
            "prefer_awake=[small-host full, queued; gpu-box asleep (wake time unmeasured), chose small-host/small]"
        ),
        "{route}"
    );
    assert!(!route.contains("nothing awake can serve"), "{route}");
}

/// Review finding, `on_full: queue`: the request queues, no wake.
#[tokio::test]
async fn review_full_awake_lane_queue_does_not_wake() {
    let f = fixture_with(Opts {
        prefer_awake: Some(true),
        small_lane: "    max_in_flight: 1\n    on_full: queue\n",
        ..Opts::default()
    })
    .await;
    let (second, first) = second_while_small_host_busy(&f, 1_500).await;
    let (content, route) = served_by(second).await;
    let _ = first.await;
    assert_eq!(content, "served=small-host", "{route}");
    assert_eq!(f.gpu.wakes(), 0, "{route}");
    assert!(route.contains("small-host full, queued"), "{route}");
}

/// The awake lane stays busy past `queue_timeout_secs`: only then does the
/// small request fall back to the sleeper, and wakes it once. The reason
/// says the awake lane was full, not that nothing awake can serve.
#[tokio::test]
async fn a_full_awake_lane_falls_back_to_the_sleeper_after_the_queue_times_out() {
    let f = fixture_with(Opts {
        prefer_awake: Some(true),
        small_lane: "    max_in_flight: 1\n    queue_timeout_secs: 1\n",
        ..Opts::default()
    })
    .await;
    let started = std::time::Instant::now();
    let (second, first) = second_while_small_host_busy(&f, 3_000).await;
    let (content, route) = served_by(second).await;
    assert!(
        started.elapsed() >= Duration::from_millis(1_300),
        "waited the queue out first: {:?}",
        started.elapsed()
    );
    assert_eq!(content, "served=gpu-box", "{route}");
    assert_eq!(f.gpu.wakes(), 1, "woken once, for the fallback: {route}");
    assert!(
        route.contains("prefer_awake=[small-host full, queued, waited"),
        "{route}"
    );
    assert!(
        route.contains("wakes gpu-box asleep (wake time unmeasured) for gpu-box/big]"),
        "{route}"
    );
    assert!(!route.contains("nothing awake can serve"), "{route}");
    let reasons = last_reasons(&f.traces);
    assert_eq!(
        reasons
            .iter()
            .filter(|r| r.starts_with("prefer_awake:"))
            .count(),
        1,
        "{reasons:?}"
    );
    let (first_content, _) = served_by(first.await.unwrap()).await;
    assert_eq!(first_content, "served=small-host");
    assert_eq!(f.gpu.wakes(), 1);
}

/// Hedging races the top `max_parallel` candidates; a sleeper among them
/// turns the race off, so nothing is dispatched into a machine that is off.
#[tokio::test]
async fn hedging_never_races_a_sleeper() {
    let f = fixture_with(Opts {
        prefer_awake: Some(true),
        server: "  hedge: { enabled: true, delay_ms: 0, max_parallel: 2 }\n",
        ..Opts::default()
    })
    .await;
    let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=small-host", "{route}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.gpu.hits.load(Ordering::SeqCst), 0, "never raced: {route}");
    assert_eq!(f.gpu.wakes(), 0);
}

async fn embed(f: &Fixture, model: &str, input: Value, wake: Option<&str>) -> reqwest::Response {
    post_to(
        &f.sb,
        "/v1/embeddings",
        model,
        json!({"input": input}),
        wake,
    )
    .await
}

/// Embeddings on a `prefer_awake` route: a small input is served awake, a
/// large batch wakes the GPU up front, and `x-switchback-wake` works.
#[tokio::test]
async fn embeddings_prefer_awake_by_input_size_and_read_the_header() {
    let f = fixture(Some(true)).await;
    let response = embed(&f, "embed", json!("hello"), None).await;
    assert_eq!(response.status(), 200);
    let route = response.headers()["x-switchback-route"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        route
            .contains("prefer_awake=[gpu-box asleep (wake time unmeasured), chose small-host/emb]"),
        "{route}"
    );
    assert_eq!(f.small.served.load(Ordering::SeqCst), 1);
    assert_eq!(f.gpu.wakes(), 0);

    // 10 inputs x 500 chars ~ 1250 tokens: over wake_for_prompt_tokens.
    let batch = json!(vec!["x".repeat(500); 10]);
    let response = embed(&f, "embed", batch, None).await;
    assert_eq!(response.status(), 200);
    let route = response.headers()["x-switchback-route"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        route.contains("prefer_awake=[large prompt (~1250 tokens, wake_for_prompt_tokens 1000), route order kept"),
        "{route}"
    );
    assert_eq!(f.gpu.served.load(Ordering::SeqCst), 1, "{route}");
    assert_eq!(f.gpu.wakes(), 1);

    f.gpu.sleep();
    f.state.capacity.observe_all().await;
    let response = embed(&f, "embed-gpu-only", json!("hello"), Some("never")).await;
    assert_eq!(response.status(), 503);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "local_lane_asleep", "{error}");
    assert_eq!(f.gpu.wakes(), 1, "never woken");

    let response = embed(&f, "embed", json!("hello"), Some("always")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(f.gpu.served.load(Ordering::SeqCst), 2);
    assert_eq!(f.gpu.wakes(), 2);
}

/// `/v1/responses` and `/v1/messages` read `x-switchback-wake` too.
#[tokio::test]
async fn the_wake_header_works_on_responses_and_messages() {
    let f = fixture(Some(true)).await;
    let response = post_to(
        &f.sb,
        "/v1/responses",
        "chat",
        json!({"input": "hi", "max_output_tokens": 64}),
        Some("always"),
    )
    .await;
    assert_eq!(response.status(), 200, "{response:?}");
    let route = response.headers()["x-switchback-route"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(route.contains("x-switchback-wake=always"), "{route}");
    assert_eq!(f.gpu.served.load(Ordering::SeqCst), 1);
    assert_eq!(f.gpu.wakes(), 1);

    f.gpu.sleep();
    f.state.capacity.observe_all().await;
    let messages = json!({"max_tokens": 64, "messages": [{"role": "user", "content": "hi"}]});
    let response = post_to(
        &f.sb,
        "/v1/messages",
        "gpu-only",
        messages.clone(),
        Some("never"),
    )
    .await;
    assert_eq!(response.status(), 503);
    assert_eq!(f.gpu.wakes(), 1, "never woken");
    let response = post_to(&f.sb, "/v1/messages", "chat", messages, Some("always")).await;
    assert_eq!(response.status(), 200, "{response:?}");
    let route = response.headers()["x-switchback-route"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(route.contains("x-switchback-wake=always"), "{route}");
    assert_eq!(f.gpu.served.load(Ordering::SeqCst), 2);
    assert_eq!(f.gpu.wakes(), 2);
}

async fn preview(f: &Fixture, model: &str, wake: Option<&str>) -> reqwest::Response {
    post_to(&f.sb, "/cp/v1/route-preview", model, small_request(), wake).await
}

/// A route preview says what wake-aware ordering would do and never wakes;
/// `x-switchback-wake: never` with only sleepers answers 503 there too.
#[tokio::test]
async fn route_preview_never_wakes_and_never_gives_503() {
    let f = fixture(Some(true)).await;
    let response = preview(&f, "chat", None).await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["decision"]["prefer_awake"],
        "gpu-box asleep (wake time unmeasured), chose small-host/small",
        "{body}"
    );
    let response = preview(&f, "gpu-only", Some("always")).await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["candidates"], json!(["gpu-box/big"]), "{body}");

    let response = preview(&f, "gpu-only", Some("never")).await;
    assert_eq!(response.status(), 503);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "local_lane_asleep", "{error}");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(f.gpu.wakes(), 0, "a preview never wakes");
    assert_eq!(f.gpu.hits.load(Ordering::SeqCst), 0);
    assert_eq!(gpu_last_wake_result(&f), None);
}

/// `/v1/targets/stats` shows a tenant key the wake cost only of machines
/// behind routes it can reach; an admin key sees every machine.
#[tokio::test]
async fn target_stats_wake_costs_are_scoped_to_the_tenant() {
    let f = fixture_with(Opts {
        prefer_awake: Some(true),
        top: r#"api_keys:
  - { key: "sk-test-admin", tenant: ops, role: admin }
  - { key: "sk-test-small", tenant: small-team }
  - { key: "sk-test-gpu", tenant: gpu-team }
tenants:
  - id: ops
  - id: small-team
    allowed_routes: ["small-only"]
  - id: gpu-team
    allowed_routes: ["gpu-only"]
"#,
        ..Opts::default()
    })
    .await;
    let hosts = |key: &'static str| {
        let sb = f.sb.clone();
        async move {
            let stats: Value = reqwest::Client::new()
                .get(format!("{sb}/v1/targets/stats"))
                .bearer_auth(key)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            stats["wake"]
                .as_array()
                .unwrap_or_else(|| panic!("{stats}"))
                .iter()
                .map(|row| row["host"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(hosts("sk-test-admin").await, vec!["gpu-box", "gpu-two"]);
    assert_eq!(hosts("sk-test-gpu").await, vec!["gpu-box"]);
    assert!(hosts("sk-test-small").await.is_empty());
}

/// Two sleeping lanes on one route, nothing awake: the first is woken,
/// the second stays asleep, and the decision says so in one line.
#[tokio::test]
async fn two_sleeping_lanes_wake_one_and_say_it_once() {
    let f = fixture(Some(true)).await;
    let (content, route) =
        served_by(chat(&f.sb, "two-sleepers", small_request(), None).await).await;
    assert_eq!(content, "served=gpu-box");
    assert_eq!(f.gpu.wakes(), 1);
    assert_eq!(f.gpu_two.wakes(), 0, "the second sleeper stays asleep");
    let reasons = last_reasons(&f.traces);
    let lines: Vec<&String> = reasons
        .iter()
        .filter(|r| r.starts_with("prefer_awake:"))
        .collect();
    assert_eq!(
        lines,
        vec!["prefer_awake: nothing awake can serve, wakes gpu-box asleep (wake time unmeasured) for gpu-box/big"],
        "{reasons:?}"
    );
    assert!(
        route.contains("prefer_awake=[nothing awake can serve, wakes gpu-box asleep"),
        "{route}"
    );
}

/// A machine that is waking, or powering off, is not up: a small request
/// goes to the awake machine and says why.
#[tokio::test]
async fn a_waking_or_powering_off_machine_is_not_awake() {
    let f = fixture_with(Opts {
        prefer_awake: Some(true),
        wake_delay: "sleep 1 && ",
        gpu_lane: "    poweroff_command: \"sleep 1\"\n",
        ..Opts::default()
    })
    .await;
    // Waking: one request wakes the GPU (it takes a second); a small
    // request meanwhile is served awake.
    let sb = f.sb.clone();
    let waking =
        tokio::spawn(async move { chat(&sb, "gpu-only", small_request(), Some("always")).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=small-host");
    assert!(
        route.contains(
            "prefer_awake=[gpu-box waking (wake time unmeasured), chose small-host/small]"
        ),
        "{route}"
    );
    let (content, _) = served_by(waking.await.unwrap()).await;
    assert_eq!(content, "served=gpu-box");
    assert_eq!(f.gpu.wakes(), 1);

    // Powering off: the power-off command takes a second.
    let sb = f.sb.clone();
    let off = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{sb}/v1/hosts/gpu-box/power"))
            .json(&json!({"action": "off"}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=small-host");
    assert!(
        route.contains("prefer_awake=[gpu-box powering off (wake ~"),
        "{route}"
    );
    let off = off.await.unwrap();
    assert_eq!(off.status(), 200, "{:?}", off.text().await);
    assert_eq!(f.gpu.wakes(), 1, "no wake during the power-off");
}
