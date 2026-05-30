//! End-to-end egress layer: two accounts of one provider exit from different
//! outbound paths — account A through a proxy, account B direct — and each
//! request's trace records the egress it actually used. Proves the "call these
//! APIs like they're from different places" requirement, observably.
//!
//! The "proxy" here is an axum server: reqwest, proxying a plain-HTTP upstream,
//! sends the request to the proxy address (absolute-form target), so the proxy
//! node answers directly and tags its response — which lets the test see which
//! path served each request without a full forwarding proxy.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

#[derive(Clone)]
struct Node {
    tag: &'static str,
    hits: Arc<AtomicUsize>,
}

async fn chat(State(node): State<Node>, Json(_body): Json<Value>) -> Json<Value> {
    node.hits.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "id": "x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": format!("via={}", node.tag) }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    }))
}

/// Spawn an OpenAI-shaped node that tags its response and counts hits.
async fn spawn_node(tag: &'static str) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .with_state(Node { tag, hits: hits.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), hits)
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

async fn post_chat(base: &str, client: &reqwest::Client) -> String {
    let resp: Value = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&json!({"model":"pool/some-model","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn two_accounts_exit_from_different_paths_and_traces_record_it() {
    let (upstream, upstream_hits) = spawn_node("direct").await;
    let (proxy, proxy_hits) = spawn_node("proxy").await;

    // acct-a routes through `viaproxy`; acct-b goes direct. Round-robin so both
    // get used across two requests.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
egress:
  - id: viaproxy
    kind: proxy
    url: "{proxy}"
providers:
  - id: pool
    type: openai_compatible
    base_url: "{upstream}"
    selection: round_robin
    sticky: 1
    accounts:
      - id: acct-a
        auth: {{ kind: api_key, inline: "k" }}
        priority: 0
        egress: viaproxy
      - id: acct-b
        auth: {{ kind: api_key, inline: "k" }}
        priority: 1
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "pool/some-model"
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();

    // Two requests → round-robin uses each account once.
    let a = post_chat(&switchback, &client).await;
    let b = post_chat(&switchback, &client).await;
    let mut served: Vec<String> = vec![a, b];
    served.sort();
    assert_eq!(
        served,
        vec!["via=direct".to_string(), "via=proxy".to_string()],
        "one request went direct, one through the proxy"
    );
    assert_eq!(proxy_hits.load(Ordering::SeqCst), 1, "proxy used exactly once");
    assert_eq!(upstream_hits.load(Ordering::SeqCst), 1, "direct used exactly once");

    // The traces record which egress each account used.
    let traces: Value = client
        .get(format!("{switchback}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(traces["count"], 2);
    let mut pairs: Vec<(String, String)> = traces["traces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            let att = &t["attempts"][0];
            (
                att["account_id"].as_str().unwrap().to_string(),
                att["egress"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("acct-a".to_string(), "viaproxy".to_string()),
            ("acct-b".to_string(), "direct".to_string()),
        ],
        "trace shows acct-a via the proxy egress, acct-b direct"
    );
}

#[derive(Clone, Default)]
struct Captured {
    user_agent: Arc<Mutex<Option<String>>>,
    app_id: Arc<Mutex<Option<String>>>,
}

async fn capture_chat(State(cap): State<Captured>, headers: HeaderMap, Json(_b): Json<Value>) -> Json<Value> {
    let get = |h: &str| headers.get(h).and_then(|v| v.to_str().ok()).map(String::from);
    *cap.user_agent.lock().unwrap() = get("user-agent");
    *cap.app_id.lock().unwrap() = get("x-app-id");
    Json(json!({
        "id": "x", "object": "chat.completion",
        "choices": [{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"ok"}}],
        "usage": {"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }))
}

#[tokio::test]
async fn egress_applies_custom_user_agent_and_headers() {
    // Upstream that records the User-Agent + x-app-id it was called with.
    let cap = Captured::default();
    let app = Router::new()
        .route("/chat/completions", post(capture_chat))
        .with_state(cap.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // A `direct` egress that carries a client identity (custom UA + header).
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
egress:
  - id: branded
    kind: direct
    user_agent: "SwitchbackUA/9.9"
    headers:
      x-app-id: "app-123"
providers:
  - id: pool
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
        egress: branded
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "pool/some-model"
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();
    let served = post_chat(&switchback, &client).await;
    assert_eq!(served, "ok");

    assert_eq!(
        cap.user_agent.lock().unwrap().as_deref(),
        Some("SwitchbackUA/9.9"),
        "upstream must see the configured User-Agent"
    );
    assert_eq!(
        cap.app_id.lock().unwrap().as_deref(),
        Some("app-123"),
        "upstream must see the configured custom header"
    );
}

#[tokio::test]
async fn dead_proxy_account_falls_over_to_a_direct_account() {
    let (upstream, upstream_hits) = spawn_node("direct").await;

    // acct-dead routes through a proxy that isn't listening (127.0.0.1:1 →
    // refused); acct-good goes direct. fill_first tries acct-dead first; the
    // connection error must fall over to acct-good, not fail the request.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
egress:
  - id: deadproxy
    kind: proxy
    url: "http://127.0.0.1:1"
providers:
  - id: pool
    type: openai_compatible
    base_url: "{upstream}"
    selection: fill_first
    accounts:
      - id: acct-dead
        auth: {{ kind: api_key, inline: "k" }}
        priority: 0
        egress: deadproxy
      - id: acct-good
        auth: {{ kind: api_key, inline: "k" }}
        priority: 1
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "pool/some-model"
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();

    let served = post_chat(&switchback, &client).await;
    assert_eq!(served, "via=direct", "fell over from the dead proxy to direct");
    assert_eq!(upstream_hits.load(Ordering::SeqCst), 1);

    // The trace shows the failed proxy attempt then the successful direct one.
    let traces: Value = client
        .get(format!("{switchback}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(traces["count"], 1);
    let attempts = traces["traces"][0]["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "one failed proxy attempt, one direct success");
    assert_eq!(attempts[0]["account_id"], "acct-dead");
    assert_eq!(attempts[0]["egress"], "deadproxy");
    assert_eq!(attempts[0]["outcome"], "failed");
    assert_eq!(attempts[0]["fell_over"], true);
    assert_eq!(attempts[1]["account_id"], "acct-good");
    assert_eq!(attempts[1]["egress"], "direct");
    assert_eq!(attempts[1]["outcome"], "success");
}

#[tokio::test]
async fn disabled_egress_falls_back_to_direct_and_trace_says_so() {
    let (upstream, upstream_hits) = spawn_node("direct").await;
    let (proxy, proxy_hits) = spawn_node("proxy").await;

    // acct-a names `viaproxy`, but the egress is toggled OFF → it must fall back
    // to direct (no need to edit the account), and the trace must say "direct".
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
egress:
  - id: viaproxy
    kind: proxy
    url: "{proxy}"
    enabled: false
providers:
  - id: pool
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: acct-a
        auth: {{ kind: api_key, inline: "k" }}
        egress: viaproxy
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "pool/some-model"
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();

    let served = post_chat(&switchback, &client).await;
    assert_eq!(served, "via=direct", "disabled egress fell back to direct");
    assert_eq!(proxy_hits.load(Ordering::SeqCst), 0, "disabled proxy not used");
    assert_eq!(upstream_hits.load(Ordering::SeqCst), 1);

    let traces: Value = client
        .get(format!("{switchback}/v1/traces"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        traces["traces"][0]["attempts"][0]["egress"], "direct",
        "trace records the effective (fallen-back) egress"
    );
}
