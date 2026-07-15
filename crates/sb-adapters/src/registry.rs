use std::collections::HashMap;
use std::sync::Arc;

use sb_adapter::ProviderAdapter;
use sb_core::{
    ApiKind, AuthScheme, Catalog, Config, CostProfile, ExecutionTarget, ExecutionTargetKind,
    HealthState, ProviderKind, ServerToolProtocol, Usage,
};
use serde::Deserialize;

use crate::{
    AnthropicCodec, ClaudeCodeNativeRelayCodec, ComposedAdapter, GeminiCodec, MockAdapter,
    OpenAiCodec, OpenAiResponsesCodec, VertexCodec,
};

struct ProviderEntry {
    adapter: Arc<dyn ProviderAdapter>,
    kind: ExecutionTargetKind,
}

/// Which wire family a configured provider speaks — drives the default
/// capability profile when the catalog has no per-model entry.
fn api_kind_of(kind: &ProviderKind) -> ApiKind {
    match kind {
        ProviderKind::Mock | ProviderKind::ComfyUi { .. } | ProviderKind::Fal { .. } => {
            ApiKind::Mock
        }
        ProviderKind::OpenaiCompatible { .. } | ProviderKind::CodexNativeRelay { .. } => {
            ApiKind::OpenAiCompatible
        }
        // Bedrock (Claude) speaks the Anthropic wire.
        ProviderKind::Anthropic { .. }
        | ProviderKind::Bedrock { .. }
        | ProviderKind::ClaudeCodeNativeRelay { .. } => ApiKind::Anthropic,
        // Vertex speaks the Gemini wire, so it shares Gemini's capabilities.
        ProviderKind::Gemini { .. } | ProviderKind::Vertex { .. } => ApiKind::Gemini,
    }
}

pub struct AdapterRegistry {
    providers: HashMap<String, ProviderEntry>,
    order: Vec<String>,
    /// Per-model capability/context facts (§13.3 seam). When a model is listed
    /// here it is authoritative for routing; otherwise the per-api-kind default
    /// applies. Empty when no `catalog:` is configured.
    catalog: Catalog,
    /// Shared per-egress HTTP clients (every ComposedAdapter holds an Arc to it).
    egress: Arc<crate::egress::EgressPool>,
    /// Per-`provider/model` price + policy-tag index from `server.cost_map`,
    /// used to stamp `ExecutionTarget.cost` (cost-aware sort) and `policy_tags`
    /// (free/promo/aggregator gating). Empty when no cost map is configured.
    cost_index: HashMap<String, CostEntry>,
    /// Live per-`provider/model` latency EWMA, fed by the server after each
    /// attempt; stamped onto targets for latency-aware routing.
    latency: crate::latency::LatencyTracker,
}

impl AdapterRegistry {
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let mut providers = HashMap::new();
        let mut order = Vec::new();
        // One pool of outbound clients for the whole registry; each adapter
        // selects a client from it per attempt by the resolved egress id.
        let egress = Arc::new(crate::egress::EgressPool::from_config(cfg)?);

        // Price index for cost-aware routing (empty unless a cost map is set).
        let cost_index = match &cfg.server.cost_map {
            Some(path) => load_cost_index(path)?,
            None => HashMap::new(),
        };

