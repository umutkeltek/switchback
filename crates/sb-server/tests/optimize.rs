//! Speed tricks end to end against stub llama.cpp routers ("optimize",
//! layer A): conversation stickiness (never a wake, never a queue),
//! `id_slot` for a sticky conversation, cached prompt tokens recorded,
//! loaded-model preference and swap counting, the reasoning guard (on, off,
//! and a request that asks for thinking itself), and the empty answer after
//! thinking it prevents.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

/// One machine's engine: a llama.cpp router with a loaded-model state
/// (`--models-max 1`), a swap delay, prefix-cache counts and slots.
#[derive(Default)]
struct EngineState {
    loaded: Vec<String>,
    models: Vec<String>,
    chats: Vec<Value>,
    /// First user messages seen (a repeat is a prefix-cache hit).
    seen: Vec<String>,
    loads: usize,
    /// `GET /models` calls (the listing poller must never reach a
    /// sleeping machine).
    listings: usize,
    /// A loaded instance's command line, per model (`status.args`).
    args: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Clone)]
struct Machine {
    name: &'static str,
    state: Arc<Mutex<EngineState>>,
    up: PathBuf,
    swap_ms: u64,
}

impl Machine {
    fn new(name: &'static str, models: &[&str], loaded: &[&str]) -> Self {
        let up = temp(&format!("{name}-up"));
        std::fs::write(&up, b"up").unwrap();
        let state = EngineState {
            models: models.iter().map(|m| m.to_string()).collect(),
            loaded: loaded.iter().map(|m| m.to_string()).collect(),
            ..EngineState::default()
        };
        Machine {
            name,
            state: Arc::new(Mutex::new(state)),
            up,
            swap_ms: 150,
        }
    }
    fn chats(&self) -> Vec<Value> {
        self.state.lock().unwrap().chats.clone()
    }
    fn last_chat(&self) -> Value {
        self.chats().last().cloned().unwrap_or(Value::Null)
    }
    fn sleep(&self) {
        let _ = std::fs::remove_file(&self.up);
    }
}

fn temp(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sb-optimize-{name}-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ))
}

async fn health(State(m): State<Machine>) -> StatusCode {
    if m.up.exists() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Shaped like a real llama.cpp router (build b12359): a preset model's
/// row has no `sidecars` field, and a loaded one carries `status.args`.
async fn models(State(m): State<Machine>) -> Json<Value> {
    let mut st = m.state.lock().unwrap();
    st.listings += 1;
    let data: Vec<Value> = st
        .models
        .iter()
        .map(|id| {
            let status = if st.loaded.contains(id) {
                "loaded"
            } else {
                "unloaded"
            };
            let mut row = json!({"id": id, "path": format!("/models/{id}.gguf"),
                                 "status": {"value": status}});
            if let Some(args) = st.args.get(id).filter(|_| status == "loaded") {
                row["status"]["args"] = json!(args);
            }
            row
        })
        .collect();
    Json(json!({"data": data}))
}

fn text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|messages| messages.iter().find(|m| m["role"] == "user"))
        .map(|m| m["content"].to_string())
        .unwrap_or_default()
}

