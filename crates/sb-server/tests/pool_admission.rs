//! Token-pool admission end to end, through real HTTP, against a fake
//! llama.cpp router whose model shares one context pool among its slots
//! (`--kv-unified`) and, like the real engine, fails a request that would
//! overflow the pool ("Context size has been exceeded").
//!
//! The fake's tokenizer counts words; so does its pool. The gateway counts
//! large prompts with the fake's `/tokenize` and adds the chat template's
//! overhead, so its reservations are never below what the fake uses.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Default)]
struct Engine {
    /// `--ctx-size` the listing shows for model `qwen` (the pool).
    ctx: u64,
    /// Tokens the chats inside the engine use now, and the most ever.
    in_use: u64,
    peak: u64,
    /// Chats that would have overflowed the pool (the real engine fails
    /// every running request then).
    overflowed: u32,
    /// Chats that reached the engine for `qwen`, and every model's.
    chats: u32,
    all_chats: Vec<String>,
    tokenize_calls: u32,
    listing_calls: u32,
    /// Chats inside the engine now.
    running: u32,
    /// Answers in finish order (by planted code).
    finished: VecDeque<String>,
}

#[derive(Clone)]
struct Fake {
    engine: Arc<Mutex<Engine>>,
    /// One permit lets one held chat finish.
    finish: Arc<tokio::sync::Semaphore>,
}

impl Fake {
    fn get<T>(&self, read: impl FnOnce(&Engine) -> T) -> T {
        read(&self.engine.lock().unwrap())
    }
    fn release(&self, n: usize) {
        self.finish.add_permits(n);
    }
}

fn words(text: &str) -> u64 {
    text.split_whitespace().count() as u64
}

fn prompt_words(body: &Value) -> u64 {
    body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .map(|m| words(m["content"].as_str().unwrap_or("")))
                .sum()
        })
        .unwrap_or(0)
}

async fn models(State(fake): State<Fake>) -> Json<Value> {
    let mut engine = fake.engine.lock().unwrap();
    engine.listing_calls += 1;
    let ctx = engine.ctx.to_string();
    Json(json!({"data": [
        {"id": "qwen", "status": {"value": "loaded", "args": [
            "llama-server", "--ctx-size", ctx, "--parallel", "3", "--kv-unified"]}},
        {"id": "other", "status": {"value": "unloaded"}}
    ]}))
}

async fn tokenize(State(fake): State<Fake>, Json(body): Json<Value>) -> Json<Value> {
    fake.engine.lock().unwrap().tokenize_calls += 1;
    let n = words(body["content"].as_str().unwrap_or(""));
    Json(json!({"tokens": vec![1; n as usize]}))
}

async fn chat(State(fake): State<Fake>, Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or("").to_string();
    let code = body["messages"][0]["content"]
        .as_str()
        .and_then(|text| text.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    fake.engine.lock().unwrap().all_chats.push(model.clone());
    let prompt = prompt_words(&body);
    if model != "qwen" {
        return Json(json!({
            "id": "x", "object": "chat.completion", "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": code},
                         "finish_reason": "stop"}],
            "usage": {"prompt_tokens": prompt, "completion_tokens": 1, "total_tokens": prompt + 1}
        }))
        .into_response();
    }
    let generate = body["max_tokens"]
        .as_u64()
        .or_else(|| body["max_completion_tokens"].as_u64())
        .unwrap_or(50);
    let need = prompt + generate;
    {
        let mut engine = fake.engine.lock().unwrap();
        engine.chats += 1;
        if engine.in_use + need > engine.ctx {
            engine.overflowed += 1;
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"error": {"code": 500, "message": "Context size has been exceeded.",
                                      "type": "server_error"}}),
                ),
            )
                .into_response();
        }
        engine.in_use += need;
        engine.peak = engine.peak.max(engine.in_use);
        engine.running += 1;
    }
    // Leaves the pool when the chat ends or the gateway drops the call.
    struct Leave(Fake, u64);
    impl Drop for Leave {
        fn drop(&mut self) {
            let mut engine = self.0.engine.lock().unwrap();
            engine.in_use -= self.1;
            engine.running -= 1;
        }
    }
    let leave = Leave(fake.clone(), need);
    if body["stream"].as_bool() == Some(true) {
        let first = format!(
            "data: {}\n\n",
            json!({"id": "x", "object": "chat.completion.chunk", "model": model,
                   "choices": [{"index": 0, "delta": {"role": "assistant", "content": "part "}}]})
        );
        let stream = async_stream(fake.clone(), first, code, leave);
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    let permit = fake.finish.acquire().await.unwrap();
    permit.forget();
    fake.engine.lock().unwrap().finished.push_back(code.clone());
    drop(leave);
    Json(json!({
        "id": "x", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": code},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": prompt, "completion_tokens": 1, "total_tokens": prompt + 1}
    }))
    .into_response()
}

