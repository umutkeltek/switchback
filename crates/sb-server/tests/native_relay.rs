//! Native subscription relay tests. These use fake upstreams and fake local
//! credential files, but exercise the real Switchback server path end to end.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::HeaderMap;
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
}

async fn fake_codex_responses(
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
    Json(json!({
        "id": "resp_native_fake",
        "object": "response",
        "status": "completed",
        "model": "gpt-test",
        "output": [{
            "type": "message",
            "id": "msg_native_fake",
            "status": "completed",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "codex-native-ok", "annotations": [] }]
        }],
        "usage": {
            "input_tokens": 2,
            "input_tokens_details": { "cached_tokens": 0 },
            "output_tokens": 1,
            "output_tokens_details": { "reasoning_tokens": 0 },
            "total_tokens": 3
        }
    }))
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
    std::env::temp_dir().join(format!(
        "switchback-claude-native-relay-{}-{nanos}.json",
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
