//! Image-in, image-out workflow jobs: `POST /v1/jobs` with kind
//! `workflow_execution`.
//!
//! A configured ComfyUI workflow takes an uploaded image (and an optional
//! text prompt), runs on the media lane like any other media job, and
//! gives its output back as an artifact. The mask is what the caller
//! saves; the job itself is cancellable while it runs.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const CLIENT: &str = "sk-test-client-a";
const OPERATOR: &str = "sk-test-operator";

/// The upload ComfyUI got: its content type and its body.
type Upload = (String, Vec<u8>);

/// A fake ComfyUI: it records the graph it was sent, the upload it got and
/// the calls it answered.
#[derive(Clone, Default)]
struct FakeComfy {
    /// History answers `{}` (the job is still running) this many times first.
    pending: usize,
    /// Never finish (for a cancel).
    forever: bool,
    history_calls: Arc<AtomicUsize>,
    calls: Arc<Mutex<Vec<String>>>,
    /// The upload's content type and body.
    upload: Arc<Mutex<Option<Upload>>>,
    /// The graph the job was queued with.
    prompt: Arc<Mutex<Option<Value>>>,
    /// The cancels it was told about.
    cancels: Arc<Mutex<Vec<(String, Value)>>>,
    viewed: Arc<AtomicBool>,
}

impl FakeComfy {
    fn record(&self, path: &str) {
        self.calls.lock().unwrap().push(path.to_string());
    }
}

/// A PNG the fake serves as the mask.
fn png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

