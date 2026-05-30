//! Endpoint auth: when a key is configured, ALL `/v1/*` and `/cp/v1/*` endpoints
//! require it — not just the inference path. `/health` and `/` stay public; with
//! no key configured the gateway is open (local default).

use std::sync::Arc;

fn cfg_yaml(api_key_line: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
{api_key_line}
providers:
  - id: mock
    type: mock
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "mock/echo"
"#
    )
}

async fn spawn(yaml: &str) -> String {
    let cfg = sb_core::Config::from_yaml(yaml).unwrap();
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

async fn status(url: &str, key: Option<&str>) -> u16 {
    let mut rb = reqwest::Client::new().get(url);
    if let Some(k) = key {
        rb = rb.header("authorization", format!("Bearer {k}"));
    }
    rb.send().await.unwrap().status().as_u16()
}

const PROTECTED: &[&str] = &[
    "/v1/config",
    "/v1/providers",
    "/v1/models",
    "/v1/usage",
    "/v1/traces",
    "/v1/runtime",
    "/v1/revisions",
    "/v1/audit",
    "/v1/health",
    "/v1/tenants",
    "/v1/plugins",
    "/cp/v1",
    "/cp/v1/resources/providers",
];

#[tokio::test]
async fn configured_key_protects_all_read_endpoints() {
    let sb = spawn(&cfg_yaml(r#"  api_key: "topsecret""#)).await;

    // Public shell stays open.
    assert_eq!(status(&format!("{sb}/health"), None).await, 200);
    assert_eq!(status(&format!("{sb}/"), None).await, 200);

    // Every sensitive endpoint is 401 without the key, 200 with it.
    for path in PROTECTED {
        assert_eq!(
            status(&format!("{sb}{path}"), None).await,
            401,
            "{path} must require a key"
        );
        assert_eq!(
            status(&format!("{sb}{path}"), Some("topsecret")).await,
            200,
            "{path} must accept the key"
        );
    }

    // Wrong key is rejected.
    assert_eq!(status(&format!("{sb}/v1/config"), Some("nope")).await, 401);

    // Inference is gated too.
    let chat = reqwest::Client::new()
        .post(format!("{sb}/v1/chat/completions"))
        .json(&serde_json::json!({"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 401);
}

#[tokio::test]
async fn no_key_configured_is_open_local_default() {
    let sb = spawn(&cfg_yaml("")).await;
    // With no api_key/api_keys, read endpoints are open (local-first default).
    assert_eq!(status(&format!("{sb}/v1/config"), None).await, 200);
    assert_eq!(status(&format!("{sb}/v1/usage"), None).await, 200);
    assert_eq!(status(&format!("{sb}/health"), None).await, 200);
}
