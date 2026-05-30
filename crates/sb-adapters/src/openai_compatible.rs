use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream, PreparedRequest, ProviderAdapter};
use sb_core::{
    AiResponse, AiStreamEvent, AuthScheme, CapabilityProfile, ContentPart, ErrorClass,
    ToolCallStart,
};

pub struct OpenAiCompatibleAdapter {
    pub base_url: String,
    pub http: reqwest::Client,
    pub capabilities: CapabilityProfile,
    pub auth: AuthScheme,
}

impl OpenAiCompatibleAdapter {
    pub fn new(
        base_url: String,
        capabilities: CapabilityProfile,
        timeouts: sb_core::Timeouts,
        auth: AuthScheme,
    ) -> Self {
        // NOTE: no total `.timeout()` — that would cap long streamed generations.
        // `connect_timeout` fails fast on an unreachable upstream; `read_timeout`
        // bounds idle time between bytes so a hung stream is detected without
        // limiting a healthy long one.
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(timeouts.connect_ms))
            .read_timeout(std::time::Duration::from_millis(timeouts.read_ms))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            base_url,
            http,
            capabilities,
            auth,
        }
    }
}

fn response_to_events(resp: &AiResponse) -> Vec<AiStreamEvent> {
    let mut events = vec![AiStreamEvent::MessageStart {
        id: resp.id.clone(),
        model: resp.model.clone(),
    }];

    let text = resp.message.text();
    if !text.is_empty() {
        events.push(AiStreamEvent::TextDelta { text });
    }

    let mut tool_index = 0u32;
    for part in &resp.message.content {
        if let ContentPart::ToolUse { id, name, args } = part {
            events.push(AiStreamEvent::ToolCallStart(ToolCallStart {
                index: tool_index,
                id: id.clone(),
                name: name.clone(),
            }));
            events.push(AiStreamEvent::ToolCallArgsDelta {
                index: tool_index,
                json: serde_json::to_string(args).unwrap_or_default(),
            });
            events.push(AiStreamEvent::ToolCallEnd { index: tool_index });
            tool_index += 1;
        }
    }

    events.push(AiStreamEvent::UsageDelta {
        usage: resp.usage.clone(),
    });
    events.push(AiStreamEvent::MessageEnd {
        finish_reason: resp.finish_reason,
    });
    events
}

#[async_trait::async_trait]
impl ProviderAdapter for OpenAiCompatibleAdapter {
    fn id(&self) -> &str {
        "openai_compatible"
    }

    fn capabilities(&self, _model: &str) -> CapabilityProfile {
        self.capabilities.clone()
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        let body = sb_protocols::openai::request_to_openai_wire(
            &prepared.request,
            &prepared.target.model,
            prepared.request.stream,
        );
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let request_builder = self.http.post(&url).json(&body);
        let request_builder = crate::apply_auth(request_builder, &self.auth, prepared.lease.as_ref());

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
                let mut decoder = sb_protocols::openai::OpenAiStreamDecoder::new();

                loop {
                    // Cancel-on-disconnect: the moment the client hangs up (the
                    // receiver is dropped) stop reading the upstream — no orphaned
                    // task burning tokens / holding the connection open.
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
                                        if data == "[DONE]" {
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
            let canonical = sb_protocols::openai::parse_openai_chat_response(&value)
                .map_err(AdapterError::invalid)?;
            let events = response_to_events(&canonical);

            Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
        }
    }

    async fn embeddings(
        &self,
        body: serde_json::Value,
        _target: sb_core::ExecutionTarget,
        lease: Option<sb_core::CredentialLease>,
    ) -> Result<serde_json::Value, AdapterError> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let request_builder = self.http.post(&url).json(&body);
        let request_builder = crate::apply_auth(request_builder, &self.auth, lease.as_ref());

        let response = request_builder
            .send()
            .await
            .map_err(|e| AdapterError::network(e.to_string()))?;
        let status = response.status();

        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let class = self.classify_error(Some(status.as_u16()), &body_text);
            return Err(
                AdapterError::new(class, format!("upstream {} error", status.as_u16()))
                    .with_status(status.as_u16()),
            );
        }

        let value = response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| AdapterError::invalid(e.to_string()))?;
        Ok(value)
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
