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

use axum::body::{Body, Bytes};
use axum::extract::ws::{
    rejection::WebSocketUpgradeRejection, Message as AxumWsMessage, WebSocket, WebSocketUpgrade,
};
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
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

const TAP_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

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
        .layer(DefaultBodyLimit::max(TAP_MAX_REQUEST_BYTES))
        .with_state(state)
}

async fn forward(
    State(st): State<TapState>,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
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

    // Best-effort metadata from the request body (never logged beyond this).
    let parsed: Option<serde_json::Value> = serde_json::from_slice(&body).ok();
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
    let mut rb = st.client.request(method, &url).body(body.clone());
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
                &request_id,
                &inbound_model,
                streamed,
                502,
                started,
                false,
            );
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap upstream request failed");
            return (StatusCode::BAD_GATEWAY, "tap upstream request failed").into_response();
        }
    };

    let status = upstream_resp.status();
    record_trace(
        &st,
        &request_id,
        &inbound_model,
        streamed,
        status.as_u16(),
        started,
        status.is_success(),
    );

    // Copy the upstream status + response headers (minus hop-by-hop) and stream
    // the body back unchanged. Capture tees the body to the sink without buffering.
    let mut builder = Response::builder().status(status);
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let body = match &st.capture_sink {
        Some(sink) => Body::from_stream(CaptureStream {
            inner: upstream_resp.bytes_stream(),
            buf: Vec::new(),
            finalize: Some(CaptureFinalize {
                sink: sink.clone(),
                request_id,
                upstream: st.upstream.clone(),
                model: inbound_model,
                request_body: body,
                status: status.as_u16(),
            }),
        }),
        None => Body::from_stream(upstream_resp.bytes_stream()),
    };

    builder.body(body).unwrap_or_else(|_| {
        (StatusCode::BAD_GATEWAY, "tap could not build response").into_response()
    })
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
            record_trace(&st, &request_id, "", true, 502, started, false);
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
            record_trace(&st, &request_id, "", true, 502, started, false);
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
            record_trace(&st, &request_id, "", true, 502, started, false);
            tracing::warn!(tap = %st.id, host = %st.upstream_host, error = %err, "tap websocket upstream connect failed");
            return (
                StatusCode::BAD_GATEWAY,
                "tap websocket upstream connect failed",
            )
                .into_response();
        }
    };

    record_trace(&st, &request_id, "", true, 101, started, true);
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

fn record_trace(
    st: &TapState,
    request_id: &str,
    inbound_model: &str,
    streamed: bool,
    status: u16,
    started: Instant,
    ok: bool,
) {
    let mut decision = RouteDecision::new(request_id, "transparent_tap");
    decision.add_reason(format!("tap={}", st.id));
    decision.add_reason(format!("upstream={}", st.upstream_host));
    let latency = started.elapsed().as_millis() as u64;
    let mut trace = RequestTrace::start(request_id, 0, inbound_model, "tap", decision)
        .with_client_metadata(Some(st.id.clone()), Some("passthrough".to_string()));
    let class = if ok { None } else { Some("upstream_error") };
    trace.attempt(match class {
        None => Attempt::success(
            st.upstream_host.clone(),
            "tap",
            inbound_model,
            "client-native",
            "direct",
            latency,
        ),
        Some(c) => Attempt::failed(
            st.upstream_host.clone(),
            "tap",
            inbound_model,
            "client-native",
            "direct",
            latency,
            c,
            false,
        ),
    });
    st.traces.record(trace.finish(status, latency, streamed));
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

/// Tees a forwarded byte stream into an accumulator, writing the full
/// `{request, response}` to the capture sink when the stream ends — so streaming
/// to the client is never buffered.
struct CaptureStream<S> {
    inner: S,
    buf: Vec<u8>,
    finalize: Option<CaptureFinalize>,
}

impl<S> Stream for CaptureStream<S>
where
    S: Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                self.buf.extend_from_slice(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(err))) => Poll::Ready(Some(Err(std::io::Error::other(err)))),
            Poll::Ready(None) => {
                if let Some(fin) = self.finalize.take() {
                    let body = std::mem::take(&mut self.buf);
                    write_capture(fin, body);
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
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
    async fn tap_accepts_native_sized_request_bodies() {
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

        let payload = Bytes::from(vec![b'a'; 3 * 1024 * 1024]);
        let resp = reqwest::Client::new()
            .post(format!("http://{tap_addr}/responses"))
            .body(payload.clone())
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["body_len"], payload.len());
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
