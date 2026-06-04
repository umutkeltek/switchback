//! YAML config (v1 control plane). Compiled once into an in-memory snapshot;
//! never read in the hot path per-request.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    /// Optional encrypted credential vault (age file + OS-keychain key). When
    /// present, accounts may reference secrets by name (`auth.vault`).
    #[serde(default)]
    pub vault: Option<VaultConfig>,
    /// Optional typed catalog (§13.3 seams: provider/model/account/credential/
    /// price entities). v1 is seams-not-machinery — carried + validated, the
    /// hot path still runs off `providers`/`routes` above.
    #[serde(default)]
    pub catalog: Option<crate::catalog::Catalog>,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    /// Native client compatibility profiles. These do not own secrets; they
    /// describe how clients such as Codex or Claude Code should point at
    /// Switchback while Switchback uses its own provider/account pool.
    #[serde(default)]
    pub client_profiles: Vec<ClientProfileConfig>,
    /// Simple local UX sugar: `model: "coder"` can map to an ordered list of
    /// provider/model targets. Runtime compiles this into a normal route plan.
    #[serde(default)]
    pub combos: BTreeMap<String, ComboConfig>,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// First-class tenants — the quota/attribution unit. Each carries optional
    /// hard limits (spend + concurrency). Requests are attributed to a tenant via
    /// the API key that authenticated them (see `api_keys`).
    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
    /// API keys mapping an inbound bearer token to a tenant (+ optional project
    /// label). When non-empty, this is the authoritative key list and inbound
    /// auth requires a match; when empty, `server.api_key` governs (single-tenant,
    /// no quota) and behaviour is unchanged.
    #[serde(default)]
    pub api_keys: Vec<ApiKeyConfig>,
    /// Trusted built-in plugins (Oracle #6, tier 1), compiled into the snapshot at
    /// publish time and run on the hot path: `pre_route` (inspect/modify/reject),
    /// `post_route` / `post_attempt` (observe), `select_egress` (choose a path).
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,
    /// Named outbound network paths. An account/provider can route its upstream
    /// calls through a declared HTTP(S)/SOCKS5 `proxy` egress, choosing which
    /// IP/proxy each request exits from. `direct` (no proxy) is always implicit.
    #[serde(default)]
    pub egress: Vec<EgressConfig>,
}

/// One configured plugin. `type` selects the built-in or sandbox tier; the rest
/// are its settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginConfig {
    /// Reject requests whose model matches one of `models` (exact or `prefix*`).
    ModelBlocklist {
        #[serde(default)]
        models: Vec<String>,
    },
    /// Inject fixed `tags` into the request's metadata before routing.
    RequestTag {
        #[serde(default)]
        tags: std::collections::BTreeMap<String, String>,
    },
    /// Pin requests whose model matches `models` to the named `egress` path.
    EgressPin {
        egress: String,
        #[serde(default)]
        models: Vec<String>,
    },
    /// A sandboxed Wasm plugin (Oracle #6 tier 2): a `.wasm`/`.wat` module that
    /// exports `memory`, `alloc(i32)->i32`, and `pre_route(ptr,len)->i32` (0 =
    /// continue, else = reject HTTP status). Only honored when the `wasm` build
    /// feature is enabled. `failure_mode=closed` makes publish/validation fail
    /// if the plugin cannot activate; `open` turns it into a loud no-op.
    Wasm {
        path: String,
        #[serde(default)]
        failure_mode: PluginFailureMode,
        #[serde(default = "default_wasm_timeout_ms")]
        timeout_ms: u64,
        #[serde(default = "default_wasm_fuel")]
        fuel: u64,
    },
}

fn default_wasm_timeout_ms() -> u64 {
    10
}

fn default_wasm_fuel() -> u64 {
    1_000_000
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginFailureMode {
    #[default]
    Open,
    Closed,
}

impl PluginFailureMode {
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Closed)
    }
}

/// A tenant: the unit of quota and usage attribution. Hard limits reject before
/// upstream dispatch — `budget_usd` (cumulative attributed spend) and
/// `max_concurrency` (simultaneous in-flight requests, reserved before dispatch
/// and released on completion).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantConfig {
    pub id: String,
    /// Optional allow-list of route names this tenant may use. Empty = all routes
    /// visible. Applies before routing so disallowed routes never dispatch.
    #[serde(default)]
    pub allowed_routes: Vec<String>,
    /// Optional allow-list of provider ids this tenant may route to. Empty = all
    /// providers visible.
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// Optional allow-list of concrete credential accounts as `provider/account`.
    /// Empty = all accounts visible. Providers without explicit accounts expose
    /// the synthesized `provider/default` account.
    #[serde(default)]
    pub allowed_accounts: Vec<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub max_concurrency: Option<u32>,
}

/// An inbound API key bound to a tenant (+ optional project label for
/// attribution). The key itself is a secret; it redacts in `Debug`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub tenant: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub role: ApiKeyRole,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyRole {
    #[default]
    Client,
    Operator,
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientProfileKind {
    /// OpenAI Responses-compatible client profile. This is the shape Codex
    /// expects when it points at a provider base URL.
    Codex,
    /// Anthropic Messages-compatible client profile. This is the shape Claude
    /// Code expects when it points at an Anthropic base URL.
    ClaudeCode,
}

impl ClientProfileKind {
    pub fn default_id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
        }
    }

    pub fn protocol(self) -> &'static str {
        match self {
            Self::Codex => "openai_responses",
            Self::ClaudeCode => "anthropic_messages",
        }
    }

    pub fn required_endpoints(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["/v1/responses", "/v1/models"],
            Self::ClaudeCode => &["/v1/messages", "/v1/messages/count_tokens"],
        }
    }

    pub fn session_headers(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["x-codex-session-id", "x-switchback-session-id"],
            Self::ClaudeCode => &["x-switchback-session-id", "x-session-id"],
        }
    }
}

fn default_client_profile_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientProfileConfig {
    /// Stable profile id exposed to operators/LLMs, e.g. `codex`.
    pub id: String,
    pub kind: ClientProfileKind,
    /// Disabled profiles remain visible in diagnostics but are not considered
    /// ready. This is useful while staging a client cutover.
    #[serde(default = "default_client_profile_enabled")]
    pub enabled: bool,
    /// Optional list of model ids this client should use. Empty means "all
    /// visible Switchback models/routes are acceptable".
    #[serde(default)]
    pub models: Vec<String>,
    /// Optional list of Switchback account refs (`provider/account`) that this
    /// profile is expected to use. Empty means "any visible account".
    #[serde(default)]
    pub accounts: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

impl std::fmt::Debug for ApiKeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyConfig")
            .field("key", &"[redacted]")
            .field("key_env", &self.key_env)
            .field("key_hash", &self.key_hash.as_ref().map(|_| "[redacted]"))
            .field("prefix", &self.prefix)
            .field("tenant", &self.tenant)
            .field("project", &self.project)
            .field("role", &self.role)
            .finish()
    }
}

impl Config {
    /// Resolve an inbound bearer token to its `(tenant, project, role)`. `None`
    /// = the key is not in `api_keys`.
    pub fn principal_for_key(&self, key: &str) -> Option<(&str, Option<&str>, ApiKeyRole)> {
        let mut matched = None;
        for configured in &self.api_keys {
            if api_key_matches(configured, key) {
                matched = Some((
                    configured.tenant.as_str(),
                    configured.project.as_deref(),
                    configured.role,
                ));
            }
        }
        matched
    }

    /// Constant-time comparison for the legacy single admin key.
    pub fn server_api_key_matches(&self, key: &str) -> bool {
        self.server
            .api_key
            .as_deref()
            .is_some_and(|expected| constant_time_secret_eq(expected, key))
    }

    /// The tenant record by id, if declared (for its quota limits).
    pub fn tenant(&self, id: &str) -> Option<&TenantConfig> {
        self.tenants.iter().find(|t| t.id == id)
    }

    /// True when the config contains secret material inline rather than by env
    /// var/vault/file reference. Used to prevent durable draft persistence from
    /// silently storing API keys/tokens in SQLite.
    pub fn has_inline_secret_material(&self) -> bool {
        non_empty(&self.server.api_key)
            || self.api_keys.iter().any(|k| non_empty(&k.key))
            || self
                .providers
                .iter()
                .any(provider_has_inline_secret_material)
            || self.egress.iter().any(egress_has_inline_secret_material)
    }

    /// Cross-reference and policy validation that serde shape checks cannot
    /// express. This deliberately avoids resolving secrets or constructing
    /// network clients, so it is safe to run in every config/publish path.
    pub fn semantic_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        let mut provider_ids = BTreeSet::new();
        for (i, provider) in self.providers.iter().enumerate() {
            if provider.id.trim().is_empty() {
                problems.push(format!("providers[{i}].id is empty"));
            } else if !provider_ids.insert(provider.id.as_str()) {
                problems.push(format!("providers[{i}].id duplicates `{}`", provider.id));
            }
            if provider
                .model_hint
                .as_ref()
                .is_some_and(|hint| hint.trim().is_empty())
            {
                problems.push(format!("providers[{i}].model_hint is empty"));
            }
        }

