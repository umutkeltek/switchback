//! The adapter boundary. Every execution target (model API, local runtime,
//! later: agent/tool) is reached through a `ProviderAdapter`. Adapters own
//! all provider-specific knowledge; the router and core stay agnostic.
//!
//! Adapters ALWAYS emit a normalized `Stream<AiStreamEvent>`. Non-streaming
//! responses are produced by collecting that stream upstream — one path.

use async_trait::async_trait;
use futures::stream::BoxStream;
use sb_core::{
    AiRequest, AiResponse, AiStreamEvent, CapabilityProfile, ContentPart, CredentialLease,
    ErrorClass, ExecutionTarget, HealthState, ToolCallStart,
};

/// The normalized event stream an adapter produces.
pub type EventStream = BoxStream<'static, Result<AiStreamEvent, AdapterError>>;

/// An execution failure, already classified into the shared `ErrorClass`
/// so the router can decide fallback/cooldown without provider knowledge.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{class:?} ({}): {message}", status.map(|s| s.to_string()).unwrap_or_else(|| "-".into()))]
pub struct AdapterError {
    pub class: ErrorClass,
    pub message: String,
    pub status: Option<u16>,
    pub retry_after_ms: Option<u64>,
}

impl AdapterError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        AdapterError {
            class,
            message: message.into(),
            status: None,
            retry_after_ms: None,
        }
    }
    pub fn network(message: impl Into<String>) -> Self {
        AdapterError::new(ErrorClass::Network, message)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        AdapterError::new(ErrorClass::InvalidRequest, message)
    }
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }
    pub fn with_retry_after_ms(mut self, ms: u64) -> Self {
        self.retry_after_ms = Some(ms);
        self
    }
    pub fn should_fallback(&self) -> bool {
        self.class.should_fallback()
    }
}

/// Everything an adapter needs to run one attempt. Owned (cheap clone vs a
/// network round-trip) so the returned stream can be `'static`.
pub struct PreparedRequest {
    pub request: AiRequest,
    pub target: ExecutionTarget,
    pub lease: Option<CredentialLease>,
    /// Outbound egress (named network path) to use, if any. `None` = the default
    /// path. The adapter resolves this to a client via the egress pool.
    pub egress_id: Option<String>,
}

impl PreparedRequest {
    pub fn new(
        request: AiRequest,
        target: ExecutionTarget,
        lease: Option<CredentialLease>,
    ) -> Self {
        PreparedRequest {
            request,
            target,
            lease,
            egress_id: None,
        }
    }

    /// Set the outbound egress path for this attempt.
    pub fn with_egress(mut self, egress_id: Option<String>) -> Self {
        self.egress_id = egress_id;
        self
    }
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Provider id, e.g. `openai_compatible` or `mock`.
    fn id(&self) -> &str;

    /// Declared capabilities for a given upstream model. The router
    /// hard-filters on these before attempting execution.
    fn capabilities(&self, model: &str) -> CapabilityProfile;

    /// Metadata-only request warnings the adapter can predict before dispatch,
    /// such as lossy target-dialect schema downleveling.
    fn request_warnings(&self, _req: &AiRequest, _target: &ExecutionTarget) -> Vec<String> {
        Vec::new()
    }

    /// Execute one attempt, returning the normalized event stream.
    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError>;

    async fn embeddings(
        &self,
        _body: serde_json::Value,
        _target: sb_core::ExecutionTarget,
        _lease: Option<sb_core::CredentialLease>,
        _egress_id: Option<String>,
    ) -> Result<serde_json::Value, AdapterError> {
        Err(AdapterError::new(
            sb_core::ErrorClass::UnsupportedCapability,
            "embeddings not supported by this adapter",
        ))
    }

    /// List upstream model ids visible to this provider/account. Adapters that
    /// have no supported model-list endpoint return `UnsupportedCapability`.
    async fn list_models(
        &self,
        _lease: Option<sb_core::CredentialLease>,
        _egress_id: Option<String>,
    ) -> Result<Vec<String>, AdapterError> {
        Err(AdapterError::new(
            sb_core::ErrorClass::UnsupportedCapability,
            "model listing not supported by this adapter",
        ))
    }

    /// Map an upstream HTTP status + body into the shared error taxonomy.
    fn classify_error(&self, status: Option<u16>, body: &str) -> ErrorClass;

    /// Cheap liveness signal (default: healthy). Override for real probes.
    async fn health_check(&self) -> HealthState {
        HealthState::Healthy
    }
}

/// Collapse a fully-assembled (non-streamed) response into the canonical event
/// stream every adapter emits. The non-streaming path is just "parse one
/// upstream JSON body, then replay it as the same `AiStreamEvent` sequence a
/// real stream would have produced" — one path, per the streaming-first rule.
/// Shared so every adapter's collect-path is identical (openai, anthropic, …).
pub fn response_to_events(resp: &AiResponse) -> Vec<AiStreamEvent> {
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
