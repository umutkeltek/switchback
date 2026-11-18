//! Public model ids for workflows: a ComfyUI workflow can carry a public
//! `model_id` (named by job, e.g. `image/flux`) and `aliases` (old names kept
//! working during a move). Callers see and are recorded under the public id;
//! aliases resolve on every request path but are never listed; a request
//! through an alias keeps the name it sent. A config without the keys keeps
//! the automatic `<provider>/<workflow>` id.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

// --- a ComfyUI stand-in -----------------------------------------------------

#[derive(Clone, Default)]
struct FakeComfy {
    prompts: Arc<Mutex<usize>>,
}

async fn comfy_prompt(State(comfy): State<FakeComfy>) -> Json<Value> {
    *comfy.prompts.lock().unwrap() += 1;
    Json(json!({"prompt_id": "prompt-1", "number": 1, "node_errors": {}}))
}

async fn comfy_history(axum::extract::Path(prompt_id): axum::extract::Path<String>) -> Json<Value> {
    Json(json!({
        prompt_id: {
            "status": {"completed": true},
            "outputs": {"9": {"images": [{"filename": "out_00001_.png", "subfolder": "", "type": "output"}]}}
        }
    }))
}

async fn comfy_view() -> impl IntoResponse {
    ([("content-type", "image/png")], png())
}

async fn comfy_stats() -> Json<Value> {
    Json(json!({"system": {"os": "test"}, "devices": []}))
}

async fn spawn_comfy() -> (String, FakeComfy) {
    let comfy = FakeComfy::default();
    let app = Router::new()
        .route("/prompt", post(comfy_prompt))
        .route("/history/{prompt_id}", get(comfy_history))
        .route("/view", get(comfy_view))
        .route("/system_stats", get(comfy_stats))
        .with_state(comfy.clone());
    (serve_router(app).await, comfy)
}

fn png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

async fn serve_router(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// --- the gateway ------------------------------------------------------------

/// Start a gateway on `config` after the same validation a load runs.
async fn spawn_gateway(config: &str) -> String {
    let cfg = sb_core::Config::from_yaml(config).unwrap();
    sb_runtime::Engine::validate_config(&cfg).expect("config loads");
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        Arc::new(sb_ledger::UsageLedger::in_memory()),
    );
    serve_router(sb_server::build_app(state)).await
}

/// A gateway config with ComfyUI provider `media` at `comfy_url` holding
/// image workflow `flux` (with `flux_keys` added to it) and a text route.
fn media_config(comfy_url: &str, flux_keys: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: up
    type: mock
  - id: media
    type: comfyui
    base_url: "{comfy_url}"
    workflows:
      - id: flux
        kind: image_generation
        graph:
          "6":
            class_type: CLIPTextEncode
            inputs: {{ text: "" }}
          "9":
            class_type: SaveImage
            inputs: {{ filename_prefix: "out" }}
        bindings:
          prompt: {{ path: ["6", "inputs", "text"] }}
        output_node_ids: ["9"]
{flux_keys}
routes:
  - name: text
    match: {{ model: "*" }}
    targets: ["up/echo"]
"#
    )
}

const PUBLIC_WITH_OLD_ALIAS: &str =
    "        model_id: image/flux\n        aliases: [media/flux, flux-v1]";

async fn get_json(url: String) -> Value {
    let response = reqwest::get(&url).await.unwrap();
    assert!(
        response.status().is_success(),
        "GET {url}: {}",
        response.status()
    );
    response.json().await.unwrap()
}

