//! Transparent tap (Mode B). A passthrough listener that forwards a client's
//! request VERBATIM to a fixed upstream — its own `Authorization`, its own
//! headers, its raw body — streams the response back unchanged, and only
//! observes (records a metadata trace; optionally the full bodies to a separate
//! local file). No canonical-IR round-trip, no credential lease: the vendor sees
//! the native client's request, so there is nothing re-shaped to flag.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::{sync_channel, SyncSender, TrySendError},
    Arc, Mutex,
};
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::ws::{
    rejection::WebSocketUpgradeRejection, CloseFrame as AxumCloseFrame, Message as AxumWsMessage,
    WebSocket, WebSocketUpgrade,
};
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{
    header::{CONTENT_LENGTH, CONTENT_TYPE},
    HeaderMap, Method, StatusCode, Uri,
};
use axum::response::{IntoResponse, Response};
use axum::Router;
use futures::{SinkExt, Stream, StreamExt};
use sb_bodylog::{BodyEventInput, BodyLogger, CaptureStage};
use sb_core::{RouteDecision, TapConfig};
use sb_trace::{Attempt, RequestTrace, TraceLog};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    protocol::{frame::coding::CloseCode, CloseFrame as TungsteniteCloseFrame},
    Message as TungsteniteMessage,
};

/// Hop-by-hop headers the proxy must not copy; the HTTP client manages framing.
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

const TAP_METADATA_BODY_BYTES: usize = 1024 * 1024;
const TAP_SSE_TERMINAL_WINDOW_BYTES: usize = 8192;
const TAP_CAPTURE_QUEUE_CAPACITY: usize = 256;
const TAP_CAPTURE_QUEUE_MAX_BYTES: usize = 64 * 1024 * 1024;

fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

fn is_auth_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key" | "api-key" | "x-goog-api-key"
    )
}

/// One bounded blocking worker per tap keeps body persistence off Tokio's
/// executor without creating an unbounded task/thread per captured event.
/// Capture is observational: a full or failed queue is reported and dropped,
/// never allowed to stall native request forwarding.
#[derive(Clone)]
struct CaptureWorker {
    sender: SyncSender<CaptureJob>,
    budget: CaptureBudget,
}

struct CaptureJob {
    input: BodyEventInput,
    _budget: CaptureBudgetPermit,
}

#[derive(Clone)]
struct CaptureBudget {
    queued_bytes: Arc<AtomicUsize>,
    max_bytes: usize,
}

impl CaptureBudget {
    fn new(max_bytes: usize) -> Self {
        Self {
            queued_bytes: Arc::new(AtomicUsize::new(0)),
            max_bytes,
        }
    }

    fn try_reserve(&self, bytes: usize) -> Option<CaptureBudgetPermit> {
        let mut queued = self.queued_bytes.load(Ordering::Relaxed);
        loop {
            let next = queued.checked_add(bytes)?;
            if next > self.max_bytes {
                return None;
            }
            match self.queued_bytes.compare_exchange_weak(
                queued,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(CaptureBudgetPermit {
                        queued_bytes: Arc::clone(&self.queued_bytes),
                        bytes,
                    });
                }
                Err(actual) => queued = actual,
            }
        }
    }

    fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Acquire)
    }
}

struct CaptureBudgetPermit {
    queued_bytes: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for CaptureBudgetPermit {
    fn drop(&mut self) {
        self.queued_bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl CaptureWorker {
    fn new(logger: BodyLogger) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel::<CaptureJob>(TAP_CAPTURE_QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("switchback-tap-capture".to_string())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let request_id = job.input.request_id.clone();
                    let stage = job.input.capture_stage;
                    if let Err(err) = logger.record(job.input) {
                        tracing::warn!(%request_id, ?stage, error = %err, "tap body capture failed");
                    }
                }
            })?;
        Ok(Self {
            sender,
            budget: CaptureBudget::new(TAP_CAPTURE_QUEUE_MAX_BYTES),
        })
    }

    fn submit(&self, input: BodyEventInput) {
        let body_bytes = input.body.len();
        let Some(budget) = self.budget.try_reserve(body_bytes) else {
            tracing::warn!(
                request_id = %input.request_id,
                stage = ?input.capture_stage,
                body_bytes,
                queued_bytes = self.budget.queued_bytes(),
                max_queued_bytes = TAP_CAPTURE_QUEUE_MAX_BYTES,
                "tap body capture dropped because the bounded byte budget is full"
            );
            return;
        };
        let job = CaptureJob {
            input,
            _budget: budget,
        };
        match self.sender.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                tracing::warn!(
                    request_id = %job.input.request_id,
                    stage = ?job.input.capture_stage,
                    queue_capacity = TAP_CAPTURE_QUEUE_CAPACITY,
                    queued_bytes = self.budget.queued_bytes(),
                    "tap body capture dropped because the bounded queue is full"
                );
            }
            Err(TrySendError::Disconnected(job)) => {
                tracing::warn!(
                    request_id = %job.input.request_id,
                    stage = ?job.input.capture_stage,
                    "tap body capture dropped because the worker stopped"
                );
            }
        }
    }
}

#[derive(Clone)]
struct TapState {
    id: String,
    upstream: String,
    upstream_host: String,
    headers: Vec<(String, String)>,
    capture_worker: Option<CaptureWorker>,
    traces: Arc<TraceLog>,
    client: reqwest::Client,
}

/// Build the axum app for one tap listener. Every request, any method/path, is
/// forwarded to `tap.upstream`. `capture_sink` is the compatibility event log;
/// body bytes go through `sb-bodylog` when `capture_bodies` is enabled.
pub(crate) fn build_tap_app(
    tap: &TapConfig,
    traces: Arc<TraceLog>,
    capture_sink: Option<PathBuf>,
) -> Router {
    // A plain client: no per-egress identity injection (that path refuses auth
    // headers); the tap forwards the client's own credentials untouched. No
    // total timeout so long streamed responses aren't cut off.
    let client = reqwest::Client::builder()
        .build()
        .expect("tap reqwest client builds");
    let upstream = tap.upstream.trim_end_matches('/').to_string();
    let upstream_host = upstream
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(&upstream)
        .to_string();
    let headers = tap
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let state = TapState {
        id: tap.id.clone(),
        upstream,
        upstream_host,
        headers,
        capture_worker: if tap.capture_bodies {
            capture_sink.and_then(|sink| {
                let logger = match BodyLogger::from_legacy_sink(sink) {
                    Ok(logger) => logger,
                    Err(err) => {
                        tracing::warn!(tap = %tap.id, error = %err, "tap body logger disabled");
                        return None;
                    }
                };
                match CaptureWorker::new(logger) {
                    Ok(worker) => Some(worker),
                    Err(err) => {
                        tracing::warn!(tap = %tap.id, error = %err, "tap body capture worker disabled");
                        None
                    }
                }
            })
        } else {
            None
        },
        traces,
        client,
    };
    Router::new()
        .fallback(forward)
        .layer(DefaultBodyLimit::disable())
        .with_state(state)
}

