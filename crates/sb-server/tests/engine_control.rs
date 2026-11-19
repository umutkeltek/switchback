//! Engine control end to end against a fake llama.cpp router: load-time
//! settings applied through `settings_command` (next load and now), load,
//! unload and `GET /v1/engines`, never waking a sleeping machine unless
//! asked, never interrupting a request in flight.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Default)]
struct Router_ {
    /// Every call: "GET /models?reload=1", "POST /models/load m", ...
    calls: Vec<String>,
    status: BTreeMap<String, String>,
    chats: Vec<Value>,
    /// GPU memory the measurement reports (bytes).
    total: u64,
    /// Chat requests inside the engine now.
    busy: usize,
    /// `busy` at every `GET /models?reload=1` and every unload.
    busy_at_reload: Vec<usize>,
    busy_at_unload: Vec<usize>,
    unload_delay_ms: u64,
    /// The overlay JSON the settings command writes; on a re-read the
    /// router compares each model's section with the one it read last and,
    /// like llama.cpp, unloads a running model whose section changed.
    overlay_path: Option<PathBuf>,
    presets: BTreeMap<String, Value>,
    /// The engine has not measured any model.
    unmeasured: bool,
    /// How long a "slow please" chat takes (ms; 0 = 1500).
    slow_ms: u64,
}

#[derive(Clone)]
struct Stub {
    inner: Arc<Mutex<Router_>>,
    /// The machine is up while this file exists.
    up_marker: PathBuf,
}

impl Stub {
    fn calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.clone()
    }
    fn engine_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|call| call.contains("/models"))
            .collect()
    }
    fn set(&self, model: &str, status: &str) {
        self.inner
            .lock()
            .unwrap()
            .status
            .insert(model.to_string(), status.to_string());
    }
}

async fn health(State(stub): State<Stub>) -> StatusCode {
    if stub.up_marker.exists() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn models(
    State(stub): State<Stub>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Json<Value> {
    let mut inner = stub.inner.lock().unwrap();
    let call = match query.get("reload") {
        Some(_) => {
            let busy = inner.busy;
            inner.busy_at_reload.push(busy);
            let read: BTreeMap<String, Value> = inner
                .overlay_path
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|overlay| overlay["models"].as_object().cloned())
                .map(|models| models.into_iter().collect())
                .unwrap_or_default();
            let changed: Vec<String> = read
                .keys()
                .chain(inner.presets.keys())
                .filter(|id| read.get(*id) != inner.presets.get(*id))
                .cloned()
                .collect();
            for id in changed {
                if matches!(
                    inner.status.get(&id).map(String::as_str),
                    Some("loaded" | "loading")
                ) {
                    inner.status.insert(id.clone(), "unloaded".to_string());
                    inner.calls.push(format!("reload unloaded {id}"));
                }
            }
            inner.presets = read;
            "GET /models?reload=1".to_string()
        }
        None => "GET /models".to_string(),
    };
    inner.calls.push(call);
    let data: Vec<Value> = inner
        .status
        .iter()
        .map(|(id, status)| {
            if inner.unmeasured {
                return json!({"id": id, "status": {"value": status}, "memory": {"status": "unmeasured"}});
            }
            json!({
                "id": id,
                "status": {"value": status, "args": ["llama-server", "--ctx-size", "4096"]},
                "memory": {
                    "status": "measured",
                    "min": {"n_ctx": 4096, "devices": [
                        {"kind": "device", "name": "CUDA0", "total": inner.total, "model": 1000, "context": 100, "compute": 10},
                        {"kind": "host", "name": "Host", "model": 5, "context": 0, "compute": 1}]},
                    "max": {"n_ctx": 131072, "devices": []},
                    "effective": null
                }
            })
        })
        .collect();
    Json(json!({"data": data}))
}

async fn load(State(stub): State<Stub>, Json(body): Json<Value>) -> Json<Value> {
    let model = body["model"].as_str().unwrap_or("").to_string();
    let mut inner = stub.inner.lock().unwrap();
    inner.calls.push(format!("POST /models/load {model}"));
    inner.status.insert(model, "loaded".to_string());
    Json(json!({"success": true}))
}

async fn unload(State(stub): State<Stub>, Json(body): Json<Value>) -> Json<Value> {
    let model = body["model"].as_str().unwrap_or("").to_string();
    let delay = stub.inner.lock().unwrap().unload_delay_ms;
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let mut inner = stub.inner.lock().unwrap();
    let busy = inner.busy;
    inner.busy_at_unload.push(busy);
    inner.calls.push(format!("POST /models/unload {model}"));
    inner.status.insert(model, "unloaded".to_string());
    Json(json!({"success": true}))
}

async fn chat(State(stub): State<Stub>, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let slow = body.to_string().contains("slow please");
    {
        let mut inner = stub.inner.lock().unwrap();
        inner.calls.push("POST /v1/chat/completions".to_string());
        inner.chats.push(body);
        inner.busy += 1;
    }
    if slow {
        let ms = match stub.inner.lock().unwrap().slow_ms {
            0 => 1500,
            ms => ms,
        };
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    stub.inner.lock().unwrap().busy -= 1;
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "ok"}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

fn temp(name: &str) -> PathBuf {
    // Tests run in parallel and the clock may tick in microseconds: a
    // counter keeps every name apart.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sb-engine-{name}-{}-{nanos}-{n}",
        std::process::id()
    ))
}

