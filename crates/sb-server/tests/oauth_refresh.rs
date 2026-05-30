//! End-to-end OAuth refresh through the live request path.
//!
//! Proves the whole seam wired together: a `refresh`-only oauth account (no
//! static access token) causes switchback to hit a token endpoint, mint an
//! access token, and present it as `Authorization: Bearer <token>` to the
//! upstream — all transparently inside one `/v1/chat/completions` call.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

/// Fake OAuth2 token endpoint. Each `refresh_token` grant mints a new token and
/// counts the calls (so the test can assert exactly one refresh fired).
#[derive(Clone, Default)]
struct TokenServer {
    calls: Arc<AtomicUsize>,
}

async fn token_handler(State(srv): State<TokenServer>) -> Json<Value> {
    let n = srv.calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "access_token": format!("minted-access-{n}"),
        "token_type": "bearer",
        "expires_in": 3600,
    }))
}

/// Fake upstream that records the Authorization header it was called with.
#[derive(Clone, Default)]
struct Upstream {
    seen_auth: Arc<Mutex<Vec<String>>>,
}

async fn upstream_handler(
    State(up): State<Upstream>,
    headers: HeaderMap,
    Json(_body): Json<Value>,
) -> Json<Value> {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_string();
    up.seen_auth.lock().unwrap().push(auth);
    Json(json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": "ok" }
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
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
    let state = sb_server::AppState {
        config: Arc::new(cfg),
        registry: Arc::new(registry),
        resolver: Arc::new(resolver),
        ledger: Arc::new(sb_ledger::UsageLedger::in_memory()),
    };
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn refresh_only_oauth_account_mints_token_and_presents_it_upstream() {
    let token_srv = TokenServer::default();
    let upstream = Upstream::default();

    let token_url = spawn(
        Router::new()
            .route("/token", post(token_handler))
            .with_state(token_srv.clone()),
    )
    .await;
    let upstream_url = spawn(
        Router::new()
            .route("/chat/completions", post(upstream_handler))
            .with_state(upstream.clone()),
    )
    .await;

    // An oauth account with ONLY a refresh token + token_url — no static access
    // token. The gateway must mint one before talking to the upstream.
    let cfg = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: pool
    type: openai_compatible
    base_url: "{upstream_url}"
    accounts:
      - id: oauth-acct
        auth:
          kind: oauth
          refresh: "seed-refresh-token"
          token_url: "{token_url}/token"
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "pool/some-model"
"#
    );
    let switchback = spawn_switchback(&cfg).await;
    let client = reqwest::Client::new();

    // Two requests: the first mints a token, the second reuses the cached one.
    for _ in 0..2 {
        let resp: Value = client
            .post(format!("{switchback}/v1/chat/completions"))
            .json(&json!({"model":"pool/some-model","messages":[{"role":"user","content":"hi"}]}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["choices"][0]["message"]["content"], "ok");
    }

    // The upstream must have seen the minted bearer token on every call.
    let seen = upstream.seen_auth.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "upstream should have been called twice");
    for auth in &seen {
        assert_eq!(
            auth, "Bearer minted-access-0",
            "upstream must receive the live-minted oauth token"
        );
    }

    // Exactly one refresh fired despite two requests (token cached within expiry).
    assert_eq!(
        token_srv.calls.load(Ordering::SeqCst),
        1,
        "a valid cached token must not be re-refreshed on every request"
    );
}