        let mut tenant_ids = BTreeSet::new();
        for (i, tenant) in self.tenants.iter().enumerate() {
            if tenant.id.trim().is_empty() {
                problems.push(format!("tenants[{i}].id is empty"));
            } else if !tenant_ids.insert(tenant.id.as_str()) {
                problems.push(format!("tenants[{i}].id duplicates `{}`", tenant.id));
            }
        }

        let mut egress_ids = BTreeSet::new();
        for (i, egress) in self.egress.iter().enumerate() {
            if egress.id.trim().is_empty() {
                problems.push(format!("egress[{i}].id is empty"));
            } else if egress.id == "direct" {
                problems.push(format!("egress[{i}].id `direct` is reserved"));
            } else if !egress_ids.insert(egress.id.as_str()) {
                problems.push(format!("egress[{i}].id duplicates `{}`", egress.id));
            }
        }

        if let Some(provider) = self.server.default_provider.as_deref() {
            if !provider_ids.contains(provider) {
                problems.push(format!(
                    "server.default_provider `{provider}` does not match a provider id"
                ));
            }
        }
        if let Some(egress) = self.server.default_egress.as_deref() {
            if egress != "direct" && !egress_ids.contains(egress) {
                problems.push(format!(
                    "server.default_egress `{egress}` does not match an egress id"
                ));
            }
        }
        if self
            .server
            .api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
            && !self.api_keys.is_empty()
        {
            problems.push(
                "server.api_key and api_keys cannot both be configured; use api_keys for multi-tenant auth"
                    .to_string(),
            );
        }

        if self.server.block_private_networks {
            for (i, provider) in self.providers.iter().enumerate() {
                for (field, url) in provider_urls(provider) {
                    if let Some(reason) = private_url_reason(url) {
                        problems.push(format!(
                            "providers[{i}].{field} `{url}` is blocked: {reason}"
                        ));
                    }
                }
                for (ai, account) in provider.accounts.iter().enumerate() {
                    if let AuthConfig::Oauth {
                        token_url: Some(url),
                        ..
                    } = &account.auth
                    {
                        if let Some(reason) = private_url_reason(url) {
                            problems.push(format!(
                                "providers[{i}].accounts[{ai}].auth.token_url `{url}` is blocked: {reason}"
                            ));
                        }
                    }
                }
            }
            for (i, egress) in self.egress.iter().enumerate() {
                if let EgressKind::Proxy { url: Some(url), .. } = &egress.kind {
                    if let Some(reason) = private_url_reason(url) {
                        problems.push(format!("egress[{i}].url `{url}` is blocked: {reason}"));
                    }
                }
            }
        }

        for (provider_id, cap) in &self.server.budget.per_provider_usd {
            if !provider_ids.contains(provider_id.as_str()) {
                problems.push(format!(
                    "server.budget.per_provider_usd `{provider_id}` does not match a provider id"
                ));
            }
            if !cap.is_finite() || *cap < 0.0 {
                problems.push(format!(
                    "server.budget.per_provider_usd `{provider_id}` must be a finite non-negative number"
                ));
            }
        }

        for (i, key) in self.api_keys.iter().enumerate() {
            let configured_sources = [
                non_empty(&key.key),
                non_empty(&key.key_env),
                non_empty(&key.key_hash),
            ]
            .into_iter()
            .filter(|present| *present)
            .count();
            if configured_sources != 1 {
                problems.push(format!(
                    "api_keys[{i}] must set exactly one of key, key_env, key_hash"
                ));
            }
            if key
                .key
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                problems.push(format!("api_keys[{i}].key is empty"));
            }
            if key
                .key_env
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                problems.push(format!("api_keys[{i}].key_env is empty"));
            }
            if let Some(hash) = key.key_hash.as_deref() {
                match normalize_sha256_hash(hash) {
                    Some(_) => {}
                    None => problems.push(format!(
                        "api_keys[{i}].key_hash must be sha256:<64 hex chars>"
                    )),
                }
            }
            if key
                .prefix
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                problems.push(format!("api_keys[{i}].prefix is empty"));
            }
            if key.tenant.trim().is_empty() {
                problems.push(format!("api_keys[{i}].tenant is empty"));
            } else if !tenant_ids.contains(key.tenant.as_str()) {
                problems.push(format!(
                    "api_keys[{i}].tenant `{}` does not match a tenant id",
                    key.tenant
                ));
            }
        }

        let mut api_key_values = BTreeSet::new();
        let mut api_key_hashes = BTreeSet::new();
        let mut api_key_envs = BTreeSet::new();
        for (i, key) in self.api_keys.iter().enumerate() {
            if let Some(value) = key.key.as_deref().filter(|value| !value.is_empty()) {
                if !api_key_values.insert(value) {
                    problems.push(format!("api_keys[{i}].key duplicates a previous API key"));
                }
            }
            if let Some(env) = key.key_env.as_deref().filter(|value| !value.is_empty()) {
                if !api_key_envs.insert(env) {
                    problems.push(format!(
                        "api_keys[{i}].key_env duplicates a previous API key env reference"
                    ));
                }
            }
            if let Some(hash) = key.key_hash.as_deref().and_then(normalize_sha256_hash) {
                if !api_key_hashes.insert(hash) {
                    problems.push(format!(
                        "api_keys[{i}].key_hash duplicates a previous API key hash"
                    ));
                }
            }
        }

        for (pi, provider) in self.providers.iter().enumerate() {
            if let Some(egress) = provider.egress.as_deref() {
                if egress != "direct" && !egress_ids.contains(egress) {
                    problems.push(format!(
                        "providers[{pi}].egress `{egress}` does not match an egress id"
                    ));
                }
            }
            let mut account_ids = BTreeSet::new();
            for (ai, account) in provider.accounts.iter().enumerate() {
                if account.id.trim().is_empty() {
                    problems.push(format!("providers[{pi}].accounts[{ai}].id is empty"));
                } else if !account_ids.insert(account.id.as_str()) {
                    problems.push(format!(
                        "providers[{pi}].accounts[{ai}].id duplicates `{}`",
                        account.id
                    ));
                }
                if let Some(egress) = account.egress.as_deref() {
                    if egress != "direct" && !egress_ids.contains(egress) {
                        problems.push(format!(
                            "providers[{pi}].accounts[{ai}].egress `{egress}` does not match an egress id"
                        ));
                    }
                }
                if self.vault.is_none() {
                    for (field, name) in auth_vault_refs(&account.auth) {
                        problems.push(format!(
                            "providers[{pi}].accounts[{ai}].auth.{field} references vault secret `{name}` but no `vault:` is configured"
                        ));
                    }
                }
            }
        }

        let mut client_profile_ids = BTreeSet::new();
        for (ci, profile) in self.client_profiles.iter().enumerate() {
            if profile.id.trim().is_empty() {
                problems.push(format!("client_profiles[{ci}].id is empty"));
            } else if !client_profile_ids.insert(profile.id.as_str()) {
                problems.push(format!(
                    "client_profiles[{ci}].id duplicates `{}`",
                    profile.id
                ));
            }
            for (mi, model) in profile.models.iter().enumerate() {
                if model.trim().is_empty() {
                    problems.push(format!("client_profiles[{ci}].models[{mi}] is empty"));
                }
            }
            for (ai, account_ref) in profile.accounts.iter().enumerate() {
                let Some((provider_id, account_id)) = account_ref.split_once('/') else {
                    problems.push(format!(
                        "client_profiles[{ci}].accounts[{ai}] `{account_ref}` must be `provider/account`"
                    ));
                    continue;
                };
                if provider_id.is_empty() || account_id.is_empty() {
                    problems.push(format!(
                        "client_profiles[{ci}].accounts[{ai}] `{account_ref}` must be `provider/account`"
                    ));
                    continue;
                }
                let Some(provider) = self.providers.iter().find(|p| p.id == provider_id) else {
                    problems.push(format!(
                        "client_profiles[{ci}].accounts[{ai}] `{account_ref}` references unknown provider `{provider_id}`"
                    ));
                    continue;
                };
                if !provider_has_account(provider, account_id) {
                    problems.push(format!(
                        "client_profiles[{ci}].accounts[{ai}] `{account_ref}` references unknown account `{account_id}`"
                    ));
                }
            }
        }

        let mut route_names = BTreeSet::new();
        for (ri, route) in self.routes.iter().enumerate() {
            if route.name.trim().is_empty() {
                problems.push(format!("routes[{ri}].name is empty"));
            } else if !route_names.insert(route.name.as_str()) {
                problems.push(format!("routes[{ri}].name duplicates `{}`", route.name));
            }
            if route.targets.is_empty() {
                problems.push(format!("routes[{ri}].targets must not be empty"));
            }
            for (ti, target) in route.targets.iter().enumerate() {
                let Some((provider, model)) = target.split_once('/') else {
                    problems.push(format!(
                        "routes[{ri}].targets[{ti}] `{target}` must be `provider/model`"
                    ));
                    continue;
                };
                if provider.is_empty() || model.is_empty() {
                    problems.push(format!(
                        "routes[{ri}].targets[{ti}] `{target}` must be `provider/model`"
                    ));
                } else if !provider_ids.contains(provider) {
                    problems.push(format!(
                        "routes[{ri}].targets[{ti}] `{target}` references unknown provider `{provider}`"
                    ));
                }
            }
        }

        let exact_route_models = self
            .routes
            .iter()
            .filter_map(|route| route.match_.model.as_deref())
            .filter(|model| *model != "*")
            .collect::<BTreeSet<_>>();
        for (name, combo) in &self.combos {
            if name.trim().is_empty() {
                problems.push("combos contains an empty name".to_string());
            }
            if name.contains('/') {
                problems.push(format!(
                    "combos[{name}].name must not contain `/` (reserved for provider/model ids)"
                ));
            }
            if crate::ExecutionProfile::from_model(name).is_some() {
                problems.push(format!(
                    "combos[{name}] conflicts with a built-in execution profile"
                ));
            }
            if exact_route_models.contains(name.as_str()) {
                problems.push(format!(
                    "combos[{name}] conflicts with an exact route match"
                ));
            }
            if combo.models.is_empty() {
                problems.push(format!("combos[{name}].models must not be empty"));
            }
            for (mi, target) in combo.models.iter().enumerate() {
                let Some((provider, model)) = target.split_once('/') else {
                    problems.push(format!(
                        "combos[{name}].models[{mi}] `{target}` must be `provider/model`"
                    ));
                    continue;
                };
                if provider.is_empty() || model.is_empty() {
                    problems.push(format!(
                        "combos[{name}].models[{mi}] `{target}` must be `provider/model`"
                    ));
                } else if !provider_ids.contains(provider) {
                    problems.push(format!(
                        "combos[{name}].models[{mi}] `{target}` references unknown provider `{provider}`"
                    ));
                }
            }
        }

        for (ti, tenant) in self.tenants.iter().enumerate() {
            for (ri, route) in tenant.allowed_routes.iter().enumerate() {
                if route.trim().is_empty() {
                    problems.push(format!("tenants[{ti}].allowed_routes[{ri}] is empty"));
                } else if !route_names.contains(route.as_str())
                    && !self.combos.contains_key(route)
                    && route != "direct"
                    && !route.starts_with("combo/")
                    && !route.starts_with("default:")
                {
                    problems.push(format!(
                        "tenants[{ti}].allowed_routes[{ri}] `{route}` does not match a route/combo name"
                    ));
                }
            }
            for (pi, provider_id) in tenant.allowed_providers.iter().enumerate() {
                if provider_id.trim().is_empty() {
                    problems.push(format!("tenants[{ti}].allowed_providers[{pi}] is empty"));
                } else if !provider_ids.contains(provider_id.as_str()) {
                    problems.push(format!(
                        "tenants[{ti}].allowed_providers[{pi}] `{provider_id}` does not match a provider id"
                    ));
                }
            }
            for (ai, account_ref) in tenant.allowed_accounts.iter().enumerate() {
                let Some((provider_id, account_id)) = account_ref.split_once('/') else {
                    problems.push(format!(
                        "tenants[{ti}].allowed_accounts[{ai}] `{account_ref}` must be `provider/account`"
                    ));
                    continue;
                };
                if provider_id.is_empty() || account_id.is_empty() {
                    problems.push(format!(
                        "tenants[{ti}].allowed_accounts[{ai}] `{account_ref}` must be `provider/account`"
                    ));
                    continue;
                }
                let Some(provider) = self.providers.iter().find(|p| p.id == provider_id) else {
                    problems.push(format!(
                        "tenants[{ti}].allowed_accounts[{ai}] `{account_ref}` references unknown provider `{provider_id}`"
                    ));
                    continue;
                };
                if !provider_has_account(provider, account_id) {
                    problems.push(format!(
                        "tenants[{ti}].allowed_accounts[{ai}] `{account_ref}` references unknown account `{account_id}`"
                    ));
                }
            }
        }

        for (i, plugin) in self.plugins.iter().enumerate() {
            match plugin {
                PluginConfig::EgressPin { egress, .. } => {
                    if egress != "direct" && !egress_ids.contains(egress.as_str()) {
                        problems.push(format!(
                            "plugins[{i}].egress `{egress}` does not match an egress id"
                        ));
                    }
                }
                PluginConfig::Wasm {
                    path,
                    timeout_ms,
                    fuel,
                    ..
                } => {
                    if path.trim().is_empty() {
                        problems.push(format!("plugins[{i}].path must not be empty"));
                    }
                    if *timeout_ms == 0 {
                        problems.push(format!("plugins[{i}].timeout_ms must be greater than 0"));
                    }
                    if *fuel == 0 {
                        problems.push(format!("plugins[{i}].fuel must be greater than 0"));
                    }
                }
                PluginConfig::ModelBlocklist { .. } | PluginConfig::RequestTag { .. } => {}
            }
        }

        problems
    }
}