enum TapRequestBody {
    Buffered(Bytes),
    Streaming(Body),
}

impl TapRequestBody {
    fn bytes(&self) -> Option<&Bytes> {
        match self {
            TapRequestBody::Buffered(bytes) => Some(bytes),
            TapRequestBody::Streaming(_) => None,
        }
    }
}

async fn forward(
    State(st): State<TapState>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    if let Ok(ws) = ws {
        return forward_websocket(st, ws, uri, headers).await;
    }

    let started = Instant::now();
    let request_id = sb_core::new_id("tap");
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_else(|| uri.path());
    let url = format!("{}{}", st.upstream, path_and_query);

    let (body, capture_request_body) =
        match prepare_request_body(&st, &request_id, body, &headers).await {
            Ok(prepared) => prepared,
            Err(response) => return response,
        };

    // Best-effort metadata from the request body (never logged beyond this).
    let parsed: Option<serde_json::Value> = body
        .bytes()
        .and_then(|bytes| serde_json::from_slice(bytes).ok());
    let inbound_model = parsed
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let streamed = parsed
        .as_ref()
        .and_then(|v| v.get("stream"))
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    if let (Some(worker), Some(request_body)) =
        (st.capture_worker.clone(), capture_request_body.clone())
    {
        write_request_capture(RequestCapture {
            worker,
            tap_id: st.id.clone(),
            request_id: request_id.clone(),
            upstream: st.upstream.clone(),
            model: inbound_model.clone(),
            content_type: header_content_type(&headers),
            metadata: serde_json::json!({
                "method": method.as_str(),
                "path": path_and_query,
                "timing_source": "switchback_edge",
                "token_source": "provider_usage",
            }),
            body: request_body,
        });
    }

    // Forward the request verbatim: client headers minus hop-by-hop, raw body.
    // Authorization and every vendor header (anthropic-beta, user-agent, …) pass
    // through untouched — this is what makes it indistinguishable from native.
    let mut rb = st.client.request(method, &url);
    rb = match body {
        TapRequestBody::Buffered(body) => rb.body(body),
        TapRequestBody::Streaming(body) => {
            rb.body(reqwest::Body::wrap_stream(body.into_data_stream()))
        }
    };
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        rb = rb.header(name, value);
    }
    for (name, value) in &st.headers {
        if is_hop_by_hop(name) || is_auth_header(name) {
            continue;
        }
        rb = rb.header(name, value);
    }

    let upstream_resp = match rb.send().await {
        Ok(resp) => resp,
        Err(err) => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: &inbound_model,
                    streamed,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                },
            );
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap upstream request failed");
            return (StatusCode::BAD_GATEWAY, "tap upstream request failed").into_response();
        }
    };

    let status = upstream_resp.status();
    let observe_sse_terminal =
        status.is_success() && (streamed || is_sse_response(upstream_resp.headers()));

    // Copy the upstream status + response headers (minus hop-by-hop) and stream
    // the body back unchanged. Capture tees the body to the sink without buffering.
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let capture_finalize = st.capture_worker.as_ref().map(|worker| CaptureFinalize {
        worker: worker.clone(),
        tap_id: st.id.clone(),
        request_id: request_id.clone(),
        upstream: st.upstream.clone(),
        model: inbound_model.clone(),
        status: status.as_u16(),
        content_type: header_content_type(upstream_resp.headers()),
    });
    let trace_finalize = TapTraceFinalize {
        st,
        request_id,
        inbound_model,
        streamed,
        status: status.as_u16(),
        started,
        upstream_ok: status.is_success(),
    };
    let body = Body::from_stream(TapResponseStream {
        inner: upstream_resp.bytes_stream(),
        response_buf: Vec::new(),
        capture_finalize,
        trace_finalize: Some(trace_finalize),
        observe_sse_terminal,
        saw_terminal: false,
        sse_window: Vec::new(),
    });

    builder.body(body).unwrap_or_else(|_| {
        (StatusCode::BAD_GATEWAY, "tap could not build response").into_response()
    })
}

async fn prepare_request_body(
    st: &TapState,
    request_id: &str,
    body: Body,
    headers: &HeaderMap,
) -> Result<(TapRequestBody, Option<Bytes>), Response> {
    if st.capture_worker.is_some() {
        let body = to_bytes(body, usize::MAX).await.map_err(|err| {
            tracing::warn!(tap = %st.id, error = %err, "tap request body capture failed");
            (StatusCode::BAD_GATEWAY, "tap request body capture failed").into_response()
        })?;
        return Ok((TapRequestBody::Buffered(body.clone()), Some(body)));
    }

    if request_body_len(headers).is_some_and(|len| len <= TAP_METADATA_BODY_BYTES) {
        let body = to_bytes(body, TAP_METADATA_BODY_BYTES).await.map_err(|err| {
            tracing::warn!(tap = %st.id, request_id = %request_id, error = %err, "tap request body metadata read failed");
            (
                StatusCode::BAD_GATEWAY,
                "tap request body metadata read failed",
            )
                .into_response()
        })?;
        return Ok((TapRequestBody::Buffered(body), None));
    }

    Ok((TapRequestBody::Streaming(body), None))
}

fn request_body_len(headers: &HeaderMap) -> Option<usize> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

