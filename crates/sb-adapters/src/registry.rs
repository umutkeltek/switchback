use std::collections::HashMap;
use std::sync::Arc;

use sb_adapter::ProviderAdapter;
use sb_core::{
    ApiKind, AuthScheme, Catalog, Config, ExecutionTarget, ExecutionTargetKind, HealthState,
    ProviderKind,
};

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

            // Every real provider is now `ComposedAdapter(WireCodec × AuthScheme)`
            // — a wire codec composed with how it authenticates. New providers
            // that reuse a wire format are data here, not a new adapter.
            let timeouts = cfg.server.timeouts;
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
                            timeouts,
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
                            timeouts,
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
                            timeouts,
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
                                timeouts,
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
        })
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
            cost: None,
            policy_tags: Vec::new(),
            health: HealthState::Healthy,
        })
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.order.clone()
    }
}