fn constant_time_secret_eq(expected: &str, presented: &str) -> bool {
    // Compare fixed-width SHA-256 digests rather than the raw bytes. This makes
    // the comparison loop a constant 32 iterations regardless of input length —
    // the previous hand-rolled compare looped to `max(expected, presented)` and
    // seeded `diff` with the length XOR, leaking the configured secret's length
    // as a timing oracle. Hashing both sides absorbs the length difference; the
    // only length-dependent timing left is hashing the *presented* value, whose
    // length the caller already controls and which reveals nothing about the
    // secret. Branchless OR-accumulate keeps the per-byte compare flat.
    use sha2::{Digest, Sha256};
    let expected = Sha256::digest(expected.as_bytes());
    let presented = Sha256::digest(presented.as_bytes());
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(presented.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn api_key_matches(configured: &ApiKeyConfig, presented: &str) -> bool {
    let mut matched = false;
    if let Some(expected) = configured.key.as_deref().filter(|value| !value.is_empty()) {
        matched |= constant_time_secret_eq(expected, presented);
    }
    if let Some(env_name) = configured
        .key_env
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        if let Ok(expected) = std::env::var(env_name) {
            if !expected.is_empty() {
                matched |= constant_time_secret_eq(&expected, presented);
            }
        }
    }
    if let Some(expected_hash) = configured
        .key_hash
        .as_deref()
        .and_then(normalize_sha256_hash)
    {
        matched |= constant_time_secret_eq(expected_hash, &sha256_hex(presented.as_bytes()));
    }
    matched
}

fn normalize_sha256_hash(hash: &str) -> Option<&str> {
    let hash = hash.strip_prefix("sha256:")?;
    if hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Some(hash)
    } else {
        None
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn non_empty(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|v| !v.trim().is_empty())
}

fn provider_has_inline_secret_material(provider: &ProviderConfig) -> bool {
    provider_kind_has_inline_secret_material(&provider.kind)
        || provider
            .accounts
            .iter()
            .any(|account| auth_has_inline_secret_material(&account.auth))
}

fn provider_kind_has_inline_secret_material(kind: &ProviderKind) -> bool {
    match kind {
        ProviderKind::Mock
        | ProviderKind::Bedrock { .. }
        | ProviderKind::CodexNativeRelay { .. }
        | ProviderKind::ClaudeCodeNativeRelay { .. } => false,
        ProviderKind::OpenaiCompatible { api_key, .. }
        | ProviderKind::Anthropic { api_key, .. }
        | ProviderKind::Gemini { api_key, .. }
        | ProviderKind::Vertex { api_key, .. } => non_empty(api_key),
    }
}

fn provider_has_account(provider: &ProviderConfig, account_id: &str) -> bool {
    if provider.accounts.is_empty() {
        account_id == "default"
    } else {
        provider
            .accounts
            .iter()
            .any(|account| account.id == account_id)
    }
}

fn auth_has_inline_secret_material(auth: &AuthConfig) -> bool {
    match auth {
        AuthConfig::None
        | AuthConfig::CodexOauth { .. }
        | AuthConfig::ClaudeCodeOauth { .. }
        | AuthConfig::ServiceAccount { .. } => false,
        AuthConfig::ApiKey { inline, .. } => non_empty(inline),
        AuthConfig::Oauth {
            token,
            refresh,
            client_secret,
            ..
        } => non_empty(token) || non_empty(refresh) || non_empty(client_secret),
        AuthConfig::AwsSigV4 {
            access_key,
            secret_key,
            session_token,
            ..
        } => non_empty(access_key) || non_empty(secret_key) || non_empty(session_token),
    }
}

fn egress_has_inline_secret_material(egress: &EgressConfig) -> bool {
    match &egress.kind {
        EgressKind::Proxy { url: Some(url), .. } => url_has_credentials(url),
        _ => false,
    }
}

fn auth_vault_refs(auth: &AuthConfig) -> Vec<(&'static str, &str)> {
    match auth {
        AuthConfig::ApiKey {
            vault: Some(name), ..
        } if !name.trim().is_empty() => vec![("vault", name.as_str())],
        AuthConfig::Oauth {
            token_vault,
            refresh_vault,
            client_secret_vault,
            ..
        } => [
            ("token_vault", token_vault.as_deref()),
            ("refresh_vault", refresh_vault.as_deref()),
            ("client_secret_vault", client_secret_vault.as_deref()),
        ]
        .into_iter()
        .filter_map(|(field, name)| {
            name.filter(|value| !value.trim().is_empty())
                .map(|value| (field, value))
        })
        .collect(),
        AuthConfig::CodexOauth {
            token_vault: Some(name),
            ..
        }
        | AuthConfig::ClaudeCodeOauth {
            token_vault: Some(name),
            ..
        } if !name.trim().is_empty() => vec![("token_vault", name.as_str())],
        _ => Vec::new(),
    }
}

fn url_has_credentials(url: &str) -> bool {
    let Some((_scheme, rest)) = url.split_once("://") else {
        return false;
    };
    rest.split(['/', '?', '#'])
        .next()
        .is_some_and(|authority| authority.rsplit('@').nth(1).is_some())
}

fn provider_urls(provider: &ProviderConfig) -> Vec<(&'static str, &str)> {
    match &provider.kind {
        ProviderKind::OpenaiCompatible { base_url, .. }
        | ProviderKind::Anthropic { base_url, .. }
        | ProviderKind::Gemini { base_url, .. } => vec![("base_url", base_url.as_str())],
        ProviderKind::Vertex { base_url, .. } | ProviderKind::Bedrock { base_url, .. } => base_url
            .as_deref()
            .map(|url| vec![("base_url", url)])
            .unwrap_or_default(),
        ProviderKind::Mock
        | ProviderKind::CodexNativeRelay { base_url: None }
        | ProviderKind::ClaudeCodeNativeRelay { base_url: None } => Vec::new(),
        ProviderKind::CodexNativeRelay {
            base_url: Some(base_url),
        }
        | ProviderKind::ClaudeCodeNativeRelay {
            base_url: Some(base_url),
        } => vec![("base_url", base_url.as_str())],
    }
}

pub fn private_url_reason(url: &str) -> Option<String> {
    let host = host_from_url(url)?;
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Some("localhost host".to_string());
    }
    if let Ok(ip) = lower.parse::<IpAddr>() {
        if private_ip_reason(ip).is_some() {
            return Some(format!("private or local IP host `{host}`"));
        }
    }
    None
}