async fn forward_websocket(
    st: TapState,
    ws: WebSocketUpgrade,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let started = Instant::now();
    let request_id = sb_core::new_id("tap");
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or_else(|| uri.path());
    let url = match websocket_upstream_url(&st.upstream, path_and_query) {
        Some(url) => url,
        None => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: "",
                    streamed: true,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                },
            );
            return (
                StatusCode::BAD_GATEWAY,
                "tap upstream is not websocket-compatible",
            )
                .into_response();
        }
    };

    let mut upstream_request = match url.into_client_request() {
        Ok(request) => request,
        Err(err) => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: "",
                    streamed: true,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                },
            );
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap websocket request build failed");
            return (
                StatusCode::BAD_GATEWAY,
                "tap websocket request build failed",
            )
                .into_response();
        }
    };

    for (name, value) in headers.iter() {
        if should_forward_websocket_header(name.as_str()) {
            upstream_request
                .headers_mut()
                .append(name.clone(), value.clone());
        }
    }
    for (name, value) in &st.headers {
        if is_hop_by_hop(name) || is_auth_header(name) {
            continue;
        }
        let Ok(name) =
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())
        else {
            continue;
        };
        let Ok(value) = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(value) else {
            continue;
        };
        upstream_request.headers_mut().append(name, value);
    }

    let requested_protocols: Vec<String> = ws
        .requested_protocols()
        .filter_map(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .collect();

    let (upstream_socket, _) = match connect_async(upstream_request).await {
        Ok(upstream) => upstream,
        Err(err) => {
            record_trace(
                &st,
                TapTraceInput {
                    request_id: &request_id,
                    inbound_model: "",
                    streamed: true,
                    status: 502,
                    started,
                    ok: false,
                    warning: None,
                },
            );
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap websocket upstream connect failed");
            return (
                StatusCode::BAD_GATEWAY,
                "tap websocket upstream connect failed",
            )
                .into_response();
        }
    };

    let capture_finalize = st.capture_worker.as_ref().map(|worker| {
        Arc::new(Mutex::new(WebSocketCapture::new(
            worker.clone(),
            st.id.clone(),
            request_id.clone(),
            st.upstream.clone(),
        )))
    });

    let trace_finalize = TapWebSocketTraceFinalize {
        st,
        request_id,
        started,
    };
    ws.protocols(requested_protocols)
        .on_upgrade(move |client_socket| {
            bridge_websockets(
                client_socket,
                upstream_socket,
                trace_finalize,
                capture_finalize,
            )
        })
}

fn websocket_upstream_url(upstream: &str, path_and_query: &str) -> Option<String> {
    upstream
        .strip_prefix("http://")
        .map(|rest| format!("ws://{rest}{path_and_query}"))
        .or_else(|| {
            upstream
                .strip_prefix("https://")
                .map(|rest| format!("wss://{rest}{path_and_query}"))
        })
        .or_else(|| {
            upstream
                .strip_prefix("ws://")
                .map(|rest| format!("ws://{rest}{path_and_query}"))
        })
        .or_else(|| {
            upstream
                .strip_prefix("wss://")
                .map(|rest| format!("wss://{rest}{path_and_query}"))
        })
}

fn should_forward_websocket_header(name: &str) -> bool {
    if is_hop_by_hop(name) {
        return false;
    }

    let lower = name.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-accept"
            | "sec-websocket-extensions"
    )
}

async fn bridge_websockets(
    client_socket: WebSocket,
    upstream_socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    trace_finalize: TapWebSocketTraceFinalize,
    capture_finalize: Option<Arc<Mutex<WebSocketCapture>>>,
) {
    let (mut client_tx, mut client_rx) = client_socket.split();
    let (mut upstream_tx, mut upstream_rx) = upstream_socket.split();
    let client_capture = capture_finalize.clone();
    let upstream_capture = capture_finalize.clone();

    let client_to_upstream = async {
        loop {
            match client_rx.next().await {
                Some(Ok(message)) => {
                    let warning = axum_close_warning("websocket_client_closed", &message);
                    if let Some(capture) = client_capture.as_ref() {
                        if let Ok(mut capture) = capture.lock() {
                            capture.record_client(&message);
                        }
                    }
                    let Some(message) = axum_to_tungstenite(message) else {
                        continue;
                    };
                    let closing = message.is_close();
                    if upstream_tx.send(message).await.is_err() {
                        return Some("websocket_upstream_send_failed".to_string());
                    }
                    if closing {
                        return warning;
                    }
                }
                Some(Err(err)) => {
                    return Some(format!(
                        "websocket_client_read_error:{}",
                        warning_token(&err.to_string())
                    ));
                }
                None => return Some("websocket_client_ended_without_close_frame".to_string()),
            }
        }
    };

    let upstream_to_client = async {
        loop {
            match upstream_rx.next().await {
                Some(Ok(message)) => {
                    let warning = tungstenite_close_warning("websocket_upstream_closed", &message);
                    if let Some(capture) = upstream_capture.as_ref() {
                        if let Ok(mut capture) = capture.lock() {
                            capture.record_upstream(&message);
                        }
                    }
                    let Some(message) = tungstenite_to_axum(message) else {
                        continue;
                    };
                    let closing = matches!(message, AxumWsMessage::Close(_));
                    if client_tx.send(message).await.is_err() {
                        return Some("websocket_client_send_failed".to_string());
                    }
                    if closing {
                        return warning;
                    }
                }
                Some(Err(err)) => {
                    return Some(format!(
                        "websocket_upstream_read_error:{}",
                        warning_token(&err.to_string())
                    ));
                }
                None => return Some("websocket_upstream_ended_without_close_frame".to_string()),
            }
        }
    };

    let warning = tokio::select! {
        warning = client_to_upstream => warning,
        warning = upstream_to_client => warning,
    };
    if let Some(capture) = capture_finalize {
        if let Ok(capture) = capture.lock() {
            write_websocket_capture(capture.clone(), 101);
        }
    }
    trace_finalize.record(warning);
}

fn axum_to_tungstenite(message: AxumWsMessage) -> Option<TungsteniteMessage> {
    match message {
        AxumWsMessage::Text(text) => Some(TungsteniteMessage::Text(text.to_string().into())),
        AxumWsMessage::Binary(binary) => Some(TungsteniteMessage::Binary(binary)),
        AxumWsMessage::Ping(ping) => Some(TungsteniteMessage::Ping(ping)),
        AxumWsMessage::Pong(pong) => Some(TungsteniteMessage::Pong(pong)),
        AxumWsMessage::Close(close) => Some(TungsteniteMessage::Close(
            close.map(axum_close_to_tungstenite),
        )),
    }
}

