//! `/v1/audio/*` end to end against fake engines on loopback: the
//! multipart body reaches the engine intact (with `model` set to the
//! target's own id and unknown fields kept), the engine's content type comes
//! back, uploads past the limit are refused, targets without the audio
//! capability are skipped with a reason, failures fall over, a full lane
//! spills, usage is recorded per request, audio bytes are stored nowhere,
//! and nothing is cached.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};

/// A byte pattern no text field contains: every byte value, so a file that
/// went through a text conversion anywhere would not match.
fn audio_bytes() -> Vec<u8> {
    let mut bytes = b"RIFF-AUDIO-MARKER-7f3a".to_vec();
    bytes.extend((0..=255u8).cycle().take(4096));
    bytes
}

#[derive(Clone, Default)]
struct Engine {
    calls: Arc<AtomicUsize>,
    /// (path, content type, authorization, raw body) of every call.
    seen: Arc<Mutex<Vec<(String, String, String, Vec<u8>)>>>,
    /// Answer every call with this status instead (a failing engine).
    fail_with: Option<u16>,
    /// Hold every answer until this is notified (a busy engine).
    gate: Option<Arc<tokio::sync::Notify>>,
}

impl Engine {
    fn failing(status: u16) -> Self {
        Engine {
            fail_with: Some(status),
            ..Engine::default()
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last(&self) -> (String, String, String, Vec<u8>) {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("engine was called")
    }
}

/// The `name="..."` field's value in a raw multipart body (test helper).
fn form_field(body: &[u8], name: &str) -> Option<String> {
    let marker = format!("name=\"{name}\"\r\n\r\n");
    let text = String::from_utf8_lossy(body);
    let start = text.find(&marker)? + marker.len();
    let end = text[start..].find("\r\n--")? + start;
    Some(text[start..end].to_string())
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

async fn engine_call(
    State(engine): State<Engine>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    engine.calls.fetch_add(1, Ordering::SeqCst);
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    engine.seen.lock().unwrap().push((
        uri.path().to_string(),
        header("content-type"),
        header("authorization"),
        body.to_vec(),
    ));
    if let Some(gate) = &engine.gate {
        gate.notified().await;
    }
    if let Some(status) = engine.fail_with {
        return (StatusCode::from_u16(status).unwrap(), "engine failed").into_response();
    }
    if uri.path() == "/audio/speech" {
        let body: Value = serde_json::from_slice(&body).unwrap();
        let mut audio = b"ID3-SPEECH:".to_vec();
        audio.extend(body["input"].as_str().unwrap_or("").as_bytes());
        return ([("content-type", "audio/mpeg")], audio).into_response();
    }
    match form_field(&body, "response_format").as_deref() {
        Some("srt") => (
            [("content-type", "text/plain; charset=utf-8")],
            "1\n00:00:00,000 --> 00:00:01,000\nhello\n",
        )
            .into_response(),
        Some("text") => ([("content-type", "text/plain")], "hello world").into_response(),
        Some("diarized_json") => axum::Json(json!({
            "task": "transcribe", "duration": 42.5, "text": "hi there",
            "segments": [
                {"speaker": "SPEAKER_00", "start": 0.0, "end": 1.0, "text": "hi"},
                {"speaker": "SPEAKER_01", "start": 63.2, "end": 64.0, "text": "there"}
            ]
        }))
        .into_response(),
        _ => {
            axum::Json(json!({"text": "hello world", "usage": {"type": "duration", "seconds": 7}}))
                .into_response()
        }
    }
}

async fn spawn_engine(engine: Engine) -> String {
    let app = Router::new()
        .route("/audio/transcriptions", post(engine_call))
        .route("/audio/translations", post(engine_call))
        .route("/audio/speech", post(engine_call))
        .route("/health", get(|| async { StatusCode::OK }))
        .with_state(engine);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

struct Gateway {
    base: String,
    ledger: Arc<sb_ledger::UsageLedger>,
}

async fn spawn_gateway(cfg_yaml: &str, ledger: sb_ledger::UsageLedger) -> Gateway {
    let cfg = sb_core::Config::from_yaml(cfg_yaml).unwrap();
    let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
    let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
    let ledger = Arc::new(ledger);
    let state = sb_server::AppState::new(
        Arc::new(cfg),
        Arc::new(registry),
        Arc::new(resolver),
        ledger.clone(),
    );
    let app = sb_server::build_app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Gateway {
        base: format!("http://{addr}"),
        ledger,
    }
}

/// A gateway with a `transcribe` route over `targets`, a `speak` route over
/// `speak_targets`, providers `stt1`/`stt2` (audio in), `tts` (audio out)
/// and `text` (neither), each at the given engine URL.
fn config(urls: &Urls, transcribe: &[&str], speak: &[&str], extra_server: &str) -> String {
    let list = |targets: &[&str]| {
        targets
            .iter()
            .map(|t| format!("      - \"{t}\"\n"))
            .collect::<String>()
    };
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
{extra_server}
providers:
  - id: stt1
    type: openai_compatible
    base_url: "{stt1}"
    capabilities: {{ audio_in: true }}
    accounts:
      - {{ id: a, auth: {{ kind: api_key, inline: "engine-one-key" }} }}
  - id: stt2
    type: openai_compatible
    base_url: "{stt2}"
    capabilities: {{ audio_in: true }}
    accounts:
      - {{ id: a, auth: {{ kind: none }} }}
  - id: tts
    type: openai_compatible
    base_url: "{tts}"
    capabilities: {{ audio_out: true }}
    accounts:
      - {{ id: a, auth: {{ kind: none }} }}
  - id: text
    type: openai_compatible
    base_url: "{text}"
    accounts:
      - {{ id: a, auth: {{ kind: none }} }}
routes:
  - name: transcribe
    match: {{ model: "transcribe" }}
    pin_order: true
    targets:
{transcribe_targets}  - name: speak
    match: {{ model: "speak" }}
    targets:
{speak_targets}"#,
        stt1 = urls.stt1,
        stt2 = urls.stt2,
        tts = urls.tts,
        text = urls.text,
        transcribe_targets = list(transcribe),
        speak_targets = list(speak),
    )
}

struct Urls {
    stt1: String,
    stt2: String,
    tts: String,
    text: String,
}

struct Engines {
    stt1: Engine,
    stt2: Engine,
    tts: Engine,
    text: Engine,
    urls: Urls,
}

async fn engines_with(stt1: Engine, stt2: Engine) -> Engines {
    let (tts, text) = (Engine::default(), Engine::default());
    let urls = Urls {
        stt1: spawn_engine(stt1.clone()).await,
        stt2: spawn_engine(stt2.clone()).await,
        tts: spawn_engine(tts.clone()).await,
        text: spawn_engine(text.clone()).await,
    };
    Engines {
        stt1,
        stt2,
        tts,
        text,
        urls,
    }
}

async fn engines() -> Engines {
    engines_with(Engine::default(), Engine::default()).await
}

const BOUNDARY: &str = "client-boundary-1234";

/// A multipart body as an OpenAI client sends it.
fn multipart(fields: &[(&str, &str)], file: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"talk.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend(file);
    body.extend(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn upload(base: &str, path: &str, body: Vec<u8>) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn transcribe(base: &str, fields: &[(&str, &str)]) -> reqwest::Response {
    upload(
        base,
        "/v1/audio/transcriptions",
        multipart(fields, &audio_bytes()),
    )
    .await
}

async fn trace(base: &str, request_id: &str) -> Value {
    reqwest::Client::new()
        .get(format!("{base}/v1/traces/{request_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn request_id(response: &reqwest::Response) -> String {
    response.headers()["x-switchback-request-id"]
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn transcription_reaches_the_engine_intact_and_keeps_its_content_type() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(&e.urls, &["stt1/whisper-large"], &["tts/voice"], ""),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;

    let response = transcribe(
        &gw.base,
        &[
            ("model", "transcribe"),
            ("response_format", "srt"),
            ("language", "de"),
            ("timestamp_granularities[]", "segment"),
            ("num_speakers", "2"),
            ("max_speakers", "3"),
        ],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/plain; charset=utf-8"
    );
    assert!(response.headers().contains_key("x-switchback-revision"));
    assert_eq!(
        response.text().await.unwrap(),
        "1\n00:00:00,000 --> 00:00:01,000\nhello\n"
    );

    let (path, content_type, auth, body) = e.stt1.last();
    assert_eq!(path, "/audio/transcriptions");
    assert!(content_type.starts_with("multipart/form-data; boundary="));
    assert_eq!(auth, "Bearer engine-one-key", "the provider's own auth");
    assert_eq!(form_field(&body, "model").as_deref(), Some("whisper-large"));
    assert_eq!(form_field(&body, "response_format").as_deref(), Some("srt"));
    assert_eq!(form_field(&body, "language").as_deref(), Some("de"));
    assert_eq!(
        form_field(&body, "timestamp_granularities[]").as_deref(),
        Some("segment")
    );
    assert_eq!(form_field(&body, "num_speakers").as_deref(), Some("2"));
    assert_eq!(form_field(&body, "max_speakers").as_deref(), Some("3"));
    assert!(contains(&body, b"filename=\"talk.wav\""));
    assert!(contains(&body, b"Content-Type: audio/wav\r\n\r\n"));
    let mut file = b"\r\n\r\n".to_vec();
    file.extend(audio_bytes());
    file.extend(b"\r\n--");
    assert!(contains(&body, &file), "file bytes arrive unchanged");
    assert_eq!(e.stt2.calls() + e.tts.calls() + e.text.calls(), 0);
}

#[tokio::test]
async fn translation_goes_to_the_translations_endpoint_and_json_stays_json() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(&e.urls, &["stt1/whisper-large"], &["tts/voice"], ""),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let response = upload(
        &gw.base,
        "/v1/audio/translations",
        multipart(&[("model", "transcribe")], &audio_bytes()),
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["text"], "hello world");
    assert_eq!(e.stt1.last().0, "/audio/translations");

    let records = gw.ledger.snapshot();
    assert_eq!(records.len(), 1);
    let record = serde_json::to_value(&records[0]).unwrap();
    assert_eq!(record["workload_kind"], "audio_translation");
    assert_eq!(record["pricing_unit"], "per_second");
    assert_eq!(record["units_consumed"], 7.0, "usage type duration");
}

#[tokio::test]
async fn an_upload_over_the_limit_is_refused_with_413_before_any_engine() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(
            &e.urls,
            &["stt1/whisper-large"],
            &["tts/voice"],
            "  audio_max_upload_bytes: 2048",
        ),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let response = transcribe(&gw.base, &[("model", "transcribe")]).await;
    assert_eq!(response.status(), 413);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["error"]["type"], "audio_too_large");
    assert!(value["error"]["message"]
        .as_str()
        .unwrap()
        .contains("2048 bytes"));

    // Without a Content-Length (a chunked upload), the limit still holds.
    let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
        multipart(&[("model", "transcribe")], &audio_bytes())
            .chunks(512)
            .map(|chunk| Ok(chunk.to_vec()))
            .collect();
    let response = reqwest::Client::new()
        .post(format!("{}/v1/audio/transcriptions", gw.base))
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(reqwest::Body::wrap_stream(futures::stream::iter(chunks)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 413);
    assert_eq!(e.stt1.calls(), 0);
}

#[tokio::test]
async fn a_target_without_audio_in_is_skipped_with_the_reason() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(
            &e.urls,
            &["text/qwen3", "stt1/whisper-large"],
            &["tts/voice"],
            "",
        ),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let response = transcribe(&gw.base, &[("model", "transcribe")]).await;
    assert_eq!(response.status(), 200);
    let id = request_id(&response);
    assert_eq!(e.text.calls(), 0);
    assert_eq!(e.stt1.calls(), 1);
    let trace = trace(&gw.base, &id).await;
    let rejected = trace["decision"]["rejected"].as_array().unwrap();
    assert!(
        rejected.iter().any(|r| r["target_id"] == "text/qwen3"
            && r["reason"] == "audio input required but target does not support it"),
        "{trace}"
    );

    // Only text targets: refused with the reason, nothing sent anywhere.
    let gw = spawn_gateway(
        &config(&e.urls, &["text/qwen3"], &["tts/voice"], ""),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let response = transcribe(&gw.base, &[("model", "transcribe")]).await;
    assert_eq!(response.status(), 400);
    let value: Value = response.json().await.unwrap();
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("audio input required but target does not support it"),
        "{value}"
    );
    assert_eq!(e.text.calls(), 0);
}

#[tokio::test]
async fn a_failing_engine_falls_over_to_the_next_target() {
    let e = engines_with(Engine::failing(503), Engine::default()).await;
    let gw = spawn_gateway(
        &config(
            &e.urls,
            &["stt1/whisper-large", "stt2/whisper-small"],
            &["tts/voice"],
            "",
        ),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let response = transcribe(
        &gw.base,
        &[("model", "transcribe"), ("response_format", "text")],
    )
    .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "hello world");
    assert_eq!(e.stt1.calls(), 1);
    assert_eq!(e.stt2.calls(), 1);
    assert_eq!(
        form_field(&e.stt2.last().3, "model").as_deref(),
        Some("whisper-small")
    );
    let records = gw.ledger.snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].provider_id, "stt2");
}

#[tokio::test]
async fn a_full_lane_spills_to_the_second_target() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let busy = Engine {
        gate: Some(gate.clone()),
        ..Engine::default()
    };
    let e = engines_with(busy, Engine::default()).await;
    let lane = format!(
        r#"
local_executors:
  - name: stt1
    base_url: "{url}"
    health_endpoint: "{url}/health"
    max_in_flight: 1
    on_full: spill
"#,
        url = e.urls.stt1
    );
    let cfg = config(
        &e.urls,
        &["stt1/whisper-large", "stt2/whisper-small"],
        &["tts/voice"],
        "",
    ) + &lane;
    let gw = spawn_gateway(&cfg, sb_ledger::UsageLedger::in_memory()).await;

    // The first request takes the lane's one place and waits at the engine.
    let base = gw.base.clone();
    let first = tokio::spawn(async move { transcribe(&base, &[("model", "transcribe")]).await });
    for _ in 0..200 {
        if e.stt1.calls() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(e.stt1.calls(), 1, "the first request reached the lane");

    let second = transcribe(&gw.base, &[("model", "transcribe")]).await;
    assert_eq!(second.status(), 200);
    let id = request_id(&second);
    assert_eq!(e.stt2.calls(), 1, "spilled to the second target");
    assert_eq!(e.stt1.calls(), 1, "the full lane got nothing more");
    let trace = trace(&gw.base, &id).await;
    assert!(
        trace["decision"]["rejected"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["reason"].as_str().unwrap_or("").contains("spilled")),
        "{trace}"
    );

    gate.notify_one();
    assert_eq!(first.await.unwrap().status(), 200);
}

#[tokio::test]
async fn usage_records_kind_target_latency_and_audio_seconds() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(&e.urls, &["stt1/whisper-large"], &["tts/voice"], ""),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let response = transcribe(
        &gw.base,
        &[
            ("model", "transcribe"),
            ("response_format", "diarized_json"),
        ],
    )
    .await;
    assert_eq!(response.status(), 200);
    let id = request_id(&response);
    let value: Value = response.json().await.unwrap();
    assert_eq!(value["segments"][1]["speaker"], "SPEAKER_01");

    let records = gw.ledger.snapshot();
    assert_eq!(records.len(), 1, "one usage record per request");
    let record = serde_json::to_value(&records[0]).unwrap();
    assert_eq!(record["request_id"], id);
    assert_eq!(record["workload_kind"], "audio_transcription");
    assert_eq!(record["provider_id"], "stt1");
    assert_eq!(record["model"], "whisper-large");
    assert_eq!(record["requested_model"], "transcribe");
    assert_eq!(record["pricing_unit"], "per_second");
    assert_eq!(record["units_consumed"], 42.5);
    assert_eq!(record["streamed"], false);
    assert!(record["latency_ms"].is_u64());
    let trace = trace(&gw.base, &id).await;
    assert_eq!(trace["final_status"], 200, "{trace}");
}

#[tokio::test]
async fn audio_bytes_are_stored_nowhere() {
    let e = engines().await;
    let dir = std::env::temp_dir().join(sb_core::new_id("audio-test"));
    std::fs::create_dir_all(&dir).unwrap();
    let usage_log = dir.join("usage.jsonl");
    let gw = spawn_gateway(
        &config(&e.urls, &["stt1/whisper-large"], &["tts/voice"], ""),
        sb_ledger::UsageLedger::with_sink(&usage_log),
    )
    .await;
    let response = transcribe(&gw.base, &[("model", "transcribe")]).await;
    assert_eq!(response.status(), 200);
    let id = request_id(&response);
    let speech = reqwest::Client::new()
        .post(format!("{}/v1/audio/speech", gw.base))
        .json(&json!({"model": "speak", "input": "say this", "voice": "a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(speech.status(), 200);
    let speech_id = request_id(&speech);
    assert!(contains(&speech.bytes().await.unwrap(), b"ID3-SPEECH:"));

    let mut stored = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        stored.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    for path in [
        format!("/v1/traces/{id}"),
        format!("/v1/traces/{speech_id}"),
        "/v1/traces".to_string(),
        "/v1/usage".to_string(),
        "/v1/usage/events".to_string(),
    ] {
        let body = reqwest::get(format!("{}{path}", gw.base))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        stored.extend(body);
    }
    stored.extend(serde_json::to_vec(&gw.ledger.snapshot()).unwrap());
    assert!(contains(&stored, id.as_bytes()), "the records are there");
    assert!(!contains(&stored, b"RIFF-AUDIO-MARKER"), "no audio bytes");
    assert!(!contains(&stored, b"ID3-SPEECH"), "no speech audio");
    assert!(!contains(&stored, b"say this"), "no speech text");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn two_identical_requests_both_reach_the_engine() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(&e.urls, &["stt1/whisper-large"], &["tts/voice"], ""),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    for _ in 0..2 {
        let response = transcribe(&gw.base, &[("model", "transcribe")]).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    assert_eq!(e.stt1.calls(), 2, "audio is never answered from a cache");
    assert_eq!(gw.ledger.snapshot().len(), 2);
}

#[tokio::test]
async fn speech_streams_audio_from_an_audio_out_target_only() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(
            &e.urls,
            &["stt1/whisper-large"],
            &["stt1/whisper-large", "tts/kokoro"],
            "",
        ),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let response = reqwest::Client::new()
        .post(format!("{}/v1/audio/speech", gw.base))
        .json(&json!({
            "model": "speak", "input": "Grüße", "voice": "af_heart",
            "response_format": "mp3", "speed": 1.2, "lang_code": "de"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "audio/mpeg");
    let id = request_id(&response);
    let audio = response.bytes().await.unwrap();
    assert_eq!(audio.as_ref(), "ID3-SPEECH:Grüße".as_bytes());

    assert_eq!(e.stt1.calls(), 0, "a speech-to-text target never speaks");
    let (path, content_type, _, body) = e.tts.last();
    assert_eq!(path, "/audio/speech");
    assert_eq!(content_type, "application/json");
    let sent: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(sent["model"], "kokoro");
    assert_eq!(sent["voice"], "af_heart");
    assert_eq!(sent["speed"], 1.2);
    assert_eq!(sent["lang_code"], "de", "extra fields pass through");

    let trace = trace(&gw.base, &id).await;
    assert!(trace["decision"]["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["target_id"] == "stt1/whisper-large"
            && r["reason"] == "audio output required but target does not support it"));
    let record = serde_json::to_value(&gw.ledger.snapshot()[0]).unwrap();
    assert_eq!(record["workload_kind"], "audio_speech");
    assert_eq!(record["pricing_unit"], "per_character");
    assert_eq!(record["units_consumed"], 5.0, "characters, not bytes");
    assert_eq!(record["streamed"], true);
}

#[tokio::test]
async fn bad_audio_requests_are_refused_before_routing() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(&e.urls, &["stt1/whisper-large"], &["tts/voice"], ""),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    // No file.
    let mut body = Vec::new();
    body.extend(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ntranscribe\r\n--{BOUNDARY}--\r\n"
        )
        .as_bytes(),
    );
    let response = upload(&gw.base, "/v1/audio/transcriptions", body).await;
    assert_eq!(response.status(), 400);
    // No model.
    let response = upload(
        &gw.base,
        "/v1/audio/transcriptions",
        multipart(&[], &audio_bytes()),
    )
    .await;
    assert_eq!(response.status(), 400);
    // Not multipart.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/audio/transcriptions", gw.base))
        .json(&json!({"model": "transcribe"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    // Speech without input.
    let response = reqwest::Client::new()
        .post(format!("{}/v1/audio/speech", gw.base))
        .json(&json!({"model": "speak"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(e.stt1.calls() + e.tts.calls(), 0);
}

#[tokio::test]
async fn models_lists_audio_models_with_their_kind() {
    let e = engines().await;
    let gw = spawn_gateway(
        &config(&e.urls, &["stt1/whisper-large"], &["tts/kokoro"], ""),
        sb_ledger::UsageLedger::in_memory(),
    )
    .await;
    let models: Value = reqwest::get(format!("{}/v1/models", gw.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kind = |id: &str| {
        models["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed: {models}"))
            .get("kind")
            .cloned()
    };
    assert_eq!(kind("transcribe"), Some(json!("audio_transcription")));
    assert_eq!(
        kind("stt1/whisper-large"),
        Some(json!("audio_transcription"))
    );
    assert_eq!(kind("speak"), Some(json!("audio_speech")));
    assert_eq!(kind("tts"), Some(json!("audio_speech")));
    assert_eq!(kind("text"), None, "text models carry no kind");
}
