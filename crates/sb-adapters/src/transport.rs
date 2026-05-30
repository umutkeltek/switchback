//! The **Transport** seam of the `Codec × Signer × Transport` decomposition.
//!
//! A [`Transport`] owns the wire FRAMING — how to carve model-native JSON events
//! out of a raw upstream byte stream — independent of semantics (which is the
//! codec's `decoder`). Two framings exist today:
//!   - [`HttpTransport`] → [`SseFramer`]: text SSE, `data:` lines, `\n\n` frames.
//!   - [`EventStreamTransport`] → [`EventStreamFramer`]: AWS binary
//!     `application/vnd.amazon.eventstream`, each chunk wrapping a base64 event.
//!
//! Splitting framing out is what lets Bedrock (event-stream) ride the one
//! `ComposedAdapter` execute loop instead of a bespoke adapter; the codec's
//! `decoder` then turns each extracted JSON value into canonical events.

use base64::Engine as _;
use serde_json::Value;

use crate::event_stream::EventStreamDecoder;

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

/// Parses text SSE: blank-line (`\n\n`) separated frames; the `data:` line's JSON
/// is the payload (any `event:` line is ignored — codecs dispatch on payload
/// fields). `[DONE]` and empty data are skipped.
#[derive(Default)]
pub struct SseFramer {
    buffer: String,
}

impl Framer for SseFramer {
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Value>, String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some(pos) = self.buffer.find("\n\n") {
            let frame: String = self.buffer.drain(..pos + 2).collect();
            for line in frame.lines() {
                let trimmed = line.trim();
                if let Some(data) = trimmed.strip_prefix("data:") {
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(data) {
                        out.push(value);
                    }
                }
            }
        }
        Ok(out)
    }
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
                    if let Some(value) = unwrap_chunk(&msg.payload) {
                        out.push(value);
                    }
                }
            }
        }
        Ok(out)
    }
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

    #[test]
    fn sse_framer_extracts_data_payloads_across_chunks() {
        let mut f = SseFramer::default();
        // A frame split across two pushes, plus [DONE] which is skipped.
        assert!(f.push(b"data: {\"a\":1}\n").unwrap().is_empty());
        let out = f.push(b"\ndata: [DONE]\n\n").unwrap();
        assert_eq!(out, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn http_transport_has_no_special_accept() {
        assert!(HttpTransport.stream_accept().is_none());
        assert_eq!(
            EventStreamTransport.stream_accept(),
            Some("application/vnd.amazon.eventstream")
        );
    }
}