fn tungstenite_to_axum(message: TungsteniteMessage) -> Option<AxumWsMessage> {
    match message {
        TungsteniteMessage::Text(text) => Some(AxumWsMessage::Text(text.to_string().into())),
        TungsteniteMessage::Binary(binary) => Some(AxumWsMessage::Binary(binary)),
        TungsteniteMessage::Ping(ping) => Some(AxumWsMessage::Ping(ping)),
        TungsteniteMessage::Pong(pong) => Some(AxumWsMessage::Pong(pong)),
        TungsteniteMessage::Close(close) => {
            Some(AxumWsMessage::Close(close.map(tungstenite_close_to_axum)))
        }
        TungsteniteMessage::Frame(_) => None,
    }
}

fn axum_close_to_tungstenite(close: AxumCloseFrame) -> TungsteniteCloseFrame {
    TungsteniteCloseFrame {
        code: CloseCode::from(close.code),
        reason: close.reason.to_string().into(),
    }
}

fn tungstenite_close_to_axum(close: TungsteniteCloseFrame) -> AxumCloseFrame {
    AxumCloseFrame {
        code: u16::from(close.code),
        reason: close.reason.to_string().into(),
    }
}

fn axum_close_warning(prefix: &str, message: &AxumWsMessage) -> Option<String> {
    match message {
        AxumWsMessage::Close(Some(frame)) => close_warning(prefix, frame.code, &frame.reason),
        AxumWsMessage::Close(None) => None,
        _ => None,
    }
}

fn tungstenite_close_warning(prefix: &str, message: &TungsteniteMessage) -> Option<String> {
    match message {
        TungsteniteMessage::Close(Some(frame)) => {
            close_warning(prefix, u16::from(frame.code), &frame.reason)
        }
        TungsteniteMessage::Close(None) => None,
        _ => None,
    }
}

fn close_warning(prefix: &str, code: u16, reason: &str) -> Option<String> {
    let reason = warning_token(reason);
    if code == 1000 && reason.is_empty() {
        return None;
    }
    if reason.is_empty() {
        Some(format!("{prefix}:{code}"))
    } else {
        Some(format!("{prefix}:{code}:{reason}"))
    }
}

fn warning_token(value: &str) -> String {
    value
        .chars()
        .filter_map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '=') {
                Some(ch)
            } else if ch.is_whitespace() || matches!(ch, ':' | '/' | '\\' | '"' | '\'') {
                Some('_')
            } else {
                None
            }
        })
        .take(96)
        .collect()
}

struct TapTraceInput<'a> {
    request_id: &'a str,
    inbound_model: &'a str,
    streamed: bool,
    status: u16,
    started: Instant,
    ok: bool,
    warning: Option<String>,
}

fn record_trace(st: &TapState, input: TapTraceInput<'_>) {
    let mut decision = RouteDecision::new(input.request_id, "transparent_tap");
    decision.add_reason(format!("tap={}", st.id));
    decision.add_reason(format!("upstream={}", st.upstream_host));
    let latency = input.started.elapsed().as_millis() as u64;
    let mut trace = RequestTrace::start(input.request_id, 0, input.inbound_model, "tap", decision)
        .with_client_metadata(Some(st.id.clone()), Some("passthrough".to_string()));
    if let Some(warning) = input.warning.as_deref() {
        trace.warning(warning);
    }
    let class = if input.ok {
        None
    } else {
        Some(input.warning.as_deref().unwrap_or("upstream_error"))
    };
    trace.attempt(match class {
        None => Attempt::success(
            st.upstream_host.clone(),
            "tap",
            input.inbound_model,
            "client-native",
            "direct",
            latency,
        ),
        Some(c) => Attempt::failed(
            st.upstream_host.clone(),
            "tap",
            input.inbound_model,
            "client-native",
            "direct",
            latency,
            c,
            false,
        ),
    });
    st.traces
        .record(trace.finish(input.status, latency, input.streamed));
}

/// Holds what to persist once the response stream completes.
struct CaptureFinalize {
    worker: CaptureWorker,
    tap_id: String,
    request_id: String,
    upstream: String,
    model: String,
    status: u16,
    content_type: Option<String>,
}

struct RequestCapture {
    worker: CaptureWorker,
    tap_id: String,
    request_id: String,
    upstream: String,
    model: String,
    content_type: Option<String>,
    metadata: serde_json::Value,
    body: Bytes,
}

#[derive(Clone)]
struct CapturedWsFrame {
    kind: &'static str,
    text: Option<String>,
    body: Vec<u8>,
    close_code: Option<u16>,
}

#[derive(Clone)]
struct WebSocketCapture {
    worker: CaptureWorker,
    tap_id: String,
    request_id: String,
    upstream: String,
    model: String,
    client_frame_count: usize,
    upstream_frame_count: usize,
}

impl WebSocketCapture {
    fn new(worker: CaptureWorker, tap_id: String, request_id: String, upstream: String) -> Self {
        Self {
            worker,
            tap_id,
            request_id,
            upstream,
            model: String::new(),
            client_frame_count: 0,
            upstream_frame_count: 0,
        }
    }

    fn record_client(&mut self, message: &AxumWsMessage) {
        self.client_frame_count += 1;
        let Some(frame) = axum_frame_body(message) else {
            return;
        };
        if let Some(text) = frame.text.as_deref() {
            self.capture_model(text);
        }
        write_ws_frame_capture(
            self,
            CaptureStage::ClientInbound,
            "client",
            self.client_frame_count,
            frame,
        );
    }

    fn record_upstream(&mut self, message: &TungsteniteMessage) {
        self.upstream_frame_count += 1;
        let Some(frame) = tungstenite_frame_body(message) else {
            return;
        };
        write_ws_frame_capture(
            self,
            CaptureStage::ClientResponse,
            "upstream",
            self.upstream_frame_count,
            frame,
        );
    }

