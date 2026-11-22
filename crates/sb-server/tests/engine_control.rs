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
use futures::StreamExt;
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
    /// Like llama.cpp's router: an unload is answered at once and the
    /// model's process exits this much later; until then the listing says
    /// `loaded` and a load gets 400 "model is already running".
    exit_delay_ms: u64,
    /// Models whose process is exiting, and when it is gone.
    exiting: BTreeMap<String, std::time::Instant>,
    /// Loads refused because the model still ran.
    refused_loads: Vec<String>,
    /// Each loaded model's command line (`status.args`): its preset
    /// section as the router read it when it loaded the model.
    args: BTreeMap<String, Vec<String>>,
    /// An engine that keeps running the old settings: a load leaves the
    /// model's command line as it was.
    keep_old_args: bool,
    /// The overlay JSON the settings command writes; on a re-read the
    /// router compares each model's section with the one it read last and,
    /// like llama.cpp, unloads a running model whose section changed.
    overlay_path: Option<PathBuf>,
    presets: BTreeMap<String, Value>,
    /// The engine has not measured any model.
    unmeasured: bool,
    /// How long a "slow please" chat takes (ms; 0 = 1500).
    slow_ms: u64,
    /// Like a router started with `--api-key`: every call but `/health`
    /// must carry this header (name, value) or gets 401.
    require: Option<(String, String)>,
    /// Every request the router saw, with its credential headers.
    seen: Vec<Seen>,
    /// Like llama.cpp's router (0 = off): a chat for a model that is not
    /// running loads it first (`loading` for this long), and fails when
    /// that load is ended under it; a chat that meets a process that is
    /// exiting fails too.
    autoload_ms: u64,
    /// Chats that failed for one of those reasons.
    failed_chats: Vec<String>,
    /// A plain `GET /models` (not a re-read) answers this much later.
    listing_delay_ms: u64,
    /// How often each model's process was stopped: a chat's own load fails
    /// when this changed while it loaded.
    stops: BTreeMap<String, u64>,
    /// How long a load takes (the model says `loading` meanwhile).
    load_delay_ms: u64,
}

#[derive(Clone, Debug)]
struct Seen {
    /// Path and query, e.g. "/models?reload=1".
    path: String,
    authorization: Option<String>,
    api_key: Option<String>,
    refused: bool,
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

/// Processes whose exit time passed are gone.
fn settle(inner: &mut Router_) {
    let now = std::time::Instant::now();
    let gone: Vec<String> = inner
        .exiting
        .iter()
        .filter(|(_, at)| **at <= now)
        .map(|(id, _)| id.clone())
        .collect();
    for id in gone {
        inner.exiting.remove(&id);
        inner.status.insert(id, "unloaded".to_string());
    }
}

/// Stop a model's process: at once, or after `exit_delay_ms` (the
/// listing says `loaded` until then).
fn stop(inner: &mut Router_, id: &str) {
    *inner.stops.entry(id.to_string()).or_default() += 1;
    if inner.exit_delay_ms == 0 {
        inner.status.insert(id.to_string(), "unloaded".to_string());
    } else if !inner.exiting.contains_key(id) {
        let at = std::time::Instant::now() + Duration::from_millis(inner.exit_delay_ms);
        inner.exiting.insert(id.to_string(), at);
    }
}

/// Record each request's credential headers; refuse one without the
/// required key like llama.cpp's `--api-key` (`/health` stays open).
async fn auth_gate(
    State(stub): State<Stub>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // A block, so nothing borrowing the request lives across the await.
    let seen = {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let required = stub.inner.lock().unwrap().require.clone();
        Seen {
            path: request
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default(),
            authorization: header("authorization"),
            api_key: header("x-api-key"),
            refused: required.is_some_and(|(name, value)| {
                request.uri().path() != "/health"
                    && header(&name).as_deref() != Some(value.as_str())
            }),
        }
    };
    let refused = seen.refused;
    stub.inner.lock().unwrap().seen.push(seen);
    if refused {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": {"code": 401, "message": "Invalid API Key",
                                  "type": "authentication_error"}})),
        )
            .into_response();
    }
    next.run(request).await
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
    let delay = stub.inner.lock().unwrap().listing_delay_ms;
    if delay > 0 && !query.contains_key("reload") {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let mut inner = stub.inner.lock().unwrap();
    settle(&mut inner);
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
                    stop(&mut inner, &id);
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
            let args = inner.args.get(id).map_or_else(
                || json!(["llama-server", "--ctx-size", "4096"]),
                |args| json!(args),
            );
            json!({
                "id": id,
                "status": {"value": status, "args": args},
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

async fn load(State(stub): State<Stub>, Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or("").to_string();
    let delay = {
        let mut inner = stub.inner.lock().unwrap();
        settle(&mut inner);
        inner.calls.push(format!("POST /models/load {model}"));
        let running = matches!(
            inner.status.get(&model).map(String::as_str),
            Some("loaded" | "loading")
        );
        if running {
            inner.refused_loads.push(model);
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": {"code": 400, "message": "model is already running",
                                      "type": "invalid_request_error"}}),
                ),
            )
                .into_response();
        }
        if inner.load_delay_ms > 0 {
            inner.status.insert(model.clone(), "loading".to_string());
        }
        inner.load_delay_ms
    };
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let mut inner = stub.inner.lock().unwrap();
    start_process(&mut inner, &model);
    inner.status.insert(model, "loaded".to_string());
    Json(json!({"success": true})).into_response()
}

/// A model's new process: its command line from the preset the router
/// read last.
fn start_process(inner: &mut Router_, model: &str) {
    if inner.keep_old_args {
        return;
    }
    let mut flags = BTreeMap::from([("ctx-size".to_string(), "4096".to_string())]);
    if let Some(settings) = inner
        .presets
        .get(model)
        .and_then(|preset| preset["settings"].as_object())
    {
        for (key, value) in settings {
            flags.insert(key.clone(), value.as_str().unwrap_or_default().to_string());
        }
    }
    let mut args = vec!["llama-server".to_string()];
    for (key, value) in flags {
        args.push(format!("--{key}"));
        args.push(value);
    }
    inner.args.insert(model.to_string(), args);
}

/// With `autoload_ms`: make sure the chat's model runs, as llama.cpp's
/// router does: load it when it does not, fail when its process is exiting
/// or its load is ended under it. `Err` is the failure answered.
async fn autoload(stub: &Stub, model: &str) -> Result<(), Box<Response>> {
    let fail = |stub: &Stub, why: &str| {
        stub.inner
            .lock()
            .unwrap()
            .failed_chats
            .push(format!("{model}: {why}"));
        Err(Box::new(
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"code": 500, "message": why, "type": "server_error"}})),
            )
                .into_response(),
        ))
    };
    let (ms, stops) = {
        let mut inner = stub.inner.lock().unwrap();
        settle(&mut inner);
        if inner.autoload_ms == 0 {
            return Ok(());
        }
        let stops = inner.stops.get(model).copied().unwrap_or(0);
        if inner.exiting.contains_key(model) {
            drop(inner);
            return fail(stub, "the model's process is exiting");
        }
        match inner.status.get(model).map(String::as_str) {
            Some("loaded" | "sleeping") => return Ok(()),
            Some("loading") => {}
            _ => {
                inner
                    .status
                    .insert(model.to_string(), "loading".to_string());
                inner.calls.push(format!("autoload {model}"));
            }
        }
        (inner.autoload_ms, stops)
    };
    tokio::time::sleep(Duration::from_millis(ms)).await;
    let mut inner = stub.inner.lock().unwrap();
    settle(&mut inner);
    if inner.stops.get(model).copied().unwrap_or(0) != stops {
        drop(inner);
        return fail(stub, "the model's load was ended");
    }
    match inner.status.get(model).map(String::as_str) {
        Some("loading") if !inner.exiting.contains_key(model) => {
            start_process(&mut inner, model);
            inner.status.insert(model.to_string(), "loaded".to_string());
            Ok(())
        }
        Some("loaded") if !inner.exiting.contains_key(model) => Ok(()),
        _ => {
            drop(inner);
            fail(stub, "the model's load was ended")
        }
    }
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
    settle(&mut inner);
    stop(&mut inner, &model);
    Json(json!({"success": true}))
}