async fn spawn_stub(up: bool) -> (String, Stub) {
    let marker = temp("up");
    if up {
        std::fs::write(&marker, b"up").unwrap();
    }
    let stub = Stub {
        inner: Arc::default(),
        up_marker: marker,
    };
    stub.set("m", "loaded");
    stub.set("other", "unloaded");
    stub.inner.lock().unwrap().total = 24u64 << 30;
    let app = Router::new()
        .route("/health", get(health))
        .route("/models", get(models))
        .route("/models/load", post(load))
        .route("/models/unload", post(unload))
        .route("/v1/chat/completions", post(chat))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), stub)
}

struct Setup {
    gw: String,
    stub: Stub,
    overlay: PathBuf,
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

struct Opts<'a> {
    up: bool,
    /// Top-level YAML (tenants, api_keys, ...).
    extra: &'a str,
    settings_command: Option<String>,
    /// More keys of the lane (`max_in_flight: 1`, ...), each on its own
    /// line, indented like the lane's keys.
    lane: &'a str,
    apply_wait_secs: u64,
    /// A settings command writing the overlay JSON, with `{marker}` (the
    /// machine-up marker) and `{overlay}` filled in.
    command_template: Option<&'a str>,
}

impl Default for Opts<'_> {
    fn default() -> Self {
        Opts {
            up: true,
            extra: "",
            settings_command: None,
            lane: "",
            apply_wait_secs: 1,
            command_template: None,
        }
    }
}

async fn setup(up: bool, engine_extra: &str, settings_command: Option<String>) -> Setup {
    setup_with(Opts {
        up,
        extra: engine_extra,
        settings_command,
        ..Opts::default()
    })
    .await
    .0
}

async fn setup_with(opts: Opts<'_>) -> (Setup, sb_server::AppState) {
    let (stub_url, stub) = spawn_stub(opts.up).await;
    let overlay = temp("overlay.json");
    let default_command = opts.settings_command.is_none();
    let command = match (opts.settings_command, opts.command_template) {
        (Some(command), _) => command,
        (None, Some(template)) => template
            .replace("{marker}", &quoted(&stub.up_marker))
            .replace("{overlay}", &quoted(&overlay)),
        (None, None) => format!("cat > {}", quoted(&overlay)),
    };
    if default_command {
        stub.inner.lock().unwrap().overlay_path = Some(overlay.clone());
    }
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "{stub_url}/v1"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: other
    type: openai_compatible
    base_url: "{stub_url}/v1"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
local_executors:
  - name: text
    base_url: "{stub_url}"
    health_endpoint: "{stub_url}/health"
    wake_command: "touch {marker}"
    boot_timeout_secs: 10
    health_poll_interval_ms: 50
{lane}
    engine:
      kind: llama_cpp_router
      settings_command: "{command}"
      apply_wait_secs: {wait}
      load_timeout_secs: 5
{extra}
"#,
        marker = quoted(&stub.up_marker),
        command = command.replace('"', "\\\""),
        lane = opts.lane,
        wait = opts.apply_wait_secs,
        extra = opts.extra,
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
    let app = sb_server::build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (
        Setup {
            gw: format!("http://{addr}"),
            stub,
            overlay,
        },
        state,
    )
}

