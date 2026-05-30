//! The adapter boundary. Every execution target (model API, local runtime,
//! later: agent/tool) is reached through a `ProviderAdapter`. Adapters own
//! all provider-specific knowledge; the router and core stay agnostic.
//!
//! Adapters ALWAYS emit a normalized `Stream<AiStreamEvent>`. Non-streaming
//! responses are produced by collecting that stream upstream — one path.

use async_trait::async_trait;
use futures::stream::BoxStream;
use sb_core::{
    AiRequest, AiStreamEvent, CapabilityProfile, CredentialLease, ErrorClass, ExecutionTarget,
    HealthState,
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
        AdapterError { class, message: message.into(), status: None, retry_after_ms: None }
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
}

impl PreparedRequest {
    pub fn new(request: AiRequest, target: ExecutionTarget, lease: Option<CredentialLease>) -> Self {
        PreparedRequest { request, target, lease }
    }
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// Provider id, e.g. `openai_compatible` or `mock`.
    fn id(&self) -> &str;

    /// Declared capabilities for a given upstream model. The router
    /// hard-filters on these before attempting execution.
    fn capabilities(&self, model: &str) -> CapabilityProfile;

    /// Execute one attempt, returning the normalized event stream.
    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError>;

    /// Map an upstream HTTP status + body into the shared error taxonomy.
    fn classify_error(&self, status: Option<u16>, body: &str) -> ErrorClass;

    /// Cheap liveness signal (default: healthy). Override for real probes.
    async fn health_check(&self) -> HealthState {
        HealthState::Healthy
    }
}
