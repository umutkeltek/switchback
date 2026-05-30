//! Error taxonomy. Adapters map upstream failures into `ErrorClass`, which
//! drives fallback and cooldown decisions in the router.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Authentication,
    Authorization,
    RateLimited,
    QuotaExceeded,
    ProviderOverloaded,
    Timeout,
    Network,
    InvalidRequest,
    ContextTooLong,
    UnsupportedCapability,
    SafetyBlocked,
    ServerError,
    StreamInterrupted,
    Unknown,
}

impl ErrorClass {
    /// Should the orchestrator try the next candidate (next ACCOUNT, then next
    /// TARGET) on this error? In a multi-account gateway an auth failure on one
    /// account means "try another account", not "fail the request" — the
    /// per-account lock already prevents retrying the same bad credential. Only
    /// a malformed request or a safety refusal is pointless to re-route, so
    /// those surface verbatim; everything else (auth, rate-limit, overload,
    /// timeout, network, 5xx) falls back.
    pub fn should_fallback(self) -> bool {
        !matches!(self, ErrorClass::InvalidRequest | ErrorClass::SafetyBlocked)
    }

    /// Surface verbatim to the client rather than retrying elsewhere.
    pub fn is_client_error(self) -> bool {
        matches!(
            self,
            ErrorClass::InvalidRequest
                | ErrorClass::ContextTooLong
                | ErrorClass::UnsupportedCapability
        )
    }

    /// Stable snake_case name (matches the serde rename) — for logs/traces.
    pub fn as_str(self) -> &'static str {
        use ErrorClass::*;
        match self {
            Authentication => "authentication",
            Authorization => "authorization",
            RateLimited => "rate_limited",
            QuotaExceeded => "quota_exceeded",
            ProviderOverloaded => "provider_overloaded",
            Timeout => "timeout",
            Network => "network",
            InvalidRequest => "invalid_request",
            ContextTooLong => "context_too_long",
            UnsupportedCapability => "unsupported_capability",
            SafetyBlocked => "safety_blocked",
            ServerError => "server_error",
            StreamInterrupted => "stream_interrupted",
            Unknown => "unknown",
        }
    }

    pub fn http_status(self) -> u16 {
        use ErrorClass::*;
        match self {
            Authentication => 401,
            Authorization => 403,
            RateLimited | QuotaExceeded | ProviderOverloaded => 429,
            Timeout => 504,
            Network | ServerError | StreamInterrupted | Unknown => 502,
            InvalidRequest => 400,
            ContextTooLong => 413,
            UnsupportedCapability => 422,
            SafetyBlocked => 451,
        }
    }
}

/// Errors produced inside the core/router (not adapter/HTTP errors).
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("config error: {0}")]
    Config(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("no route or target matched model `{0}`")]
    NoRoute(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_policy_is_sane() {
        assert!(ErrorClass::RateLimited.should_fallback());
        assert!(ErrorClass::Timeout.should_fallback());
        // multi-account: a bad key on one account must try the others
        assert!(ErrorClass::Authentication.should_fallback());
        assert!(ErrorClass::Authorization.should_fallback());
        // these are pointless to re-route
        assert!(!ErrorClass::InvalidRequest.should_fallback());
        assert!(!ErrorClass::SafetyBlocked.should_fallback());
    }
}
