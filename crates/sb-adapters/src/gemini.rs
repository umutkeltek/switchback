//! The Google Gemini adapter. Same streaming-first shape as the others, but
//! speaks GenerateContent: `POST /v1beta/models/{model}:generateContent` (or
//! `:streamGenerateContent?alt=sse`), `x-goog-api-key` auth, and the SSE stream
//! decoded by `sb_protocols::gemini`. The model name lives in the URL path.

use futures::StreamExt;
use sb_adapter::{response_to_events, AdapterError, EventStream, PreparedRequest, ProviderAdapter};
use sb_core::{AuthKind, CapabilityProfile, ErrorClass};

pub struct GeminiAdapter {
    pub base_url: String,
    pub http: reqwest::Client,
    pub capabilities: CapabilityProfile,
}

impl GeminiAdapter {
    pub fn new(
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
            base_url,
            http,
            capabilities,
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for GeminiAdapter {
    fn id(&self) -> &str {
        "gemini"
    }

    fn capabilities(&self, _model: &str) -> CapabilityProfile {
        self.capabilities.clone()
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        let body = sb_protocols::gemini::request_to_gemini_wire(&prepared.request);
        let model = prepared.target.model.clone();
        let base = self.base_url.trim_end_matches('/');
        let method = if prepared.request.stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        };
        let url = format!("{base}/v1beta/models/{model}:{method}");

        let mut request_builder = self.http.post(&url).json(&body);
        if let Some(lease) = &prepared.lease {
            if lease.auth_kind != AuthKind::None && !lease.secret.is_empty() {
                request_builder = request_builder.header("x-goog-api-key", lease.secret.expose());
            }
        }

        let response = request_builder
            .send()
            .await
            .map_err(|e| AdapterError::network(e.to_string()))?;
        let status = response.status();

        if !status.is_success() {
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
                let mut decoder = sb_protocols::gemini::GeminiStreamDecoder::new(model);

                loop {
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

                            while let Some(pos) = buffer.find("\n\n") {
                                let frame: String = buffer.drain(..pos + 2).collect();
                                for line in frame.lines() {
                                    let trimmed = line.trim();
                                    if let Some(data) = trimmed.strip_prefix("data:") {
                                        let data = data.trim();
                                        if data.is_empty() {
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
            let canonical =
                sb_protocols::gemini::parse_gemini_response(&value).map_err(AdapterError::invalid)?;
            let events = response_to_events(&canonical);

            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    fn classify_error(&self, status: Option<u16>, _body: &str) -> ErrorClass {
        match status {
            Some(401) => ErrorClass::Authentication,
            Some(403) => ErrorClass::Authorization,
            Some(429) => ErrorClass::RateLimited,
            Some(400) => ErrorClass::InvalidRequest,
            Some(408) | Some(504) => ErrorClass::Timeout,
            Some(value) if (500..600).contains(&value) => ErrorClass::ServerError,
            _ => ErrorClass::Unknown,
        }
    }
}
