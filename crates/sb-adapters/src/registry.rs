use std::collections::HashMap;
use std::sync::Arc;

use sb_adapter::ProviderAdapter;
use sb_core::{
    ApiKind, AuthScheme, Catalog, Config, CostProfile, ExecutionTarget, ExecutionTargetKind,
    HealthState, ProviderKind,
};
use serde::Deserialize;

use crate::{AnthropicCodec, ComposedAdapter, GeminiCodec, MockAdapter, OpenAiCodec, VertexCodec};

struct ProviderEntry {
    adapter: Arc<dyn ProviderAdapter>,
    kind: ExecutionTargetKind,
}

/// Which wire family a configured provider speaks — drives the default
/// capability profile when the catalog has no per-model entry.
fn api_kind_of(kind: &ProviderKind) -> ApiKind {
    match kind {
        ProviderKind::Mock => ApiKind::Mock,
        ProviderKind::OpenaiCompatible { .. } => ApiKind::OpenAiCompatible,
        // Bedrock (Claude) speaks the Anthropic wire.
        ProviderKind::Anthropic { .. } | ProviderKind::Bedrock { .. } => ApiKind::Anthropic,
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
            let caps = api_kind_of(&provider.kind).default_capabilities();

            // Every real provider is now `ComposedAdapter(WireCodec × AuthScheme)`
            // — a wire codec composed with how it authenticates. New providers
            // that reuse a wire format are data here, not a new adapter.
            let (adapter, kind): (Arc<dyn ProviderAdapter>, ExecutionTargetKind) =
                match &provider.kind {
                    ProviderKind::Mock => (Arc::new(MockAdapter), ExecutionTargetKind::ModelApi),
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
                        )),
                        ExecutionTargetKind::OpenAiCompatibleApi,
                    ),
                    ProviderKind::Anthropic { base_url, .. } => (
                        Arc::new(ComposedAdapter::with_scheme(
                            Box::new(AnthropicCodec),
                            AuthScheme::Header {
                                name: "x-api-key".to_string(),
                            },
                            base_url.clone(),
                            caps,
                            egress.clone(),
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
                            )),
                            ExecutionTargetKind::ModelApi,
                        )
                    }
                    ProviderKind::Bedrock {
                        region,
                        access_key_env,
                        secret_key_env,
                        session_token_env,
                        base_url,
                    } => {
                        // SigV4 creds resolve from env at startup (fail-fast).
                        let access_key_id = std::env::var(access_key_env).map_err(|_| {
                            format!(
                                "provider {}: AWS access key env `{access_key_env}` not set",
                                provider.id
                            )
                        })?;
                        let secret_access_key = std::env::var(secret_key_env).map_err(|_| {
                            format!(
                                "provider {}: AWS secret key env `{secret_key_env}` not set",
                                provider.id
                            )
                        })?;
                        let session_token =
                            session_token_env.as_ref().and_then(|n| std::env::var(n).ok());
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
                                    creds: crate::sigv4::AwsCredentials {
                                        access_key_id,
                                        secret_access_key,
                                        session_token,
                                    },
                                    region: region.clone(),
                                    service: "bedrock".to_string(),
                                }),
                                Box::new(crate::EventStreamTransport),
                                base,
                                caps,
                                egress.clone(),
                            )),
                            ExecutionTargetKind::ModelApi,
                        )
                    }
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
        let capabilities = self
            .catalog
            .models
            .iter()
            .find(|m| m.id == model && m.provider_id == provider_id)
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
            health: HealthState::Healthy,
            // Stamped later by the runtime from the non-secret account-pool view.
            healthy_accounts: None,
            unverified: false,
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
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read cost_map `{path}`: {e}"))?;
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
