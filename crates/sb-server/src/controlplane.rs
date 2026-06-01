//! Control plane — read surface over the running configuration, for a dashboard
//! UI and a machine/AI-driven CLI to both observe settings without a rewrite.
//!
//! INVARIANT (AGENTS.md #3 — secrets never leave the process): every config view
//! goes through [`redact_config`], which strips inline secret material. Env var
//! NAMES and vault REFERENCES are kept (they are not secrets and are what an
//! operator needs to see); inline values, tokens, and proxy credentials are not.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sb_core::{Config, PluginConfig, ProviderConfig, ProviderKind, TenantConfig};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

use crate::tenancy::Principal;
use crate::AppState;

pub(crate) fn audit_context(
    source: impl Into<String>,
    detail: impl Into<String>,
    principal: &crate::tenancy::Principal,
) -> sb_runtime::AuditContext {
    sb_runtime::AuditContext::new(source, detail).with_actor(
        principal.role_name(),
        principal.tenant.clone(),
        principal.project.clone(),
    )
}

/// Keys whose string value is secret material and must be masked. Matched
/// exactly, so endpoint/name siblings (`token_url`, `token_env`, `api_key_env`,
/// `url_env`, `vault`, `source_ref`) are deliberately kept.
fn is_secret_key(key: &str) -> bool {
    matches!(
        key,
        "inline" | "token" | "refresh" | "client_secret" | "api_key" | "password" | "secret"
    )
}

/// Mask credentials in a proxy URL: `scheme://user:pass@host` → `scheme://[redacted]@host`.
fn mask_url_creds(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some((_creds, host)) = rest.rsplit_once('@') {
            return format!("{scheme}://[redacted]@{host}");
        }
    }
    url.to_string()
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_secret_key(key) {
                    if let Value::String(s) = val {
                        if !s.is_empty() {
                            *s = "[redacted]".to_string();
                        }
                    }
                } else if key == "url" {
                    // Egress proxy url (provider endpoints use the `base_url` key).
                    if let Value::String(s) = val {
                        *s = mask_url_creds(s);
                    }
                } else {
                    redact_value(val);
                }
            }
        }
        Value::Array(arr) => arr.iter_mut().for_each(redact_value),
        _ => {}
    }
}

fn redact_inbound_api_keys(value: &mut Value) {
    let Some(keys) = value.get_mut("api_keys").and_then(Value::as_array_mut) else {
        return;
    };
    for entry in keys {
        if let Some(key) = entry.get("key").and_then(Value::as_str) {
            if !key.is_empty() {
                entry["key"] = Value::String("[redacted]".to_string());
            }
        }
        if let Some(hash) = entry.get("key_hash").and_then(Value::as_str) {
            if !hash.is_empty() {
                entry["key_hash"] = Value::String("sha256:[redacted]".to_string());
            }
        }
    }
}

/// The effective config as JSON with all secret material masked. The one place
/// a config view is produced for external eyes (HTTP + CLI share it).
pub fn redact_config(cfg: &Config) -> Value {
    let mut v = serde_json::to_value(cfg).unwrap_or(Value::Null);
    redact_value(&mut v);
    redact_inbound_api_keys(&mut v);
    v
}

pub(crate) fn tenant_scope(principal: &Principal) -> Option<&str> {
    if principal.is_admin() {
        None
    } else {
        principal.tenant.as_deref()
    }
}

