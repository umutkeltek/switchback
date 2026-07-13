use std::sync::{Arc, Mutex};

use futures::StreamExt;
use sb_adapter::EventStream;
use sb_core::{AiRequest, AiResponse, AiStreamEvent, ContentPart, FinishReason, Role};
use tokio::sync::OwnedSemaphorePermit;
use zeroize::{Zeroize, Zeroizing};

use super::{QualityEval, QualityJob};

const TRUNCATION_MARKER: &str = "\n[... CONTEXT TRUNCATED ...]\n";

pub(super) struct CaptureBuffer {
    bytes: Zeroizing<Vec<u8>>,
    max: usize,
    invalid: bool,
}

impl CaptureBuffer {
    fn new(max: usize) -> Self {
        Self {
            bytes: Zeroizing::new(Vec::with_capacity(max.min(4096))),
            max,
            invalid: false,
        }
    }

    pub(super) fn from_bytes(bytes: Vec<u8>, max: usize) -> Option<Self> {
        Self::from_zeroizing(Zeroizing::new(bytes), max)
    }

    fn from_zeroizing(bytes: Zeroizing<Vec<u8>>, max: usize) -> Option<Self> {
        if bytes.len() > max || std::str::from_utf8(&bytes).is_err() {
            return None;
        }
        Some(Self {
            bytes,
            max,
            invalid: false,
        })
    }

    pub(super) fn append(&mut self, text: &str) -> bool {
        if self.invalid {
            return false;
        }
        if self.bytes.len().saturating_add(text.len()) > self.max {
            self.bytes.zeroize();
            self.invalid = true;
            return false;
        }
        self.bytes.extend_from_slice(text.as_bytes());
        true
    }

    fn invalidate(&mut self) {
        self.bytes.zeroize();
        self.invalid = true;
    }

    pub(super) fn as_str(&self) -> Option<&str> {
        if self.invalid {
            None
        } else {
            std::str::from_utf8(&self.bytes).ok()
        }
    }

    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(super) fn chars(&self) -> usize {
        self.as_str()
            .map(str::chars)
            .map(Iterator::count)
            .unwrap_or(0)
    }
}

pub(super) fn request_is_text_only(req: &AiRequest) -> bool {
    req.messages.iter().all(|message| {
        message
            .content
            .iter()
            .all(|part| matches!(part, ContentPart::Text { .. }))
    })
}

fn append_prefix(dst: &mut Vec<u8>, src: &[u8], cap: usize) {
    let remaining = cap.saturating_sub(dst.len());
    if remaining == 0 {
        return;
    }
    let mut end = src.len().min(remaining);
    while end > 0 && std::str::from_utf8(&src[..end]).is_err() {
        end -= 1;
    }
    dst.extend_from_slice(&src[..end]);
}

fn append_tail(dst: &mut Vec<u8>, src: &[u8], cap: usize) {
    if cap == 0 {
        dst.zeroize();
        return;
    }
    if src.len() >= cap {
        dst.zeroize();
        let mut start = src.len() - cap;
        while start < src.len() && std::str::from_utf8(&src[start..]).is_err() {
            start += 1;
        }
        dst.extend_from_slice(&src[start..]);
        return;
    }
    let overflow = dst.len().saturating_add(src.len()).saturating_sub(cap);
    if overflow > 0 {
        let mut start = overflow;
        while start < dst.len() && std::str::from_utf8(&dst[start..]).is_err() {
            start += 1;
        }
        dst[..start].zeroize();
        dst.drain(..start);
    }
    dst.extend_from_slice(src);
}