        for provider in &cfg.providers {
            if providers.contains_key(&provider.id) {
                return Err(format!("duplicate provider id {}", provider.id));
            }

            // Adapters now declare realistic per-api-kind capabilities (e.g.
            // Gemini can't do native JSON-Schema) instead of "everything true",
            // so the router's hard filter is meaningful even without a catalog.
            let mut caps = api_kind_of(&provider.kind).default_capabilities();

            // Native-relay targets are first-party passthrough to the user's own
            // subscription backend (ChatGPT Codex / Claude Code), which is a
            // current multimodal model. The generic OpenAI-compatible default is
            // conservative (no vision) because arbitrary OpenAI-shaped providers
            // may not support images — but the native relay always does, so opt
            // it in explicitly rather than rejecting screenshots at the router.
            if matches!(
                provider.kind,
                ProviderKind::CodexNativeRelay { .. } | ProviderKind::ClaudeCodeNativeRelay { .. }
            ) {
                caps.vision_in = true;
            }
            match provider.kind {
                ProviderKind::CodexNativeRelay { .. } => {
                    caps.server_tools = true;
                    caps.server_tool_protocols
                        .push(ServerToolProtocol::OpenAiResponses);
                    caps.image_out = true;
                    caps.reasoning_summary = true;
                }
                ProviderKind::Anthropic { .. } | ProviderKind::ClaudeCodeNativeRelay { .. } => {
                    caps.server_tools = true;
                    caps.server_tool_protocols
                        .push(ServerToolProtocol::Anthropic);
                    caps.reasoning_summary = true;
                }
                _ => {}
            }

            // Operator capability overrides are the final word: they win over
            // both the api-kind default and the native-relay derived caps, so an
            // operator can correct a conservative default (e.g. opt an OpenAI-
            // compatible vLLM deployment into vision) without a catalog entry +
            // provider FK. A catalog model row, when present, still overrides
            // per-model in `target_for_provider_model`.
            provider.capabilities.apply_to(&mut caps);

            // Every real provider is now `ComposedAdapter(WireCodec × AuthScheme)`
            // — a wire codec composed with how it authenticates. New providers
            // that reuse a wire format are data here, not a new adapter.
            let (adapter, kind): (Arc<dyn ProviderAdapter>, ExecutionTargetKind) =
                match &provider.kind {
                    ProviderKind::Mock => (Arc::new(MockAdapter), ExecutionTargetKind::ModelApi),
                    ProviderKind::ComfyUi { .. } | ProviderKind::Fal { .. } => {
                        // Media providers are configured as workload executors. They must not
                        // enter the text/model adapter registry, but config loading
                        // should still succeed so the workload surface can use it.
                        continue;
                    }
                    ProviderKind::OpenaiCompatible {
                        base_url,
                        auth_scheme,
                        ..
                    } => (
                        Arc::new(ComposedAdapter::with_scheme(
                            Box::new(OpenAiCodec),
                            auth_scheme.clone().unwrap_or_default(),
                            base_url.clone(),
                            caps,
                            egress.clone(),
                            cfg.server.block_private_networks,
                        )),
                        ExecutionTargetKind::OpenAiCompatibleApi,
                    ),
                    ProviderKind::Anthropic {
                        base_url,
                        auth_scheme,
                        ..
                    } => (
                        Arc::new(ComposedAdapter::with_scheme(
                            Box::new(AnthropicCodec),
                            auth_scheme.clone().unwrap_or_else(|| AuthScheme::Header {
                                name: "x-api-key".to_string(),
                            }),
                            base_url.clone(),
                            caps,
                            egress.clone(),
                            cfg.server.block_private_networks,
                        )),
                        ExecutionTargetKind::ModelApi,
                    ),
                    ProviderKind::Gemini { base_url, .. } => (
                        Arc::new(ComposedAdapter::with_scheme(
                            Box::new(GeminiCodec),
                            AuthScheme::Header {
                                name: "x-goog-api-key".to_string(),
                            },
                            base_url.clone(),
                            caps,
                            egress.clone(),
                            cfg.server.block_private_networks,
                        )),
                        ExecutionTargetKind::ModelApi,
                    ),
                    ProviderKind::Vertex {
                        project,
                        region,
                        base_url,
                        ..
                    } => {
                        let base = base_url.clone().unwrap_or_else(|| {
                            format!("https://{region}-aiplatform.googleapis.com")
                        });
                        (
                            Arc::new(ComposedAdapter::with_scheme(
                                Box::new(VertexCodec::new(project.clone(), region.clone())),
                                AuthScheme::Bearer, // OAuth access token
                                base,
                                caps,
                                egress.clone(),
                                cfg.server.block_private_networks,
                            )),
                            ExecutionTargetKind::ModelApi,
                        )
                    }
                    ProviderKind::Bedrock {
                        region, base_url, ..
                    } => {
                        let base = base_url.clone().unwrap_or_else(|| {
                            format!("https://bedrock-runtime.{region}.amazonaws.com")
                        });
                        // Bedrock now rides the one ComposedAdapter loop too:
                        // the Bedrock codec (Anthropic wire) × a SigV4 signer ×
                        // the AWS event-stream transport. No bespoke adapter.
                        (
                            Arc::new(ComposedAdapter::new(
                                Box::new(crate::BedrockCodec),
                                Box::new(crate::SigV4Signer {
                                    region: region.clone(),
                                    service: "bedrock".to_string(),
                                }),
                                Box::new(crate::EventStreamTransport),
                                base,
                                caps,
                                egress.clone(),
                                cfg.server.block_private_networks,
                            )),
                            ExecutionTargetKind::ModelApi,
                        )
                    }
                    ProviderKind::CodexNativeRelay { base_url } => (
                        Arc::new(ComposedAdapter::new(
                            Box::new(OpenAiResponsesCodec::codex_native_relay()),
                            Box::new(crate::CodexNativeSigner),
                            Box::new(crate::HttpTransport),
                            base_url.clone().unwrap_or_else(|| {
                                "https://chatgpt.com/backend-api/codex".to_string()
                            }),
                            caps,
                            egress.clone(),
                            cfg.server.block_private_networks,
                        )),
                        ExecutionTargetKind::ModelApi,
                    ),
                    ProviderKind::ClaudeCodeNativeRelay { base_url } => (
                        Arc::new(ComposedAdapter::with_scheme(
                            Box::new(ClaudeCodeNativeRelayCodec),
                            AuthScheme::Bearer,
                            base_url
                                .clone()
                                .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
                            caps,
                            egress.clone(),
                            cfg.server.block_private_networks,
                        )),
                        ExecutionTargetKind::ModelApi,
                    ),
                };

            providers.insert(provider.id.clone(), ProviderEntry { adapter, kind });
            order.push(provider.id.clone());
        }