/// A streamed answer: the first chunk at once, the rest once released.
fn async_stream(
    fake: Fake,
    first: String,
    code: String,
    leave: impl Send + 'static,
) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    futures::stream::unfold(0u8, move |step| {
        let (fake, first, code) = (fake.clone(), first.clone(), code.clone());
        let _keep = &leave;
        async move {
            match step {
                0 => Some((Ok(Bytes::from(first)), 1)),
                1 => {
                    let permit = fake.finish.acquire().await.unwrap();
                    permit.forget();
                    let last = format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"id": "x", "object": "chat.completion.chunk", "model": "qwen",
                               "choices": [{"index": 0, "delta": {"content": code},
                                            "finish_reason": "stop"}]})
                    );
                    Some((Ok(Bytes::from(last)), 2))
                }
                _ => None,
            }
        }
    })
}

async fn start_fake(ctx: u64) -> (String, Fake) {
    let fake = Fake {
        engine: Arc::new(Mutex::new(Engine {
            ctx,
            ..Engine::default()
        })),
        finish: Arc::new(tokio::sync::Semaphore::new(0)),
    };
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/models", get(models))
        .route("/tokenize", post(tokenize))
        .route("/v1/chat/completions", post(chat))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), fake)
}

struct Gateway {
    url: String,
    state: sb_server::AppState,
}

async fn start_gateway(fake_url: &str, lane_extra: &str) -> Gateway {
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "{fake_url}/v1"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: other
    type: openai_compatible
    base_url: "{fake_url}/v1"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: mix
    match: {{ model: mix }}
    pin_order: true
    targets: [text/qwen, other/cloud]
local_executors:
  - name: text
    base_url: "{fake_url}"
    health_endpoint: "{fake_url}/health"
{lane_extra}
    engine:
      kind: llama_cpp_router
"#
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
    let url = format!("http://{addr}");
    // The engine's listing, as the poller or `GET /v1/engines` takes it.
    let engines = reqwest::get(format!("{url}/v1/engines")).await.unwrap();
    assert!(engines.status().is_success());
    Gateway { url, state }
}

/// A prompt of `n` words, its first word a code to find in the answer.
fn prompt(code: &str, n: usize) -> String {
    let mut text = code.to_string();
    for i in 1..n {
        text.push_str(if i % 7 == 0 { " river" } else { " stone" });
    }
    text
}

