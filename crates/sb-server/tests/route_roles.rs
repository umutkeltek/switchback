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
  - key: "sk-operator"
    tenant: ops
    role: operator
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
    let duplicate = reqwest::Client::new()
        .post(format!("{}/v1/setup/apply", gw.url))
        .bearer_auth("sk-operator")
        .header("content-type", "application/yaml")
        .body("schema: switchback/setup@1\nmodels:\n  one/m: {}\n  one/m: {}\n")
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), 400);
    assert_eq!(gw.state.engine.model_settings().revision("one/m"), 3);

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

struct McpChild {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::BufReader<tokio::process::ChildStdout>,
    next_id: u64,
}

impl McpChild {
    fn start(gateway: &Gateway) -> Self {
        use std::process::Stdio;

        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_switchback"))
            .args(["mcp", "--gateway", &gateway.url])
            .env("SWITCHBACK_AI_KEY", "sk-operator")
            .env("RUST_LOG", "trace")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = tokio::io::BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            next_id: 1,
        }
    }

    async fn write(&mut self, message: Value) {
        use tokio::io::AsyncWriteExt;

        let mut line = message.to_string();
        line.push('\n');
        self.input.write_all(line.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        use tokio::io::AsyncBufReadExt;

        let id = self.next_id;
        self.next_id += 1;
        self.write(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.output.read_line(&mut line),
        )
        .await
        .expect("MCP request timed out")
        .unwrap();
        assert!(!line.is_empty());
        assert!(
            !line.contains("sk-operator"),
            "MCP stdout contained its bearer key"
        );
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id);
        assert!(response["result"].is_object(), "{response}");
        response["result"].clone()
    }

    async fn call(&mut self, tool: &str, arguments: Value) -> Value {
        let result = self
            .request("tools/call", json!({"name": tool, "arguments": arguments}))
            .await;
        let data = &result["structuredContent"];
        assert!(data.is_object(), "{result}");
        assert_eq!(
            result["isError"] == true,
            data["error"].is_object(),
            "{result}"
        );
        data.clone()
    }

    async fn undo(&mut self, result: &Value) -> Value {
        self.call(
            result["undo"]["tool"].as_str().unwrap(),
            result["undo"]["arguments"].clone(),
        )
        .await
    }

    async fn finish(self) {
        drop(self.input);
        let output = self.child.wait_with_output().await.unwrap();
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sk-operator"));
    }
}

