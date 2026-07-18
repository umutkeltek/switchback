use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures::StreamExt;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use sb_bodylog::{BodyEventInput, BodyLogger, CaptureStage};
use sb_core::{new_id, ForwardProxyConfig};
use sb_trace::TraceLog;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::{self, ServerConfig};
use tokio_rustls::TlsAcceptor;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BUFFERED_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const HOP_BY_HOP: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

#[derive(Clone)]
struct ForwardProxyState {
    id: String,
    intercept_hosts: Arc<HashSet<String>>,
    tunnel_unknown_hosts: bool,
    upstream_overrides: Arc<BTreeMap<String, String>>,
    upstream_routes: Arc<Vec<ForwardProxyUpstreamRoute>>,
    tls_acceptor: TlsAcceptor,
    capture_logger: Option<BodyLogger>,
    client: reqwest::Client,
}

#[derive(Clone)]
struct ForwardProxyUpstreamRoute {
    host: String,
    path_prefixes: Vec<String>,
    upstream: String,
}

impl ForwardProxyState {
    fn select_upstream(&self, host: &str, target: &str) -> String {
        self.upstream_routes
            .iter()
            .find(|route| route.matches(host, target))
            .map(|route| route.upstream.clone())
            .or_else(|| self.upstream_overrides.get(host).cloned())
            .unwrap_or_else(|| format!("https://{host}"))
    }
}

impl ForwardProxyUpstreamRoute {
    fn matches(&self, host: &str, target: &str) -> bool {
        self.host == host
            && (self.path_prefixes.is_empty()
                || self
                    .path_prefixes
                    .iter()
                    .any(|prefix| target.starts_with(prefix)))
    }
}

pub(crate) async fn spawn_forward_proxy_listener(
    cfg: ForwardProxyConfig,
    listener: TcpListener,
    _traces: Arc<TraceLog>,
    capture_sink: Option<PathBuf>,
) -> Result<JoinHandle<Result<()>>> {
    let state = Arc::new(build_state(cfg, capture_sink)?);
    Ok(tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await?;
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_connection(state, stream).await {
                    tracing::warn!(%peer, error = %err, "forward proxy connection failed");
                }
            });
        }
    }))
}

fn build_state(
    cfg: ForwardProxyConfig,
    capture_sink: Option<PathBuf>,
) -> Result<ForwardProxyState> {
    let intercept_hosts: HashSet<String> = cfg
        .intercept_hosts
        .iter()
        .map(|host| normalize_host(host))
        .collect();
    let ca = Arc::new(CaAuthority::load_or_create(
        cfg.ca_cert_path.as_deref(),
        cfg.ca_key_path.as_deref(),
    )?);
    let resolver = Arc::new(MitmCertResolver::new(intercept_hosts.clone(), ca.clone()));
    let tls_config = ServerConfig::builder_with_provider(ca.provider.clone())
        .with_safe_default_protocol_versions()
        .context("build forward proxy TLS protocol versions")?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    let capture_logger = if cfg.capture_bodies {
        capture_sink.and_then(|sink| match BodyLogger::from_legacy_sink(sink) {
            Ok(logger) => Some(logger),
            Err(err) => {
                tracing::warn!(
                    proxy = %cfg.id,
                    error = %err,
                    "forward proxy body logger disabled"
                );
                None
            }
        })
    } else {
        None
    };
    let client = reqwest::Client::builder()
        .build()
        .context("forward proxy reqwest client builds")?;

    Ok(ForwardProxyState {
        id: cfg.id,
        intercept_hosts: Arc::new(intercept_hosts),
        tunnel_unknown_hosts: cfg.tunnel_unknown_hosts,
        upstream_overrides: Arc::new(
            cfg.upstream_overrides
                .into_iter()
                .map(|(host, upstream)| (normalize_host(&host), upstream))
                .collect(),
        ),
        upstream_routes: Arc::new(
            cfg.upstream_routes
                .into_iter()
                .map(|route| ForwardProxyUpstreamRoute {
                    host: normalize_host(&route.host),
                    path_prefixes: route.path_prefixes,
                    upstream: route.upstream,
                })
                .collect(),
        ),
        tls_acceptor: TlsAcceptor::from(Arc::new(tls_config)),
        capture_logger,
        client,
    })
}

