//! Execution-control metadata: normalized jobs, cache fingerprints, receipts,
//! harness descriptors, and evaluation events.
//!
//! This module is provider-agnostic. It summarizes [`AiRequest`] for routing,
//! caching, and observability; it is not a second request IR.

use crate::{AiRequest, PrivacyClass, ResponseFormat};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const EXECUTION_POLICY_VERSION: &str = "execution-policy/v1";
pub const CACHE_KEY_VERSION: &str = "execution-cache-key/v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTaskType {
    Chat,
    Coding,
    Extraction,
    Judge,
    ToolAgent,
    Embeddings,
    Unknown,
}

impl ExecutionTaskType {
    pub fn parse(value: &str) -> Self {
        match value {
            "chat" => Self::Chat,
            "coding" | "code" | "code_patch" => Self::Coding,
            "extract" | "extraction" | "classify" | "classification" => Self::Extraction,
            "judge" | "critique" | "review" => Self::Judge,
            "tool_agent" | "agent" => Self::ToolAgent,
            "embedding" | "embeddings" => Self::Embeddings,
            _ => Self::Unknown,
        }
    }

    pub fn infer(req: &AiRequest) -> Self {
        if let Some(task) = req.metadata.get("task_type") {
            return Self::parse(task);
        }
        if req.requires_tools() {
            return Self::ToolAgent;
        }
        match req.model.as_str() {
            "auto/coding" | "coding" => Self::Coding,
            "auto/extract" | "auto/classify" => Self::Extraction,
            "auto/judge" | "auto/critique" => Self::Judge,
            _ => Self::Chat,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionSource {
    Api,
    Codex,
    ClaudeCode,
    Opencode,
    Tap,
    Harness,
    Unknown,
}

impl ExecutionSource {
    pub fn parse(value: &str) -> Self {
        match value {
            "api" | "openai_chat" | "openai_responses" | "anthropic_messages" => Self::Api,
            "codex" => Self::Codex,
            "claude" | "claude-code" | "claude_code" => Self::ClaudeCode,
            "opencode" => Self::Opencode,
            "tap" | "native_tap" => Self::Tap,
            "harness" => Self::Harness,
            _ => Self::Unknown,
        }
    }

    pub fn infer(req: &AiRequest) -> Self {
        if let Some(source) = req.metadata.get("execution_source") {
            return Self::parse(source);
        }
        if let Some(profile) = req.metadata.get("client_profile") {
            return Self::parse(profile);
        }
        if req.metadata.contains_key("client_protocol") {
            return Self::Api;
        }
        Self::Api
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum LatencyPreference {
    Interactive,
    Balanced,
    Batch,
}

impl LatencyPreference {
    pub fn infer(req: &AiRequest) -> Self {
        if let Some(value) = req.metadata.get("latency_preference") {
            return match value.as_str() {
                "interactive" | "fast" | "low_latency" => Self::Interactive,
                "batch" | "cheap" | "throughput" => Self::Batch,
                _ => Self::Balanced,
            };
        }
        if req.stream {
            Self::Interactive
        } else {
            Self::Balanced
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestedCapabilities {
    pub streaming: bool,
    pub tool_calling: bool,
    pub vision_in: bool,
    pub json_schema: bool,
}

impl RequestedCapabilities {
    pub fn from_request(req: &AiRequest) -> Self {
        RequestedCapabilities {
            streaming: req.stream,
            tool_calling: req.requires_tools(),
            vision_in: req.requires_vision(),
            json_schema: matches!(req.response_format, Some(ResponseFormat::JsonSchema { .. })),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputSize {
    pub message_count: usize,
    pub tool_count: usize,
    pub approx_input_chars: usize,
}

impl InputSize {
    pub fn from_request(req: &AiRequest) -> Self {
        let message_chars = req
            .messages
            .iter()
            .map(|message| message.text().len())
            .sum::<usize>();
        let system_chars = req.system.as_deref().map(str::len).unwrap_or_default();
        InputSize {
            message_count: req.messages.len(),
            tool_count: req.tools.len(),
            approx_input_chars: message_chars + system_chars,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionJob {
    pub job_id: String,
    pub task_type: ExecutionTaskType,
    pub source: ExecutionSource,
    pub privacy_level: PrivacyClass,
    pub latency_preference: LatencyPreference,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_ceiling_micros: Option<u64>,
    pub context_fingerprint: String,
    pub requested_capabilities: RequestedCapabilities,
    pub input_size: InputSize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

impl ExecutionJob {
    pub fn from_request(req: &AiRequest) -> Self {
        let exact_key = CacheKey::exact_request(req);
        ExecutionJob {
            job_id: req.id.clone(),
            task_type: ExecutionTaskType::infer(req),
            source: ExecutionSource::infer(req),
            privacy_level: req.privacy_class,
            latency_preference: LatencyPreference::infer(req),
            cost_ceiling_micros: req
                .metadata
                .get("cost_ceiling_micros")
                .and_then(|value| value.parse::<u64>().ok()),
            context_fingerprint: exact_key.key,
            requested_capabilities: RequestedCapabilities::from_request(req),
            input_size: InputSize::from_request(req),
            tenant: req.tenant.clone(),
            project: req.project.clone(),
            workspace_id: req.metadata.get("workspace_id").cloned(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CacheLayer {
    ExactRequest,
    ContextArtifact,
    ToolResult,
    ProviderResponse,
    HarnessExecution,
    NegativeFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheKey {
    pub version: String,
    pub layer: CacheLayer,
    pub key: String,
}

impl CacheKey {
    pub fn exact_request(req: &AiRequest) -> Self {
        let material = ExactRequestFingerprint::from(req);
        let bytes = serde_json::to_vec(&material).unwrap_or_default();
        CacheKey {
            version: CACHE_KEY_VERSION.to_string(),
            layer: CacheLayer::ExactRequest,
            key: format!("{:x}", Sha256::digest(bytes)),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct ExactRequestFingerprint<'a> {
    model: &'a str,
    system: &'a Option<String>,
    messages: &'a [crate::Message],
    tools: &'a [crate::ToolSpec],
    response_format: &'a Option<ResponseFormat>,
    stream: bool,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
    priority: crate::Priority,
    privacy_class: PrivacyClass,
    metadata: BTreeMap<&'a String, &'a String>,
    tenant: &'a Option<String>,
    project: &'a Option<String>,
    passthrough: &'a serde_json::Map<String, crate::Json>,
}

impl<'a> From<&'a AiRequest> for ExactRequestFingerprint<'a> {
    fn from(req: &'a AiRequest) -> Self {
        ExactRequestFingerprint {
            model: &req.model,
            system: &req.system,
            messages: &req.messages,
            tools: &req.tools,
            response_format: &req.response_format,
            stream: req.stream,
            max_output_tokens: req.max_output_tokens,
            temperature: req.temperature,
            priority: req.priority,
            privacy_class: req.privacy_class,
            metadata: req.metadata.iter().collect(),
            tenant: &req.tenant,
            project: &req.project,
            passthrough: &req.passthrough,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachePolicy {
    pub version: String,
    pub layer: CacheLayer,
    pub allow_sensitive: bool,
    pub allow_confidential: bool,
}

impl Default for CachePolicy {
    fn default() -> Self {
        CachePolicy {
            version: CACHE_KEY_VERSION.to_string(),
            layer: CacheLayer::ExactRequest,
            allow_sensitive: false,
            allow_confidential: false,
        }
    }
}

impl CachePolicy {
    pub fn exact_request() -> Self {
        Self::default()
    }

    pub fn eligibility(&self, req: &AiRequest) -> CacheEligibility {
        match req.privacy_class {
            PrivacyClass::Standard => CacheEligibility::Eligible,
            PrivacyClass::Sensitive if self.allow_sensitive => CacheEligibility::Eligible,
            PrivacyClass::Confidential if self.allow_confidential => CacheEligibility::Eligible,
            PrivacyClass::Sensitive => {
                CacheEligibility::Bypass("privacy_class=sensitive".to_string())
            }
            PrivacyClass::Confidential => {
                CacheEligibility::Bypass("privacy_class=confidential".to_string())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CacheEligibility {
    Eligible,
    Bypass(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheStatus {
    Hit,
    Miss,
    Bypass,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CacheLookupReceipt {
    pub layer: CacheLayer,
    pub status: CacheStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub policy_version: String,
}

impl CacheLookupReceipt {
    pub fn for_request(req: &AiRequest, policy: &CachePolicy, cache: &ExactRequestCache) -> Self {
        match policy.eligibility(req) {
            CacheEligibility::Eligible => {
                let key = CacheKey::exact_request(req);
                let status = if cache.contains(&key) {
                    CacheStatus::Hit
                } else {
                    CacheStatus::Miss
                };
                CacheLookupReceipt {
                    layer: key.layer,
                    status,
                    key: Some(key.key),
                    reason: None,
                    policy_version: policy.version.clone(),
                }
            }
            CacheEligibility::Bypass(reason) => CacheLookupReceipt {
                layer: policy.layer,
                status: CacheStatus::Bypass,
                key: None,
                reason: Some(reason),
                policy_version: policy.version.clone(),
            },
        }
    }
}

/// Metadata-only exact-request cache. It remembers fingerprints, never prompts
/// or responses, so it is safe as an MVP cache layer and future hit-rate signal.
#[derive(Debug, Default, Clone)]
pub struct ExactRequestCache {
    entries: BTreeSet<String>,
}

impl ExactRequestCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn contains(&self, key: &CacheKey) -> bool {
        self.entries.contains(&key.key)
    }

    pub fn remember(&mut self, key: CacheKey) {
        self.entries.insert(key.key);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionReceipt {
    pub policy_version: String,
    pub job: ExecutionJob,
    #[serde(default)]
    pub candidates: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_route: Option<String>,
    #[serde(default)]
    pub fallback_path: Vec<String>,
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_latency_ms: Option<f64>,
    pub cache: CacheLookupReceipt,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationEventKind {
    RunStarted,
    RouteSelected,
    CacheLookup,
    ProviderCallStarted,
    ProviderCallFinished,
    HarnessCallStarted,
    HarnessCallFinished,
    FallbackTriggered,
    FinalStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationEvent {
    pub kind: EvaluationEventKind,
    pub policy_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheLookupReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl EvaluationEvent {
    pub fn new(kind: EvaluationEventKind) -> Self {
        EvaluationEvent {
            kind,
            policy_version: EXECUTION_POLICY_VERSION.to_string(),
            target_id: None,
            cache: None,
            status: None,
            latency_ms: None,
            cost_micros: None,
            metadata: BTreeMap::new(),
        }
    }

    pub fn cache_lookup(receipt: CacheLookupReceipt) -> Self {
        let mut event = Self::new(EvaluationEventKind::CacheLookup);
        event.cache = Some(receipt);
        event
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessDescriptor {
    pub name: String,
    pub version: String,
    pub capabilities: HarnessCapabilities,
    #[serde(default)]
    pub supported_task_types: Vec<ExecutionTaskType>,
    #[serde(default)]
    pub required_tools: Vec<String>,
    pub input_contract: String,
    pub output_contract: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessCapabilities {
    pub streaming_events: bool,
    pub artifacts: bool,
    pub tool_logs: bool,
    pub cost_metadata: bool,
    pub latency_metadata: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarnessArtifact {
    pub kind: String,
    pub reference: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HarnessRunStatus {
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarnessRunSummary {
    pub harness: String,
    pub status: HarnessRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    #[serde(default)]
    pub artifacts: Vec<HarnessArtifact>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentPart, Message, Role};

    fn request_with_id(id: &str) -> AiRequest {
        let mut req = AiRequest::new(
            "auto/cheap",
            vec![Message {
                role: Role::User,
                content: vec![ContentPart::text("summarize this")],
            }],
        );
        req.id = id.to_string();
        req.tenant = Some("tenant-a".to_string());
        req.project = Some("project-a".to_string());
        req
    }

    #[test]
    fn exact_request_cache_key_ignores_request_id() {
        let a = request_with_id("req_a");
        let b = request_with_id("req_b");

        assert_eq!(CacheKey::exact_request(&a), CacheKey::exact_request(&b));
    }

    #[test]
    fn exact_request_cache_key_changes_with_material_fields() {
        let a = request_with_id("req_a");
        let mut b = request_with_id("req_b");
        b.model = "auto/fast".to_string();

        assert_ne!(CacheKey::exact_request(&a), CacheKey::exact_request(&b));
    }

    #[test]
    fn sensitive_requests_bypass_cache_by_default() {
        let mut req = request_with_id("req_a");
        req.privacy_class = PrivacyClass::Sensitive;

        let receipt = CacheLookupReceipt::for_request(
            &req,
            &CachePolicy::default(),
            &ExactRequestCache::new(),
        );

        assert_eq!(receipt.status, CacheStatus::Bypass);
        assert_eq!(receipt.key, None);
        assert_eq!(receipt.reason.as_deref(), Some("privacy_class=sensitive"));
    }

    #[test]
    fn exact_request_cache_records_miss_and_hit_without_bodies() {
        let req = request_with_id("req_a");
        let policy = CachePolicy::default();
        let mut cache = ExactRequestCache::new();

        let miss = CacheLookupReceipt::for_request(&req, &policy, &cache);
        assert_eq!(miss.status, CacheStatus::Miss);
        let key = CacheKey::exact_request(&req);
        cache.remember(key);
        let hit = CacheLookupReceipt::for_request(&req, &policy, &cache);
        assert_eq!(hit.status, CacheStatus::Hit);
        assert_eq!(hit.key, miss.key);
    }

    #[test]
    fn harness_descriptor_is_metadata_contract_only() {
        let descriptor = HarnessDescriptor {
            name: "external-codex".to_string(),
            version: "contract/v1".to_string(),
            capabilities: HarnessCapabilities {
                streaming_events: true,
                artifacts: true,
                tool_logs: true,
                cost_metadata: false,
                latency_metadata: true,
            },
            supported_task_types: vec![ExecutionTaskType::Coding],
            required_tools: vec!["shell".to_string()],
            input_contract: "execution-job/v1".to_string(),
            output_contract: "harness-run-summary/v1".to_string(),
        };

        let json = serde_json::to_string(&descriptor).unwrap();
        assert!(json.contains("external-codex"));
        assert!(!json.contains("token"));
        assert!(!json.contains("secret"));
    }
}