async fn chat(State(stub): State<Stub>, body: Bytes) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap();
    let slow = body.to_string().contains("slow please");
    if let Err(failed) = autoload(&stub, body["model"].as_str().unwrap_or_default()).await {
        return *failed;
    }
    if body["stream"] == json!(true) {
        // A streamed answer: 40 chunks, 50 ms apart.
        stub.inner
            .lock()
            .unwrap()
            .calls
            .push("POST /v1/chat/completions stream".to_string());
        let chunks = (0..40)
            .map(|i| {
                format!(
                    "data: {{\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"t{i}\"}},\"finish_reason\":null}}]}}\n\n"
                )
            })
            .chain(std::iter::once("data: [DONE]\n\n".to_string()));
        let stream = futures::stream::iter(chunks).then(|chunk| async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            Ok::<_, std::io::Error>(Bytes::from(chunk))
        });
        return (
            [("content-type", "text/event-stream")],
            axum::body::Body::from_stream(stream),
        )
            .into_response();
    }
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

/// An embeddings or audio call that stays in the engine for 1.2 s.
async fn slow_engine_call(stub: &Stub, call: &str) {
    {
        let mut inner = stub.inner.lock().unwrap();
        inner.calls.push(call.to_string());
        inner.busy += 1;
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    stub.inner.lock().unwrap().busy -= 1;
}

async fn embeddings(State(stub): State<Stub>, _body: Bytes) -> Json<Value> {
    slow_engine_call(&stub, "POST /v1/embeddings").await;
    Json(json!({"object": "list", "model": "m",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
        "usage": {"prompt_tokens": 1, "total_tokens": 1}}))
}

async fn transcription(State(stub): State<Stub>, _body: Bytes) -> Json<Value> {
    slow_engine_call(&stub, "POST /v1/audio/transcriptions").await;
    Json(json!({"text": "hello"}))
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

async fn spawn_stub(up: bool, require: Option<(&str, &str)>) -> (String, Stub) {
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
    {
        let mut inner = stub.inner.lock().unwrap();
        inner.total = 24u64 << 30;
        inner.require = require.map(|(name, value)| (name.to_string(), value.to_string()));
    }
    let app = Router::new()
        .route("/health", get(health))
        .route("/models", get(models))
        .route("/models/load", post(load))
        .route("/models/unload", post(unload))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/audio/transcriptions", post(transcription))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            stub.clone(),
            auth_gate,
        ))
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
    /// line, indented like the lane's keys. `{marker}` is the machine-up marker.
    lane: &'a str,
    apply_wait_secs: u64,
    /// The engine's `load_timeout_secs` (default 5).
    load_timeout_secs: u64,
    /// A settings command writing the overlay JSON, with `{marker}` (the
    /// machine-up marker) and `{overlay}` filled in.
    command_template: Option<&'a str>,
    /// The lane's wake command, with `{marker}` filled in (default: touch it).
    wake_template: Option<&'a str>,
    /// More `server:` keys, each on its own line, indented two spaces.
    server: &'a str,
    /// The `text` provider's credential keys, each on its own line,
    /// indented like its keys (default: an `accounts:` entry with key "k").
    text_auth: Option<&'a str>,
    /// The fake router requires this header (name, value), like
    /// llama.cpp's `--api-key`.
    require: Option<(&'a str, &'a str)>,
}

impl Default for Opts<'_> {
    fn default() -> Self {
        Opts {
            up: true,
            extra: "",
            settings_command: None,
            lane: "",
            apply_wait_secs: 1,
            load_timeout_secs: 5,
            command_template: None,
            wake_template: None,
            server: "",
            text_auth: None,
            require: None,
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
    let (stub_url, stub) = spawn_stub(opts.up, opts.require).await;
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
{server}
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "{stub_url}/v1"
    capabilities: {{ audio_in: true }}
{text_auth}
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
    wake_command: "{wake}"
    boot_timeout_secs: 10
    health_poll_interval_ms: 50
{lane}
    engine:
      kind: llama_cpp_router
      settings_command: "{command}"
      apply_wait_secs: {wait}
      load_timeout_secs: {load_timeout}
{extra}
"#,
        wake = opts
            .wake_template
            .unwrap_or("touch {marker}")
            .replace("{marker}", &quoted(&stub.up_marker)),
        command = command.replace('"', "\\\""),
        lane = opts.lane.replace("{marker}", &quoted(&stub.up_marker)),
        server = opts.server,
        wait = opts.apply_wait_secs,
        load_timeout = opts.load_timeout_secs,
        extra = opts.extra,
        text_auth = opts.text_auth.unwrap_or(
            "    accounts:\n      - id: a\n        auth: { kind: api_key, inline: \"k\" }"
        ),
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
    assert_eq!(view["apply"]["ok"], json!(true), "{view}");
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
async fn apply_now_loads_only_after_the_old_process_ended() {
    // A real router answers the unload at once and ends the model's process
    // later; a load before then is refused (400 "model is already running")
    // and the new load setting never takes effect.
    let s = setup(true, "", None).await;
    s.stub.inner.lock().unwrap().exit_delay_ms = 400;
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"flash_attn": true}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(true), "{view}");
    assert_eq!(view["apply"]["reloaded_model"], json!(true), "{view}");
    let inner = s.stub.inner.lock().unwrap();
    assert!(
        inner.refused_loads.is_empty(),
        "a load reached the engine while the old process ran: {:?}",
        inner.calls
    );
    assert_eq!(inner.status["m"], "loaded", "{:?}", inner.calls);
    assert!(inner.exiting.is_empty(), "{:?}", inner.calls);
}

#[tokio::test]
async fn unload_answers_once_the_model_process_ended() {
    let s = setup(true, "", None).await;
    s.stub.inner.lock().unwrap().exit_delay_ms = 300;
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        s.stub.inner.lock().unwrap().status["m"],
        "unloaded",
        "the unload answered while the model's process still ran"
    );
    // So a load right after it is not refused.
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert!(s.stub.inner.lock().unwrap().refused_loads.is_empty());
}

#[tokio::test]
async fn a_load_after_a_pending_apply_waits_for_the_old_process() {
    // The settings command fails at first, so the write stays pending for
    // the lane; `sb ai load` then runs the apply, whose re-read makes the
    // router end the model's process in the background. A load sent
    // before that end is refused (400 "model is already running") and the
    // old settings would count as loaded.
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.exit_delay_ms = 400;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"flash_attn": true}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    std::fs::write(format!("{}.go", s.overlay.display()), b"go").unwrap();
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let inner = s.stub.inner.lock().unwrap();
    assert!(
        inner.refused_loads.is_empty(),
        "the load reached the engine while the old process ran: {:?}",
        inner.calls
    );
    assert!(inner.exiting.is_empty(), "{:?}", inner.calls);
    assert_eq!(inner.status["m"], "loaded", "{:?}", inner.calls);
    let args = inner.args["m"].join(" ");
    assert!(args.contains("--flash-attn on"), "{args}");
}

#[tokio::test]
async fn an_engine_that_keeps_the_old_settings_fails_the_apply() {
    // The model loads again, but its command line still has the old
    // settings: the apply says so instead of reporting them applied.
    let s = setup(true, "", None).await;
    s.stub.inner.lock().unwrap().keep_old_args = true;
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 32768, "flash_attn": true}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(false), "{view}");
    let error = view["apply"]["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("the engine still runs the old settings"),
        "{view}"
    );
    assert!(error.contains("ctx-size 32768 (it runs 4096)"), "{error}");
    assert!(
        error.contains("flash-attn on (it runs without it)"),
        "{error}"
    );
}

