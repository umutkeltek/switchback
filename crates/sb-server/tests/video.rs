//! Video through a ComfyUI workflow: an input image uploaded through
//! Switchback, a long-running job polled to its end, the video stored as an
//! artifact with its media type, usage recorded as video, cancel mid-run,
//! and the provider's credential on every ComfyUI call.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const KEY_ENV: &str = "SB_TEST_VIDEO_COMFY_KEY";
const KEY: &str = "test-video-proxy-key-0000";

type Upload = (String, Vec<u8>);

#[derive(Clone, Default)]
struct FakeComfy {
    /// History answers `{}` (still running) this many times first.
    pending: usize,
    /// Never finish (for cancel and timeout).
    forever: bool,
    /// Answer like a core video node (`images` + `animated`) instead of a
    /// node pack's `gifs`.
    core_style: bool,
    history_calls: Arc<AtomicUsize>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
    /// The upload's content type and body.
    upload: Arc<Mutex<Option<Upload>>>,
    prompt: Arc<Mutex<Option<Value>>>,
    cancels: Arc<Mutex<Vec<(String, Value)>>>,
    viewed: Arc<AtomicBool>,
}

impl FakeComfy {
    fn record(&self, path: &str, headers: &HeaderMap) {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        self.calls.lock().unwrap().push((path.to_string(), auth));
    }
}

fn png() -> Vec<u8> {
    vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1,
        13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ]
}

/// A "video" big enough to arrive in several chunks.
fn video_bytes() -> Vec<u8> {
    (0..300_000u32).map(|n| (n % 251) as u8).collect()
}

