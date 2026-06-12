//! Transparent tap (Mode B). A passthrough listener that forwards a client's
//! request VERBATIM to a fixed upstream — its own `Authorization`, its own
//! headers, its raw body — streams the response back unchanged, and only
//! observes (records a metadata trace; optionally the full bodies to a separate
//! local file). No canonical-IR round-trip, no credential lease: the vendor sees
//! the native client's request, so there is nothing re-shaped to flag.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::ws::{
    rejection::WebSocketUpgradeRejection, Message as AxumWsMessage, WebSocket, WebSocketUpgrade,
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
use sb_core::{RouteDecision, TapConfig};
use sb_trace::{Attempt, RequestTrace, TraceLog};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as TungsteniteMessage};

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

fn is_hop_by_hop(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str())
}

#[derive(Clone)]
struct TapState {
    id: String,
    upstream: String,
    upstream_host: String,
    capture_sink: Option<PathBuf>,
    traces: Arc<TraceLog>,
    client: reqwest::Client,
}

/// Build the axum app for one tap listener. Every request, any method/path, is
/// forwarded to `tap.upstream`. `capture_sink` (when `capture_bodies`) receives
/// `{request, response}` JSONL.
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
    let state = TapState {
        id: tap.id.clone(),
        upstream,
        upstream_host,
        capture_sink: if tap.capture_bodies {
            capture_sink
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

    let capture_finalize = match (&st.capture_sink, capture_request_body) {
        (Some(sink), Some(request_body)) => Some(CaptureFinalize {
            sink: sink.clone(),
            request_id: request_id.clone(),
            upstream: st.upstream.clone(),
            model: inbound_model.clone(),
            request_body,
            status: status.as_u16(),
        }),
        _ => None,
    };
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
    if st.capture_sink.is_some() {
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

    record_trace(
        &st,
        TapTraceInput {
            request_id: &request_id,
            inbound_model: "",
            streamed: true,
            status: 101,
            started,
            ok: true,
            warning: None,
        },
    );
    ws.protocols(requested_protocols)
        .on_upgrade(move |client_socket| bridge_websockets(client_socket, upstream_socket))
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
) {
    let (mut client_tx, mut client_rx) = client_socket.split();
    let (mut upstream_tx, mut upstream_rx) = upstream_socket.split();

    let client_to_upstream = async {
        while let Some(Ok(message)) = client_rx.next().await {
            let Some(message) = axum_to_tungstenite(message) else {
                continue;
            };
            let closing = message.is_close();
            if upstream_tx.send(message).await.is_err() || closing {
                break;
            }
        }
        let _ = upstream_tx.close().await;
    };

    let upstream_to_client = async {
        while let Some(Ok(message)) = upstream_rx.next().await {
            let Some(message) = tungstenite_to_axum(message) else {
                continue;
            };
            let closing = matches!(message, AxumWsMessage::Close(_));
            if client_tx.send(message).await.is_err() || closing {
                break;
            }
        }
        let _ = client_tx.close().await;
    };

    tokio::select! {
        _ = client_to_upstream => {}
        _ = upstream_to_client => {}
    }
}

fn axum_to_tungstenite(message: AxumWsMessage) -> Option<TungsteniteMessage> {
    match message {
        AxumWsMessage::Text(text) => Some(TungsteniteMessage::Text(text.to_string().into())),
        AxumWsMessage::Binary(binary) => Some(TungsteniteMessage::Binary(binary)),
        AxumWsMessage::Ping(ping) => Some(TungsteniteMessage::Ping(ping)),
        AxumWsMessage::Pong(pong) => Some(TungsteniteMessage::Pong(pong)),
        AxumWsMessage::Close(_) => Some(TungsteniteMessage::Close(None)),
    }
}

fn tungstenite_to_axum(message: TungsteniteMessage) -> Option<AxumWsMessage> {
    match message {
        TungsteniteMessage::Text(text) => Some(AxumWsMessage::Text(text.to_string().into())),
        TungsteniteMessage::Binary(binary) => Some(AxumWsMessage::Binary(binary)),
        TungsteniteMessage::Ping(ping) => Some(AxumWsMessage::Ping(ping)),
        TungsteniteMessage::Pong(pong) => Some(AxumWsMessage::Pong(pong)),
        TungsteniteMessage::Close(_) => Some(AxumWsMessage::Close(None)),
        TungsteniteMessage::Frame(_) => None,
    }
}

struct TapTraceInput<'a> {
    request_id: &'a str,
    inbound_model: &'a str,
    streamed: bool,
    status: u16,
    started: Instant,
    ok: bool,
    warning: Option<&'a str>,
}

fn record_trace(st: &TapState, input: TapTraceInput<'_>) {
    let mut decision = RouteDecision::new(input.request_id, "transparent_tap");
    decision.add_reason(format!("tap={}", st.id));
    decision.add_reason(format!("upstream={}", st.upstream_host));
    let latency = input.started.elapsed().as_millis() as u64;
    let mut trace = RequestTrace::start(input.request_id, 0, input.inbound_model, "tap", decision)
        .with_client_metadata(Some(st.id.clone()), Some("passthrough".to_string()));
    if let Some(warning) = input.warning {
        trace.warning(warning);
    }
    let class = if input.ok {
        None
    } else {
        Some(input.warning.unwrap_or("upstream_error"))
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
    sink: PathBuf,
    request_id: String,
    upstream: String,
    model: String,
    request_body: Bytes,
    status: u16,
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
    // Off the runtime: a single JSONL append. Bodies are the user's own prompts
    // and the model's replies, written only because they enabled capture.
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let line = serde_json::json!({
            "request_id": fin.request_id,
            "timestamp_unix": sb_trace::now_unix(),
            "upstream": fin.upstream,
            "model": fin.model,
            "status": fin.status,
            "request": String::from_utf8_lossy(&fin.request_body),
            "response": String::from_utf8_lossy(&response),
        });
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&fin.sink)
        {
            let _ = writeln!(f, "{line}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::ws::{Message as AxumWsMessage, WebSocketUpgrade};
    use axum::routing::{any, post};
    use axum::Json;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

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

        let recent = traces.recent(8);
        assert_eq!(recent.len(), 1, "the tap recorded one WebSocket trace");
        assert_eq!(recent[0].route, "tap");
        assert_eq!(recent[0].final_status, 101);
        assert!(recent[0].streamed);
    }
}
