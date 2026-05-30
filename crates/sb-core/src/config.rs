//! YAML config (v1 control plane). Compiled once into an in-memory snapshot;
//! never read in the hot path per-request.

use serde::{Deserialize, Serialize};
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

/// One configured built-in plugin. `type` selects the built-in; the rest are its
/// settings. The public Wasm tier (Oracle #6 tier 2) would add a `wasm` variant.
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
}

impl std::fmt::Debug for ApiKeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyConfig")
            .field("key", &"[redacted]")
            .field("tenant", &self.tenant)
            .field("project", &self.project)
            .finish()
    }
}

impl Config {
    /// Resolve an inbound bearer token to its `(tenant, project)`. `None` = the
    /// key is not in `api_keys`.
    pub fn principal_for_key(&self, key: &str) -> Option<(&str, Option<&str>)> {
        self.api_keys
            .iter()
            .find(|k| k.key == key)
            .map(|k| (k.tenant.as_str(), k.project.as_deref()))
    }

    /// The tenant record by id, if declared (for its quota limits).
    pub fn tenant(&self, id: &str) -> Option<&TenantConfig> {
        self.tenants.iter().find(|t| t.id == id)
    }
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
    /// Optional path to a SQLite state store (durable control-plane state). When
    /// set, every published config revision + an audit row per reload/runtime
    /// change are persisted here, surfaced at `/v1/revisions` and `/v1/audit`.
    /// Metadata only (revision, config hash, source, timestamp) — no config body,
    /// so no secrets land in the DB. Unset = persistence disabled (in-memory only).
    #[serde(default)]
    pub state_store: Option<String>,
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
    /// Optional price ceiling (blended USD per 1M tokens): cost-aware routing
    /// rejects any priced candidate above it (OpenRouter `max_price` idea). A
    /// candidate with no known price is never rejected on cost.
    #[serde(default)]
    pub cost_max_per_mtok: Option<f64>,
    /// Latency-aware routing toggle: order candidates fastest-first by an EWMA
    /// of observed upstream latency. `cost_aware` wins when both are on.
    #[serde(default)]
    pub latency_aware: bool,
    /// Cost-routing policy gates (all default-allow). Set false to exclude that
    /// lane from cost-aware routing: `cost_allow_free` (free tiers / price 0),
    /// `cost_allow_promo` (time-boxed promo prices), `cost_allow_aggregator`
    /// (third-party open-weight hosts).
    #[serde(default = "default_true")]
    pub cost_allow_free: bool,
    #[serde(default = "default_true")]
    pub cost_allow_promo: bool,
    #[serde(default = "default_true")]
    pub cost_allow_aggregator: bool,
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
            compress_tool_results: false,
            usage_log: None,
            trace_log: None,
            state_store: None,
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
            default_egress: None,
            egress_enabled: true,
            otel_endpoint: None,
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
    /// endpoint, authenticated with an OAuth2 Bearer token (e.g. from
    /// `gcloud auth print-access-token`). Same codec as Gemini, different URL +
    /// auth — i.e. mostly data on the `WireCodec × AuthScheme` seam. (Automatic
    /// service-account JWT refresh is a follow-on; supply a token for now.)
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

/// How a provider attaches its credential on the wire — the "auth" half of the
/// `AuthScheme × WireCodec` decomposition (audit §9.6). Composing this with a
/// wire codec means most API-key providers are *data*, not a new adapter. The
/// `Signed`/`ServiceAccount` variants (AWS SigV4, GCP JWT) are the seam for
/// Bedrock/Vertex; they're declared but not yet implemented.
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
