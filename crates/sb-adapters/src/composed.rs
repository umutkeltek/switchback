//! The generic adapter: `WireCodec × AuthScheme`. One execute loop serves every
//! wire format — the three hand-written adapters collapse into this plus three
//! thin [`crate::codec`] impls. Adding a provider that reuses a wire format is
//! now data (a codec + an auth scheme), not a new adapter.

use futures::StreamExt;
use sb_adapter::{
    response_to_events, AdapterError, EventStream, PreparedRequest, ProviderAdapter,
};
use sb_core::{AuthScheme, CapabilityProfile, ErrorClass};

use crate::apply_auth;
use crate::codec::WireCodec;

pub struct ComposedAdapter {
    codec: Box<dyn WireCodec>,
    auth: AuthScheme,
    base_url: String,
    capabilities: CapabilityProfile,
    /// Shared pool of per-egress HTTP clients. The attempt's `egress_id` selects
    /// which outbound path (direct / a configured proxy) the call exits from.
    egress: std::sync::Arc<crate::egress::EgressPool>,
}

impl ComposedAdapter {
    pub fn new(
        codec: Box<dyn WireCodec>,
        auth: AuthScheme,
        base_url: String,
        capabilities: CapabilityProfile,
        egress: std::sync::Arc<crate::egress::EgressPool>,
    ) -> Self {
        Self {
            codec,
            auth,
            base_url,
            capabilities,
            egress,
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for ComposedAdapter {
    fn id(&self) -> &str {
        self.codec.id()
    }

    fn capabilities(&self, _model: &str) -> CapabilityProfile {
        self.capabilities.clone()
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        let stream = prepared.request.stream;
        let model = prepared.target.model.clone();
        let body = self.codec.request_body(&prepared.request, &model, stream);
        let url = self.codec.url(&self.base_url, &model, stream);

        // Select the outbound path for this attempt (direct unless the account/
        // provider named an egress and it's enabled). The path carries both the
        // proxy client and an optional client identity (custom UA + headers).
        let path = self.egress.path(prepared.egress_id.as_deref());
        let mut builder = path.client().post(&url).json(&body);
        for (name, value) in self.codec.headers() {
            builder = builder.header(name, value);
        }
        builder = apply_auth(builder, &self.auth, prepared.lease.as_ref());
        builder = path.apply_identity(builder); // legitimate UA/header identity

        let response = builder
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

        if stream {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let mut upstream = response.bytes_stream();
            let mut decoder = self.codec.decoder(&model);

            tokio::spawn(async move {
                let mut buffer = String::new();
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

                            // SSE frames are blank-line separated. The payload is
                            // the `data:` line; codecs dispatch on the payload's
                            // own fields, so any `event:` line is ignored here.
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
            let canonical = self
                .codec
                .parse_response(&value)
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
        let Some(url) = self.codec.embeddings_url(&self.base_url) else {
            return Err(AdapterError::new(
                ErrorClass::UnsupportedCapability,
                "embeddings not supported by this wire format",
            ));
        };

        // Embeddings use the default path for now (no per-attempt egress here).
        let http = self.egress.client(None);
        let mut builder = http.post(&url).json(&body);
        builder = self.egress.path(None).apply_identity(builder);
        for (name, value) in self.codec.headers() {
            builder = builder.header(name, value);
        }
        builder = apply_auth(builder, &self.auth, lease.as_ref());

        let response = builder
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

        response
            .json::<serde_json::Value>()
            .await
            .map_err(|e| AdapterError::invalid(e.to_string()))
    }

    fn classify_error(&self, status: Option<u16>, _body: &str) -> ErrorClass {
        match status {
            Some(401) => ErrorClass::Authentication,
            Some(403) => ErrorClass::Authorization,
            // 429 rate limit; 529 (Anthropic "overloaded") falls in the 5xx arm.
            Some(429) => ErrorClass::RateLimited,
            Some(400) | Some(422) => ErrorClass::InvalidRequest,
            Some(408) | Some(504) => ErrorClass::Timeout,
            Some(value) if (500..600).contains(&value) => ErrorClass::ServerError,
            _ => ErrorClass::Unknown,
        }
    }
}