#[tokio::test]
async fn loading_a_model_that_runs_is_not_an_error() {
    let s = setup(true, "", None).await;
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], json!("loaded"));
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

/// A load setting reverted to revision 0 is applied exactly like an unset
/// of it (a PUT without it): the same overlay reaches the settings command,
/// so the engine's config returns to its base, and the same apply runs.
#[tokio::test]
async fn revert_to_zero_applies_like_an_unset_of_the_load_settings() {
    async fn after(clear: &str) -> (Value, Value, Vec<String>) {
        let s = setup(true, "", None).await;
        let (status, _) = put(
            &s.gw,
            "text/m",
            0,
            json!({"load": {"ctx_size": 8192}, "locked": ["load.ctx_size"]}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            overlay_of(&s)["models"]["m"]["settings"]["ctx-size"],
            json!("8192")
        );
        let before = s.stub.calls().len();
        let (status, view) = if clear == "unset" {
            put(&s.gw, "text/m", 1, json!({})).await
        } else {
            let r = reqwest::Client::new()
                .post(format!("{}/v1/models/text/m/settings/revert", s.gw))
                .header("if-match", "1")
                .json(&json!({"revision": 0}))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json().await.unwrap())
        };
        assert_eq!(status, 200, "{clear}: {view}");
        assert_eq!(view["revision"], json!(2), "{clear}: {view}");
        assert_eq!(view["document"], json!({}), "{clear}: {view}");
        // The message names the revision and `at_ms` the time; the rest
        // is what the engine did.
        let mut apply = view["apply"].clone();
        if let Some(apply) = apply.as_object_mut() {
            apply.remove("message");
            apply.remove("at_ms");
        }
        let calls = s.stub.calls()[before..].to_vec();
        (overlay_of(&s), apply, calls)
    }
    let (unset_overlay, unset_apply, unset_calls) = after("unset").await;
    let (revert_overlay, revert_apply, revert_calls) = after("revert").await;
    assert_eq!(unset_apply["ok"], json!(true), "{unset_apply}");
    assert_eq!(unset_apply["mode"], json!("next_load"), "{unset_apply}");
    assert!(
        unset_overlay["models"]["m"]["settings"]
            .get("ctx-size")
            .is_none(),
        "{unset_overlay}"
    );
    assert_eq!(revert_overlay, unset_overlay);
    assert_eq!(revert_apply, unset_apply);
    assert_eq!(revert_calls, unset_calls);
}

/// A settings command that refuses a change exits non-zero and prints its
/// reason on stdout (stderr empty). The API's apply error and the message
/// `sb ai set ... --load` prints carry that reason, not a bare "exit 2:".
#[tokio::test]
async fn a_refused_settings_change_shows_the_commands_reason() {
    let command = "cat >/dev/null; echo 'refused: key X is pinned'; exit 2".to_string();
    let (s, _) = setup_with(Opts {
        settings_command: Some(command),
        ..Opts::default()
    })
    .await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(false), "{view}");
    let error = view["apply"]["error"].as_str().unwrap_or_default();
    assert!(error.contains("exit 2: refused: key X is pinned"), "{view}");
    assert!(
        view["apply"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("refused: key X is pinned"),
        "{view}"
    );

    // The same through the CLI.
    let cli = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../cli/ai_gateway.py");
    let home = temp("cli-home");
    std::fs::create_dir_all(&home).unwrap();
    let run = tokio::process::Command::new("python3")
        .arg("-B")
        .arg(&cli)
        .args(["set", "text/m", "ctx_size=16384", "--load"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &home)
        .env("SWITCHBACK_AI_URL", &s.gw)
        .output()
        .await
        .expect("python3 runs the CLI");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert!(run.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains("refused: key X is pinned"),
        "stdout: {stdout} stderr: {stderr}"
    );
}

fn spawn_embedding(gw: &str) -> tokio::task::JoinHandle<u16> {
    let gw = gw.to_string();
    tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{gw}/v1/embeddings"))
            .json(&json!({"model": "text/m", "input": "hello"}))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    })
}

fn spawn_transcription(gw: &str) -> tokio::task::JoinHandle<u16> {
    const BOUNDARY: &str = "engine-control-boundary";
    let mut body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ntext/m\r\n\
         --{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
         Content-Type: audio/wav\r\n\r\n"
    )
    .into_bytes();
    body.extend(b"RIFF-AUDIO");
    body.extend(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    let gw = gw.to_string();
    tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("{gw}/v1/audio/transcriptions"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    })
}

/// An embeddings request in flight on `m` holds off an unload of `m`.
#[tokio::test]
async fn rr_unload_waits_for_an_embeddings_request() {
    let (s, _) = setup_with(Opts {
        apply_wait_secs: 10,
        ..Opts::default()
    })
    .await;
    let emb = spawn_embedding(&s.gw);
    wait_for_chat(&s).await;
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(emb.await.unwrap(), 200);
    assert_eq!(status, 200, "{body}");
    let busy = s.stub.inner.lock().unwrap().busy_at_unload.clone();
    assert_eq!(
        busy,
        vec![0],
        "the unload reached the router with an embeddings request in flight on m"
    );
}

/// An embeddings request in flight on `m` holds off a preset re-read of `m`.
#[tokio::test]
async fn rr_reload_waits_for_an_embeddings_request() {
    let s = setup(true, "", None).await;
    let emb = spawn_embedding(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 65536}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(emb.await.unwrap(), 200);
    let calls = s.stub.calls();
    let busy = s.stub.inner.lock().unwrap().busy_at_reload.clone();
    assert!(
        busy.iter().all(|b| *b == 0) && !calls.contains(&"reload unloaded m".to_string()),
        "reload=1 reached the router with {busy:?} embeddings request(s) in flight: {calls:?}"
    );
}

/// An audio transcription in flight on `m` holds off an unload of `m`.
#[tokio::test]
async fn unload_waits_for_an_audio_transcription() {
    let (s, _) = setup_with(Opts {
        apply_wait_secs: 10,
        ..Opts::default()
    })
    .await;
    let audio = spawn_transcription(&s.gw);
    wait_for_chat(&s).await;
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(audio.await.unwrap(), 200);
    assert_eq!(status, 200, "{body}");
    let busy = s.stub.inner.lock().unwrap().busy_at_unload.clone();
    assert_eq!(
        busy,
        vec![0],
        "the unload reached the router with a transcription in flight on m"
    );
}

/// An audio transcription in flight on `m` holds off a preset re-read of `m`.
#[tokio::test]
async fn reload_waits_for_an_audio_transcription() {
    let s = setup(true, "", None).await;
    let audio = spawn_transcription(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 65536}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(audio.await.unwrap(), 200);
    let calls = s.stub.calls();
    let busy = s.stub.inner.lock().unwrap().busy_at_reload.clone();
    assert!(
        busy.iter().all(|b| *b == 0) && !calls.contains(&"reload unloaded m".to_string()),
        "reload=1 reached the router with {busy:?} transcription(s) in flight: {calls:?}"
    );
}

/// While `text/m`'s gate is closed (an unload in progress), a request on a
/// route with a healthy second target falls back to it at once instead of
/// waiting for the gate and failing with 503.
#[tokio::test]
async fn rr_closed_gate_on_first_choice_falls_back_instead_of_503() {
    let (s, _) = setup_with(Opts {
        extra: "routes:\n  - name: r\n    match: { model: r }\n    targets: [\"text/m\", \"other/m\"]\n",
        ..Opts::default()
    })
    .await;
    s.stub.inner.lock().unwrap().unload_delay_ms = 6000;
    let gw = s.gw.clone();
    let unload =
        tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/unload", json!({})).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(&json!({"model": "r", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let route = r
        .headers()
        .get("x-switchback-route")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = r.text().await.unwrap();
    let elapsed = started.elapsed();
    let _ = unload.await.unwrap();
    assert_eq!(
        status,
        200,
        "after {elapsed:?}: {body}; calls {:?}",
        s.stub.calls()
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "waited {elapsed:?} at the gate"
    );
    assert!(route.contains("other/m"), "{route}");
}

