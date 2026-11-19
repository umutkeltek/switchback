//! Per-model settings end to end: the API (`/v1/models/{model}/settings`),
//! what reaches the upstream (precedence, profiles, locks, reasoning), the
//! revision rules, auth and tenant scoping, and persistence across a
//! restart.

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

type Seen = Arc<Mutex<Vec<Bytes>>>;

async fn record(State(seen): State<Seen>, body: Bytes) -> Response {
    let streamed = serde_json::from_slice::<Value>(&body)
        .is_ok_and(|request| request["stream"] == json!(true));
    seen.lock().unwrap().push(body);
    if streamed {
        let sse = "data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n\
                   data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n\
                   data: [DONE]\n\n";
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    Json(json!({
        "id": "chatcmpl-x",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": {"role": "assistant", "content": "ok"}
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

async fn record_embeddings(State(seen): State<Seen>, body: Bytes) -> Json<Value> {
    seen.lock().unwrap().push(body);
    Json(json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}],
        "model": "m",
        "usage": {"prompt_tokens": 1, "total_tokens": 1}
    }))
}

async fn spawn_upstream() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/chat/completions", post(record))
        .route("/embeddings", post(record_embeddings))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn spawn_gateway(cfg_yaml: &str, db: Option<&str>) -> (String, sb_server::AppState) {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let mut engine = sb_runtime::Engine::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    if let Some(db) = db {
        engine = engine.with_store(Arc::new(sb_store::SqliteStore::open(db).unwrap()));
    }
    let state = sb_server::AppState::from_engine(engine);
    let app = sb_server::build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

fn config(upstream: &str, extra: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: other
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
routes:
  - name: fast
    match: {{ model: fast }}
    targets: ["text/m"]
  - name: careful
    match: {{ model: careful }}
    targets: ["text/m"]
    settings_profile: precise
{extra}
"#
    )
}

fn last_body(seen: &Seen) -> Value {
    serde_json::from_slice(seen.lock().unwrap().last().unwrap()).unwrap()
}

async fn chat(
    base: &str,
    key: Option<&str>,
    body: Value,
    profile: Option<&str>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/v1/chat/completions"))
        .json(&body);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    if let Some(profile) = profile {
        request = request.header("x-switchback-profile", profile);
    }
    request.send().await.unwrap()
}

async fn put_settings(base: &str, model: &str, revision: u64, doc: Value) -> reqwest::Response {
    reqwest::Client::new()
        .put(format!("{base}/v1/models/{model}/settings"))
        .header("if-match", revision.to_string())
        .json(&doc)
        .send()
        .await
        .unwrap()
}

fn hi(model: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": "hi"}]})
}

/// What main (a1ade2d, before model settings) sent upstream for these
/// requests (plain, streamed and embeddings), byte for byte: a target without settings must still get
/// exactly this.
const GOLDEN: &[(&str, &str)] = &[
    (
        "llama_plain",
        r#"{"messages":[{"content":"s","role":"system"},{"content":"hi","role":"user"}],"model":"m","stream":false}"#,
    ),
    (
        "llama_sampled",
        r#"{"chat_template_kwargs":{"enable_thinking":true},"max_tokens":64,"messages":[{"content":"s","role":"system"},{"content":"hi","role":"user"}],"min_p":0.05,"model":"m","repeat_penalty":1.1,"seed":7,"stop":["x"],"stream":false,"temperature":0.699999988079071,"top_k":20,"top_p":0.9}"#,
    ),
    (
        "llama_both",
        r#"{"messages":[{"content":"hi","role":"user"}],"model":"m","repeat_penalty":1.1,"repetition_penalty":1.3,"stream":false}"#,
    ),
    (
        "vllm_plain",
        r#"{"messages":[{"content":"s","role":"system"},{"content":"hi","role":"user"}],"model":"m","stream":false}"#,
    ),
    (
        "vllm_sampled",
        r#"{"chat_template_kwargs":{"enable_thinking":true},"max_tokens":64,"messages":[{"content":"s","role":"system"},{"content":"hi","role":"user"}],"min_p":0.05,"model":"m","repeat_penalty":1.1,"seed":7,"stop":["x"],"stream":false,"temperature":0.699999988079071,"top_k":20,"top_p":0.9}"#,
    ),
    (
        "vllm_both",
        r#"{"messages":[{"content":"hi","role":"user"}],"model":"m","repeat_penalty":1.1,"repetition_penalty":1.3,"stream":false}"#,
    ),
    (
        "llama_stream_plain",
        r#"{"messages":[{"content":"s","role":"system"},{"content":"hi","role":"user"}],"model":"m","stream":true}"#,
    ),
    (
        "llama_stream_sampled",
        r#"{"chat_template_kwargs":{"enable_thinking":true},"max_tokens":64,"messages":[{"content":"s","role":"system"},{"content":"hi","role":"user"}],"min_p":0.05,"model":"m","repeat_penalty":1.1,"seed":7,"stop":["x"],"stream":true,"temperature":0.699999988079071,"top_k":20,"top_p":0.9}"#,
    ),
    (
        "vllm_stream_sampled",
        r#"{"chat_template_kwargs":{"enable_thinking":true},"max_tokens":64,"messages":[{"content":"s","role":"system"},{"content":"hi","role":"user"}],"min_p":0.05,"model":"m","repeat_penalty":1.1,"seed":7,"stop":["x"],"stream":true,"temperature":0.699999988079071,"top_k":20,"top_p":0.9}"#,
    ),
    (
        "llama_embeddings",
        r#"{"encoding_format":"float","input":"hello","model":"m"}"#,
    ),
    (
        "vllm_embeddings",
        r#"{"dimensions":8,"input":["a","b"],"model":"m"}"#,
    ),
];

fn golden(name: &str) -> &'static str {
    GOLDEN.iter().find(|(n, _)| *n == name).unwrap().1
}

