//! The explainable route decision. Every request produces one of these;
//! it is logged and surfaced (header `x-switchback-route`). Routing is
//! never an opaque black box — this is the enterprise moat, built from day 1.

use serde::{Deserialize, Serialize};

/// User-facing virtual model contracts such as `auto/cheap`. These are not a
/// second routing system: the runtime resolves them into the same candidate list
/// and the router emits a normal [`RouteDecision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionProfile {
    Auto,
    Cheap,
    Fast,
    Coding,
    Private,
    LargeContext,
}

impl ExecutionProfile {
    pub fn from_model(model: &str) -> Option<Self> {
        match model {
            "auto" => Some(Self::Auto),
            "auto/cheap" => Some(Self::Cheap),
            "auto/fast" => Some(Self::Fast),
            "auto/coding" => Some(Self::Coding),
            "auto/private" => Some(Self::Private),
            "auto/large-context" | "auto/large_context" => Some(Self::LargeContext),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cheap => "auto/cheap",
            Self::Fast => "auto/fast",
            Self::Coding => "auto/coding",
            Self::Private => "auto/private",
            Self::LargeContext => "auto/large-context",
        }
    }
}

/// How the router orders surviving candidates. Default = declared fallback
/// order. `cost_aware` re-sorts cheapest-first by blended price; an optional
/// `max_price_per_mtok` caps eligibility. Extensible (latency-aware etc. later).
#[derive(Debug, Clone)]
pub struct RoutingPolicy {
    /// Optional execution profile that requested this plan.
    pub profile: Option<ExecutionProfile>,
    pub cost_aware: bool,
    pub max_price_per_mtok: Option<f64>,
    /// Sort surviving candidates fastest-first by observed latency EWMA.
    /// `cost_aware` takes precedence when both are set.
    pub latency_aware: bool,
    /// Cost-routing policy gates (all default-allow). A candidate tagged with a
    /// disallowed lane is rejected: `free` (price 0 / free tier), `promo`
    /// (time-boxed price), `aggregator` (third-party host of open weights).
    pub allow_free: bool,
    pub allow_promo: bool,
    pub allow_aggregator: bool,
    /// Apply lane gates even when cost-aware routing is off. This is used by
    /// policy-style profiles such as `auto/private`.
    pub enforce_lane_policy: bool,
}

impl Default for RoutingPolicy {
    fn default() -> Self {
        RoutingPolicy {
            profile: None,
            cost_aware: false,
            max_price_per_mtok: None,
            latency_aware: false,
            allow_free: true,
            allow_promo: true,
            allow_aggregator: true,
            enforce_lane_policy: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetRef {
    /// `provider/model`.
    pub target_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl TargetRef {
    pub fn new(id: impl Into<String>) -> Self {
        TargetRef {
            target_id: id.into(),
            account_id: None,
        }
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
    /// The execution profile requested by the client, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<TargetRef>,
    #[serde(default)]
    pub fallbacks: Vec<TargetRef>,
    #[serde(default)]
    pub reason: Vec<String>,
    #[serde(default)]
    pub rejected: Vec<RejectedCandidate>,
    /// The selected target is an unknown-model pass-through (forwarded verbatim to
    /// the default provider): its capabilities and price are NOT catalog-verified.
    /// Surfaced so clients/operators don't treat it as a known model (Oracle #5).
    #[serde(default)]
    pub unverified: bool,
}

impl RouteDecision {
    pub fn new(request_id: impl Into<String>, strategy: impl Into<String>) -> Self {
        RouteDecision {
            request_id: request_id.into(),
            strategy: strategy.into(),
            profile: None,
            selected: None,
            fallbacks: Vec::new(),
            reason: Vec::new(),
            rejected: Vec::new(),
            unverified: false,
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
        let fb: Vec<&str> = self
            .fallbacks
            .iter()
            .map(|t| t.target_id.as_str())
            .collect();
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
