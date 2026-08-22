use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

use crate::tap::{
    is_execution_observation_header, CaptureAccumulator, CapturePayload, CaptureProfileAuthority,
    CaptureWorker, TapCaptureContext, TAP_CAPTURE_BODY_MAX_BYTES,
};

/// How long an upstream connection may sit idle in this proxy's pool.
///
/// A keep-alive peer decides its own idle budget and closes without telling us;
/// every connection held past that point is a request that will fail on its
/// first write. reqwest's default is 90s of idle retention, which is longer
/// than the observed survival of these upstream connections: on 2026-08-22 the
/// failures clustered entirely on the idle-then-burst cadence and never once on
/// a steady 40-request probe. Ten seconds is short enough that a reused
/// connection is very likely still open, and long enough that a burst still
/// reuses one. It narrows the window; the re-send below closes it.
const UPSTREAM_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounded per-host pool: enough for a Remote Control burst, small enough that
/// a quiet period cannot leave a large fleet of stale sockets behind.
const UPSTREAM_POOL_MAX_IDLE_PER_HOST: usize = 8;
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
    capture_worker: Option<CaptureWorker>,
    capture_authority: CaptureProfileAuthority,
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
    spawn_forward_proxy_listener_with_authority(
        cfg,
        listener,
        capture_sink,
        CaptureProfileAuthority::load_live_default(),
    )
    .await
}

