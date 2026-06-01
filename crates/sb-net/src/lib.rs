//! Shared hosted-mode network safety guard.
//!
//! `server.block_private_networks` is one policy, so provider upstreams,
//! OAuth/service-account token endpoints, and proxy URLs should all go through
//! the same literal-host + DNS private-IP checks.

use std::net::ToSocketAddrs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkUrlKind {
    ProviderUpstream,
    OauthToken,
    ServiceAccountToken,
    Proxy,
}

impl NetworkUrlKind {
    fn label(self) -> &'static str {
        match self {
            Self::ProviderUpstream => "provider upstream",
            Self::OauthToken => "OAuth token endpoint",
            Self::ServiceAccountToken => "service-account token endpoint",
            Self::Proxy => "proxy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkGuardError {
    InvalidUrl(String),
    ResolveFailed(String),
    BlockedPrivate(String),
}

impl std::fmt::Display for NetworkGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUrl(message)
            | Self::ResolveFailed(message)
            | Self::BlockedPrivate(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for NetworkGuardError {}

pub async fn guard_url(
    url: &str,
    kind: NetworkUrlKind,
    block_private_networks: bool,
) -> Result<(), NetworkGuardError> {
    if !block_private_networks {
        return Ok(());
    }
    let (host, port) = guard_literal_url(url, kind)?;
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    let resolved = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| {
            NetworkGuardError::ResolveFailed(format!("resolve {} host `{host}`: {e}", kind.label()))
        })?;
    for addr in resolved {
        if sb_core::private_ip_reason(addr.ip()).is_some() {
            return Err(NetworkGuardError::BlockedPrivate(format!(
                "blocked {} host `{host}` resolving to private/local IP `{}`",
                kind.label(),
                addr.ip()
            )));
        }
    }
    Ok(())
}

pub fn guard_url_blocking(
    url: &str,
    kind: NetworkUrlKind,
    block_private_networks: bool,
) -> Result<(), NetworkGuardError> {
    if !block_private_networks {
        return Ok(());
    }
    let (host, port) = guard_literal_url(url, kind)?;
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    let resolved = (host.as_str(), port).to_socket_addrs().map_err(|e| {
        NetworkGuardError::ResolveFailed(format!("resolve {} host `{host}`: {e}", kind.label()))
    })?;
    for addr in resolved {
        if sb_core::private_ip_reason(addr.ip()).is_some() {
            return Err(NetworkGuardError::BlockedPrivate(format!(
                "blocked {} host `{host}` resolving to private/local IP `{}`",
                kind.label(),
                addr.ip()
            )));
        }
    }
    Ok(())
}

fn guard_literal_url(url: &str, kind: NetworkUrlKind) -> Result<(String, u16), NetworkGuardError> {
    if let Some(reason) = sb_core::private_url_reason(url) {
        return Err(NetworkGuardError::BlockedPrivate(format!(
            "blocked private-network {} URL `{url}`: {reason}",
            kind.label()
        )));
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| {
        NetworkGuardError::InvalidUrl(format!("invalid {} URL `{url}`: {e}", kind.label()))
    })?;
    let host = parsed.host_str().ok_or_else(|| {
        NetworkGuardError::InvalidUrl(format!("{} URL `{url}` has no host", kind.label()))
    })?;
    let port = parsed.port_or_known_default().unwrap_or(443);
    Ok((host.to_string(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn async_guard_blocks_literal_private_hosts() {
        let err = guard_url(
            "http://127.0.0.1:11434/v1",
            NetworkUrlKind::ProviderUpstream,
            true,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, NetworkGuardError::BlockedPrivate(_)));
    }

    #[tokio::test]
    async fn async_guard_blocks_localhost_dns() {
        let err = guard_url("http://localhost/token", NetworkUrlKind::OauthToken, true)
            .await
            .unwrap_err();
        assert!(matches!(err, NetworkGuardError::BlockedPrivate(_)));
    }

    #[test]
    fn blocking_guard_blocks_proxy_localhost() {
        let err =
            guard_url_blocking("socks5://localhost:1080", NetworkUrlKind::Proxy, true).unwrap_err();
        assert!(matches!(err, NetworkGuardError::BlockedPrivate(_)));
    }

    #[test]
    fn disabled_guard_allows_local_urls() {
        guard_url_blocking("http://127.0.0.1:1", NetworkUrlKind::Proxy, false).unwrap();
    }
}
