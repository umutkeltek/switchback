//! The Anthropic Messages adapter. Mirrors `openai_compatible` structurally —
//! same streaming-first execute(), same cancel-on-disconnect loop, same
//! collect-to-events non-stream path — but speaks the Anthropic wire format:
//! `POST /v1/messages`, `x-api-key` + `anthropic-version` headers, and the
//! named-event SSE stream decoded by `sb_protocols::anthropic`.

use futures::StreamExt;
use sb_adapter::{response_to_events, AdapterError, EventStream, PreparedRequest, ProviderAdapter};
use sb_core::{AuthKind, CapabilityProfile, ErrorClass};

pub struct AnthropicAdapter {
    pub base_url: String,
    pub http: reqwest::Client,
    pub capabilities: CapabilityProfile,
}

impl AnthropicAdapter {
    pub fn new(
        base_url: String,
        capabilities: CapabilityProfile,
        timeouts: sb_core::Timeouts,
    ) -> Self {
        // Same timeout posture as openai_compatible: no total `.timeout()` (it
        // would cap long streamed generations); `connect_timeout` fails fast on
        // an unreachable upstream, `read_timeout` bounds idle time between bytes.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(timeouts.connect_ms))
            .read_timeout(std::time::Duration::from_millis(timeouts.read_ms))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base_url,
            http,
            capabilities,
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for AnthropicAdapter {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn capabilities(&self, _model: &str) -> CapabilityProfile {
        self.capabilities.clone()
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        let body = sb_protocols::anthropic::request_to_anthropic_wire(
            &prepared.request,
            &prepared.target.model,
            prepared.request.stream,
        );
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut request_builder = self
            .http
            .post(&url)
            .header(
                "anthropic-version",
                sb_protocols::anthropic::ANTHROPIC_VERSION,
            )
            .json(&body);

        if let Some(lease) = &prepared.lease {
            if lease.auth_kind != AuthKind::None && !lease.secret.is_empty() {
                // Anthropic authenticates with `x-api-key`, not bearer.
                request_builder = request_builder.header("x-api-key", lease.secret.expose());
            }
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| AdapterError::network(e.to_string()))?;
        let status = response.status();

        if !status.is_success() {
            // Panic-free: on a body-read error we classify on status alone.
            let body_text: String = response.text().await.unwrap_or_default();
            let class = self.classify_error(Some(status.as_u16()), &body_text);
            return Err(
                AdapterError::new(class, format!("upstream {} error", status.as_u16()))
                    .with_status(status.as_u16()),
            );
        }

        if prepared.request.stream {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let mut upstream = response.bytes_stream();

            tokio::spawn(async move {
                let mut buffer = String::new();
                let mut decoder = sb_protocols::anthropic::AnthropicStreamDecoder::new();

                loop {
                    // Cancel-on-disconnect: stop reading upstream the moment the
                    // client hangs up (receiver dropped) — no orphaned task.
                    let chunk_result = tokio::select! {
                        _ = tx.closed() => break,
                        chunk = upstream.next() => match chunk {
                            Some(chunk) => chunk,
                            None => break,
                        },
                    };
                    match chunk_result {
                        Err(_) => {
                            let _ = tx
                                .send(Err(AdapterError::network("stream byte error")))
                                .await;
                            break;
                        }
                        Ok(chunk) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));

                            // SSE frames are separated by a blank line. Anthropic
                            // carries the event name on the `event:` line and the
                            // payload on `data:`; we dispatch on the payload's own
                            // `type` field, so the `event:` line is ignored here.
                            while let Some(pos) = buffer.find("\n\n") {
                                let frame: String = buffer.drain(..pos + 2).collect();

                                for line in frame.lines() {
                                    let trimmed = line.trim();
                                    if let Some(data) = trimmed.strip_prefix("data:") {
                                        let data = data.trim();
                                        if data.is_empty() || data == "[DONE]" {
                                            continue;
                                        }

                                        if let Ok(value) =
                                            serde_json::from_str::<serde_json::Value>(data)
                                        {
                                            for event in decoder.decode(&value) {
                                                if tx.send(Ok(event)).await.is_err() {
                                                    return;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                for event in decoder.finish() {
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            });

            Ok(tokio_stream::wrappers::ReceiverStream::new(rx).boxed())
        } else {
            let full = response
                .bytes()
                .await
                .map_err(|e| AdapterError::network(e.to_string()))?;
            let value = serde_json::from_slice::<serde_json::Value>(&full)
                .map_err(|e| AdapterError::invalid(e.to_string()))?;
            let canonical = sb_protocols::anthropic::parse_anthropic_response(&value)
                .map_err(AdapterError::invalid)?;
            let events = response_to_events(&canonical);

            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    fn classify_error(&self, status: Option<u16>, _body: &str) -> ErrorClass {
        match status {
            Some(401) => ErrorClass::Authentication,
            Some(403) => ErrorClass::Authorization,
            // 429 rate limit; 529 is Anthropic's "overloaded" — both retryable
            // (529 falls into the 500..600 ServerError arm below).
            Some(429) => ErrorClass::RateLimited,
            Some(400) | Some(422) => ErrorClass::InvalidRequest,
            Some(408) | Some(504) => ErrorClass::Timeout,
            Some(value) if (500..600).contains(&value) => ErrorClass::ServerError,
            _ => ErrorClass::Unknown,
        }
    }
}