        Ok(Self {
            providers,
            order,
            catalog: cfg.catalog.clone().unwrap_or_default(),
            egress,
            cost_index,
            latency: crate::latency::LatencyTracker::new(),
        })
    }

    /// Fold a successful attempt's latency into the per-`provider/model` EWMA
    /// (called by the server) so later routing can prefer the fastest host.
    pub fn record_latency(&self, provider_id: &str, model: &str, latency_ms: f64) {
        self.latency.record(provider_id, model, latency_ms);
    }

    /// Attributed cost (micro-USD) of `usage` for `provider/model` — the ONE cost
    /// function used by both the router (for ordering) and the ledger/trace (audit
    /// #5), so a request's route decision and its recorded cost can't disagree.
    /// Precedence: the router's `server.cost_map` index first (cached-input at the
    /// input rate, reasoning at the output rate), then the typed `catalog` as a
    /// fallback. A model in neither contributes 0 (raw usage still recorded).
    pub fn cost_micros(&self, provider_id: &str, model: &str, usage: &Usage) -> u64 {
        if let Some(entry) = self.cost_index.get(&format!("{provider_id}/{model}")) {
            let input =
                (usage.input_tokens + usage.cached_input_tokens) as f64 * entry.cost.input_per_mtok;
            let output =
                (usage.output_tokens + usage.reasoning_tokens) as f64 * entry.cost.output_per_mtok;
            return (input + output).round().max(0.0) as u64;
        }
        // Fallback: the typed price catalog (the other configured source).
        let per = |kind: sb_core::TokenKind, tokens: u64| {
            self.catalog
                .current_price(model, kind)
                .map(|p| p.unit_price_micros_per_mtok.saturating_mul(tokens) / 1_000_000)
                .unwrap_or(0)
        };
        per(sb_core::TokenKind::Input, usage.input_tokens)
            .saturating_add(per(sb_core::TokenKind::Output, usage.output_tokens))
            .saturating_add(per(
                sb_core::TokenKind::CachedInput,
                usage.cached_input_tokens,
            ))
            .saturating_add(per(sb_core::TokenKind::Reasoning, usage.reasoning_tokens))
    }

    /// Current input/output price for one concrete target. This follows the
    /// same source precedence as [`Self::cost_micros`] and lets callers fail
    /// closed before dispatch when price is unknown.
    pub fn cost_profile(&self, provider_id: &str, model: &str) -> Option<CostProfile> {
        if let Some(entry) = self.cost_index.get(&format!("{provider_id}/{model}")) {
            return Some(entry.cost);
        }
        if !self
            .catalog
            .models
            .iter()
            .any(|candidate| candidate.provider_id == provider_id && candidate.id == model)
        {
            return None;
        }
        let input = self
            .catalog
            .current_price(model, sb_core::TokenKind::Input)?
            .unit_price_micros_per_mtok as f64
            / 1_000_000.0;
        let output = self
            .catalog
            .current_price(model, sb_core::TokenKind::Output)?
            .unit_price_micros_per_mtok as f64
            / 1_000_000.0;
        Some(CostProfile {
            input_per_mtok: input,
            output_per_mtok: output,
        })
    }

    /// Fold a streamed attempt's time-to-first-token into the per-`provider/model`
    /// TTFT EWMA, so interactive requests can rank on first-byte responsiveness.
    pub fn record_ttft(&self, provider_id: &str, model: &str, ttft_ms: f64) {
        self.latency.record_ttft(provider_id, model, ttft_ms);
    }

    /// The egress id that will actually be used for `egress_id` — `"direct"`
    /// when it's unknown, disabled, or the master switch is off. The server
    /// records this in the trace so it reflects what really happened.
    pub fn effective_egress(&self, egress_id: Option<&str>) -> String {
        self.egress.effective(egress_id).to_string()
    }

    pub fn adapter(&self, provider_id: &str) -> Option<Arc<dyn ProviderAdapter>> {
        self.providers
            .get(provider_id)
            .map(|entry| Arc::clone(&entry.adapter))
    }

    pub fn target_for(&self, target_id: &str) -> Option<ExecutionTarget> {
        let (provider_id, model) = target_id.split_once('/')?;
        self.target_for_provider_model(provider_id, model)
    }

    /// Build a target from an explicit `(provider, model)`. Unlike `target_for`
    /// this does not split on `/`, so the model may itself contain slashes
    /// (e.g. OpenRouter `author/model` ids) — used for default-provider
    /// pass-through of an arbitrary requested model.
    pub fn target_for_provider_model(
        &self,
        provider_id: &str,
        model: &str,
    ) -> Option<ExecutionTarget> {
        let entry = self.providers.get(provider_id)?;

        // Capability source: a catalog model entry (authoritative, per-model)
        // wins; otherwise the adapter's per-api-kind default.
        let catalog_model = self
            .catalog
            .models
            .iter()
            .find(|m| m.id == model && m.provider_id == provider_id);
        let capabilities = catalog_model
            .map(|m| m.capability_profile())
            .unwrap_or_else(|| entry.adapter.capabilities(model));

        Some(ExecutionTarget {
            id: format!("{provider_id}/{model}"),
            kind: entry.kind,
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            capabilities,
            cost: self
                .cost_index
                .get(&format!("{provider_id}/{model}"))
                .map(|e| e.cost),
            latency_ewma_ms: self.latency.get(provider_id, model),
            ttft_ewma_ms: self.latency.get_ttft(provider_id, model),
            policy_tags: self
                .cost_index
                .get(&format!("{provider_id}/{model}"))
                .map(|e| e.tags.clone())
                .unwrap_or_default(),
            task_tags: catalog_model.map(|m| m.tags.clone()).unwrap_or_default(),
            health: HealthState::Healthy,
            // Stamped later by the runtime from the non-secret account-pool view.
            healthy_accounts: None,
            unverified: false,
            // Stamped later by the runtime from the scorecard projection.
            outcome: None,
            // Stamped later by the runtime from the quality projection.
            quality: None,
        })
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.order.clone()
    }
}