async fn spawn_comfy(fake: FakeComfy) -> String {
    async fn upload(State(fake): State<FakeComfy>, headers: HeaderMap, body: Bytes) -> Json<Value> {
        fake.record("/upload/image", &headers);
        let content_type = headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        *fake.upload.lock().unwrap() = Some((content_type, body.to_vec()));
        Json(json!({"name": "frame.png", "subfolder": "sb", "type": "input"}))
    }
    async fn prompt(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.record("/prompt", &headers);
        *fake.prompt.lock().unwrap() = Some(body);
        Json(json!({"prompt_id": "vid-1", "number": 1, "node_errors": {}}))
    }
    async fn history(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Path(id): Path<String>,
    ) -> Json<Value> {
        fake.record("/history", &headers);
        let calls = fake.history_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if fake.forever || calls <= fake.pending {
            return Json(json!({}));
        }
        let outputs = if fake.core_style {
            json!({"20": {
                "images": [{"filename": "clip_00001_.mp4", "subfolder": "video", "type": "output"}],
                "animated": [true]
            }})
        } else {
            json!({"20": {"gifs": [{
                "filename": "clip_00001.mp4",
                "subfolder": "",
                "type": "output",
                "format": "video/h264-mp4",
                "frame_rate": 16.0,
                "fullpath": "/data/outputs/never-copied/clip_00001.mp4"
            }]}})
        };
        Json(json!({ id: {"status": {"completed": true}, "outputs": outputs} }))
    }
    async fn view(State(fake): State<FakeComfy>, headers: HeaderMap) -> impl IntoResponse {
        fake.record("/view", &headers);
        fake.viewed.store(true, Ordering::SeqCst);
        // A node pack's server or a proxy may answer a generic type: the
        // file name decides then.
        let content_type = if fake.core_style {
            "video/mp4"
        } else {
            "application/octet-stream"
        };
        ([("content-type", content_type)], video_bytes())
    }
    async fn queue(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.record("/queue", &headers);
        fake.cancels
            .lock()
            .unwrap()
            .push(("/queue".to_string(), body));
        Json(json!({}))
    }
    async fn queue_state(State(fake): State<FakeComfy>, headers: HeaderMap) -> Json<Value> {
        fake.record("/queue/state", &headers);
        Json(json!({"queue_running": [[1, "vid-1", {}, {}, []]], "queue_pending": []}))
    }
    async fn interrupt(
        State(fake): State<FakeComfy>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        fake.record("/interrupt", &headers);
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

fn config(comfy_url: &str, timeout_secs: u64) -> String {
    format!(
        r#"
api_keys:
  - {{ key: "sk-test-client-a", tenant: team-a }}
  - {{ key: "sk-test-client-b", tenant: team-b }}
  - {{ key: "sk-test-operator", tenant: team-a, role: operator }}
tenants:
  - id: team-a
  - id: team-b
providers:
  - id: comfy
    type: comfyui
    base_url: "{comfy_url}"
    api_key_env: {KEY_ENV}
    workflows:
      - id: img2vid
        kind: video_generation
        timeout_secs: {timeout_secs}
        graph:
          "10": {{ class_type: LoadImage, inputs: {{ image: "" }} }}
          "11": {{ class_type: CLIPTextEncode, inputs: {{ text: "" }} }}
          "12": {{ class_type: KSampler, inputs: {{ seed: 0 }} }}
          "13": {{ class_type: ExampleImageToVideo, inputs: {{ length: 1 }} }}
          "14": {{ class_type: CreateVideo, inputs: {{ fps: 1 }} }}
          "20": {{ class_type: SaveVideo, inputs: {{ filename_prefix: "clip" }} }}
        bindings:
          prompt: {{ path: ["11", "inputs", "text"] }}
          seed: {{ path: ["12", "inputs", "seed"] }}
          image: {{ path: ["10", "inputs", "image"] }}
          frames: {{ path: ["13", "inputs", "length"] }}
          fps: {{ path: ["14", "inputs", "fps"] }}
        output_node_ids: ["20"]
"#
    )
}

async fn serve(config: &str) -> String {
    std::env::set_var(KEY_ENV, KEY);
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

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn upload_frame(base: &str, key: &str) -> String {
    let response = client()
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
    assert_eq!(record["bytes"], png().len());
    record["artifact_id"].as_str().unwrap().to_string()
}

async fn submit_video(base: &str, key: &str, image: &str) -> reqwest::Response {
    client()
        .post(format!("{base}/v1/jobs"))
        .bearer_auth(key)
        .json(&json!({
            "kind": "video_generation",
            "workflow": "comfy/img2vid",
            "inputs": {"prompt": "waves roll in", "seed": 7, "image": image, "frames": 49, "fps": 16}
        }))
        .send()
        .await
        .unwrap()
}

async fn get_json(base: &str, key: &str, path: &str) -> Value {
    client()
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
        let job = get_json(base, "sk-test-client-a", &format!("/v1/jobs/{id}")).await;
        if job["status"] == status {
            return job;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "job {id} never reached `{status}`: {}",
        get_json(base, "sk-test-client-a", &format!("/v1/jobs/{id}")).await
    );
}

fn assert_credential_everywhere(fake: &FakeComfy) {
    let calls = fake.calls.lock().unwrap().clone();
    assert!(!calls.is_empty());
    for (path, auth) in calls {
        assert_eq!(
            auth,
            format!("Bearer {KEY}"),
            "{path} without the credential"
        );
    }
}

#[tokio::test]
async fn an_image_to_video_job_uploads_its_frame_and_stores_the_video() {
    let fake = FakeComfy {
        pending: 4,
        ..FakeComfy::default()
    };
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url, 120)).await;

    let frame = upload_frame(&base, "sk-test-client-a").await;
    let response = submit_video(&base, "sk-test-client-a", &frame).await;
    assert_eq!(response.status(), 202);
    let accepted: Value = response.json().await.unwrap();
    assert_eq!(accepted["kind"], "video_generation");
    let id = accepted["id"].as_str().unwrap().to_string();

    let done = wait_for(&base, &id, "succeeded", 20).await;
    assert!(
        fake.history_calls.load(Ordering::SeqCst) > 4,
        "polled while the job ran"
    );

    // The frame went to ComfyUI's upload as a multipart `image` file.
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

    // The bound graph: uploaded name, frames, fps, seed, prompt.
    let prompt = fake.prompt.lock().unwrap().clone().unwrap();
    let graph = &prompt["prompt"];
    assert_eq!(graph["10"]["inputs"]["image"], "sb/frame.png");
    assert_eq!(graph["13"]["inputs"]["length"], 49);
    assert_eq!(graph["14"]["inputs"]["fps"], 16);
    assert_eq!(graph["12"]["inputs"]["seed"], 7);
    assert_eq!(graph["11"]["inputs"]["text"], "waves roll in");

    let artifact = &done["artifacts"][0];
    assert_eq!(artifact["kind"], "video");
    assert_eq!(artifact["media_type"], "video/mp4");
    assert_eq!(artifact["fps"], 16.0);
    assert_eq!(artifact["bytes"], video_bytes().len());
    assert!(
        !done.to_string().contains("never-copied"),
        "no local path: {done}"
    );
    assert!(
        !done.to_string().contains("waves roll in"),
        "no prompt: {done}"
    );
    let events: Vec<_> = done["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event"].as_str().unwrap().to_string())
        .collect();
    assert!(events.contains(&"input_uploaded".to_string()), "{events:?}");

    let artifact_id = artifact["artifact_id"].as_str().unwrap();
    let response = client()
        .get(format!("{base}/v1/artifacts/{artifact_id}"))
        .bearer_auth("sk-test-client-a")
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "video/mp4");
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        video_bytes().as_slice()
    );

    // The ledger counts one video, not an image.
    let usage = get_json(&base, "sk-test-operator", "/v1/usage").await;
    let row = usage["recent"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["request_id"] == id.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("no usage row for {id}: {usage}"));
    assert_eq!(row["workload_kind"], "video_generation", "{row}");
    assert_eq!(row["pricing_unit"], "per_video", "{row}");
    assert_eq!(row["units_consumed"], 1.0, "{row}");

    assert_credential_everywhere(&fake);
}