async fn put(gw: &str, model: &str, revision: u64, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .put(format!("{gw}/v1/models/{model}/settings"))
        .header("if-match", revision.to_string())
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn post_to(gw: &str, path: &str, body: Value) -> (u16, Value) {
    let r = reqwest::Client::new()
        .post(format!("{gw}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

async fn get_json(url: String) -> Value {
    reqwest::get(url).await.unwrap().json().await.unwrap()
}

fn overlay_of(setup: &Setup) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&setup.overlay).unwrap()).unwrap()
}

#[tokio::test]
async fn next_load_writes_the_overlay_reloads_presets_and_shows_pending() {
    let s = setup(true, "", None).await;
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 65536, "flash_attn": true, "mlock": true, "cache_type_k": "q8_0"},
               "engine_args": {"n-cpu-moe": 4}}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["mode"], json!("next_load"));
    assert_eq!(view["apply"]["ok"], json!(true), "{view}");
    assert_eq!(
        overlay_of(&s)["models"]["m"]["settings"],
        json!({"ctx-size": "65536", "flash-attn": "on", "load-mode": "mmap+mlock",
               "cache-type-k": "q8_0", "n-cpu-moe": "4"})
    );
    let calls = s.stub.calls();
    assert!(
        calls.contains(&"GET /models?reload=1".to_string()),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.starts_with("POST /models")),
        "next_load loaded or unloaded a model itself: {calls:?}"
    );
    // `m` was loaded and idle: the engine re-read its preset and unloaded
    // it, so its next request loads it with the new settings.
    assert!(
        calls.contains(&"reload unloaded m".to_string()),
        "{calls:?}"
    );

    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    let lane = &engines["data"][0];
    assert_eq!(lane["lane"], json!("text"));
    assert_eq!(lane["state"], json!("up"));
    let m = lane["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "m")
        .unwrap();
    assert_eq!(m["status"], json!("unloaded"));
    assert_eq!(m["pending"], json!(false));
    assert_eq!(m["memory"]["min"]["bytes"], json!(1110));
    assert_eq!(m["memory"]["min"]["host_bytes"], json!(6));
    assert_eq!(lane["pending_apply"], json!(false));
    assert!(lane.get("note").is_none(), "{lane}");
    let view = get_json(format!("{}/v1/models/text/m/settings", s.gw)).await;
    assert_eq!(view["targets"][0]["load"]["pending"], json!(false));
    assert_eq!(
        view["targets"][0]["load"]["settings"]["ctx_size"]["engine_key"],
        json!("ctx-size")
    );
}

#[tokio::test]
async fn apply_now_reloads_a_loaded_model() {
    let s = setup(true, "", None).await;
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 32768}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["mode"], json!("now"));
    let calls = s.stub.calls();
    // The re-read unloads the running model (its preset changed); the
    // control then loads it again with the new settings.
    let unload = calls
        .iter()
        .position(|c| c == "POST /models/unload m" || c == "reload unloaded m");
    let load = calls.iter().position(|c| c == "POST /models/load m");
    assert!(unload.is_some() && load > unload, "{calls:?}");
    assert_eq!(view["apply"]["reloaded_model"], json!(true), "{view}");
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    let m = engines["data"][0]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "m")
        .unwrap()
        .clone();
    assert_eq!(m["pending"], json!(false));
}

#[tokio::test]
async fn unload_and_apply_now_never_interrupt_a_request_in_flight() {
    let s = setup(true, "", None).await;
    // Longer than the unload's and the apply's waits (1 s each) together.
    s.stub.inner.lock().unwrap().slow_ms = 3500;
    let gw = s.gw.clone();
    let slow = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "text/m", "messages": [{"role": "user", "content": "slow please"}]}))
            .send()
            .await
            .unwrap()
            .status()
    });
    // Wait until the request reached the engine.
    for _ in 0..100 {
        if !s.stub.inner.lock().unwrap().chats.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Unload waits for it (apply_wait_secs 1 here), then refuses.
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["type"], json!("model_busy"));
    // apply: now on the busy model: the write stands and answers 200; the
    // engine re-reads the new settings when the model is idle.
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 8192}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["revision"], json!(1), "the settings were written");
    assert_eq!(view["apply"]["reloaded"], json!(false), "{view}");
    assert_eq!(view["apply"]["reason"], json!("model_busy"), "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    assert!(
        !s.stub.calls().iter().any(|c| c.contains("unload")),
        "a busy model was unloaded: {:?}",
        s.stub.calls()
    );
    assert!(s.stub.inner.lock().unwrap().busy_at_reload.is_empty());
    assert_eq!(slow.await.unwrap(), 200);
    // Idle now: the held-back apply runs by itself.
    wait_for(&s, |calls| {
        calls.contains(&"GET /models?reload=1".to_string())
    })
    .await;
    let busy = s.stub.inner.lock().unwrap().busy_at_reload.clone();
    assert!(busy.iter().all(|b| *b == 0), "{busy:?}");
    assert_eq!(
        overlay_of(&s)["models"]["m"]["settings"]["ctx-size"],
        json!("8192")
    );
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert!(s
        .stub
        .calls()
        .contains(&"POST /models/unload m".to_string()));
}

