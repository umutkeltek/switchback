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
        Some(_) => "GET /models?reload=1".to_string(),
        None => "GET /models".to_string(),
    };
    inner.calls.push(call);
    let data: Vec<Value> = inner
        .status
        .iter()
        .map(|(id, status)| {
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
    let mut inner = stub.inner.lock().unwrap();
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
    }
    if slow {
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
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

async fn setup(up: bool, engine_extra: &str, settings_command: Option<String>) -> Setup {
    let (stub_url, stub) = spawn_stub(up).await;
    let overlay = temp("overlay.json");
    let command = settings_command.unwrap_or_else(|| format!("cat > {}", quoted(&overlay)));
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
    engine:
      kind: llama_cpp_router
      settings_command: "{command}"
      apply_wait_secs: 1
      load_timeout_secs: 5
{engine_extra}
"#,
        marker = quoted(&stub.up_marker),
        command = command.replace('"', "\\\""),
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
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Setup {
        gw: format!("http://{addr}"),
        stub,
        overlay,
    }
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
    let calls = s.stub.engine_calls();
    assert!(
        calls.contains(&"GET /models?reload=1".to_string()),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| c.starts_with("POST")),
        "next_load reloaded a model: {calls:?}"
    );

    // The engines view refreshes the listing; the loaded model still runs
    // its old arguments, so its load settings are pending until reload.
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
    assert_eq!(m["pending"], json!(true));
    assert_eq!(m["memory"]["min"]["bytes"], json!(1110));
    assert_eq!(m["memory"]["min"]["host_bytes"], json!(6));
    let view = get_json(format!("{}/v1/models/text/m/settings", s.gw)).await;
    assert_eq!(view["targets"][0]["load"]["pending"], json!(true));
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
    let calls = s.stub.engine_calls();
    let unload = calls.iter().position(|c| c == "POST /models/unload m");
    let load = calls.iter().position(|c| c == "POST /models/load m");
    assert!(unload.is_some() && load > unload, "{calls:?}");
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
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["error"]["type"], json!("model_busy"));
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 8192}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 409, "{view}");
    assert_eq!(
        view["revision"],
        json!(1),
        "the settings were still written"
    );
    assert!(
        !s.stub.calls().iter().any(|c| c.contains("unload")),
        "a busy model was unloaded"
    );
    assert_eq!(slow.await.unwrap(), 200);
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
    ] {
        let (status, view) = put(&s.gw, "text/m", 0, body.clone()).await;
        assert_eq!(status, 400, "{body}: {view}");
        let message = view["error"]["message"].as_str().unwrap();
        assert!(message.contains(needle), "{body}: {message}");
    }
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
