//! Switchback's plugin API — tier 1 of the two-tier system (Oracle #6): trusted
//! built-ins as trait objects, zero serialization overhead, run on the hot path.
//! (Tier 2 — sandboxed Wasm — would implement this same [`Plugin`] trait behind a
//! Wasmtime host; tier 3 — dynamic libs — is an internal escape hatch only.)
//!
//! Plugins are compiled into the immutable `CompiledSnapshot` at config-publish
//! time (the runtime builds a [`PluginHost`] from `config.plugins`), so plugin
//! setup is part of publication, never a surprise in the request path.
//!
//! Hooks (the public, first-version set):
//!   - `pre_route`  — inspect / modify / REJECT the canonical request.
//!   - `post_route` — observe the routing decision (read-only).
//!   - `select_egress` — choose a named egress path for an attempt.
//!   - `post_attempt` — observe an attempt outcome.
//!
//! By design plugins never see provider wire formats or raw secrets, and egress
//! plugins pick among NAMED paths rather than mutating socket behaviour.

use std::collections::BTreeMap;
use std::sync::Arc;

use sb_core::{AiRequest, PluginConfig, RouteDecision};

/// What a `pre_route` hook decided.
pub enum PluginOutcome {
    /// Proceed (the request may have been modified in place).
    Continue,
    /// Short-circuit: reject the request with this status + message.
    Reject { status: u16, message: String },
}

/// Read-only view of an attempt outcome handed to `post_attempt`. Metadata only
/// — never secrets or content.
pub struct AttemptInfo<'a> {
    pub request_id: &'a str,
    pub target_id: &'a str,
    pub provider_id: &'a str,
    pub account_id: &'a str,
    pub egress: &'a str,
    pub ok: bool,
    pub error_class: Option<&'a str>,
    pub latency_ms: u64,
}

/// A trusted built-in plugin. Every hook has a no-op default, so a plugin
/// implements only what it needs.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;

    /// Inspect / modify the request before routing; may reject it.
    fn pre_route(&self, _req: &mut AiRequest) -> PluginOutcome {
        PluginOutcome::Continue
    }

    /// Observe the routing decision (read-only).
    fn post_route(&self, _req: &AiRequest, _decision: &RouteDecision) {}

    /// Choose a named egress path for this attempt. `None` = no preference.
    fn select_egress(&self, _req: &AiRequest, _target_id: &str) -> Option<String> {
        None
    }

    /// Observe an attempt outcome (success or failure).
    fn post_attempt(&self, _info: &AttemptInfo) {}
}

/// The ordered set of active plugins. The runtime holds one per snapshot and
/// calls the hooks at the matching points in `execute`.
#[derive(Default, Clone)]
pub struct PluginHost {
    plugins: Arc<Vec<Box<dyn Plugin>>>,
}

impl PluginHost {
    pub fn new(plugins: Vec<Box<dyn Plugin>>) -> Self {
        Self {
            plugins: Arc::new(plugins),
        }
    }