async fn in_flight_on_m(gw: &str) -> u64 {
    let engines = get_json(format!("{gw}/v1/engines")).await;
    engines["data"][0]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "m")
        .map(|row| row["in_flight"].as_u64().unwrap_or(0))
        .unwrap_or(0)
}

/// A streamed request counts on `m` until its body ends, and a client that
/// disconnects mid-stream releases the count.
#[tokio::test]
async fn rr_stream_counts_to_body_end_and_disconnect_releases() {
    let s = setup(true, "", None).await;
    let body =
        json!({"model": "text/m", "stream": true, "messages": [{"role": "user", "content": "hi"}]});
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(in_flight_on_m(&s.gw).await, 1, "released at headers");
    let text = r.text().await.unwrap();
    assert!(text.contains("[DONE]"), "{text}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(in_flight_on_m(&s.gw).await, 0, "leaked after a full stream");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(&body)
        .send()
        .await
        .unwrap();
    let mut bytes = r.bytes_stream();
    let _ = bytes.next().await;
    assert_eq!(in_flight_on_m(&s.gw).await, 1);
    drop(bytes);
    let mut left = 1;
    for _ in 0..100 {
        left = in_flight_on_m(&s.gw).await;
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(left, 0, "leaked after a client disconnect");
    let (status, view) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 200, "{view}");
}

/// A lane whose settings command always fails: a request for a model of the
/// lane WITHOUT settings is never delayed by a retry of that apply and never
/// runs the command.
#[tokio::test]
async fn a_failing_apply_never_delays_a_model_without_settings() {
    let runs = temp("runs");
    let command = format!("echo run >> {}; sleep 2; false", quoted(&runs));
    let (s, _) = setup_with(Opts {
        up: false,
        settings_command: Some(command),
        ..Opts::default()
    })
    .await;
    let count = || {
        std::fs::read_to_string(&runs)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(false), "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    assert_eq!(count(), 1);
    // `other` has no settings: its request goes straight to the engine.
    let started = std::time::Instant::now();
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(&json!({"model": "text/other", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r.status(), 200);
    assert!(
        elapsed < Duration::from_millis(1500),
        "a request without settings waited {elapsed:?} for a failing apply"
    );
    assert_eq!(count(), 1, "the request ran the failing settings command");
}

/// A first choice skipped for its provider's budget lets go of its admission
/// at once: an unload of it does not wait for a request served elsewhere.
#[tokio::test]
async fn a_first_choice_skipped_for_budget_drops_its_admission() {
    let (s, _) = setup_with(Opts {
        server: "  budget:\n    per_provider_usd: { text: 0.0 }\n",
        extra: "routes:\n  - name: r\n    match: { model: r }\n    targets: [\"text/m\", \"other/m\"]\n",
        ..Opts::default()
    })
    .await;
    // Served by other/m (text is over its budget), slowly.
    let slow = tokio::spawn({
        let gw = s.gw.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{gw}/v1/chat/completions"))
                .json(&json!({"model": "r", "messages": [{"role": "user", "content": "slow please"}]}))
                .send()
                .await
                .unwrap()
                .status()
                .as_u16()
        }
    });
    wait_for_chat(&s).await;
    assert_eq!(in_flight_on_m(&s.gw).await, 0, "still counted on text/m");
    let (status, view) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(slow.await.unwrap(), 200);
}

/// Settings written while the lane wakes: the engine may already have read
/// its presets, so the write stays pending and the engine re-reads them once
/// the lane is up, before the waking request reaches it.
#[tokio::test]
async fn a_write_on_a_waking_lane_is_re_read_when_it_comes_up() {
    let (s, state) = setup_with(Opts {
        up: false,
        lane: "    route_health: true\n    wake_on_request: true\n    wake_wait_secs: 10",
        wake_template: Some("sleep 1 && touch {marker}"),
        ..Opts::default()
    })
    .await;
    state.capacity.observe_all().await; // the supervisor saw the machine off
    let waking = spawn_chat(&s.gw, "hi");
    let mut lane_state = Value::Null;
    for _ in 0..100 {
        let engines = get_json(format!("{}/v1/engines", s.gw)).await;
        lane_state = engines["data"][0]["state"].clone();
        if lane_state == json!("waking") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 32768}})).await;
    assert_eq!(status, 200, "{view}");
    assert!(
        !s.stub.up_marker.exists(),
        "the lane came up before the write ({lane_state})"
    );
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    assert_eq!(waking.await.unwrap(), 200);
    let calls = s.stub.calls();
    let reload = calls.iter().position(|c| c == "GET /models?reload=1");
    let chat = calls.iter().position(|c| c == "POST /v1/chat/completions");
    assert!(
        reload.is_some() && reload < chat,
        "the engine never re-read the settings written while waking: {calls:?}"
    );
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(
        engines["data"][0]["pending_apply"],
        json!(false),
        "{engines}"
    );
}

// ---- The engine control carries the provider's key --------------------
//
// A llama.cpp router started with `--api-key` refuses every call without
// it (`/health` stays open). The engine control's calls (listing, re-read,
// load, unload, `/props`) must carry the key the provider's text requests
// carry; the lane's health probes keep sending none, as before.

/// The key the fake routers require. A test value, not a secret.
const ROUTER_KEY: &str = "engine-router-test-key";

impl Stub {
    fn seen(&self) -> Vec<Seen> {
        self.inner.lock().unwrap().seen.clone()
    }
    /// The engine-control calls the router saw (everything but `/health`
    /// and the text requests under `/v1/`).
    fn seen_engine_calls(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|seen| seen.path != "/health" && !seen.path.starts_with("/v1/"))
            .collect()
    }
}

fn model_ids(engines: &Value) -> Vec<String> {
    engines["data"][0]["models"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn engine_calls_carry_the_providers_bearer_key() {
    const ENV: &str = "SB_TEST_ENGINE_CONTROL_BEARER_KEY";
    std::env::set_var(ENV, ROUTER_KEY);
    let bearer = format!("Bearer {ROUTER_KEY}");
    let (s, _) = setup_with(Opts {
        text_auth: Some("    api_key_env: SB_TEST_ENGINE_CONTROL_BEARER_KEY"),
        require: Some(("authorization", &bearer)),
        ..Opts::default()
    })
    .await;

    // The listing (and `/props`) through `GET /v1/engines`.
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    let lane = &engines["data"][0];
    assert_eq!(lane["state"], json!("up"), "{engines}");
    assert_eq!(model_ids(&engines), vec!["m", "other"], "{engines}");
    assert!(lane["error"].is_null(), "{lane}");

    // A load.
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/unload", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");

    // An apply now: the re-read, then the unload and load of the model.
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 32768}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(true), "{view}");
    assert_eq!(view["apply"]["reloaded_model"], json!(true), "{view}");

    let calls = s.stub.calls();
    for call in [
        "GET /models",
        "POST /models/unload m",
        "POST /models/load m",
        "GET /models?reload=1",
    ] {
        assert!(calls.contains(&call.to_string()), "no {call}: {calls:?}");
    }
    let engine_calls = s.stub.seen_engine_calls();
    for path in [
        "/props",
        "/models",
        "/models?reload=1",
        "/models/load",
        "/models/unload",
    ] {
        assert!(
            engine_calls.iter().any(|seen| seen.path == path),
            "the engine control never called {path}: {engine_calls:?}"
        );
    }
    for seen in &engine_calls {
        assert!(!seen.refused, "the router refused {seen:?}");
        assert_eq!(
            seen.authorization.as_deref(),
            Some(bearer.as_str()),
            "{seen:?}"
        );
    }
}

