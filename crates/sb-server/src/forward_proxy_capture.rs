//! File-backed capture for large intercepted requests and streamed responses.
use std::io::Read as _;
use std::sync::Arc;

use axum::body::Bytes;
use futures::Stream;
use sb_bodylog::{BodyCaptureGap, BodyEventInput, ReadyCapture};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::http_capture::{
    bounded_file_io, reserve_body_memory, FinishedCapture, HttpCapture, MemoryPermit,
};
use crate::tap::{CapturePayload, CaptureWorker};

const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub(crate) enum CaptureBodyFailure {
    Admission(&'static str),
    Client(std::io::Error),
    Storage(&'static str),
}

impl std::fmt::Display for CaptureBodyFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admission(reason) | Self::Storage(reason) => f.write_str(reason),
            Self::Client(error) => write!(f, "client body incomplete: {error}"),
        }
    }
}
impl std::error::Error for CaptureBodyFailure {}

fn transport_metadata(input: &mut BodyEventInput, complete: bool, warning: Option<&str>) {
    let metadata = if input.metadata.get("capture_metadata").is_some() {
        input
            .metadata
            .get_mut("capture_metadata")
            .expect("checked metadata")
    } else {
        &mut input.metadata
    };
    if !metadata.is_object() {
        *metadata = serde_json::json!({});
    }
    let object = metadata.as_object_mut().expect("object metadata");
    object.insert("transport_complete".into(), complete.into());
    if !complete {
        object.insert("capture_prefix_only".into(), true.into());
        object.insert("capture_stream_incomplete".into(), true.into());
    }
    if let Some(warning) = warning {
        object.insert("capture_warning".into(), warning.into());
    }
}

/// The live lease is the disk reservation. Every upload reader holds this Arc
/// across blocking I/O. Publication can start only after all those readers close.
pub(crate) struct PreparedUpload {
    ready: Option<ReadyCapture>,
    memory: Vec<MemoryPermit>,
    worker: CaptureWorker,
}
impl std::fmt::Debug for PreparedUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedUpload(protected)")
    }
}
impl Drop for PreparedUpload {
    fn drop(&mut self) {
        if let Some(ready) = self.ready.take() {
            self.worker
                .submit_ready_capture(ready, std::mem::take(&mut self.memory));
        }
    }
}