pub fn private_ip_reason(ip: IpAddr) -> Option<&'static str> {
    let blocked = match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.octets()[0] == 169 && ip.octets()[1] == 254
        }
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            ip.is_loopback()
                || ip.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
        }
    };
    blocked.then_some("private or local IP")
}

pub fn host_from_url(url: &str) -> Option<String> {
    let (_scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    if host_port.starts_with('[') {
        return host_port
            .split_once(']')
            .map(|(host, _)| host.trim_start_matches('[').to_string());
    }
    let host = host_port.split(':').next().unwrap_or(host_port);
    (!host.is_empty()).then(|| host.to_string())
}

/// One named outbound path. `enabled: false` toggles it off without deleting it
/// (callers that referenced it fall back to `direct`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressConfig {
    pub id: String,
    #[serde(default, flatten)]
    pub kind: EgressKind,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional client identity applied to requests on this path: a custom
    /// `User-Agent` and arbitrary headers (e.g. an app id).
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

/// How an egress reaches upstreams. `Direct` is the no-proxy default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EgressKind {
    /// No proxy — the machine's default route.
    #[default]
    Direct,
    /// Route through an HTTP(S)/SOCKS5 proxy. The URL may carry credentials, so
    /// prefer `url_env` (read from an env var) over inline `url` in shared config.
    Proxy {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        url_env: Option<String>,
    },
}

fn default_true() -> bool {
    true
}

/// Where the encrypted vault file lives and which keychain service holds its key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultConfig {
    /// Path to the age-encrypted vault file.
    pub path: String,
    /// OS-keychain service name the age identity is stored under.
    #[serde(default = "default_vault_service")]
    pub keychain_service: String,
}

fn default_vault_service() -> String {
    "switchback".to_string()
}

fn default_trace_ring_size() -> usize {
    256
}

fn default_trace_sample() -> f64 {
    1.0
}

fn default_admission_timeout_ms() -> u64 {
    10_000
}

fn default_admission_slot_ttl_ms() -> u64 {
    600_000
}

fn default_tenant_concurrency_ttl_ms() -> u64 {
    600_000
}

fn default_idempotency_inflight_ttl_ms() -> u64 {
    600_000
}

/// How much of a request/response telemetry may capture. Only `MetadataOnly` is
/// enforced today (it's what the gateway already does); the richer modes are
/// reserved for a future body-capture path and are documented seams.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    /// Route + attempt metadata only — never prompt/response content (default).
    #[default]
    MetadataOnly,
    /// Reserved: summaries with content transforms applied. Not yet enforced.
    SummaryWithTransforms,
    /// Reserved: full request/response bodies, encrypted at rest. Not yet enforced.
    FullBodyEncrypted,
}

fn default_langfuse_public_key_env() -> String {
    "LANGFUSE_PUBLIC_KEY".to_string()
}

fn default_langfuse_secret_key_env() -> String {
    "LANGFUSE_SECRET_KEY".to_string()
}

fn default_langfuse_host() -> String {
    "https://cloud.langfuse.com".to_string()
}

/// Langfuse export helper over the existing OpenTelemetry seam. Secret values
/// are never stored here: API keys are read from env and encoded into OTLP HTTP
/// headers at process startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LangfuseConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Langfuse base URL. The OTLP traces path is appended automatically unless
    /// `otel_endpoint` is set.
    #[serde(default = "default_langfuse_host")]
    pub host: String,
    /// Optional full OTLP traces endpoint. Defaults to
    /// `{host}/api/public/otel/v1/traces`.
    #[serde(default)]
    pub otel_endpoint: Option<String>,
    #[serde(default = "default_langfuse_public_key_env")]
    pub public_key_env: String,
    #[serde(default = "default_langfuse_secret_key_env")]
    pub secret_key_env: String,
}