fn golden_config(upstream: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: vtext
    type: openai_compatible
    engine: vllm
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
  - id: other
    type: openai_compatible
    base_url: "{upstream}"
    accounts:
      - id: a
        auth: {{ kind: api_key, inline: "k" }}
"#
    )
}

#[tokio::test]
async fn no_settings_means_byte_identical_upstream_request() {
    let (upstream, seen) = spawn_upstream().await;
    // Gateway A: no settings anywhere. Gateway B: settings on another
    // target only. Both must send main's bytes for targets without settings.
    let (a, _) = spawn_gateway(&golden_config(&upstream), None).await;
    let (b, _) = spawn_gateway(&golden_config(&upstream), None).await;
    assert_eq!(
        put_settings(
            &b,
            "other/x",
            0,
            json!({"defaults": {"temperature": 0.1, "repeat_penalty": 1.2}})
        )
        .await
        .status(),
        200
    );
    let plain = |model: &str| json!({"model": model, "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}]});
    let sampled = |model: &str| {
        json!({"model": model,
            "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "hi"}],
            "temperature": 0.7, "top_p": 0.9, "top_k": 20, "min_p": 0.05, "repeat_penalty": 1.1,
            "seed": 7, "max_tokens": 64, "stop": ["x"], "chat_template_kwargs": {"enable_thinking": true}})
    };
    let both = |model: &str| {
        json!({"model": model, "messages": [{"role": "user", "content": "hi"}],
            "repeat_penalty": 1.1, "repetition_penalty": 1.3})
    };
    let cases = [
        ("llama_plain", plain("text/m")),
        ("llama_sampled", sampled("text/m")),
        ("llama_both", both("text/m")),
        ("vllm_plain", plain("vtext/m")),
        ("vllm_sampled", sampled("vtext/m")),
        ("vllm_both", both("vtext/m")),
    ];
    let streamed = |mut body: Value| {
        body["stream"] = json!(true);
        body
    };
    let stream_cases = [
        ("llama_stream_plain", streamed(plain("text/m"))),
        ("llama_stream_sampled", streamed(sampled("text/m"))),
        ("vllm_stream_sampled", streamed(sampled("vtext/m"))),
    ];
    let embedding_cases = [
        (
            "llama_embeddings",
            json!({"model": "text/m", "input": "hello", "encoding_format": "float"}),
        ),
        (
            "vllm_embeddings",
            json!({"model": "vtext/m", "input": ["a", "b"], "dimensions": 8}),
        ),
    ];
    for gateway in [&a, &b] {
        for (name, body) in cases.iter().chain(&stream_cases) {
            let r = chat(gateway, None, body.clone(), None).await;
            assert_eq!(r.status(), 200);
            assert!(r.headers().get("x-switchback-settings").is_none());
            let text = r.text().await.unwrap();
            if body["stream"] == json!(true) {
                assert!(text.contains("[DONE]"), "{name}: {text}");
            }
            let sent = seen.lock().unwrap().last().unwrap().clone();
            assert_eq!(
                std::str::from_utf8(&sent).unwrap(),
                golden(name),
                "{name}: a target without settings changed its request"
            );
        }
        for (name, body) in &embedding_cases {
            let r = reqwest::Client::new()
                .post(format!("{gateway}/v1/embeddings"))
                .json(body)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200, "{name}");
            let sent = seen.lock().unwrap().last().unwrap().clone();
            assert_eq!(
                std::str::from_utf8(&sent).unwrap(),
                golden(name),
                "{name}: a target without settings changed its request"
            );
        }
    }
}

#[tokio::test]
async fn vllm_gets_repetition_penalty_only_when_settings_set_it_and_a_lock_wins() {
    let (upstream, seen) = spawn_upstream().await;
    let (gw, _) = spawn_gateway(&golden_config(&upstream), None).await;
    assert_eq!(
        put_settings(
            &gw,
            "vtext/m",
            0,
            json!({"defaults": {"repeat_penalty": 1.2}, "locked": ["repeat_penalty"]})
        )
        .await
        .status(),
        200
    );
    // The settings' value goes out under vLLM's name; the client's own
    // vLLM-spelled value loses to the lock.
    let r = chat(
        &gw,
        None,
        json!({"model": "vtext/m", "messages": [{"role": "user", "content": "hi"}],
               "repetition_penalty": 1.9}),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let header = r.headers()["x-switchback-settings"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(header.contains("locked=repeat_penalty"), "{header}");
    let body = last_body(&seen);
    assert_eq!(body["repetition_penalty"], json!(1.2), "{body}");
    assert!(body.get("repeat_penalty").is_none(), "{body}");
    // Unlocked: the client's own value (either spelling) wins and is sent
    // as the client sent it.
    assert_eq!(
        put_settings(
            &gw,
            "vtext/m",
            1,
            json!({"defaults": {"repeat_penalty": 1.2}})
        )
        .await
        .status(),
        200
    );
    let r = chat(
        &gw,
        None,
        json!({"model": "vtext/m", "messages": [{"role": "user", "content": "hi"}],
               "repetition_penalty": 1.9}),
        None,
    )
    .await;
    assert_eq!(r.status(), 200);
    let body = last_body(&seen);
    assert_eq!(body["repetition_penalty"], json!(1.9), "{body}");
    assert!(body.get("repeat_penalty").is_none(), "{body}");
}

#[tokio::test]
async fn defaults_profiles_request_and_locks_reach_the_engine_in_order() {
    let (upstream, seen) = spawn_upstream().await;
    let (gw, _) = spawn_gateway(&config(&upstream, ""), None).await;
    let doc = json!({
        "defaults": {"temperature": 0.6, "top_k": 20, "min_p": 0.05, "seed": 7,
                     "max_tokens": 256, "extra": {"dry_multiplier": 0.8}},
        "profiles": {
            "precise": {"temperature": 0.2, "top_k": 5},
            "nothink": {"reasoning": {"enabled": false}, "temperature": 0.7}
        },
        "locked": ["seed"]
    });
    assert_eq!(put_settings(&gw, "text/m", 0, doc).await.status(), 200);

    // Defaults only; the request's own temperature wins, the locked seed
    // replaces the request's seed and the header says so.
    let mut body = hi("fast");
    body["temperature"] = json!(1.1);
    body["seed"] = json!(99);
    let r = chat(&gw, None, body, None).await;
    assert_eq!(r.status(), 200);
    let header = r.headers()["x-switchback-settings"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(header.contains("locked=seed"), "{header}");
    let sent = last_body(&seen);
    assert_eq!(
        sent["temperature"].as_f64().map(|t| (t * 10.0).round()),
        Some(11.0)
    );
    assert_eq!(sent["seed"], json!(7));
    assert_eq!(sent["top_k"], json!(20));
    assert_eq!(sent["min_p"], json!(0.05));
    assert_eq!(sent["max_tokens"], json!(256));
    assert_eq!(sent["dry_multiplier"], json!(0.8));

    // Route profile (careful -> precise) over defaults.
    chat(&gw, None, hi("careful"), None).await;
    let sent = last_body(&seen);
    assert_eq!(sent["temperature"], json!(0.2));
    assert_eq!(sent["top_k"], json!(5));

    // Request profile by header beats the route profile; reasoning off is
    // translated for llama.cpp.
    let r = chat(&gw, None, hi("careful"), Some("nothink")).await;
    assert!(r.headers()["x-switchback-settings"]
        .to_str()
        .unwrap()
        .contains("profile=nothink"));
    let sent = last_body(&seen);
    assert_eq!(sent["temperature"], json!(0.7));
    assert_eq!(
        sent["top_k"],
        json!(5),
        "route profile still under the request profile"
    );
    assert_eq!(
        sent["chat_template_kwargs"]["enable_thinking"],
        json!(false)
    );

    // Profile and settings in the body; the body object never reaches the
    // engine.
    let mut body = hi("fast");
    body["switchback"] = json!({"profile": "precise", "settings": {"top_k": 3, "seed": 1}});
    chat(&gw, None, body, None).await;
    let sent = last_body(&seen);
    assert_eq!(sent["temperature"], json!(0.2));
    assert_eq!(sent["top_k"], json!(3), "request settings beat the profile");
    assert_eq!(
        sent["seed"],
        json!(7),
        "locked key ignores request settings"
    );
    assert!(
        sent.get("switchback").is_none(),
        "switchback object reached the engine"
    );
    let raw = String::from_utf8(seen.lock().unwrap().last().unwrap().to_vec()).unwrap();
    assert!(!raw.contains("switchback"));
}

#[tokio::test]
async fn unknown_profile_and_unknown_key_are_400s() {
    let (upstream, seen) = spawn_upstream().await;
    let (gw, _) = spawn_gateway(&config(&upstream, ""), None).await;
    put_settings(
        &gw,
        "text/m",
        0,
        json!({"profiles": {"nothink": {"reasoning": {"enabled": false}}, "long": {"max_tokens": 9000}}}),
    )
    .await;
    let r = chat(&gw, None, hi("fast"), Some("nope")).await;
    assert_eq!(r.status(), 400);
    let e: Value = r.json().await.unwrap();
    let message = e["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("long") && message.contains("nothink"),
        "{message}"
    );

    let mut body = hi("fast");
    body["switchback"] = json!({"settings": {"tempreature": 1}});
    let r = chat(&gw, None, body, None).await;
    assert_eq!(r.status(), 400);
    let e: Value = r.json().await.unwrap();
    assert!(e["error"]["message"]
        .as_str()
        .unwrap()
        .contains("tempreature"));

    let r = put_settings(&gw, "text/m", 1, json!({"defaults": {"bogus": 1}})).await;
    assert_eq!(r.status(), 400);
    assert_eq!(
        seen.lock().unwrap().len(),
        0,
        "a refused request reached the engine"
    );
}

#[tokio::test]
async fn revisions_conflicts_patch_history_and_revert() {
    let (upstream, _) = spawn_upstream().await;
    let (gw, _) = spawn_gateway(&config(&upstream, ""), None).await;
    let client = reqwest::Client::new();
    let url = format!("{gw}/v1/models/text/m/settings");

    // A write must say which revision it edits.
    let r = client
        .put(&url)
        .json(&json!({"defaults": {"temperature": 0.5}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 428);

    assert_eq!(
        put_settings(&gw, "text/m", 0, json!({"defaults": {"temperature": 0.5}}))
            .await
            .status(),
        200
    );
    // Two writers edited revision 0: the second gets 409.
    let r = put_settings(&gw, "text/m", 0, json!({"defaults": {"temperature": 0.9}})).await;
    assert_eq!(r.status(), 409);
    let e: Value = r.json().await.unwrap();
    assert_eq!(e["error"]["current_revision"], json!(1));

    // PATCH merges; null removes; body revision works too.
    let r = client
        .patch(&url)
        .json(&json!({"revision": 1, "defaults": {"temperature": null, "top_k": 40}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let view: Value = r.json().await.unwrap();
    assert_eq!(view["revision"], json!(2));
    assert_eq!(view["document"], json!({"defaults": {"top_k": 40}}));

    let history: Value = client
        .get(format!("{gw}/v1/models/text/m/settings/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = history["history"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["action"], json!("patch"));
    assert_eq!(
        rows[0]["changed"],
        json!(["defaults.temperature", "defaults.top_k"])
    );

    // Revert is a write: it names the revision it edits.

    let r = client
        .post(format!("{gw}/v1/models/text/m/settings/revert"))
        .json(&json!({"revision": 1}))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 428);

    let r = client
        .post(format!("{gw}/v1/models/text/m/settings/revert"))
        .header("if-match", "*")
        .json(&json!({"revision": 1}))
        .send()
        .await
        .unwrap();

    assert_eq!(r.status(), 200);

    assert_eq!(r.headers()["etag"], "\"3\"");
    let view: Value = r.json().await.unwrap();
    assert_eq!(view["revision"], json!(3));
    assert_eq!(view["document"], json!({"defaults": {"temperature": 0.5}}));

    // The effective view: every key, with its source.
    let view: Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    let effective = &view["targets"][0]["effective"];
    assert_eq!(effective["temperature"]["value"], json!(0.5));
    assert_eq!(effective["temperature"]["source"], json!("defaults"));
    assert_eq!(effective["top_k"]["source"], json!("engine"));
    assert_eq!(view["revision"], json!(3));
}

#[tokio::test]
async fn settings_survive_a_restart() {
    let (upstream, seen) = spawn_upstream().await;
    let dir = std::env::temp_dir().join(format!("sb-model-settings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("state.sqlite");
    let _ = std::fs::remove_file(&db);
    let db = db.to_str().unwrap().to_string();
    {
        let (gw, _) = spawn_gateway(&config(&upstream, ""), Some(&db)).await;
        assert_eq!(
            put_settings(&gw, "text/m", 0, json!({"defaults": {"top_k": 33}}))
                .await
                .status(),
            200
        );
    }
    let (gw, _) = spawn_gateway(&config(&upstream, ""), Some(&db)).await;
    let view: Value = reqwest::get(format!("{gw}/v1/models/text/m/settings"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["revision"], json!(1));
    assert_eq!(view["targets"][0]["durable"], json!(true));
    chat(&gw, None, hi("fast"), None).await;
    assert_eq!(last_body(&seen)["top_k"], json!(33));
    let history: Value = reqwest::get(format!("{gw}/v1/models/text/m/settings/history"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history["history"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn client_keys_read_only_their_targets_and_never_write() {
    let (upstream, _) = spawn_upstream().await;
    let extra = r#"
tenants:
  - id: acme
    allowed_providers: ["text"]
api_keys:
  - key: "sk-admin"
    tenant: ops
    role: admin
  - key: "sk-client"
    tenant: acme
    role: client
"#;
    let (gw, _) = spawn_gateway(&config(&upstream, extra), None).await;
    let client = reqwest::Client::new();
    let r = client
        .put(format!("{gw}/v1/models/text/m/settings"))
        .bearer_auth("sk-admin")
        .header("if-match", "0")
        .json(&json!({"defaults": {"temperature": 0.3}, "locked": ["temperature"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .get(format!("{gw}/v1/models/text/m/settings"))
        .bearer_auth("sk-client")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let view: Value = r.json().await.unwrap();
    assert_eq!(
        view["targets"][0]["effective"]["temperature"]["value"],
        json!(0.3)
    );
    assert_eq!(
        view["targets"][0]["effective"]["temperature"]["locked"],
        json!(true)
    );
    assert!(
        view["targets"][0].get("document").is_none(),
        "client saw the document"
    );
    assert!(
        view["targets"][0].get("load").is_none(),
        "client saw load settings"
    );

    // A target the tenant cannot reach is not there for it.
    let r = client
        .get(format!("{gw}/v1/models/other/x/settings"))
        .bearer_auth("sk-client")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    for request in [
        client.put(format!("{gw}/v1/models/text/m/settings")),
        client.patch(format!("{gw}/v1/models/text/m/settings")),
        client.post(format!("{gw}/v1/models/text/m/settings/revert")),
        client.get(format!("{gw}/v1/models/text/m/settings/history")),
        client.post(format!("{gw}/v1/models/text/m/load")),
        client.post(format!("{gw}/v1/models/text/m/unload")),
    ] {
        let r = request
            .bearer_auth("sk-client")
            .header("if-match", "1")
            .json(&json!({"defaults": {}}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }
}

#[tokio::test]
async fn the_switchback_object_never_reaches_an_embeddings_upstream() {
    let (upstream, seen) = spawn_upstream().await;
    let (gw, _) = spawn_gateway(&config(&upstream, ""), None).await;
    let r = reqwest::Client::new()
        .post(format!("{gw}/v1/embeddings"))
        .json(&json!({"model": "text/m", "input": "hi", "switchback": {"profile": "x"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let raw = String::from_utf8(seen.lock().unwrap().last().unwrap().to_vec()).unwrap();
    assert!(!raw.contains("switchback"), "{raw}");
}
