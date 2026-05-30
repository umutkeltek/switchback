use std::collections::HashMap;
use std::sync::Arc;

use sb_adapter::ProviderAdapter;
use sb_core::{CapabilityProfile, Config, ExecutionTarget, ExecutionTargetKind, HealthState, ProviderKind};

use crate::{MockAdapter, OpenAiCompatibleAdapter};

struct ProviderEntry {
    adapter: Arc<dyn ProviderAdapter>,
    kind: ExecutionTargetKind,
}

pub struct AdapterRegistry {
    providers: HashMap<String, ProviderEntry>,
    order: Vec<String>,
}

impl AdapterRegistry {
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let mut providers = HashMap::new();
        let mut order = Vec::new();

        for provider in &cfg.providers {
            if providers.contains_key(&provider.id) {
                return Err(format!("duplicate provider id {}", provider.id));
            }

            let (adapter, kind): (Arc<dyn ProviderAdapter>, ExecutionTargetKind) = match &provider.kind {
                ProviderKind::Mock => (Arc::new(MockAdapter), ExecutionTargetKind::ModelApi),
                ProviderKind::OpenaiCompatible { base_url, .. } => (
                    Arc::new(OpenAiCompatibleAdapter::new(
                        base_url.clone(),
                        CapabilityProfile::default(),
                    )),
                    ExecutionTargetKind::OpenAiCompatibleApi,
                ),
            };

            providers.insert(
                provider.id.clone(),
                ProviderEntry {
                    adapter,
                    kind,
                },
            );
            order.push(provider.id.clone());
        }

        Ok(Self { providers, order })
    }

    pub fn adapter(&self, provider_id: &str) -> Option<Arc<dyn ProviderAdapter>> {
        self.providers.get(provider_id).map(|entry| Arc::clone(&entry.adapter))
    }

    pub fn target_for(&self, target_id: &str) -> Option<ExecutionTarget> {
        let (provider_id, model) = target_id.split_once('/')?;
        let entry = self.providers.get(provider_id)?;

        Some(ExecutionTarget {
            id: target_id.to_string(),
            kind: entry.kind,
            provider_id: provider_id.to_string(),
            model: model.to_string(),
            capabilities: entry.adapter.capabilities(model),
            cost: None,
            policy_tags: Vec::new(),
            health: HealthState::Healthy,
        })
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.order.clone()
    }
}
