//! Egress pool — one prebuilt `reqwest::Client` per named outbound path.
//!
//! Lets an account/provider route its upstream calls through a declared
//! HTTP(S)/SOCKS5 proxy, choosing which IP/proxy each request exits from.
//!
//! Resolution is forgiving: an unknown, disabled, or (master-switch-off) egress
//! id falls back to `direct`, and [`EgressPool::effective`] reports the id that
//! was actually used so a trace can record the truth.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use sb_core::{Config, EgressKind, Timeouts};

/// The implicit, always-present no-proxy path.
pub const DIRECT: &str = "direct";

/// One resolved outbound path: a client (with its proxy, if any) plus an
/// optional client identity (custom User-Agent + headers) applied per request.
#[derive(Debug)]
pub struct EgressPath {
    id: String,
    client: reqwest::Client,
    user_agent: Option<String>,
    headers: Vec<(String, String)>,
}

impl EgressPath {
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// The effective egress id (what a trace should record).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Apply this path's client identity to a request: a custom `User-Agent`
    /// and any configured headers. No-op when none are configured. Auth-bearing
    /// headers are REFUSED — an egress identity selects a network path, it must
    /// never set or override credentials (the adapter applies auth afterwards).
    pub fn apply_identity(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(ua) = &self.user_agent {
            builder = builder.header(reqwest::header::USER_AGENT, ua);
        }
        for (name, value) in &self.headers {
            if is_auth_header(name) {
                tracing::warn!(
                    header = %name,
                    "egress identity tried to set an auth-bearing header — refused"
                );
                continue;
            }
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }
}

/// Headers an egress identity may NOT set — credentials belong to the lease, not
/// the network path. Matched case-insensitively.
fn is_auth_header(name: &str) -> bool {
    const DENY: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "x-api-key",
        "x-goog-api-key",
        "api-key",
        "cookie",
    ];
    let lower = name.to_ascii_lowercase();
    DENY.contains(&lower.as_str())
}

#[derive(Debug)]
pub struct EgressPool {
    /// Always contains `DIRECT`; plus one path per enabled egress.
    paths: HashMap<String, EgressPath>,
    /// Master switch — when false the pool only ever hands out `direct`.
    enabled: bool,
}

impl EgressPool {
    /// Build a path per enabled egress (sharing the server's timeouts). Fails
    /// fast on a malformed proxy URL or a proxy egress missing its url/url_env.
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let timeouts = cfg.server.timeouts;
        let enabled = cfg.server.egress_enabled;

        let mut paths = HashMap::new();
        paths.insert(
            DIRECT.to_string(),
            EgressPath {
                id: DIRECT.to_string(),
                client: build_client(&timeouts, None)?,
                user_agent: None,
                headers: Vec::new(),
            },
        );

        if enabled {
            for egress in &cfg.egress {
                if !egress.enabled {
                    continue; // toggled off → callers referencing it fall back
                }
                let proxy = match &egress.kind {
                    EgressKind::Direct => None,
                    EgressKind::Proxy { url, url_env } => {
                        let resolved =
                            resolve_url(url.as_deref(), url_env.as_deref()).ok_or_else(|| {
                                format!("egress `{}`: proxy needs `url` or `url_env`", egress.id)
                            })?;
                        if cfg.server.block_private_networks {
                            if let Some(reason) = private_url_reason(&resolved) {
                                return Err(format!(
                                    "egress `{}` proxy `{resolved}` is blocked: {reason}",
                                    egress.id
                                ));
                            }
                        }
                        Some(resolved)
                    }
                };
                let client = build_client(&timeouts, proxy.as_deref())
                    .map_err(|e| format!("egress `{}`: {e}", egress.id))?;
                paths.insert(
                    egress.id.clone(),
                    EgressPath {
                        id: egress.id.clone(),
                        client,
                        user_agent: egress.user_agent.clone(),
                        headers: egress
                            .headers
                            .iter()
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    },
                );
            }
        }