pub(crate) fn scoped_config_for_principal(cfg: &Config, principal: &Principal) -> Config {
    let Some(tenant_id) = tenant_scope(principal) else {
        return cfg.clone();
    };
    let Some(tenant) = cfg.tenant(tenant_id) else {
        return empty_scoped_config(cfg);
    };

    let mut scoped = cfg.clone();
    scoped.vault = None;
    scoped.catalog = None;
    scoped.tenants.retain(|t| t.id == tenant_id);
    scoped.api_keys.retain(|key| key.tenant == tenant_id);
    scoped.plugins.retain(plugin_visible_to_tenant);

    scoped.providers.retain_mut(|provider| {
        if !provider_visible_to_tenant(tenant, provider) {
            return false;
        }
        if !tenant.allowed_accounts.is_empty() {
            provider
                .accounts
                .retain(|account| account_visible_to_tenant(tenant, &provider.id, &account.id));
        }
        true
    });

    scoped.routes.retain_mut(|route| {
        if !route_name_visible_to_tenant(tenant, &route.name) {
            return false;
        }
        route
            .targets
            .retain(|target| target_visible_to_tenant(tenant, cfg, target));
        !route.targets.is_empty()
    });

    scoped.combos.retain(|name, combo| {
        route_name_visible_to_tenant(tenant, name)
            && combo
                .models
                .iter()
                .any(|target| target_visible_to_tenant(tenant, cfg, target))
    });
    for combo in scoped.combos.values_mut() {
        combo
            .models
            .retain(|target| target_visible_to_tenant(tenant, cfg, target));
    }

    let visible_egress = visible_egress_ids(&scoped);
    scoped
        .egress
        .retain(|egress| visible_egress.contains(&egress.id));
    scoped
}

pub(crate) fn redact_config_for_principal(cfg: &Config, principal: &Principal) -> Value {
    let scoped = scoped_config_for_principal(cfg, principal);
    let mut value = redact_config(&scoped);
    if tenant_scope(principal).is_some() {
        redact_tenant_operator_config_shape(&mut value);
    }
    value
}

pub(crate) fn provider_visible_to_principal(
    cfg: &Config,
    principal: &Principal,
    provider: &ProviderConfig,
) -> bool {
    match tenant_scope(principal) {
        Some(tenant_id) => cfg
            .tenant(tenant_id)
            .map(|tenant| provider_visible_to_tenant(tenant, provider))
            .unwrap_or(false),
        None => true,
    }
}

pub(crate) fn account_visible_to_principal(
    cfg: &Config,
    principal: &Principal,
    provider_id: &str,
    account_id: &str,
) -> bool {
    match tenant_scope(principal) {
        Some(tenant_id) => cfg
            .tenant(tenant_id)
            .map(|tenant| account_visible_to_tenant(tenant, provider_id, account_id))
            .unwrap_or(false),
        None => true,
    }
}

fn empty_scoped_config(cfg: &Config) -> Config {
    let mut scoped = cfg.clone();
    scoped.vault = None;
    scoped.catalog = None;
    scoped.providers.clear();
    scoped.combos.clear();
    scoped.routes.clear();
    scoped.tenants.clear();
    scoped.api_keys.clear();
    scoped.plugins.clear();
    scoped.egress.clear();
    scoped
}

fn redact_tenant_operator_config_shape(value: &mut Value) {
    let Value::Object(map) = value else {
        return;
    };
    map.remove("vault");
    map.remove("catalog");
    if let Some(Value::Object(server)) = map.get_mut("server") {
        let keep = [
            "cost_aware",
            "latency_aware",
            "hedge_enabled",
            "retry_max",
            "cost_max_per_mtok",
            "cost_allow_free",
            "cost_allow_promo",
            "cost_allow_aggregator",
            "unknown_cost_policy",
            "unknown_context_policy",
            "strict_schema_downlevel",
            "privacy_mode",
        ];
        server.retain(|key, _| keep.contains(&key.as_str()));
    }
}

fn provider_visible_to_tenant(tenant: &TenantConfig, provider: &ProviderConfig) -> bool {
    if !tenant.allowed_providers.is_empty()
        && !tenant
            .allowed_providers
            .iter()
            .any(|allowed| allowed == &provider.id)
    {
        return false;
    }
    if tenant.allowed_accounts.is_empty() {
        return true;
    }
    if provider.accounts.is_empty() {
        return account_visible_to_tenant(tenant, &provider.id, "default");
    }
    provider
        .accounts
        .iter()
        .any(|account| account_visible_to_tenant(tenant, &provider.id, &account.id))
}

fn account_visible_to_tenant(tenant: &TenantConfig, provider_id: &str, account_id: &str) -> bool {
    tenant.allowed_accounts.is_empty()
        || tenant
            .allowed_accounts
            .iter()
            .any(|allowed| allowed == &format!("{provider_id}/{account_id}"))
}