impl Default for LangfuseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_langfuse_host(),
            otel_endpoint: None,
            public_key_env: default_langfuse_public_key_env(),
            secret_key_env: default_langfuse_secret_key_env(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    /// If set, inbound `/v1` requests must present this as `Authorization: Bearer`.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub timeouts: Timeouts,
    /// Admission control (Oracle #8): a global cap on simultaneously in-flight
    /// requests. When set, a request waits up to `admission_timeout_ms` for a slot
    /// (bounded backpressure) and is shed with 503 if the wait is exceeded. Unset
    /// = unlimited. Pairs with per-tenant `max_concurrency`.
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// How long a request may queue for a global admission slot before being shed
    /// (503). Only relevant when `max_concurrency` is set.
    #[serde(default = "default_admission_timeout_ms")]
    pub admission_timeout_ms: u64,
    /// TTL for durable global admission slots. Only used when a state store is
    /// configured; prevents abandoned cross-process slots from surviving forever
    /// if a gateway process crashes mid-request.
    #[serde(default = "default_admission_slot_ttl_ms")]
    pub admission_slot_ttl_ms: u64,
    /// TTL for durable tenant concurrency slots. Only used when a state store is
    /// configured; prevents abandoned cross-process slots from surviving forever
    /// if a gateway process crashes mid-request.
    #[serde(default = "default_tenant_concurrency_ttl_ms")]
    pub tenant_concurrency_ttl_ms: u64,
    /// Collect-path byte ceiling (Oracle #8): cap the assembled content of a
    /// NON-streaming response. A response that exceeds it is aborted rather than
    /// buffered unbounded. Unset = no cap.
    #[serde(default)]
    pub max_response_bytes: Option<u64>,
    /// Telemetry privacy mode (Oracle #8). `metadata_only` (the default, and the
    /// only one enforced today: logs/traces are route+attempt metadata, never
    /// prompt/response content) — the richer modes are reserved seams.
    #[serde(default)]
    pub privacy_mode: PrivacyMode,
    /// Reject high-lossiness JSON-Schema downlevels instead of warning and
    /// dispatching. Off by default so current Gemini/Vertex compatibility keeps
    /// working; enable when schema fidelity is more important than fallback.
    #[serde(default)]
    pub strict_schema_downlevel: bool,
    /// Opt-in RTK-style tool-result compression on the request path. Off by
    /// default — heuristic compaction is fail-safe (never-grow/never-empty) but
    /// can re-shape content, so it's a deliberate choice.
    #[serde(default)]
    pub compress_tool_results: bool,
    /// Optional path to append the usage/cost ledger as JSONL (an audit trail).
    /// The in-memory ledger + `/v1/usage` summary work regardless.
    #[serde(default)]
    pub usage_log: Option<String>,
    /// Optional path to append the per-request trace log as JSONL. The in-memory
    /// recent-traces ring + `/v1/traces` work regardless. Traces are metadata
    /// only (route decision + attempts + cost) — never secrets or content.
    #[serde(default)]
    pub trace_log: Option<String>,
    /// Optional SQLite state store (durable control-plane state). A string keeps
    /// the legacy shorthand (`state_store: "/path/state.sqlite"`); the object
    /// form adds startup policy (`path`, `required`).
    #[serde(default)]
    pub state_store: Option<StateStoreConfig>,
    /// Permit durable `/cp/v1` drafts whose proposed config contains inline
    /// secret material. Off by default: durable drafts survive restarts, so
    /// inline API keys/tokens must be an explicit operator choice.
    #[serde(default)]
    pub persist_secret_bearing_drafts: bool,
    /// Idempotency persistence policy. Response-body replay is useful, but it
    /// stores model output; keep it explicit rather than implied by `state_store`.
    #[serde(default)]
    pub idempotency: IdempotencyConfig,
    /// How many recent traces the in-memory ring keeps for `/v1/traces`.
    #[serde(default = "default_trace_ring_size")]
    pub trace_ring_size: usize,
    /// Fraction of requests to record a trace for (0.0–1.0). 1.0 = every request
    /// (default); lower values sample by a stable hash of the request id, so a
    /// request is either fully traced or not at all. Structured logs are emitted
    /// regardless — only the `/v1/traces` ring + JSONL sink are sampled.
    #[serde(default = "default_trace_sample")]
    pub trace_sample: f64,
    /// Pass-through provider for any model that matches no route and isn't a
    /// `provider/model` target. Point it at e.g. `openrouter` and ANY model that
    /// provider serves works with no per-model config and no rebuild — adding a
    /// model becomes a data/runtime concern, not a code change.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Cost-aware routing toggle. When on, the router re-orders a route's
    /// surviving candidates cheapest-first (by blended input+output price from
    /// the cost map) instead of using the declared fallback order. Off by
    /// default — declared order is preserved.
    #[serde(default)]
    pub cost_aware: bool,
    /// Path to a cost map JSON (e.g. config/provider-registry.json). Loaded at
    /// startup into a per-(provider, model) price index that feeds cost-aware
    /// routing. Without it, cost-aware routing has no prices to sort by.
    #[serde(default)]
    pub cost_map: Option<String>,
    /// Optional price ceiling (blended USD per 1M tokens): routing rejects any
    /// priced candidate above it (OpenRouter `max_price` idea). Unknown prices
    /// follow `cost_unknown`.
    #[serde(default)]
    pub cost_max_per_mtok: Option<f64>,
    /// Latency-aware routing toggle: order candidates fastest-first by an EWMA
    /// of observed upstream latency. `cost_aware` wins when both are on.
    #[serde(default)]
    pub latency_aware: bool,
    /// Cost-routing policy gates (all default-allow). Set false to exclude that
    /// lane from routing: `cost_allow_free` (free tiers / price 0),
    /// `cost_allow_promo` (time-boxed promo prices), `cost_allow_aggregator`
    /// (third-party open-weight hosts). These are hard gates, not ordering hints.
    #[serde(default = "default_true")]
    pub cost_allow_free: bool,
    #[serde(default = "default_true")]
    pub cost_allow_promo: bool,
    #[serde(default = "default_true")]
    pub cost_allow_aggregator: bool,
    /// Policy for candidates without a known price: `allow` (default),
    /// `penalize` (eligible but sorted after priced candidates in cost-aware
    /// mode), or `reject`.
    #[serde(default)]
    pub cost_unknown: crate::routing::UnknownCostPolicy,
    /// Policy for candidates without a known context window when a route asks
    /// for `min_context_tokens`: `allow` (default) or `reject`.
    #[serde(default)]
    pub context_unknown: crate::routing::UnknownContextPolicy,
    /// Permit an unauthenticated admin gateway on non-loopback binds. Loopback
    /// stays open by default for local-first quickstart; `0.0.0.0` / `::` must
    /// either configure API keys or opt in here.
    #[serde(default)]
    pub allow_open_admin: bool,
    /// Default egress when neither the account nor the provider names one.
    #[serde(default)]
    pub default_egress: Option<String>,
    /// Master switch for the egress layer. When false, every call goes `direct`
    /// regardless of per-account/provider bindings (a kill-switch).
    #[serde(default = "default_true")]
    pub egress_enabled: bool,
    /// OTLP/HTTP traces endpoint (the full signal URL, e.g.
    /// `http://localhost:4318/v1/traces`) to export request/attempt spans to.
    /// Only active when the binary is built with the `otel` feature; the same
    /// spans render locally regardless.
    #[serde(default)]
    pub otel_endpoint: Option<String>,
    /// Langfuse convenience wrapper around OTLP/HTTP export. Requires the
    /// `otel` feature just like `otel_endpoint`.
    #[serde(default)]
    pub langfuse: LangfuseConfig,
    /// Hosted-mode SSRF guard. Off by default so local-first deployments can
    /// route to Ollama/vLLM/loopback. When enabled, config validation rejects
    /// provider/proxy/token URLs with localhost, private, link-local, or
    /// unspecified literal hosts.
    #[serde(default)]
    pub block_private_networks: bool,
    /// Resilience: same-target retry on transient errors (before fallover).
    #[serde(default)]
    pub retry: RetryConfig,
    /// Resilience: provider-level circuit breaker (fast-fail a failing provider).
    #[serde(default)]
    pub circuit_breaker: BreakerConfig,
    /// Spend caps from the usage ledger.
    #[serde(default)]
    pub budget: BudgetConfig,
    /// Request hedging: race the top candidates, take the first, cancel the rest.
    #[serde(default)]
    pub hedge: HedgeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StateStoreConfig {
    Path(String),
    Detailed {
        path: String,
        #[serde(default)]
        required: bool,
    },
}

impl StateStoreConfig {
    pub fn path(&self) -> &str {
        match self {
            StateStoreConfig::Path(path) => path,
            StateStoreConfig::Detailed { path, .. } => path,
        }
    }

    pub fn required(&self) -> bool {
        match self {
            StateStoreConfig::Path(_) => false,
            StateStoreConfig::Detailed { required, .. } => *required,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdempotencyConfig {
    /// Store rendered non-streaming response bodies for durable idempotency
    /// replay. Off by default because this persists model output.
    #[serde(default)]
    pub persist_response_bodies: bool,
    /// TTL for durable in-flight idempotency claims. Only used when a state store
    /// is configured; prevents abandoned keys from blocking forever after a
    /// process crash.
    #[serde(default = "default_idempotency_inflight_ttl_ms")]
    pub inflight_ttl_ms: u64,
}

impl Default for IdempotencyConfig {
    fn default() -> Self {
        Self {
            persist_response_bodies: false,
            inflight_ttl_ms: default_idempotency_inflight_ttl_ms(),
        }
    }
}

/// Same-target retry on transient errors (timeout/network/5xx) BEFORE falling
/// over to another account/target. Off by default (`max_retries: 0`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default)]
    pub max_retries: u32,
    #[serde(default = "default_retry_base_ms")]
    pub base_delay_ms: u64,
    #[serde(default = "default_retry_max_ms")]
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            max_retries: 0,
            base_delay_ms: default_retry_base_ms(),
            max_delay_ms: default_retry_max_ms(),
        }
    }
}

fn default_retry_base_ms() -> u64 {
    100
}
fn default_retry_max_ms() -> u64 {
    2_000
}

/// Provider-level circuit breaker: after `failure_threshold` consecutive
/// failures across a provider's accounts, OPEN the provider for `open_secs`
/// (fast-fail its targets), then HALF-OPEN to probe recovery. Off by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_breaker_threshold")]
    pub failure_threshold: u32,
    #[serde(default = "default_breaker_open_secs")]
    pub open_secs: u64,
}

impl Default for BreakerConfig {
    fn default() -> Self {
        BreakerConfig {
            enabled: false,
            failure_threshold: default_breaker_threshold(),
            open_secs: default_breaker_open_secs(),
        }
    }
}

fn default_breaker_threshold() -> u32 {
    5
}
fn default_breaker_open_secs() -> u64 {
    30
}

/// Spend caps. `max_usd` is a hard total ceiling (reject new requests once the
/// ledger's attributed spend reaches it); `per_provider_usd` caps per provider
/// (a provider over its cap is routed around). Unset = no cap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetConfig {
    #[serde(default)]
    pub max_usd: Option<f64>,
    #[serde(default)]
    pub per_provider_usd: std::collections::BTreeMap<String, f64>,
}

/// Request hedging: for NON-streaming requests, send the request to up to
/// `max_parallel` candidates (the second after `delay_ms`), take the first
/// success, cancel the losers. Off by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HedgeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_hedge_delay_ms")]
    pub delay_ms: u64,
    #[serde(default = "default_hedge_max")]
    pub max_parallel: u32,
}

impl Default for HedgeConfig {
    fn default() -> Self {
        HedgeConfig {
            enabled: false,
            delay_ms: default_hedge_delay_ms(),
            max_parallel: default_hedge_max(),
        }
    }
}

