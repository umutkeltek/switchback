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
        ProviderKind::Anthropic { .. } => ApiKind::Anthropic,
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
    /// Per-`provider/model` blended price index from `server.cost_map`, used to
    /// stamp `ExecutionTarget.cost` so cost-aware routing can sort by it. Empty
    /// when no cost map is configured.
    cost_index: HashMap<String, CostProfile>,
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
                        Arc::new(ComposedAdapter::new(
                            Box::new(OpenAiCodec),
                            auth_scheme.clone().unwrap_or_default(),
                            base_url.clone(),
                            caps,
                            egress.clone(),
                        )),
                        ExecutionTargetKind::OpenAiCompatibleApi,
                    ),
                    ProviderKind::Anthropic { base_url, .. } => (
                        Arc::new(ComposedAdapter::new(
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
                        Arc::new(ComposedAdapter::new(
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
                            Arc::new(ComposedAdapter::new(
                                Box::new(VertexCodec::new(project.clone(), region.clone())),
                                AuthScheme::Bearer, // OAuth access token
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
        })
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
                .copied(),
            policy_tags: Vec::new(),
            health: HealthState::Healthy,
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

#[derive(Deserialize)]
struct CostMapFile {
    #[serde(default)]
    models: Vec<CostMapEntry>,
    #[serde(default)]
    spread: Vec<CostMapSpread>,
}

#[derive(Deserialize)]
struct CostMapEntry {
    provider_id: String,
    model_id: String,
    #[serde(default)]
    input_micros_per_mtok: Option<u64>,
    #[serde(default)]
    output_micros_per_mtok: Option<u64>,
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
}

fn load_cost_index(path: &str) -> Result<HashMap<String, CostProfile>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read cost_map `{path}`: {e}"))?;
    let file: CostMapFile =
        serde_json::from_str(&text).map_err(|e| format!("parse cost_map `{path}`: {e}"))?;

    let to_usd = |micros: u64| micros as f64 / 1_000_000.0;
    let mut index = HashMap::new();

    // Direct per-model prices win (inserted first); spread hosts fill the rest.
    for entry in &file.models {
        if let (Some(input), Some(output)) =
            (entry.input_micros_per_mtok, entry.output_micros_per_mtok)
        {
            index.insert(
                format!("{}/{}", entry.provider_id, entry.model_id),
                CostProfile {
                    input_per_mtok: to_usd(input),
                    output_per_mtok: to_usd(output),
                },
            );
        }
    }
    for spread in &file.spread {
        for host in &spread.hosts {
            if let (Some(input), Some(output)) =
                (host.input_micros_per_mtok, host.output_micros_per_mtok)
            {
                index
                    .entry(format!("{}/{}", host.provider_id, spread.model))
                    .or_insert(CostProfile {
                        input_per_mtok: to_usd(input),
                        output_per_mtok: to_usd(output),
                    });
            }
        }
    }

    Ok(index)
}