async fn handle_connection(state: Arc<ForwardProxyState>, mut client: TcpStream) -> Result<()> {
    let Some(connect) = read_http_head(&mut client).await? else {
        return Ok(());
    };
    if !connect.method.eq_ignore_ascii_case("CONNECT") {
        client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n")
            .await?;
        return Ok(());
    }
    let (host, port) = parse_authority(&connect.target)
        .with_context(|| format!("invalid CONNECT target `{}`", connect.target))?;
    let host = normalize_host(&host);
    let authority = format!("{host}:{port}");
    if !state.intercept_hosts.contains(&host) {
        if !state.tunnel_unknown_hosts {
            client
                .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\n\r\n")
                .await?;
            return Ok(());
        }
        let mut upstream = TcpStream::connect(&authority)
            .await
            .with_context(|| format!("connect upstream tunnel `{authority}`"))?;
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
        return Ok(());
    }

    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let tls = state
        .tls_acceptor
        .accept(client)
        .await
        .context("accept intercepted TLS")?;
    handle_intercepted_tls(state, tls, host).await
}

async fn handle_intercepted_tls<S>(
    state: Arc<ForwardProxyState>,
    mut stream: S,
    host: String,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(request) = read_http_request(&mut stream).await? {
        if request.headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
        }) {
            write_simple_response(&mut stream, 501, "chunked request bodies are not supported")
                .await?;
            continue;
        }
        let request_id = new_id("fpx");
        let selected_upstream = state.select_upstream(&host, &request.target);
        if let Some(logger) = state.capture_logger.as_ref() {
            write_capture(
                logger,
                BodyEventInput {
                    request_id: request_id.clone(),
                    capture_stage: CaptureStage::ClientInbound,
                    protocol: "forward-proxy".to_string(),
                    upstream: Some(host.clone()),
                    model: request.model.clone(),
                    status: None,
                    content_type: request.content_type.clone(),
                    metadata: serde_json::json!({
                        "proxy_id": state.id,
                        "method": request.method,
                        "path": request.target,
                        "selected_upstream": selected_upstream.clone(),
                    }),
                    body: request.body.clone(),
                },
            );
        }
        let response =
            forward_intercepted_request(&state, &request, &selected_upstream, &mut stream).await;
        match response {
            Ok(response) => {
                if let Some(logger) = state.capture_logger.as_ref() {
                    write_capture(
                        logger,
                        BodyEventInput {
                            request_id,
                            capture_stage: CaptureStage::UpstreamResponse,
                            protocol: "forward-proxy".to_string(),
                            upstream: Some(host.clone()),
                            model: request.model,
                            status: Some(response.status),
                            content_type: response.content_type,
                            metadata: serde_json::json!({
                                "proxy_id": state.id,
                                "method": request.method,
                                "path": request.target,
                                "selected_upstream": selected_upstream.clone(),
                            }),
                            body: response.body,
                        },
                    );
                }
            }
            Err(err) => {
                tracing::warn!(proxy = %state.id, host = %host, error = %err, "forward proxy upstream request failed");
                write_simple_response(&mut stream, 502, "forward proxy upstream request failed")
                    .await?;
            }
        }
    }
    Ok(())
}

async fn forward_intercepted_request(
    state: &ForwardProxyState,
    request: &ParsedRequest,
    upstream: &str,
    stream: &mut (impl AsyncWrite + Unpin),
) -> Result<InterceptedResponse> {
    let url = format!("{}{}", upstream.trim_end_matches('/'), request.target);
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .with_context(|| format!("unsupported method `{}`", request.method))?;
    let mut rb = state
        .client
        .request(method, &url)
        .body(request.body.clone());
    for (name, value) in &request.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        rb = rb.header(name, value);
    }
    let resp = rb
        .send()
        .await
        .context("send intercepted upstream request")?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let content_type = header_value(&headers, "content-type");
    stream
        .write_all(
            format!(
                "HTTP/1.1 {} {}\r\n",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            )
            .as_bytes(),
        )
        .await?;
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) || name.as_str().eq_ignore_ascii_case("content-length") {
            continue;
        }
        stream
            .write_all(
                format!(
                    "{}: {}\r\n",
                    name.as_str(),
                    value.to_str().unwrap_or_default()
                )
                .as_bytes(),
            )
            .await?;
    }
    stream
        .write_all(b"transfer-encoding: chunked\r\n\r\n")
        .await?;
    stream.flush().await?;

    let mut body = Vec::new();
    let mut chunks = resp.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.context("read intercepted upstream response chunk")?;
        if chunk.is_empty() {
            continue;
        }
        body.extend_from_slice(&chunk);
        stream
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await?;
        stream.write_all(&chunk).await?;
        stream.write_all(b"\r\n").await?;
        stream.flush().await?;
    }
    stream.write_all(b"0\r\n\r\n").await?;
    stream.flush().await?;
    Ok(InterceptedResponse {
        status: status.as_u16(),
        content_type,
        body,
    })
}