fn default_hedge_delay_ms() -> u64 {
    150
}
fn default_hedge_max() -> u32 {
    2
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            bind: "127.0.0.1:8765".to_string(),
            api_key: None,
            timeouts: Timeouts::default(),
            max_concurrency: None,
            admission_timeout_ms: default_admission_timeout_ms(),
            admission_slot_ttl_ms: default_admission_slot_ttl_ms(),
            tenant_concurrency_ttl_ms: default_tenant_concurrency_ttl_ms(),
            max_response_bytes: None,
            privacy_mode: PrivacyMode::default(),
            strict_schema_downlevel: false,
            compress_tool_results: false,
            usage_log: None,
            trace_log: None,
            state_store: None,
            persist_secret_bearing_drafts: false,
            idempotency: IdempotencyConfig::default(),
            trace_ring_size: default_trace_ring_size(),
            trace_sample: default_trace_sample(),
            default_provider: None,
            cost_aware: false,
            cost_map: None,
            cost_max_per_mtok: None,
            latency_aware: false,
            cost_allow_free: true,
            cost_allow_promo: true,
            cost_allow_aggregator: true,
            cost_unknown: crate::routing::UnknownCostPolicy::Allow,
            context_unknown: crate::routing::UnknownContextPolicy::Allow,
            allow_open_admin: false,
            default_egress: None,
            egress_enabled: true,
            otel_endpoint: None,
            langfuse: LangfuseConfig::default(),
            block_private_networks: false,
            retry: RetryConfig::default(),
            circuit_breaker: BreakerConfig::default(),
            budget: BudgetConfig::default(),
            hedge: HedgeConfig::default(),
        }
    }
}

/// Upstream HTTP timeouts. Deliberately NOT a total request timeout — that
/// would cap long streamed generations. `connect` fails fast on an unreachable
/// upstream; `read` bounds the idle time between bytes, so a hung stream is
/// detected without limiting a healthy long one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Timeouts {
    #[serde(default = "default_connect_ms")]
    pub connect_ms: u64,
    #[serde(default = "default_read_ms")]
    pub read_ms: u64,
}

fn default_connect_ms() -> u64 {
    10_000
}
fn default_read_ms() -> u64 {
    300_000
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            connect_ms: default_connect_ms(),
            read_ms: default_read_ms(),
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
        /// How the key is attached on the wire. Defaults to `bearer`; set e.g.
        /// `{ kind: header, name: x-api-key }` for an OpenAI-shaped endpoint that
        /// authenticates differently — no new adapter needed.
        #[serde(default)]
        auth_scheme: Option<AuthScheme>,
    },
    /// Anthropic Messages API (`/v1/messages`). A distinct wire format from
    /// OpenAI — `x-api-key` auth, top-level `system`, content-block streaming —
    /// so it gets its own protocol module + adapter, never the openai one.
    Anthropic {
        #[serde(default = "default_anthropic_base_url")]
        base_url: String,
        /// Read the key from this env var at startup (preferred — never in the file).
        #[serde(default)]
        api_key_env: Option<String>,
        /// Inline key (discouraged; for quick local testing only).
        #[serde(default)]
        api_key: Option<String>,
        /// How the credential is attached on the Anthropic wire. Defaults to
        /// `x-api-key`; set `{ kind: bearer }` for Claude Code OAuth tokens.
        #[serde(default)]
        auth_scheme: Option<AuthScheme>,
    },
    /// Google Gemini GenerateContent API. A third wire format — `x-goog-api-key`
    /// auth, `contents`/`parts`, model in the URL path — its own module/adapter.
    Gemini {
        #[serde(default = "default_gemini_base_url")]
        base_url: String,
        /// Read the key from this env var at startup (preferred — never in the file).
        #[serde(default)]
        api_key_env: Option<String>,
        /// Inline key (discouraged; for quick local testing only).
        #[serde(default)]
        api_key: Option<String>,
    },
    /// Google **Vertex AI** — the Gemini wire format on GCP's project-scoped
    /// endpoint, authenticated with an OAuth2 Bearer token. Tokens can be
    /// supplied directly (`api_key_env` / `api_key`) or minted per account via
    /// `AuthConfig::ServiceAccount` and `ServiceAccountMinter`.
    Vertex {
        project: String,
        region: String,
        /// Defaults to `https://{region}-aiplatform.googleapis.com`.
        #[serde(default)]
        base_url: Option<String>,
        /// OAuth access token — read from this env var (preferred).
        #[serde(default)]
        api_key_env: Option<String>,
        /// Inline token (discouraged; quick local testing only).
        #[serde(default)]
        api_key: Option<String>,
    },
    /// **AWS Bedrock** — the Anthropic (Claude) wire on Bedrock's runtime
    /// endpoint, authenticated with AWS SigV4 request signing (access key +
    /// secret key, optionally a session token), and streamed as the binary
    /// `application/vnd.amazon.eventstream` framing rather than SSE.
    Bedrock {
        region: String,
        /// AWS access key id — read from this env var.
        #[serde(default = "default_aws_access_env")]
        access_key_env: String,
        /// AWS secret access key — read from this env var.
        #[serde(default = "default_aws_secret_env")]
        secret_key_env: String,
        /// Optional STS session token env var (for temporary credentials).
        #[serde(default)]
        session_token_env: Option<String>,
        /// Defaults to `https://bedrock-runtime.{region}.amazonaws.com`.
        #[serde(default)]
        base_url: Option<String>,
    },
    /// Planned first-party Codex subscription relay. This is deliberately
    /// distinct from `openai_compatible` + `codex_oauth`: it is not implemented
    /// until audited native wire fixtures exist.
    CodexNativeRelay {
        #[serde(default)]
        base_url: Option<String>,
    },
    /// First-party Claude Code subscription relay. This is deliberately
    /// distinct from `anthropic` + `claude_code_oauth`: it carries the native
    /// relay provider intent while reusing the audited Anthropic Messages wire.
    ClaudeCodeNativeRelay {
        #[serde(default)]
        base_url: Option<String>,
    },
}

fn default_aws_access_env() -> String {
    "AWS_ACCESS_KEY_ID".to_string()
}
fn default_aws_secret_env() -> String {
    "AWS_SECRET_ACCESS_KEY".to_string()
}

fn default_anthropic_base_url() -> String {
    "https://api.anthropic.com".to_string()
}

fn default_gemini_base_url() -> String {
    "https://generativelanguage.googleapis.com".to_string()
}

fn default_codex_oauth_token_env() -> Option<String> {
    Some("CODEX_ACCESS_TOKEN".to_string())
}

fn default_codex_oauth_token_file() -> Option<String> {
    Some("${HOME}/.codex/auth.json".to_string())
}

fn default_codex_oauth_access_token_pointer() -> String {
    "/tokens/access_token".to_string()
}

fn default_claude_code_oauth_token_env() -> Option<String> {
    Some("CLAUDE_CODE_OAUTH_TOKEN".to_string())
}

fn default_claude_code_oauth_token_file() -> Option<String> {
    Some("${HOME}/.claude/.credentials.json".to_string())
}

fn default_claude_code_oauth_access_token_pointer() -> String {
    "/claudeAiOauth/accessToken".to_string()
}

/// How a simple API-key credential is attached on the wire. Request-signing auth
/// such as AWS SigV4 lives in `RequestSigner`; service-account JWT minting lives
/// in account auth and yields bearer leases before this layer sees the request.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthScheme {
    /// No credential attached.
    None,
    /// `Authorization: Bearer <secret>` (OpenAI, OpenRouter, most key providers).
    #[default]
    Bearer,
    /// The secret in a named header, e.g. `x-api-key` / `x-goog-api-key`.
    Header { name: String },
    /// The secret in a query parameter, e.g. `?key=<secret>`.
    Query { name: String },
}