#[tokio::test]
async fn engine_calls_use_the_providers_header_scheme() {
    const ENV: &str = "SB_TEST_ENGINE_CONTROL_HEADER_KEY";
    std::env::set_var(ENV, ROUTER_KEY);
    let (s, _) = setup_with(Opts {
        text_auth: Some(
            "    api_key_env: SB_TEST_ENGINE_CONTROL_HEADER_KEY\n    auth_scheme: { kind: header, name: x-api-key }",
        ),
        require: Some(("x-api-key", ROUTER_KEY)),
        ..Opts::default()
    })
    .await;

    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(model_ids(&engines), vec!["m", "other"], "{engines}");
    let (status, body) = post_to(&s.gw, "/v1/models/text/other/load", json!({})).await;
    assert_eq!(status, 200, "{body}");

    let engine_calls = s.stub.seen_engine_calls();
    assert!(
        engine_calls.iter().any(|seen| seen.path == "/models/load"),
        "{engine_calls:?}"
    );
    for seen in &engine_calls {
        assert!(!seen.refused, "the router refused {seen:?}");
        assert_eq!(seen.api_key.as_deref(), Some(ROUTER_KEY), "{seen:?}");
        assert_eq!(seen.authorization, None, "{seen:?}");
    }
}

#[tokio::test]
async fn a_provider_without_a_key_sends_no_credential_to_the_engine() {
    let (s, _) = setup_with(Opts {
        text_auth: Some(""),
        ..Opts::default()
    })
    .await;

    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    assert_eq!(model_ids(&engines), vec!["m", "other"], "{engines}");
    let (status, body) = post_to(&s.gw, "/v1/models/text/other/load", json!({})).await;
    assert_eq!(status, 200, "{body}");

    let engine_calls = s.stub.seen_engine_calls();
    assert!(
        engine_calls.iter().any(|seen| seen.path == "/models/load"),
        "{engine_calls:?}"
    );
    for seen in &engine_calls {
        assert_eq!(seen.authorization, None, "{seen:?}");
        assert_eq!(seen.api_key, None, "{seen:?}");
    }
}

