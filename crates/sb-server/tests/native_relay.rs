//! Native subscription relay tests. These use fake upstreams and fake local
//! credential files, but exercise the real Switchback server path end to end.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone, Default)]
struct SeenUpstream {
    auth: Arc<Mutex<Vec<String>>>,
    billing: Arc<Mutex<Vec<String>>>,
    version: Arc<Mutex<Vec<String>>>,
    chatgpt_account: Arc<Mutex<Vec<String>>>,
    models: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

static TEMP_CREDENTIAL_COUNTER: AtomicU64 = AtomicU64::new(0);

async fn fake_codex_responses(
    State(seen): State<SeenUpstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    seen.auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>")
            .to_string(),
    );
    seen.chatgpt_account.lock().unwrap().push(
        headers
            .get("chatgpt-account-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>")
            .to_string(),
    );
    seen.models.lock().unwrap().push(
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_string(),
    );
    seen.bodies.lock().unwrap().push(body);
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_native_fake","object":"response","status":"in_progress","model":"gpt-test","output":[]}}

"#,
        r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"codex-native-ok"}

"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"id":"resp_native_fake","object":"response","status":"completed","model":"gpt-test","output":[],"usage":{"input_tokens":2,"input_tokens_details":{"cached_tokens":0},"output_tokens":1,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":3}}}

"#,
    ]
    .join("");
    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        sse,
    )
}

/// A fake Codex backend that streams a reasoning summary then a tool call —
/// exercises the agentic decode -> collect -> egress path through the real
/// server (the function_call / reasoning frames the live backend emits).
async fn fake_codex_responses_agentic(
    State(seen): State<SeenUpstream>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    seen.bodies.lock().unwrap().push(body);
    let sse = [
        r#"event: response.created
data: {"type":"response.created","response":{"id":"resp_a","model":"gpt-test"}}

"#,
        r#"event: response.reasoning_summary_text.delta
data: {"type":"response.reasoning_summary_text.delta","delta":"weighing the tool"}

"#,
        r#"event: response.output_item.added
data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":""}}

"#,
        r#"event: response.function_call_arguments.delta
data: {"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"city\":\"Paris\"}"}

"#,
        r#"event: response.function_call_arguments.done
data: {"type":"response.function_call_arguments.done","output_index":1,"arguments":"{\"city\":\"Paris\"}"}

"#,
        r#"event: response.completed
data: {"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":3,"total_tokens":8}}}

"#,
    ]
    .join("");
    (
        [
            ("content-type", "text/event-stream"),
            ("cache-control", "no-cache"),
        ],
        sse,
    )
}

async fn fake_claude_messages(
    State(seen): State<SeenUpstream>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    seen.auth.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>")
            .to_string(),
    );
    seen.billing.lock().unwrap().push(
        headers
            .get("x-anthropic-billing-header")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>")
            .to_string(),
    );
    seen.version.lock().unwrap().push(
        headers
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<none>")
            .to_string(),
    );
    seen.models.lock().unwrap().push(
        body.get("model")
            .and_then(Value::as_str)
            .unwrap_or("<missing>")
            .to_string(),
    );
    Json(json!({
        "id": "msg_native_fake",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{ "type": "text", "text": "native-ok" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 2, "output_tokens": 1 }
    }))
}

async fn spawn(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn spawn_switchback(cfg_yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    spawn(sb_server::build_app(state)).await
}

fn temp_credential_path() -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = TEMP_CREDENTIAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "switchback-native-relay-{}-{seq}-{nanos}.json",
        std::process::id()
    ))
}