/// How a single account authenticates. Multiple methods coexist across the
/// accounts of one provider (api_key on one, oauth on another).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthConfig {
    /// No credential (local Ollama, mock).
    #[default]
    None,
    /// Bearer API key, read from env (preferred) or inline (discouraged).
    ApiKey {
        #[serde(default)]
        env: Option<String>,
        #[serde(default)]
        inline: Option<String>,
        /// Name of a secret in the encrypted vault. Highest precedence — wins
        /// over `env`/`inline` when a vault is configured.
        #[serde(default)]
        vault: Option<String>,
    },
    /// OAuth bearer. With `refresh_*` + `token_url`, the access token is
    /// refreshed live before use by `sb-credentials::RefreshCoordinator`
    /// (one refresh per account even under concurrent load). Without them it's
    /// a static token.
    Oauth {
        /// Initial access token (optional — if absent and a refresh token is
        /// present, the first request mints one).
        #[serde(default)]
        token_env: Option<String>,
        #[serde(default)]
        token: Option<String>,
        /// Vault secret name for the initial access token.
        #[serde(default)]
        token_vault: Option<String>,
        /// Refresh token (env preferred). With `token_url` it enables live
        /// refresh: an expired access token is refreshed before use.
        #[serde(default)]
        refresh_env: Option<String>,
        #[serde(default)]
        refresh: Option<String>,
        /// Vault secret name for the refresh token. When the upstream rotates
        /// refresh tokens, Switchback persists the replacement back to this
        /// vault secret atomically before using it.
        #[serde(default)]
        refresh_vault: Option<String>,
        /// OAuth2 token endpoint for the `refresh_token` grant.
        #[serde(default)]
        token_url: Option<String>,
        /// OAuth client id (sent in the refresh request, where required).
        #[serde(default)]
        client_id: Option<String>,
        /// OAuth client secret (env preferred).
        #[serde(default)]
        client_secret_env: Option<String>,
        #[serde(default)]
        client_secret: Option<String>,
        /// Vault secret name for the OAuth client secret.
        #[serde(default)]
        client_secret_vault: Option<String>,
    },
    /// Native Codex OAuth access token source. Reads the current token from
    /// `CODEX_ACCESS_TOKEN`, a vault secret, or `${HOME}/.codex/auth.json`
    /// (`/tokens/access_token`) at lease time, so the native client can keep
    /// refreshing its own store.
    CodexOauth {
        #[serde(default = "default_codex_oauth_token_env")]
        token_env: Option<String>,
        #[serde(default)]
        token_vault: Option<String>,
        #[serde(default = "default_codex_oauth_token_file")]
        token_file: Option<String>,
        #[serde(default = "default_codex_oauth_access_token_pointer")]
        access_token_pointer: String,
    },
    /// Native Claude Code OAuth access token source. Supports the portable
    /// `CLAUDE_CODE_OAUTH_TOKEN` produced by `claude setup-token`, or a JSON
    /// credentials file that exposes `claudeAiOauth.accessToken`.
    ClaudeCodeOauth {
        #[serde(default = "default_claude_code_oauth_token_env")]
        token_env: Option<String>,
        #[serde(default)]
        token_vault: Option<String>,
        #[serde(default = "default_claude_code_oauth_token_file")]
        token_file: Option<String>,
        #[serde(default = "default_claude_code_oauth_access_token_pointer")]
        access_token_pointer: String,
    },
    /// GCP service-account JSON key (for Vertex AI). The access token is minted
    /// from the key via the JWT-bearer grant and refreshed before expiry by
    /// `sb-credentials::ServiceAccountMinter`. Provide the key JSON via a file
    /// path or an env var holding the JSON.
    ServiceAccount {
        #[serde(default)]
        key_file: Option<String>,
        #[serde(default)]
        key_env: Option<String>,
        /// OAuth scope to request (defaults to cloud-platform).
        #[serde(default)]
        scope: Option<String>,
    },
    /// AWS SigV4 credentials. Used by Bedrock and kept in the account/lease
    /// boundary so account selection, lockout, and policy apply uniformly.
    AwsSigV4 {
        /// Access key id (env preferred).
        #[serde(default = "default_aws_access_env")]
        access_key_env: String,
        #[serde(default)]
        access_key: Option<String>,
        /// Secret access key (env preferred).
        #[serde(default = "default_aws_secret_env")]
        secret_key_env: String,
        #[serde(default)]
        secret_key: Option<String>,
        /// Optional STS session token.
        #[serde(default)]
        session_token_env: Option<String>,
        #[serde(default)]
        session_token: Option<String>,
    },
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
    /// Outbound egress for this account (overrides the provider's). Lets two
    /// accounts of the same provider exit from different proxies/IPs.
    #[serde(default)]
    pub egress: Option<String>,
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
    /// Optional upstream model id used by diagnostics/CLI smoke tests when the
    /// provider has no model-list endpoint. This does not affect routing.
    #[serde(default)]
    pub model_hint: Option<String>,
    /// Explicit multi-account list. If empty, a single default account is
    /// synthesized from the provider kind's legacy `api_key`/`api_key_env`.
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
    #[serde(default)]
    pub selection: SelectionStrategy,
    /// Round-robin stickiness (consecutive requests before rotating). Default 1.
    #[serde(default)]
    pub sticky: Option<u32>,
    /// Default outbound egress for this provider's accounts (an account can
    /// override). Falls back to `server.default_egress`, then `direct`.
    #[serde(default)]
    pub egress: Option<String>,
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
    /// Require native structured-output / JSON-Schema support. Also inferred
    /// from a request whose `response_format` is a JSON Schema.
    #[serde(default)]
    pub json_schema: Option<bool>,
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

/// A named, local-friendly route profile: ordered provider/model targets plus a
/// simple strategy. The runtime compiles this into the same route-planning path
/// as `routes`, preserving `RouteDecision` explainability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboConfig {
    #[serde(default)]
    pub strategy: ComboStrategy,
    #[serde(default)]
    pub require: RouteRequire,
    /// Ordered candidate list. In fallback mode the first is primary; in
    /// round-robin mode this order is rotated per request before planning.
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComboStrategy {
    #[default]
    Fallback,
    RoundRobin,
}

impl ComboStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            ComboStrategy::Fallback => "fallback",
            ComboStrategy::RoundRobin => "round_robin",
        }
    }
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

    /// Find the route whose `match.model` exactly equals the requested model.
    pub fn exact_route_for<'a>(&'a self, model: &str) -> Option<&'a RouteConfig> {
        self.routes
            .iter()
            .find(|r| r.match_.model.as_deref() == Some(model))
    }

    /// The first catch-all `*` route, if configured.
    pub fn wildcard_route(&self) -> Option<&RouteConfig> {
        self.routes
            .iter()
            .find(|r| r.match_.model.as_deref() == Some("*"))
    }

    /// A named combo profile, if the requested model is one.
    pub fn combo_for(&self, model: &str) -> Option<&ComboConfig> {
        self.combos.get(model)
    }

    /// Find the route whose `match.model` equals the requested model, or the
    /// first `*` route as default.
    pub fn route_for<'a>(&'a self, model: &str) -> Option<&'a RouteConfig> {
        self.exact_route_for(model)
            .or_else(|| self.wildcard_route())
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
    fn parses_state_store_startup_policy_forms() {
        let legacy = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
  state_store: "/tmp/switchback.sqlite"
providers:
  - id: mock
    type: mock
"#,
        )
        .unwrap();

        let legacy_store = legacy.server.state_store.as_ref().unwrap();
        assert_eq!(legacy_store.path(), "/tmp/switchback.sqlite");
        assert!(!legacy_store.required());

        let detailed = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
  state_store:
    path: "/tmp/switchback-required.sqlite"
    required: true
providers:
  - id: mock
    type: mock
"#,
        )
        .unwrap();

        let detailed_store = detailed.server.state_store.as_ref().unwrap();
        assert_eq!(detailed_store.path(), "/tmp/switchback-required.sqlite");
        assert!(detailed_store.required());
    }

    #[test]
    fn route_lookup_falls_back_to_star() {
        let cfg = Config::from_yaml(SAMPLE).unwrap();
        assert_eq!(cfg.route_for("coding").unwrap().name, "coding");
        // unknown model -> default `*` route
        assert_eq!(cfg.route_for("anything/else").unwrap().name, "default");
    }

    #[test]
    fn parses_simple_combo_profiles() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
combos:
  coder:
    strategy: round_robin
    require:
      streaming: true
    models:
      - "mock/sonnet"
      - "mock/gpt"
"#,
        )
        .unwrap();

        let combo = cfg.combo_for("coder").unwrap();
        assert_eq!(combo.strategy, ComboStrategy::RoundRobin);
        assert_eq!(combo.require.streaming, Some(true));
        assert_eq!(combo.models, vec!["mock/sonnet", "mock/gpt"]);
    }

    #[test]
    fn parses_client_profiles_for_native_proxy_clients() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: openai
    type: openai_compatible
    base_url: "https://api.openai.com/v1"
    accounts:
      - id: codex-team
        auth: { kind: oauth, refresh_env: CODEX_REFRESH, token_url: "https://oauth.example/token" }
  - id: anthropic
    type: anthropic
    accounts:
      - id: claude-team
        auth: { kind: api_key, env: ANTHROPIC_API_KEY }
client_profiles:
  - id: codex
    kind: codex
    models: ["coding"]
    accounts: ["openai/codex-team"]
  - id: claude-code
    kind: claude_code
    models: ["claude"]
    accounts: ["anthropic/claude-team"]
routes:
  - name: coding
    match: { model: "coding" }
    targets: ["openai/gpt-5.5"]
  - name: claude
    match: { model: "claude" }
    targets: ["anthropic/claude-sonnet"]
"#,
        )
        .unwrap();

        assert_eq!(cfg.client_profiles.len(), 2);
        assert_eq!(cfg.client_profiles[0].kind, ClientProfileKind::Codex);
        assert_eq!(
            cfg.client_profiles[0].kind.required_endpoints(),
            &["/v1/responses", "/v1/models"]
        );
        assert_eq!(cfg.client_profiles[1].kind.protocol(), "anthropic_messages");
        assert!(cfg.semantic_problems().is_empty());
    }

    #[test]
    fn client_profile_account_refs_are_validated() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
client_profiles:
  - id: codex
    kind: codex
    models: [""]
    accounts: ["mock/missing", "badref"]
"#,
        )
        .unwrap();

        let problems = cfg.semantic_problems().join("; ");
        assert!(problems.contains("client_profiles[0].models[0] is empty"));
        assert!(problems.contains("references unknown account `missing`"));
        assert!(problems.contains("must be `provider/account`"));
    }

    #[test]
    fn tenant_policy_references_are_validated() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
    accounts:
      - id: team
        auth: { kind: api_key, inline: "k" }
tenants:
  - id: acme
    allowed_routes: ["default"]
    allowed_providers: ["mock"]
    allowed_accounts: ["mock/team"]
routes:
  - name: default
    match: { model: "*" }
    targets: ["mock/echo"]
"#,
        )
        .unwrap();
        assert!(cfg.semantic_problems().is_empty());

        let invalid = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
