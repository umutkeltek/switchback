use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use sb_credentials::provider_accounts::{ProviderAccountAuthority, ReconcileRequest, SourcePaths};
use tower::ServiceExt;

const CFG: &str = r#"server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
routes:
  - name: default
    match: { model: "*" }
    targets: ["mock/echo"]
"#;

fn app() -> axum::Router {
    let cfg = sb_core::Config::from_yaml(CFG).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    sb_server::build_app(sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    ))
}

#[tokio::test]
async fn accounts_is_loopback_only_and_metadata_only() {
    let root =
        std::env::temp_dir().join(format!("switchback-admin-accounts-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("SWITCHBACK_STATE_DIR", &root);
    ProviderAccountAuthority::open(root.join("provider-accounts.sqlite"))
        .unwrap()
        .reconcile(ReconcileRequest::apply(SourcePaths::default()))
        .unwrap();
    let ok = app()
        .oneshot(
            Request::builder()
                .uri("/admin/accounts")
                .extension(ConnectInfo(
                    "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(ok.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["schema"], "switchback/admin-accounts@1");
    assert_eq!(body["metadata_only"], true);
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    for forbidden in [
        "access_token",
        "refresh_token",
        "id_token",
        "authorization",
        "cookie",
    ] {
        assert!(!text.contains(forbidden), "leaked key {forbidden}")
    }
    let denied = app()
        .oneshot(
            Request::builder()
                .uri("/admin/accounts")
                .extension(ConnectInfo(
                    "192.0.2.1:40000".parse::<SocketAddr>().unwrap(),
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    std::env::remove_var("SWITCHBACK_STATE_DIR");
    std::fs::remove_dir_all(root).unwrap();
}