fn route_name_visible_to_tenant(tenant: &TenantConfig, route_name: &str) -> bool {
    tenant.allowed_routes.is_empty()
        || tenant.allowed_routes.iter().any(|allowed| {
            allowed == route_name
                || route_name == format!("combo/{allowed}")
                || route_name.ends_with(&format!(" via {allowed}"))
        })
}

fn target_visible_to_tenant(tenant: &TenantConfig, cfg: &Config, target: &str) -> bool {
    let Some(provider_id) = target.split_once('/').map(|(provider, _)| provider) else {
        return true;
    };
    cfg.providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .map(|provider| provider_visible_to_tenant(tenant, provider))
        .unwrap_or(false)
}

fn plugin_visible_to_tenant(plugin: &PluginConfig) -> bool {
    matches!(plugin, PluginConfig::RequestTag { .. })
}

fn visible_egress_ids(cfg: &Config) -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    if let Some(egress) = cfg.server.default_egress.as_deref() {
        ids.insert(egress.to_string());
    }
    for provider in &cfg.providers {
        if let Some(egress) = provider.egress.as_deref() {
            ids.insert(egress.to_string());
        }
        for account in &provider.accounts {
            if let Some(egress) = account.egress.as_deref() {
                ids.insert(egress.to_string());
            }
        }
    }
    ids.remove("direct");
    ids
}

/// Navigate a dotted path (`server.cost_aware`, `providers.0.id`) into a value.
pub fn pointer_get<'a>(value: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut cur = value;
    for seg in dotted.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

pub fn provider_type_name(kind: &ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Mock => "mock",
        ProviderKind::OpenaiCompatible { .. } => "openai_compatible",
        ProviderKind::Anthropic { .. } => "anthropic",
        ProviderKind::Gemini { .. } => "gemini",
        ProviderKind::Vertex { .. } => "vertex",
        ProviderKind::Bedrock { .. } => "bedrock",
    }
}

// --- HTTP handlers ---------------------------------------------------------

/// `GET /v1/config` — the full effective config, redacted.
pub async fn config_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<Value> {
    let snap = state.snapshot();
    let mut v = redact_config_for_principal(&snap.config, &principal);
    if let Value::Object(map) = &mut v {
        map.insert("revision".to_string(), json!(snap.revision));
        if let Some(tenant) = tenant_scope(&principal) {
            map.insert("scope".to_string(), json!({ "tenant": tenant }));
        }
    }
    Json(v)
}

/// `GET /v1/providers` — per-provider summary (id, type, egress, account ids,
/// routing-relevant feature toggles). The dashboard/CLI's at-a-glance view.
pub async fn providers_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<Value> {
    let snap = state.snapshot();
    let scoped = scoped_config_for_principal(&snap.config, &principal);
    let providers: Vec<Value> = snap
        .config
        .providers
        .iter()
        .filter(|p| provider_visible_to_principal(&snap.config, &principal, p))
        .map(|p| {
            let accounts = snap
                .resolver
                .account_ids(&p.id)
                .into_iter()
                .filter(|account| {
                    account_visible_to_principal(&snap.config, &principal, &p.id, account)
                })
                .collect::<Vec<_>>();
            json!({
                "id": p.id,
                "type": provider_type_name(&p.kind),
                "egress": p.egress,
                "selection": format!("{:?}", p.selection).to_lowercase(),
                "accounts": accounts,
            })
        })
        .collect();

    let s = &scoped.server;
    Json(json!({
        "providers": providers,
        "routing": {
            "cost_aware": s.cost_aware,
            "latency_aware": s.latency_aware,
            "cost_max_per_mtok": s.cost_max_per_mtok,
            "allow_free": s.cost_allow_free,
            "allow_promo": s.cost_allow_promo,
            "allow_aggregator": s.cost_allow_aggregator,
            "default_provider": s.default_provider,
        },
        "egress": {
            "enabled": s.egress_enabled,
            "default": s.default_egress,
            "paths": scoped.egress.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
        },
    }))
}

