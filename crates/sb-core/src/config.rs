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
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub max_concurrency: Option<u32>,
}

/// An inbound API key bound to a tenant (+ optional project label for
/// attribution). The key itself is a secret; it redacts in `Debug`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ApiKeyConfig {
    pub key: String,
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

impl std::fmt::Debug for ApiKeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyConfig")
            .field("key", &"[redacted]")
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
            if constant_time_secret_eq(&configured.key, key) {
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
            || self.api_keys.iter().any(|k| !k.key.trim().is_empty())
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
            if key.key.is_empty() {
                problems.push(format!("api_keys[{i}].key is empty"));
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
        for (i, key) in self.api_keys.iter().enumerate() {
            if !key.key.is_empty() && !api_key_values.insert(key.key.as_str()) {
                problems.push(format!("api_keys[{i}].key duplicates a previous API key"));
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
    let expected = expected.as_bytes();
    let presented = presented.as_bytes();
    let max_len = expected.len().max(presented.len());
    let mut diff = expected.len() ^ presented.len();
    for i in 0..max_len {
        let a = expected.get(i).copied().unwrap_or(0);
        let b = presented.get(i).copied().unwrap_or(0);
        diff |= usize::from(a ^ b);
    }
    diff == 0
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
        ProviderKind::Mock | ProviderKind::Bedrock { .. } => false,
        ProviderKind::OpenaiCompatible { api_key, .. }
        | ProviderKind::Anthropic { api_key, .. }
        | ProviderKind::Gemini { api_key, .. }
        | ProviderKind::Vertex { api_key, .. } => non_empty(api_key),
    }
}

fn auth_has_inline_secret_material(auth: &AuthConfig) -> bool {
    match auth {
        AuthConfig::None | AuthConfig::ServiceAccount { .. } => false,
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
        ProviderKind::Mock => Vec::new(),
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdempotencyConfig {
    /// Store rendered non-streaming response bodies for durable idempotency
    /// replay. Off by default because this persists model output.
    #[serde(default)]
    pub persist_response_bodies: bool,
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
            max_response_bytes: None,
            privacy_mode: PrivacyMode::default(),
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
        /// Refresh token (env preferred). With `token_url` it enables live
        /// refresh: an expired access token is refreshed before use.
        #[serde(default)]
        refresh_env: Option<String>,
        #[serde(default)]
        refresh: Option<String>,
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