#[tokio::test]
async fn a_sleeping_lane_is_never_woken_by_reads_writes_or_a_plain_load() {
    let s = setup(false, "", None).await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["reloaded"], json!(false));
    assert!(s.overlay.exists(), "the overlay was not written");
    let _ = get_json(format!("{}/v1/models/text/m/settings", s.gw)).await;
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(engines["data"][0]["state"], json!("asleep"));
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["type"], json!("lane_asleep"));
    let (status, _) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 200);
    assert!(!s.stub.up_marker.exists(), "something woke the machine");
    assert_eq!(s.stub.engine_calls(), Vec::<String>::new());

    // With wake: the machine comes up and the model loads.
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({"wake": true})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("loaded"));
    assert!(s.stub.up_marker.exists());
    assert!(s.stub.calls().contains(&"POST /models/load m".to_string()));
}

#[tokio::test]
async fn engine_arguments_and_paths_are_checked() {
    let s = setup(true, "", None).await;
    for (body, needle) in [
        (json!({"engine_args": {"port": 1}}), "router's own"),
        (
            json!({"engine_args": {"not-an-arg": 1}}),
            "not a llama-server argument",
        ),
        (
            json!({"engine_args": {"model-draft": "/x.gguf"}}),
            "allow_model_path",
        ),
        (json!({"load": {"mmproj": "/x.gguf"}}), "allow_model_path"),
        (json!({"load": {"ctx_size": -3}}), "ctx_size"),
        (
            json!({"engine_args": {"tools": "all"}}),
            "`tools` is not allowed",
        ),
        (
            json!({"engine_args": {"mcp-servers-config": "/x.json"}}),
            "`mcp-servers-config` is not allowed",
        ),
        (
            json!({"engine_args": {"spec-draft-hf": "org/x"}}),
            "`spec-draft-hf` is not allowed",
        ),
    ] {
        let (status, view) = put(&s.gw, "text/m", 0, body.clone()).await;
        assert_eq!(status, 400, "{body}: {view}");
        let message = view["error"]["message"].as_str().unwrap();
        assert!(message.contains(needle), "{body}: {message}");
    }
    // A model the engine does not list takes no load settings.
    let (status, view) = put(
        &s.gw,
        "text/qwen-typo",
        0,
        json!({"load": {"ctx_size": 4096}}),
    )
    .await;
    assert_eq!(status, 400, "{view}");
    assert!(
        view["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not list `qwen-typo`"),
        "{view}"
    );
    // A target without an engine control takes no load settings.
    let (status, view) = put(&s.gw, "other/x", 0, json!({"load": {"ctx_size": 4096}})).await;
    assert_eq!(status, 400, "{view}");
    assert!(view["error"]["message"]
        .as_str()
        .unwrap()
        .contains("engine control"));
    assert!(!s.overlay.exists());
}

