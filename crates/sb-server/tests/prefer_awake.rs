//! Wake-aware routing ("prefer awake") end-to-end. One route lists a target
//! on a GPU machine that sleeps when idle (`gpu-box`: a wake command brings
//! it up) and one on an always-on machine (`small-host`: no wake, its lane
//! closes when its health guard takes the memory). Both engines are loopback
//! stubs; the wake command appends a line to a marker file, so every test can
//! count the wakes it caused.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// The sleeping GPU machine: up only while its `.up` file exists. The wake
/// command appends one line to `marker` (the wake count) and creates it.
#[derive(Clone)]
struct GpuBox {
    marker: std::path::PathBuf,
    served: Arc<AtomicUsize>,
}

impl GpuBox {
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
    if !gpu.up() {
        return (StatusCode::SERVICE_UNAVAILABLE, "host off").into_response();
    }
    gpu.served.fetch_add(1, Ordering::SeqCst);
    completion("served=gpu-box")
}

/// The always-on machine: `up` false = its lane closed (the health guard
/// took the memory); `fail` true = the engine answers 500.
#[derive(Clone, Default)]
struct SmallHost {
    down: Arc<AtomicBool>,
    fail: Arc<AtomicBool>,
    served: Arc<AtomicUsize>,
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
    if small.fail.load(Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "engine error").into_response();
    }
    completion("served=small-host")
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

async fn listen(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

struct Fixture {
    sb: String,
    gpu: GpuBox,
    small: SmallHost,
    state: sb_server::AppState,
    traces: Arc<sb_trace::TraceLog>,
}

/// `prefer_awake`: `None` = not configured; `Some(enabled)` = on the route.
async fn fixture(prefer_awake: Option<bool>) -> Fixture {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let marker = std::env::temp_dir().join(format!(
        "sb-prefer-awake-{}-{nanos}-{}.wakes",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let gpu = GpuBox {
        marker: marker.clone(),
        served: Arc::new(AtomicUsize::new(0)),
    };
    let gpu_url = listen(
        Router::new()
            .route("/chat/completions", post(gpu_chat))
            .route("/health", get(gpu_health))
            .with_state(gpu.clone()),
    )
    .await;
    let small = SmallHost::default();
    let small_url = listen(
        Router::new()
            .route("/chat/completions", post(small_chat))
            .route("/health", get(small_health))
            .with_state(small.clone()),
    )
    .await;
    let prefer = match prefer_awake {
        None => String::new(),
        Some(enabled) => format!(
            "    prefer_awake: {{ enabled: {enabled}, wake_for_prompt_tokens: 1000, wake_for_output_tokens: 500 }}\n"
        ),
    };
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
  retry:
    max_retries: 0
providers:
  - id: gpu-box
    type: openai_compatible
    base_url: "{gpu_url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
  - id: small-host
    type: openai_compatible
    base_url: "{small_url}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
routes:
  - name: chat
    match: {{ model: "chat" }}
{prefer}    targets:
      - "gpu-box/big"
      - "small-host/small"
  - name: gpu-only
    match: {{ model: "gpu-only" }}
{prefer}    targets:
      - "gpu-box/big"
local_executors:
  - name: gpu-box
    base_url: "{gpu_url}"
    health_endpoint: "{gpu_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 10
    wake_command: "echo wake >> '{}' && touch '{}'"
    boot_timeout_secs: 10
    health_poll_interval_ms: 20
  - name: small-host
    base_url: "{small_url}"
    health_endpoint: "{small_url}/health"
    route_health: true
"#,
        marker.display(),
        marker.with_extension("up").display()
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
    // The supervisor's first look: the GPU machine is off, the small one up.
    state.capacity.observe_all().await;
    Fixture {
        sb,
        gpu,
        small,
        state,
        traces,
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.gpu.sleep();
        let _ = std::fs::remove_file(&self.gpu.marker);
    }
}

async fn chat(sb: &str, model: &str, body: Value, wake: Option<&str>) -> reqwest::Response {
    let mut body = body;
    body["model"] = json!(model);
    let mut request = reqwest::Client::new()
        .post(format!("{sb}/v1/chat/completions"))
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
    let f = fixture(Some(true)).await;
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
        json!([{"host": "gpu-box", "lanes": ["gpu-box"], "last_wake_ms": measured,
                "wake_ewma_ms": measured, "samples": 1}]),
        "{stats}"
    );

    // The machine sleeps again: the next small request's reason line now
    // carries the measured wake time.
    f.gpu.sleep();
    f.state.capacity.observe_all().await;
    let (content, route) = served_by(chat(&f.sb, "chat", small_request(), None).await).await;
    assert_eq!(content, "served=small-host");
    let seconds = (measured + 500) / 1000;
    assert!(
        route.contains(&format!(
            "prefer_awake=[gpu-box asleep (wake ~{seconds}s), chose small-host/small]"
        )),
        "{route}"
    );
    assert_eq!(f.gpu.wakes(), 1, "no second wake");
}