tenants:
  - id: acme
    allowed_routes: ["missing"]
    allowed_providers: ["ghost"]
    allowed_accounts: ["mock/missing"]
routes:
  - name: default
    match: { model: "*" }
    targets: ["mock/echo"]
"#,
        )
        .unwrap();
        let problems = invalid.semantic_problems();
        assert!(problems
            .iter()
            .any(|problem| problem.contains("allowed_routes")));
        assert!(problems
            .iter()
            .any(|problem| problem.contains("allowed_providers")));
        assert!(problems
            .iter()
            .any(|problem| problem.contains("allowed_accounts")));
    }

    #[test]
    fn wasm_plugin_defaults_and_bounds_are_semantic_config() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
plugins:
  - type: wasm
    path: "/tmp/policy.wasm"
"#,
        )
        .unwrap();

        let PluginConfig::Wasm {
            failure_mode,
            timeout_ms,
            fuel,
            ..
        } = &cfg.plugins[0]
        else {
            panic!("expected wasm plugin");
        };
        assert_eq!(*failure_mode, PluginFailureMode::Open);
        assert_eq!(*timeout_ms, 10);
        assert_eq!(*fuel, 1_000_000);

        let invalid = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
plugins:
  - type: wasm
    path: ""
    timeout_ms: 0
    fuel: 0
"#,
        )
        .unwrap();
        let problems = invalid.semantic_problems();
        assert!(problems
            .iter()
            .any(|p| p.contains("path must not be empty")));
        assert!(problems.iter().any(|p| p.contains("timeout_ms")));
        assert!(problems.iter().any(|p| p.contains("fuel")));
    }

    #[test]
    fn inbound_api_keys_match_without_plain_equality() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
  api_key: "admin-secret"
tenants:
  - id: acme
api_keys:
  - key: "tenant-secret"
    tenant: acme
    project: api
    role: operator
providers:
  - id: mock
    type: mock
"#,
        )
        .unwrap();

        let (tenant, project, role) = cfg.principal_for_key("tenant-secret").unwrap();
        assert_eq!(tenant, "acme");
        assert_eq!(project, Some("api"));
        assert_eq!(role, ApiKeyRole::Operator);
        assert!(cfg.principal_for_key("tenant-secret-x").is_none());
        assert!(cfg.server_api_key_matches("admin-secret"));
        assert!(!cfg.server_api_key_matches("admin-secret-x"));

        assert!(constant_time_secret_eq("same", "same"));
        assert!(!constant_time_secret_eq("same", "same-but-longer"));
        // Digest-based compare: equal only for identical secrets, regardless of
        // length; differing same-length inputs and the empty string don't match.
        assert!(!constant_time_secret_eq("abcd", "abce"));
        assert!(!constant_time_secret_eq("short", ""));
        assert!(!constant_time_secret_eq("", "short"));
        let long = "x".repeat(4096);
        assert!(constant_time_secret_eq(&long, &long));
        assert!(!constant_time_secret_eq(&long, &"x".repeat(4095)));
    }

    #[test]
    fn inbound_api_key_env_and_hash_authenticate_without_inline_secret() {
        let env_name = "SWITCHBACK_TEST_INBOUND_API_KEY";
        std::env::set_var(env_name, "env-secret");
        let hashed = format!("sha256:{}", sha256_hex(b"hashed-secret"));
        let cfg = Config::from_yaml(&format!(
            r#"
server:
  bind: "127.0.0.1:0"
tenants:
  - id: acme
  - id: beta
api_keys:
  - key_env: "{env_name}"
    tenant: acme
    project: env
    role: operator
  - key_hash: "{hashed}"
    prefix: "sk_hash_"
    tenant: beta
providers:
  - id: mock
    type: mock
"#
        ))
        .unwrap();

        let (tenant, project, role) = cfg.principal_for_key("env-secret").unwrap();
        assert_eq!(tenant, "acme");
        assert_eq!(project, Some("env"));
        assert_eq!(role, ApiKeyRole::Operator);

        let (tenant, project, role) = cfg.principal_for_key("hashed-secret").unwrap();
        assert_eq!(tenant, "beta");
        assert_eq!(project, None);
        assert_eq!(role, ApiKeyRole::Client);
        assert!(cfg.principal_for_key("wrong-secret").is_none());
        assert!(!cfg.has_inline_secret_material());
        assert!(cfg.semantic_problems().is_empty());
        std::env::remove_var(env_name);
    }

    #[test]
    fn api_key_semantics_reject_ambiguous_sources_and_legacy_conflict() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
  api_key: "admin-secret"
tenants:
  - id: acme
api_keys:
  - key: "tenant-secret"
    key_env: "TENANT_SECRET"
    tenant: acme
providers:
  - id: mock
    type: mock
"#,
        )
        .unwrap();

        let problems = cfg.semantic_problems().join("; ");
        assert!(problems.contains("server.api_key and api_keys cannot both be configured"));
        assert!(problems.contains("must set exactly one of key, key_env, key_hash"));
    }

    #[test]
    fn combo_validation_catches_bad_targets() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: mock
    type: mock
combos:
  bad:
    models:
      - "ghost/model"
"#,
        )
        .unwrap();

        let problems = cfg.semantic_problems();
        assert!(problems.iter().any(|p| p.contains("unknown provider")));
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

    #[test]
    fn parses_native_oauth_account_sources_and_anthropic_bearer_scheme() {
        let cfg = Config::from_yaml(
            r#"
providers:
  - id: openai
    type: openai_compatible
    base_url: "https://api.openai.com/v1"
    accounts:
      - id: codex-native
        auth: { kind: codex_oauth }
  - id: anthropic
    type: anthropic
    auth_scheme: { kind: bearer }
    accounts:
      - id: claude-native
        auth: { kind: claude_code_oauth }
"#,
        )
        .expect("parse");

        match &cfg.providers[0].accounts[0].auth {
            AuthConfig::CodexOauth {
                token_env,
                token_file,
                access_token_pointer,
                ..
            } => {
                assert_eq!(token_env.as_deref(), Some("CODEX_ACCESS_TOKEN"));
                assert_eq!(token_file.as_deref(), Some("${HOME}/.codex/auth.json"));
                assert_eq!(access_token_pointer, "/tokens/access_token");
            }
            other => panic!("expected codex_oauth, got {other:?}"),
        }
        match &cfg.providers[1].kind {
            ProviderKind::Anthropic { auth_scheme, .. } => {
                assert_eq!(auth_scheme, &Some(AuthScheme::Bearer));
            }
            other => panic!("expected anthropic provider, got {other:?}"),
        }
        match &cfg.providers[1].accounts[0].auth {
            AuthConfig::ClaudeCodeOauth {
                token_env,
                token_file,
                access_token_pointer,
                ..
            } => {
                assert_eq!(token_env.as_deref(), Some("CLAUDE_CODE_OAUTH_TOKEN"));
                assert_eq!(
                    token_file.as_deref(),
                    Some("${HOME}/.claude/.credentials.json")
                );
                assert_eq!(access_token_pointer, "/claudeAiOauth/accessToken");
            }
            other => panic!("expected claude_code_oauth, got {other:?}"),
        }
    }

    #[test]
    fn parses_first_party_native_relay_provider_intent() {
        let cfg = Config::from_yaml(
            r#"
providers:
  - id: codex-relay
    type: codex_native_relay
  - id: claude-relay
    type: claude_code_native_relay
    base_url: "https://example.invalid/native"
"#,
        )
        .expect("parse");

        match &cfg.providers[0].kind {
            ProviderKind::CodexNativeRelay { base_url } => {
                assert_eq!(base_url, &None);
            }
            other => panic!("expected codex_native_relay, got {other:?}"),
        }
        match &cfg.providers[1].kind {
            ProviderKind::ClaudeCodeNativeRelay { base_url } => {
                assert_eq!(base_url.as_deref(), Some("https://example.invalid/native"));
            }
            other => panic!("expected claude_code_native_relay, got {other:?}"),
        }
    }

    #[test]
    fn semantic_validation_requires_vault_config_for_oauth_vault_refs() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
providers:
  - id: p
    type: openai_compatible
    base_url: "https://api.example.com/v1"
    accounts:
      - id: oauth
        auth:
          kind: oauth
          refresh_vault: oauth-refresh
          token_url: "https://oauth.example.com/token"
routes:
  - name: default
    match: { model: "*" }
    targets: ["p/model"]
"#,
        )
        .unwrap();

        let problems = cfg.semantic_problems();

        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("auth.refresh_vault")),
            "expected missing vault problem, got {problems:?}"
        );
    }

    #[test]
    fn semantic_validation_can_block_private_provider_urls() {
        let cfg = Config::from_yaml(
            r#"
server:
  bind: "127.0.0.1:0"
  block_private_networks: true
providers:
  - id: local
    type: openai_compatible
    base_url: "http://127.0.0.1:11434/v1"
    api_key: "k"
"#,
        )
        .unwrap();

        let problems = cfg.semantic_problems();

        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("providers[0].base_url")),
            "private provider URL should be rejected when block_private_networks is on: {problems:?}"
        );
    }
}