// --- Cost map loading ------------------------------------------------------
// Reads a cost map JSON (e.g. config/provider-registry.json) into a per-
// `provider/model` blended price index. Money in the file is integer micro-USD
// per Mtok; we store USD/Mtok floats (CostProfile). Unknown JSON fields are
// ignored, so the rich registry file is consumed as-is.

/// A cost-map entry: blended price plus routing policy tags (`free`/`promo`/
/// `aggregator`) so cost-aware routing can gate those lanes.
#[derive(Clone)]
struct CostEntry {
    cost: CostProfile,
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct CostMapFile {
    #[serde(default)]
    providers: Vec<CostMapProvider>,
    #[serde(default)]
    models: Vec<CostMapEntry>,
    #[serde(default)]
    spread: Vec<CostMapSpread>,
}

#[derive(Deserialize)]
struct CostMapProvider {
    id: String,
    #[serde(default)]
    aggregator: bool,
}

#[derive(Deserialize)]
struct CostMapEntry {
    provider_id: String,
    model_id: String,
    #[serde(default)]
    input_micros_per_mtok: Option<u64>,
    #[serde(default)]
    output_micros_per_mtok: Option<u64>,
    /// Present = a time-boxed promotional price.
    #[serde(default)]
    effective_to: Option<String>,
}

#[derive(Deserialize)]
struct CostMapSpread {
    model: String,
    #[serde(default)]
    hosts: Vec<CostMapHost>,
}

#[derive(Deserialize)]
struct CostMapHost {
    provider_id: String,
    #[serde(default)]
    input_micros_per_mtok: Option<u64>,
    #[serde(default)]
    output_micros_per_mtok: Option<u64>,
    #[serde(default)]
    note: Option<String>,
}

fn policy_tags(input: u64, output: u64, promo: bool, aggregator: bool) -> Vec<String> {
    let mut tags = Vec::new();
    if input == 0 && output == 0 {
        tags.push("free".to_string());
    }
    if promo {
        tags.push("promo".to_string());
    }
    if aggregator {
        tags.push("aggregator".to_string());
    }
    tags
}

fn load_cost_index(path: &str) -> Result<HashMap<String, CostEntry>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read cost_map `{path}`: {e}"))?;
    let file: CostMapFile =
        serde_json::from_str(&text).map_err(|e| format!("parse cost_map `{path}`: {e}"))?;

