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
    /// Should the router try the next candidate on this error? Real client
    /// errors and safety blocks are NOT masked by falling back.
    pub fn should_fallback(self) -> bool {
        !matches!(
            self,
            ErrorClass::InvalidRequest | ErrorClass::SafetyBlocked | ErrorClass::Authentication
        )
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
        assert!(!ErrorClass::InvalidRequest.should_fallback());
        assert!(!ErrorClass::SafetyBlocked.should_fallback());
    }
}