#[tokio::test]
async fn no_settings_means_no_engine_calls() {
    let s = setup(true, "", None).await;
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(&json!({"model": "text/m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(s.stub.engine_calls(), Vec::<String>::new());
    assert!(!s.overlay.exists());
}

#[tokio::test]
async fn a_hold_can_preload_a_model_after_its_wake() {
    let s = setup(false, "", None).await;
    s.stub.set("m", "unloaded");
    let (status, body) = post_to(
        &s.gw,
        "/v1/hosts/text/holds",
        json!({"ttl_secs": 600, "reason": "eval run", "wake": true, "load": "text/m"}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["load"]["target"], json!("text/m"));
    for _ in 0..200 {
        if s.stub.calls().contains(&"POST /models/load m".to_string()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the hold did not preload: {:?}", s.stub.calls());
}

#[tokio::test]
async fn the_reference_overlay_script_writes_the_presets_the_router_reads() {
    let base = temp("base.ini");
    let out = temp("models.ini");
    std::fs::write(
        &base,
        "version = 1\n\n[m]\nmodel = /models/m.gguf\nctx-size = 4096\n; switchback-pinned: n-gpu-layers\n",
    )
    .unwrap();
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/llamacpp-preset-overlay.py");
    let command = format!(
        "python3 -I {} --base {} --out {}",
        quoted(&script),
        quoted(&base),
        quoted(&out)
    );
    let s = setup(true, "", Some(command)).await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 65536}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(true), "{view}");
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("; managed by switchback\nctx-size = 65536"),
        "{text}"
    );
    assert!(text.contains("model = /models/m.gguf"), "{text}");
    // A pinned key: the script refuses, the write stands, the apply says so.
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"n_gpu_layers": 10}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(false), "{view}");
    assert!(
        view["apply"]["error"].as_str().unwrap().contains("pinned"),
        "{view}"
    );
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(engines["data"][0]["pending_apply"], json!(true));
}

// ---------------------------------------------------------------- variants

const POLICY: &str = r#"{"allow": ["ctx_size"], "bounds": {"ctx_size": [4096, 131072]},
                        "max_variants": 1, "variant_idle_ttl_secs": 3600}"#;

async fn with_policy(s: &Setup, key: Option<&str>) {
    let policy: Value = serde_json::from_str(POLICY).unwrap();
    let mut request = reqwest::Client::new()
        .put(format!("{}/v1/models/text/m/settings", s.gw))
        .header("if-match", "0")
        .json(&json!({"load": {"ctx_size": 8192}, "request_load": policy}));
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let r = request.send().await.unwrap();
    assert_eq!(r.status(), 200);
}

async fn chat_with_load(s: &Setup, key: Option<&str>, load: Value) -> (u16, Value, Option<String>) {
    let mut request = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(
            &json!({"model": "text/m", "messages": [{"role": "user", "content": "hi"}],
                      "switchback": {"load": load}}),
        );
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let r = request.send().await.unwrap();
    let status = r.status().as_u16();
    let settings = r
        .headers()
        .get("x-switchback-settings")
        .map(|v| v.to_str().unwrap().to_string());
    (status, r.json().await.unwrap_or(Value::Null), settings)
}

fn reloads(stub: &Stub) -> usize {
    stub.calls()
        .iter()
        .filter(|c| *c == "GET /models?reload=1")
        .count()
}