async fn write_simple_response<S>(stream: &mut S, status: u16, body: &str) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Error\r\ncontent-length: {}\r\ncontent-type: text/plain\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.flush().await?;
    Ok(())
}

fn write_capture(logger: &BodyLogger, input: BodyEventInput) {
    if let Err(err) = logger.record(input) {
        tracing::warn!(error = %err, "forward proxy body capture failed");
    }
}

#[derive(Debug)]
struct HttpHead {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

#[derive(Debug)]
struct ParsedRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    content_type: Option<String>,
    model: Option<String>,
}

#[derive(Debug)]
struct InterceptedResponse {
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

async fn read_http_head<S>(stream: &mut S) -> Result<Option<HttpHead>>
where
    S: AsyncRead + Unpin,
{
    let Some(head) = read_header_block(stream).await? else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&head).context("http head is not utf-8")?;
    let mut lines = text.split("\r\n");
    let first = lines.next().context("empty http head")?;
    let mut parts = first.split_whitespace();
    let method = parts.next().context("missing method")?.to_string();
    let target = parts.next().context("missing target")?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Ok(Some(HttpHead {
        method,
        target,
        headers,
    }))
}

async fn read_http_request<S>(stream: &mut S) -> Result<Option<ParsedRequest>>
where
    S: AsyncRead + Unpin,
{
    let Some(head) = read_http_head(stream).await? else {
        return Ok(None);
    };
    let content_length = head
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_BUFFERED_REQUEST_BYTES {
        anyhow::bail!("request body too large for forward proxy buffer: {content_length}");
    }
    let mut body = vec![0_u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).await?;
    }
    let content_type = head
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone());
    let model = extract_model(&body, content_type.as_deref());
    Ok(Some(ParsedRequest {
        method: head.method,
        target: head.target,
        headers: head.headers,
        body,
        content_type,
        model,
    }))
}

async fn read_header_block<S>(stream: &mut S) -> Result<Option<Vec<u8>>>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let n = stream.read(&mut byte).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            anyhow::bail!("connection closed while reading http head");
        }
        buf.push(byte[0]);
        if buf.len() > MAX_HEADER_BYTES {
            anyhow::bail!("http head exceeded {MAX_HEADER_BYTES} bytes");
        }
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(Some(buf));
        }
    }
}

fn parse_authority(authority: &str) -> Option<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        return Some((host.to_string(), port.parse().ok()?));
    }
    let (host, port) = authority.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

fn header_value(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn extract_model(body: &[u8], content_type: Option<&str>) -> Option<String> {
    if !content_type
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("json")
    {
        return None;
    }
    let json = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    json.get("model")
        .and_then(|model| model.as_str())
        .map(str::to_string)
}

struct CaAuthority {
    issuer: Issuer<'static, KeyPair>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl CaAuthority {
    fn load_or_create(cert_path: Option<&Path>, key_path: Option<&Path>) -> Result<Self> {
        let provider = crypto_provider();
        if let (Some(cert_path), Some(key_path)) = (cert_path, key_path) {
            if cert_path.exists() && key_path.exists() {
                let cert_pem = std::fs::read_to_string(cert_path)
                    .with_context(|| format!("read CA cert `{}`", cert_path.display()))?;
                let key_pem = std::fs::read_to_string(key_path)
                    .with_context(|| format!("read CA key `{}`", key_path.display()))?;
                let key = KeyPair::from_pem(&key_pem).context("parse forward proxy CA key")?;
                let issuer = Issuer::from_ca_cert_pem(&cert_pem, key)
                    .context("parse forward proxy CA cert")?;
                return Ok(Self { issuer, provider });
            }
            let (cert_pem, key_pem, issuer) = generate_ca()?;
            write_private_file(cert_path, cert_pem.as_bytes())?;
            write_private_file(key_path, key_pem.as_bytes())?;
            return Ok(Self { issuer, provider });
        }
        let (_, _, issuer) = generate_ca()?;
        Ok(Self { issuer, provider })
    }

    fn leaf_cert(&self, host: &str) -> Result<CertifiedKey> {
        let mut params = CertificateParams::new(vec![host.to_string()])?;
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, format!("Switchback MITM {host}"));
        params.is_ca = IsCa::NoCa;
        params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);
        let key = KeyPair::generate().context("generate MITM leaf key")?;
        let cert = params
            .signed_by(&key, &self.issuer)
            .context("sign MITM leaf certificate")?;
        let cert_der = CertificateDer::from(cert);
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        CertifiedKey::from_der(vec![cert_der], key_der, &self.provider)
            .context("build rustls certified key")
    }
}