pub(super) fn render_request(req: &AiRequest, max: usize) -> Option<CaptureBuffer> {
    let mut head = Zeroizing::new(Vec::with_capacity(max.min(4096)));
    let mut tail = Zeroizing::new(Vec::with_capacity(max.min(4096)));
    let mut total = 0usize;
    {
        let mut feed = |text: &str| {
            total = total.saturating_add(text.len());
            append_prefix(&mut head, text.as_bytes(), max);
            append_tail(&mut tail, text.as_bytes(), max);
        };

        if let Some(system) = &req.system {
            feed("[system]\n");
            feed(system);
            feed("\n");
        }
        for message in &req.messages {
            let role = match message.role {
                Role::System => "[system]\n",
                Role::User => "[user]\n",
                Role::Assistant => "[assistant]\n",
                Role::Tool => "[tool]\n",
            };
            feed(role);
            for part in &message.content {
                let ContentPart::Text { text } = part else {
                    return None;
                };
                feed(text);
            }
            feed("\n");
        }
    }

    if total <= max {
        return CaptureBuffer::from_zeroizing(head, max);
    }
    if max < TRUNCATION_MARKER.len() {
        let mut marker = TRUNCATION_MARKER.as_bytes()[..max].to_vec();
        while !marker.is_empty() && std::str::from_utf8(&marker).is_err() {
            marker.pop();
        }
        return CaptureBuffer::from_bytes(marker, max);
    }
    let payload = max - TRUNCATION_MARKER.len();
    let head_cap = payload / 2;
    let tail_cap = payload - head_cap;
    let mut output = Zeroizing::new(Vec::with_capacity(max));
    append_prefix(&mut output, &head, head_cap);
    output.extend_from_slice(TRUNCATION_MARKER.as_bytes());
    let tail_start = tail.len().saturating_sub(tail_cap);
    let mut start = tail_start;
    while start < tail.len() && std::str::from_utf8(&tail[start..]).is_err() {
        start += 1;
    }
    output.extend_from_slice(&tail[start..]);
    CaptureBuffer::from_zeroizing(output, max)
}

pub(crate) struct QualityCapture {
    eval: Arc<QualityEval>,
    permit: Option<OwnedSemaphorePermit>,
    served_request_id: String,
    sample_revision: u64,
    evaluator_id: String,
    input: CaptureBuffer,
    output_max: usize,
    min_output_chars: usize,
}

impl QualityCapture {
    pub(super) fn new(
        eval: Arc<QualityEval>,
        permit: OwnedSemaphorePermit,
        served_request_id: String,
        sample_revision: u64,
        evaluator_id: String,
        input: CaptureBuffer,
        cfg: &sb_core::QualityEvalConfig,
    ) -> Self {
        Self {
            eval,
            permit: Some(permit),
            served_request_id,
            sample_revision,
            evaluator_id,
            input,
            output_max: cfg.max_output_bytes,
            min_output_chars: cfg.min_output_chars,
        }
    }

    fn complete_buffer(
        mut self,
        served_target_id: String,
        class: String,
        output: CaptureBuffer,
    ) -> bool {
        self.permit.take();
        if output.as_str().is_none() || output.chars() < self.min_output_chars {
            return false;
        }
        let job = QualityJob {
            served_request_id: self.served_request_id.clone(),
            served_target_id,
            class,
            sample_revision: self.sample_revision,
            evaluator_id: self.evaluator_id.clone(),
            input: std::mem::replace(&mut self.input, CaptureBuffer::new(0)),
            output,
        };
        self.eval.try_enqueue(job)
    }

    pub(crate) fn complete_response(
        self,
        served_target_id: String,
        class: String,
        response: &AiResponse,
    ) -> bool {
        if response.finish_reason != FinishReason::Stop {
            return false;
        }
        let mut output = CaptureBuffer::new(self.output_max);
        for part in &response.message.content {
            let ContentPart::Text { text } = part else {
                output.invalidate();
                return false;
            };
            if !output.append(text) {
                return false;
            }
        }
        self.complete_buffer(served_target_id, class, output)
    }
}

struct StreamingCapture {
    capture: QualityCapture,
    served_target_id: String,
    class: String,
    output: CaptureBuffer,
}

pub(crate) struct StreamCaptureHandle {
    inner: Arc<Mutex<Option<StreamingCapture>>>,
}