#[tokio::test]
async fn a_request_load_setting_makes_and_reuses_a_variant() {
    let s = setup(true, "", None).await;
    with_policy(&s, None).await;
    let before = reloads(&s.stub);
    let (status, body, settings) = chat_with_load(&s, None, json!({"ctx_size": 65536})).await;
    assert_eq!(status, 200, "{body}");
    let sent = s.stub.inner.lock().unwrap().chats.last().unwrap().clone();
    let model = sent["model"].as_str().unwrap().to_string();
    assert!(model.starts_with("m~") && model.len() == 10, "{model}");
    assert!(sent.get("switchback").is_none());
    assert!(settings.unwrap().contains(&format!("variant={model}")));
    let overlay = overlay_of(&s);
    assert_eq!(overlay["models"][&model]["base"], json!("m"));
    assert_eq!(
        overlay["models"][&model]["settings"]["ctx-size"],
        json!("65536")
    );
    assert_eq!(
        overlay["models"]["m"]["settings"]["ctx-size"],
        json!("8192")
    );
    assert_eq!(reloads(&s.stub), before + 1);

    // The same load settings again: the same variant, no new apply.
    let (status, _, _) = chat_with_load(&s, None, json!({"ctx_size": 65536})).await;
    assert_eq!(status, 200);
    assert_eq!(
        s.stub.inner.lock().unwrap().chats.last().unwrap()["model"],
        json!(model)
    );
    assert_eq!(reloads(&s.stub), before + 1);

    // Another one with max_variants 1: the idle old variant goes.
    let (status, _, _) = chat_with_load(&s, None, json!({"ctx_size": 32768})).await;
    assert_eq!(status, 200);
    let newer = s.stub.inner.lock().unwrap().chats.last().unwrap()["model"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(newer, model);
    let overlay = overlay_of(&s);
    assert!(overlay["models"].get(&model).is_none(), "{overlay}");
    assert!(overlay["models"].get(&newer).is_some(), "{overlay}");
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    let variants = engines["data"][0]["variants"].as_array().unwrap();
    assert_eq!(variants.len(), 1);
    assert_eq!(variants[0]["name"], json!(newer));
    assert_eq!(variants[0]["target"], json!("text/m"));
}

#[tokio::test]
async fn request_load_settings_are_checked_against_the_policy() {
    let extra = r#"
tenants:
  - id: acme
  - id: ops
api_keys:
  - { key: "sk-admin", tenant: ops, role: admin }
  - { key: "sk-client", tenant: acme, role: client }
"#;
    let s = setup(true, extra, None).await;
    // No policy yet: refused, naming the policy.
    let (status, body, _) = chat_with_load(&s, Some("sk-client"), json!({"ctx_size": 65536})).await;
    assert_eq!(status, 400, "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("request_load"),
        "{body}"
    );

    with_policy(&s, Some("sk-admin")).await;
    let before = s.stub.engine_calls().len();
    for (key, load, kind) in [
        (
            Some("sk-client"),
            json!({"ctx_size": 999999}),
            "out_of_bounds",
        ),
        (
            Some("sk-client"),
            json!({"cache_type_k": "q8_0"}),
            "request_load_refused",
        ),
        (Some("sk-admin"), json!({"ctx_size": 2048}), "out_of_bounds"),
        (
            Some("sk-admin"),
            json!({"mmproj": "/x.gguf"}),
            "request_load_refused",
        ),
        (
            Some("sk-client"),
            json!({"ctx_size": "big"}),
            "invalid_settings",
        ),
    ] {
        let (status, body, _) = chat_with_load(&s, key, load.clone()).await;
        assert_eq!(status, 400, "{load}: {body}");
        assert_eq!(body["error"]["type"], json!(kind), "{load}: {body}");
    }
    assert_eq!(
        s.stub.engine_calls().len(),
        before,
        "a refused load reached the engine"
    );
    assert!(s.stub.inner.lock().unwrap().chats.is_empty());

    // An operator-class key may set any typed key within bounds.
    let (status, body, _) =
        chat_with_load(&s, Some("sk-admin"), json!({"cache_type_k": "q8_0"})).await;
    assert_eq!(status, 200, "{body}");
    let (status, body, _) = chat_with_load(&s, Some("sk-client"), json!({"ctx_size": 16384})).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn a_variant_that_would_not_fit_is_refused_with_the_numbers() {
    let s = setup(true, "", None).await;
    with_policy(&s, None).await;
    // 1010 bytes of weights and compute, 100 bytes of KV per 4096 tokens:
    // 131072 tokens need 4210 bytes; the GPU has 2000.
    s.stub.inner.lock().unwrap().total = 2000;
    let (status, body, _) = chat_with_load(&s, None, json!({"ctx_size": 131072})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["type"], json!("would_not_fit"));
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("4210 bytes") && message.contains("1900 bytes"),
        "{message}"
    );
    let overlay = overlay_of(&s);
    assert_eq!(overlay["models"].as_object().unwrap().len(), 1, "{overlay}");
    assert!(s.stub.inner.lock().unwrap().chats.is_empty());
}

// ---------------------------------------------------------------- no request is cut off

async fn wait_for(s: &Setup, done: impl Fn(&[String]) -> bool) {
    for _ in 0..400 {
        if done(&s.stub.calls()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never happened: {:?}", s.stub.calls());
}

async fn wait_for_chat(s: &Setup) {
    for _ in 0..200 {
        if s.stub.inner.lock().unwrap().busy > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the slow chat never reached the engine");
}

fn spawn_chat(gw: &str, content: &str) -> tokio::task::JoinHandle<u16> {
    let gw = gw.to_string();
    let content = content.to_string();
    tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{gw}/v1/chat/completions"))
            .json(&json!({"model": "text/m", "messages": [{"role": "user", "content": content}]}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    })
}

fn spawn_slow(gw: &str) -> tokio::task::JoinHandle<u16> {
    spawn_chat(gw, "slow please")
}

/// llama.cpp: `GET /models?reload=1` unloads a RUNNING model whose preset
/// changed. A default `apply: next_load` write must not send reload=1
/// while a request is in flight on a model whose preset it changed.
#[tokio::test]
async fn review_next_load_reloads_presets_under_an_in_flight_request() {
    let s = setup(true, "", None).await;
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 65536}})).await;
    assert_eq!(status, 200, "{view}");
    let busy = s.stub.inner.lock().unwrap().busy_at_reload.clone();
    assert_eq!(slow.await.unwrap(), 200);
    assert!(
        busy.iter().all(|b| *b == 0),
        "reload=1 reached the router while {busy:?} request(s) were in flight on the changed model"
    );
}