#[tokio::test]
async fn a_core_video_node_output_is_read_from_images() {
    let fake = FakeComfy {
        core_style: true,
        ..FakeComfy::default()
    };
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url, 120)).await;
    let frame = upload_frame(&base, "sk-test-client-a").await;
    let id = submit_video(&base, "sk-test-client-a", &frame)
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let done = wait_for(&base, &id, "succeeded", 20).await;
    assert_eq!(done["artifacts"][0]["media_type"], "video/mp4");
    assert_eq!(done["artifacts"][0]["kind"], "video");
    assert_eq!(done["artifacts"][0]["provenance"]["subfolder"], "video");
}

#[tokio::test]
async fn a_cancel_mid_run_interrupts_comfyui_with_the_credential() {
    let fake = FakeComfy {
        forever: true,
        ..FakeComfy::default()
    };
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url, 120)).await;
    let frame = upload_frame(&base, "sk-test-client-a").await;
    let id = submit_video(&base, "sk-test-client-a", &frame)
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for(&base, &id, "running", 10).await;
    while fake.history_calls.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let cancelled: Value = client()
        .post(format!("{base}/v1/jobs/{id}/cancel"))
        .bearer_auth("sk-test-client-a")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    let cancels = fake.cancels.lock().unwrap().clone();
    assert_eq!(
        cancels,
        vec![
            ("/queue".to_string(), json!({"delete": ["vid-1"]})),
            ("/interrupt".to_string(), json!({"prompt_id": "vid-1"})),
        ]
    );
    assert!(!fake.viewed.load(Ordering::SeqCst));
    assert_credential_everywhere(&fake);
}

#[tokio::test]
async fn a_video_job_ends_at_its_workflow_timeout() {
    let fake = FakeComfy {
        forever: true,
        ..FakeComfy::default()
    };
    let comfy_url = spawn_comfy(fake).await;
    let base = serve(&config(&comfy_url, 1)).await;
    let frame = upload_frame(&base, "sk-test-client-a").await;
    let id = submit_video(&base, "sk-test-client-a", &frame)
        .await
        .json::<Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let failed = wait_for(&base, &id, "failed", 10).await;
    let last = failed["events"].as_array().unwrap().last().unwrap().clone();
    assert!(
        last["detail"]["error"]
            .as_str()
            .unwrap()
            .contains("did not produce artifacts within 1 s"),
        "{last}"
    );
}

#[tokio::test]
async fn video_runs_only_as_a_background_job_and_inputs_are_the_callers_own() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url, 120)).await;

    let blocking = client()
        .post(format!("{base}/v1/images/generations"))
        .bearer_auth("sk-test-client-a")
        .json(&json!({"model": "comfy/img2vid", "prompt": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocking.status(), 400);
    let error: Value = blocking.json().await.unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("POST /v1/jobs"));

    let not_an_image = client()
        .post(format!("{base}/v1/uploads"))
        .bearer_auth("sk-test-client-a")
        .header("content-type", "text/plain")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(not_an_image.status(), 415);

    // Another tenant's upload cannot be named as an input.
    let frame = upload_frame(&base, "sk-test-client-a").await;
    let response = submit_video(&base, "sk-test-client-b", &frame).await;
    assert_eq!(response.status(), 400);
    assert!(
        fake.calls.lock().unwrap().is_empty(),
        "nothing reached ComfyUI"
    );
}

#[tokio::test]
async fn the_workflow_list_shows_a_video_workflow_with_its_inputs() {
    let comfy_url = spawn_comfy(FakeComfy::default()).await;
    let base = serve(&config(&comfy_url, 120)).await;
    let listed = get_json(&base, "sk-test-operator", "/v1/workflows").await;
    let video = listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workflow| workflow["id"] == "comfy/img2vid")
        .cloned()
        .unwrap();
    assert_eq!(video["kind"], "video_generation");
    let inputs: Vec<_> = video["inputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(inputs, vec!["prompt", "seed", "image", "frames", "fps"]);
    assert_eq!(video["outputs"][0]["media_type"], "video/*");
}

