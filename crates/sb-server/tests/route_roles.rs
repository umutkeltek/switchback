//! Route roles end to end over HTTP (`/v1/roles`): a role set in the store
//! answers its name on the next request, a reload of a regenerated config
//! and a restart keep it, revert 0 gives the config's route back, locks
//! refuse writes with their reason, and every target is checked before
//! anything is written.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

type Seen = Arc<Mutex<usize>>;

/// An engine that answers every chat with its own name.
async fn answer(State((name, seen)): State<(String, Seen)>, _body: Bytes) -> Response {
    *seen.lock().unwrap() += 1;
    Json(json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": format!("served={name}")}
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

async fn spawn_engine(name: &str) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/chat/completions", post(answer))
        .route("/health", get(|| async { "ok" }))
        .with_state((name.to_string(), seen.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn config(one: &str, two: &str, fast_target: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: one
    type: openai_compatible
    base_url: "{one}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: two
    type: openai_compatible
    base_url: "{two}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
combos:
  mix:
    models: ["one/m"]
routes:
  - name: fast
    match: {{ model: fast }}
    pin_order: true
    targets: ["{fast_target}"]
  - name: spare
    match: {{ model: spare }}
    targets: ["two/m"]
api_keys:
  - key: "sk-admin"
    tenant: ops
    role: admin
  - key: "sk-client"
    tenant: ops
    role: client
tenants:
  - id: ops
"#
    )
}

struct Gateway {
    url: String,
    state: sb_server::AppState,
}

async fn spawn_gateway(cfg_path: &std::path::Path, db: &std::path::Path) -> Gateway {
    let cfg = sb_core::Config::from_path(cfg_path).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    )
    .with_store(Arc::new(
        sb_store::SqliteStore::open(db.to_str().unwrap()).unwrap(),
    ));
    engine.set_config_path(cfg_path.to_path_buf());
    let state = sb_server::AppState::from_engine(engine);
    let app = sb_server::build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Gateway {
        url: format!("http://{addr}"),
        state,
    }
}

/// Who answered one chat for `model`, and the decision's reasons.
async fn answered_by(gw: &Gateway, model: &str) -> (String, Vec<String>) {
    let r = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", gw.url))
        .bearer_auth("sk-admin")
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let trace = gw.state.traces.recent(1).pop().expect("trace");
    (
        body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .to_string(),
        trace.decision.reason.clone(),
    )
}

async fn send(
    gw: &Gateway,
    method: reqwest::Method,
    path: &str,
    key: &str,
    if_match: Option<u64>,
    body: Option<Value>,
) -> (u16, Value) {
    let mut request = reqwest::Client::new()
        .request(method, format!("{}{path}", gw.url))
        .bearer_auth(key);
    if let Some(revision) = if_match {
        request = request.header("if-match", revision.to_string());
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let r = request.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

fn temp_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "sb-roles-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    (dir.join("gateway.yaml"), dir.join("state.sqlite"))
}

#[tokio::test]
async fn a_role_set_routes_now_and_survives_reload_and_restart_until_reverted() {
    let (one, _) = spawn_engine("one").await;
    let (two, _) = spawn_engine("two").await;
    let (cfg_path, db) = temp_paths("cycle");
    std::fs::write(&cfg_path, config(&one, &two, "one/m")).unwrap();
    let gw = spawn_gateway(&cfg_path, &db).await;

    assert_eq!(answered_by(&gw, "fast").await.0, "served=one");
    let (status, roles) = send(
        &gw,
        reqwest::Method::GET,
        "/v1/roles",
        "sk-admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    let fast = roles["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "fast")
        .unwrap()
        .clone();
    assert_eq!(fast["source"], "config");
    assert_eq!(fast["targets"], json!(["one/m"]));
    assert_eq!(fast["revision"], 0);

    // `sb ai role set fast two/m`.
    let (status, row) = send(
        &gw,
        reqwest::Method::PUT,
        "/v1/roles/fast",
        "sk-admin",
        Some(0),
        Some(json!({"targets": ["two/m"]})),
    )
    .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["source"], "store");
    assert_eq!(row["revision"], 1);
    let retry = reqwest::Client::new()
        .put(format!("{}/v1/roles/fast", gw.url))
        .bearer_auth("sk-admin")
        .header("if-match", "0")
        .json(&json!({"targets": ["two/m"], "check": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(retry.status(), 200);
    assert_eq!(retry.headers()["etag"], "\"1\"");
    let retried: Value = retry.json().await.unwrap();
    assert_eq!(retried["unchanged"], true, "{retried}");
    assert_eq!(retried["check"], Value::Null);

    let (served, reasons) = answered_by(&gw, "fast").await;
    assert_eq!(served, "served=two", "the next request, no restart");
    assert!(
        reasons
            .iter()
            .any(|r| r == "role `fast`: targets from the role store (revision 1)"),
        "{reasons:?}"
    );
    // `/v1/models` and the settings of the name show the role's target.
    let (_, models) = send(
        &gw,
        reqwest::Method::GET,
        "/v1/models",
        "sk-admin",
        None,
        None,
    )
    .await;
    assert!(models["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["id"] == "two/m"));
    let (_, settings) = send(
        &gw,
        reqwest::Method::GET,
        "/v1/models/fast/settings",
        "sk-admin",
        None,
        None,
    )
    .await;
    assert_eq!(settings["targets"][0]["target"], "two/m", "{settings}");

    // A regenerated config (the same route, another target) is reloaded.
    std::fs::write(&cfg_path, config(&one, &two, "one/other")).unwrap();
    let (status, reload) = send(
        &gw,
        reqwest::Method::POST,
        "/v1/reload",
        "sk-admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200, "{reload}");
    assert_eq!(
        answered_by(&gw, "fast").await.0,
        "served=two",
        "after reload"
    );

    // A restart: a new gateway on the same config file and state store.
    let gw = spawn_gateway(&cfg_path, &db).await;
    assert_eq!(
        answered_by(&gw, "fast").await.0,
        "served=two",
        "after restart"
    );

    // Revert to 0: the config's (regenerated) route answers again.
    let (status, row) = send(
        &gw,
        reqwest::Method::POST,
        "/v1/roles/fast/revert",
        "sk-admin",
        Some(1),
        Some(json!({"to": 0})),
    )
    .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["source"], "config");
    assert_eq!(row["revision"], 2);
    let (served, reasons) = answered_by(&gw, "fast").await;
    assert_eq!(served, "served=one");
    assert!(
        !reasons.iter().any(|r| r.starts_with("role ")),
        "{reasons:?}"
    );

    // The history says every step.
    let (_, detail) = send(
        &gw,
        reqwest::Method::GET,
        "/v1/roles/fast",
        "sk-admin",
        None,
        None,
    )
    .await;
    let actions: Vec<&str> = detail["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, vec!["revert:0", "set"]);
    let _ = std::fs::remove_dir_all(cfg_path.parent().unwrap());
}

#[tokio::test]
async fn writes_are_checked_before_anything_is_written() {
    let (one, _) = spawn_engine("one").await;
    let (two, _) = spawn_engine("two").await;
    let (cfg_path, db) = temp_paths("checks");
    std::fs::write(&cfg_path, config(&one, &two, "one/m")).unwrap();
    let gw = spawn_gateway(&cfg_path, &db).await;
    let put = |name: &'static str, revision: Option<u64>, body: Value| {
        let gw = &gw;
        async move {
            send(
                gw,
                reqwest::Method::PUT,
                &format!("/v1/roles/{name}"),
                "sk-admin",
                revision,
                Some(body),
            )
            .await
        }
    };

    // Unknown targets: no such provider, no such model.
    let (status, body) = put("fast", Some(0), json!({"targets": ["nope/m"]})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("no provider `nope`"));
    let (status, body) = put("fast", Some(0), json!({"targets": ["two/never-listed"]})).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["type"], "unknown_target");
    // Internal ids are not roles.
    for name in ["one", "mix", "auto"] {
        let (status, body) = put(name, Some(0), json!({"targets": ["two/m"]})).await;
        assert_eq!(status, 400, "{name}: {body}");
        assert_eq!(body["error"]["type"], "invalid_role", "{name}");
    }
    // A write names the revision it edited.
    let (status, _) = put("fast", None, json!({"targets": ["two/m"]})).await;
    assert_eq!(status, 428);
    let (status, _) = put("fast", Some(7), json!({"targets": ["two/m"]})).await;
    assert_eq!(status, 409);
    // Nothing was written by any of them.
    assert_eq!(gw.state.engine.route_roles().revision("fast"), 0);
    assert_eq!(answered_by(&gw, "fast").await.0, "served=one");

    // A client key reads names and targets, and never writes.
    let (status, _) = send(
        &gw,
        reqwest::Method::PUT,
        "/v1/roles/fast",
        "sk-client",
        Some(0),
        Some(json!({"targets": ["two/m"]})),
    )
    .await;
    assert_eq!(status, 403);
    let (status, list) = send(
        &gw,
        reqwest::Method::GET,
        "/v1/roles",
        "sk-client",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(list["data"][0].get("revision").is_none(), "{list}");

    // A lock refuses set and revert with its reason; unlock lets it.
    let (status, row) = send(
        &gw,
        reqwest::Method::POST,
        "/v1/roles/fast/lock",
        "sk-admin",
        None,
        Some(json!({"reason": "night job uses it"})),
    )
    .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["locked"], true);
    let (status, body) = put("fast", Some(1), json!({"targets": ["two/m"]})).await;
    assert_eq!(status, 423);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("night job uses it"));
    let (status, _) = send(
        &gw,
        reqwest::Method::POST,
        "/v1/roles/fast/unlock",
        "sk-admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    let (status, row) = put(
        "fast",
        Some(2),
        json!({"targets": ["two/m", "one/m"], "check": true}),
    )
    .await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(row["check"]["ok"], true, "{row}");
    assert_eq!(row["check"]["reply"], "served=two");
    assert_eq!(answered_by(&gw, "fast").await.0, "served=two");

    // A new name is a role of its own.
    let (status, row) = put("long", Some(0), json!({"targets": ["one/m"]})).await;
    assert_eq!(status, 200, "{row}");
    assert_eq!(answered_by(&gw, "long").await.0, "served=one");
    let _ = std::fs::remove_dir_all(cfg_path.parent().unwrap());
}

/// `--check` fails before anything is written when the first target fails.
#[tokio::test]
async fn a_failed_check_writes_nothing() {
    let (one, _) = spawn_engine("one").await;
    // `two` points at a port nothing listens on.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (cfg_path, db) = temp_paths("check");
    std::fs::write(&cfg_path, config(&one, &closed, "one/m")).unwrap();
    let gw = spawn_gateway(&cfg_path, &db).await;
    let (status, body) = send(
        &gw,
        reqwest::Method::PUT,
        "/v1/roles/fast",
        "sk-admin",
        Some(0),
        Some(json!({"targets": ["two/m"], "check": true})),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["type"], "check_failed");
    assert_eq!(gw.state.engine.route_roles().revision("fast"), 0);
    assert_eq!(answered_by(&gw, "fast").await.0, "served=one");
    let _ = std::fs::remove_dir_all(cfg_path.parent().unwrap());
}

#[tokio::test]
async fn setup_plans_and_partially_applies_with_lock_protection_and_cas_undo() {
    let (one, _) = spawn_engine("one").await;
    let (two, _) = spawn_engine("two").await;
    let (config_path, db) = temp_paths("setup");
    std::fs::write(&config_path, config(&one, &two, "one/m")).unwrap();
    let gw = spawn_gateway(&config_path, &db).await;
    let post = |path: &'static str, document: Value| {
        send(
            &gw,
            reqwest::Method::POST,
            path,
            "sk-admin",
            None,
            Some(document),
        )
    };
    let schema = "switchback/setup@1";
    let setup = json!({
        "schema": schema,
        "names": {"fast": {"targets": ["one/m"]}, "new": {"targets": ["two/m"]}},
        "models": {"one/m": {"defaults": {"temperature": 0.4}, "locked": ["temperature"],
            "lock_reasons": {"temperature": "keep reports stable"}}},
    });
    for (method, path) in [
        (reqwest::Method::GET, "/v1/setup"),
        (reqwest::Method::POST, "/v1/setup/plan"),
        (reqwest::Method::POST, "/v1/setup/apply"),
    ] {
        assert_eq!(
            send(&gw, method, path, "sk-client", None, Some(setup.clone()))
                .await
                .0,
            403
        );
    }
    let (status, planned) = post("/v1/setup/plan", setup.clone()).await;
    assert_eq!(status, 200, "{planned}");
    assert_eq!(planned["items"][0]["status"], "create", "{planned}");
    assert_eq!(planned["items"][1]["status"], "unchanged", "{planned}");
    assert_eq!(planned["not_in_file"][0]["name"], "spare");
    assert_eq!(gw.state.engine.model_settings().revision("one/m"), 0);
    assert_eq!(gw.state.engine.route_roles().revision("new"), 0);
    let (status, applied) = post("/v1/setup/apply", setup.clone()).await;
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["items"][0]["revision_after"], 1);
    assert_eq!(applied["items"][1]["revision_after"], 0);
    assert_eq!(applied["items"][2]["revision_after"], 1);
    let (_, retry) = post("/v1/setup/apply", setup).await;
    assert!(
        retry["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"] == "unchanged"),
        "{retry}"
    );

    let change = json!({
        "schema": schema,
        "names": {"new": {"targets": ["one/m"]}},
        "models": {"one/m": {"defaults": {"temperature": 0.4}}, "two/m": {"bogus": true}},
    });
    let (_, refused) = post("/v1/setup/apply", change.clone()).await;
    assert_eq!(refused["items"][0]["status"], "refused");
    assert!(refused["items"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("keep reports stable"));
    assert_eq!(refused["items"][1]["status"], "invalid");
    assert_eq!(refused["items"][2]["revision_after"], 2);
    assert_eq!(gw.state.engine.model_settings().revision("one/m"), 1);
    let mut authorized = change;
    authorized["unlock"] = json!(["one/m"]);
    let (_, unlocked) = post("/v1/setup/apply", authorized).await;
    assert_eq!(unlocked["items"][0]["revision_after"], 2);
    let undo = unlocked["items"][0]["undo"].clone();
    assert_eq!(undo["if_match"]["models"]["one/m"], 2);
    let (_, restored) = post("/v1/setup/apply", undo.clone()).await;
    assert_eq!(restored["items"][0]["revision_after"], 3, "{restored}");
    assert_eq!(
        gw.state
            .engine
            .model_settings()
            .get("one/m")
            .unwrap()
            .doc
            .lock_reason("temperature"),
        Some("keep reports stable")
    );

    let (status, exported) = send(
        &gw,
        reqwest::Method::GET,
        "/v1/setup?effective=true",
        "sk-admin",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(exported["names"]["fast"]["source"], "config");
    assert_eq!(exported["names"]["new"]["targets"], json!(["one/m"]));
    assert_eq!(
        exported["models"]["one/m"]["locked"],
        json!(["temperature"])
    );
    let (_, plan_export) = post("/v1/setup/plan", exported).await;
    assert!(
        plan_export["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["status"] == "unchanged"),
        "{plan_export}"
    );

    let svc = reqwest::Client::new()
        .post(format!("{}/v1/setup/plan", gw.url))
        .bearer_auth("sk-admin")
        .header("content-type", "application/yaml")
        .body("schema: switchback/setup@1\nnames:\n  fast:\n    targets: [one/m]\n")
        .send()
        .await
        .unwrap();
    assert_eq!(svc.status(), 200);
    let yaml_plan: Value = svc.json().await.unwrap();
    assert_eq!(yaml_plan["items"][0]["status"], "unchanged", "{yaml_plan}");

    let _ = std::fs::remove_dir_all(config_path.parent().unwrap());
}

#[tokio::test]
async fn setup_failed_model_check_restores_even_a_newly_locked_document() {
    let (one, _) = spawn_engine("one").await;
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (config_path, db) = temp_paths("setup-check");
    std::fs::write(&config_path, config(&one, &closed, "one/m")).unwrap();
    let gw = spawn_gateway(&config_path, &db).await;
    let (status, report) = send(
        &gw,
        reqwest::Method::POST,
        "/v1/setup/apply",
        "sk-admin",
        None,
        Some(json!({
            "schema": "switchback/setup@1", "check": true,
            "models": {"two/m": {"defaults": {"temperature": 0.4}, "locked": ["temperature"]}},
        })),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(report["items"][0]["status"], "refused", "{report}");
    assert_eq!(
        report["items"][0]["check"]["restored"]["revision"], 2,
        "{report}"
    );
    let entry = gw.state.engine.model_settings().get("two/m").unwrap();
    assert!(entry.doc.is_empty());
    let rows = gw
        .state
        .engine
        .model_settings()
        .history("two/m", 10)
        .unwrap();
    assert_eq!(rows[0].action, "setup:check_failed");
    let _ = std::fs::remove_dir_all(config_path.parent().unwrap());
}