async fn spawn_comfy(fake: FakeComfy) -> String {
    async fn upload(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        body: axum::body::Bytes,
    ) -> Json<Value> {
        fake.record("/upload/image");
        let content_type = headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        *fake.upload.lock().unwrap() = Some((content_type, body.to_vec()));
        Json(json!({"name": "input.png", "subfolder": "sb", "type": "input"}))
    }
    async fn prompt(State(fake): State<FakeComfy>, Json(body): Json<Value>) -> Json<Value> {
        fake.record("/prompt");
        *fake.prompt.lock().unwrap() = Some(body);
        Json(json!({"prompt_id": "wf-1", "number": 1, "node_errors": {}}))
    }
    async fn history(State(fake): State<FakeComfy>, Path(id): Path<String>) -> Json<Value> {
        fake.record("/history");
        let calls = fake.history_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if fake.forever || calls <= fake.pending {
            return Json(json!({}));
        }
        Json(json!({ id: {
            "status": {"completed": true},
            "outputs": {
                "4": {"images": [{"filename": "mask_00001_.png", "subfolder": "", "type": "output"}]}
            }
        }}))
    }
    async fn view(State(fake): State<FakeComfy>) -> impl IntoResponse {
        fake.record("/view");
        fake.viewed.store(true, Ordering::SeqCst);
        ([("content-type", "image/png")], png())
    }
    async fn queue(State(fake): State<FakeComfy>, Json(body): Json<Value>) -> Json<Value> {
        fake.record("/queue");
        fake.cancels
            .lock()
            .unwrap()
            .push(("/queue".to_string(), body));
        Json(json!({}))
    }
    async fn queue_state(State(fake): State<FakeComfy>) -> Json<Value> {
        fake.record("/queue/state");
        Json(json!({"queue_running": [[1, "wf-1", {}, {}, []]], "queue_pending": []}))
    }
    async fn interrupt(State(fake): State<FakeComfy>, Json(body): Json<Value>) -> Json<Value> {
        fake.record("/interrupt");
        fake.cancels
            .lock()
            .unwrap()
            .push(("/interrupt".to_string(), body));
        Json(json!({}))
    }
    let app = Router::new()
        .route("/upload/image", post(upload))
        .route("/prompt", post(prompt))
        .route("/history/{id}", get(history))
        .route("/view", get(view))
        .route("/queue", get(queue_state).post(queue))
        .route("/interrupt", post(interrupt))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// A gateway with one `workflow_execution` workflow: it takes an image and
/// an optional text, and saves a mask.
fn config(comfy_url: &str) -> String {
    format!(
        r#"
api_keys:
  - {{ key: "{CLIENT}", tenant: team-a }}
  - {{ key: "{OPERATOR}", tenant: team-a, role: operator }}
tenants:
  - id: team-a
providers:
  - id: comfy
    type: comfyui
    base_url: "{comfy_url}"
    workflows:
      - id: sam3
        kind: workflow_execution
        model_id: segment/sam3
        timeout_secs: 60
        graph:
          "1": {{ class_type: LoadImage, inputs: {{ image: "" }} }}
          "2": {{ class_type: CLIPTextEncode, inputs: {{ text: "everything" }} }}
          "3": {{ class_type: ExampleSegmenter, inputs: {{ image: ["1", 0], text: ["2", 0] }} }}
          "4": {{ class_type: SaveImage, inputs: {{ images: ["3", 0], filename_prefix: "mask" }} }}
        bindings:
          image: {{ path: ["1", "inputs", "image"] }}
          prompt: {{ path: ["2", "inputs", "text"] }}
        output_node_ids: ["4"]
"#
    )
}

async fn serve(config: &str) -> String {
    let cfg = sb_core::Config::from_yaml(config).unwrap();
    sb_runtime::Engine::validate_config(&cfg).expect("valid test config");
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

async fn upload_image(base: &str, key: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/uploads"))
        .bearer_auth(key)
        .header("content-type", "image/png")
        .body(png())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let record: Value = response.json().await.unwrap();
    assert_eq!(record["media_type"], "image/png");
    record["artifact_id"].as_str().unwrap().to_string()
}

async fn submit(base: &str, key: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/v1/jobs"))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn get_json(base: &str, key: &str, path: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}{path}"))
        .bearer_auth(key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_for(base: &str, id: &str, status: &str, seconds: u64) -> Value {
    for _ in 0..seconds * 20 {
        let job = get_json(base, CLIENT, &format!("/v1/jobs/{id}")).await;
        if job["status"] == status {
            return job;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!(
        "job {id} never reached `{status}`: {}",
        get_json(base, CLIENT, &format!("/v1/jobs/{id}")).await
    );
}

// A workflow job takes an image, binds it into the graph, and gives the
// mask back as an artifact.
#[tokio::test]
async fn a_workflow_job_uploads_its_image_and_saves_the_mask() {
    let fake = FakeComfy {
        pending: 3,
        ..FakeComfy::default()
    };
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url)).await;

    let image = upload_image(&base, CLIENT).await;
    let response = submit(
        &base,
        CLIENT,
        json!({"kind": "workflow_execution", "model": "segment/sam3",
               "inputs": {"image": image}}),
    )
    .await;
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    assert_eq!(accepted["kind"], "workflow_execution");
    assert_eq!(accepted["target"], "segment/sam3");
    let id = accepted["id"].as_str().unwrap().to_string();

    let done = wait_for(&base, &id, "succeeded", 20).await;
    assert!(
        fake.history_calls.load(Ordering::SeqCst) > 3,
        "polled while the job ran"
    );

    // The image went to ComfyUI's upload as a multipart `image` file.
    let (content_type, body) = fake.upload.lock().unwrap().clone().unwrap();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("name=\"image\"; filename=\"switchback-art_"),
        "{text}"
    );
    assert!(text.contains("name=\"type\"\r\n\r\ninput"));
    assert!(body
        .windows(png().len())
        .any(|window| window == png().as_slice()));

    // The graph the job was queued with carries the uploaded file's name.
    let prompt = fake.prompt.lock().unwrap().clone().unwrap();
    let graph = &prompt["prompt"];
    assert_eq!(graph["1"]["inputs"]["image"], "sb/input.png");
    assert!(prompt["client_id"]
        .as_str()
        .unwrap()
        .starts_with("switchback-"));

    // The mask is the job's artifact.
    let artifact = &done["artifacts"][0];
    assert_eq!(artifact["kind"], "image");
    assert_eq!(artifact["media_type"], "image/png");
    assert_eq!(artifact["bytes"], png().len());
    assert!(
        !done.to_string().contains("switchback-art_"),
        "no artifact id: {done}"
    );

    let artifact_id = artifact["artifact_id"].as_str().unwrap();
    let response = reqwest::Client::new()
        .get(format!("{base}/v1/artifacts/{artifact_id}"))
        .bearer_auth(CLIENT)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    assert_eq!(response.bytes().await.unwrap().as_ref(), png().as_slice());
}

// The text prompt is bound into the graph when the caller gives one.
#[tokio::test]
async fn a_workflow_job_binds_its_text_prompt() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url)).await;

    let image = upload_image(&base, CLIENT).await;
    let response = submit(
        &base,
        CLIENT,
        json!({"kind": "workflow_execution", "model": "segment/sam3",
               "inputs": {"image": image, "prompt": "the cat"}}),
    )
    .await;
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    wait_for(&base, &id, "succeeded", 20).await;

    let prompt = fake.prompt.lock().unwrap().clone().unwrap();
    assert_eq!(prompt["prompt"]["2"]["inputs"]["text"], "the cat");
}

// The text is optional: without one the graph keeps its own default.
#[tokio::test]
async fn a_workflow_job_may_run_without_a_text_prompt() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url)).await;

    let image = upload_image(&base, CLIENT).await;
    let response = submit(
        &base,
        CLIENT,
        json!({"kind": "workflow_execution", "model": "segment/sam3",
               "inputs": {"image": image}}),
    )
    .await;
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();
    wait_for(&base, &id, "succeeded", 20).await;

    let prompt = fake.prompt.lock().unwrap().clone().unwrap();
    assert_eq!(
        prompt["prompt"]["2"]["inputs"]["text"], "everything",
        "the graph keeps its own text: {prompt}"
    );
}