#[tokio::test]
async fn health_probes_of_an_openai_compatible_lane_still_send_no_key() {
    const ENV: &str = "SB_TEST_ENGINE_CONTROL_PROBE_KEY";
    std::env::set_var(ENV, ROUTER_KEY);
    let bearer = format!("Bearer {ROUTER_KEY}");
    // Asleep at first: a load with `wake` wakes the lane, and the
    // activator probes `/health` until it answers.
    let (s, _) = setup_with(Opts {
        up: false,
        text_auth: Some("    api_key_env: SB_TEST_ENGINE_CONTROL_PROBE_KEY"),
        require: Some(("authorization", &bearer)),
        ..Opts::default()
    })
    .await;

    let (status, body) = post_to(&s.gw, "/v1/models/text/other/load", json!({"wake": true})).await;
    assert_eq!(status, 200, "{body}");
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", s.gw))
        .json(&json!({"model": "text/m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let seen = s.stub.seen();
    let probes: Vec<&Seen> = seen.iter().filter(|seen| seen.path == "/health").collect();
    assert!(!probes.is_empty(), "no health probe: {seen:?}");
    for probe in probes {
        assert_eq!(probe.authorization, None, "{probe:?}");
        assert_eq!(probe.api_key, None, "{probe:?}");
    }
    // The text request itself carries the key, as before.
    let chat = seen
        .iter()
        .find(|seen| seen.path == "/v1/chat/completions")
        .expect("the chat reached the router");
    assert_eq!(chat.authorization.as_deref(), Some(bearer.as_str()));
}

// ---------------------------------------------------------------- reviewer follow-ups

/// The preset section has both a load setting and a raw engine argument
/// for one flag; the raw one wins (the overlay writes it last), and the
/// check of the reloaded model expects that value, not the load setting's.
#[tokio::test]
async fn an_apply_checks_the_arguments_the_preset_was_given() {
    let s = setup(true, "", None).await;
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 8192}, "engine_args": {"ctx-size": 16384}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(
        overlay_of(&s)["models"]["m"]["settings"]["ctx-size"],
        json!("16384")
    );
    assert_eq!(view["apply"]["ok"], json!(true), "{view}");
    assert_eq!(view["apply"]["reloaded_model"], json!(true), "{view}");
    let args = s.stub.inner.lock().unwrap().args["m"].join(" ");
    assert!(args.contains("--ctx-size 16384"), "{args}");
}

/// An engine that keeps the old settings after a reload: the message says
/// so and does not promise a retry (nothing is pending to retry).
#[tokio::test]
async fn kept_old_settings_get_their_own_message() {
    let s = setup(true, "", None).await;
    s.stub.inner.lock().unwrap().keep_old_args = true;
    let (status, view) = put(
        &s.gw,
        "text/m",
        0,
        json!({"load": {"ctx_size": 32768}, "apply": "now"}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["ok"], json!(false), "{view}");
    assert_eq!(view["apply"]["reason"], json!("old_settings"), "{view}");
    let message = view["apply"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("reloaded, but"), "{message}");
    assert!(
        message.contains("ctx-size 32768 (it runs 4096)"),
        "{message}"
    );
    assert!(!message.contains("retried"), "{message}");
}

/// A hold's preload runs a pending apply that rewrites the model while a
/// request waits for it. The request must not be admitted between the
/// re-read and the preload's own load: it would start loading the model
/// and the preload would end that load under it.
#[tokio::test]
async fn a_request_racing_a_hold_preload_through_a_pending_apply_is_served() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && sleep 0.4 && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.autoload_ms = 600;
        inner.listing_delay_ms = 150;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    std::fs::write(format!("{}.go", s.overlay.display()), b"go").unwrap();
    let (status, body) = post_to(
        &s.gw,
        "/v1/hosts/text/holds",
        json!({"ttl_secs": 600, "reason": "eval run", "load": "text/m"}),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    // The preload lists the models twice (its wake check, then the model's
    // row from before the apply), and only then runs its apply, whose
    // settings command takes 0.4 s: the request comes in meanwhile.
    wait_for(&s, |calls| {
        calls.iter().filter(|call| *call == "GET /models").count() >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let chat = spawn_chat(&s.gw, "hi");
    assert_eq!(chat.await.unwrap(), 200, "{:?}", s.stub.calls());
    wait_for(&s, |calls| {
        calls.contains(&"POST /models/load m".to_string())
    })
    .await;
    let inner = s.stub.inner.lock().unwrap();
    assert!(
        inner.failed_chats.is_empty(),
        "{:?} {:?}",
        inner.failed_chats,
        inner.calls
    );
    assert!(
        inner.args["m"].join(" ").contains("--ctx-size 8192"),
        "{:?}",
        inner.args
    );
}

/// A load whose pending apply fails again leaves the model running with
/// its old settings: it is not counted as loaded with the new revision.
#[tokio::test]
async fn a_load_whose_apply_failed_again_stays_pending() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    s.stub.inner.lock().unwrap().overlay_path = Some(s.overlay.clone());
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let view = get_json(format!("{}/v1/models/text/m/settings", s.gw)).await;
    assert_eq!(view["targets"][0]["load"]["pending"], json!(true), "{view}");
    let engines = get_json(format!("{}/v1/engines", s.gw)).await;
    let m = engines["data"][0]["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "m")
        .unwrap()
        .clone();
    assert_eq!(m["pending"], json!(true), "{engines}");
}

/// Shared by the two tests below: a model held back while busy (its new
/// section pending), then a request that arrives while the apply that runs
/// once it is idle holds it. Returns the request's status and how long it
/// took.
async fn request_behind_a_held_back_apply(s: &Setup) -> (u16, Duration) {
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    // That re-read ended `m`'s process: let it exit before the slow
    // request loads it again.
    let exit = s.stub.inner.lock().unwrap().exit_delay_ms;
    tokio::time::sleep(Duration::from_millis(exit + 100)).await;
    let slow = spawn_slow(&s.gw);
    wait_for_chat(s).await;
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["deferred"], json!(["m"]), "{view}");
    assert_eq!(slow.await.unwrap(), 200);
    // Idle: the held-back apply runs (its command takes 0.4 s) and holds
    // `m`; this request waits at that hold.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = std::time::Instant::now();
    let status = spawn_chat(&s.gw, "hi").await.unwrap();
    (status, started.elapsed())
}

/// The request path's apply rewrites the model a request waits for: the
/// gate opens only once the old process is gone, so the request loads the
/// model with its new settings instead of meeting the exiting process.
#[tokio::test]
async fn a_request_behind_an_apply_waits_for_the_old_process_to_end() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("sleep 0.4 && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.exit_delay_ms = 300;
        inner.autoload_ms = 300;
    }
    let (status, _) = request_behind_a_held_back_apply(&s).await;
    let inner = s.stub.inner.lock().unwrap();
    assert_eq!(status, 200, "{:?} {:?}", inner.failed_chats, inner.calls);
    assert!(inner.failed_chats.is_empty(), "{:?}", inner.failed_chats);
    assert!(
        inner.args["m"].join(" ").contains("--ctx-size 16384"),
        "{:?}",
        inner.args
    );
}

/// An old process that does not end: the gate still opens within the
/// lane's `load_timeout_secs` (5 s here), well inside the request's own
/// gate wait, so the request is answered instead of 503 `model_paused`.
#[tokio::test]
async fn the_wait_for_an_old_process_is_bounded() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("sleep 0.4 && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    s.stub.inner.lock().unwrap().overlay_path = Some(s.overlay.clone());
    // The first apply's re-read must not leave `m` exiting: the slow
    // request loads it again first.
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.exit_delay_ms = 20_000;
        inner.autoload_ms = 100;
    }
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["deferred"], json!(["m"]), "{view}");
    assert_eq!(slow.await.unwrap(), 200);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = std::time::Instant::now();
    let status = spawn_chat(&s.gw, "hi").await.unwrap();
    let took = started.elapsed();
    assert_ne!(status, 503, "the request was answered model_paused");
    assert!(
        took >= Duration::from_secs(4) && took < Duration::from_secs(10),
        "{took:?}"
    );
    let view = get_json(format!("{}/v1/models/text/m/settings", s.gw)).await;
    let note = view["targets"][0]["load"]["last_apply"]["settle"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(note.contains("still ran"), "{view}");
}

/// A chat for `model` on the `text` lane: its status and how long it took.
async fn timed_chat(gw: &str, model: &str) -> (u16, Duration) {
    let started = std::time::Instant::now();
    let status = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&json!({"model": format!("text/{model}"),
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16();
    (status, started.elapsed())
}

/// The note a settle left on the lane's last apply.
async fn settle_note(s: &Setup) -> String {
    let view = get_json(format!("{}/v1/models/text/m/settings", s.gw)).await;
    view["targets"][0]["load"]["last_apply"]["settle"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The lane-up retry's apply holds `m` while its old process runs past
/// every bound. The gate wait (33 s: `command_timeout_secs` 3 + 30) is
/// shorter than `load_timeout_secs` (60 s), so the gate wait less a second
/// bounds the hold, counted from the first hold: the settings command
/// (2 s) is inside it. The retry's own hold opens with the apply's, before
/// the slow listing (2.5 s) is taken again. The chat that woke the lane is
/// served, never 503 `model_paused`.
#[tokio::test]
async fn a_lane_up_retry_opens_the_gate_within_the_requests_wait() {
    let (s, state) = setup_with(Opts {
        up: false,
        lane: "    route_health: true\n    wake_on_request: true\n    wake_wait_secs: 10\n    command_timeout_secs: 3",
        command_template: Some("test -e {marker} && sleep 2 && cat > {overlay}"),
        load_timeout_secs: 60,
        ..Opts::default()
    })
    .await;
    state.capacity.observe_all().await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 32768}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.exit_delay_ms = 120_000;
        inner.listing_delay_ms = 2_500;
    }
    let (status, took) = timed_chat(&s.gw, "m").await;
    assert_eq!(status, 200, "took {took:?}: {:?}", s.stub.calls());
    // The gate opened at the bound (32 s from the first hold), not before;
    // the 200 above says it opened before the chat's own wait ran out.
    assert!(
        took >= Duration::from_secs(30) && took < Duration::from_secs(40),
        "{took:?}"
    );
    let calls = s.stub.calls();
    assert!(
        calls.contains(&"POST /models/unload m".to_string()),
        "{calls:?}"
    );
    let note = settle_note(&s).await;
    assert!(note.contains("still ran 32 s"), "{note}");
}

/// One model's settle never holds back another: the lane-up retry holds
/// both pending models; `m`'s old process takes 3 s to exit, and `other`,
/// which does not run, opens at the first listing.
#[tokio::test]
async fn a_model_that_does_not_run_never_waits_for_anothers_exit() {
    let (s, state) = setup_with(Opts {
        up: false,
        lane: "    route_health: true\n    wake_on_request: true\n    wake_wait_secs: 10",
        command_template: Some("test -e {marker} && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    state.capacity.observe_all().await;
    for model in ["text/m", "text/other"] {
        let (status, view) = put(&s.gw, model, 0, json!({"load": {"ctx_size": 8192}})).await;
        assert_eq!(status, 200, "{view}");
        assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    }
    s.stub.inner.lock().unwrap().exit_delay_ms = 3_000;
    let (status, took) = timed_chat(&s.gw, "other").await;
    assert_eq!(status, 200, "{:?}", s.stub.calls());
    // Well before `m`'s old process exits (3 s).
    assert!(took < Duration::from_millis(2_500), "{took:?}");
    let calls = s.stub.calls();
    assert!(
        calls.contains(&"POST /models/unload m".to_string()),
        "{calls:?}"
    );
    // `m` itself still waits for its old process.
    let inner = s.stub.inner.lock().unwrap();
    assert!(inner.exiting.contains_key("m"), "{:?}", inner.calls);
}

/// A load after a pending apply holds no gate: chats that come in during
/// the (long) load are admitted at once, not held until the load ends.
#[tokio::test]
async fn chats_during_a_load_after_a_pending_apply_are_served() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.exit_delay_ms = 300;
        inner.load_delay_ms = 3_000;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"flash_attn": true}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    std::fs::write(format!("{}.go", s.overlay.display()), b"go").unwrap();
    let gw = s.gw.clone();
    let load = tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/load", json!({})).await });
    wait_for(&s, |calls| {
        calls.contains(&"POST /models/load m".to_string())
    })
    .await;
    let (status, took) = timed_chat(&s.gw, "m").await;
    assert_eq!(status, 200, "{:?}", s.stub.calls());
    assert!(took < Duration::from_millis(1_500), "{took:?}");
    let (status, body) = load.await.unwrap();
    assert_eq!(status, 200, "{body}");
    let inner = s.stub.inner.lock().unwrap();
    assert!(inner.refused_loads.is_empty(), "{:?}", inner.calls);
    assert!(
        inner.args["m"].join(" ").contains("--flash-attn on"),
        "{:?}",
        inner.args
    );
}

/// A power-off that lands while a settle waits for an old process ends
/// the settle at once: no listing reaches the machine after it, and the
/// apply's record says why the settle stopped.
#[tokio::test]
async fn a_power_off_ends_a_settle_without_another_listing() {
    let (s, _state) = setup_with(Opts {
        lane: "    poweroff_command: \"sleep 1 && rm -f {marker}\"",
        load_timeout_secs: 30,
        ..Opts::default()
    })
    .await;
    s.stub.inner.lock().unwrap().overlay_path = Some(s.overlay.clone());
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.exit_delay_ms = 60_000;
        inner.autoload_ms = 100;
    }
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["deferred"], json!(["m"]), "{view}");
    assert_eq!(slow.await.unwrap(), 200);
    // Idle: the held-back apply runs, and its settle waits for `m`.
    wait_for(&s, |calls| {
        calls.contains(&"POST /models/unload m".to_string())
    })
    .await;
    let (status, body) = post_to(&s.gw, "/v1/hosts/text/power", json!({"action": "off"})).await;
    assert_eq!(status, 200, "{body}");
    let listings = |s: &Setup| {
        s.stub
            .calls()
            .iter()
            .filter(|call| *call == "GET /models")
            .count()
    };
    let after = listings(&s);
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    assert_eq!(listings(&s), after, "{:?}", s.stub.calls());
    let note = settle_note(&s).await;
    assert!(note.contains("powering off"), "{note}");
}

/// The settings view names what an unload may take on the model's lane, so
/// a client can wait longer than that.
#[tokio::test]
async fn the_settings_view_names_what_an_unload_may_take() {
    let (s, _state) = setup_with(Opts::default()).await;
    let view = get_json(format!("{}/v1/models/text/m/settings", s.gw)).await;
    let engine = &view["targets"][0]["load"]["engine"];
    assert_eq!(engine["apply_wait_secs"], json!(1), "{view}");
    assert_eq!(engine["load_timeout_secs"], json!(5), "{view}");
}

/// A load for a busy model whose section is pending; a chat comes in right
/// after it. The load closes no gate (the busy model's section is held
/// back), so the chat is served long before its gate wait (35 s =
/// max(apply_wait_secs 35, command_timeout_secs 5 + 30)) could run out.
#[tokio::test]
async fn a_load_drain_and_its_apply_stay_within_a_waiting_chats_wait() {
    let (s, _state) = setup_with(Opts {
        lane: "    command_timeout_secs: 5",
        apply_wait_secs: 35,
        command_template: Some("test -e {overlay}.go && sleep 4 && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.slow_ms = 32_500;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    std::fs::write(format!("{}.go", s.overlay.display()), b"go").unwrap();
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    let gw = s.gw.clone();
    let load = tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/load", json!({})).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, took) = timed_chat(&s.gw, "m").await;
    let slow_status = slow.await.unwrap();
    let (load_status, load_body) = load.await.unwrap();
    assert_eq!(
        status,
        200,
        "chat took {took:?}; slow {slow_status}; load {load_status} {load_body}; calls {:?}",
        s.stub.calls()
    );
    assert_eq!(slow_status, 200);
}

/// An operator load that comes in after a held-back apply's retry ended
/// (its lock let go) but before that retry's settle took its first
/// listing waits for the settle: the settle's first pass unloads a model
/// it finds loaded, and would end the fresh load.
#[tokio::test]
async fn a_load_waits_for_a_settle_that_holds_its_model() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("sleep 0.4 && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    s.stub.inner.lock().unwrap().overlay_path = Some(s.overlay.clone());
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["deferred"], json!(["m"]), "{view}");
    // The settle's first listing answers late: the load below comes in
    // before it.
    s.stub.inner.lock().unwrap().listing_delay_ms = 1_500;
    let rereads = |calls: &[String]| {
        calls
            .iter()
            .filter(|call| *call == "GET /models?reload=1")
            .count()
    };
    let before = rereads(&s.stub.calls());
    assert_eq!(slow.await.unwrap(), 200);
    // Idle: the held-back apply runs (its command takes 0.4 s), re-reads
    // and leaves a settle holding `m`.
    wait_for(&s, |calls| rereads(calls) > before).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}; {:?}", s.stub.calls());
    let calls = s.stub.calls();
    let loaded_at = calls
        .iter()
        .rposition(|call| call == "POST /models/load m")
        .unwrap_or_else(|| panic!("never loaded: {calls:?}"));
    assert!(
        !calls[loaded_at..].contains(&"POST /models/unload m".to_string()),
        "the settle unloaded the fresh load: {calls:?}"
    );
    let inner = s.stub.inner.lock().unwrap();
    assert_eq!(
        inner.status.get("m").map(String::as_str),
        Some("loaded"),
        "{:?}",
        inner.calls
    );
    assert!(
        inner.args["m"].join(" ").contains("--ctx-size 16384"),
        "{:?}",
        inner.args
    );
}

/// A load for a busy model whose section is held back, while another apply
/// holds the lane's apply lock (its settings command takes 6 s). The load
/// waits for that lock without closing a gate: a chat that comes in
/// meanwhile is served then, not after the other apply.
#[tokio::test]
async fn a_load_lets_its_drain_go_while_another_apply_runs() {
    let (s, _state) = setup_with(Opts {
        lane: "    command_timeout_secs: 7",
        apply_wait_secs: 45,
        command_template: Some("if test -e {overlay}.slow; then sleep 6; fi; cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.slow_ms = 1_500;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["deferred"], json!(["m"]), "{view}");
    // Another apply (a settings write for `other`) takes the lock first.
    let slow_marker = format!("{}.slow", s.overlay.display());
    std::fs::write(&slow_marker, b"slow").unwrap();
    let gw = s.gw.clone();
    let other = tokio::spawn(async move {
        put(&gw, "text/other", 0, json!({"load": {"ctx_size": 8192}})).await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let gw = s.gw.clone();
    let load = tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/load", json!({})).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::remove_file(&slow_marker).unwrap();
    let (status, took) = timed_chat(&s.gw, "m").await;
    assert_eq!(
        status,
        200,
        "chat took {took:?}; calls {:?}",
        s.stub.calls()
    );
    assert!(
        took < Duration::from_secs(3),
        "chat took {took:?}; calls {:?}",
        s.stub.calls()
    );
    assert_eq!(slow.await.unwrap(), 200);
    let (status, view) = other.await.unwrap();
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["changed"], json!(["other"]), "{view}");
    let (status, body) = load.await.unwrap();
    assert_eq!(status, 200, "{body}");
}

/// The reviewer's repro: a load while a busy model's new section is held
/// back, and a chat for that model a moment later. The load closes no
/// gate, so the chat is served (never 503 `model_paused`).
#[tokio::test]
async fn a_chat_during_a_load_behind_a_held_back_apply_is_served() {
    let (s, _state) = setup_with(Opts {
        lane: "    command_timeout_secs: 1",
        apply_wait_secs: 40,
        load_timeout_secs: 60,
        ..Opts::default()
    })
    .await;
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.exit_delay_ms = 45_000;
        inner.autoload_ms = 100;
        inner.slow_ms = 5_000;
    }
    let slow = spawn_slow(&s.gw);
    wait_for_chat(&s).await;
    let (status, view) = put(&s.gw, "text/m", 1, json!({"load": {"ctx_size": 16384}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["deferred"], json!(["m"]), "{view}");
    let gw = s.gw.clone();
    let load = tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/load", json!({})).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let (status, body) = post_to(
        &s.gw,
        "/v1/chat/completions",
        json!({"model": "text/m", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    let took = started.elapsed();
    let slow_status = slow.await.unwrap();
    let (load_status, load_body) = load.await.unwrap();
    let failed = s.stub.inner.lock().unwrap().failed_chats.clone();
    assert_eq!(
        status,
        200,
        "chat took {took:?}: {body}; slow {slow_status}; load {load_status} {load_body}; \
         failed {failed:?}; calls {:?}",
        s.stub.calls()
    );
    assert_eq!(slow_status, 200);
}

/// A load after a pending apply whose re-read leaves the model's old
/// process running past the load's bound (`load_timeout_secs` 2): the load
/// fails with a clear error and stops nothing (no unload, no load), and
/// chats for the model are served at once the whole time.
#[tokio::test]
async fn a_load_whose_old_process_outlives_its_bound_fails_and_stops_nothing() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        load_timeout_secs: 2,
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.exit_delay_ms = 20_000;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"flash_attn": true}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    std::fs::write(format!("{}.go", s.overlay.display()), b"go").unwrap();
    let gw = s.gw.clone();
    let load = tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/load", json!({})).await });
    wait_for(&s, |calls| calls.contains(&"reload unloaded m".to_string())).await;
    for _ in 0..4 {
        let (status, took) = timed_chat(&s.gw, "m").await;
        assert_eq!(status, 200, "{:?}", s.stub.calls());
        assert!(took < Duration::from_millis(800), "chat took {took:?}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let (status, body) = load.await.unwrap();
    assert_eq!(status, 504, "{body}");
    assert_eq!(
        body["error"]["type"],
        json!("old_process_running"),
        "{body}"
    );
    let calls = s.stub.calls();
    assert!(
        !calls.contains(&"POST /models/unload m".to_string()),
        "{calls:?}"
    );
    assert!(
        !calls.contains(&"POST /models/load m".to_string()),
        "{calls:?}"
    );
    // The old process was left to end by itself.
    assert!(s.stub.inner.lock().unwrap().exiting.contains_key("m"));
}

/// A load after a pending apply that rewrote the model's section, found
/// still running its old process, never sends `/models/unload`: it waits
/// for the process the re-read ended, then loads the new settings.
#[tokio::test]
async fn a_load_after_a_pending_apply_never_unloads() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.exit_delay_ms = 600;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    std::fs::write(format!("{}.go", s.overlay.display()), b"go").unwrap();
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let inner = s.stub.inner.lock().unwrap();
    assert!(
        !inner.calls.contains(&"POST /models/unload m".to_string()),
        "{:?}",
        inner.calls
    );
    assert!(inner.refused_loads.is_empty(), "{:?}", inner.calls);
    assert_eq!(inner.status["m"], "loaded", "{:?}", inner.calls);
    assert!(
        inner.args["m"].join(" ").contains("--ctx-size 8192"),
        "{:?}",
        inner.args
    );
}

/// `ctx_size 8192` applied and loaded (`--ctx-size 8192`), then a failed
/// apply leaves `{ctx_size 8192, mmap false}` pending: only `load-mode`
/// changes, a key the arguments check does not compare, so the old
/// process's arguments already match every checked key. After the next
/// re-read the old process takes `exit_ms` to end. Leaves the settings
/// command able to run again.
async fn load_mode_change_pending(s: &Setup, exit_ms: u64) {
    s.stub.inner.lock().unwrap().overlay_path = Some(s.overlay.clone());
    let go = format!("{}.go", s.overlay.display());
    std::fs::write(&go, b"go").unwrap();
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"ctx_size": 8192}})).await;
    assert_eq!(status, 200, "{view}");
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        s.stub.inner.lock().unwrap().args["m"].join(" "),
        "llama-server --ctx-size 8192"
    );
    std::fs::remove_file(&go).unwrap();
    let (status, view) = put(
        &s.gw,
        "text/m",
        1,
        json!({"load": {"ctx_size": 8192, "mmap": false}}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    s.stub.inner.lock().unwrap().exit_delay_ms = exit_ms;
    std::fs::write(&go, b"go").unwrap();
}

/// The reviewer's scenario: with only `load-mode` changed, the old,
/// exiting process passes the arguments check, and must not count as the
/// new one. The load answers 200 only once a fresh process runs with
/// `--load-mode none`.
#[tokio::test]
async fn a_load_whose_change_the_args_check_misses_waits_for_a_fresh_process() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        ..Opts::default()
    })
    .await;
    load_mode_change_pending(&s, 1_500).await;
    let (status, body) = post_to(&s.gw, "/v1/models/text/m/load", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let inner = s.stub.inner.lock().unwrap();
    assert!(
        inner.refused_loads.is_empty(),
        "the load reached the engine while the old process ran: {:?}",
        inner.calls
    );
    assert!(inner.exiting.is_empty(), "{:?}", inner.calls);
    assert_eq!(inner.status["m"], "loaded", "{:?}", inner.calls);
    let args = inner.args["m"].join(" ");
    assert!(args.contains("--load-mode none"), "{args}");
}

/// A fresh process (a request's own load) that starts while the load waits
/// for the old one, with arguments that differ from the old process's
/// only in a key the check does not compare: it counts as the new one, so
/// the load ends at once instead of at its bound.
#[tokio::test]
async fn a_fresh_process_with_other_args_ends_a_loads_wait() {
    let (s, _state) = setup_with(Opts {
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        load_timeout_secs: 10,
        ..Opts::default()
    })
    .await;
    load_mode_change_pending(&s, 20_000).await;
    let gw = s.gw.clone();
    let load = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let answer = post_to(&gw, "/v1/models/text/m/load", json!({})).await;
        (answer, started.elapsed())
    });
    wait_for(&s, |calls| calls.contains(&"reload unloaded m".to_string())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        // The old process ends and a request's load starts the new one.
        let mut inner = s.stub.inner.lock().unwrap();
        inner.exiting.remove("m");
        start_process(&mut inner, "m");
        inner.status.insert("m".to_string(), "loaded".to_string());
    }
    let ((status, body), took) = load.await.unwrap();
    assert_eq!(status, 200, "{body}; {:?}", s.stub.calls());
    assert!(took < Duration::from_secs(4), "{took:?}");
    let args = s.stub.inner.lock().unwrap().args["m"].join(" ");
    assert!(args.contains("--load-mode none"), "{args}");
}

/// A power-off that lands while a load waits for the model's old process
/// ends the load at once with 503 naming why: no listing reaches the
/// machine after it.
#[tokio::test]
async fn a_power_off_ends_a_loads_wait_without_another_listing() {
    let (s, _state) = setup_with(Opts {
        lane: "    poweroff_command: \"sleep 1 && rm -f {marker}\"",
        command_template: Some("test -e {overlay}.go && cat > {overlay}"),
        load_timeout_secs: 30,
        ..Opts::default()
    })
    .await;
    {
        let mut inner = s.stub.inner.lock().unwrap();
        inner.overlay_path = Some(s.overlay.clone());
        inner.exit_delay_ms = 60_000;
    }
    let (status, view) = put(&s.gw, "text/m", 0, json!({"load": {"flash_attn": true}})).await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["apply"]["pending"], json!(true), "{view}");
    std::fs::write(format!("{}.go", s.overlay.display()), b"go").unwrap();
    let gw = s.gw.clone();
    let load = tokio::spawn(async move { post_to(&gw, "/v1/models/text/m/load", json!({})).await });
    wait_for(&s, |calls| calls.contains(&"reload unloaded m".to_string())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (status, body) = post_to(&s.gw, "/v1/hosts/text/power", json!({"action": "off"})).await;
    assert_eq!(status, 200, "{body}");
    let listings = |s: &Setup| {
        s.stub
            .calls()
            .iter()
            .filter(|call| *call == "GET /models")
            .count()
    };
    let after = listings(&s);
    let (status, body) = tokio::time::timeout(Duration::from_secs(3), load)
        .await
        .expect("the load still waits after the power-off")
        .unwrap();
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["error"]["type"], json!("machine_going_off"), "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("powering off"), "{body}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(listings(&s), after, "{:?}", s.stub.calls());
    assert!(
        !s.stub.calls().contains(&"POST /models/load m".to_string()),
        "{:?}",
        s.stub.calls()
    );
}
