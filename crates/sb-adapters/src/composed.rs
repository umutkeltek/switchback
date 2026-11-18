//! The generic adapter: `Codec × Signer × Transport`. One execute loop serves
//! every provider — the codec translates the wire, the [`crate::signer`] attaches
//! or signs auth, and the [`crate::transport`] frames the byte stream. Simple
//! providers use [`ComposedAdapter::with_scheme`] (bearer/header auth + text SSE);
//! request-signing / binary-framed providers (Bedrock) pass an explicit signer +
//! transport — no bespoke adapter, no complexity tax on the simple path.

use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream, PreparedRequest, ProviderAdapter};
use sb_core::{AiStreamEvent, AuthScheme, CapabilityProfile, ErrorClass};

use crate::codec::StreamDecoder;
use crate::codec::WireCodec;
use crate::signer::{RequestSigner, SchemeSigner, SignTarget};
use crate::transport::{HttpTransport, Transport};

pub struct ComposedAdapter {
    codec: Box<dyn WireCodec>,
    signer: Box<dyn RequestSigner>,
    transport: Box<dyn Transport>,
    base_url: String,
    capabilities: CapabilityProfile,
    /// Shared pool of per-egress HTTP clients. The attempt's `egress_id` selects
    /// which outbound path (direct / a configured proxy) the call exits from.
    egress: std::sync::Arc<crate::egress::EgressPool>,
    block_private_networks: bool,
}

impl ComposedAdapter {
    /// Full composition — for providers needing a non-default signer/transport.
    pub fn new(
        codec: Box<dyn WireCodec>,
        signer: Box<dyn RequestSigner>,
        transport: Box<dyn Transport>,
        base_url: String,
        capabilities: CapabilityProfile,
        egress: std::sync::Arc<crate::egress::EgressPool>,
        block_private_networks: bool,
    ) -> Self {
        Self {
            codec,
            signer,
            transport,
            base_url,
            capabilities,
            egress,
            block_private_networks,
        }
    }

    /// The simple path: an [`AuthScheme`] signer + plain HTTP/text-SSE transport.
    /// What every OpenAI-shaped / Anthropic / Gemini provider uses.
    pub fn with_scheme(
        codec: Box<dyn WireCodec>,
        auth: AuthScheme,
        base_url: String,
        capabilities: CapabilityProfile,
        egress: std::sync::Arc<crate::egress::EgressPool>,
        block_private_networks: bool,
    ) -> Self {
        Self::new(
            codec,
            Box::new(SchemeSigner(auth)),
            Box::new(HttpTransport),
            base_url,
            capabilities,
            egress,
            block_private_networks,
        )
    }

    async fn guard_url(&self, url: &str) -> Result<(), AdapterError> {
        sb_net::guard_url(
            url,
            sb_net::NetworkUrlKind::ProviderUpstream,
            self.block_private_networks,
        )
        .await
        .map_err(|e| match e {
            sb_net::NetworkGuardError::InvalidUrl(message) => AdapterError::invalid(message),
            sb_net::NetworkGuardError::ResolveFailed(message) => AdapterError::network(message),
            sb_net::NetworkGuardError::BlockedPrivate(message) => {
                AdapterError::new(ErrorClass::SafetyBlocked, message)
            }
        })
    }
}

fn stream_framing_error(message: String) -> AdapterError {
    let lower = message.to_ascii_lowercase();
    let class = if lower.contains("throttl") || lower.contains("rate limit") {
        ErrorClass::RateLimited
    } else if lower.contains("quota") {
        ErrorClass::QuotaExceeded
    } else if lower.contains("overload") {
        ErrorClass::ProviderOverloaded
    } else if lower.contains("timeout") || lower.contains("timed out") {
        ErrorClass::Timeout
    } else if lower.contains("malformed")
        || lower.contains("bad lengths")
        || lower.contains("truncated")
        || lower.contains("unsupported header")
    {
        ErrorClass::InvalidRequest
    } else {
        ErrorClass::StreamInterrupted
    };
    AdapterError::new(class, message)
}

