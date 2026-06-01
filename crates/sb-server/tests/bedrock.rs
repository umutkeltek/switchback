//! End-to-end Bedrock adapter: an OpenAI-shaped client hits switchback, which
//! SigV4-signs the request to a fake Bedrock upstream, sends the Anthropic body,
//! and translates the response back — both the non-streaming InvokeModel JSON
//! and the binary `invoke-with-response-stream` event-stream framing.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Value};

/// Build one AWS event-stream message wrapping an Anthropic event as Bedrock
/// does: payload = {"bytes": base64(<event json>)}, header :event-type=chunk.
fn bedrock_frame(event_json: &str) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD.encode(event_json.as_bytes());
    let payload = json!({ "bytes": b64 }).to_string();
    let payload = payload.as_bytes();

    let mut headers = Vec::new();
    let name = ":event-type";
    headers.push(name.len() as u8);
    headers.extend_from_slice(name.as_bytes());
    headers.push(7u8); // string type
    headers.extend_from_slice(&("chunk".len() as u16).to_be_bytes());
    headers.extend_from_slice(b"chunk");

    let total = 12 + headers.len() + payload.len() + 4;
    let mut msg = Vec::new();
    msg.extend_from_slice(&(total as u32).to_be_bytes());
    msg.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    msg.extend_from_slice(&0u32.to_be_bytes()); // prelude crc (unverified)
    msg.extend_from_slice(&headers);
    msg.extend_from_slice(payload);
    msg.extend_from_slice(&0u32.to_be_bytes()); // message crc (unverified)
    msg
}

#[derive(Clone, Default)]
struct Seen {
    auth: Arc<Mutex<Option<String>>>,
    amz_date: Arc<Mutex<Option<String>>>,
    amz_security_token: Arc<Mutex<Option<String>>>,
}

async fn fake_bedrock(
    State(seen): State<Seen>,
    uri: Uri,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let get = |h: &str| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    };
    *seen.auth.lock().unwrap() = get("authorization");
    *seen.amz_date.lock().unwrap() = get("x-amz-date");
    *seen.amz_security_token.lock().unwrap() = get("x-amz-security-token");

    if uri.path().ends_with("invoke-with-response-stream") {
        let mut buf = Vec::new();
        buf.extend(bedrock_frame(
            r#"{"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}"#,
        ));
        buf.extend(bedrock_frame(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        ));
        buf.extend(bedrock_frame(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi "}}"#,
        ));
        buf.extend(bedrock_frame(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"bedrock"}}"#,
        ));
        buf.extend(bedrock_frame(r#"{"type":"content_block_stop","index":0}"#));
        buf.extend(bedrock_frame(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        ));
        buf.extend(bedrock_frame(r#"{"type":"message_stop"}"#));
        ([(CONTENT_TYPE, "application/vnd.amazon.eventstream")], buf).into_response()
    } else {
        // InvokeModel non-stream → the model-native Anthropic response.
        Json(json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "content": [{ "type": "text", "text": "hi from bedrock" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 5, "output_tokens": 3 }
        }))
        .into_response()
    }
}

async fn spawn_fake_bedrock() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .fallback(post(fake_bedrock))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), seen)
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

fn cfg(base: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: bedrock
    type: bedrock
    region: us-east-1
    base_url: "{base}"
    accounts:
      - id: default
        auth:
          kind: aws_sig_v4
          access_key_env: SB_BEDROCK_TEST_UNSET_ACCESS_KEY
          access_key: "AKIDEXAMPLE"
          secret_key_env: SB_BEDROCK_TEST_UNSET_SECRET_KEY
          secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "bedrock/anthropic.claude-3-5-sonnet-20240620-v1:0"
"#
    )
}

fn cfg_with_account_creds(base: &str) -> String {
    format!(
        r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: bedrock
    type: bedrock
    region: us-east-1
    base_url: "{base}"
    accounts:
      - id: selected-aws-account
        auth:
          kind: aws_sig_v4
          access_key_env: SB_BEDROCK_TEST_UNSET_ACCESS_KEY
          access_key: "AKIDACCOUNT"
          secret_key_env: SB_BEDROCK_TEST_UNSET_SECRET_KEY
          secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"
          session_token: "session-token"
routes:
  - name: default
    match: {{ model: "*" }}
    targets:
      - "bedrock/anthropic.claude-3-5-sonnet-20240620-v1:0"
"#
    )
}

#[tokio::test]
async fn bedrock_non_stream_signs_and_translates() {
    let (base, seen) = spawn_fake_bedrock().await;
    let switchback = spawn_switchback(&cfg(&base)).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"x","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["choices"][0]["message"]["content"], "hi from bedrock");

    // The upstream received a well-formed SigV4 signature + date.
    let auth = seen.auth.lock().unwrap().clone().unwrap_or_default();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
            && auth.contains("/us-east-1/bedrock/aws4_request")
            && auth.contains("Signature="),
        "expected a SigV4 Authorization header, got: {auth}"
    );
    assert!(
        seen.amz_date.lock().unwrap().is_some(),
        "x-amz-date present"
    );
}

#[tokio::test]
async fn bedrock_signs_with_selected_account_lease_credentials() {
    let (base, seen) = spawn_fake_bedrock().await;
    let switchback = spawn_switchback(&cfg_with_account_creds(&base)).await;

    let resp: Value = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"x","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["choices"][0]["message"]["content"], "hi from bedrock");

    let auth = seen.auth.lock().unwrap().clone().unwrap_or_default();
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDACCOUNT/")
            && auth.contains("/us-east-1/bedrock/aws4_request"),
        "expected selected account SigV4 key, got: {auth}"
    );
    assert_eq!(
        seen.amz_security_token.lock().unwrap().as_deref(),
        Some("session-token")
    );
}

#[tokio::test]
async fn bedrock_stream_decodes_event_stream_framing() {
    let (base, _seen) = spawn_fake_bedrock().await;
    let switchback = spawn_switchback(&cfg(&base)).await;

    let text = reqwest::Client::new()
        .post(format!("{switchback}/v1/chat/completions"))
        .json(&json!({"model":"x","stream":true,"messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // The event-stream frames decoded into OpenAI SSE deltas reassembling the text.
    assert!(
        text.contains("\"content\":\"hi \""),
        "missing first delta: {text}"
    );
    assert!(
        text.contains("\"content\":\"bedrock\""),
        "missing second delta: {text}"
    );
    assert!(text.contains("[DONE]"), "stream not terminated: {text}");
}
