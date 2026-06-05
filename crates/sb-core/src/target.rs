//! Execution targets and capability profiles. Routing is capability-based,
//! not provider-name-based: a request is matched against what a target can do.

use serde::{Deserialize, Serialize};

use crate::ImageSourceKind;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTargetKind {
    ModelApi,
    OpenAiCompatibleApi,
    LocalRuntime,
    CodingAgent,
    McpTool,
    RemoteAgent,
    Gateway,
    FallbackGroup,
}

/// What a target can do. The router hard-filters on these before scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityProfile {
    pub text_in: bool,
    pub text_out: bool,
    pub vision_in: bool,
    /// Empty means "legacy/unspecified": when `vision_in` is true, accept all
    /// currently-known image source kinds. Operators can set this to make image
    /// routing source-specific without breaking old catalogs.
    #[serde(default)]
    pub vision_sources: Vec<ImageSourceKind>,
    pub streaming: bool,
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub json_schema: bool,
    pub max_context_tokens: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

impl Default for CapabilityProfile {
    fn default() -> Self {
        CapabilityProfile {
            text_in: true,
            text_out: true,
            vision_in: false,
            vision_sources: Vec::new(),
            streaming: true,
            tool_calling: true,
            parallel_tool_calls: false,
            json_schema: false,
            max_context_tokens: None,
            max_output_tokens: None,
        }
    }
}

impl CapabilityProfile {
    /// A conservative text-only profile (no tools).
    pub fn basic_text() -> Self {
        CapabilityProfile {
            tool_calling: false,
            ..Default::default()
        }
    }

    pub fn supports_image_source(&self, source: ImageSourceKind) -> bool {
        self.vision_in && (self.vision_sources.is_empty() || self.vision_sources.contains(&source))
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct CostProfile {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

impl CostProfile {
    /// Blended price signal for routing: input + output per Mtok. Output usually
    /// dominates real spend, but at routing time completion length is unknown,
    /// so an equal blend is the honest, deterministic default.
    pub fn blended_per_mtok(&self) -> f64 {
        self.input_per_mtok + self.output_per_mtok
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    #[default]
    Healthy,
    Degraded,
    Down,
}

/// A concrete, routable place a request can be executed. Today these are
/// model APIs; tomorrow the same enum carries agents, tools, and gateways.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTarget {
    /// `provider/model`, e.g. `openrouter/openai/gpt-4o`.
    pub id: String,
    pub kind: ExecutionTargetKind,
    pub provider_id: String,
    /// Upstream model name as the provider expects it.
    pub model: String,
    #[serde(default)]
    pub capabilities: CapabilityProfile,
    #[serde(default)]
    pub cost: Option<CostProfile>,
    /// Recent observed total-latency EWMA (ms) for this target, stamped at
    /// routing time. `None` = not yet measured. Drives latency-aware routing.
    #[serde(default)]
    pub latency_ewma_ms: Option<f64>,
    /// Recent time-to-first-token EWMA (ms), stamped at routing time (streamed
    /// responses only). `None` = never streamed. Interactive (streaming) requests
    /// rank on this first; non-streaming ones rank on `latency_ewma_ms`.
    #[serde(default)]
    pub ttft_ewma_ms: Option<f64>,
    #[serde(default)]
    pub policy_tags: Vec<String>,
    /// Non-policy workload tags such as `coding`; used by execution profiles.
    #[serde(default)]
    pub task_tags: Vec<String>,
    #[serde(default)]
    pub health: HealthState,
    /// Currently-usable accounts in this target's pool (not locked, circuit not
    /// open), stamped at routing time from the non-secret account-pool view.
    /// `None` = unknown (not stamped); `Some(0)` = no healthy account right now,
    /// which the router demotes below targets that can actually execute.
    #[serde(default)]
    pub healthy_accounts: Option<usize>,
    /// This target is an unknown-model pass-through (forwarded verbatim to the
    /// default provider) — its capabilities + price are not catalog-verified.
    #[serde(default)]
    pub unverified: bool,
}

impl ExecutionTarget {
    pub fn new(
        provider_id: impl Into<String>,
        model: impl Into<String>,
        kind: ExecutionTargetKind,
    ) -> Self {
        let provider_id = provider_id.into();
        let model = model.into();
        ExecutionTarget {
            id: format!("{provider_id}/{model}"),
            kind,
            provider_id,
            model,
            capabilities: CapabilityProfile::default(),
            cost: None,
            latency_ewma_ms: None,
            ttft_ewma_ms: None,
            policy_tags: Vec::new(),
            task_tags: Vec::new(),
            health: HealthState::Healthy,
            healthy_accounts: None,
            unverified: false,
        }
    }
}
