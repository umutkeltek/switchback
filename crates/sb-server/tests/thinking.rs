//! Thinking text from OpenAI-compatible engines (llama.cpp and vLLM stream it
//! as `reasoning_content`, some servers as `reasoning`) through the one
//! pipeline: the adapter decodes it into the canonical thinking event, and
//! `/v1/chat/completions` renders it back as `reasoning_content` on stream
//! deltas and on the collected message, only when the upstream produced any.
//! `/v1/messages` and `/v1/responses` render it in their own thinking form.

use std::sync::Arc;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// What the fake engine answers.
#[derive(Clone)]
struct Engine {
    /// Thinking chunks, sent under `field`.
    thinking: Vec<&'static str>,
    field: &'static str,
    answer: Vec<&'static str>,
    finish: &'static str,
    /// Every request body the engine received.
    seen: Arc<std::sync::Mutex<Vec<Value>>>,
}

fn chunk(delta: Value, finish: Value) -> String {
    let chunk = json!({
        "id": "chatcmpl-upstream",
        "object": "chat.completion.chunk",
        "created": 1,
        "model": "engine-model",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    });
    format!("data: {chunk}\n\n")
}

async fn engine_chat(State(engine): State<Engine>, Json(body): Json<Value>) -> Response {
    engine.seen.lock().unwrap().push(body.clone());
    if body["stream"].as_bool().unwrap_or(false) {
        let mut frames = chunk(json!({"role": "assistant"}), Value::Null);
        for text in &engine.thinking {
            frames.push_str(&chunk(json!({ engine.field: text }), Value::Null));
        }
        for text in &engine.answer {
            frames.push_str(&chunk(json!({ "content": text }), Value::Null));
        }
        frames.push_str(&chunk(json!({}), json!(engine.finish)));
        frames.push_str("data: [DONE]\n\n");
        return ([("content-type", "text/event-stream")], frames).into_response();
    }
    let answer = engine.answer.concat();
    let mut message = json!({
        "role": "assistant",
        "content": if answer.is_empty() { Value::Null } else { json!(answer) },
    });
    if !engine.thinking.is_empty() {
        message[engine.field] = json!(engine.thinking.concat());
    }
    Json(json!({
        "id": "chatcmpl-upstream",
        "object": "chat.completion",
        "created": 1,
        "model": "engine-model",
        "choices": [{"index": 0, "message": message, "finish_reason": engine.finish}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 7},
    }))
    .into_response()
}

/// A gateway in front of a fake engine that answers as `engine` says.
async fn gateway(engine: Engine) -> String {
    gateway_with(engine, "").await
}

/// [`gateway`] with extra `server:` settings (one indented YAML line).
async fn gateway_with(engine: Engine, server: &str) -> String {
    let upstream = Router::new()
        .route("/v1/chat/completions", post(engine_chat))
        .with_state(engine);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let engine_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });

    let cfg = sb_core::Config::from_yaml(&format!(
        r#"
server:
  bind: "127.0.0.1:0"
{server}
providers:
  - id: engine
    type: openai_compatible
    engine: llama_cpp
    base_url: "{engine_url}/v1"
routes:
  - name: default
    match: {{ model: "*" }}
    targets: ["engine/engine-model"]
"#
    ))
    .unwrap();
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

fn thinking_then_answer() -> Engine {
    Engine {
        thinking: vec!["Let me ", "think."],
        field: "reasoning_content",
        answer: vec!["Hi", "!"],
        finish: "stop",
        seen: Arc::default(),
    }
}

async fn post_json(base: &str, path: &str, body: Value) -> (String, String) {
    let response = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let request_id = response
        .headers()
        .get("x-switchback-request-id")
        .map(|value| value.to_str().unwrap().to_string())
        .unwrap_or_default();
    (response.text().await.unwrap(), request_id)
}

async fn chat(base: &str, stream: bool) -> (String, String) {
    post_json(
        base,
        "/v1/chat/completions",
        json!({
            "model": "engine/engine-model",
            "stream": stream,
            "messages": [{"role": "user", "content": "hello"}],
        }),
    )
    .await
}

/// The `delta` and `finish_reason` of every chunk of a chat stream.
fn deltas(body: &str) -> Vec<(Value, Value)> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
        .filter_map(|chunk| {
            let choice = chunk["choices"].get(0)?.clone();
            Some((choice["delta"].clone(), choice["finish_reason"].clone()))
        })
        .collect()
}