impl StreamCaptureHandle {
    pub(crate) fn complete(self, finish_reason: Option<FinishReason>) -> bool {
        let Some(capture) = self.inner.lock().ok().and_then(|mut inner| inner.take()) else {
            return false;
        };
        if finish_reason != Some(FinishReason::Stop) {
            return false;
        }
        capture
            .capture
            .complete_buffer(capture.served_target_id, capture.class, capture.output)
    }
}

pub(crate) fn tee_stream(
    stream: EventStream,
    capture: QualityCapture,
    served_target_id: String,
    class: String,
) -> (EventStream, StreamCaptureHandle) {
    let output_max = capture.output_max;
    let inner = Arc::new(Mutex::new(Some(StreamingCapture {
        capture,
        served_target_id,
        class,
        output: CaptureBuffer::new(output_max),
    })));
    let sink = Arc::clone(&inner);
    let stream = stream
        .map(move |item| {
            if let Ok(mut guard) = sink.lock() {
                if let Some(capture) = guard.as_mut() {
                    match &item {
                        Ok(AiStreamEvent::TextDelta { text }) => {
                            capture.output.append(text);
                        }
                        Ok(
                            AiStreamEvent::MessageStart { .. }
                            | AiStreamEvent::UsageDelta { .. }
                            | AiStreamEvent::MessageEnd { .. },
                        ) => {}
                        _ => capture.output.invalidate(),
                    }
                }
            }
            item
        })
        .boxed();
    (stream, StreamCaptureHandle { inner })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use futures::stream;
    use sb_adapter::AdapterError;
    use sb_core::{Message, Usage};

    use super::*;

    #[test]
    fn capture_buffer_accepts_exact_cap_and_rejects_overflow_without_truncating() {
        let mut buffer = CaptureBuffer::new(4);
        assert!(buffer.append("éé"));
        assert_eq!(buffer.as_str(), Some("éé"));
        assert!(!buffer.append("x"));
        assert_eq!(buffer.len(), 0);
        assert!(buffer.as_str().is_none());
    }

    #[test]
    fn request_rendering_preserves_head_tail_marker_and_utf8_boundaries() {
        let mut request = AiRequest::new(
            "mock/echo",
            vec![Message::user(format!("old-{}-new", "🙂".repeat(40)))],
        );
        request.system = Some("important-system-rule".to_string());
        let rendered = render_request(&request, 96).unwrap();
        let text = rendered.as_str().unwrap();
        assert!(text.starts_with("[system]"));
        assert!(text.contains("CONTEXT TRUNCATED"));
        assert!(text.ends_with("-new\n"));
        assert!(rendered.len() <= 96);
    }

    #[tokio::test]
    async fn tee_keeps_text_only_and_invalidates_reasoning() {
        let cfg = sb_core::QualityEvalConfig {
            enabled: true,
            min_output_chars: 1,
            max_output_bytes: 32,
            ..Default::default()
        };
        let eval = Arc::new(QualityEval::new(&cfg));
        let permit = eval.permits.clone().try_acquire_owned().unwrap();
        let input = CaptureBuffer::from_bytes(b"input".to_vec(), 16).unwrap();
        let capture = QualityCapture::new(
            eval.clone(),
            permit,
            "req".into(),
            1,
            crate::quality_eval::rubric::evaluator_id(&cfg.body_allowed_targets),
            input,
            &cfg,
        );
        let source: EventStream = stream::iter(vec![
            Ok::<_, AdapterError>(AiStreamEvent::MessageStart {
                id: "r".into(),
                model: "m".into(),
            }),
            Ok(AiStreamEvent::TextDelta { text: "ok".into() }),
            Ok(AiStreamEvent::ReasoningDelta {
                text: "unsupported".into(),
            }),
            Ok(AiStreamEvent::UsageDelta {
                usage: Usage::default(),
            }),
        ])
        .boxed();
        let (mut stream, handle) = tee_stream(source, capture, "mock/echo".into(), "any".into());
        while stream.next().await.is_some() {}
        assert!(!handle.complete(Some(FinishReason::Stop)));
        assert_eq!(eval.stats.queue_depth.load(Ordering::Relaxed), 0);
    }
}