/// The current live knobs + the config revision they belong to.
fn runtime_json(state: &AppState) -> Value {
    let snap = state.snapshot();
    let mut v = serde_json::to_value(&snap.runtime).unwrap_or(Value::Null);
    if let Value::Object(map) = &mut v {
        map.insert("revision".to_string(), json!(snap.revision));
    }
    v
}

/// `GET /v1/runtime` — the live, runtime-toggleable knobs + revision.
pub async fn runtime_get(State(state): State<AppState>) -> Json<Value> {
    Json(runtime_json(&state))
}

/// `GET /v1/revisions` — published config-revision history (newest first). Each
/// entry is metadata only (revision, config hash, source, timestamp). Empty +
/// `persistence: disabled` when no `server.state_store` is configured.
pub async fn revisions_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<Value> {
    match state.engine.store() {
        Some(store) => {
            let mut visible_revisions = None;
            if let Some(tenant) = tenant_scope(&principal) {
                match store.list_audit(100) {
                    Ok(entries) => {
                        visible_revisions = Some(
                            entries
                                .into_iter()
                                .filter(|entry| entry.actor_tenant.as_deref() == Some(tenant))
                                .map(|entry| entry.revision)
                                .collect::<std::collections::HashSet<_>>(),
                        );
                    }
                    Err(e) => {
                        return Json(json!({ "revisions": [], "error": e.to_string() }));
                    }
                }
            }
            match store.list_revisions(100) {
                Ok(mut revs) => {
                    if let Some(visible) = visible_revisions {
                        revs.retain(|rev| visible.contains(&rev.revision));
                        Json(
                            json!({ "revisions": revs, "scope": { "tenant": tenant_scope(&principal) } }),
                        )
                    } else {
                        Json(json!({ "revisions": revs }))
                    }
                }
                Err(e) => Json(json!({ "revisions": [], "error": e.to_string() })),
            }
        }
        None => Json(json!({ "revisions": [], "persistence": "disabled" })),
    }
}

/// `GET /v1/audit` — control-plane change audit log (newest first): one entry per
/// bootstrap / reload / runtime-knob change.
pub async fn audit_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<Value> {
    match state.engine.store() {
        Some(store) => match store.list_audit(100) {
            Ok(mut entries) => {
                if let Some(tenant) = tenant_scope(&principal) {
                    entries.retain(|entry| entry.actor_tenant.as_deref() == Some(tenant));
                    Json(json!({ "audit": entries, "scope": { "tenant": tenant } }))
                } else {
                    Json(json!({ "audit": entries }))
                }
            }
            Err(e) => Json(json!({ "audit": [], "error": e.to_string() })),
        },
        None => Json(json!({ "audit": [], "persistence": "disabled" })),
    }
}

/// `GET /v1/health` — the non-secret account-pool view the router uses: per
/// provider, how many accounts are currently usable out of the total, and whether
/// the circuit is open. This is the model-agnostic (account-wide) view; routing
/// stamps the per-model count onto each candidate at decision time.
pub async fn health_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<Value> {
    let snap = state.snapshot();
    let providers: Vec<Value> = snap
        .config
        .providers
        .iter()
        .filter(|p| provider_visible_to_principal(&snap.config, &principal, p))
        .map(|p| {
            let ph = snap.resolver.pool_health(&p.id, "");
            let accounts = snap
                .resolver
                .account_health(&p.id, "")
                .into_iter()
                .filter(|account| {
                    account_visible_to_principal(&snap.config, &principal, &p.id, &account.id)
                })
                .collect::<Vec<_>>();
            let accounts_total = accounts.len();
            let accounts_healthy = accounts.iter().filter(|account| account.healthy).count();
            json!({
                "id": p.id,
                "accounts_total": accounts_total,
                "accounts_healthy": accounts_healthy,
                "accounts": accounts,
                "circuit_open": ph.circuit_open,
                "status": if ph.circuit_open || accounts_healthy == 0 { "degraded" } else { "healthy" },
            })
        })
        .collect();
    let healthy = providers
        .iter()
        .filter(|p| p["status"] == "healthy")
        .count();
    Json(json!({
        "providers": providers,
        "summary": { "providers": providers.len(), "healthy": healthy },
        "admission": {
            "max_concurrency": state.admission.limit(),
            "available": state.admission.available(),
        },
        "revision": snap.revision,
    }))
}

