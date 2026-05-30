//! Egress pool — one prebuilt `reqwest::Client` per named outbound path.
//!
//! Lets an account/provider route its upstream calls through a declared proxy so
//! different accounts call "from different places". This is **network-path
//! selection only** (HTTP(S)/SOCKS5 proxy): it changes which IP/proxy the HTTPS
//! request exits from. It does NOT touch TLS/JA3 fingerprints or impersonate any
//! client — that is a separate, default-off layer (see docs/design).
//!
//! Resolution is forgiving: an unknown, disabled, or (master-switch-off) egress
//! id falls back to `direct`, and [`EgressPool::effective`] reports the id that
//! was actually used so a trace can record the truth.

use std::collections::HashMap;
use std::time::Duration;

use sb_core::{Config, EgressKind, Timeouts};

/// The implicit, always-present no-proxy path.
pub const DIRECT: &str = "direct";

#[derive(Debug)]
pub struct EgressPool {
    /// Always contains `DIRECT`; plus one client per enabled proxy egress.
    clients: HashMap<String, reqwest::Client>,
    /// Master switch — when false the pool only ever hands out `direct`.
    enabled: bool,
}

impl EgressPool {
    /// Build a client per enabled egress (sharing the server's timeouts). Fails
    /// fast on a malformed proxy URL or a proxy egress missing its url/url_env.
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let timeouts = cfg.server.timeouts;
        let enabled = cfg.server.egress_enabled;

        let mut clients = HashMap::new();
        clients.insert(DIRECT.to_string(), build_client(&timeouts, None)?);

        if enabled {
            for egress in &cfg.egress {
                if !egress.enabled {
                    continue; // toggled off → callers referencing it fall back
                }
                let proxy = match &egress.kind {
                    EgressKind::Direct => None,
                    EgressKind::Proxy { url, url_env } => Some(
                        resolve_url(url.as_deref(), url_env.as_deref()).ok_or_else(|| {
                            format!("egress `{}`: proxy needs `url` or `url_env`", egress.id)
                        })?,
                    ),
                };
                let client = build_client(&timeouts, proxy.as_deref())
                    .map_err(|e| format!("egress `{}`: {e}", egress.id))?;
                clients.insert(egress.id.clone(), client);
            }
        }

        Ok(Self { clients, enabled })
    }

    /// In-memory pool with only `direct` (for tests / no-egress configs).
    pub fn direct_only() -> Self {
        let mut clients = HashMap::new();
        clients.insert(DIRECT.to_string(), reqwest::Client::new());
        Self {
            clients,
            enabled: true,
        }
    }

    /// The client for an egress id. Unknown / disabled / master-off → `direct`.
    pub fn client(&self, egress_id: Option<&str>) -> &reqwest::Client {
        self.resolve(egress_id).1
    }

    /// The egress id actually used for `egress_id` (so a trace records the truth,
    /// e.g. `"direct"` when a disabled egress fell back).
    pub fn effective(&self, egress_id: Option<&str>) -> &str {
        self.resolve(egress_id).0
    }

    fn resolve(&self, egress_id: Option<&str>) -> (&str, &reqwest::Client) {
        if self.enabled {
            if let Some(id) = egress_id {
                if let Some((key, client)) = self.clients.get_key_value(id) {
                    return (key.as_str(), client);
                }
            }
        }
        let (key, client) = self
            .clients
            .get_key_value(DIRECT)
            .expect("direct client always present");
        (key.as_str(), client)
    }
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
        assert_eq!(pool.effective(Some("viaproxy")), DIRECT, "disabled → direct");
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
        assert_eq!(pool.effective(Some("viaproxy")), DIRECT, "master off → direct");
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
