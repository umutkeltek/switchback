//! Native-client compatibility profiles: Codex and Claude Code can point at
//! Switchback's proxy endpoints while credentials/accounts remain owned by
//! Switchback config.

use std::sync::Arc;

use serde_json::{json, Value};

const CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: codex
        auth:
          kind: oauth
          token: "tok-hidden"
          refresh: "refresh-hidden"
          token_url: "https://oauth.example/token"
      - id: claude
        auth: { kind: api_key, inline: "sk-hidden" }
client_profiles:
  - id: codex
    kind: codex
    models: ["codex-native"]
    accounts: ["mock/codex"]
  - id: claude-code
    kind: claude_code
    models: ["claude-native"]
    accounts: ["mock/claude"]
routes:
  - name: codex
    match: { model: "codex-native" }
    targets: ["mock/echo"]
  - name: claude
    match: { model: "claude-native" }
    targets: ["mock/echo"]
"#;

async fn spawn() -> String {
    let cfg = sb_core::Config::from_yaml(CFG).unwrap();
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
async fn client_profiles_report_switchback_accounts_without_secrets() {
    let base = spawn().await;
    let profiles: Value = reqwest::Client::new()
        .get(format!("{base}/v1/client-profiles"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(profiles["metadata_only"], true);
    let codex = profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|profile| profile["id"] == "codex")
        .unwrap();
    assert_eq!(codex["ready"], true);
    assert_eq!(codex["protocol"], "openai_responses");
    assert_eq!(codex["required_endpoints"][0], "/v1/responses");
    assert_eq!(codex["accounts"]["checks"][0]["ref"], "mock/codex");
    assert_eq!(codex["accounts"]["checks"][0]["configured"], true);
    assert_eq!(codex["accounts"]["checks"][0]["healthy"], true);
    assert_eq!(codex["accounts"]["checks"][0]["auth_kind"], "oauth");
    assert_eq!(
        codex["accounts"]["checks"][0]["auth_sources"],
        json!(["access_token", "refresh_token"])
    );

    let claude = profiles["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|profile| profile["id"] == "claude-code")
        .unwrap();
    assert_eq!(claude["ready"], true);
    assert_eq!(claude["protocol"], "anthropic_messages");
    assert_eq!(
        claude["required_endpoints"],
        json!(["/v1/messages", "/v1/messages/count_tokens"])
    );
    assert_eq!(claude["accounts"]["checks"][0]["ref"], "mock/claude");
    assert_eq!(claude["accounts"]["checks"][0]["configured"], true);
    assert_eq!(claude["accounts"]["checks"][0]["healthy"], true);
    assert_eq!(claude["accounts"]["checks"][0]["auth_kind"], "api_key");

    let serialized = serde_json::to_string(&profiles).unwrap();
    assert!(!serialized.contains("hidden"));
}

#[tokio::test]
async fn codex_responses_and_claude_messages_stamp_native_profile_metadata() {
    let base = spawn().await;
    let client = reqwest::Client::new();

    let codex_resp = client
        .post(format!("{base}/v1/responses"))
        .header("x-codex-session-id", "codex-session-1")
        .json(&json!({
            "model": "codex-native",
            "input": "hi from codex"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        codex_resp
            .headers()
            .get("x-switchback-client-profile")
            .unwrap()
            .to_str()
            .unwrap(),
        "codex"
    );
    let codex_req_id = codex_resp
        .headers()
        .get("x-switchback-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let codex_body: Value = codex_resp.json().await.unwrap();
    assert_eq!(codex_body["object"], "response");

    let codex_trace: Value = client
        .get(format!("{base}/v1/traces/{codex_req_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(codex_trace["client_profile"], "codex");
    assert_eq!(codex_trace["client_protocol"], "openai_responses");
    assert_eq!(codex_trace["session_id"], "codex-session-1");

    let claude_resp = client
        .post(format!("{base}/v1/messages"))
        .header("x-switchback-session-id", "claude-session-1")
        .json(&json!({
            "model": "claude-native",
            "messages": [{"role": "user", "content": "hi from claude"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        claude_resp
            .headers()
            .get("x-switchback-client-profile")
            .unwrap()
            .to_str()
            .unwrap(),
        "claude-code"
    );
    let claude_req_id = claude_resp
        .headers()
        .get("x-switchback-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let claude_body: Value = claude_resp.json().await.unwrap();
    assert_eq!(claude_body["type"], "message");

    let claude_trace: Value = client
        .get(format!("{base}/v1/traces/{claude_req_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(claude_trace["client_profile"], "claude-code");
    assert_eq!(claude_trace["client_protocol"], "anthropic_messages");
    assert_eq!(claude_trace["session_id"], "claude-session-1");
}