fn generate_ca() -> Result<(String, String, Issuer<'static, KeyPair>)> {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "Switchback Mode D Local CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::CrlSign,
    ];
    let key = KeyPair::generate().context("generate forward proxy CA key")?;
    let cert = params
        .self_signed(&key)
        .context("self-sign forward proxy CA")?;
    let cert_pem = cert.pem();
    let key_pem = key.serialize_pem();
    let issuer = Issuer::new(params, key);
    Ok((cert_pem, key_pem, issuer))
}

fn crypto_provider() -> Arc<rustls::crypto::CryptoProvider> {
    if let Some(provider) = rustls::crypto::CryptoProvider::get_default() {
        return provider.clone();
    }
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.extension().and_then(|e| e.to_str()) == Some("key") {
            0o600
        } else {
            0o644
        };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

struct MitmCertResolver {
    allowed: HashSet<String>,
    ca: Arc<CaAuthority>,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl MitmCertResolver {
    fn new(allowed: HashSet<String>, ca: Arc<CaAuthority>) -> Self {
        Self {
            allowed,
            ca,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

impl fmt::Debug for MitmCertResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MitmCertResolver")
            .field("allowed", &self.allowed)
            .finish_non_exhaustive()
    }
}

impl ResolvesServerCert for MitmCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let host = normalize_host(client_hello.server_name()?);
        if !self.allowed.contains(&host) {
            return None;
        }
        let mut cache = self.cache.lock().ok()?;
        if let Some(cert) = cache.get(&host) {
            return Some(cert.clone());
        }
        let cert = Arc::new(self.ca.leaf_cert(&host).ok()?);
        cache.insert(host, cert.clone());
        Some(cert)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::body::Bytes;
    use axum::response::Response;
    use axum::routing::post;
    use axum::{Json, Router};
    use futures::StreamExt;
    use sb_core::{ForwardProxyConfig, ForwardProxyUpstreamRoute};
    use sb_trace::TraceLog;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::{sleep, timeout, Duration};

    fn temp_capture_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "switchback-forward-proxy-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn forward_proxy_tunnels_non_allowlisted_connect_hosts() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = upstream.accept().await.unwrap();
            let mut buf = [0_u8; 5];
            stream.read_exact(&mut buf).await.unwrap();
            assert_eq!(&buf, b"ping!");
            stream.write_all(b"pong!").await.unwrap();
        });

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let cfg = ForwardProxyConfig {
            id: "mode-d-test".to_string(),
            bind: "127.0.0.1:0".to_string(),
            intercept_hosts: vec!["api.anthropic.test".to_string()],
            tunnel_unknown_hosts: true,
            capture_bodies: false,
            ca_cert_path: None,
            ca_key_path: None,
            upstream_overrides: BTreeMap::new(),
            upstream_routes: Vec::new(),
        };
        let handle = super::spawn_forward_proxy_listener(
            cfg,
            proxy_listener,
            std::sync::Arc::new(TraceLog::in_memory(16)),
            None,
        )
        .await
        .unwrap();

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client
            .write_all(
                format!("CONNECT {upstream_addr} HTTP/1.1\r\nHost: {upstream_addr}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();

        let mut response = Vec::new();
        read_until(&mut client, &mut response, b"\r\n\r\n")
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200"),
            "unexpected CONNECT response: {}",
            String::from_utf8_lossy(&response)
        );

        client.write_all(b"ping!").await.unwrap();
        let mut echoed = [0_u8; 5];
        client.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"pong!");

        handle.abort();
    }

    #[tokio::test]
    async fn forward_proxy_intercepts_allowlisted_https_and_captures_bodies() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|body: Bytes| async move {
                assert!(
                    String::from_utf8_lossy(&body).contains("mode-d-request-secret"),
                    "upstream receives original request body"
                );
                Json(serde_json::json!({
                    "id": "msg_test",
                    "content": "mode-d-response-secret"
                }))
            }),
        );
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

        let root = temp_capture_root("https");
        let state_dir = root.join("state");
        // No env override: the proxy logger derives state_dir/body/archive, keeping
        // this test isolated from concurrent tests' process-global env mutations.
        let archive_root = state_dir.join("body").join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let ca_cert_path = root.join("mode-d-ca.pem");
        let ca_key_path = root.join("mode-d-ca.key");

        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let mut upstream_overrides = BTreeMap::new();
        upstream_overrides.insert(
            "api.anthropic.test".to_string(),
            format!("http://{upstream_addr}"),
        );
        let cfg = ForwardProxyConfig {
            id: "claude-remote-test".to_string(),
            bind: "127.0.0.1:0".to_string(),
            intercept_hosts: vec!["api.anthropic.test".to_string()],
            tunnel_unknown_hosts: true,
            capture_bodies: true,
            ca_cert_path: Some(ca_cert_path.clone()),
            ca_key_path: Some(ca_key_path),
            upstream_overrides,
            upstream_routes: Vec::new(),
        };
        let handle = super::spawn_forward_proxy_listener(
            cfg,
            proxy_listener,
            std::sync::Arc::new(TraceLog::in_memory(16)),
            Some(legacy_jsonl.clone()),
        )
        .await
        .unwrap();

        let ca = fs::read(&ca_cert_path).unwrap();
        let ca = reqwest::Certificate::from_pem(&ca).unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{proxy_addr}")).unwrap())
            .add_root_certificate(ca)
            .no_brotli()
            .no_gzip()
            .no_deflate()
            .build()
            .unwrap();
        let resp = client
            .post("https://api.anthropic.test/v1/messages")
            .header("content-type", "application/json")
            .body(r#"{"model":"claude","input":"mode-d-request-secret"}"#)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("mode-d-response-secret"));

        // D3: proxy captures day-route into the archive partition, not the legacy sink.
        let legacy: String = fs::read_dir(&archive_root)
            .unwrap()
            .flatten()
            .flat_map(|y| fs::read_dir(y.path()).into_iter().flatten().flatten())
            .flat_map(|m| fs::read_dir(m.path()).into_iter().flatten().flatten())
            .map(|d| d.path().join("tap-bodies.jsonl"))
            .filter(|p| p.exists())
            .filter_map(|p| fs::read_to_string(p).ok())
            .collect();
        assert!(legacy.contains(r#""capture_stage":"client_inbound""#));
        assert!(legacy.contains(r#""capture_stage":"upstream_response""#));
        assert!(legacy.contains(r#""protocol":"forward-proxy""#));
        assert!(legacy.contains(r#""selected_upstream":"#));

        handle.abort();
    }

    #[tokio::test]
    async fn forward_proxy_routes_matching_paths_to_specific_upstream() {
        let headroom = Router::new().route(
            "/v1/messages",
            post(|body: Bytes| async move {
                assert!(
                    String::from_utf8_lossy(&body).contains("route-to-headroom"),
                    "path route upstream receives message request body"
                );
                Json(serde_json::json!({ "from": "headroom" }))
            }),
        );
        let headroom_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let headroom_addr = headroom_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(headroom_listener, headroom).await.unwrap() });

        let direct = Router::new().route(
            "/api/claude_cli/bootstrap",
            post(|| async move { Json(serde_json::json!({ "from": "direct" })) }),
        );
        let direct_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_addr = direct_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(direct_listener, direct).await.unwrap() });

        let root = temp_capture_root("routes");
        let ca_cert_path = root.join("mode-d-ca.pem");
        let ca_key_path = root.join("mode-d-ca.key");
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let mut upstream_overrides = BTreeMap::new();
        upstream_overrides.insert(
            "api.anthropic.test".to_string(),
            format!("http://{direct_addr}"),
        );
        let cfg = ForwardProxyConfig {
            id: "claude-remote-route-test".to_string(),
            bind: "127.0.0.1:0".to_string(),
            intercept_hosts: vec!["api.anthropic.test".to_string()],
            tunnel_unknown_hosts: true,
            capture_bodies: false,
            ca_cert_path: Some(ca_cert_path.clone()),
            ca_key_path: Some(ca_key_path),
            upstream_overrides,
            upstream_routes: vec![ForwardProxyUpstreamRoute {
                host: "api.anthropic.test".to_string(),
                path_prefixes: vec![
                    "/v1/messages".to_string(),
                    "/v1/messages/count_tokens".to_string(),
                ],
                upstream: format!("http://{headroom_addr}"),
            }],
        };
        let handle = super::spawn_forward_proxy_listener(
            cfg,
            proxy_listener,
            std::sync::Arc::new(TraceLog::in_memory(16)),
            None,
        )
        .await
        .unwrap();

        let ca = fs::read(&ca_cert_path).unwrap();
        let ca = reqwest::Certificate::from_pem(&ca).unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{proxy_addr}")).unwrap())
            .add_root_certificate(ca)
            .no_brotli()
            .no_gzip()
            .no_deflate()
            .build()
            .unwrap();
        let message_resp = client
            .post("https://api.anthropic.test/v1/messages?beta=true")
            .body("route-to-headroom")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(message_resp.contains(r#""from":"headroom""#));

        let bootstrap_resp = client
            .post("https://api.anthropic.test/api/claude_cli/bootstrap")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(bootstrap_resp.contains(r#""from":"direct""#));

        handle.abort();
    }

    #[tokio::test]
    async fn forward_proxy_streams_intercepted_responses_before_completion() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|| async move {
                let stream = futures::stream::unfold(0, |state| async move {
                    match state {
                        0 => Some((
                            Ok::<Bytes, Infallible>(Bytes::from_static(b"data: first\n\n")),
                            1,
                        )),
                        1 => {
                            sleep(Duration::from_millis(350)).await;
                            Some((
                                Ok::<Bytes, Infallible>(Bytes::from_static(b"data: second\n\n")),
                                2,
                            ))
                        }
                        _ => None,
                    }
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap()
            }),
        );
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

        let root = temp_capture_root("stream");
        let ca_cert_path = root.join("mode-d-ca.pem");
        let ca_key_path = root.join("mode-d-ca.key");
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let mut upstream_overrides = BTreeMap::new();
        upstream_overrides.insert(
            "api.anthropic.test".to_string(),
            format!("http://{upstream_addr}"),
        );
        let cfg = ForwardProxyConfig {
            id: "claude-remote-stream-test".to_string(),
            bind: "127.0.0.1:0".to_string(),
            intercept_hosts: vec!["api.anthropic.test".to_string()],
            tunnel_unknown_hosts: true,
            capture_bodies: false,
            ca_cert_path: Some(ca_cert_path.clone()),
            ca_key_path: Some(ca_key_path),
            upstream_overrides,
            upstream_routes: Vec::new(),
        };
        let handle = super::spawn_forward_proxy_listener(
            cfg,
            proxy_listener,
            std::sync::Arc::new(TraceLog::in_memory(16)),
            None,
        )
        .await
        .unwrap();

        let ca = fs::read(&ca_cert_path).unwrap();
        let ca = reqwest::Certificate::from_pem(&ca).unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::https(format!("http://{proxy_addr}")).unwrap())
            .add_root_certificate(ca)
            .no_brotli()
            .no_gzip()
            .no_deflate()
            .build()
            .unwrap();
        let resp = client
            .post("https://api.anthropic.test/v1/messages")
            .body("{}")
            .send()
            .await
            .unwrap();
        let mut stream = resp.bytes_stream();
        let first = timeout(Duration::from_millis(150), stream.next())
            .await
            .expect("first chunk should arrive before upstream completes")
            .unwrap()
            .unwrap();
        assert_eq!(&first[..], b"data: first\n\n");

        handle.abort();
    }

    async fn read_until(
        stream: &mut TcpStream,
        buf: &mut Vec<u8>,
        needle: &[u8],
    ) -> std::io::Result<()> {
        let mut byte = [0_u8; 1];
        loop {
            stream.read_exact(&mut byte).await?;
            buf.push(byte[0]);
            if buf.ends_with(needle) {
                return Ok(());
            }
        }
    }
}
