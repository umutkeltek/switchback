//! The **Transport** seam of the `Codec × Signer × Transport` decomposition.
//!
//! A [`Transport`] owns the wire FRAMING — how to carve model-native JSON events
//! out of a raw upstream byte stream — independent of semantics (which is the
//! codec's `decoder`). Two framings exist today:
//!   - [`HttpTransport`] → [`SseFramer`]: text SSE, `data:` lines, blank-line frames.
//!   - [`EventStreamTransport`] → [`EventStreamFramer`]: AWS binary
//!     `application/vnd.amazon.eventstream`, each chunk wrapping a base64 event.
//!
//! Splitting framing out is what lets Bedrock (event-stream) ride the one
//! `ComposedAdapter` execute loop instead of a bespoke adapter; the codec's
//! `decoder` then turns each extracted JSON value into canonical events.

use base64::Engine as _;
use serde_json::Value;

use crate::event_stream::EventStreamDecoder;

const MAX_SSE_BUFFER_BYTES: usize = 1_048_576;

/// Extracts complete model-native JSON values from pushed raw bytes. Stateful
/// (buffers partial frames across chunks). One per streamed response.
pub trait Framer: Send {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String>;
}

/// The framing strategy for a wire format.
pub trait Transport: Send + Sync {
    /// `Accept` header to send on a streaming request, if the framing needs one.
    fn stream_accept(&self) -> Option<&'static str> {
        None
    }
    /// A fresh framer for one streamed response.
    fn framer(&self) -> Box<dyn Framer>;
}

// --- HTTP + text SSE --------------------------------------------------------

/// Standard HTTP transport: streamed responses are text SSE.
pub struct HttpTransport;

impl Transport for HttpTransport {
    fn framer(&self) -> Box<dyn Framer> {
        Box::new(SseFramer::default())
    }
}

/// Parses text SSE: blank-line separated frames; all `data:` lines are joined as
/// the JSON payload. `[DONE]` and empty data are skipped. Provider error frames
/// are surfaced as stream errors instead of being fed into a codec as normal
/// model data.
#[derive(Default)]
pub struct SseFramer {
    buffer: String,
}

impl Framer for SseFramer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        let chunk = String::from_utf8_lossy(bytes)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        self.buffer.push_str(&chunk);
        if self.buffer.len() > MAX_SSE_BUFFER_BYTES {
            return Err(format!(
                "SSE frame buffer exceeded {} bytes",
                MAX_SSE_BUFFER_BYTES
            ));
        }
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.find("\n\n") {
            let frame: String = self.buffer.drain(..pos + 2).collect();
            if let Some(value) = parse_sse_frame(&frame)? {
                out.push(value);
            }
        }
        Ok(out)
    }
}

fn parse_sse_frame(frame: &str) -> Result<Option<Value>, String> {
    let mut event = None;
    let mut data_lines = Vec::new();

    for line in frame.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(value) = sse_field(line, "event") {
            event = Some(value.to_string());
        } else if let Some(value) = sse_field(line, "data") {
            data_lines.push(value.to_string());
        }
    }

    if data_lines.is_empty() {
        return Ok(None);
    }

    let data = data_lines.join("\n");
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(None);
    }

    let value = serde_json::from_str::<Value>(data)
        .map_err(|error| format!("malformed SSE JSON frame: {error}"))?;

    if event.as_deref() == Some("error") {
        return Err(stream_error_message(&value)
            .unwrap_or_else(|| "upstream stream error event".to_string()));
    }
    if let Some(message) = stream_error_message(&value) {
        return Err(message);
    }

    Ok(Some(value))
}

fn sse_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(field)?;
    let rest = rest.strip_prefix(':')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

fn stream_error_message(value: &Value) -> Option<String> {
    let error = value.get("error")?;
    if let Some(message) = error.as_str() {
        return Some(message.to_string());
    }
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return Some(message.to_string());
    }
    Some("upstream stream error".to_string())
}

// --- AWS binary event-stream ------------------------------------------------

/// AWS Bedrock streaming transport: binary `application/vnd.amazon.eventstream`.
pub struct EventStreamTransport;

impl Transport for EventStreamTransport {
    fn stream_accept(&self) -> Option<&'static str> {
        Some("application/vnd.amazon.eventstream")
    }
    fn framer(&self) -> Box<dyn Framer> {
        Box::new(EventStreamFramer::new())
    }
}

/// Decodes AWS event-stream frames; each chunk message wraps the model-native
/// event as `{"bytes": base64(<event json>)}`, which we unwrap to the JSON value.
pub struct EventStreamFramer {
    decoder: EventStreamDecoder,
}

impl EventStreamFramer {
    fn new() -> Self {
        Self {
            decoder: EventStreamDecoder::new(),
        }
    }
}

impl Framer for EventStreamFramer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        self.decoder.push(bytes);
        let mut out = Vec::new();
        loop {
            match self.decoder.next_message() {
                None => break,
                Some(Err(e)) => return Err(e),
                Some(Ok(msg)) => {
                    if let Some(error) = event_stream_error(&msg) {
                        return Err(error);
                    }
                    if let Some(value) = unwrap_chunk(&msg.payload) {
                        out.push(value);
                    }
                }
            }
        }
        Ok(out)
    }
}

