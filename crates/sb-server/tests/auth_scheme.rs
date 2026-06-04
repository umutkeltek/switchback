//! "Auth as data": an OpenAI-shaped provider configured with a non-bearer
//! `auth_scheme` attaches its key the way the config says — no new adapter. The
//! fake upstream reports which auth it received; we assert the key rode as
//! `x-api-key`, not `Authorization: Bearer`.

use std::sync::Arc;

use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// Fake OpenAI `/chat/completions` that echoes the auth it saw into the message.
async fn fake_openai(headers: HeaderMap) -> Json<Value> {
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("absent")
        .to_string();
    let authorization = if headers.contains_key("authorization") {
        "present"
    } else {
        "absent"
    };
    Json(json!({
        "id": "chatcmpl-fake",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {
                "role": "assistant",
                "content": format!("x-api-key={x_api_key} authorization={authorization}")
            }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    }))
}

/// Fake Anthropic `/v1/messages` that echoes the auth it saw into the content.
async fn fake_anthropic(headers: HeaderMap) -> Json<Value> {
    let x_api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("absent")
        .to_string();
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("absent")
        .to_string();
    Json(json!({
        "id": "msg_fake",
        "type": "message",
        "role": "assistant",
        "model": "claude-test",
        "content": [{
            "type": "text",
            "text": format!("x-api-key={x_api_key} authorization={authorization}")
        }],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    }))
}

async fn spawn_fake_openai() -> String {
    let app = Router::new().route("/chat/completions", post(fake_openai));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

async fn spawn_fake_anthropic() -> String {
    let app = Router::new().route("/v1/messages", post(fake_anthropic));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
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
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn openai_shaped_provider_with_header_auth_is_pure_config() {
    let upstream = spawn_fake_openai().await;
    // OpenAI wire format, but authenticates with x-api-key — declared as DATA.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: weird
    type: openai_compatible
    base_url: "{upstream}"
    auth_scheme: {{ kind: header, name: x-api-key }}
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "secret-xyz" }}
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "weird/some-model"
"#
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"some-model","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // The key rode as x-api-key, and NOT as Authorization: Bearer — proving the
    // auth scheme is composed from config, not hardcoded bearer.
    let content = resp["choices"][0]["message"]["content"].as_str().unwrap();
    assert_eq!(
        content, "x-api-key=secret-xyz authorization=absent",
        "got: {content}"
    );
}

#[tokio::test]
async fn anthropic_provider_can_use_bearer_for_claude_code_oauth() {
    let upstream = spawn_fake_anthropic().await;
    let mut token_file = std::env::temp_dir();
    token_file.push(format!(
        "sb-claude-code-auth-scheme-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&token_file);
    std::fs::write(
        &token_file,
        r#"{"claudeAiOauth":{"accessToken":"claude-oauth-token"}}"#,
    )
    .unwrap();
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: anthropic-oauth
    type: anthropic
    base_url: "{upstream}"
    auth_scheme: {{ kind: bearer }}
    accounts:
      - id: claude-code
        auth:
          kind: claude_code_oauth
          token_env: null
          token_file: "{}"
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "anthropic-oauth/claude-test"
"#,
        token_file.display()
    );
    let switchback = spawn_switchback(&cfg).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/messages"))
        .json(&json!({
            "model":"claude-test",
            "max_tokens": 8,
            "messages":[{"role":"user","content":"hi"}]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let content = resp["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        content, "x-api-key=absent authorization=Bearer claude-oauth-token",
        "got: {content}"
    );
    std::fs::remove_file(token_file).ok();
}

#[tokio::test]
async fn providers_endpoint_reports_non_secret_auth_kinds() {
    let cfg = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mixed
    type: mock
    accounts:
      - id: api
        auth: { kind: api_key, inline: "sk-hidden" }
      - id: oauth
        auth: { kind: oauth, token: "tok-hidden", refresh: "refresh-hidden", token_url: "https://oauth.example.com/token" }
      - id: codex
        auth: { kind: codex_oauth, token_env: CODEX_ACCESS_TOKEN, token_file: "${HOME}/.codex/auth.json" }
      - id: claude
        auth: { kind: claude_code_oauth, token_env: CLAUDE_CODE_OAUTH_TOKEN, token_file: "${HOME}/.claude/.credentials.json" }
      - id: aws
        auth:
          kind: aws_sig_v4
          access_key: "ak-hidden"
          secret_key: "sec-hidden"
routes:
  - name: default
    match: { model: "*" }
    targets: ["mixed/echo"]
"#;
    let switchback = spawn_switchback(cfg).await;

    let providers: Value = reqwest::Client::new()
        .get(format!("{switchback}/v1/providers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let provider = &providers["providers"][0];
    assert_eq!(
        provider["auth_kinds"],
        json!([
            "api_key",
            "aws_sigv4",
            "claude_code_oauth",
            "codex_oauth",
            "oauth"
        ])
    );
    assert_eq!(
        provider["accounts_detail"][0],
        json!({"id":"api","auth_kind":"api_key","auth_sources":["inline"],"egress":null})
    );
    assert_eq!(provider["accounts_detail"][1]["auth_kind"], "oauth");
    assert_eq!(
        provider["accounts_detail"][1]["auth_sources"],
        json!(["access_token", "refresh_token"])
    );
    assert_eq!(provider["accounts_detail"][2]["auth_kind"], "codex_oauth");
    assert_eq!(
        provider["accounts_detail"][2]["auth_sources"],
        json!(["access_token_env", "native_token_file"])
    );
    assert_eq!(
        provider["accounts_detail"][3]["auth_kind"],
        "claude_code_oauth"
    );
    assert_eq!(
        provider["accounts_detail"][3]["auth_sources"],
        json!(["access_token_env", "native_token_file"])
    );
    assert_eq!(provider["accounts_detail"][4]["auth_kind"], "aws_sigv4");
    let serialized = serde_json::to_string(&providers).unwrap();
    assert!(
        !serialized.contains("hidden"),
        "provider view leaked secret material"
    );
}