    fn capture_model(&mut self, text: &str) {
        if !self.model.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        if let Some(model) = value.get("model").and_then(|v| v.as_str()) {
            self.model = model.to_string();
            return;
        }
        if let Some(model) = value
            .get("response")
            .and_then(|v| v.get("model"))
            .and_then(|v| v.as_str())
        {
            self.model = model.to_string();
        }
    }
}

struct TapTraceFinalize {
    st: TapState,
    request_id: String,
    inbound_model: String,
    streamed: bool,
    status: u16,
    started: Instant,
    upstream_ok: bool,
}

impl TapTraceFinalize {
    fn record(self, status_override: Option<u16>, warning: Option<&'static str>) {
        let status = status_override.unwrap_or(self.status);
        record_trace(
            &self.st,
            TapTraceInput {
                request_id: &self.request_id,
                inbound_model: &self.inbound_model,
                streamed: self.streamed,
                status,
                started: self.started,
                ok: self.upstream_ok && warning.is_none(),
                warning: warning.map(ToOwned::to_owned),
            },
        );
    }
}

struct TapWebSocketTraceFinalize {
    st: TapState,
    request_id: String,
    started: Instant,
}

impl TapWebSocketTraceFinalize {
    fn record(self, warning: Option<String>) {
        record_trace(
            &self.st,
            TapTraceInput {
                request_id: &self.request_id,
                inbound_model: "",
                streamed: true,
                status: 101,
                started: self.started,
                ok: true,
                warning,
            },
        );
    }
}

/// Tees the upstream response to the client unchanged while finalizing metadata
/// after the stream ends. Body capture remains explicit; SSE terminal detection
/// keeps only a tiny rolling window and never writes content to traces.
struct TapResponseStream<S> {
    inner: S,
    response_buf: Vec<u8>,
    capture_finalize: Option<CaptureFinalize>,
    trace_finalize: Option<TapTraceFinalize>,
    observe_sse_terminal: bool,
    saw_terminal: bool,
    sse_window: Vec<u8>,
}

impl<S> TapResponseStream<S> {
    fn observe_chunk(&mut self, chunk: &Bytes) {
        if !self.observe_sse_terminal || self.saw_terminal {
            return;
        }
        self.sse_window.extend_from_slice(chunk);
        if self.sse_window.len() > TAP_SSE_TERMINAL_WINDOW_BYTES {
            let excess = self.sse_window.len() - TAP_SSE_TERMINAL_WINDOW_BYTES;
            self.sse_window.drain(..excess);
        }
        if sse_window_has_terminal_event(&self.sse_window) {
            self.saw_terminal = true;
            self.sse_window.clear();
        }
    }

    fn finalize(&mut self, status_override: Option<u16>, warning: Option<&'static str>) {
        if let Some(fin) = self.capture_finalize.take() {
            let body = std::mem::take(&mut self.response_buf);
            write_capture(fin, body);
        }
        if let Some(fin) = self.trace_finalize.take() {
            fin.record(status_override, warning);
        }
    }
}

impl<S> Drop for TapResponseStream<S> {
    fn drop(&mut self) {
        if self.trace_finalize.is_some() {
            self.finalize(Some(499), Some("client_aborted"));
        }
    }
}