async fn chat(State(m): State<Machine>, Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or("").to_string();
    let swap = {
        let mut st = m.state.lock().unwrap();
        st.chats.push(body.clone());
        if st.loaded.contains(&model) {
            false
        } else {
            st.loaded = vec![model.clone()];
            st.loads += 1;
            true
        }
    };
    if swap {
        tokio::time::sleep(Duration::from_millis(m.swap_ms)).await;
    }
    if body.to_string().contains("slow please") {
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
    let first = text(&body);
    let cached = {
        let mut st = m.state.lock().unwrap();
        if st.seen.contains(&first) {
            64
        } else {
            st.seen.push(first);
            0
        }
    };
    let slot = body["id_slot"].as_u64().unwrap_or(1);
    let thinking_off = body["chat_template_kwargs"]["enable_thinking"] == json!(false);
    let max_tokens = body["max_tokens"].as_u64().unwrap_or(4096);
    let (content, reasoning, finish) = if thinking_off {
        (format!("ok from {}", m.name), None, "stop")
    } else if max_tokens < 200 {
        // Thinking eats the whole budget: the failure the guard prevents.
        (String::new(), Some("let me think about this"), "length")
    } else {
        (format!("ok from {}", m.name), Some("thought"), "stop")
    };
    let timings = json!({"prompt_n": 100 - cached, "prompt_ms": 10.0, "cache_n": cached,
                         "predicted_n": 20, "predicted_ms": 100.0,
                         "draft_n": 10, "draft_n_accepted": 7});
    if body["stream"] == json!(true) {
        let mut delta = json!({"content": content});
        if let Some(reasoning) = reasoning {
            delta = json!({"reasoning_content": reasoning});
        }
        let frames = [
            json!({"id": "x", "model": model,
                   "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
            json!({"id": "x", "model": model, "id_slot": slot, "timings": timings,
                   "choices": [{"index": 0, "delta": {}, "finish_reason": finish}]}),
        ];
        let mut sse = String::new();
        for frame in frames {
            sse.push_str(&format!("data: {frame}\n\n"));
        }
        sse.push_str("data: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    let mut message = json!({"role": "assistant", "content": content});
    if let Some(reasoning) = reasoning {
        message["reasoning_content"] = json!(reasoning);
    }
    Json(json!({
        "id": "x", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "finish_reason": finish, "message": message}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20, "total_tokens": 120},
        "id_slot": slot,
        "timings": timings
    }))
    .into_response()
}

async fn props(State(_): State<Machine>) -> Json<Value> {
    Json(json!({"build_info": "b12359-b1b770d90", "model_path": "", "n_ctx": 0}))
}

async fn noop(State(_): State<Machine>, Json(_): Json<Value>) -> Json<Value> {
    Json(json!({"success": true}))
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
            .route("/props", get(props))
            .route("/models/load", post(noop))
            .route("/models/unload", post(noop))
            .route("/v1/chat/completions", post(chat))
            .with_state(machine.clone()),
    )
    .await
}

struct Fixture {
    gw: String,
    state: sb_server::AppState,
    gpu: Machine,
    small: Machine,
    wakes: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.gpu.up);
        let _ = std::fs::remove_file(&self.small.up);
        let _ = std::fs::remove_file(&self.wakes);
    }
}

/// `optimize` = lines under `server.optimize:` (four-space indent).
async fn fixture(optimize: &str) -> Fixture {
    fixture_with(optimize, "").await
}

/// `fixture` plus top-level config lines (`extra`), e.g. tenants and keys.
async fn fixture_with(optimize: &str, extra: &str) -> Fixture {
    let gpu = Machine::new("gpu-box", &["a", "b"], &["b"]);
    let small = Machine::new("small-host", &["m"], &["m"]);
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
{extra}
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
combos:
  rr:
    strategy: round_robin
    models: ["gpu-box/a", "small-host/m"]
routes:
  - name: pair
    match: {{ model: "pair" }}
    prefer_awake: {{ enabled: true, wake_for_prompt_tokens: 100000, wake_for_output_tokens: 100000 }}
    targets: ["gpu-box/a", "small-host/m"]
  - name: same-box
    match: {{ model: "same-box" }}
    targets: ["gpu-box/a", "gpu-box/b", "small-host/m"]
  - name: same-box-pinned
    match: {{ model: "same-box-pinned" }}
    pin_order: true
    targets: ["gpu-box/a", "gpu-box/b"]
local_executors:
  - name: gpu-box
    host: gpu-box
    base_url: "{gpu_url}"
    health_endpoint: "{gpu_url}/health"
    route_health: true
    wake_on_request: true
    wake_wait_secs: 5
    wake_command: "echo wake >> '{wakes}' && touch '{gpu_up}'"
    boot_timeout_secs: 5
    health_poll_interval_ms: 20
    engine:
      kind: llama_cpp_router
      settings_command: "cat > /dev/null"
      models_max: 1
  - name: small-host
    base_url: "{small_url}"
    health_endpoint: "{small_url}/health"
    route_health: true
    max_in_flight: 1
    queue_timeout_secs: 30
"#,
        wakes = wakes.display(),
        gpu_up = gpu.up.display(),
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
        small,
        wakes,
    }
}