// A workflow job can be cancelled while it runs.
#[tokio::test]
async fn a_workflow_job_can_be_cancelled() {
    let fake = FakeComfy {
        forever: true,
        ..FakeComfy::default()
    };
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url)).await;

    let image = upload_image(&base, CLIENT).await;
    let response = submit(
        &base,
        CLIENT,
        json!({"kind": "workflow_execution", "model": "segment/sam3",
               "inputs": {"image": image}}),
    )
    .await;
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    let id = accepted["id"].as_str().unwrap().to_string();

    // The job runs: the graph was queued.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fake.prompt.lock().unwrap().is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the graph was never queued"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/jobs/{id}/cancel"))
        .bearer_auth(CLIENT)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cancelled: Value = response.json().await.unwrap();
    assert_eq!(cancelled["status"], "cancelled");

    // ComfyUI was told to drop the prompt.
    let cancels = fake.cancels.lock().unwrap().clone();
    assert!(
        cancels
            .iter()
            .any(|(path, body)| path == "/queue" && body["delete"] == json!(["wf-1"])),
        "{cancels:?}"
    );
    assert!(
        !fake.viewed.load(Ordering::SeqCst),
        "the cancelled job fetched its artifact"
    );
}

// A workflow job is checked before the job exists: the kind, the workflow
// and its inputs.
#[tokio::test]
async fn a_workflow_job_is_checked_before_the_job_exists() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url)).await;
    let image = upload_image(&base, CLIENT).await;

    let cases = [
        // A kind that does not run.
        (
            json!({"kind": "text_generation", "model": "segment/sam3", "inputs": {"prompt": "x"}}),
            400,
        ),
        // A workflow that is not configured.
        (
            json!({"kind": "workflow_execution", "model": "segment/nope", "inputs": {"image": image}}),
            400,
        ),
        // An image workflow is not a workflow job.
        (
            json!({"kind": "workflow_execution", "model": "comfy/txt2img", "inputs": {"image": image}}),
            400,
        ),
        // An input the workflow does not bind.
        (
            json!({"kind": "workflow_execution", "model": "segment/sam3",
                   "inputs": {"image": image, "frames": 9}}),
            400,
        ),
        // A workflow that binds an image runs on one: none given.
        (
            json!({"kind": "workflow_execution", "model": "segment/sam3",
                   "inputs": {"prompt": "the cat"}}),
            400,
        ),
        // An input image the caller may not read.
        (
            json!({"kind": "workflow_execution", "model": "segment/sam3",
                   "inputs": {"image": "art_missing"}}),
            400,
        ),
    ];
    for (body, status) in cases {
        let response = submit(&base, CLIENT, body.clone()).await;
        assert_eq!(response.status(), status, "{body}");
    }
    let listed: Value = get_json(&base, CLIENT, "/v1/jobs").await;
    assert!(listed["data"].as_array().unwrap().is_empty(), "{listed}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

// The workflow shows up in the gateway's list of workflows, with the
// inputs it takes.
#[tokio::test]
async fn a_workflow_job_lists_its_workflow() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url)).await;

    let listed = get_json(&base, OPERATOR, "/v1/workflows").await;
    let workflow = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "segment/sam3")
        .expect("the workflow is listed");
    assert_eq!(workflow["kind"], "workflow_execution");
    let inputs: Vec<&str> = workflow["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["name"].as_str().unwrap())
        .collect();
    assert!(inputs.contains(&"image"), "{inputs:?}");
    assert!(inputs.contains(&"prompt"), "{inputs:?}");
    let required = |name: &str| {
        workflow["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|field| field["name"] == name)
            .unwrap()["required"]
            .clone()
    };
    assert_eq!(required("prompt"), false, "a workflow may run without one");
    assert_eq!(required("image"), true, "a workflow job runs on its image");

    // A client key may not list the workflows (it may still run one by id).
    let response = reqwest::Client::new()
        .get(format!("{base}/v1/workflows"))
        .bearer_auth(CLIENT)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
}

/// A gateway whose `workflow_execution` workflow binds an image but no
/// text prompt.
fn config_without_prompt(comfy_url: &str) -> String {
    config(comfy_url).replace(
        "          prompt: { path: [\"2\", \"inputs\", \"text\"] }\n",
        "",
    )
}

// A prompt the workflow cannot take is refused before the job exists, so
// no machine is woken for a job that would fail at binding; and the
// workflow lists no prompt input.
#[tokio::test]
async fn a_prompt_the_workflow_does_not_bind_is_refused_before_the_job_exists() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let config = config_without_prompt(&comfy_url);
    assert!(!config.contains("prompt: {"), "{config}");
    let base = serve(&config).await;
    let image = upload_image(&base, CLIENT).await;
    let response = submit(
        &base,
        CLIENT,
        json!({"kind": "workflow_execution", "model": "segment/sam3",
               "inputs": {"image": image, "prompt": "the cat"}}),
    )
    .await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not bind `prompt`"),
        "{body}"
    );
    let listed: Value = get_json(&base, CLIENT, "/v1/jobs").await;
    assert!(listed["data"].as_array().unwrap().is_empty(), "{listed}");
    assert!(fake.calls.lock().unwrap().is_empty(), "ComfyUI was reached");

    let workflows = get_json(&base, OPERATOR, "/v1/workflows").await;
    let workflow = workflows["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == "segment/sam3")
        .expect("the workflow is listed");
    let inputs: Vec<&str> = workflow["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["name"].as_str().unwrap())
        .collect();
    assert!(!inputs.contains(&"prompt"), "{inputs:?}");
}
