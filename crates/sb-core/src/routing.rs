//! The explainable route decision. Every request produces one of these;
//! it is logged and surfaced (header `x-switchback-route`). Routing is
//! never an opaque black box — this is the enterprise moat, built from day 1.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetRef {
    /// `provider/model`.
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl TargetRef {
    pub fn new(id: impl Into<String>) -> Self {
        TargetRef { target_id: id.into(), account_id: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedCandidate {
    pub target_id: String,
    pub reason: String,
}

/// Why a request went where it went: what was selected, the ordered
/// fallbacks behind it, the human-readable reasons, and what was rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteDecision {
    pub request_id: String,
    pub strategy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<TargetRef>,
    #[serde(default)]
    pub fallbacks: Vec<TargetRef>,
    #[serde(default)]
    pub reason: Vec<String>,
    #[serde(default)]
    pub rejected: Vec<RejectedCandidate>,
}

impl RouteDecision {
    pub fn new(request_id: impl Into<String>, strategy: impl Into<String>) -> Self {
        RouteDecision {
            request_id: request_id.into(),
            strategy: strategy.into(),
            selected: None,
            fallbacks: Vec::new(),
            reason: Vec::new(),
            rejected: Vec::new(),
        }
    }

    pub fn with_reason(mut self, r: impl Into<String>) -> Self {
        self.reason.push(r.into());
        self
    }

    pub fn add_reason(&mut self, r: impl Into<String>) {
        self.reason.push(r.into());
    }

    pub fn reject(&mut self, target_id: impl Into<String>, reason: impl Into<String>) {
        self.rejected.push(RejectedCandidate {
            target_id: target_id.into(),
            reason: reason.into(),
        });
    }

    /// Compact one-line summary for the `x-switchback-route` header.
    pub fn summary(&self) -> String {
        let sel = self
            .selected
            .as_ref()
            .map(|t| t.target_id.as_str())
            .unwrap_or("none");
        let fb: Vec<&str> = self.fallbacks.iter().map(|t| t.target_id.as_str()).collect();
        format!(
            "strategy={} selected={} fallbacks=[{}] rejected={}",
            self.strategy,
            sel,
            fb.join(","),
            self.rejected.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_summary_is_informative() {
        let mut d = RouteDecision::new("req_1", "ordered_fallback");
        d.selected = Some(TargetRef::new("mock/echo"));
        d.fallbacks.push(TargetRef::new("openrouter/openai/gpt-4o"));
        d.add_reason("route=default");
        d.reject("ollama/qwen", "provider unhealthy");
        let s = d.summary();
        assert!(s.contains("selected=mock/echo"));
        assert!(s.contains("rejected=1"));
    }
}
