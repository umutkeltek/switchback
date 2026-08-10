use std::time::Duration;

use sb_adapter::{AdapterError, EventStream};
use sb_core::{AiResponse, Usage};

/// A committed execution failure, rendered to the client's wire format by the
/// HTTP edge. Carries an HTTP-ish status hint, an error type string, the
/// message, optional typed retry timing, and (when a routing decision was made)
/// the route summary so the edge can still stamp `x-switchback-route`.
#[derive(Debug, Clone)]
pub struct ExecError {
    pub status: u16,
    pub error_type: String,
    pub message: String,
    pub summary: Option<String>,
    pub retry_after: Option<Duration>,
}

impl ExecError {
    pub fn new(
        status: u16,
        error_type: impl Into<String>,
        message: impl Into<String>,
        summary: Option<String>,
    ) -> Self {
        ExecError {
            status,
            error_type: error_type.into(),
            message: message.into(),
            summary,
            retry_after: None,
        }
    }

    /// Preserve a machine-readable retry delay for the HTTP edge. The message
    /// remains human-readable and is never parsed to recover this value.
    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after = Some(retry_after);
        self
    }

    /// An upstream attempt failure (after a routing decision was made).
    pub(crate) fn upstream(error: &AdapterError, summary: &str) -> Self {
        ExecError {
            status: error.class.http_status(),
            error_type: "upstream_error".to_string(),
            message: error.message.clone(),
            summary: Some(summary.to_string()),
            retry_after: None,
        }
    }
}

/// Committed result of the shared execution core: a live stream (client wants
/// streaming), a collected response (non-streaming), or a structured error.
pub enum ExecOutcome {
    Stream {
        stream: EventStream,
        summary: String,
    },
    Collected {
        response: Box<AiResponse>,
        summary: String,
    },
    Error(ExecError),
}

/// Committed result of the embeddings runtime path. The response stays in the
/// OpenAI-compatible embeddings wire shape because embeddings are not canonical
/// chat/message IR.
pub enum EmbeddingsOutcome {
    Json {
        value: serde_json::Value,
        summary: String,
        request_id: String,
    },
    Error {
        error: ExecError,
        request_id: String,
    },
}

pub(crate) fn embeddings_usage(value: &serde_json::Value) -> Usage {
    let prompt = value
        .pointer("/usage/prompt_tokens")
        .and_then(serde_json::Value::as_u64);
    let total = value
        .pointer("/usage/total_tokens")
        .and_then(serde_json::Value::as_u64);
    Usage {
        input_tokens: prompt.or(total).unwrap_or_default(),
        ..Usage::default()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::ExecError;

    #[test]
    fn exec_error_preserves_exact_retry_after_duration() {
        let retry_after = Duration::from_millis(1_501);

        let error = ExecError::new(503, "provider_unavailable", "cooling down", None)
            .with_retry_after(retry_after);

        assert_eq!(error.retry_after, Some(retry_after));
    }
}