    let aggregators: std::collections::HashSet<&str> = file
        .providers
        .iter()
        .filter(|p| p.aggregator)
        .map(|p| p.id.as_str())
        .collect();
    let to_usd = |micros: u64| micros as f64 / 1_000_000.0;
    let mut index = HashMap::new();

    // Direct per-model prices win (inserted first); spread hosts fill the rest.
    for entry in &file.models {
        if let (Some(input), Some(output)) =
            (entry.input_micros_per_mtok, entry.output_micros_per_mtok)
        {
            index.insert(
                format!("{}/{}", entry.provider_id, entry.model_id),
                CostEntry {
                    cost: CostProfile {
                        input_per_mtok: to_usd(input),
                        output_per_mtok: to_usd(output),
                    },
                    tags: policy_tags(
                        input,
                        output,
                        entry.effective_to.is_some(),
                        aggregators.contains(entry.provider_id.as_str()),
                    ),
                },
            );
        }
    }
    for spread in &file.spread {
        for host in &spread.hosts {
            if let (Some(input), Some(output)) =
                (host.input_micros_per_mtok, host.output_micros_per_mtok)
            {
                let promo = host
                    .note
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains("promo"));
                index
                    .entry(format!("{}/{}", host.provider_id, spread.model))
                    .or_insert(CostEntry {
                        cost: CostProfile {
                            input_per_mtok: to_usd(input),
                            output_per_mtok: to_usd(output),
                        },
                        tags: policy_tags(
                            input,
                            output,
                            promo,
                            aggregators.contains(host.provider_id.as_str()),
                        ),
                    });
            }
        }
    }

    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_native_relay_target_advertises_vision() {
        // A native-relay provider has no catalog entry, so capabilities fall to
        // the api-kind default (OpenAI-compatible = no vision). The first-party
        // passthrough override must opt vision back in so screenshots route.
        let cfg: sb_core::Config = serde_json::from_value(serde_json::json!({
            "providers": [{
                "id": "codex-relay",
                "type": "codex_native_relay",
                "accounts": [{ "id": "codex-native", "auth": { "kind": "codex_oauth" } }]
            }]
        }))
        .expect("config parses");

        let registry = AdapterRegistry::from_config(&cfg).expect("registry builds");
        let target = registry
            .target_for("codex-relay/gpt-5.5")
            .expect("native-relay target");
        assert!(
            target.capabilities.vision_in,
            "codex native relay must advertise vision input"
        );
    }

    #[test]
    fn openai_compatible_capability_override_opts_into_vision() {
        // An OpenAI-compatible provider (e.g. a local vLLM box serving a VL
        // model) defaults to no vision — conservative/fail-closed. The per-
        // provider `capabilities` override is the lightweight way to opt in
        // without a full catalog entry + provider FK.
        let cfg: sb_core::Config = serde_json::from_value(serde_json::json!({
            "providers": [{
                "id": "vllm-primary",
                "type": "openai_compatible",
                "base_url": "http://127.0.0.1:8000/v1",
                "capabilities": { "vision_in": true, "max_context_tokens": 256000 }
            }]
        }))
        .expect("config parses");

        let registry = AdapterRegistry::from_config(&cfg).expect("registry builds");
        let target = registry
            .target_for_provider_model("vllm-primary", "Qwen/Qwen3-VL-30B-A3B-Instruct")
            .expect("openai-compatible target");
        assert!(
            target.capabilities.vision_in,
            "capability override must opt the provider into vision input"
        );
        assert_eq!(target.capabilities.max_context_tokens, Some(256000));
        // Override only widens what's asserted: the generous OpenAI-compatible
        // defaults (json_schema/tool_calling) are left intact.
        assert!(target.capabilities.json_schema);
        assert!(target.capabilities.tool_calling);
    }

    #[test]
    fn openai_compatible_without_override_stays_text_only() {
        // Control: no override → the conservative api-kind default holds, so the
        // router still fails closed on vision for an arbitrary OpenAI endpoint.
        let cfg: sb_core::Config = serde_json::from_value(serde_json::json!({
            "providers": [{
                "id": "vllm-text",
                "type": "openai_compatible",
                "base_url": "http://127.0.0.1:8000/v1"
            }]
        }))
        .expect("config parses");

        let registry = AdapterRegistry::from_config(&cfg).expect("registry builds");
        let target = registry
            .target_for_provider_model("vllm-text", "some-text-model")
            .expect("openai-compatible target");
        assert!(
            !target.capabilities.vision_in,
            "without an override an OpenAI-compatible provider must default to no vision"
        );
    }

    #[test]
    fn comfyui_provider_is_not_registered_as_text_adapter() {
        let cfg: sb_core::Config = serde_json::from_value(serde_json::json!({
            "providers": [{
                "id": "comfy-local",
                "type": "comfyui",
                "base_url": "http://127.0.0.1:8188"
            }]
        }))
        .expect("config parses");

        let registry =
            AdapterRegistry::from_config(&cfg).expect("registry skips workflow provider");
        assert!(registry.adapter("comfy-local").is_none());
        assert!(registry
            .target_for_provider_model("comfy-local", "default")
            .is_none());
    }
}