struct UploadReader {
    // Declaration order closes the descriptor before releasing its lease Arc.
    file: Option<std::fs::File>,
    upload: Arc<PreparedUpload>,
    remaining: u64,
}
struct ChargedChunk {
    bytes: Vec<u8>,
    _memory: MemoryPermit,
}
impl AsRef<[u8]> for ChargedChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl PreparedUpload {
    pub(crate) fn body(self: &Arc<Self>) -> reqwest::Body {
        let remaining = self
            .ready
            .as_ref()
            .expect("live ready capture")
            .body_bytes();
        let state = UploadReader {
            file: None,
            upload: self.clone(),
            remaining,
        };
        let stream = futures::stream::unfold(Some(state), |state| async move {
            let mut state = state?;
            if state.remaining == 0 {
                return None;
            }
            let memory = loop {
                if let Some(memory) = reserve_body_memory(CHUNK_BYTES) {
                    break memory;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            };
            let result = bounded_file_io(move || {
                let read = (|| -> std::io::Result<Bytes> {
                    if state.file.is_none() {
                        state.file = Some(
                            state
                                .upload
                                .ready
                                .as_ref()
                                .expect("live ready capture")
                                .open_body_reader()
                                .map_err(|error| std::io::Error::other(error.to_string()))?,
                        );
                    }
                    let count = state.remaining.min(CHUNK_BYTES as u64) as usize;
                    let mut bytes = vec![0u8; count];
                    state
                        .file
                        .as_mut()
                        .expect("opened reader")
                        .read_exact(&mut bytes)?;
                    state.remaining -= count as u64;
                    Ok(Bytes::from_owner(ChargedChunk {
                        bytes,
                        _memory: memory,
                    }))
                })();
                let next = if read.is_ok() && state.remaining != 0 {
                    Some(state)
                } else {
                    None
                };
                (read, next)
            })
            .await;
            match result {
                Ok((bytes, next)) => Some((bytes, next)),
                Err(reason) => Some((Err(std::io::Error::other(reason)), None)),
            }
        });
        reqwest::Body::wrap_stream(stream)
    }
}

/// Capture an authorized, known-length large request before its upstream call.
/// A failure refuses before upstream side effects and preserves any actual prefix.
pub(crate) async fn capture_request_body<S>(
    worker: CaptureWorker,
    mut input: BodyEventInput,
    stream: &mut S,
    body_bytes: usize,
    policy: Option<&str>,
) -> Result<Arc<PreparedUpload>, CaptureBodyFailure>
where
    S: AsyncRead + Unpin,
{
    if let Some(object) = input.metadata.as_object_mut() {
        object.insert("expected_body_bytes".into(), (body_bytes as u64).into());
        object.insert("model_observation".into(), "omitted_large_body".into());
    }
    let capture = match HttpCapture::file_backed(worker.body_logger(), input.clone()).await {
        Ok(capture) => capture,
        Err(reason) => {
            transport_metadata(&mut input, false, Some(reason));
            worker.submit_authorized_payload(
                input,
                CapturePayload::Gap(BodyCaptureGap {
                    reason: reason.into(),
                    body_sha256: format!("{:x}", Sha256::digest([])),
                    body_bytes: 0,
                }),
                policy,
            );
            return Err(CaptureBodyFailure::Admission(reason));
        }
    };
    let mut remaining = body_bytes;
    let mut buffer = [0u8; CHUNK_BYTES];
    while remaining != 0 {
        let wanted = remaining.min(buffer.len());
        match stream.read(&mut buffer[..wanted]).await {
            Ok(0) => {
                transport_metadata(&mut input, false, Some("client_body_incomplete"));
                capture.close();
                worker.submit_authorized_payload(input, CapturePayload::Http(capture), policy);
                return Err(CaptureBodyFailure::Client(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            Ok(count) => {
                if let Err(reason) = capture
                    .observe_file_chunk(Bytes::copy_from_slice(&buffer[..count]))
                    .await
                {
                    transport_metadata(&mut input, false, Some(reason));
                    capture.close();
                    worker.submit_authorized_payload(input, CapturePayload::Http(capture), policy);
                    return Err(CaptureBodyFailure::Storage(reason));
                }
                remaining -= count;
            }
            Err(error) => {
                transport_metadata(&mut input, false, Some("client_body_incomplete"));
                capture.close();
                worker.submit_authorized_payload(input, CapturePayload::Http(capture), policy);
                return Err(CaptureBodyFailure::Client(error));
            }
        }
    }
    capture.mark_transport_eof().await;
    transport_metadata(&mut input, true, None);
    match capture.finish(input, None).await {
        FinishedCapture::Ready(ready, memory) => Ok(Arc::new(PreparedUpload {
            ready: Some(ready),
            memory,
            worker,
        })),
        finished => {
            worker.submit_finished_capture(finished);
            Err(CaptureBodyFailure::Storage("capture_spill_finalize_failed"))
        }
    }
}

pub(crate) struct ResponseCaptureContext {
    pub worker: CaptureWorker,
    pub input: BodyEventInput,
    pub policy: Option<String>,
    pub allow_payload: bool,
}

pub(crate) struct ResponseCapture {
    worker: CaptureWorker,
    input: Option<BodyEventInput>,
    policy: Option<String>,
    capture: Option<HttpCapture>,
    complete: bool,
}
impl ResponseCaptureContext {
    pub(crate) fn start(mut self, status: u16, content_type: Option<String>) -> ResponseCapture {
        self.input.status = Some(status);
        self.input.content_type = content_type;
        transport_metadata(&mut self.input, false, None);
        let capture = if self.allow_payload {
            HttpCapture::new(self.worker.body_logger(), self.input.clone())
        } else {
            HttpCapture::metadata_only(self.worker.body_logger(), self.input.clone())
        };
        ResponseCapture {
            worker: self.worker,
            input: Some(self.input),
            policy: self.policy,
            capture: Some(capture),
            complete: false,
        }
    }
}
impl ResponseCapture {
    pub(crate) fn tee<S, E>(
        &self,
        stream: S,
    ) -> futures::stream::BoxStream<'static, Result<Bytes, E>>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: Send + 'static,
    {
        HttpCapture::tee(
            stream,
            self.capture
                .as_ref()
                .expect("live response capture")
                .clone(),
        )
    }
    pub(crate) fn complete(&mut self) {
        self.complete = true;
    }
}
impl Drop for ResponseCapture {
    fn drop(&mut self) {
        if let (Some(mut input), Some(capture)) = (self.input.take(), self.capture.take()) {
            transport_metadata(
                &mut input,
                self.complete,
                (!self.complete).then_some("forward_proxy_stream_incomplete"),
            );
            if capture.close() {
                self.worker.submit_authorized_payload(
                    input,
                    CapturePayload::Http(capture),
                    self.policy.as_deref(),
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "forward_proxy_capture_tests.rs"]
mod tests;