#[tokio::test]
async fn an_upload_is_capped_while_it_arrives_and_must_be_the_image_it_claims() {
    let comfy_url = spawn_comfy(FakeComfy::default()).await;
    let base = serve(&format!(
        "server:\n  bind: \"127.0.0.1:0\"\n  artifacts:\n    max_upload_bytes: 100\n{}",
        config(&comfy_url, 120)
    ))
    .await;
    let mut too_big = png();
    too_big.resize(101, 0);
    let declared = client()
        .post(format!("{base}/v1/uploads"))
        .bearer_auth("sk-test-client-a")
        .header("content-type", "image/png")
        .body(too_big.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(declared.status(), 413);
    // No length announced (a chunked body): refused while it arrives.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        too_big.chunks(10).map(|chunk| Ok(chunk.to_vec())).collect();
    let streamed = client()
        .post(format!("{base}/v1/uploads"))
        .bearer_auth("sk-test-client-a")
        .header("content-type", "image/png")
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .unwrap();
    assert_eq!(streamed.status(), 413);

    let page = client()
        .post(format!("{base}/v1/uploads"))
        .bearer_auth("sk-test-client-a")
        .header("content-type", "image/png")
        .body("<html><script>alert(1)</script></html>")
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 415);

    // A real image within the cap is still taken.
    upload_frame(&base, "sk-test-client-a").await;
}

#[tokio::test]
async fn a_size_the_video_workflow_does_not_bind_is_refused() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = serve(&config(&comfy_url, 120)).await;
    let frame = upload_frame(&base, "sk-test-client-a").await;
    let response = client()
        .post(format!("{base}/v1/jobs"))
        .bearer_auth("sk-test-client-a")
        .json(&json!({
            "kind": "video_generation",
            "workflow": "comfy/img2vid",
            "inputs": {"prompt": "waves", "image": frame, "size": "64x64"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert!(fake.calls.lock().unwrap().is_empty());
}

// What `sb ai video --seconds 2 --aspect 16:9` sends (48 frames at 24 fps
// and a 16:9 size of about 0.4 MP, by `model` id) to a workflow whose
// bindings round the frame count up to 8k+1 and the size to multiples of
// 32: the job runs and the graph gets the values the model needs.
#[tokio::test]
async fn what_the_cli_sends_for_a_video_runs_with_rounding_bindings() {
    let fake = FakeComfy::default();
    let comfy_url = spawn_comfy(fake.clone()).await;
    let base = config(&comfy_url, 120);
    let fps_line = base
        .lines()
        .find(|line| line.contains(r#"fps: { path: ["14""#))
        .unwrap();
    let indent = &fps_line[..fps_line.len() - fps_line.trim_start().len()];
    let cfg = base
        .replace(
            fps_line,
            &format!(
                "{fps_line}\n{indent}width: {{ path: [\"13\", \"inputs\", \"width\"], multiple_of: 32 }}\n{indent}height: {{ path: [\"13\", \"inputs\", \"height\"], multiple_of: 32 }}"
            ),
        )
        .replace(
            r#"frames: { path: ["13", "inputs", "length"] }"#,
            r#"frames: { path: ["13", "inputs", "length"], round_up: { step: 8, offset: 1 } }"#,
        )
        .replace("inputs: { length: 1 }", "inputs: { length: 1, width: 1, height: 1 }");
    assert!(cfg.contains("round_up") && cfg.contains("multiple_of") && cfg.contains("width: 1"));
    let base = serve(&cfg).await;
    let frame = upload_frame(&base, "sk-test-client-a").await;
    let response = client()
        .post(format!("{base}/v1/jobs"))
        .bearer_auth("sk-test-client-a")
        .json(&json!({
            "kind": "video_generation",
            "model": "comfy/img2vid",
            "inputs": {"prompt": "waves", "seed": 3, "frames": 48, "size": "864x480", "image": frame}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 202);
    let id = response.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let done = wait_for(&base, &id, "succeeded", 20).await;
    let prompt = fake.prompt.lock().unwrap().clone().unwrap();
    let graph = &prompt["prompt"];
    assert_eq!(graph["13"]["inputs"]["length"], 49, "48 rounded up to 8k+1");
    assert_eq!(graph["13"]["inputs"]["width"], 864);
    assert_eq!(graph["13"]["inputs"]["height"], 480);
    let provenance = &done["artifacts"][0]["provenance"];
    assert_eq!(provenance["input.frames.sent"], "48", "{done}");
    assert_eq!(provenance["input.frames.used"], "49");
}