#[tokio::test]
async fn codex_native_relay_uses_native_oauth_account_and_responses_wire() {
    let credentials = temp_credential_path();
    std::fs::write(
        &credentials,
        r#"{"tokens":{"access_token":"fake-codex-access","account_id":"fake-chatgpt-account"}}"#,
    )
    .unwrap();

    let seen = SeenUpstream::default();
    let upstream = spawn(
        Router::new()
            .route("/responses", post(fake_codex_responses))
            .with_state(seen.clone()),
    )
    .await;

    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: codex-native
    type: codex_native_relay
    base_url: "{upstream}"
    accounts:
      - id: local-codex
        auth:
          kind: codex_oauth
          token_file: "{}"
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "codex-native/gpt-test"
"#,
        credentials.display()
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/responses"))
        .json(&json!({
            "model": "gpt-test",
            "input": "hi",
            "max_output_tokens": 100
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["object"], "response");
    assert_eq!(resp["output"][0]["content"][0]["text"], "codex-native-ok");
    assert_eq!(
        seen.auth.lock().unwrap().as_slice(),
        ["Bearer fake-codex-access"]
    );
    assert_eq!(
        seen.chatgpt_account.lock().unwrap().as_slice(),
        ["fake-chatgpt-account"]
    );
    assert_eq!(seen.models.lock().unwrap().as_slice(), ["gpt-test"]);
    let bodies = seen.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["stream"], true);
    assert_eq!(bodies[0]["store"], false);
    assert!(bodies[0].get("max_output_tokens").is_none());
    assert!(bodies[0]["instructions"]
        .as_str()
        .unwrap()
        .contains("Codex"));

    let _ = std::fs::remove_file(credentials);
}

#[tokio::test]
async fn codex_native_relay_surfaces_reasoning_and_tool_calls() {
    let credentials = temp_credential_path();
    std::fs::write(
        &credentials,
        r#"{"tokens":{"access_token":"fake-codex-access","account_id":"acct"}}"#,
    )
    .unwrap();

    let seen = SeenUpstream::default();
    let upstream = spawn(
        Router::new()
            .route("/responses", post(fake_codex_responses_agentic))
            .with_state(seen.clone()),
    )
    .await;

    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: codex-native
    type: codex_native_relay
    base_url: "{upstream}"
    accounts:
      - id: local-codex
        auth:
          kind: codex_oauth
          token_file: "{}"
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "codex-native/gpt-test"
"#,
        credentials.display()
    );
    let switchback = spawn_switchback(&cfg).await;

    // Non-stream client request: forces the collect path over the forced-stream
    // upstream, proving reasoning + tool calls survive both decode and collect.
    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/responses"))
        .json(&json!({
            "model": "gpt-test",
            "input": "weather in Paris?",
            "tools": [{"type":"function","name":"get_weather",
                "parameters":{"type":"object","properties":{"city":{"type":"string"}}}}]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let output = resp["output"].as_array().expect("output array");
    // Reasoning leads, then the function call (no answer text in this fixture).
    assert!(
        output
            .iter()
            .any(|i| i["type"] == "reasoning" && i["summary"][0]["text"] == "weighing the tool"),
        "reasoning item present: {output:?}"
    );
    let call = output
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("function_call item");
    assert_eq!(call["name"], "get_weather");
    assert_eq!(call["arguments"], r#"{"city":"Paris"}"#);
    assert_eq!(call["call_id"], "call_1");

    let _ = std::fs::remove_file(credentials);
}

#[tokio::test]
async fn codex_native_relay_reissues_tool_result_turn() {
    let credentials = temp_credential_path();
    std::fs::write(
        &credentials,
        r#"{"tokens":{"access_token":"fake-codex-access","account_id":"acct"}}"#,
    )
    .unwrap();

    let seen = SeenUpstream::default();
    let upstream = spawn(
        Router::new()
            .route("/responses", post(fake_codex_responses))
            .with_state(seen.clone()),
    )
    .await;

    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: codex-native
    type: codex_native_relay
    base_url: "{upstream}"
    accounts:
      - id: local-codex
        auth:
          kind: codex_oauth
          token_file: "{}"
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "codex-native/gpt-test"
"#,
        credentials.display()
    );
    let switchback = spawn_switchback(&cfg).await;

    let _resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/responses"))
        .json(&json!({
            "model": "gpt-test",
            "input": [
                {"type":"message","role":"user","content":[{"type":"input_text","text":"inspect this"}]},
                {"type":"function_call","call_id":"call_1","name":"inspect","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":[
                    {"type":"input_text","text":"tool text"},
                    {"type":"input_image","image_url":"data:image/png;base64,abc","detail":"low"}
                ]}
            ]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let bodies = seen.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let input = bodies[0]["input"].as_array().expect("upstream input");
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("function_call reissued");
    let result = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .expect("function_call_output reissued");
    assert_eq!(call["call_id"], "call_1");
    assert_eq!(result["call_id"], "call_1");
    let output = result["output"]
        .as_array()
        .expect("structured output preserved");
    assert_eq!(output[0]["type"], "input_text");
    assert_eq!(output[0]["text"], "tool text");
    assert_eq!(output[1]["type"], "input_image");
    assert_eq!(output[1]["image_url"], "data:image/png;base64,abc");
    assert_eq!(output[1]["detail"], "low");

    let _ = std::fs::remove_file(credentials);
}

#[tokio::test]
async fn claude_code_native_relay_uses_native_oauth_and_first_party_headers() {
    let credentials = temp_credential_path();
    std::fs::write(
        &credentials,
        r#"{"claudeAiOauth":{"accessToken":"fake-native-access"}}"#,
    )
    .unwrap();

    let seen = SeenUpstream::default();
    let upstream = spawn(
        Router::new()
            .route("/v1/messages", post(fake_claude_messages))
            .with_state(seen.clone()),
    )
    .await;

    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: claude-native
    type: claude_code_native_relay
    base_url: "{upstream}"
    accounts:
      - id: local-claude
        auth:
          kind: claude_code_oauth
          token_file: "{}"
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "claude-native/claude-test"
"#,
        credentials.display()
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/messages"))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 100,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(resp["type"], "message");
    assert_eq!(resp["content"][0]["text"], "native-ok");

    assert_eq!(
        seen.auth.lock().unwrap().as_slice(),
        ["Bearer fake-native-access"]
    );
    assert_eq!(
        seen.version.lock().unwrap().as_slice(),
        [sb_protocols::anthropic::ANTHROPIC_VERSION]
    );
    let billing = seen.billing.lock().unwrap().clone();
    assert_eq!(billing.len(), 1);
    assert!(
        billing[0].contains("cc_entrypoint=switchback-native-relay"),
        "missing native relay attribution: {billing:?}"
    );
    assert_eq!(seen.models.lock().unwrap().as_slice(), ["claude-test"]);

    let _ = std::fs::remove_file(credentials);
}