async fn trace(base: &str, request_id: &str) -> Value {
    reqwest::get(format!("{base}/v1/traces/{request_id}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_stream_carries_thinking_as_reasoning_content_before_the_answer() {
    let base = gateway(thinking_then_answer()).await;
    let (body, request_id) = chat(&base, true).await;
    let deltas: Vec<Value> = deltas(&body).into_iter().map(|(delta, _)| delta).collect();
    assert_eq!(
        deltas,
        vec![
            json!({"role": "assistant"}),
            json!({"reasoning_content": "Let me "}),
            json!({"reasoning_content": "think."}),
            json!({"content": "Hi"}),
            json!({"content": "!"}),
            json!({}),
        ],
        "{body}"
    );
    let trace = trace(&base, &request_id).await;
    assert!(trace["ttft_ms"].is_u64(), "{trace}");
}

#[tokio::test]
async fn a_collected_response_carries_reasoning_content_on_the_message() {
    let base = gateway(thinking_then_answer()).await;
    let (body, _) = chat(&base, false).await;
    let response: Value = serde_json::from_str(&body).unwrap();
    let message = &response["choices"][0]["message"];
    assert_eq!(message["reasoning_content"], "Let me think.", "{body}");
    assert_eq!(message["content"], "Hi!");
}

// Seen live: a stream that only thinks (here until `max_tokens`) showed the
// client nothing for 23 s and its trace had no `ttft_ms`.
#[tokio::test]
async fn a_stream_that_only_thinks_shows_it_and_records_ttft() {
    let base = gateway(Engine {
        answer: vec![],
        finish: "length",
        ..thinking_then_answer()
    })
    .await;
    let (body, request_id) = chat(&base, true).await;
    let deltas = deltas(&body);
    assert_eq!(
        deltas[1..],
        [
            (json!({"reasoning_content": "Let me "}), Value::Null),
            (json!({"reasoning_content": "think."}), Value::Null),
            (json!({}), json!("length")),
        ],
        "{body}"
    );
    let trace = trace(&base, &request_id).await;
    assert!(trace["ttft_ms"].is_u64(), "{trace}");

    let (body, _) = chat(&base, false).await;
    let response: Value = serde_json::from_str(&body).unwrap();
    let choice = &response["choices"][0];
    assert_eq!(choice["finish_reason"], "length");
    assert_eq!(choice["message"]["content"], Value::Null);
    assert_eq!(choice["message"]["reasoning_content"], "Let me think.");
}

// Some servers name the field `reasoning`; it is read the same way.
#[tokio::test]
async fn the_reasoning_field_is_read_too() {
    let base = gateway(Engine {
        field: "reasoning",
        ..thinking_then_answer()
    })
    .await;
    let (body, _) = chat(&base, true).await;
    assert!(deltas(&body).contains(&(json!({"reasoning_content": "think."}), Value::Null)));
    let (body, _) = chat(&base, false).await;
    let response: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        response["choices"][0]["message"]["reasoning_content"],
        "Let me think."
    );
}

// No thinking upstream: nothing about thinking downstream, the chunks and
// the message exactly as before.
#[tokio::test]
async fn without_thinking_the_output_is_unchanged() {
    let base = gateway(Engine {
        thinking: vec![],
        ..thinking_then_answer()
    })
    .await;
    let (body, _) = chat(&base, true).await;
    assert!(!body.contains("reasoning"), "{body}");
    let deltas: Vec<Value> = deltas(&body).into_iter().map(|(delta, _)| delta).collect();
    assert_eq!(
        deltas,
        vec![
            json!({"role": "assistant"}),
            json!({"content": "Hi"}),
            json!({"content": "!"}),
            json!({}),
        ]
    );
    let (body, _) = chat(&base, false).await;
    assert!(!body.contains("reasoning"), "{body}");
    let response: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        response["choices"][0]["message"],
        json!({"role": "assistant", "content": "Hi!"})
    );
}