/// The in-flight check and the unload are not atomic: a request admitted
/// after the check but before the unload lands reaches an engine that is
/// unloading its model.
#[tokio::test]
async fn review_unload_admits_a_request_between_check_and_unload() {
    let s = setup(true, "", None).await;
    s.stub.inner.lock().unwrap().unload_delay_ms = 300;
    let gw = s.gw.clone();
    let unload =
        tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/unload", json!({})).await });
    tokio::time::sleep(Duration::from_millis(80)).await;
    let slow = spawn_slow(&s.gw);
    let (status, body) = unload.await.unwrap();
    assert_eq!(status, 200, "{body}");
    let _ = slow.await.unwrap();
    let busy = s.stub.inner.lock().unwrap().busy_at_unload.clone();
    assert!(
        busy.iter().all(|b| *b == 0),
        "the router unloaded `m` with {busy:?} request(s) in flight on it"
    );
}

/// A request still waiting for its lane (queued behind a full lane, as it
/// would wait for a wake or the GPU group) counts on its model: an unload
/// waits for it instead of unloading the model it is about to use.
#[tokio::test]
async fn unload_waits_for_a_request_queued_for_its_lane() {
    let (s, _) = setup_with(Opts {
        lane: "    max_in_flight: 1\n    on_full: queue\n    queue_timeout_secs: 10",
        apply_wait_secs: 10,
        ..Opts::default()
    })
    .await;
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    // Queued behind the slow one: admitted to `m`, not yet at the engine.
    let queued = spawn_chat(&s.gw, "hi");
    let mut admitted = 0;
    for _ in 0..200 {
        let engines = get_json(format!("{}/v1/engines", s.gw)).await;
        admitted = engines["data"][0]["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "m")
            .map(|row| row["in_flight"].as_u64().unwrap_or(0))
            .unwrap_or(0);
        if admitted == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(admitted, 2, "the queued request was not counted on `m`");
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(slow.await.unwrap(), 200);
    assert_eq!(queued.await.unwrap(), 200);
    let calls = s.stub.calls();
    let chats: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| *c == "POST /v1/chat/completions")
        .map(|(i, _)| i)
        .collect();
    let unload = calls
        .iter()
        .position(|c| c == "POST /models/unload m")
        .unwrap();
    assert_eq!(chats.len(), 2, "{calls:?}");
    assert!(
        chats.iter().all(|i| *i < unload),
        "unloaded before the queued request ran: {calls:?}"
    );
    let busy = s.stub.inner.lock().unwrap().busy_at_unload.clone();
    assert_eq!(busy, vec![0]);
}

/// Once the gateway knows what the engine read, a re-read that changes
/// only idle models proceeds at once beside a busy one; a change of the
/// busy model is held back (its old section stays) until it is idle.
#[tokio::test]
async fn a_reload_proceeds_for_idle_models_and_holds_back_a_busy_one() {
    let s = setup(true, "", None).await;
    let (status, _) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200);
    s.stub.set("m", "loaded");
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    // Another model of the lane: re-read now; `m`'s section is unchanged.
    let reloads_before = reloads(&s.stub);
    let (status, view) = put(&s.gw, "text/other", 0, json!({"load": {"ctx_size": 4096}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["reloaded"], json!(true), "{view}");
    assert_eq!(reloads(&s.stub), reloads_before + 1);
    // `m` itself: held back, its old section stays in the overlay.
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    assert_eq!(view["apply"]["deferred"], json!(["m"]), "{view}");
    assert_eq!(reloads(&s.stub), reloads_before + 1);
    assert_eq!(
        overlay_of(&s)["models"]["m"]["settings"]["ctx-size"],
        json!("8192")
    );
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(engines["data"][0]["deferred"], json!(["m"]));
    assert_eq!(engines["data"][0]["pending_apply"], json!(true));
    let unloads_of_m = |stub: &Stub| {
        stub.calls()
            .iter()
            .filter(|c| *c == "reload unloaded m")
            .count()
    };
    // Once by the first write (idle then), never while it is busy.
    assert_eq!(unloads_of_m(&s.stub), 1, "{:?}", s.stub.calls());
    assert_eq!(slow.await.unwrap(), 200);
    // Idle: applied by itself.
    wait_for(&s, |_| reloads(&s.stub) == reloads_before + 2).await;
    assert_eq!(
        overlay_of(&s)["models"]["m"]["settings"]["ctx-size"],
        json!("16384")
    );
    assert_eq!(unloads_of_m(&s.stub), 2, "{:?}", s.stub.calls());
    let busy = s.stub.inner.lock().unwrap().busy_at_reload.clone();
    assert_eq!(busy.last(), Some(&0), "{busy:?}");
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(engines["data"][0]["pending_apply"], json!(false));
}

/// A settings command that fails while the machine sleeps leaves the apply
/// pending; the request that wakes the machine gets it applied before it
/// reaches the engine, so the model serves with the new settings.
#[tokio::test]
async fn a_pending_apply_runs_after_a_wake_before_the_model_serves() {
    // The command works only while the machine is up (as an ssh would).
    let (s, state) = setup_with(Opts {
        up: false,
        lane: "    route_health: true\n    wake_on_request: true\n    wake_wait_secs: 10",
        command_template: Some("test -e {marker} && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    state.capacity.observe_all().await; // the supervisor saw the machine off
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 32768}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(false), "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    assert!(!s.stub.up_marker.exists());
    // This request wakes the machine; the apply runs before it is served.
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(&json!({"model": "text/m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(s.stub.up_marker.exists());
    let calls = s.stub.calls();
    let reload = calls.iter().position(|c| c == "GET /models?reload=1");
    let chat = calls.iter().position(|c| c == "POST /v1/chat/completions");
    assert!(
        reload.is_some() && reload < chat,
        "the request reached the engine before the apply: {calls:?}"
    );
    let overlay: Value =
        serde_json::from_str(&std::fs::read_to_string(&s.overlay).unwrap()).unwrap();
    assert_eq!(
        overlay["models"]["m"]["settings"]["ctx-size"],
        json!("32768")
    );
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(
        engines["data"][0]["pending_apply"],
        json!(false),
        "{engines}"
    );
}

/// A variant's fit cannot be checked (no measurement, no estimate): a
/// client key gets 409 `fit_unknown` saying how to fix it; an operator key
/// may go ahead, and the answer says `fit=unchecked`.
#[tokio::test]
async fn an_unchecked_fit_is_refused_for_clients_and_marked_for_operators() {
    let extra = r#"
tenants:
  - id: acme
  - id: ops
api_keys:
  - { key: "sk-admin", tenant: ops, role: admin }
  - { key: "sk-client", tenant: acme, role: client }
"#;
    let s = setup(true, extra, None).await;
    with_policy(&s, Some("sk-admin")).await;
    s.stub.inner.lock().unwrap().unmeasured = true;
    let (status, body, _) = chat_with_load(&s, Some("sk-client"), json!({"ctx_size": 65536})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["type"], json!("fit_unknown"));
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("estimate"),
        "{body}"
    );
    let (status, body, settings) =
        chat_with_load(&s, Some("sk-admin"), json!({"ctx_size": 65536})).await;
    assert_eq!(status, 200, "{body}");
    assert!(settings.unwrap().contains("fit=unchecked"));
}

/// Revert goes through the PUT path: it needs the edited revision (428
/// without, 409 stale) and applies the reverted load settings.
#[tokio::test]
async fn revert_needs_a_revision_and_applies_like_a_put() {
    let s = setup(true, "", None).await;
    assert_eq!(
        put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}}))
            .await
            .0,
        200
    );
    assert_eq!(
        put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}}))
            .await
            .0,
        200
    );
    let revert = |revision: Option<&str>| {
        let mut request = reqwest::Client::new()
            .post(format!("{}/v1/models/text/m/settings/revert", s.gw))
            .json(&json!({"revision": 1}));
        if let Some(revision) = revision {
            request = request.header("if-match", revision);
        }
        request.send()
    };
    let r = revert(None).await.unwrap();
    assert_eq!(r.status(), 428);
    let r = revert(Some("1")).await.unwrap();
    assert_eq!(r.status(), 409);
    let r = revert(Some("\"2\"")).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["etag"], "\"3\"");
    let view: Value = r.json().await.unwrap();
    assert_eq!(view["revision"], json!(3));
    assert_eq!(view["apply"]["ok"], json!(true), "{view}");
    assert_eq!(
        overlay_of(&s)["models"]["m"]["settings"]["ctx-size"],
        json!("8192")
    );
}
