//! Host holds and the client view of the capacity report over real HTTP,
//! with tenant keys: a client key sees and ends only its own holds, at most
//! eight at once, and reads a capacity report with no commands and nothing
//! of another tenant.

use std::sync::Arc;

use serde_json::{json, Value};

const CLIENT_A: &str = "sk-test-client-a";
const CLIENT_B: &str = "sk-test-client-b";
const OPERATOR: &str = "sk-test-operator";
const WAKE: &str = "/usr/bin/true wake-marker-command";

fn config() -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
  max_hold_secs: 7200
api_keys:
  - {{ key: "{CLIENT_A}", tenant: team-a }}
  - {{ key: "{CLIENT_B}", tenant: team-b }}
  - {{ key: "{OPERATOR}", tenant: ops, role: operator }}
tenants:
  - id: team-a
  - id: team-b
  - id: ops
providers:
  - id: text
    type: openai_compatible
    base_url: "http://127.0.0.1:9/v1"
local_executors:
  - name: text
    host: gpu-box
    base_url: "http://127.0.0.1:9"
    health_endpoint: "http://127.0.0.1:9/health"
    wake_command: "{WAKE}"
    poweroff_command: "/usr/bin/true poweroff-marker-command"
"#
    )
}

async fn serve() -> String {
    let cfg = sb_core::Config::from_yaml(&config()).unwrap();
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

async fn call(
    method: reqwest::Method,
    url: String,
    key: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::new().request(method, url).bearer_auth(key);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap_or(Value::Null))
}

async fn take(sb: &str, key: &str, reason: &str) -> (u16, Value) {
    call(
        reqwest::Method::POST,
        format!("{sb}/v1/hosts/gpu-box/holds"),
        key,
        Some(json!({"ttl_secs": 3600, "reason": reason})),
    )
    .await
}

// Another tenant's hold answers 404 to a DELETE, a PATCH and a read, and
// stays in force; the owner ends it.
#[tokio::test]
async fn another_tenants_hold_is_not_found() {
    let sb = serve().await;
    let (status, hold) = take(&sb, CLIENT_A, "benchmark").await;
    assert_eq!(status, 201, "{hold}");
    assert_eq!(hold["host"], "gpu-box");
    assert_eq!(hold["state"], "active");
    assert_eq!(hold["owner"], "team-a");
    assert!(
        hold["expires_at"].as_str().unwrap().ends_with('Z'),
        "{hold}"
    );
    let id = hold["id"].as_str().unwrap();
    let url = format!("{sb}/v1/hosts/gpu-box/holds/{id}");

    let (status, _) = call(reqwest::Method::DELETE, url.clone(), CLIENT_B, None).await;
    assert_eq!(status, 404);
    let (status, _) = call(
        reqwest::Method::PATCH,
        url.clone(),
        CLIENT_B,
        Some(json!({"ttl_secs": 10})),
    )
    .await;
    assert_eq!(status, 404);
    let (_, listed) = call(
        reqwest::Method::GET,
        format!("{sb}/v1/hosts/gpu-box/holds"),
        CLIENT_B,
        None,
    )
    .await;
    assert_eq!(listed["data"], json!([]), "team-b sees none of team-a's");
    let (_, listed) = call(
        reqwest::Method::GET,
        format!("{sb}/v1/hosts/gpu-box/holds"),
        OPERATOR,
        None,
    )
    .await;
    assert_eq!(
        listed["data"].as_array().unwrap().len(),
        1,
        "operators see all"
    );
    assert_eq!(listed["durable"], false, "no state directory in this test");

    // The owner extends it, then ends it.
    let (status, longer) = call(
        reqwest::Method::PATCH,
        url.clone(),
        CLIENT_A,
        Some(json!({"ttl_secs": 7200})),
    )
    .await;
    assert_eq!(status, 200, "{longer}");
    assert!(longer["expires_at_ms"].as_u64() > hold["expires_at_ms"].as_u64());
    let (status, _) = call(
        reqwest::Method::PATCH,
        url.clone(),
        CLIENT_A,
        Some(json!({"ttl_secs": 7201})),
    )
    .await;
    assert_eq!(status, 400, "longer than server.max_hold_secs");
    let (status, _) = call(reqwest::Method::DELETE, url.clone(), CLIENT_A, None).await;
    assert_eq!(status, 200);
    let (status, _) = call(reqwest::Method::DELETE, url, CLIENT_A, None).await;
    assert_eq!(status, 404, "ended");

    let (status, _) = call(
        reqwest::Method::POST,
        format!("{sb}/v1/hosts/no-such-box/holds"),
        CLIENT_A,
        Some(json!({"ttl_secs": 60})),
    )
    .await;
    assert_eq!(status, 404, "an unknown machine");
}

// A tenant has at most eight active holds; the ninth is 429, and another
// tenant is not counted against it.
#[tokio::test]
async fn a_tenant_has_at_most_eight_active_holds() {
    let sb = serve().await;
    for n in 0..8 {
        let (status, body) = take(&sb, CLIENT_A, &format!("job {n}")).await;
        assert_eq!(status, 201, "{body}");
    }
    let (status, body) = take(&sb, CLIENT_A, "one too many").await;
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["error"]["type"], "rate_limit_error");
    let (status, _) = take(&sb, CLIENT_B, "its own").await;
    assert_eq!(status, 201);
}

// A client key reads the capacity report (it got 403 before) as a
// read-only view: its own holds only, no commands, no details, no other
// tenant's names. An operator key keeps the full report.
#[tokio::test]
async fn a_client_key_gets_a_read_only_capacity_view() {
    let sb = serve().await;
    take(&sb, CLIENT_A, "mine").await;
    take(&sb, CLIENT_B, "theirs").await;

    let (status, view) = call(
        reqwest::Method::GET,
        format!("{sb}/v1/workloads/capacity"),
        CLIENT_A,
        None,
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["view"], "client");
    let lane = &view["data"][0];
    assert_eq!(lane["name"], "text");
    assert_eq!(lane["host"], "gpu-box");
    assert_eq!(lane["state"], "offline");
    assert!(lane.get("poweroff_in_secs").is_some());
    let holds = lane["holds"].as_array().unwrap();
    assert_eq!(holds.len(), 1, "{view}");
    assert_eq!(holds[0]["reason"], "mine");
    let text = view.to_string();
    for hidden in [
        "wake-marker-command",
        "poweroff-marker-command",
        "team-b",
        "theirs",
        "detail",
        "artifacts",
        "last_error",
    ] {
        assert!(
            !text.contains(hidden),
            "{hidden} in the client view: {text}"
        );
    }

    let (status, full) = call(
        reqwest::Method::GET,
        format!("{sb}/v1/workloads/capacity"),
        OPERATOR,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(full.get("view").is_none());
    assert_eq!(full["data"][0]["holds"].as_array().unwrap().len(), 2);
    assert!(full["data"][0].get("detail").is_some());
    assert_eq!(full["holds"]["durable"], false);
}