async fn spawn_forward_proxy_listener_with_authority(
    cfg: ForwardProxyConfig,
    listener: TcpListener,
    capture_sink: Option<PathBuf>,
    capture_authority: CaptureProfileAuthority,
) -> Result<JoinHandle<Result<()>>> {
    let state = Arc::new(build_state(cfg, capture_sink, capture_authority)?);
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
    capture_authority: CaptureProfileAuthority,
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
    let capture_worker = if cfg.capture_bodies {
        capture_sink.and_then(|sink| match BodyLogger::from_legacy_sink(sink) {
            Ok(logger) => match CaptureWorker::new(logger) {
                Ok(worker) => Some(worker),
                Err(err) => {
                    tracing::warn!(
                        proxy = %cfg.id,
                        error = %err,
                        "forward proxy body capture worker disabled"
                    );
                    None
                }
            },
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
        .pool_idle_timeout(UPSTREAM_POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(UPSTREAM_POOL_MAX_IDLE_PER_HOST)
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
        capture_worker,
        capture_authority,
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
        let capture_context = TapCaptureContext::from_header_pairs_with_authority(
            &request.headers,
            &state.capture_authority,
        );
        // Caller policy claims remain metadata only. The profile and revision
        // can reduce capture only after resolving through Switchback authority.
        if let Some(worker) = state
            .capture_worker
            .as_ref()
            .filter(|_| capture_context.enabled())
        {
            let metadata = capture_context.merge_metadata(serde_json::json!({
                "proxy_id": state.id,
                "method": request.method,
                "path": request.target,
                "selected_upstream": selected_upstream.clone(),
            }));
            worker.submit_authorized_payload(
                BodyEventInput {
                    request_id: request_id.clone(),
                    capture_stage: CaptureStage::ClientInbound,
                    protocol: "forward-proxy".to_string(),
                    upstream: Some(host.clone()),
                    model: request.model.clone(),
                    status: None,
                    content_type: request.content_type.clone(),
                    metadata,
                    body: Vec::new(),
                },
                CapturePayload::Full(request.body.clone()),
                capture_context.effective_capture_policy(),
            );
        }
        let response =
            forward_intercepted_request(&state, &request, &selected_upstream, &mut stream).await;
        match response {
            Ok(response) => {
                let InterceptedResponse {
                    status,
                    content_type,
                    capture,
                } = response;
                if let Some(worker) = state
                    .capture_worker
                    .as_ref()
                    .filter(|_| capture_context.enabled())
                {
                    let metadata = capture_context.merge_metadata(serde_json::json!({
                        "proxy_id": state.id,
                        "method": request.method,
                        "path": request.target,
                        "selected_upstream": selected_upstream.clone(),
                    }));
                    let input = BodyEventInput {
                        request_id,
                        capture_stage: CaptureStage::UpstreamResponse,
                        protocol: "forward-proxy".to_string(),
                        upstream: Some(host.clone()),
                        model: request.model,
                        status: Some(status),
                        content_type,
                        metadata,
                        body: Vec::new(),
                    };
                    if let Some(capture) = capture {
                        worker.submit_authorized_payload(
                            input,
                            capture,
                            capture_context.effective_capture_policy(),
                        );
                    }
                }
            }
            Err(InterceptFailure::ClientGone(err)) => {
                // The caller hung up. Every byte that follows would go into a
                // closed socket, and a 502 recorded here is evidence of a
                // response nobody received.
                //
                // Live 2026-08-22: after this proxy started recording its own
                // 502s, all of them were this — `Broken pipe (os error 32)` and
                // `Connection reset by peer (os error 54)` with an EMPTY anyhow
                // context chain, on `/worker/events`, `/worker/events/delivery`
                // and the long-lived `/worker/events/stream`. An empty chain is
                // the proof: every upstream path in this file attaches context,
                // so a bare io error can only have come from a write to the
                // caller. The upstream was never involved, and the retry warn
                // below it never fired once in 132 such events.
                tracing::debug!(
                    proxy = %state.id,
                    host = %host,
                    path = %request.target,
                    error = %err,
                    "forward proxy client disconnected before the intercepted response completed"
                );
                return Ok(());
            }
            Err(InterceptFailure::Upstream(err)) => {
                tracing::warn!(proxy = %state.id, host = %host, error = %format!("{err:#}"), "forward proxy upstream request failed");
                // The caller gets a 502 this proxy invented, so the capture
                // ledger must say so. Without this row the request stage is
                // recorded with no response stage at all, and a failed call is
                // indistinguishable from one still in flight — which is how 122
                // failures on 2026-08-22 left no capture evidence whatsoever.
                if let Some(worker) = state
                    .capture_worker
                    .as_ref()
                    .filter(|_| capture_context.enabled())
                {
                    let metadata = capture_context.merge_metadata(serde_json::json!({
                        "proxy_id": state.id,
                        "method": request.method,
                        "path": request.target,
                        "selected_upstream": selected_upstream.clone(),
                        "response_origin": "forward_proxy",
                        "upstream_error": format!("{err:#}"),
                    }));
                    worker.submit_authorized_payload(
                        BodyEventInput {
                            request_id,
                            capture_stage: CaptureStage::UpstreamResponse,
                            protocol: "forward-proxy".to_string(),
                            upstream: Some(host.clone()),
                            model: request.model,
                            status: Some(502),
                            content_type: Some("text/plain".to_string()),
                            metadata,
                            body: Vec::new(),
                        },
                        CapturePayload::Full(Vec::new()),
                        capture_context.effective_capture_policy(),
                    );
                }
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
) -> std::result::Result<InterceptedResponse, InterceptFailure> {
    let url = format!("{}{}", upstream.trim_end_matches('/'), request.target);
    let method = reqwest::Method::from_bytes(request.method.as_bytes())
        .with_context(|| format!("unsupported method `{}`", request.method))
        .map_err(InterceptFailure::Upstream)?;
    let mut rb = state
        .client
        .request(method, &url)
        .body(request.body.clone());
    for (name, value) in &request.headers {
        if is_hop_by_hop(name) || is_execution_observation_header(name) {
            continue;
        }
        rb = rb.header(name, value);
    }
    // One re-send, and only for a failure that never reached the upstream.
    //
    // A pooled connection can be closed by the peer while it still looks
    // idle-healthy here; the corpse is only discovered when the next request is
    // written to it. Live 2026-08-22 on `claude-remote-proxy`: 122 of 2678
    // intercepted requests died this way (`Broken pipe`, `Connection reset by
    // peer`), every one of them a Remote Control bridge call, and every one of
    // them surfaced to the caller as a 502 for a request the upstream never
    // saw. A steady 40-request probe never reproduced it — only the idle-then-
    // burst cadence does, which is exactly the shape of an idle-pool race.
    //
    // `try_clone` is always `Some` here because the body is a fully buffered
    // `Vec<u8>`, but a `None` must degrade to the original error rather than
    // invent one.
    let retry = rb.try_clone();
    let resp = match rb.send().await {
        Ok(resp) => resp,
        Err(err) if is_undelivered_upstream_error(&err) => {
            let Some(retry) = retry else {
                return Err(InterceptFailure::Upstream(
                    anyhow::Error::new(err).context("send intercepted upstream request"),
                ));
            };
            tracing::warn!(
                upstream = %upstream,
                path = %request.target,
                error = %err,
                "forward proxy upstream connection died before delivery; re-sending once on a fresh connection"
            );
            retry
                .send()
                .await
                .context("re-send intercepted upstream request after a dead pooled connection")
                .map_err(InterceptFailure::Upstream)?
        }
        Err(err) => {
            return Err(InterceptFailure::Upstream(
                anyhow::Error::new(err).context("send intercepted upstream request"),
            ))
        }
    };
    let status = resp.status();
    let headers = resp.headers().clone();
    let content_type = header_value(&headers, "content-type");
    write_to_client(
        stream,
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
        write_to_client(
            stream,
            format!(
                "{}: {}\r\n",
                name.as_str(),
                value.to_str().unwrap_or_default()
            )
            .as_bytes(),
        )
        .await?;
    }
    write_to_client(stream, b"transfer-encoding: chunked\r\n\r\n").await?;
    flush_client(stream).await?;

    let mut capture = state
        .capture_worker
        .as_ref()
        .map(|_| CaptureAccumulator::new(TAP_CAPTURE_BODY_MAX_BYTES));
    let mut chunks = resp.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk
            .context("read intercepted upstream response chunk")
            .map_err(InterceptFailure::Upstream)?;
        if chunk.is_empty() {
            continue;
        }
        if let Some(capture) = capture.as_mut() {
            capture.observe(&chunk);
        }
        write_to_client(stream, format!("{:x}\r\n", chunk.len()).as_bytes()).await?;
        write_to_client(stream, &chunk).await?;
        write_to_client(stream, b"\r\n").await?;
        flush_client(stream).await?;
    }
    write_to_client(stream, b"0\r\n\r\n").await?;
    flush_client(stream).await?;
    Ok(InterceptedResponse {
        status: status.as_u16(),
        content_type,
        capture: capture.map(CaptureAccumulator::finish),
    })
}

/// Why an intercepted exchange ended before a response was delivered.
///
/// These two are NOT interchangeable, and conflating them is what made a
/// client-side disconnect read as an upstream outage for a full day. Every
/// write to the caller shares one error type with every read from the upstream,
/// so a bare `?` on an io error produced `upstream request failed
/// error=Broken pipe` for a socket the CALLER had closed.
#[derive(Debug)]
enum InterceptFailure {
    /// The upstream never answered, or died while answering.
    Upstream(anyhow::Error),
    /// The caller went away mid-exchange. Writing anything more — including a
    /// 502 — writes into a closed socket, and recording a 502 capture event
    /// claims a response that nobody ever received.
    ClientGone(std::io::Error),
}

impl fmt::Display for InterceptFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upstream(err) => write!(f, "{err:#}"),
            Self::ClientGone(err) => write!(f, "{err}"),
        }
    }
}

/// Every byte written back to the caller goes through here, so a caller that
/// hung up is classified once, at the only place that can tell.
async fn write_to_client<S>(
    stream: &mut S,
    bytes: &[u8],
) -> std::result::Result<(), InterceptFailure>
where
    S: AsyncWrite + Unpin,
{
    stream
        .write_all(bytes)
        .await
        .map_err(InterceptFailure::ClientGone)
}

async fn flush_client<S>(stream: &mut S) -> std::result::Result<(), InterceptFailure>
where
    S: AsyncWrite + Unpin,
{
    stream.flush().await.map_err(InterceptFailure::ClientGone)
}

/// Whether a failed send means the upstream never received the request.
///
/// `is_connect` is a dial that failed; `is_request` is a send that failed
/// before a response existed — both mean no upstream state changed, so one
/// re-send cannot duplicate a side effect. Everything the upstream ANSWERED,
/// including a 5xx, is a delivered request and never comes through here: it is
/// an `Ok(response)` that this proxy passes straight through. Errors raised
/// later, while streaming the response body, are equally out of scope — by then
/// bytes are already on their way to the caller.
fn is_undelivered_upstream_error(err: &reqwest::Error) -> bool {
    err.is_connect() || err.is_request()
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
    capture: Option<CapturePayload>,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
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
    async fn forward_proxy_enforces_revision_resolved_capture_policy() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap, body: Bytes| async move {
                assert!(
                    String::from_utf8_lossy(&body).contains("mode-d-request-secret"),
                    "upstream receives original request body"
                );
                assert!(
                    headers.get("x-switchback-capture-policy").is_none(),
                    "internal capture policy must stop at the intercepted local edge"
                );
                assert!(
                    headers.get("x-switchback-launch-profile").is_none(),
                    "internal profile identity must stop at the intercepted local edge"
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
        let metadata_revision = format!("sha256:{}", "a".repeat(64));
        let off_revision = format!("sha256:{}", "b".repeat(64));
        let capture_authority = crate::tap::CaptureProfileAuthority::from_entries([
            ("claude-qwen", metadata_revision.as_str(), "metadata_only"),
            ("private-profile", off_revision.as_str(), "off"),
        ])
        .unwrap();
        let handle = super::spawn_forward_proxy_listener_with_authority(
            cfg,
            proxy_listener,
            Some(legacy_jsonl.clone()),
            capture_authority,
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
            .header("x-switchback-launch-profile", "claude-zai-full")
            .header("x-switchback-capture-policy", "segmented_full_wire")
            .body(r#"{"model":"claude","input":"mode-d-request-secret"}"#)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let body = resp.text().await.unwrap();
        assert!(body.contains("mode-d-response-secret"));

        let metadata_only = client
            .post("https://api.anthropic.test/v1/messages")
            .header("content-type", "application/json")
            .header("x-switchback-launch-profile", "claude-qwen")
            .header("x-switchback-capture-policy", "metadata_only")
            .header(
                "x-switchback-conformance-revision",
                metadata_revision.as_str(),
            )
            .body(r#"{"model":"claude","input":"mode-d-request-secret"}"#)
            .send()
            .await
            .unwrap();
        assert!(metadata_only.status().is_success());
        let capture_off = client
            .post("https://api.anthropic.test/v1/messages")
            .header("content-type", "application/json")
            .header("x-switchback-launch-profile", "private-profile")
            .header("x-switchback-capture-policy", "off")
            .header("x-switchback-conformance-revision", off_revision.as_str())
            .body(r#"{"model":"claude","input":"mode-d-request-secret"}"#)
            .send()
            .await
            .unwrap();
        assert!(capture_off.status().is_success());

        let logger = sb_bodylog::BodyLogger::open_existing(sb_bodylog::BodyLoggerConfig {
            state_dir,
            archive_root,
            legacy_jsonl: Some(legacy_jsonl.clone()),
            inline_threshold_bytes: 1,
        })
        .unwrap()
        .expect("forward proxy body logger created the index");
        let mut status = logger.status().unwrap();
        for _ in 0..50 {
            if status.events >= 4 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            status = logger.status().unwrap();
        }
        let events = logger.latest_events(10).unwrap();
        assert_eq!(
            events.len(),
            4,
            "revision-resolved profile policy must govern forward-proxy capture"
        );
        assert!(events
            .iter()
            .any(|event| event.capture_stage == "client_inbound"));
        assert!(events
            .iter()
            .any(|event| event.capture_stage == "upstream_response"));
        assert!(events.iter().all(|event| event.protocol == "forward-proxy"));
        let full_wire: Vec<_> = events
            .iter()
            .filter(|event| event.storage == "archive_segment")
            .collect();
        assert_eq!(full_wire.len(), 2);
        assert!(full_wire
            .iter()
            .all(|event| event.metadata["launch_profile"] == "claude-zai-full"));
        assert!(full_wire
            .iter()
            .all(|event| event.metadata.get("selected_upstream").is_some()));
        let metadata_only: Vec<_> = events
            .iter()
            .filter(|event| event.storage == "metadata_only")
            .collect();
        assert_eq!(metadata_only.len(), 2);
        assert!(metadata_only.iter().all(|event| {
            event.metadata["capture_metadata"]["launch_profile"] == "claude-qwen"
                && event.metadata["capture_metadata"]["launch_capture_policy"] == "metadata_only"
        }));
        assert!(
            metadata_only
                .iter()
                .all(|event| event.body_bytes > 0),
            "metadata-only profile capture must preserve observed body identity without storing payload bytes"
        );
        assert!(!events.iter().any(|event| {
            event.metadata["launch_profile"] == "private-profile"
                || event.metadata["capture_metadata"]["launch_profile"] == "private-profile"
        }));
        assert_eq!(logger.status().unwrap().blobs, 2);
        assert!(
            !legacy_jsonl.exists(),
            "the retired per-event compatibility sink stays frozen"
        );

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

    /// A pooled upstream connection that dies between requests must cost one
    /// re-send, not a 502 to the caller.
    ///
    /// Live shape this reproduces (2026-08-22, `claude-remote-proxy`): 122 of
    /// 2678 intercepted requests failed with `Broken pipe (os error 32)` /
    /// `Connection reset by peer (os error 54)`, every one on a Remote Control
    /// bridge endpoint with a bursty, idle-heavy cadence. A steady probe never
    /// failed — the request that fails is always the first one written to a
    /// connection the peer closed while it sat idle in the pool.
    #[tokio::test]
    async fn forward_proxy_retries_once_when_a_pooled_upstream_connection_is_dead() {
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let served_upstream = served.clone();
        tokio::spawn(async move {
            let mut connection = 0_usize;
            loop {
                let Ok((mut socket, _)) = upstream_listener.accept().await else {
                    return;
                };
                connection += 1;
                let served = served_upstream.clone();
                tokio::spawn(async move {
                    loop {
                        if read_one_upstream_request(&mut socket).await.is_none() {
                            return;
                        }
                        served.fetch_add(1, Ordering::SeqCst);
                        let body = format!("{{\"connection\":{connection}}}");
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
                            body.len()
                        );
                        if socket.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                        if socket.flush().await.is_err() {
                            return;
                        }
                        if connection == 1 {
                            // The idle-pool race, exactly: this connection stays
                            // open so the client pool keeps it, and only dies
                            // once the NEXT request has been written to it.
                            // Closing before that write would let the pool
                            // notice the EOF and quietly dial a fresh
                            // connection — which is the case that never failed
                            // in production and would make this test prove
                            // nothing.
                            let mut probe = [0_u8; 1];
                            let _ = socket.read(&mut probe).await;
                            let _ = socket.shutdown().await;
                            return;
                        }
                    }
                });
            }
        });

        let root = temp_capture_root("dead-pool");
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
            id: "claude-remote-dead-pool-test".to_string(),
            bind: "127.0.0.1:0".to_string(),
            intercept_hosts: vec!["api.anthropic.test".to_string()],
            tunnel_unknown_hosts: false,
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

        let first = client
            .post("https://api.anthropic.test/v1/code/sessions/s1/worker/events")
            .body("first")
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 200);
        let first_body = first.text().await.unwrap();
        assert!(
            first_body.contains(r#""connection":1"#),
            "first request must be served by the first upstream connection: {first_body}"
        );

        let second = client
            .post("https://api.anthropic.test/v1/code/sessions/s1/worker/events")
            .body("second")
            .send()
            .await
            .unwrap();
        assert_eq!(
            second.status(),
            200,
            "a pooled upstream connection that died while idle must be retried, not returned to the caller as 502 (upstream served {} request(s))",
            served.load(Ordering::SeqCst)
        );
        let second_body = second.text().await.unwrap();
        assert!(
            second_body.contains(r#""connection":2"#),
            "the retry must land on a fresh upstream connection: {second_body}"
        );
        assert_eq!(
            served.load(Ordering::SeqCst),
            2,
            "exactly one re-send: the dead connection never delivered a request"
        );

        handle.abort();
    }

    /// The other half of the rule: an upstream that ANSWERED has received the
    /// body, so re-sending it would duplicate a side effect. Only failures that
    /// never reached the upstream are retried.
    #[tokio::test]
    async fn forward_proxy_does_not_retry_an_upstream_that_answered() {
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        let upstream = Router::new().route(
            "/v1/messages",
            post(move |body: Bytes| {
                let counter = counter.clone();
                async move {
                    assert_eq!(&body[..], b"only-once");
                    counter.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::INTERNAL_SERVER_ERROR, "upstream refused it")
                }
            }),
        );
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

        let root = temp_capture_root("delivered-error");
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
            id: "claude-remote-delivered-error-test".to_string(),
            bind: "127.0.0.1:0".to_string(),
            intercept_hosts: vec!["api.anthropic.test".to_string()],
            tunnel_unknown_hosts: false,
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

        let response = client
            .post("https://api.anthropic.test/v1/messages")
            .body("only-once")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 500, "the upstream status passes through");
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "an upstream that answered must never be re-sent"
        );

        handle.abort();
    }

    /// What reqwest actually returns for the two ways an upstream can die
    /// mid-request, pinned so the retry classifier cannot silently stop
    /// covering them.
    ///
    /// Measured 2026-08-22 (reqwest 0.12 / hyper-util legacy): BOTH shapes are
    /// `Kind::Request` — `is_connect=false is_request=true is_body=false
    /// is_timeout=false`. `is_body()` is NOT the kind for a request body that
    /// hits a closed socket, so a classifier written around it would match
    /// nothing.
    #[tokio::test]
    async fn undelivered_upstream_errors_are_classified_as_request_errors() {
        // Shape 1: the peer reads the request HEAD, then closes before reading
        // the body — a POST whose body lands on a socket that is already going.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0_u8; 1];
                    while socket.read_exact(&mut byte).await.is_ok() {
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    drop(socket);
                });
            }
        });
        let client = reqwest::Client::builder().build().unwrap();
        let head_then_close = client
            .post(format!(
                "http://{addr}/v1/code/sessions/cse_test/worker/events"
            ))
            .body(vec![b'x'; 4 * 1024 * 1024])
            .send()
            .await
            .expect_err("the upstream closed before reading the body");
        assert!(
            !head_then_close.is_body() && !head_then_close.is_timeout(),
            "measured kinds must stay pinned: {head_then_close:?}"
        );
        assert!(
            super::is_undelivered_upstream_error(&head_then_close),
            "a body write onto a dying upstream socket is an undelivered request: {head_then_close:?}"
        );

        // Shape 2: keep-alive reuse. The first request completes, the peer then
        // closes while the connection sits in the pool, and the next POST
        // carries a JSON body.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut connection = 0_usize;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                connection += 1;
                tokio::spawn(async move {
                    if read_one_upstream_request(&mut socket).await.is_none() {
                        return;
                    }
                    let body = format!("{{\"connection\":{connection}}}");
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                    if connection == 1 {
                        let mut probe = [0_u8; 1];
                        let _ = socket.read(&mut probe).await;
                        let _ = socket.shutdown().await;
                    }
                });
            }
        });
        let client = reqwest::Client::builder().build().unwrap();
        let first = client
            .post(format!(
                "http://{addr}/v1/code/sessions/cse_test/worker/events"
            ))
            .body(r#"{"events":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), 200);
        let _ = first.text().await.unwrap();
        let reused_corpse = client
            .post(format!(
                "http://{addr}/v1/code/sessions/cse_test/worker/events"
            ))
            .body(r#"{"events":[{"kind":"probe"}]}"#)
            .send()
            .await
            .expect_err("the pooled connection was closed between requests");
        assert!(
            super::is_undelivered_upstream_error(&reused_corpse),
            "a dead pooled connection is an undelivered request: {reused_corpse:?}"
        );
    }

    /// A caller that hangs up mid-response is NOT an upstream failure.
    ///
    /// Live 2026-08-22: every one of this proxy's 502s was this — an io error
    /// with an EMPTY context chain, produced by writing the streamed response
    /// into a socket the Remote Control client had already closed, on
    /// `/worker/events`, `/worker/events/delivery` and `/worker/events/stream`.
    /// It was logged as `upstream request failed`, answered with a 502 written
    /// into the same dead socket, and recorded as a proxy-originated 502
    /// capture event — three claims about an upstream that was never involved.
    #[test]
    fn a_client_that_hangs_up_is_not_reported_as_an_upstream_failure() {
        let logs = SharedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let upstream = Router::new().route(
                    "/v1/code/sessions/cse_test/worker/events/stream",
                    post(|| async move {
                        let stream = futures::stream::unfold(0, |state| async move {
                            match state {
                                0 => Some((
                                    Ok::<Bytes, Infallible>(Bytes::from_static(b"data: first\n\n")),
                                    1,
                                )),
                                1 => {
                                    // Long enough that the caller is gone before
                                    // this chunk is written back to it, and big
                                    // enough that the write cannot quietly sit in
                                    // a kernel buffer — the proxy has to touch
                                    // the closed socket.
                                    sleep(Duration::from_millis(300)).await;
                                    Some((
                                        Ok::<Bytes, Infallible>(Bytes::from(vec![
                                            b'x';
                                            8 * 1024 * 1024
                                        ])),
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
                tokio::spawn(
                    async move { axum::serve(upstream_listener, upstream).await.unwrap() },
                );

                let root = temp_capture_root("client-hangup");
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
                    id: "claude-remote-hangup-test".to_string(),
                    bind: "127.0.0.1:0".to_string(),
                    intercept_hosts: vec!["api.anthropic.test".to_string()],
                    tunnel_unknown_hosts: false,
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
                let response = client
                    .post(
                        "https://api.anthropic.test/v1/code/sessions/cse_test/worker/events/stream",
                    )
                    .body("{}")
                    .send()
                    .await
                    .unwrap();
                let mut stream = response.bytes_stream();
                let first = timeout(Duration::from_millis(500), stream.next())
                    .await
                    .expect("the first chunk arrives before the upstream completes")
                    .unwrap()
                    .unwrap();
                assert_eq!(&first[..], b"data: first\n\n");

                // The caller goes away with the response still open — a Remote
                // Control session ending, a stream torn down, a client killed.
                drop(stream);
                drop(client);
                sleep(Duration::from_millis(700)).await;
                handle.abort();
            });
        });

        // Only this proxy's own lines matter; hyper's connect/pool debug output
        // would drown a failure message.
        let logs: String = logs
            .contents()
            .lines()
            .filter(|line| line.contains("forward proxy"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            logs.contains("client disconnected before the intercepted response completed"),
            "a caller hangup must be named as one: [{logs}]"
        );
        assert!(
            !logs.contains("forward proxy upstream request failed"),
            "a caller hangup must not be reported as an upstream failure: [{logs}]"
        );
    }

    /// Collects tracing output for assertions, without a global subscriber.
    #[derive(Clone, Default)]
    struct SharedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedLog {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for SharedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLog {
        type Writer = SharedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Reads one HTTP/1.1 request (head plus any content-length body) off a raw
    /// upstream socket. Returns `None` once the peer stops sending.
    async fn read_one_upstream_request(socket: &mut TcpStream) -> Option<()> {
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            if socket.read_exact(&mut byte).await.is_err() {
                return None;
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let content_length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if content_length > 0 {
            let mut body = vec![0_u8; content_length];
            if socket.read_exact(&mut body).await.is_err() {
                return None;
            }
        }
        Some(())
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