async fn send(gw: &str, model: &str, text: String, max_tokens: Option<u64>) -> (u16, Value) {
    let mut body = json!({"model": model, "messages": [{"role": "user", "content": text}]});
    if let Some(max) = max_tokens {
        body["max_tokens"] = json!(max);
    }
    let r = reqwest::Client::new()
        .post(format!("{gw}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

fn pool(gw: &Gateway) -> sb_runtime::token_pool::PoolReport {
    gw.state
        .engine
        .token_pools()
        .reports()
        .into_iter()
        .find(|pool| pool.model == "qwen")
        .expect("a pool for text/qwen")
}

async fn until(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn content(answer: &Value) -> String {
    answer["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

// The falsifier, both ways. Three requests of 950 prompt tokens + 100 to
// generate in a pool of 3000: together 3150, over the pool. Without
// admission all three reach the engine and it overflows; with it the third
// waits until one ends, nothing overflows, every answer is right, and the
// one that waited says so.
#[tokio::test]
async fn three_large_requests_never_overflow_the_shared_pool() {
    for admission in [false, true] {
        let (fake_url, fake) = start_fake(3000).await;
        let lane = if admission {
            "    pool_admission: { count_above_tokens: 100 }"
        } else {
            "    pool_admission: { enabled: false }"
        };
        let gw = start_gateway(&fake_url, lane).await;
        let mut tasks = Vec::new();
        for code in ["CODE-A", "CODE-B", "CODE-C"] {
            let url = gw.url.clone();
            tasks.push(tokio::spawn(async move {
                send(&url, "text/qwen", prompt(code, 950), Some(100)).await
            }));
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        if admission {
            until("two running, one waiting", || {
                fake.get(|e| e.running) == 2 && pool(&gw).waiting == 1
            })
            .await;
            let report = pool(&gw);
            assert_eq!(report.pool_tokens, Some(3000));
            assert_eq!(report.running, 2);
            // Counted by the engine: 950 words + 24 template + 100 reserve.
            assert_eq!(report.reserved_tokens, 2 * 1074);
            assert_eq!(report.waiting_tokens, 1074);
            assert_eq!(
                fake.get(|e| e.chats),
                2,
                "the third never reached the engine yet"
            );
        } else {
            until("all three reached the engine", || {
                fake.get(|e| e.chats) == 3
            })
            .await;
        }
        fake.release(3);
        let mut answers = Vec::new();
        for task in tasks {
            answers.push(task.await.unwrap());
        }
        if !admission {
            assert!(
                fake.get(|e| e.overflowed) >= 1,
                "without admission the engine's pool overflows: {answers:?}"
            );
            continue;
        }
        assert_eq!(fake.get(|e| e.overflowed), 0);
        assert!(fake.get(|e| e.peak) <= 3000);
        for ((status, answer), code) in answers.iter().zip(["CODE-A", "CODE-B", "CODE-C"]) {
            assert_eq!(*status, 200, "{answer}");
            assert_eq!(content(answer), code);
        }
        let report = pool(&gw);
        assert_eq!(
            (report.reserved_tokens, report.running, report.waiting),
            (0, 0, 0)
        );
        assert_eq!(report.waited_total, 1, "exactly one request waited");
        assert!(report.last_wait_ms.is_some());
        let traces = gw.state.engine.traces().recent(10);
        let waited: Vec<_> = traces
            .iter()
            .filter_map(|trace| trace.pool_admission.as_ref())
            .filter(|note| note.waited_ms > 0)
            .collect();
        assert_eq!(waited.len(), 1, "one trace notes the wait");
        assert_eq!(waited[0].prompt_source, "engine");
        assert_eq!(waited[0].need_tokens, 1074);
        let usage = gw.state.ledger.recent(10);
        assert_eq!(
            usage
                .iter()
                .filter(|r| r.speed.pool_wait_ms.is_some())
                .count(),
            1,
            "one usage record notes the wait"
        );
    }
}

// A request that alone is larger than one request may use is refused with
// a 400 and never sent to the engine; one without max_tokens that fits is
// admitted alone (its reserve is capped, so it never waits for itself).
#[tokio::test]
async fn too_large_is_refused_and_a_missing_max_tokens_never_deadlocks() {
    let (fake_url, fake) = start_fake(3000).await;
    let gw = start_gateway(&fake_url, "    pool_admission: { count_above_tokens: 100 }").await;
    let (status, answer) = send(&gw.url, "text/qwen", prompt("BIG", 3100), Some(10)).await;
    assert_eq!(status, 400, "{answer}");
    assert_eq!(
        answer["error"]["type"], "context_length_exceeded",
        "{answer}"
    );
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("3,124 tokens (counted by the engine)"),
        "{message}"
    );
    assert!(message.contains("at most 3,000"), "{message}");
    assert_eq!(fake.get(|e| e.chats), 0, "never sent to the engine");
    assert_eq!(pool(&gw).refused_total, 1);

    fake.release(1);
    let (status, answer) = send(&gw.url, "text/qwen", prompt("ALONE", 2900), None).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(content(&answer), "ALONE");
    let report = pool(&gw);
    assert_eq!((report.waited_total, report.reserved_tokens), (0, 0));
    let note = gw
        .state
        .engine
        .traces()
        .recent(5)
        .into_iter()
        .find_map(|trace| trace.pool_admission)
        .unwrap();
    assert_eq!(note.reserve_source, "default");
    assert_eq!(note.need_tokens, 3000, "capped at one request's context");
}

// A client that goes away while waiting leaves the queue; one that goes away
// while its answer runs (collected or streamed) gives its tokens back.
#[tokio::test]
async fn disconnects_return_reservations() {
    let (fake_url, fake) = start_fake(3000).await;
    let gw = start_gateway(&fake_url, "    pool_admission: { count_above_tokens: 100 }").await;
    let url = gw.url.clone();
    let a = tokio::spawn(async move { send(&url, "text/qwen", prompt("A", 950), Some(100)).await });
    until("A running", || fake.get(|e| e.running) == 1).await;
    // B streams: its first chunk arrives, then the client hangs up.
    let client = reqwest::Client::new();
    let mut b = client
        .post(format!("{}/v1/chat/completions", gw.url))
        .json(
            &json!({"model": "text/qwen", "stream": true, "max_tokens": 100,
                      "messages": [{"role": "user", "content": prompt("B", 950)}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(b.status().as_u16(), 200);
    let mut seen = String::new();
    while !seen.contains("part") {
        let chunk = b.chunk().await.unwrap().expect("the stream's first words");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    let url = gw.url.clone();
    let c = tokio::spawn(async move { send(&url, "text/qwen", prompt("C", 950), Some(100)).await });
    until("C waiting", || pool(&gw).waiting == 1).await;
    assert_eq!(pool(&gw).reserved_tokens, 2 * 1074);

    c.abort();
    until("C left the queue", || pool(&gw).waiting == 0).await;
    assert_eq!(fake.get(|e| e.chats), 2, "C never reached the engine");

    drop(b);
    until("B's tokens came back", || pool(&gw).reserved_tokens == 1074).await;
    a.abort();
    until("A's tokens came back", || pool(&gw).reserved_tokens == 0).await;
    let report = pool(&gw);
    assert_eq!((report.running, report.waiting), (0, 0));
    // The pool is whole again: a new request is admitted at once.
    fake.release(1);
    let (status, answer) = send(&gw.url, "text/qwen", prompt("D", 950), Some(100)).await;
    assert_eq!((status, content(&answer)), (200, "D".to_string()));
}

// A reload that shrinks the pool cuts no request already admitted; new
// ones are admitted against the new size.
#[tokio::test]
async fn a_reload_that_shrinks_the_pool_cuts_nothing() {
    let (fake_url, fake) = start_fake(3000).await;
    let gw = start_gateway(&fake_url, "    pool_admission: { count_above_tokens: 100 }").await;
    let mut running = Vec::new();
    for code in ["A", "B"] {
        let url = gw.url.clone();
        running.push(tokio::spawn(async move {
            send(&url, "text/qwen", prompt(code, 950), Some(100)).await
        }));
    }
    until("A and B running", || fake.get(|e| e.running) == 2).await;
    // The model is reloaded with a smaller context; the gateway reads the
    // listing again.
    fake.engine.lock().unwrap().ctx = 2000;
    assert!(reqwest::get(format!("{}/v1/engines", gw.url))
        .await
        .unwrap()
        .status()
        .is_success());
    let url = gw.url.clone();
    let c = tokio::spawn(async move { send(&url, "text/qwen", prompt("C", 950), Some(100)).await });
    until("C waits under the new size", || pool(&gw).waiting == 1).await;
    assert_eq!(pool(&gw).pool_tokens, Some(2000));
    assert_eq!(
        pool(&gw).reserved_tokens,
        2 * 1074,
        "nothing admitted was cut"
    );
    fake.release(1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pool(&gw).waiting, 1, "1074 + 1074 > 2000: C still waits");
    fake.release(2);
    let (status, answer) = c.await.unwrap();
    assert_eq!((status, content(&answer)), (200, "C".to_string()));
    for task in running {
        assert_eq!(task.await.unwrap().0, 200, "A and B were never cut");
    }
}

// A full pool on a route with another target spills there at once
// (`on_full: spill`, the default); a request that waits its lane's whole
// queue timeout gets a retryable 503.
#[tokio::test]
async fn a_full_pool_spills_or_times_out() {
    let (fake_url, fake) = start_fake(3000).await;
    let gw = start_gateway(
        &fake_url,
        "    queue_timeout_secs: 1\n    pool_admission: { count_above_tokens: 100 }",
    )
    .await;
    let mut running = Vec::new();
    for code in ["A", "B"] {
        let url = gw.url.clone();
        running.push(tokio::spawn(async move {
            send(&url, "text/qwen", prompt(code, 950), Some(100)).await
        }));
    }
    until("A and B running", || fake.get(|e| e.running) == 2).await;
    let (status, answer) = send(&gw.url, "mix", prompt("SPILL", 950), Some(100)).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(content(&answer), "SPILL");
    assert_eq!(
        fake.get(|e| e.all_chats.last().cloned()),
        Some("cloud".to_string())
    );
    let (status, answer) = send(&gw.url, "text/qwen", prompt("LATE", 950), Some(100)).await;
    assert_eq!(status, 503, "{answer}");
    assert_eq!(
        answer["error"]["type"], "local_lane_queue_timeout",
        "{answer}"
    );
    assert_eq!(pool(&gw).timed_out_total, 1);
    fake.release(2);
    for task in running {
        assert_eq!(task.await.unwrap().0, 200);
    }
}

// The capacity report shows each model's pool.
#[tokio::test]
async fn the_capacity_report_shows_the_pool() {
    let (fake_url, fake) = start_fake(3000).await;
    let gw = start_gateway(&fake_url, "").await;
    fake.release(1);
    let (status, _) = send(&gw.url, "text/qwen", prompt("X", 50), Some(10)).await;
    assert_eq!(status, 200);
    let report: Value = reqwest::get(format!("{}/v1/workloads/capacity", gw.url))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lane = report["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lane| lane["name"] == "text")
        .unwrap();
    let pools = lane["token_pools"].as_array().unwrap();
    assert_eq!(pools.len(), 1, "{lane}");
    assert_eq!(pools[0]["model"], "qwen");
    assert_eq!(pools[0]["pool_tokens"], 3000);
    assert_eq!(pools[0]["kv_unified"], true);
    assert_eq!(pools[0]["source"], "engine");
    assert_eq!(pools[0]["admitted_total"], 1);
    assert_eq!(pools[0]["reserved_tokens"], 0);
    // A small prompt is never counted by the engine (no call on the way).
    assert_eq!(fake.get(|e| e.tokenize_calls), 0);
}

// A full pool spills only to a later target that could take the request.
// Route [big, small]: the big pool is full and the prompt alone is over the
// small model's context, so spilling would end as a 400 "too large" at the
// small one although the big one serves it after a short wait. It waits for
// room on the big one instead and is answered there.
#[tokio::test]
async fn a_full_pool_never_spills_to_a_target_too_small_for_the_request() {
    let (big_url, big) = start_fake(3000).await;
    let (small_url, small) = start_fake(200).await;
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "{big_url}/v1"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: small
    type: openai_compatible
    engine: llama_cpp
    base_url: "{small_url}/v1"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: mix
    match: {{ model: mix }}
    pin_order: true
    targets: [text/qwen, small/qwen]
local_executors:
  - name: text
    base_url: "{big_url}"
    health_endpoint: "{big_url}/health"
    queue_timeout_secs: 5
    engine:
      kind: llama_cpp_router
  - name: small
    base_url: "{small_url}"
    health_endpoint: "{small_url}/health"
    engine:
      kind: llama_cpp_router
"#
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
    let url = format!("http://{addr}");
    let engines = reqwest::get(format!("{url}/v1/engines")).await.unwrap();
    assert!(engines.status().is_success());

    let mut running = Vec::new();
    for code in ["A", "B"] {
        let url = url.clone();
        running.push(tokio::spawn(async move {
            send(&url, "text/qwen", prompt(code, 950), Some(100)).await
        }));
    }
    until("A and B running", || big.get(|e| e.running) == 2).await;
    let waiter = {
        let url = url.clone();
        tokio::spawn(async move { send(&url, "mix", prompt("C", 950), Some(100)).await })
    };
    until("C waiting for room on the big pool", || {
        state
            .engine
            .token_pools()
            .reports()
            .into_iter()
            .any(|pool| pool.lane == "text" && pool.waiting == 1)
    })
    .await;
    assert_eq!(
        small.get(|e| e.all_chats.len()),
        0,
        "nothing reached the small model"
    );
    big.release(3);
    let (status, answer) = waiter.await.unwrap();
    assert_eq!(status, 200, "{answer}");
    assert_eq!(content(&answer), "C");
    assert_eq!(big.get(|e| e.overflowed), 0);
    for task in running {
        assert_eq!(task.await.unwrap().0, 200);
    }
}