struct Answer {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Value,
}

impl Answer {
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    }
    fn served_by(&self) -> String {
        self.body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches("ok from ")
            .to_string()
    }
}

async fn send(gw: &str, body: Value) -> Answer {
    let r = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    Answer {
        status,
        headers,
        body: r.json().await.unwrap_or(Value::Null),
    }
}

fn convo(model: &str, first: &str, turns: usize) -> Value {
    let mut messages = vec![json!({"role": "system", "content": "be brief"})];
    messages.push(json!({"role": "user", "content": first}));
    for i in 1..turns {
        messages.push(json!({"role": "assistant", "content": "sure"}));
        messages.push(json!({"role": "user", "content": format!("turn {i}")}));
    }
    json!({"model": model, "messages": messages, "max_tokens": 4000,
           "chat_template_kwargs": {"enable_thinking": false}})
}

async fn put_settings(gw: &str, target: &str, doc: Value) {
    let current = reqwest::Client::new()
        .get(format!("{gw}/v1/models/{target}/settings"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let revision = current["revision"].as_u64().unwrap_or(0);
    let r = reqwest::Client::new()
        .put(format!("{gw}/v1/models/{target}/settings"))
        .header("if-match", revision.to_string())
        .json(&doc)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
}

#[tokio::test]
async fn stickiness_keeps_a_conversation_on_its_machine() {
    let fx = fixture("    stickiness: true").await;
    // Round robin alternates the first choice; a conversation stays put.
    let first = send(&fx.gw, convo("rr", "plan a trip", 1)).await;
    assert_eq!(first.status, 200, "{:?}", first.body);
    let home = first.served_by();
    for turns in 2..5 {
        let next = send(&fx.gw, convo("rr", "plan a trip", turns)).await;
        assert_eq!(next.served_by(), home, "turn {turns} moved");
        assert!(
            next.header("x-switchback-optimize").contains("sticky="),
            "{}",
            next.header("x-switchback-optimize")
        );
    }
    // Another conversation is free to go elsewhere (round robin moved on).
    let other = send(&fx.gw, convo("rr", "something else entirely", 1)).await;
    assert_eq!(other.status, 200);
    assert!(other.header("x-switchback-optimize").is_empty());
}

/// Send `body` with an API key and a session header.
async fn send_as(gw: &str, key: &str, session: &str, body: Value) -> Answer {
    let r = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .bearer_auth(key)
        .header("x-switchback-session-id", session)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    Answer {
        status,
        headers,
        body: r.json().await.unwrap_or(Value::Null),
    }
}

#[tokio::test]
async fn two_tenants_with_the_same_session_never_steer_each_other() {
    let fx = fixture_with(
        "    stickiness: true",
        r#"tenants:
  - id: team-a
  - id: team-b
api_keys:
  - { key: "sk-team-a", tenant: team-a, role: admin }
  - { key: "sk-team-b", tenant: team-b, role: admin }"#,
    )
    .await;
    // Team A's conversation `s-1` settles on a machine.
    let a1 = send_as(&fx.gw, "sk-team-a", "s-1", convo("rr", "team a work", 1)).await;
    assert_eq!(a1.status, 200, "{:?}", a1.body);
    let home = a1.served_by();
    // Team B sends the same session id: its own conversation, so nothing
    // sticky steers it and round robin moves on.
    let b1 = send_as(&fx.gw, "sk-team-b", "s-1", convo("rr", "team b work", 1)).await;
    assert_eq!(b1.status, 200, "{:?}", b1.body);
    let line = b1.header("x-switchback-optimize");
    assert!(!line.contains("sticky="), "team b was steered: {line}");
    assert_ne!(b1.served_by(), home, "round robin moved on for team b");
    // Team A stays where it was; team B's turn did not move it.
    let a2 = send_as(&fx.gw, "sk-team-a", "s-1", convo("rr", "team a work", 2)).await;
    assert_eq!(a2.served_by(), home);
    assert!(a2
        .header("x-switchback-optimize")
        .contains(&format!("sticky={home}")));
}

#[tokio::test]
async fn without_stickiness_round_robin_moves_the_conversation() {
    let fx = fixture("    stickiness: false").await;
    let first = send(&fx.gw, convo("rr", "plan a trip", 1))
        .await
        .served_by();
    let second = send(&fx.gw, convo("rr", "plan a trip", 2)).await;
    assert_ne!(second.served_by(), first);
    assert!(second.header("x-switchback-optimize").is_empty());
}

#[tokio::test]
async fn stickiness_never_wakes_a_machine() {
    let fx = fixture("    stickiness: true").await;
    // Turn 1 lands on the GPU box (first on the route, up).
    let first = send(&fx.gw, convo("pair", "draft the memo", 1)).await;
    assert_eq!(first.served_by(), "gpu-box");
    // The box goes to sleep.
    fx.gpu.sleep();
    fx.state.capacity.observe_all().await;
    let next = send(&fx.gw, convo("pair", "draft the memo", 2)).await;
    assert_eq!(next.status, 200, "{:?}", next.body);
    assert_eq!(next.served_by(), "small-host");
    let line = next.header("x-switchback-optimize");
    assert!(line.contains("sticky=gpu-box/a not kept"), "{line}");
    assert!(!fx.wakes.exists(), "stickiness woke the machine");
}

#[tokio::test]
async fn stickiness_never_queues_behind_a_full_lane() {
    let fx = fixture("    stickiness: true").await;
    // Turn 1 is served by the small host (named directly).
    let first = send(&fx.gw, convo("small-host/m", "keep me here", 1)).await;
    assert_eq!(first.served_by(), "small-host");
    // While it is free, turn 2 stays there even though the route lists
    // the GPU box first.
    let kept = send(&fx.gw, convo("pair", "keep me here", 2)).await;
    assert_eq!(kept.served_by(), "small-host");
    assert!(kept
        .header("x-switchback-optimize")
        .contains("sticky=small-host/m"));
    // Fill the small host's one place with a slow request.
    let gw = fx.gw.clone();
    let slow = tokio::spawn(async move {
        send(
            &gw,
            json!({"model": "small-host/m", "max_tokens": 4000,
                   "chat_template_kwargs": {"enable_thinking": false},
                   "messages": [{"role": "user", "content": "slow please"}]}),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    let next = send(&fx.gw, convo("pair", "keep me here", 3)).await;
    assert_eq!(next.status, 200);
    assert_eq!(next.served_by(), "gpu-box", "went elsewhere at once");
    assert!(started.elapsed() < Duration::from_millis(700), "it waited");
    let line = next.header("x-switchback-optimize");
    assert!(line.contains("sticky=small-host/m not kept"), "{line}");
    slow.await.unwrap();
}

#[tokio::test]
async fn sticky_conversation_pins_its_slot_and_cached_tokens_are_recorded() {
    let fx = fixture("    stickiness: true").await;
    put_settings(&fx.gw, "gpu-box/a", json!({"load": {"parallel": 2}})).await;
    let first = send(&fx.gw, convo("pair", "long shared context", 1)).await;
    assert_eq!(first.served_by(), "gpu-box");
    assert!(fx.gpu.last_chat().get("id_slot").is_none());
    let next = send(&fx.gw, convo("pair", "long shared context", 2)).await;
    assert_eq!(next.served_by(), "gpu-box");
    assert_eq!(fx.gpu.last_chat()["id_slot"], json!(1));
    assert!(next.header("x-switchback-optimize").contains("slot=1"));
    // The second turn reused 64 prompt tokens: recorded on usage.
    let records = fx.state.ledger.recent(10);
    let cached: Vec<u64> = records
        .iter()
        .map(|r| r.usage.cached_input_tokens)
        .collect();
    assert!(cached.contains(&64), "{cached:?}");
    let obs = fx.state.engine.optimize().observations();
    let box_a = &obs["gpu-box/a"];
    assert_eq!(box_a.cached_tokens, 64);
    assert_eq!((box_a.draft_tokens, box_a.draft_accepted), (20, 14));
}

#[tokio::test]
async fn loaded_model_goes_first_and_swaps_are_counted() {
    let fx = fixture("    loaded_aware: true").await;
    // The engine control's listing: b is loaded, a is not.
    let r = reqwest::get(format!("{}/v1/engines", fx.gw)).await.unwrap();
    assert_eq!(r.status(), 200);
    let answer = send(&fx.gw, convo("same-box", "hello", 1)).await;
    assert_eq!(answer.status, 200);
    assert_eq!(fx.gpu.last_chat()["model"], "b", "no swap");
    let line = answer.header("x-switchback-optimize");
    assert!(line.contains("loaded=gpu-box/b over gpu-box/a"), "{line}");
    assert_eq!(fx.gpu.state.lock().unwrap().loads, 0);
    // A pinned order is kept.
    let pinned = send(&fx.gw, convo("same-box-pinned", "hi", 1)).await;
    assert!(pinned.header("x-switchback-optimize").is_empty());
    // That request made the engine swap b -> a: one swap counted on a.
    let obs = fx.state.engine.optimize().observations();
    assert_eq!(obs["gpu-box/a"].swaps_last_hour, 1);
    assert_eq!(obs["gpu-box/a"].swapped_out_last_hour["b"], 1);
    // Back to b (direct): another swap, counted on b.
    send(&fx.gw, convo("gpu-box/b", "hi", 1)).await;
    let obs = fx.state.engine.optimize().observations();
    assert_eq!(obs["gpu-box/b"].swaps_last_hour, 1);
    assert_eq!(fx.gpu.state.lock().unwrap().loads, 2);
}

#[tokio::test]
async fn loaded_preference_is_off_by_default() {
    let fx = fixture("    loaded_aware: false").await;
    reqwest::get(format!("{}/v1/engines", fx.gw)).await.unwrap();
    let answer = send(&fx.gw, convo("same-box", "hello", 1)).await;
    assert_eq!(fx.gpu.last_chat()["model"], "a");
    assert!(answer.header("x-switchback-optimize").is_empty());
}

fn ask(model: &str, max_tokens: u64, extra: Value) -> Value {
    let mut body = json!({"model": model, "max_tokens": max_tokens,
                          "messages": [{"role": "user", "content": "what is 2+2"}]});
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            body.insert(k.clone(), v.clone());
        }
    }
    body
}

#[tokio::test]
async fn reasoning_guard_turns_thinking_off_for_small_budgets() {
    let fx = fixture("    stickiness: false").await;
    // Without the guard: the budget goes to thinking, the answer is empty,
    // and the gateway counts it.
    let bare = send(&fx.gw, ask("small-host/m", 100, json!({}))).await;
    assert_eq!(bare.body["choices"][0]["finish_reason"], "length");
    let obs = fx.state.engine.optimize().observations();
    assert_eq!(obs["small-host/m"].empty_after_thinking, 1);

    put_settings(
        &fx.gw,
        "small-host/m",
        json!({"reasoning": true, "reasoning_guard": {"min_tokens": 1024}}),
    )
    .await;
    let guarded = send(&fx.gw, ask("small-host/m", 100, json!({}))).await;
    assert_eq!(guarded.served_by(), "small-host");
    let sent = fx.small.last_chat();
    assert_eq!(
        sent["chat_template_kwargs"]["enable_thinking"],
        json!(false)
    );
    assert!(sent.get("reasoning_effort").is_none(), "{sent}");
    assert!(guarded
        .header("x-switchback-settings")
        .contains("reasoning_guard=off (max_tokens 100 < 1024)"));
    assert!(guarded
        .header("x-switchback-optimize")
        .contains("reasoning_guard=thinking off"));
    // A big budget keeps thinking.
    send(&fx.gw, ask("small-host/m", 4000, json!({}))).await;
    assert!(fx.small.last_chat().get("chat_template_kwargs").is_none());
    // A request that asks for thinking keeps it, and the header says so.
    let asked = send(
        &fx.gw,
        ask("small-host/m", 100, json!({"reasoning_effort": "high"})),
    )
    .await;
    assert_eq!(
        fx.small.last_chat()["chat_template_kwargs"]["enable_thinking"],
        json!(true)
    );
    assert!(asked
        .header("x-switchback-settings")
        .contains("reasoning_guard=kept"));
    let obs = fx.state.engine.optimize().observations();
    assert_eq!(obs["small-host/m"].reasoning_guard_applied, 1);
    assert_eq!(obs["small-host/m"].empty_after_thinking, 2);

    // Guard switched off: thinking stays on again.
    put_settings(
        &fx.gw,
        "small-host/m",
        json!({"reasoning": true, "reasoning_guard": {"enabled": false}}),
    )
    .await;
    let off = send(&fx.gw, ask("small-host/m", 100, json!({}))).await;
    assert!(fx.small.last_chat().get("chat_template_kwargs").is_none());
    assert!(!off
        .header("x-switchback-settings")
        .contains("reasoning_guard"));
}

#[tokio::test]
async fn streamed_empty_answer_after_thinking_is_counted() {
    let fx = fixture("    stickiness: false").await;
    // The stub answers in one piece; a stream request still goes through
    // the gateway's stream path.
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", fx.gw))
        .json(
            &json!({"model": "small-host/m", "max_tokens": 100, "stream": true,
                      "messages": [{"role": "user", "content": "think"}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = r.text().await.unwrap();
    // The finalizer runs when the stream ends.
    for _ in 0..50 {
        let obs = fx.state.engine.optimize().observations();
        if obs
            .get("small-host/m")
            .is_some_and(|o| o.empty_after_thinking == 1)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "not counted: {:?}",
        fx.state
            .engine
            .optimize()
            .observations()
            .get("small-host/m")
            .map(|o| o.empty_after_thinking)
    );
}

fn listings(machine: &Machine) -> usize {
    machine.state.lock().unwrap().listings
}

async fn lane_state(gw: &str, lane: &str) -> String {
    let capacity: Value = reqwest::get(format!("{gw}/v1/workloads/capacity"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    capacity["data"]
        .as_array()
        .and_then(|lanes| lanes.iter().find(|l| l["name"] == lane))
        .map(|l| l["state"].as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

#[tokio::test]
async fn the_listing_poller_never_calls_a_sleeping_lane() {
    let fx = fixture("    loaded_aware: true").await;
    // Asleep from the start: never polled, never woken.
    fx.gpu.sleep();
    fx.state.capacity.observe_all().await;
    let before = listings(&fx.gpu);
    fx.state.poll_engine_listings().await;
    assert_eq!(listings(&fx.gpu), before, "a sleeping lane was polled");
    assert!(!fx.wakes.exists(), "the poller woke the machine");
    // Up again (the lifecycle adopts a machine woken outside it): polled
    // only once the lifecycle says it is up.
    std::fs::write(&fx.gpu.up, b"up").unwrap();
    fx.state.capacity.observe_all().await;
    let state = lane_state(&fx.gw, "gpu-box").await;
    fx.state.poll_engine_listings().await;
    if state != "healthy" && state != "idle_countdown" {
        assert_eq!(listings(&fx.gpu), before, "a {state} lane was polled");
    }
    let answer = send(&fx.gw, convo("gpu-box/a", "hello", 1)).await;
    assert_eq!(answer.status, 200, "{:?}", answer.body);
    let state = lane_state(&fx.gw, "gpu-box").await;
    assert!(
        state == "healthy" || state == "idle_countdown",
        "lane state {state}"
    );
    let before = listings(&fx.gpu);
    fx.state.poll_engine_listings().await;
    assert_eq!(listings(&fx.gpu), before + 1);
    // The loaded-model view follows the listing (a is loaded now).
    let view: Value = reqwest::get(format!("{}/v1/engines", fx.gw))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gpu = &view["data"][0];
    assert_eq!(gpu["lane"], "gpu-box");
    // The machine sleeps: the lifecycle may still say healthy, but its
    // probe saw it down, so it is not polled again.
    fx.gpu.sleep();
    fx.state.capacity.observe_all().await;
    let before = listings(&fx.gpu);
    fx.state.poll_engine_listings().await;
    assert_eq!(listings(&fx.gpu), before, "a sleeping lane was polled");
    // The small host has no engine control: never polled.
    assert_eq!(listings(&fx.small), 0);
    assert!(!fx.wakes.exists(), "the poller woke the machine");
}

#[tokio::test]
async fn the_spawned_poller_refreshes_up_lanes_only() {
    let fx = fixture("    loaded_aware: true").await;
    send(&fx.gw, convo("gpu-box/a", "hello", 1)).await;
    let before = listings(&fx.gpu);
    fx.state
        .spawn_engine_listing_poller(Duration::from_millis(20));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(listings(&fx.gpu) > before, "an up lane was not polled");
    fx.gpu.sleep();
    fx.state.capacity.observe_all().await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    let asleep = listings(&fx.gpu);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(listings(&fx.gpu), asleep, "a sleeping lane was polled");
    assert!(!fx.wakes.exists());
}

#[tokio::test]
async fn the_engine_view_reads_build_and_speculation_from_a_real_shaped_router() {
    let fx = fixture("    loaded_aware: true").await;
    // A loaded instance whose command line turns on MTP (the preset has
    // `spec-type = draft-mtp`); no `sidecars` anywhere, as on a real router.
    fx.gpu.state.lock().unwrap().args.insert(
        "b".into(),
        vec![
            "llama-server".into(),
            "--port".into(),
            "8081".into(),
            "--spec-type".into(),
            "draft-mtp".into(),
        ],
    );
    // Settings the gateway writes into the overlay set the type; the loaded
    // instance names a draft model on its command line.
    fx.gpu.state.lock().unwrap().args.insert(
        "a".into(),
        vec!["llama-server".into(), "-md".into(), "/models/d.gguf".into()],
    );
    fx.gpu.state.lock().unwrap().loaded.push("a".into());
    put_settings(
        &fx.gw,
        "gpu-box/a",
        json!({"load": {"spec_type": "draft-simple"}}),
    )
    .await;
    let view: Value = reqwest::get(format!("{}/v1/engines", fx.gw))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gpu = &view["data"][0];
    assert_eq!(gpu["build"], "b12359-b1b770d90", "{gpu}");
    let row = |id: &str| {
        gpu["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .cloned()
            .unwrap()
    };
    let (a, b) = (row("a"), row("b"));
    assert_eq!(a["sidecars"], json!([]));
    assert_eq!(b["sidecars"], json!([]));
    // A file identity, never the path.
    assert_eq!(a["file"].as_str().unwrap().len(), 12);
    assert_eq!(b["speculative"]["spec_type"], "draft-mtp", "{b}");
    assert_eq!(b["speculative"]["mtp"], true);
    assert_eq!(b["speculative"]["sources"], json!(["args"]));
    assert_eq!(a["speculative"]["spec_type"], "draft-simple", "{a}");
    assert_eq!(a["speculative"]["draft_model"], true);
    assert_eq!(a["speculative"]["mtp"], false);
    assert_eq!(a["speculative"]["sources"], json!(["settings", "args"]));
}