fn event_stream_error(msg: &crate::event_stream::EventMessage) -> Option<String> {
    let message_type = msg.header(":message-type");
    let event_type = msg.header(":event-type");
    let is_exception = message_type == Some("exception")
        || event_type
            .map(|kind| kind.to_ascii_lowercase().ends_with("exception"))
            .unwrap_or(false);
    if !is_exception {
        return None;
    }

    let kind = event_type.unwrap_or("exception");
    let message =
        event_stream_error_message(&msg.payload).unwrap_or_else(|| "upstream stream error".into());
    Some(format!("bedrock stream {kind}: {message}"))
}

fn event_stream_error_message(payload: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    ["message", "Message", "errorMessage", "originalMessage"]
        .iter()
        .find_map(|field| value.get(*field).and_then(Value::as_str))
        .map(ToString::to_string)
}

/// Extract + decode a Bedrock stream chunk's wrapped model event.
fn unwrap_chunk(payload: &[u8]) -> Option<Value> {
    let wrapper: Value = serde_json::from_slice(payload).ok()?;
    let b64 = wrapper.get("bytes").and_then(|v| v.as_str())?;
    let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    serde_json::from_slice(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event_stream_frame(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut encoded_headers = Vec::new();
        for (name, value) in headers {
            encoded_headers.push(name.len() as u8);
            encoded_headers.extend_from_slice(name.as_bytes());
            encoded_headers.push(7u8);
            encoded_headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            encoded_headers.extend_from_slice(value.as_bytes());
        }

        let total_len = 12 + encoded_headers.len() + payload.len() + 4;
        let mut msg = Vec::new();
        msg.extend_from_slice(&(total_len as u32).to_be_bytes());
        msg.extend_from_slice(&(encoded_headers.len() as u32).to_be_bytes());
        msg.extend_from_slice(&0u32.to_be_bytes());
        msg.extend_from_slice(&encoded_headers);
        msg.extend_from_slice(payload);
        msg.extend_from_slice(&0u32.to_be_bytes());
        msg
    }

    #[test]
    fn sse_framer_extracts_data_payloads_across_chunks() {
        let mut f = SseFramer::default();
        // A frame split across two pushes, plus [DONE] which is skipped.
        assert!(f.push(b"data: {\"a\":1}\n").unwrap().is_empty());
        let out = f.push(b"\ndata: [DONE]\n\n").unwrap();
        assert_eq!(out, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn sse_framer_accepts_crlf_frames() {
        let mut f = SseFramer::default();

        let out = f
            .push(b"event: message\r\ndata: {\"a\":1}\r\n\r\n")
            .unwrap();

        assert_eq!(out, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn sse_framer_joins_multiline_data() {
        let mut f = SseFramer::default();

        let out = f
            .push(b"data: {\"a\":\ndata: 1,\ndata: \"b\": 2}\n\n")
            .unwrap();

        assert_eq!(out, vec![serde_json::json!({"a": 1, "b": 2})]);
    }

    #[test]
    fn sse_framer_reports_malformed_json() {
        let mut f = SseFramer::default();

        let error = f.push(b"data: {not-json}\n\n").unwrap_err();

        assert!(error.contains("malformed SSE JSON frame"));
    }

    #[test]
    fn sse_rejects_frame_larger_than_max() {
        let mut f = SseFramer::default();
        let huge = vec![b'a'; MAX_SSE_BUFFER_BYTES + 1];

        let error = f.push(&huge).unwrap_err();

        assert!(error.contains("SSE frame buffer exceeded"));
    }

    #[test]
    fn sse_framer_maps_error_frames_to_errors() {
        let mut f = SseFramer::default();

        let error = f
            .push(b"event: error\ndata: {\"error\":{\"message\":\"quota exhausted\"}}\n\n")
            .unwrap_err();

        assert!(error.contains("quota exhausted"));
    }

    #[test]
    fn sse_framer_maps_openai_style_error_payloads_to_errors() {
        let mut f = SseFramer::default();

        let error = f
            .push(b"data: {\"error\":{\"message\":\"model overloaded\"}}\n\n")
            .unwrap_err();

        assert!(error.contains("model overloaded"));
    }

    #[test]
    fn http_transport_has_no_special_accept() {
        assert!(HttpTransport.stream_accept().is_none());
        assert_eq!(
            EventStreamTransport.stream_accept(),
            Some("application/vnd.amazon.eventstream")
        );
    }

    #[test]
    fn event_stream_framer_maps_bedrock_exceptions_to_errors() {
        let mut f = EventStreamFramer::new();
        let frame = event_stream_frame(
            &[
                (":message-type", "exception"),
                (":event-type", "throttlingException"),
            ],
            br#"{"message":"Too many requests"}"#,
        );

        let error = f.push(&frame).unwrap_err();

        assert!(error.contains("throttlingException"));
        assert!(error.contains("Too many requests"));
    }
}