/// `GET /v1/plugins` — the built-in plugins active in the current snapshot, in
/// run order. The control-plane view of the tier-1 plugin chain (Oracle #6).
pub async fn plugins_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Json<Value> {
    let snap = state.snapshot();
    let plugins = if tenant_scope(&principal).is_some() {
        scoped_config_for_principal(&snap.config, &principal)
            .plugins
            .iter()
            .map(|plugin| match plugin {
                PluginConfig::ModelBlocklist { .. } => "model_blocklist",
                PluginConfig::RequestTag { .. } => "request_tag",
                PluginConfig::EgressPin { .. } => "egress_pin",
                PluginConfig::Wasm { .. } => "wasm",
            })
            .map(str::to_string)
            .collect::<Vec<_>>()
    } else {
        snap.plugins.names()
    };
    Json(json!({ "plugins": plugins, "revision": snap.revision }))
}

/// `GET /v1/tenants` — configured tenants with their hard limits and live status:
/// attributed spend vs `budget_usd`, and in-flight count vs `max_concurrency`.
/// The per-tenant quota surface (no secrets — keys are never listed here).
pub async fn tenants_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
) -> Json<Value> {
    let snap = state.snapshot();
    let summary = state.ledger.summary();
    let scoped_tenant = tenant_scope(&principal);
    let tenants: Vec<Value> = snap
        .config
        .tenants
        .iter()
        .filter(|t| scoped_tenant.map(|tenant| t.id == tenant).unwrap_or(true))
        .map(|t| {
            let spent_usd = summary
                .by_tenant
                .get(&t.id)
                .map(|(_n, micros)| *micros as f64 / 1_000_000.0)
                .unwrap_or(0.0);
            json!({
                "id": t.id,
                "budget_usd": t.budget_usd,
                "spent_usd": spent_usd,
                "over_budget": t.budget_usd.map(|b| spent_usd >= b).unwrap_or(false),
                "max_concurrency": t.max_concurrency,
                "in_flight": crate::tenancy::in_flight(&state, &t.id),
            })
        })
        .collect();
    let keys = if let Some(tenant) = scoped_tenant {
        snap.config
            .api_keys
            .iter()
            .filter(|key| key.tenant == tenant)
            .count()
    } else {
        snap.config.api_keys.len()
    };
    Json(json!({ "tenants": tenants, "keys": keys }))
}

/// `GET /v1/usage/events` — the most recent durably-recorded usage events (newest
/// first). The `/v1/usage` summary aggregates these and survives restarts; this is
/// the per-event detail. Metadata only (tokens, cost, latency) — never content.
pub async fn usage_events_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
) -> Json<Value> {
    match state.engine.store() {
        Some(store) => match store.recent_usage(100) {
            Ok(mut events) => {
                if let Some(tenant) = tenant_scope(&principal) {
                    events.retain(|event| event.tenant.as_deref() == Some(tenant));
                }
                Json(json!({ "events": events }))
            }
            Err(e) => Json(json!({ "events": [], "error": e.to_string() })),
        },
        None => Json(json!({ "events": [], "persistence": "disabled" })),
    }
}

/// `POST /v1/reload` — re-read the config file and hot-swap a new snapshot.
pub async fn reload_endpoint(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
) -> Response {
    match state.reload_from_file_with_audit(audit_context(
        "file_reload",
        "config file reload",
        &principal,
    )) {
        Ok(revision) => Json(json!({ "ok": true, "revision": revision })).into_response(),
        Err(e) => (
            if e.contains("state store") {
                axum::http::StatusCode::INTERNAL_SERVER_ERROR
            } else {
                axum::http::StatusCode::BAD_REQUEST
            },
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

/// Partial update for the live knobs (all fields optional).
#[derive(serde::Deserialize)]
pub struct RuntimePatch {
    #[serde(default)]
    pub cost_aware: Option<bool>,
    #[serde(default)]
    pub latency_aware: Option<bool>,
    #[serde(default)]
    pub hedge_enabled: Option<bool>,
    #[serde(default)]
    pub retry_max: Option<u32>,
    #[serde(default)]
    pub budget_max_usd: NullablePatch<f64>,
}

/// JSON-patch-like nullable field: missing = no change, `null` = clear,
/// concrete value = set.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum NullablePatch<T> {
    #[default]
    Unset,
    Set(Option<T>),
}

impl<'de, T> Deserialize<'de> for NullablePatch<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(Self::Set)
    }
}