        Ok(Self { paths, enabled })
    }

    /// In-memory pool with only `direct` (for tests / no-egress configs).
    pub fn direct_only() -> Self {
        let mut paths = HashMap::new();
        paths.insert(
            DIRECT.to_string(),
            EgressPath {
                id: DIRECT.to_string(),
                client: reqwest::Client::new(),
                user_agent: None,
                headers: Vec::new(),
            },
        );
        Self {
            paths,
            enabled: true,
        }
    }

    /// The resolved path for an egress id. Unknown / disabled / master-off →
    /// `direct`. Carries both the client and the client identity.
    pub fn path(&self, egress_id: Option<&str>) -> &EgressPath {
        if self.enabled {
            if let Some(id) = egress_id {
                if let Some(path) = self.paths.get(id) {
                    return path;
                }
            }
        }
        self.paths.get(DIRECT).expect("direct path always present")
    }

    /// The client for an egress id (convenience over `path`).
    pub fn client(&self, egress_id: Option<&str>) -> &reqwest::Client {
        self.path(egress_id).client()
    }

    /// The egress id actually used (so a trace records the truth, e.g. `"direct"`
    /// when a disabled egress fell back).
    pub fn effective(&self, egress_id: Option<&str>) -> &str {
        self.path(egress_id).id()
    }
}

fn private_url_reason(url: &str) -> Option<String> {
    let host = host_from_url(url)?;
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Some("localhost host".to_string());
    }
    if let Ok(ip) = lower.parse::<IpAddr>() {
        let blocked = match ip {
            IpAddr::V4(ip) => {
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || (ip.octets()[0] == 169 && ip.octets()[1] == 254)
            }
            IpAddr::V6(ip) => {
                let first = ip.segments()[0];
                ip.is_loopback()
                    || ip.is_unspecified()
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80
            }
        };
        if blocked {
            return Some(format!("private or local IP host `{host}`"));
        }
    }
    None
}

fn host_from_url(url: &str) -> Option<String> {
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

/// Proxy URL precedence: env (more secure, keeps creds out of shared config) > inline.
fn resolve_url(url: Option<&str>, url_env: Option<&str>) -> Option<String> {
    if let Some(name) = url_env {
        if let Ok(value) = std::env::var(name) {
            return Some(value);
        }
    }
    url.map(String::from)
}

fn build_client(timeouts: &Timeouts, proxy: Option<&str>) -> Result<reqwest::Client, String> {
    // Same timeout shape as the default adapter client: no total timeout (would
    // cap long streams), connect_timeout fails fast, read_timeout bounds idle.
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(timeouts.connect_ms))
        .read_timeout(Duration::from_millis(timeouts.read_ms));
    if let Some(url) = proxy {
        let proxy = reqwest::Proxy::all(url).map_err(|e| format!("invalid proxy url: {e}"))?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(yaml: &str) -> Config {
        Config::from_yaml(yaml).unwrap()
    }

    #[test]
    fn direct_is_always_available() {
        let pool = EgressPool::from_config(&cfg_from("providers: []")).unwrap();
        assert_eq!(pool.effective(None), DIRECT);
        assert_eq!(pool.effective(Some("nope")), DIRECT, "unknown id → direct");
    }

    #[test]
    fn proxy_egress_is_selectable() {
        let pool = EgressPool::from_config(&cfg_from(
            r#"
egress:
  - id: viaproxy
    kind: proxy
    url: "http://127.0.0.1:9999"
"#,
        ))
        .unwrap();
        assert_eq!(pool.effective(Some("viaproxy")), "viaproxy");
    }

    #[test]
    fn disabled_egress_falls_back_to_direct() {
        let pool = EgressPool::from_config(&cfg_from(
            r#"
egress:
  - id: viaproxy
    kind: proxy
    url: "http://127.0.0.1:9999"
    enabled: false
"#,
        ))
        .unwrap();
        assert_eq!(
            pool.effective(Some("viaproxy")),
            DIRECT,
            "disabled → direct"
        );
    }

    #[test]
    fn master_switch_off_forces_direct() {
        let pool = EgressPool::from_config(&cfg_from(
            r#"
server:
  bind: "127.0.0.1:0"
  egress_enabled: false
egress:
  - id: viaproxy
    kind: proxy
    url: "http://127.0.0.1:9999"
"#,
        ))
        .unwrap();
        assert_eq!(
            pool.effective(Some("viaproxy")),
            DIRECT,
            "master off → direct"
        );
    }

    #[test]
    fn proxy_without_url_is_an_error() {
        let err = EgressPool::from_config(&cfg_from(
            r#"
egress:
  - id: broken
    kind: proxy
"#,
        ))
        .unwrap_err();
        assert!(err.contains("broken"), "error names the egress: {err}");
    }
}
