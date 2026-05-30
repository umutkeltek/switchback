use std::collections::HashMap;
use std::sync::Arc;

use sb_adapter::ProviderAdapter;
use sb_core::{
    ApiKind, Catalog, Config, ExecutionTarget, ExecutionTargetKind, HealthState, ProviderKind,
};

use crate::{AnthropicAdapter, GeminiAdapter, MockAdapter, OpenAiCompatibleAdapter};

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
        ProviderKind::Gemini { .. } => ApiKind::Gemini,
    }
}

pub struct AdapterRegistry {
    providers: HashMap<String, ProviderEntry>,
    order: Vec<String>,
    /// Per-model capability/context facts (§13.3 seam). When a model is listed
    /// here it is authoritative for routing; otherwise the per-api-kind default
    /// applies. Empty when no `catalog:` is configured.
    catalog: Catalog,
}

impl AdapterRegistry {
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let mut providers = HashMap::new();
        let mut order = Vec::new();

        for provider in &cfg.providers {
            if providers.contains_key(&provider.id) {
                return Err(format!("duplicate provider id {}", provider.id));
            }

            // Adapters now declare realistic per-api-kind capabilities (e.g.
            // Gemini can't do native JSON-Schema) instead of "everything true",
            // so the router's hard filter is meaningful even without a catalog.
            let caps = api_kind_of(&provider.kind).default_capabilities();

            let (adapter, kind): (Arc<dyn ProviderAdapter>, ExecutionTargetKind) =
                match &provider.kind {
                    ProviderKind::Mock => (Arc::new(MockAdapter), ExecutionTargetKind::ModelApi),
                    ProviderKind::OpenaiCompatible { base_url, .. } => (
                        Arc::new(OpenAiCompatibleAdapter::new(
                            base_url.clone(),
                            caps,
                            cfg.server.timeouts,
                        )),
                        ExecutionTargetKind::OpenAiCompatibleApi,
                    ),
                    ProviderKind::Anthropic { base_url, .. } => (
                        Arc::new(AnthropicAdapter::new(
                            base_url.clone(),
                            caps,
                            cfg.server.timeouts,
                        )),
                        ExecutionTargetKind::ModelApi,
                    ),
                    ProviderKind::Gemini { base_url, .. } => (
                        Arc::new(GeminiAdapter::new(
                            base_url.clone(),
                            caps,
                            cfg.server.timeouts,
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
        })
    }

    pub fn adapter(&self, provider_id: &str) -> Option<Arc<dyn ProviderAdapter>> {
        self.providers
            .get(provider_id)
            .map(|entry| Arc::clone(&entry.adapter))
    }

    pub fn target_for(&self, target_id: &str) -> Option<ExecutionTarget> {
        let (provider_id, model) = target_id.split_once('/')?;
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
            id: target_id.to_string(),
            kind: entry.kind,
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            capabilities,
            cost: None,
            policy_tags: Vec::new(),
            health: HealthState::Healthy,
        })
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.order.clone()
    }
}