async fn post_status(base: &str, path: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

// Thinking is never sent back upstream: a chat client that echoes the
// `reasoning_content` it got on its assistant message sends the engine only
// the answer, so a server that refuses it in input (DeepSeek-style) is safe.
#[tokio::test]
async fn thinking_a_chat_client_echoes_is_not_sent_upstream() {
    let engine = thinking_then_answer();
    let seen = engine.seen.clone();
    let base = gateway(engine).await;
    let (body, _) = chat(&base, false).await;
    let answer: Value = serde_json::from_str(&body).unwrap();
    let echoed = answer["choices"][0]["message"].clone();
    assert_eq!(echoed["reasoning_content"], "Let me think.", "{body}");
    let (status, reply) = post_status(
        &base,
        "/v1/chat/completions",
        json!({
            "model": "engine/engine-model",
            "messages": [
                {"role": "user", "content": "hello"},
                echoed,
                {"role": "user", "content": "and now?"},
            ],
        }),
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    let upstream = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(upstream["messages"][1]["content"], "Hi!", "{upstream}");
    assert!(!upstream.to_string().contains("reasoning"), "{upstream}");
    assert!(!upstream.to_string().contains("Let me"), "{upstream}");
}

// A client of `/v1/messages` (Claude Code, an Anthropic SDK) sends the
// assistant turn it received back on its next turn, as it came. Switchback's
// own Anthropic ingress has no input form for a `thinking` block, so the
// first answer must not carry one by default: the second turn must work.
#[tokio::test]
async fn a_messages_client_that_replays_the_answer_gets_its_next_turn() {
    let base = gateway(thinking_then_answer()).await;
    let first = json!({
        "model": "engine/engine-model",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello"}],
    });
    let (status, answer) = post_status(&base, "/v1/messages", first).await;
    assert_eq!(status, 200, "{answer}");
    let (status, error) = post_status(
        &base,
        "/v1/messages",
        json!({
            "model": "engine/engine-model",
            "max_tokens": 64,
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": answer["content"].clone()},
                {"role": "user", "content": "and now?"},
            ],
        }),
    )
    .await;
    assert_eq!(status, 200, "first answer {answer}, second {error}");
}

// The same for `/v1/responses`: a Responses client (Codex) puts the output
// items of a turn into the input of the next one.
#[tokio::test]
async fn a_responses_client_that_replays_the_output_gets_its_next_turn() {
    let base = gateway(thinking_then_answer()).await;
    let (status, answer) = post_status(
        &base,
        "/v1/responses",
        json!({"model": "engine/engine-model", "input": "hello"}),
    )
    .await;
    assert_eq!(status, 200, "{answer}");
    let mut input = vec![json!({"role": "user", "content": "hello"})];
    input.extend(answer["output"].as_array().cloned().unwrap_or_default());
    input.push(json!({"role": "user", "content": "and now?"}));
    let (status, error) = post_status(
        &base,
        "/v1/responses",
        json!({"model": "engine/engine-model", "input": input}),
    )
    .await;
    assert_eq!(status, 200, "first answer {answer}, second {error}");
}

async fn messages_and_responses_streams(base: &str) -> (String, String) {
    let (messages, _) = post_json(
        base,
        "/v1/messages",
        json!({
            "model": "engine/engine-model",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "hello"}],
        }),
    )
    .await;
    let (responses, _) = post_json(
        base,
        "/v1/responses",
        json!({"model": "engine/engine-model", "stream": true, "input": "hello"}),
    )
    .await;
    (messages, responses)
}

// By default `/v1/messages` and `/v1/responses` show no thinking from an
// OpenAI-compatible engine, as before it was decoded: their clients would
// send it back, and the next turn would fail. The answer is unchanged.
#[tokio::test]
async fn messages_and_responses_keep_engine_thinking_off_by_default() {
    let base = gateway(thinking_then_answer()).await;
    let (messages, responses) = messages_and_responses_streams(&base).await;
    assert!(!messages.contains("thinking"), "{messages}");
    assert!(messages.contains(r#""text":"Hi""#), "{messages}");
    assert!(!responses.contains("reasoning"), "{responses}");
    assert!(responses.contains(r#""delta":"Hi""#), "{responses}");
}

// With `openai_compatible_thinking_on_all_formats` the other ingress formats
// render it in their own form: Anthropic `thinking` blocks and Responses
// `reasoning` items.
#[tokio::test]
async fn messages_and_responses_render_it_in_their_own_form_when_enabled() {
    let base = gateway_with(
        thinking_then_answer(),
        "  openai_compatible_thinking_on_all_formats: true",
    )
    .await;
    let (body, _) = post_json(
        &base,
        "/v1/messages",
        json!({
            "model": "engine/engine-model",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "hello"}],
        }),
    )
    .await;
    assert!(
        body.contains(r#""delta":{"thinking":"Let me ","type":"thinking_delta"}"#),
        "{body}"
    );

    let (body, _) = post_json(
        &base,
        "/v1/responses",
        json!({"model": "engine/engine-model", "stream": true, "input": "hello"}),
    )
    .await;
    assert!(
        body.contains("response.reasoning_summary_text.delta"),
        "{body}"
    );
    assert!(body.contains(r#""delta":"think.""#), "{body}");
}