/// `PATCH /v1/runtime` — flip operational knobs without a restart. Returns the
/// new live state. Structural config (providers/routes) is not touched.
pub async fn runtime_patch(
    State(state): State<AppState>,
    Extension(principal): Extension<crate::tenancy::Principal>,
    Json(patch): Json<RuntimePatch>,
) -> Response {
    if matches!(patch.budget_max_usd, NullablePatch::Set(Some(v)) if !v.is_finite() || v < 0.0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "budget_max_usd must be a finite non-negative number or null"
            })),
        )
            .into_response();
    }
    if matches!(patch.retry_max, Some(v) if v > 10) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "retry_max must be <= 10" })),
        )
            .into_response();
    }

    // Reuses the current registry/resolver (health/credential state preserved),
    // swaps in the new knobs, bumps the revision.
    let result = state.update_runtime_with_audit(
        |rt| {
            if let Some(v) = patch.cost_aware {
                rt.cost_aware = v;
            }
            if let Some(v) = patch.latency_aware {
                rt.latency_aware = v;
            }
            if let Some(v) = patch.hedge_enabled {
                rt.hedge_enabled = v;
            }
            if let Some(v) = patch.retry_max {
                rt.retry_max = v;
            }
            if let NullablePatch::Set(v) = patch.budget_max_usd {
                rt.budget_max_usd = v;
            }
        },
        audit_context("runtime_patch", "", &principal),
    );
    match result {
        Ok(_) => Json(runtime_json(&state)).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "ok": false, "error": e })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
server:
  bind: "127.0.0.1:0"
  api_key: "sk-super-secret"
  cost_aware: true
egress:
  - id: proxied
    kind: proxy
    url: "socks5://user:hunter2@10.0.0.1:1080"
providers:
  - id: openai
    type: openai_compatible
    base_url: "https://api.openai.com/v1"
    api_key_env: "OPENAI_API_KEY"
    accounts:
      - id: a
        auth: { kind: api_key, inline: "sk-INLINE-LEAK" }
      - id: b
        auth: { kind: oauth, refresh: "rt-LEAK", token_url: "https://oauth/token", client_secret: "cs-LEAK" }
tenants:
  - id: acme
api_keys:
  - key: "sk-tenant-secret"
    tenant: acme
"#;

    #[test]
    fn redaction_masks_every_secret_but_keeps_names() {
        let cfg = Config::from_yaml(CFG).unwrap();
        let json = serde_json::to_string(&redact_config(&cfg)).unwrap();
        // No secret VALUE survives.
        for leak in [
            "sk-super-secret",
            "sk-INLINE-LEAK",
            "rt-LEAK",
            "cs-LEAK",
            "hunter2",
            "sk-tenant-secret",
        ] {
            assert!(!json.contains(leak), "redaction leaked `{leak}`");
        }
        // Non-secret references ARE kept (operator needs them).
        assert!(
            json.contains("OPENAI_API_KEY"),
            "env name should be visible"
        );
        assert!(
            json.contains("https://oauth/token"),
            "token_url is an endpoint, not a secret"
        );
        assert!(json.contains("api.openai.com"), "base_url kept");
        assert!(
            json.contains("[redacted]@10.0.0.1:1080"),
            "proxy creds masked, host kept"
        );
    }

    #[test]
    fn pointer_navigates_nested_values() {
        let cfg = Config::from_yaml(CFG).unwrap();
        let v = redact_config(&cfg);
        assert_eq!(pointer_get(&v, "server.cost_aware"), Some(&json!(true)));
        assert_eq!(pointer_get(&v, "providers.0.id"), Some(&json!("openai")));
        assert!(pointer_get(&v, "server.nope").is_none());
    }
}