impl<S> Stream for TapResponseStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if self.capture_finalize.is_some() {
                    self.response_buf.extend_from_slice(&chunk);
                }
                self.observe_chunk(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => {
                self.finalize(None, Some("upstream_stream_error"));
                Poll::Ready(Some(Err(std::io::Error::other(err))))
            }
            Poll::Ready(None) => {
                let warning = if self.observe_sse_terminal && !self.saw_terminal {
                    Some("upstream_closed_before_terminal")
                } else {
                    None
                };
                self.finalize(None, warning);
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn is_sse_response(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
}

fn sse_window_has_terminal_event(window: &[u8]) -> bool {
    let text = String::from_utf8_lossy(window);
    [
        "response.completed",
        "response.failed",
        "response.cancelled",
        "response.incomplete",
        "[DONE]",
        "message_stop",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

fn write_capture(fin: CaptureFinalize, response: Vec<u8>) {
    fin.worker.submit(BodyEventInput {
        request_id: fin.request_id,
        capture_stage: CaptureStage::ClientResponse,
        protocol: "http".to_string(),
        upstream: Some(fin.upstream),
        model: Some(fin.model),
        status: Some(fin.status),
        content_type: fin.content_type,
        metadata: serde_json::json!({
            "tap": fin.tap_id,
            "timing_source": "switchback_edge",
            "token_source": "provider_usage",
        }),
        body: response,
    });
}

fn write_websocket_capture(fin: WebSocketCapture, status: u16) {
    let summary = serde_json::json!({
        "protocol": "websocket",
        "client_frame_count": fin.client_frame_count,
        "upstream_frame_count": fin.upstream_frame_count,
    });
    fin.worker.submit(BodyEventInput {
        request_id: fin.request_id,
        capture_stage: CaptureStage::ClientSession,
        protocol: "websocket".to_string(),
        upstream: Some(fin.upstream),
        model: Some(fin.model),
        status: Some(status),
        content_type: Some("application/json".to_string()),
        metadata: serde_json::json!({
            "tap": fin.tap_id,
            "timing_source": "switchback_edge",
            "token_source": "provider_usage",
        }),
        body: serde_json::to_vec(&summary).unwrap_or_default(),
    });
}

fn write_request_capture(capture: RequestCapture) {
    capture.worker.submit(BodyEventInput {
        request_id: capture.request_id,
        capture_stage: CaptureStage::ClientInbound,
        protocol: "http".to_string(),
        upstream: Some(capture.upstream),
        model: Some(capture.model),
        status: None,
        content_type: capture.content_type,
        metadata: merge_tap_metadata(capture.tap_id, capture.metadata),
        body: capture.body.to_vec(),
    });
}

fn write_ws_frame_capture(
    capture: &WebSocketCapture,
    stage: CaptureStage,
    direction: &'static str,
    sequence: usize,
    frame: CapturedWsFrame,
) {
    let worker = capture.worker.clone();
    let request_id = capture.request_id.clone();
    let input = BodyEventInput {
        request_id,
        capture_stage: stage,
        protocol: "websocket".to_string(),
        upstream: Some(capture.upstream.clone()),
        model: Some(capture.model.clone()),
        status: Some(101),
        content_type: Some(if frame.text.is_some() {
            "text/plain".to_string()
        } else {
            "application/octet-stream".to_string()
        }),
        metadata: serde_json::json!({
            "tap": capture.tap_id.clone(),
            "direction": direction,
            "sequence": sequence,
            "frame_kind": frame.kind,
            "close_code": frame.close_code,
            "body_bytes": frame.body.len(),
            "timing_source": "switchback_edge",
            "token_source": "provider_usage",
        }),
        body: frame.body,
    };
    worker.submit(input);
}

fn merge_tap_metadata(tap_id: String, metadata: serde_json::Value) -> serde_json::Value {
    let mut object = match metadata {
        serde_json::Value::Object(object) => object,
        other => {
            let mut object = serde_json::Map::new();
            object.insert("metadata".to_string(), other);
            object
        }
    };
    object.insert("tap".to_string(), serde_json::Value::String(tap_id));
    serde_json::Value::Object(object)
}

fn header_content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn axum_frame_body(message: &AxumWsMessage) -> Option<CapturedWsFrame> {
    captured_axum_frame(message)
}

fn tungstenite_frame_body(message: &TungsteniteMessage) -> Option<CapturedWsFrame> {
    captured_tungstenite_frame(message)
}

fn captured_axum_frame(message: &AxumWsMessage) -> Option<CapturedWsFrame> {
    match message {
        AxumWsMessage::Text(text) => Some(captured_text_frame("text", &text.to_string())),
        AxumWsMessage::Binary(binary) => Some(captured_binary_frame("binary", binary.as_ref())),
        AxumWsMessage::Ping(ping) => Some(captured_binary_frame("ping", ping.as_ref())),
        AxumWsMessage::Pong(pong) => Some(captured_binary_frame("pong", pong.as_ref())),
        AxumWsMessage::Close(close) => Some(CapturedWsFrame {
            kind: "close",
            text: close.as_ref().map(|frame| frame.reason.to_string()),
            body: close
                .as_ref()
                .map(|frame| frame.reason.as_bytes().to_vec())
                .unwrap_or_default(),
            close_code: close.as_ref().map(|frame| frame.code),
        }),
    }
}

fn captured_tungstenite_frame(message: &TungsteniteMessage) -> Option<CapturedWsFrame> {
    match message {
        TungsteniteMessage::Text(text) => Some(captured_text_frame("text", text.as_ref())),
        TungsteniteMessage::Binary(binary) => {
            Some(captured_binary_frame("binary", binary.as_ref()))
        }
        TungsteniteMessage::Ping(ping) => Some(captured_binary_frame("ping", ping.as_ref())),
        TungsteniteMessage::Pong(pong) => Some(captured_binary_frame("pong", pong.as_ref())),
        TungsteniteMessage::Close(close) => Some(CapturedWsFrame {
            kind: "close",
            text: close.as_ref().map(|frame| frame.reason.to_string()),
            body: close
                .as_ref()
                .map(|frame| frame.reason.as_bytes().to_vec())
                .unwrap_or_default(),
            close_code: close.as_ref().map(|frame| u16::from(frame.code)),
        }),
        TungsteniteMessage::Frame(_) => None,
    }
}

fn captured_text_frame(kind: &'static str, text: &str) -> CapturedWsFrame {
    CapturedWsFrame {
        kind,
        text: Some(text.to_string()),
        body: text.as_bytes().to_vec(),
        close_code: None,
    }
}

fn captured_binary_frame(kind: &'static str, bytes: &[u8]) -> CapturedWsFrame {
    CapturedWsFrame {
        kind,
        text: None,
        body: bytes.to_vec(),
        close_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::extract::ws::{close_code, CloseFrame, Message as AxumWsMessage, WebSocketUpgrade};
    use axum::routing::{any, post};
    use axum::Json;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    fn temp_capture_root(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "switchback-tap-bodylog-{tag}-{}-{nanos}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    async fn wait_for_traces(traces: &TraceLog, expected: usize) -> Vec<sb_trace::TraceRecord> {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let recent = traces.recent(8);
                if recent.len() >= expected {
                    return recent;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the tap persisted its trace before the bounded test deadline")
    }

    #[tokio::test]
    async fn tap_forwards_request_verbatim_and_records_a_trace() {
        // Fake upstream: echoes back the auth header + body length it received.
        let upstream = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap, body: Bytes| async move {
                Json(serde_json::json!({
                    "seen_auth": headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "seen_beta": headers
                        .get("anthropic-beta")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "body_len": body.len(),
                }))
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        // Tap pointed at the fake upstream.
        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "test-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        // Client sends its own auth; the tap must forward it untouched.
        let resp: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{tap_addr}/v1/messages"))
            .header("authorization", "Bearer CLIENT-OWN-TOKEN")
            .header("anthropic-beta", "oauth-2025-04-20")
            .json(&serde_json::json!({"model": "claude-x", "messages": []}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(
            resp["seen_auth"], "Bearer CLIENT-OWN-TOKEN",
            "auth forwarded verbatim"
        );
        assert_eq!(
            resp["seen_beta"], "oauth-2025-04-20",
            "vendor headers forwarded"
        );
        assert!(resp["body_len"].as_u64().unwrap() > 0, "body forwarded");

        let recent = traces.recent(8);
        assert_eq!(recent.len(), 1, "the tap recorded one trace");
        assert_eq!(recent[0].inbound_model, "claude-x");
        assert_eq!(recent[0].route, "tap");
        assert_eq!(recent[0].final_status, 200);
    }

    #[tokio::test]
    async fn tap_applies_configured_non_auth_headers() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|headers: HeaderMap| async move {
                Json(serde_json::json!({
                    "seen_auth": headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                    "seen_headroom_base": headers
                        .get("x-headroom-base-url")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<none>"),
                }))
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let mut tap_headers = std::collections::BTreeMap::new();
        tap_headers.insert(
            "x-headroom-base-url".to_string(),
            "https://api.z.ai/api/anthropic".to_string(),
        );
        tap_headers.insert("authorization".to_string(), "Bearer wrong".to_string());

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "zai-headroom-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: tap_headers,
        };
        let tap_app = build_tap_app(&cfg, traces, None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let resp: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{tap_addr}/v1/messages"))
            .header("authorization", "Bearer client")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(resp["seen_auth"], "Bearer client");
        assert_eq!(resp["seen_headroom_base"], "https://api.z.ai/api/anthropic");
    }

    #[tokio::test]
    async fn tap_body_capture_writes_protected_index_and_compatibility_events() {
        let upstream = Router::new().route(
            "/v1/responses",
            post(|body: Bytes| async move {
                assert!(
                    String::from_utf8_lossy(&body).contains("capture-request-secret"),
                    "upstream receives the original request body"
                );
                Json(serde_json::json!({
                    "id": "resp_test",
                    "output_text": "capture-response-secret",
                }))
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let root = temp_capture_root("http");
        let archive_root = root.join("archive");
        fs::create_dir_all(&archive_root).unwrap();
        std::env::set_var("SWITCHBACK_BODY_ARCHIVE_ROOT", &archive_root);
        let state_dir = root.join("state");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: true,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), Some(legacy_jsonl.clone()));
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let body: serde_json::Value = reqwest::Client::new()
            .post(format!("http://{tap_addr}/v1/responses"))
            .json(&serde_json::json!({
                "model": "gpt-test",
                "input": "capture-request-secret",
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["output_text"], "capture-response-secret");

        let logger = sb_bodylog::BodyLogger::new(sb_bodylog::BodyLoggerConfig {
            state_dir,
            archive_root,
            legacy_jsonl: Some(legacy_jsonl.clone()),
            inline_threshold_bytes: 1,
        })
        .unwrap();
        let mut status = logger.status().unwrap();
        for _ in 0..50 {
            if status.events >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            status = logger.status().unwrap();
        }
        assert_eq!(status.events, 2);
        assert_eq!(status.blobs, 2);
        assert_eq!(status.spool_backlog, 0);
        assert!(status.archive_available);

        let legacy = fs::read_to_string(legacy_jsonl).unwrap();
        assert!(legacy.contains("\"archive_path\""));
        assert!(!legacy.contains("capture-request-secret"));
        assert!(!legacy.contains("capture-response-secret"));
        std::env::remove_var("SWITCHBACK_BODY_ARCHIVE_ROOT");
    }

    #[tokio::test]
    async fn tap_body_capture_never_blocks_forwarding_on_a_busy_index() {
        let upstream = Router::new().route(
            "/v1/messages",
            post(|| async { Json(serde_json::json!({"ok": true})) }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let root = temp_capture_root("busy-index");
        let state_dir = root.join("state");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "busy-capture-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: true,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces, Some(legacy_jsonl));

        // Hold the capture index so BodyLogger::record must wait for its SQLite
        // busy timeout. Observability is allowed to lag or fail; it must never
        // delay the transparent forwarding path.
        let index_path = state_dir.join("body/index.sqlite");
        let capture_lock = rusqlite::Connection::open(index_path).unwrap();
        capture_lock.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let response = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            reqwest::Client::new()
                .post(format!("http://{tap_addr}/v1/messages"))
                .header("content-type", "application/json")
                .body(r#"{"model":"claude-x","messages":[]}"#)
                .send(),
        )
        .await;

        capture_lock.execute_batch("ROLLBACK").unwrap();
        let _ = fs::remove_dir_all(root);
        let response = response
            .expect("a busy evidence index must not delay the caller")
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn tap_capture_budget_bounds_total_bytes_and_releases_reservations() {
        let budget = CaptureBudget::new(1024);
        let first = budget.try_reserve(768).unwrap();
        assert_eq!(budget.queued_bytes(), 768);
        assert!(budget.try_reserve(257).is_none());
        assert_eq!(budget.queued_bytes(), 768);

        drop(first);
        assert_eq!(budget.queued_bytes(), 0);
        let full = budget.try_reserve(1024).unwrap();
        assert_eq!(budget.queued_bytes(), 1024);
        assert!(budget.try_reserve(1).is_none());
        drop(full);
        assert_eq!(budget.queued_bytes(), 0);
    }

    #[test]
    fn tap_body_capture_does_not_delay_runtime_shutdown_when_index_is_busy() {
        let root = temp_capture_root("busy-index-shutdown");
        let state_dir = root.join("state");
        let legacy_jsonl = state_dir.join("tap-bodies.jsonl");
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (start_tx, start_rx) = std::sync::mpsc::sync_channel(1);
        let (forwarded_tx, forwarded_rx) = std::sync::mpsc::sync_channel(1);
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::sync_channel(1);

        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let upstream = Router::new().route(
                    "/v1/messages",
                    post(|| async { Json(serde_json::json!({"ok": true})) }),
                );
                let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let up_addr = up_listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

                let traces = Arc::new(TraceLog::in_memory(16));
                let cfg = TapConfig {
                    id: "busy-shutdown-tap".to_string(),
                    bind: "127.0.0.1:0".to_string(),
                    upstream: format!("http://{up_addr}"),
                    capture_bodies: true,
                    headers: Default::default(),
                };
                let tap_app = build_tap_app(&cfg, traces, Some(legacy_jsonl));
                let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let tap_addr = tap_listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });
                ready_tx.send(tap_addr).unwrap();

                tokio::task::spawn_blocking(move || start_rx.recv().unwrap())
                    .await
                    .unwrap();
                let response: serde_json::Value = reqwest::Client::new()
                    .post(format!("http://{tap_addr}/v1/messages"))
                    .header("content-type", "application/json")
                    .body(r#"{"model":"claude-x","messages":[]}"#)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                forwarded_tx.send(response["ok"] == true).unwrap();
            });
            drop(runtime);
            shutdown_tx.send(()).unwrap();
        });

        ready_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let index_path = state_dir.join("body/index.sqlite");
        let capture_lock = rusqlite::Connection::open(index_path).unwrap();
        capture_lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        start_tx.send(()).unwrap();
        let forwarded = forwarded_rx.recv_timeout(std::time::Duration::from_secs(1));

        let shutdown_was_delayed = shutdown_rx
            .recv_timeout(std::time::Duration::from_millis(250))
            .is_err();
        capture_lock.execute_batch("ROLLBACK").unwrap();
        if shutdown_was_delayed {
            shutdown_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
        }
        worker.join().unwrap();
        let _ = fs::remove_dir_all(root);

        assert!(
            forwarded.unwrap(),
            "the caller must receive the upstream response while capture is backpressured"
        );
        assert!(
            !shutdown_was_delayed,
            "best-effort capture must not keep the Tokio runtime alive under backpressure"
        );
    }

    #[tokio::test]
    async fn tap_does_not_generate_413_for_large_native_request_bodies() {
        let upstream = Router::new()
            .route(
                "/responses",
                post(|body: Bytes| async move {
                    Json(serde_json::json!({
                        "body_len": body.len(),
                    }))
                }),
            )
            .layer(DefaultBodyLimit::disable());
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let previous_tap_limit = 64 * 1024 * 1024;
        let payload = Bytes::from(vec![b'a'; previous_tap_limit + 1]);
        let resp = reqwest::Client::new()
            .post(format!("http://{tap_addr}/responses"))
            .body(payload.clone())
            .send()
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "large native payloads must reach upstream instead of being rejected by the tap"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["body_len"], payload.len());
    }

    #[tokio::test]
    async fn tap_warns_when_sse_stream_closes_before_terminal_event() {
        let upstream = Router::new().route(
            "/responses",
            post(|| async move {
                let chunks = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::from_static(
                        b"event: response.created\ndata: {}\n\n",
                    )),
                    Ok::<_, std::io::Error>(Bytes::from_static(
                        b"event: response.output_text.delta\ndata: {\"delta\":\"hi\"}\n\n",
                    )),
                ]);
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(chunks))
                    .unwrap()
            }),
        );
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let text = reqwest::Client::new()
            .post(format!("http://{tap_addr}/responses"))
            .json(&serde_json::json!({"model": "gpt-5", "stream": true}))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(text.contains("response.created"));
        assert!(text.contains("response.output_text.delta"));

        let recent = traces.recent(8);
        assert_eq!(recent.len(), 1, "the tap recorded one trace");
        assert_eq!(recent[0].final_status, 200);
        assert!(
            recent[0]
                .warnings
                .iter()
                .any(|warning| warning == "upstream_closed_before_terminal"),
            "truncated SSE streams should be visible in the trace"
        );
    }

    #[tokio::test]
    async fn tap_tunnels_websocket_upgrade_and_frames() {
        async fn upstream_ws(headers: HeaderMap, ws: WebSocketUpgrade) -> impl IntoResponse {
            let seen_auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();
            let seen_beta = headers
                .get("openai-beta")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string();

            ws.on_upgrade(move |mut socket| async move {
                if let Some(Ok(AxumWsMessage::Text(text))) = socket.recv().await {
                    let reply = format!("upstream:{seen_auth}:{seen_beta}:{text}");
                    let _ = socket.send(AxumWsMessage::Text(reply.into())).await;
                }
            })
        }

        let upstream = Router::new().route("/backend-api/codex/realtime", any(upstream_ws));
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let mut request = format!("ws://{tap_addr}/backend-api/codex/realtime?session=abc")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_static("Bearer CLIENT-WS-TOKEN"),
        );
        request
            .headers_mut()
            .insert("openai-beta", HeaderValue::from_static("realtime=v1"));

        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        socket
            .send(TungsteniteMessage::Text("client-ping".into()))
            .await
            .unwrap();
        let echoed = socket.next().await.unwrap().unwrap();
        assert_eq!(
            echoed.into_text().unwrap(),
            "upstream:Bearer CLIENT-WS-TOKEN:realtime=v1:client-ping"
        );
        socket.close(None).await.unwrap();

        // Sending a WebSocket close frame does not wait for the server-side
        // relay task to observe it and finalize its trace.
        let recent = wait_for_traces(&traces, 1).await;
        assert_eq!(recent.len(), 1, "the tap recorded one WebSocket trace");
        assert_eq!(recent[0].route, "tap");
        assert_eq!(recent[0].final_status, 101);
        assert!(recent[0].streamed);
    }

    #[tokio::test]
    async fn tap_records_websocket_upstream_close_code_and_reason() {
        async fn upstream_ws(ws: WebSocketUpgrade) -> impl IntoResponse {
            ws.on_upgrade(move |mut socket| async move {
                if let Some(Ok(AxumWsMessage::Text(_))) = socket.recv().await {
                    let _ = socket
                        .send(AxumWsMessage::Close(Some(CloseFrame {
                            code: close_code::ERROR,
                            reason: "backend_overloaded".into(),
                        })))
                        .await;
                }
            })
        }

        let upstream = Router::new().route("/backend-api/codex/realtime", any(upstream_ws));
        let up_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(up_listener, upstream).await.unwrap() });

        let traces = Arc::new(TraceLog::in_memory(16));
        let cfg = TapConfig {
            id: "codex-tap".to_string(),
            bind: "127.0.0.1:0".to_string(),
            upstream: format!("http://{up_addr}"),
            capture_bodies: false,
            headers: Default::default(),
        };
        let tap_app = build_tap_app(&cfg, traces.clone(), None);
        let tap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tap_addr = tap_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(tap_listener, tap_app).await.unwrap() });

        let (mut socket, response) =
            tokio_tungstenite::connect_async(format!("ws://{tap_addr}/backend-api/codex/realtime"))
                .await
                .unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        socket
            .send(TungsteniteMessage::Text("client-ping".into()))
            .await
            .unwrap();
        let close = socket.next().await.unwrap().unwrap();
        match close {
            TungsteniteMessage::Close(Some(frame)) => {
                assert_eq!(u16::from(frame.code), close_code::ERROR);
                assert_eq!(frame.reason, "backend_overloaded");
            }
            other => panic!("expected upstream close frame, got {other:?}"),
        }

        let recent = wait_for_traces(&traces, 1).await;
        assert_eq!(recent.len(), 1, "the tap recorded one WebSocket trace");
        assert_eq!(recent[0].final_status, 101);
        assert!(
            recent[0]
                .warnings
                .iter()
                .any(|warning| warning == "websocket_upstream_closed:1011:backend_overloaded"),
            "upstream WebSocket close metadata should be visible in the trace: {recent:?}"
        );
    }
}