async fn post_json(url: String, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn generate(base: &str, model: &str) -> reqwest::Response {
    post_json(
        format!("{base}/v1/images/generations"),
        json!({"model": model, "prompt": "a lighthouse"}),
    )
    .await
}

/// Submit a background image job naming the workflow by `field`
/// (`workflow` or `model`) and wait for it to finish.
async fn run_job(base: &str, field: &str, name: &str) -> Value {
    let response = post_json(
        format!("{base}/v1/jobs"),
        json!({"kind": "image_generation", field: name, "inputs": {"prompt": "a lighthouse"}}),
    )
    .await;
    assert!(
        response.status().is_success(),
        "submit {field}={name}: {}",
        response.status()
    );
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    for _ in 0..200 {
        let job = get_json(format!("{base}/v1/jobs/{id}")).await;
        if job["status"] == "succeeded" {
            return job;
        }
        assert_ne!(job["status"], "failed", "{job}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("job {id} never succeeded");
}

async fn job_of(base: &str, created: &Value) -> Value {
    let id = created["job"]["id"].as_str().unwrap();
    get_json(format!("{base}/v1/jobs/{id}")).await
}

fn workflow_ids(listing: &Value) -> Vec<String> {
    listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|workflow| workflow["id"].as_str().unwrap().to_string())
        .collect()
}

fn reasons(job: &Value) -> Vec<String> {
    job["route_decision"]["reason"]
        .as_array()
        .unwrap()
        .iter()
        .map(|reason| reason.as_str().unwrap().to_string())
        .collect()
}

// --- without the keys: as before ----------------------------------------------

#[tokio::test]
async fn without_the_keys_the_automatic_id_is_the_public_id() {
    let (comfy_url, _comfy) = spawn_comfy().await;
    let base = spawn_gateway(&media_config(&comfy_url, "")).await;

    let listing = get_json(format!("{base}/v1/workflows")).await;
    assert!(workflow_ids(&listing).contains(&"media/flux".to_string()));

    let response = generate(&base, "media/flux").await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await.unwrap();
    assert_eq!(created["model"], "media/flux");
    let job = job_of(&base, &created).await;
    assert_eq!(job["target"], "media/flux");
    assert!(job.get("requested_model").is_none(), "{job}");
    assert!(job["artifacts"][0]["provenance"].get("model").is_none());
    assert!(!reasons(&job)
        .iter()
        .any(|r| r.starts_with("requested_model=")));

    let usage = get_json(format!("{base}/v1/usage")).await;
    assert_eq!(usage["recent"][0]["model"], "media/flux");
    assert!(usage["recent"][0].get("requested_model").is_none());

    // No public name exists until one is configured.
    assert_eq!(
        generate(&base, "image/flux").await.status(),
        StatusCode::BAD_REQUEST
    );
}

// --- model_id ----------------------------------------------------------------

#[tokio::test]
async fn model_id_is_the_name_in_images_jobs_listing_and_records() {
    let (comfy_url, _comfy) = spawn_comfy().await;
    let base = spawn_gateway(&media_config(&comfy_url, "        model_id: image/flux")).await;

    // Listing: the public id, not the provider's.
    let ids = workflow_ids(&get_json(format!("{base}/v1/workflows")).await);
    assert!(ids.contains(&"image/flux".to_string()), "{ids:?}");
    assert!(!ids.contains(&"media/flux".to_string()), "{ids:?}");

    // Images: answered and recorded under the public id.
    let response = generate(&base, "image/flux").await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await.unwrap();
    assert_eq!(created["model"], "image/flux");
    let job = job_of(&base, &created).await;
    assert_eq!(job["target"], "image/flux");
    assert_eq!(job["route_decision"]["selected"]["target_id"], "image/flux");
    assert_eq!(job["artifacts"][0]["provenance"]["model"], "image/flux");
    assert!(job.get("requested_model").is_none(), "{job}");

    // Jobs: by `workflow` and by `model`.
    for field in ["workflow", "model"] {
        let job = run_job(&base, field, "image/flux").await;
        assert_eq!(job["target"], "image/flux", "{field}");
        assert_eq!(job["artifacts"][0]["provenance"]["model"], "image/flux");
    }

    let usage = get_json(format!("{base}/v1/usage")).await;
    for record in usage["recent"].as_array().unwrap() {
        assert_eq!(record["model"], "image/flux", "{record}");
    }

    // Setting model_id retires the automatic id unless it is listed.
    assert_eq!(
        generate(&base, "media/flux").await.status(),
        StatusCode::BAD_REQUEST
    );
    let refused = post_json(
        format!("{base}/v1/jobs"),
        json!({"kind": "image_generation", "workflow": "media/flux", "inputs": {"prompt": "x"}}),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
}

// --- aliases -----------------------------------------------------------------

#[tokio::test]
async fn an_alias_is_accepted_everywhere_hidden_from_listings_and_recorded_under_the_public_id() {
    let (comfy_url, comfy) = spawn_comfy().await;
    let base = spawn_gateway(&media_config(&comfy_url, PUBLIC_WITH_OLD_ALIAS)).await;

    // Hidden from both listings.
    let ids = workflow_ids(&get_json(format!("{base}/v1/workflows")).await);
    assert!(ids.contains(&"image/flux".to_string()), "{ids:?}");
    for alias in ["media/flux", "flux-v1"] {
        assert!(!ids.contains(&alias.to_string()), "{alias} listed: {ids:?}");
    }
    let models = get_json(format!("{base}/v1/models")).await;
    let model_ids: Vec<&str> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap())
        .collect();
    for alias in ["media/flux", "flux-v1"] {
        assert!(
            !model_ids.contains(&alias),
            "{alias} in /v1/models: {model_ids:?}"
        );
    }

    // Images through the old automatic id: served, answered and recorded
    // under the public id, the sent name kept.
    let response = generate(&base, "media/flux").await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await.unwrap();
    assert_eq!(created["model"], "image/flux");
    let job = job_of(&base, &created).await;
    assert_eq!(job["target"], "image/flux");
    assert_eq!(job["requested_model"], "media/flux");
    assert_eq!(job["route_decision"]["selected"]["target_id"], "image/flux");
    assert!(reasons(&job).contains(&"requested_model=media/flux".to_string()));
    assert_eq!(job["events"][0]["detail"]["model"], "image/flux");
    assert_eq!(job["events"][0]["detail"]["requested_model"], "media/flux");
    assert_eq!(job["artifacts"][0]["provenance"]["model"], "image/flux");

    // Jobs through the other alias, by `workflow` and by `model`.
    for field in ["workflow", "model"] {
        let job = run_job(&base, field, "flux-v1").await;
        assert_eq!(job["target"], "image/flux", "{field}");
        assert_eq!(job["requested_model"], "flux-v1", "{field}");
    }
    assert_eq!(*comfy.prompts.lock().unwrap(), 3, "every alias request ran");

    // Usage: every record under the public id; the alias ones keep the
    // name sent, so an old name's last use is visible.
    let usage = get_json(format!("{base}/v1/usage")).await;
    let recent = usage["recent"].as_array().unwrap();
    assert_eq!(recent.len(), 3, "{usage}");
    let mut sent: Vec<&str> = recent
        .iter()
        .map(|record| {
            assert_eq!(record["model"], "image/flux", "{record}");
            record["requested_model"].as_str().unwrap()
        })
        .collect();
    sent.sort_unstable();
    assert_eq!(sent, ["flux-v1", "flux-v1", "media/flux"]);
    // Rollup buckets are `[requests, cost]`.
    assert_eq!(usage["by_model"]["image/flux"][0], 3, "{usage}");
    assert!(usage["by_model"].get("media/flux").is_none(), "{usage}");

    // A request by the public id keeps no sent name.
    let created: Value = generate(&base, "image/flux").await.json().await.unwrap();
    assert!(job_of(&base, &created)
        .await
        .get("requested_model")
        .is_none());
}

#[tokio::test]
async fn a_client_picking_by_last_path_part_finds_exactly_one_workflow() {
    let (comfy_url, _comfy) = spawn_comfy().await;
    let base = spawn_gateway(&media_config(&comfy_url, PUBLIC_WITH_OLD_ALIAS)).await;

    let ids = workflow_ids(&get_json(format!("{base}/v1/workflows")).await);
    let matches: Vec<&String> = ids
        .iter()
        .filter(|id| id.rsplit('/').next() == Some("flux"))
        .collect();
    assert_eq!(matches, [&"image/flux".to_string()], "{ids:?}");
}

// --- refused at load -----------------------------------------------------------

#[test]
fn clashing_workflow_names_are_refused_at_load_with_both_owners_named() {
    let two_providers = r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: up
    type: mock
  - id: media
    type: comfyui
    base_url: "http://127.0.0.1:9"
    workflows:
      - id: flux
        kind: image_generation
        graph: {}
        model_id: image/flux
  - id: spare
    type: comfyui
    base_url: "http://127.0.0.1:9"
    workflows:
      - id: flux
        kind: image_generation
        graph: {}
        model_id: image/flux
routes:
  - name: text
    match: { model: "*" }
    targets: ["up/echo"]
"#;
    let cases = [
        (
            two_providers.to_string(),
            "providers `spare` workflow `flux` model_id `image/flux` is already used by providers `media` workflow `flux` model_id",
        ),
        (
            media_config("http://127.0.0.1:9", "        aliases: [text]"),
            "providers `media` workflow `flux` aliases[0] `text` is already used by routes[0] name",
        ),
        (
            media_config(
                "http://127.0.0.1:9",
                "        model_id: image/flux\n        aliases: [image/flux]",
            ),
            "providers `media` workflow `flux` aliases[0] `image/flux` equals its own model_id",
        ),
    ];
    for (config, expected) in cases {
        let cfg = sb_core::Config::from_yaml(&config).unwrap();
        let error = sb_runtime::Engine::validate_config(&cfg).expect_err(expected);
        assert!(
            error.contains(expected),
            "expected `{expected}` in: {error}"
        );
    }
}

// --- renaming a text lane is config alone -------------------------------------

async fn text_lane_chat(Json(body): Json<Value>) -> Json<Value> {
    Json(json!({
        "id": "chatcmpl-1", "object": "chat.completion",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": format!("served={}", body["model"])}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
}

#[tokio::test]
async fn a_text_provider_lane_and_route_renamed_together_load_and_route() {
    let lane = serve_router(
        Router::new()
            .route("/chat/completions", post(text_lane_chat))
            .route("/health", get(|| async { StatusCode::OK })),
    )
    .await;
    // The provider, its managed lane and the route targets all carry the
    // new name `text`; nothing else refers to the old one.
    let config = format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: text
    type: openai_compatible
    base_url: "{lane}"
    accounts:
      - {{ id: a1, auth: {{ kind: none }} }}
local_executors:
  - name: text
    base_url: "{lane}"
    health_endpoint: "{lane}/health"
    route_health: true
    health_poll_interval_ms: 50
routes:
  - name: chat
    match: {{ model: "chat" }}
    targets: ["text/small"]
"#
    );
    let base = spawn_gateway(&config).await;

    let response = post_json(
        format!("{base}/v1/chat/completions"),
        json!({"model": "chat", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "served=\"small\"");

    let usage = get_json(format!("{base}/v1/usage")).await;
    assert_eq!(usage["recent"][0]["provider_id"], "text", "{usage}");
}