#[tokio::test]
async fn gateway_agent_door_over_real_stdio() {
    let (one, seen) = spawn_engine("one").await;
    let (two, _) = spawn_engine("two").await;
    let (config_path, db) = temp_paths("mcp");
    std::fs::write(&config_path, config(&one, &two, "one/m")).unwrap();
    let gw = spawn_gateway(&config_path, &db).await;

    let mut mcp = McpChild::start(&gw);
    let hello = mcp
        .request(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "gateway-agent-door-test", "version": "1"}}),
        )
        .await;
    assert_eq!(hello["protocolVersion"], "2025-06-18");
    mcp.write(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .await;
    let listed = mcp.request("tools/list", json!({})).await;
    let tools = listed["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 16);
    for tool in tools {
        assert!(tool["description"]
            .as_str()
            .is_some_and(|text| !text.is_empty()));
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
    let apply_definition = tools
        .iter()
        .find(|tool| tool["name"] == "switchback_setup_apply")
        .unwrap();
    assert_eq!(
        apply_definition["inputSchema"]["required"],
        json!(["document", "confirm"])
    );
    assert_eq!(
        apply_definition["inputSchema"]["properties"]["confirm"]["const"],
        true
    );
    let original = mcp.call("switchback_setup_export", json!({})).await;
    assert_eq!(
        original,
        json!({"schema": "switchback/setup@1", "names": {}, "models": {}})
    );
    let history_args = json!({"target": "one/m"});
    let history = mcp
        .call("switchback_settings_history", history_args.clone())
        .await;
    assert_eq!(history["history"], json!([]));

    let mut document = original.clone();
    document["models"]["one/m"] = json!({"defaults": {"temperature": 0.4}});
    document["names"]["fast"] = json!({"targets": ["two/m"]});
    document["if_match"] = json!({"models": {"one/m": 0}, "names": {"fast": 0}});
    let plan = mcp
        .call("switchback_setup_plan", json!({"document": document}))
        .await;
    assert_eq!(plan["applied"], false);
    assert_eq!(plan["items"][0]["status"], "create");
    assert_eq!(
        mcp.call("switchback_settings_history", history_args.clone())
            .await,
        history
    );

    for args in [
        json!({"document": document}),
        json!({"document": document, "confirm": false}),
    ] {
        let refused = mcp.call("switchback_setup_apply", args).await;
        assert!(refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("confirm: true"));
    }
    mcp.write(json!({"jsonrpc": "2.0", "method": "tools/call", "params": {
        "name": "switchback_setup_apply", "arguments": {"document": document, "confirm": true}
    }}))
    .await;
    assert_eq!(
        mcp.call("switchback_settings_history", history_args.clone())
            .await,
        history
    );
    let dangerous_wrapper = mcp
        .call(
            "switchback_setup_apply",
            json!({
                "document": {"setup": {"schema": "switchback/setup@1", "apply_now": true}},
                "confirm": true
            }),
        )
        .await;
    assert!(dangerous_wrapper["error"].is_object());

    let args = json!({"document": document, "confirm": true});
    let applied = mcp.call("switchback_setup_apply", args.clone()).await;
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["items"][0]["revision_after"], 1);
    assert_eq!(applied["items"][1]["revision_after"], 1);
    let history = mcp
        .call("switchback_settings_history", history_args.clone())
        .await;
    assert_eq!(history["history"].as_array().unwrap().len(), 1);
    let retried = mcp.call("switchback_setup_apply", args).await;
    assert_eq!(retried["items"][0]["status"], "unchanged");
    assert_eq!(retried["items"][1]["status"], "unchanged");
    assert_eq!(retried["undo"], Value::Null);
    assert_eq!(
        mcp.call("switchback_settings_history", history_args.clone())
            .await,
        history
    );

    let selected = mcp
        .call("switchback_names_get", json!({"name": "fast"}))
        .await;
    assert_eq!(selected["targets"], json!(["two/m"]));
    let restored = mcp.undo(&applied).await;
    assert_eq!(restored["items"][0]["revision_after"], 2);
    assert_eq!(restored["items"][1]["revision_after"], 2);
    let saved = mcp.call("switchback_setup_export", json!({})).await;
    assert_eq!(saved["models"]["one/m"], json!({}));
    let selected = mcp
        .call("switchback_names_get", json!({"name": "fast"}))
        .await;
    assert_eq!(selected["targets"], json!(["one/m"]));
    assert_eq!(selected["source"], "config");

    let change = mcp
        .call(
            "switchback_settings_set",
            json!({
                "target": "one/m", "revision": 2, "patch": {"defaults": {"temperature": 0.4}}
            }),
        )
        .await;
    assert_eq!(change["document"]["defaults"]["temperature"], 0.4);
    assert_eq!(change["revision"], 3);
    let switched = mcp
        .call(
            "switchback_settings_revert",
            json!({
                "target": "one/m", "revision": 3, "to": 2
            }),
        )
        .await;
    assert_eq!(switched["document"], json!({}));
    assert_eq!(switched["revision"], 4);
    assert_eq!(mcp.undo(&switched).await["items"][0]["revision_after"], 5);
    let settings = mcp.call("switchback_settings_get", history_args).await;
    assert_eq!(
        settings["targets"][0]["effective"]["temperature"]["value"],
        0.4
    );

    let switched = mcp
        .call(
            "switchback_names_set",
            json!({
                "name": "fast", "revision": 2, "targets": ["two/m"]
            }),
        )
        .await;
    assert_eq!(switched["revision"], 3);
    assert_eq!(switched["targets"], json!(["two/m"]));
    let reverted = mcp.undo(&switched).await;
    assert_eq!(reverted["targets"], json!(["one/m"]));
    assert_eq!(reverted["undo"]["arguments"]["to"], 3);

    let failure = mcp
        .call("switchback_settings_get", json!({"target": "sk-operator"}))
        .await;
    assert_eq!(failure["status"], 404);
    assert!(failure["error"]["message"]
        .as_str()
        .unwrap()
        .contains("[redacted]"));
    assert!(failure["error"]["next"]
        .as_str()
        .unwrap()
        .contains("/v1/models"));
    for tool in [
        "switchback_setup_schema",
        "switchback_names_list",
        "switchback_models_list",
        "switchback_engines_list",
        "switchback_status",
    ] {
        let result = mcp.call(tool, json!({})).await;
        assert!(result.get("error").is_none(), "{tool}: {result}");
    }
    let preview = mcp
        .call("switchback_route_preview", json!({"model": "fast"}))
        .await;
    assert!(preview.get("decision").is_some(), "{preview}");
    assert_eq!(*seen.lock().unwrap(), 0);
    mcp.finish().await;
    println!("stdio: 16 tools; plan and unconfirmed apply kept history at 0; confirmed apply and retry kept it at 1; undo restored settings and configured routing at revision 2; bearer redacted from stdout/stderr.");
}

#[tokio::test]
async fn gateway_agent_door_http_error_hints_and_schema() {
    use reqwest::Method;

    let (one, _) = spawn_engine("one").await;
    let (two, _) = spawn_engine("two").await;
    let (config_path, db) = temp_paths("hints");
    std::fs::write(&config_path, config(&one, &two, "one/m")).unwrap();
    let gw = spawn_gateway(&config_path, &db).await;
    let (status, _) = send(
        &gw,
        Method::PUT,
        "/v1/models/one/m/settings",
        "sk-operator",
        Some(0),
        Some(json!({"defaults": {"temperature": 0.4}})),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = send(
        &gw,
        Method::POST,
        "/v1/roles/fast/lock",
        "sk-operator",
        Some(0),
        Some(json!({"reason": "test lock"})),
    )
    .await;
    assert_eq!(status, 200);

    let cases = [
        (
            Method::PATCH,
            "/v1/models/one/m/settings",
            "sk-operator",
            Some(0),
            Some(json!({"defaults": {"temperature": 0.5}})),
            409,
            "revision_conflict",
            "current document",
        ),
        (
            Method::PUT,
            "/v1/roles/fast",
            "sk-operator",
            Some(1),
            Some(json!({"targets": ["two/m"]})),
            423,
            "locked",
            "/unlock",
        ),
        (
            Method::GET,
            "/v1/models/absent/m/settings",
            "sk-operator",
            None,
            None,
            404,
            "model_not_found",
            "/v1/engines",
        ),
        (
            Method::PUT,
            "/v1/roles/spare",
            "sk-operator",
            Some(0),
            Some(json!({"targets": ["absent/m"]})),
            400,
            "unknown_target",
            "/v1/models",
        ),
        (
            Method::POST,
            "/v1/setup/plan",
            "sk-operator",
            None,
            Some(json!({"schema": "switchback/setup@1", "mystery": true})),
            400,
            "invalid_setup",
            "/v1/setup/schema",
        ),
        (
            Method::POST,
            "/v1/setup/apply",
            "sk-operator",
            None,
            Some(json!({"schema": "switchback/setup@9"})),
            400,
            "invalid_setup",
            "/v1/setup/schema",
        ),
        (
            Method::POST,
            "/v1/setup/plan",
            "sk-operator",
            None,
            Some(json!({"setup": {"schema": "switchback/setup@1", "models": []}})),
            400,
            "invalid_setup",
            "/v1/setup/schema",
        ),
        (
            Method::POST,
            "/v1/setup/plan",
            "sk-client",
            None,
            Some(json!({})),
            403,
            "permission_error",
            "operator or admin",
        ),
        (
            Method::GET,
            "/v1/setup/schema",
            "wrong",
            None,
            None,
            401,
            "invalid_request_error",
            "Authorization",
        ),
        (
            Method::PATCH,
            "/v1/models/one/m/settings",
            "sk-operator",
            Some(1),
            None,
            415,
            "invalid_request_error",
            "Content-Type",
        ),
        (
            Method::GET,
            "/v1/roles/fast?limit=oops",
            "sk-operator",
            None,
            None,
            400,
            "invalid_request_error",
            "/v1/roles",
        ),
        (
            Method::PUT,
            "/v1/setup",
            "sk-admin",
            None,
            None,
            405,
            "invalid_request_error",
            "Allow",
        ),
    ];
    for (method, path, key, revision, body, expected, kind, hint) in cases {
        let (status, payload) = send(&gw, method, path, key, revision, body).await;
        assert_eq!(status, expected, "{path}: {payload}");
        let error = &payload["error"];
        assert_eq!(error["type"], kind, "{payload}");
        assert!(error["next"].as_str().unwrap().contains(hint), "{payload}");
        let message = error["message"].as_str().unwrap();
        assert!(
            !message.is_empty() && !message.contains("untagged enum"),
            "{payload}"
        );
        if status == 409 {
            assert_eq!(error["current_revision"], 1);
            assert_eq!(error["reason"], "revision_conflict");
        }
        if path == "/v1/setup/plan" && status == 400 {
            assert!(
                message.contains("mystery")
                    || message.contains("models")
                    || message.contains("map"),
                "{message}"
            );
        }
    }

    let (status, schema) = send(
        &gw,
        Method::GET,
        "/v1/setup/schema",
        "sk-operator",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(schema["title"], "switchback/setup@1");
    let schema = jsonschema::validator_for(&schema).unwrap();
    let example: Value =
        serde_yaml::from_str(include_str!("../../../config/setup.example.yaml")).unwrap();
    assert!(schema.is_valid(&example));
    for path in ["/v1/setup", "/v1/setup?effective=true"] {
        let (status, exported) = send(&gw, Method::GET, path, "sk-operator", None, None).await;
        assert_eq!(status, 200);
        assert!(
            schema.is_valid(&exported),
            "{exported}: {:?}",
            schema.iter_errors(&exported).collect::<Vec<_>>()
        );
    }
    for invalid in [
        json!({"models": {}}),
        json!({"schema": "switchback/setup@9"}),
        json!({"schema": "switchback/setup@1", "models": {"one/m": {"unknown": true}}}),
        json!({"schema": "switchback/setup@1", "models": {"one/m": {"defaults": []}}}),
        json!({"schema": "switchback/setup@1", "names": {"fast": {"targets": "one/m"}}}),
        json!({"schema": "switchback/setup@1", "names": {"fast": {"typo": true}}}),
    ] {
        assert!(!schema.is_valid(&invalid), "{invalid}");
    }
    println!("HTTP: 12 error cases preserved type/message and added next; schema accepted the example and both exports, and rejected six invalid documents.");
}
