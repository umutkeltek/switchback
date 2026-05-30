//! AWS Bedrock adapter (Claude on Bedrock).
//!
//! Bedrock is the one provider that earns a dedicated adapter rather than a
//! `ComposedAdapter`, because both halves differ from everything else: auth is
//! SigV4 request signing (the signature depends on the built request, not a
//! header), and streaming is the binary `application/vnd.amazon.eventstream`
//! framing, not SSE. The *wire* is otherwise Anthropic Messages, so we reuse
//! `sb-protocols::anthropic` for body/response/stream translation.

use base64::Engine as _;
use futures::StreamExt;
use sb_adapter::{response_to_events, AdapterError, EventStream, PreparedRequest, ProviderAdapter};
use sb_core::{CapabilityProfile, ErrorClass};
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;

use crate::event_stream::EventStreamDecoder;
use crate::sigv4::{self, AwsCredentials, CanonicalRequest};

pub struct BedrockAdapter {
    creds: AwsCredentials,
    region: String,
    base_url: String,
    capabilities: CapabilityProfile,
    http: reqwest::Client,
}

impl BedrockAdapter {
    pub fn new(
        creds: AwsCredentials,
        region: String,
        base_url: String,
        capabilities: CapabilityProfile,
        timeouts: sb_core::Timeouts,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(timeouts.connect_ms))
            .read_timeout(std::time::Duration::from_millis(timeouts.read_ms))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            creds,
            region,
            base_url,
            capabilities,
            http,
        }
    }

    fn classify(status: u16) -> ErrorClass {
        match status {
            401 => ErrorClass::Authentication,
            403 => ErrorClass::Authorization,
            429 => ErrorClass::RateLimited,
            400 | 422 => ErrorClass::InvalidRequest,
            408 => ErrorClass::Timeout,
            s if (500..600).contains(&s) => ErrorClass::ServerError,
            _ => ErrorClass::Unknown,
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for BedrockAdapter {
    fn id(&self) -> &str {
        "bedrock"
    }

    fn capabilities(&self, _model: &str) -> CapabilityProfile {
        self.capabilities.clone()
    }

    fn classify_error(&self, status: Option<u16>, _body: &str) -> ErrorClass {
        Self::classify(status.unwrap_or(0))
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        let stream = prepared.request.stream;
        let model = prepared.target.model.clone();

        // Body = the Anthropic Messages body, minus `model`/`stream` (model is in
        // the URL, streaming is chosen by the endpoint) plus `anthropic_version`.
        let mut body =
            sb_protocols::anthropic::request_to_anthropic_wire(&prepared.request, &model, false);
        if let Value::Object(map) = &mut body {
            map.remove("model");
            map.remove("stream");
            map.insert(
                "anthropic_version".to_string(),
                Value::String("bedrock-2023-05-31".to_string()),
            );
        }
        let body_bytes = serde_json::to_vec(&body).map_err(|e| AdapterError::invalid(e.to_string()))?;

        let action = if stream {
            "invoke-with-response-stream"
        } else {
            "invoke"
        };
        let host = self
            .base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(&self.base_url)
            .to_string();
        let path = format!("/model/{}/{}", percent_encode_segment(&model), action);
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);

        // SigV4-sign the built request (method + host + path + body).
        let date = amz_date();
        let signed = sigv4::sign(
            &CanonicalRequest {
                method: "POST",
                host: &host,
                path: &path,
                query: "",
                body: &body_bytes,
            },
            &self.creds,
            &self.region,
            "bedrock",
            &date,
        );
        let accept = if stream {
            "application/vnd.amazon.eventstream"
        } else {
            "application/json"
        };
        let mut builder = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", accept)
            .body(body_bytes);
        for h in &signed {
            builder = builder.header(h.name.as_str(), h.value.as_str());
        }

        let response = builder
            .send()
            .await
            .map_err(|e| AdapterError::network(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(AdapterError::new(
                Self::classify(status.as_u16()),
                format!("bedrock {} error: {}", status.as_u16(), truncate(&text, 200)),
            )
            .with_status(status.as_u16()));
        }

        if stream {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let mut upstream = response.bytes_stream();
            tokio::spawn(async move {
                let mut framer = EventStreamDecoder::new();
                let mut decoder = sb_protocols::anthropic::AnthropicStreamDecoder::new();
                loop {
                    let chunk = tokio::select! {
                        _ = tx.closed() => break,
                        c = upstream.next() => match c { Some(c) => c, None => break },
                    };
                    let bytes = match chunk {
                        Ok(b) => b,
                        Err(_) => {
                            let _ = tx.send(Err(AdapterError::network("stream byte error"))).await;
                            break;
                        }
                    };
                    framer.push(&bytes);
                    loop {
                        match framer.next_message() {
                            None => break,
                            Some(Err(e)) => {
                                let _ = tx.send(Err(AdapterError::invalid(e))).await;
                                break;
                            }
                            // Each chunk message wraps the model-native event as
                            // {"bytes": base64(<anthropic event json>)}.
                            Some(Ok(msg)) => {
                                if let Some(event) = decode_chunk(&msg.payload) {
                                    for ev in decoder.decode(&event) {
                                        if tx.send(Ok(ev)).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for ev in decoder.finish() {
                    if tx.send(Ok(ev)).await.is_err() {
                        return;
                    }
                }
            });
            Ok(ReceiverStream::new(rx).boxed())
        } else {
            let full = response
                .bytes()
                .await
                .map_err(|e| AdapterError::network(e.to_string()))?;
            let value: Value = serde_json::from_slice(&full)
                .map_err(|e| AdapterError::invalid(e.to_string()))?;
            let canonical = sb_protocols::anthropic::parse_anthropic_response(&value)
                .map_err(AdapterError::invalid)?;
            let events = response_to_events(&canonical);
            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    async fn embeddings(
        &self,
        _body: Value,
        _target: sb_core::ExecutionTarget,
        _lease: Option<sb_core::CredentialLease>,
    ) -> Result<Value, AdapterError> {
        Err(AdapterError::new(
            ErrorClass::UnsupportedCapability,
            "embeddings not supported on the bedrock adapter",
        ))
    }
}

/// Extract + decode a Bedrock stream chunk's wrapped model event.
fn decode_chunk(payload: &[u8]) -> Option<Value> {
    let wrapper: Value = serde_json::from_slice(payload).ok()?;
    let b64 = wrapper.get("bytes").and_then(|v| v.as_str())?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Percent-encode a URL path segment (Bedrock model ids contain `:` etc.). The
/// same encoding is used for the request URL and the SigV4 canonical path.
fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// UTC timestamp as `YYYYMMDDTHHMMSSZ` for `x-amz-date`.
fn amz_date() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