    /// Build the host from config (the publish-time "prepare" step).
    pub fn from_config(configs: &[PluginConfig]) -> Self {
        let plugins = configs.iter().map(build_plugin).collect();
        Self::new(plugins)
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn names(&self) -> Vec<String> {
        self.plugins.iter().map(|p| p.name().to_string()).collect()
    }

    /// Run every `pre_route` in order; the FIRST rejection wins (and stops the
    /// chain). Earlier plugins' mutations are kept.
    pub fn pre_route(&self, req: &mut AiRequest) -> PluginOutcome {
        for plugin in self.plugins.iter() {
            if let PluginOutcome::Reject { status, message } = plugin.pre_route(req) {
                return PluginOutcome::Reject { status, message };
            }
        }
        PluginOutcome::Continue
    }

    pub fn post_route(&self, req: &AiRequest, decision: &RouteDecision) {
        for plugin in self.plugins.iter() {
            plugin.post_route(req, decision);
        }
    }

    /// The first plugin to express an egress preference wins.
    pub fn select_egress(&self, req: &AiRequest, target_id: &str) -> Option<String> {
        self.plugins
            .iter()
            .find_map(|p| p.select_egress(req, target_id))
    }

    pub fn post_attempt(&self, info: &AttemptInfo) {
        for plugin in self.plugins.iter() {
            plugin.post_attempt(info);
        }
    }
}

/// Glob match: exact, or `prefix*` (the only wildcard form, matching the route
/// matcher's style). `*` alone matches everything.
fn glob_match(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

fn build_plugin(config: &PluginConfig) -> Box<dyn Plugin> {
    match config {
        PluginConfig::ModelBlocklist { models } => Box::new(ModelBlocklist {
            models: models.clone(),
        }),
        PluginConfig::RequestTag { tags } => Box::new(RequestTag { tags: tags.clone() }),
        PluginConfig::EgressPin { egress, models } => Box::new(EgressPin {
            egress: egress.clone(),
            models: models.clone(),
        }),
    }
}

// --- Built-ins --------------------------------------------------------------

/// Reject requests whose model matches a blocked pattern (403).
pub struct ModelBlocklist {
    pub models: Vec<String>,
}

impl Plugin for ModelBlocklist {
    fn name(&self) -> &str {
        "model_blocklist"
    }
    fn pre_route(&self, req: &mut AiRequest) -> PluginOutcome {
        if self.models.iter().any(|p| glob_match(p, &req.model)) {
            PluginOutcome::Reject {
                status: 403,
                message: format!("model `{}` is blocked by policy", req.model),
            }
        } else {
            PluginOutcome::Continue
        }
    }
}

/// Inject fixed tags into the request metadata before routing.
pub struct RequestTag {
    pub tags: BTreeMap<String, String>,
}

impl Plugin for RequestTag {
    fn name(&self) -> &str {
        "request_tag"
    }
    fn pre_route(&self, req: &mut AiRequest) -> PluginOutcome {
        for (k, v) in &self.tags {
            req.metadata.entry(k.clone()).or_insert_with(|| v.clone());
        }
        PluginOutcome::Continue
    }
}

/// Pin matching models to a named egress path.
pub struct EgressPin {
    pub egress: String,
    pub models: Vec<String>,
}

impl Plugin for EgressPin {
    fn name(&self) -> &str {
        "egress_pin"
    }
    fn select_egress(&self, req: &AiRequest, _target_id: &str) -> Option<String> {
        if self.models.is_empty() || self.models.iter().any(|p| glob_match(p, &req.model)) {
            Some(self.egress.clone())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::Message;

    fn req(model: &str) -> AiRequest {
        AiRequest::new(model, vec![Message::user("hi")])
    }

    #[test]
    fn model_blocklist_rejects_matching_models() {
        let host = PluginHost::from_config(&[PluginConfig::ModelBlocklist {
            models: vec!["expensive/*".into(), "exact/model".into()],
        }]);
        assert!(matches!(
            host.pre_route(&mut req("expensive/opus")),
            PluginOutcome::Reject { status: 403, .. }
        ));
        assert!(matches!(
            host.pre_route(&mut req("exact/model")),
            PluginOutcome::Reject { .. }
        ));
        assert!(matches!(
            host.pre_route(&mut req("cheap/mini")),
            PluginOutcome::Continue
        ));
    }

    #[test]
    fn request_tag_injects_metadata_without_clobbering() {
        let mut tags = BTreeMap::new();
        tags.insert("source".to_string(), "gateway".to_string());
        let host = PluginHost::from_config(&[PluginConfig::RequestTag { tags }]);
        let mut r = req("m");
        r.metadata.insert("source".into(), "client".into()); // pre-existing wins
        r.metadata.insert("other".into(), "x".into());
        assert!(matches!(host.pre_route(&mut r), PluginOutcome::Continue));
        assert_eq!(r.metadata.get("source").unwrap(), "client");
    }

    #[test]
    fn request_tag_sets_when_absent() {
        let mut tags = BTreeMap::new();
        tags.insert("source".to_string(), "gateway".to_string());
        let host = PluginHost::from_config(&[PluginConfig::RequestTag { tags }]);
        let mut r = req("m");
        host.pre_route(&mut r);
        assert_eq!(r.metadata.get("source").unwrap(), "gateway");
    }

    #[test]
    fn egress_pin_selects_for_matching_models_only() {
        let host = PluginHost::from_config(&[PluginConfig::EgressPin {
            egress: "proxy-eu".into(),
            models: vec!["anthropic/*".into()],
        }]);
        assert_eq!(
            host.select_egress(&req("anthropic/claude"), "anthropic/claude"),
            Some("proxy-eu".to_string())
        );
        assert_eq!(host.select_egress(&req("openai/gpt"), "openai/gpt"), None);
    }

    #[test]
    fn first_reject_wins_and_first_egress_wins() {
        // A blocklist after a tag: tag still applies, then the block rejects.
        let mut tags = BTreeMap::new();
        tags.insert("k".to_string(), "v".to_string());
        let host = PluginHost::from_config(&[
            PluginConfig::RequestTag { tags },
            PluginConfig::ModelBlocklist {
                models: vec!["x/*".into()],
            },
        ]);
        let mut r = req("x/y");
        let outcome = host.pre_route(&mut r);
        assert!(matches!(outcome, PluginOutcome::Reject { .. }));
        assert_eq!(r.metadata.get("k").unwrap(), "v", "earlier mutation kept");
        assert_eq!(host.names(), vec!["request_tag", "model_blocklist"]);
    }
}
