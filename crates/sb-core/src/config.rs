//! YAML config (v1 control plane). Compiled once into an in-memory snapshot;
//! never read in the hot path per-request.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    /// If set, inbound `/v1` requests must present this as `Authorization: Bearer`.
    #[serde(default)]
    pub api_key: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "127.0.0.1:8765".to_string(),
            api_key: None,
        }
    }
}

/// A configured provider. `type:` selects the variant; remaining fields are
/// flattened alongside `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderKind {
    Mock,
    OpenaiCompatible {
        base_url: String,
        /// Read the key from this env var at startup (preferred — never in the file).
        #[serde(default)]
        api_key_env: Option<String>,
        /// Inline key (discouraged; for quick local testing only).
        #[serde(default)]
        api_key: Option<String>,
    },
}

/// How a single account authenticates. Multiple methods coexist across the
/// accounts of one provider (api_key on one, oauth on another).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthConfig {
    /// No credential (local Ollama, mock).
    None,
    /// Bearer API key, read from env (preferred) or inline (discouraged).
    ApiKey {
        #[serde(default)]
        env: Option<String>,
        #[serde(default)]
        inline: Option<String>,
    },
    /// OAuth bearer token (static in v1; `refresh_*` reserved for the live
    /// refresh seam in `sb-credentials::RefreshCoordinator`).
    Oauth {
        #[serde(default)]
        token_env: Option<String>,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        refresh_env: Option<String>,
        #[serde(default)]
        refresh: Option<String>,
    },
}

impl Default for AuthConfig {
    fn default() -> Self {
        AuthConfig::None
    }
}

/// One authenticated account belonging to a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    pub id: String,
    #[serde(default)]
    pub auth: AuthConfig,
    /// Lower = preferred under fill_first.
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub policy_tags: Vec<String>,
}

/// How the resolver picks among a provider's available accounts.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStrategy {
    /// Always the highest-priority available account (default).
    #[default]
    FillFirst,
    /// Rotate least-recently-used, staying on one for `sticky` requests.
    RoundRobin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(flatten)]
    pub kind: ProviderKind,
    /// Explicit multi-account list. If empty, a single default account is
    /// synthesized from the provider kind's legacy `api_key`/`api_key_env`.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub selection: SelectionStrategy,
    /// Round-robin stickiness (consecutive requests before rotating). Default 1.
    #[serde(default)]
    pub sticky: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteMatch {
    /// Glob-ish: `*` matches anything, or an exact route/model name.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteRequire {
    #[serde(default)]
    pub streaming: Option<bool>,
    #[serde(default)]
    pub tool_calling: Option<bool>,
    #[serde(default)]
    pub min_context_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    pub name: String,
    #[serde(default, rename = "match")]
    pub match_: RouteMatch,
    #[serde(default)]
    pub require: RouteRequire,
    /// Ordered candidate list: first is primary, rest are fallbacks.
    pub targets: Vec<String>,
}

impl Config {
    pub fn from_yaml(s: &str) -> Result<Self, crate::CoreError> {
        serde_yaml::from_str(s).map_err(|e| crate::CoreError::Config(e.to_string()))
    }

    pub fn from_path(path: &Path) -> Result<Self, crate::CoreError> {
        let s = std::fs::read_to_string(path)
            .map_err(|e| crate::CoreError::Config(format!("read {}: {e}", path.display())))?;
        Self::from_yaml(&s)
    }

    /// Find the route whose `match.model` equals the requested model, or the
    /// first `*` route as default.
    pub fn route_for<'a>(&'a self, model: &str) -> Option<&'a RouteConfig> {
        self.routes
            .iter()
            .find(|r| r.match_.model.as_deref() == Some(model))
            .or_else(|| {
                self.routes
                    .iter()
                    .find(|r| r.match_.model.as_deref() == Some("*"))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
server:
  bind: "127.0.0.1:9000"
providers:
  - id: mock
    type: mock
  - id: openrouter
    type: openai_compatible
    base_url: "https://openrouter.ai/api/v1"
    api_key_env: "OPENROUTER_API_KEY"
routes:
  - name: default
    match:
      model: "*"
    targets:
      - "mock/echo"
  - name: coding
    match:
      model: "coding"
    require:
      streaming: true
    targets:
      - "openrouter/openai/gpt-4o"
      - "mock/echo"
"#;

    #[test]
    fn parses_sample_config() {
        let cfg = Config::from_yaml(SAMPLE).expect("parse");
        assert_eq!(cfg.server.bind, "127.0.0.1:9000");
        assert_eq!(cfg.providers.len(), 2);
        match &cfg.providers[1].kind {
            ProviderKind::OpenaiCompatible {
                base_url,
                api_key_env,
                ..
            } => {
                assert_eq!(base_url, "https://openrouter.ai/api/v1");
                assert_eq!(api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
            }
            _ => panic!("expected openai_compatible"),
        }
    }

    #[test]
    fn route_lookup_falls_back_to_star() {
        let cfg = Config::from_yaml(SAMPLE).unwrap();
        assert_eq!(cfg.route_for("coding").unwrap().name, "coding");
        // unknown model -> default `*` route
        assert_eq!(cfg.route_for("anything/else").unwrap().name, "default");
    }

    const MULTI_ACCOUNT: &str = r#"
server:
  bind: "127.0.0.1:9000"
providers:
  - id: openrouter
    type: openai_compatible
    base_url: "https://openrouter.ai/api/v1"
    selection: round_robin
    sticky: 3
    accounts:
      - id: personal
        auth: { kind: api_key, env: OR_PERSONAL }
        priority: 0
      - id: work
        auth: { kind: api_key, env: OR_WORK }
        priority: 1
      - id: oauth
        auth: { kind: oauth, token_env: OR_OAUTH }
        priority: 2
"#;

    #[test]
    fn parses_multi_account_provider() {
        let cfg = Config::from_yaml(MULTI_ACCOUNT).expect("parse");
        let p = &cfg.providers[0];
        assert_eq!(p.selection, SelectionStrategy::RoundRobin);
        assert_eq!(p.sticky, Some(3));
        assert_eq!(p.accounts.len(), 3);
        assert_eq!(p.accounts[0].id, "personal");
        match &p.accounts[2].auth {
            AuthConfig::Oauth { token_env, .. } => {
                assert_eq!(token_env.as_deref(), Some("OR_OAUTH"))
            }
            _ => panic!("expected oauth"),
        }
    }
}
