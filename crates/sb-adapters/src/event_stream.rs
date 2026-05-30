//! AWS event-stream framing decoder — the streaming half of Bedrock.
//!
//! Bedrock streams `application/vnd.amazon.eventstream`, a binary framing that
//! is NOT SSE, so `ComposedAdapter`'s SSE decoder can't read it. Each message:
//!
//! ```text
//! [ total_len u32 ][ headers_len u32 ][ prelude_crc u32 ]   (12-byte prelude)
//! [ headers ... headers_len bytes ]
//! [ payload ... (total_len - 12 - headers_len - 4) bytes ]
//! [ message_crc u32 ]
//! ```
//!
//! A header is `[name_len u8][name][value_type u8][value]`; for string headers
//! (type 7, the only type AWS uses for `:event-type` / `:message-type` /
//! `:content-type`) the value is `[len u16][bytes]`. We parse string headers and
//! yield the payload. CRCs are not verified in v1 (the framing is internal to a
//! TLS session, not adversarial); the layout lengths are authoritative.

/// One decoded event-stream message.
#[derive(Debug, Clone)]
pub struct EventMessage {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl EventMessage {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Accumulates bytes from a stream and yields complete messages as they arrive.
#[derive(Default)]
pub struct EventStreamDecoder {
    buf: Vec<u8>,
}

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes received from the upstream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Parse the next complete message, if the buffer holds one. `None` = need
    /// more bytes; `Some(Err)` = malformed framing.
    pub fn next_message(&mut self) -> Option<Result<EventMessage, String>> {
        if self.buf.len() < 12 {
            return None;
        }
        let total_len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        let headers_len =
            u32::from_be_bytes([self.buf[4], self.buf[5], self.buf[6], self.buf[7]]) as usize;
        if total_len < 16 || headers_len > total_len.saturating_sub(16) {
            return Some(Err(format!("event-stream: bad lengths total={total_len} headers={headers_len}")));
        }
        if self.buf.len() < total_len {
            return None; // message not fully arrived yet
        }

        let headers_start = 12;
        let headers_end = headers_start + headers_len;
        let payload_end = total_len - 4; // before the trailing message CRC
        let headers = match parse_headers(&self.buf[headers_start..headers_end]) {
            Ok(h) => h,
            Err(e) => {
                self.buf.drain(..total_len);
                return Some(Err(e));
            }
        };
        let payload = self.buf[headers_end..payload_end].to_vec();
        self.buf.drain(..total_len);
        Some(Ok(EventMessage { headers, payload }))
    }
}

fn parse_headers(mut slice: &[u8]) -> Result<Vec<(String, String)>, String> {
    let mut headers = Vec::new();
    while !slice.is_empty() {
        if slice.len() < 2 {
            return Err("event-stream: truncated header".to_string());
        }
        let name_len = slice[0] as usize;
        slice = &slice[1..];
        if slice.len() < name_len + 1 {
            return Err("event-stream: truncated header name".to_string());
        }
        let name = String::from_utf8_lossy(&slice[..name_len]).to_string();
        slice = &slice[name_len..];
        let value_type = slice[0];
        slice = &slice[1..];
        // 7 = string: [u16 len][bytes]. Other types are unused by Bedrock here.
        if value_type != 7 {
            return Err(format!("event-stream: unsupported header value type {value_type}"));
        }
        if slice.len() < 2 {
            return Err("event-stream: truncated header value length".to_string());
        }
        let value_len = u16::from_be_bytes([slice[0], slice[1]]) as usize;
        slice = &slice[2..];
        if slice.len() < value_len {
            return Err("event-stream: truncated header value".to_string());
        }
        let value = String::from_utf8_lossy(&slice[..value_len]).to_string();
        slice = &slice[value_len..];
        headers.push((name, value));
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one event-stream message with a single string header + payload.
    fn frame(header: (&str, &str), payload: &[u8]) -> Vec<u8> {
        // header: [name_len u8][name][type=7][len u16][value]
        let (hname, hval) = header;
        let mut headers = Vec::new();
        headers.push(hname.len() as u8);
        headers.extend_from_slice(hname.as_bytes());
        headers.push(7u8);
        headers.extend_from_slice(&(hval.len() as u16).to_be_bytes());
        headers.extend_from_slice(hval.as_bytes());

        let total_len = 12 + headers.len() + payload.len() + 4;
        let mut msg = Vec::new();
        msg.extend_from_slice(&(total_len as u32).to_be_bytes());
        msg.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        msg.extend_from_slice(&0u32.to_be_bytes()); // prelude crc (unverified)
        msg.extend_from_slice(&headers);
        msg.extend_from_slice(payload);
        msg.extend_from_slice(&0u32.to_be_bytes()); // message crc (unverified)
        msg
    }

    #[test]
    fn decodes_a_single_message() {
        let mut dec = EventStreamDecoder::new();
        dec.push(&frame((":event-type", "chunk"), br#"{"delta":"hi"}"#));
        let msg = dec.next_message().unwrap().unwrap();
        assert_eq!(msg.header(":event-type"), Some("chunk"));
        assert_eq!(msg.payload, br#"{"delta":"hi"}"#);
        assert!(dec.next_message().is_none(), "buffer drained");
    }

    #[test]
    fn buffers_a_partial_message_until_complete() {
        let full = frame((":event-type", "chunk"), br#"{"x":1}"#);
        let mut dec = EventStreamDecoder::new();
        // Feed half — not enough yet.
        dec.push(&full[..full.len() / 2]);
        assert!(dec.next_message().is_none(), "incomplete → None");
        // Feed the rest — now it parses.
        dec.push(&full[full.len() / 2..]);
        assert_eq!(dec.next_message().unwrap().unwrap().payload, br#"{"x":1}"#);
    }

    #[test]
    fn decodes_two_back_to_back_messages() {
        let mut dec = EventStreamDecoder::new();
        dec.push(&frame((":event-type", "chunk"), b"a"));
        dec.push(&frame((":event-type", "chunk"), b"b"));
        assert_eq!(dec.next_message().unwrap().unwrap().payload, b"a");
        assert_eq!(dec.next_message().unwrap().unwrap().payload, b"b");
        assert!(dec.next_message().is_none());
    }
}