/// Error text of a failed upstream call. It never quotes the URL: the
/// `query` auth scheme puts the credential there. The error kind stays.
fn upstream_error_text(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

fn is_reasoning(event: &AiStreamEvent) -> bool {
    matches!(event, AiStreamEvent::ReasoningDelta { .. })
}

fn finish_decoder_if_clean(
    decoder: &mut dyn StreamDecoder,
    had_stream_error: bool,
) -> Vec<sb_core::AiStreamEvent> {
    if had_stream_error {
        Vec::new()
    } else {
        decoder.finish()
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

    fn request_warnings(
        &self,
        req: &sb_core::AiRequest,
        target: &sb_core::ExecutionTarget,
    ) -> Vec<String> {
        self.codec
            .request_warnings(req, &target.model)
            .into_iter()
            .map(|warning| format!("{}: {warning}", self.codec.id()))
            .collect()
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        let downstream_stream = prepared.request.stream;
        let stream = self.codec.upstream_stream(downstream_stream);
        let model = prepared.target.model.clone();
        let body = self
            .codec
            .request_body(&prepared.request, &model, downstream_stream)
            .map_err(AdapterError::invalid)?;
        // Serialize ONCE so the exact bytes we sign are the exact bytes we send.
        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| AdapterError::invalid(e.to_string()))?;
        let url = self.codec.url(&self.base_url, &model, stream);
        self.guard_url(&url).await?;

        // Sign over the built request (the signer reads the parts it needs).
        let (host, path, query) = crate::signer::split_url(&url);
        let additions = self.signer.sign(
            &SignTarget {
                method: "POST",
                host: &host,
                path: &path,
                query: &query,
                body: &body_bytes,
            },
            prepared.lease.as_ref(),
        );

        // Select the outbound path for this attempt (direct unless the account/
        // provider named an egress and it's enabled). The path carries both the
        // proxy client and an optional client identity (custom UA + headers).
        let epath = self.egress.path(prepared.egress_id.as_deref());
        let mut builder = epath
            .client()
            .post(&url)
            .header("content-type", "application/json")
            .body(body_bytes);
        for (name, value) in self.codec.headers() {
            builder = builder.header(name, value);
        }
        if stream {
            if let Some(accept) = self.transport.stream_accept() {
                builder = builder.header("accept", accept);
            }
        }
        // Egress identity FIRST (it can't set auth headers), then auth LAST so the
        // lease's credentials always win — an egress can never override auth.
        builder = epath.apply_identity(builder); // per-path UA + non-auth headers
        for (name, value) in &additions.headers {
            builder = builder.header(name, value);
        }
        if !additions.query.is_empty() {
            builder = builder.query(&additions.query);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| AdapterError::network(upstream_error_text(e)))?;
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

        // Thinking the client's wire format cannot take back is dropped here,
        // once, for the stream and the collected response alike.
        let keeps_reasoning = self.codec.keeps_reasoning(&prepared.request);
        let wanted = move |event: &AiStreamEvent| keeps_reasoning || !is_reasoning(event);
        if stream {
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let mut upstream = response.bytes_stream();
            let mut framer = self.transport.framer();
            let mut decoder = self.codec.decoder(&model);

            tokio::spawn(async move {
                let mut had_stream_error = false;
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
                    let bytes = match chunk_result {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            had_stream_error = true;
                            let _ = tx
                                .send(Err(AdapterError::network("stream byte error")))
                                .await;
                            break;
                        }
                    };
                    // Transport frames raw bytes → JSON values; codec decodes
                    // each value → canonical events. Framing and semantics, split.
                    match framer.push(&bytes) {
                        Ok(values) => {
                            for value in values {
                                for event in decoder.decode(&value).into_iter().filter(wanted) {
                                    if tx.send(Ok(event)).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            had_stream_error = true;
                            let _ = tx.send(Err(stream_framing_error(e))).await;
                            break;
                        }
                    }
                }

                for event in finish_decoder_if_clean(decoder.as_mut(), had_stream_error)
                    .into_iter()
                    .filter(wanted)
                {
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
                .map_err(|e| AdapterError::network(upstream_error_text(e)))?;
            let value = serde_json::from_slice::<serde_json::Value>(&full)
                .map_err(|e| AdapterError::invalid(e.to_string()))?;
            let canonical = self
                .codec
                .parse_response(&value)
                .map_err(AdapterError::invalid)?;
            let events = self.codec.response_events(&canonical);

            Ok(futures::stream::iter(events.into_iter().filter(wanted).map(Ok)).boxed())
        }
    }

    async fn embeddings(
        &self,
        body: serde_json::Value,
        _target: sb_core::ExecutionTarget,
        lease: Option<sb_core::CredentialLease>,
        egress_id: Option<String>,
    ) -> Result<serde_json::Value, AdapterError> {
        let Some(url) = self.codec.embeddings_url(&self.base_url) else {
            return Err(AdapterError::new(
                ErrorClass::UnsupportedCapability,
                "embeddings not supported by this wire format",
            ));
        };
        self.guard_url(&url).await?;

        let body_bytes =
            serde_json::to_vec(&body).map_err(|e| AdapterError::invalid(e.to_string()))?;
        let (host, path, query) = crate::signer::split_url(&url);
        let additions = self.signer.sign(
            &SignTarget {
                method: "POST",
                host: &host,
                path: &path,
                query: &query,
                body: &body_bytes,
            },
            lease.as_ref(),
        );

        let epath = self.egress.path(egress_id.as_deref());
        let mut builder = epath
            .client()
            .post(&url)
            .header("content-type", "application/json")
            .body(body_bytes);
        for (name, value) in self.codec.headers() {
            builder = builder.header(name, value);
        }
        // Identity first (no auth headers allowed), auth last so the lease wins.
        builder = epath.apply_identity(builder);
        for (name, value) in &additions.headers {
            builder = builder.header(name, value);
        }
        if !additions.query.is_empty() {
            builder = builder.query(&additions.query);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| AdapterError::network(upstream_error_text(e)))?;
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
            .map_err(|e| AdapterError::invalid(upstream_error_text(e)))
    }

    async fn list_models(
        &self,
        lease: Option<sb_core::CredentialLease>,
        egress_id: Option<String>,
    ) -> Result<Vec<String>, AdapterError> {
        let Some(url) = self.codec.models_url(&self.base_url) else {
            return Err(AdapterError::new(
                ErrorClass::UnsupportedCapability,
                "model listing not supported by this wire format",
            ));
        };
        self.guard_url(&url).await?;

        let body_bytes: Vec<u8> = Vec::new();
        let (host, path, query) = crate::signer::split_url(&url);
        let additions = self.signer.sign(
            &SignTarget {
                method: "GET",
                host: &host,
                path: &path,
                query: &query,
                body: &body_bytes,
            },
            lease.as_ref(),
        );

        let epath = self.egress.path(egress_id.as_deref());
        let mut builder = epath.client().get(&url);
        for (name, value) in self.codec.headers() {
            builder = builder.header(name, value);
        }
        builder = epath.apply_identity(builder);
        for (name, value) in &additions.headers {
            builder = builder.header(name, value);
        }
        if !additions.query.is_empty() {
            builder = builder.query(&additions.query);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| AdapterError::network(upstream_error_text(e)))?;
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
            .map_err(|e| AdapterError::invalid(upstream_error_text(e)))?;
        self.codec
            .parse_models_response(&value)
            .map_err(AdapterError::invalid)
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

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::{AiStreamEvent, FinishReason};

    struct FinishOnlyDecoder;

    impl StreamDecoder for FinishOnlyDecoder {
        fn decode(&mut self, _frame: &serde_json::Value) -> Vec<AiStreamEvent> {
            Vec::new()
        }

        fn finish(&mut self) -> Vec<AiStreamEvent> {
            vec![AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop,
            }]
        }
    }

    #[test]
    fn stream_framing_errors_are_classified_from_provider_signals() {
        assert_eq!(
            stream_framing_error("bedrock stream throttlingException: slow down".to_string()).class,
            ErrorClass::RateLimited
        );
        assert_eq!(
            stream_framing_error(
                "bedrock stream modelStreamErrorException: quota exhausted".to_string()
            )
            .class,
            ErrorClass::QuotaExceeded
        );
        assert_eq!(
            stream_framing_error("malformed SSE JSON frame".to_string()).class,
            ErrorClass::InvalidRequest
        );
    }

    #[test]
    fn framing_error_does_not_emit_decoder_finish_events() {
        let mut decoder = FinishOnlyDecoder;

        let events = finish_decoder_if_clean(&mut decoder, true);

        assert!(events.is_empty());
    }

    #[test]
    fn clean_stream_flushes_decoder_finish_events() {
        let mut decoder = FinishOnlyDecoder;

        let events = finish_decoder_if_clean(&mut decoder, false);

        assert!(matches!(
            events.as_slice(),
            [AiStreamEvent::MessageEnd {
                finish_reason: FinishReason::Stop
            }]
        ));
    }

    const FAKE_QUERY_SECRET: &str = "fake-query-secret-123";

    /// A provider whose credential rides in the URL (`?key=…`, the `query`
    /// auth scheme), with a short idle timeout.
    fn query_credential_adapter(base_url: String) -> ComposedAdapter {
        let cfg = sb_core::Config::from_yaml(
            "server:\n  bind: \"127.0.0.1:0\"\n  timeouts: { connect_ms: 2000, read_ms: 300 }\n",
        )
        .expect("config");
        ComposedAdapter::with_scheme(
            Box::new(crate::OpenAiCodec),
            AuthScheme::Query { name: "key".into() },
            base_url,
            CapabilityProfile::default(),
            std::sync::Arc::new(crate::egress::EgressPool::from_config(&cfg).expect("egress")),
            false,
        )
    }

    /// The error of every call the adapter makes: chat (non-stream and
    /// stream), embeddings and model listing.
    async fn every_call_error(adapter: &ComposedAdapter) -> Vec<AdapterError> {
        let lease = || {
            Some(sb_core::CredentialLease::bearer(
                "default",
                FAKE_QUERY_SECRET,
            ))
        };
        let target = sb_core::ExecutionTarget::new(
            "q",
            "some-model",
            sb_core::ExecutionTargetKind::ModelApi,
        );
        let mut errors = Vec::new();
        for stream in [false, true] {
            let mut req =
                sb_core::AiRequest::new("q/some-model", vec![sb_core::Message::user("hi")]);
            req.stream = stream;
            match adapter
                .execute(PreparedRequest::new(req, target.clone(), lease()))
                .await
            {
                Ok(_) => panic!("chat call (stream={stream}) did not fail"),
                Err(error) => errors.push(error),
            }
        }
        errors.push(
            adapter
                .embeddings(
                    serde_json::json!({"model": "some-model", "input": "hi"}),
                    target,
                    lease(),
                    None,
                )
                .await
                .expect_err("embeddings call did not fail"),
        );
        errors.push(
            adapter
                .list_models(lease(), None)
                .await
                .expect_err("model listing did not fail"),
        );
        errors
    }

    // The `query` scheme puts the credential in the URL, and a reqwest
    // error's text quotes the URL: the error must drop it, and keep the rest.
    #[tokio::test]
    async fn a_failed_call_never_quotes_a_url_that_carries_the_credential() {
        // Nothing listens on port 1: connection refused.
        let refused = query_credential_adapter("http://127.0.0.1:1/v1".to_string());

        // Accepts and never answers: the idle (read) timeout fires.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let hangs = query_credential_adapter(format!("http://{addr}/v1"));

        for (upstream, adapter) in [("refused", &refused), ("hangs", &hangs)] {
            let errors = every_call_error(adapter).await;
            assert_eq!(errors.len(), 4);
            for error in errors {
                let shown = format!("{error} / {error:?}");
                assert!(!shown.contains(FAKE_QUERY_SECRET), "{upstream}: {shown}");
                assert!(!shown.contains("key="), "{upstream}: {shown}");
                assert_eq!(error.class, ErrorClass::Network, "{upstream}: {shown}");
                assert!(
                    error.message.contains("error sending request"),
                    "{upstream}: the error kind stays: {shown}"
                );
            }
        }
    }

    #[tokio::test]
    async fn private_network_guard_blocks_dns_and_literal_private_hosts() {
        let adapter = ComposedAdapter::with_scheme(
            Box::new(crate::OpenAiCodec),
            AuthScheme::None,
            "https://api.example.com/v1".to_string(),
            CapabilityProfile::default(),
            std::sync::Arc::new(crate::egress::EgressPool::direct_only()),
            true,
        );

        let literal_host = "127.0.0.1";
        let literal = adapter
            .guard_url(&format!("http://{literal_host}:11434/v1/chat/completions"))
            .await;
        assert!(matches!(
            literal,
            Err(AdapterError {
                class: ErrorClass::SafetyBlocked,
                ..
            })
        ));

        let localhost_host = "localhost";
        let localhost = adapter
            .guard_url(&format!(
                "http://{localhost_host}:11434/v1/chat/completions"
            ))
            .await;
        assert!(matches!(
            localhost,
            Err(AdapterError {
                class: ErrorClass::SafetyBlocked,
                ..
            })
        ));
    }
}
