//! HTTP stream capture: bounded inline prefixes, then owned durable spill.
//! ModeD streams authorized large requests from the same durable spill owner.
//! Buffered fallback requests and WebSocket frames retain their existing limits.
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use axum::body::Bytes;
use futures::{Stream, StreamExt};
use sb_bodylog::{
    BodyCaptureGap, BodyEventInput, BodyLogger, CaptureMode, CaptureSpill, ReadyCapture,
    HTTP_SPILL_MAX_BODY_BYTES,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Semaphore};

const INLINE_BYTES: usize = 256 * 1024;
const BODY_MEMORY_BYTES: usize = 96 * 1024 * 1024;
const METADATA_RESERVATION: usize = 128 * 1024;
const IO_CHUNK_BYTES: usize = 64 * 1024;
static IO_TASKS: Semaphore = Semaphore::const_new(16);
static MEMORY: OnceLock<Arc<MemoryBudget>> = OnceLock::new();

/// Shared by capture writes and file-backed forward-proxy uploads. Ownership
/// stays inside the callback across cancellation, including the capture lease.
pub(crate) async fn bounded_file_io<T, F>(operation: F) -> Result<T, &'static str>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = IO_TASKS
        .acquire()
        .await
        .map_err(|_| "capture_io_unavailable")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
    .map_err(|_| "capture_io_task_failed")
}

pub(crate) fn reserve_body_memory(bytes: usize) -> Option<MemoryPermit> {
    MEMORY
        .get_or_init(|| {
            Arc::new(MemoryBudget {
                used: AtomicUsize::new(0),
                limit: BODY_MEMORY_BYTES,
            })
        })
        .reserve(bytes)
}

struct MemoryBudget {
    used: AtomicUsize,
    limit: usize,
}

#[derive(Clone)]
pub(crate) struct MemoryPermit(Arc<MemoryReservation>);
struct MemoryReservation {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}
impl std::fmt::Debug for MemoryPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MemoryPermit").field(&self.0.bytes).finish()
    }
}
impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
impl MemoryBudget {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<MemoryPermit> {
        let mut current = self.used.load(Ordering::Acquire);
        loop {
            let next = current.checked_add(bytes)?;
            if next > self.limit {
                return None;
            }
            match self.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(MemoryPermit(Arc::new(MemoryReservation {
                        budget: self.clone(),
                        bytes,
                    })))
                }
                Err(actual) => current = actual,
            }
        }
    }
}

enum Data {
    Inline {
        body: Vec<u8>,
        memory: Vec<MemoryPermit>,
    },
    Spill(Box<CaptureSpill>),
    Busy,
    Gap(&'static str),
}
struct State {
    logger: BodyLogger,
    seed: Option<BodyEventInput>,
    data: Data,
    hasher: Sha256,
    bytes: u64,
    eof: bool,
    metadata_memory: Option<MemoryPermit>,
    budget: Arc<MemoryBudget>,
}

#[derive(Clone)]
pub(crate) struct HttpCapture {
    state: Arc<Mutex<Option<State>>>,
    closed: Arc<AtomicBool>,
}
impl std::fmt::Debug for HttpCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HttpCapture(protected)")
    }
}
pub(crate) enum FinishedCapture {
    Inline(BodyEventInput, Vec<MemoryPermit>),
    Ready(ReadyCapture, Vec<MemoryPermit>),
    Gap(BodyEventInput, BodyCaptureGap, Vec<MemoryPermit>),
}

impl HttpCapture {
    pub(crate) fn new(logger: BodyLogger, seed: BodyEventInput) -> Self {
        let budget = MEMORY
            .get_or_init(|| {
                Arc::new(MemoryBudget {
                    used: AtomicUsize::new(0),
                    limit: BODY_MEMORY_BYTES,
                })
            })
            .clone();
        Self::with_budget(logger, seed, budget)
    }
    /// Reserve durable storage before consuming a large client body. Failure
    /// leaves the caller free to refuse before any upstream side effect.
    pub(crate) async fn file_backed(
        logger: BodyLogger,
        seed: BodyEventInput,
    ) -> Result<Self, &'static str> {
        let capture = Self::new(logger, seed);
        let mut guard = capture.state.lock().await;
        let state = guard.as_mut().ok_or("capture_state_unavailable")?;
        let memory = state
            .metadata_memory
            .clone()
            .ok_or("capture_memory_budget_exhausted")?;
        let logger = state.logger.clone();
        let seed = state.seed.take().ok_or("capture_seed_unavailable")?;
        state.data = Data::Busy;
        let (result, _memory) = bounded_file_io(move || {
            let result = (|| {
                if logger
                    .pressure_status()
                    .map_err(|_| "capture_pressure_unavailable")?
                    .mode
                    == CaptureMode::MetadataOnly
                {
                    return Err("pressure_metadata_only");
                }
                logger
                    .begin_capture_spill(seed)
                    .map_err(|_| "capture_spill_admission_failed")
            })();
            (result, memory)
        })
        .await?;
        state.data = Data::Spill(Box::new(result?));
        drop(guard);
        Ok(capture)
    }

    pub(crate) fn metadata_only(logger: BodyLogger, seed: BodyEventInput) -> Self {
        let capture = Self::new(logger, seed);
        // This handle has not escaped, so the async mutex cannot be held.
        if let Some(state) = capture
            .state
            .try_lock()
            .expect("new capture is unlocked")
            .as_mut()
        {
            state.data = Data::Gap("policy_metadata_only");
            state.seed = None;
        }
        capture
    }

    pub(crate) async fn observe_file_chunk(&self, bytes: Bytes) -> Result<(), &'static str> {
        if self.closed.load(Ordering::Acquire) {
            return Err("capture_stream_closed");
        }
        self.observe(bytes).await;
        let guard = self.state.lock().await;
        match guard.as_ref().map(|state| &state.data) {
            Some(Data::Spill(_)) => Ok(()),
            Some(Data::Gap(reason)) => Err(reason),
            _ => Err("capture_file_state_unavailable"),
        }
    }

    pub(crate) async fn mark_transport_eof(&self) {
        if let Some(state) = self.state.lock().await.as_mut() {
            state.eof = true;
        }
    }

    fn with_budget(logger: BodyLogger, seed: BodyEventInput, budget: Arc<MemoryBudget>) -> Self {
        let metadata_memory = budget.reserve(METADATA_RESERVATION);
        let (seed, data) = if metadata_memory.is_some() {
            (
                Some(seed),
                Data::Inline {
                    body: Vec::new(),
                    memory: Vec::new(),
                },
            )
        } else {
            (None, Data::Gap("capture_memory_budget_exhausted"))
        };
        Self {
            state: Arc::new(Mutex::new(Some(State {
                logger,
                seed,
                data,
                hasher: Sha256::new(),
                bytes: 0,
                eof: false,
                metadata_memory,
                budget,
            }))),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn tee<S, E>(
        inner: S,
        capture: Self,
    ) -> futures::stream::BoxStream<'static, Result<Bytes, E>>
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: Send + 'static,
    {
        futures::stream::unfold(
            (inner.boxed(), capture),
            |(mut inner, capture)| async move {
                match inner.next().await {
                    Some(Ok(bytes)) => {
                        capture.observe(bytes.clone()).await;
                        Some((Ok(bytes), (inner, capture)))
                    }
                    Some(Err(error)) => Some((Err(error), (inner, capture))),
                    None => {
                        if let Some(state) = capture.state.lock().await.as_mut() {
                            state.eof = true;
                        }
                        None
                    }
                }
            },
        )
        .boxed()
    }

    async fn observe(&self, bytes: Bytes) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut guard = self.state.lock().await;
        let Some(state) = guard.as_mut() else {
            return;
        };
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        // Bound work per scheduler turn even if a custom upstream emits one
        // unusually large frame. The original Bytes allocation is retained,
        // never copied into another whole-body buffer.
        for (index, chunk) in bytes.chunks(IO_CHUNK_BYTES).enumerate() {
            state.hasher.update(chunk);
            state.bytes = state.bytes.saturating_add(chunk.len() as u64);
            if index % 16 == 15 {
                tokio::task::yield_now().await;
            }
        }
        if state.bytes > HTTP_SPILL_MAX_BODY_BYTES {
            state.data = Data::Gap("body_limit_exceeded");
            return;
        }
        if matches!(state.data, Data::Gap(_)) {
            return;
        }
        let Some(chunk_memory) = state.budget.reserve(bytes.len()) else {
            state.data = Data::Gap("capture_memory_budget_exhausted");
            return;
        };
        let data = std::mem::replace(&mut state.data, Data::Busy);
        match data {
            Data::Inline {
                mut body,
                mut memory,
            } if body.len().saturating_add(bytes.len()) <= INLINE_BYTES => {
                let wanted = body.len() + bytes.len();
                let target = if wanted <= body.capacity() {
                    body.capacity()
                } else {
                    wanted
                        .max(body.capacity().saturating_mul(2))
                        .clamp(4096, INLINE_BYTES)
                };
                let extra = target.saturating_sub(body.capacity());
                if let Some(credit) = state.budget.reserve(extra.saturating_mul(3)) {
                    if extra != 0 {
                        memory.push(credit);
                    }
                    if body
                        .try_reserve_exact(target.saturating_sub(body.len()))
                        .is_err()
                    {
                        state.data = Data::Gap("capture_memory_allocation_failed");
                    } else {
                        body.extend_from_slice(&bytes);
                        state.data = Data::Inline { body, memory };
                    }
                } else {
                    state.data = Data::Gap("capture_memory_budget_exhausted");
                }
            }
            Data::Inline { body, memory } => {
                let logger = state.logger.clone();
                let seed = state.seed.take();
                let metadata_memory = state.metadata_memory.clone();
                let permit = IO_TASKS
                    .acquire()
                    .await
                    .expect("fixed capture I/O semaphore");
                let result = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let _body_memory = memory;
                    let _chunk_memory = chunk_memory;
                    let result = (|| -> Result<CaptureSpill, &'static str> {
                        if logger
                            .pressure_status()
                            .map_err(|_| "capture_pressure_unavailable")?
                            .mode
                            == CaptureMode::MetadataOnly
                        {
                            return Err("pressure_metadata_only");
                        }
                        let mut spill = logger
                            .begin_capture_spill(seed.ok_or("capture_seed_unavailable")?)
                            .map_err(|_| "capture_spill_admission_failed")?;
                        for chunk in body
                            .chunks(IO_CHUNK_BYTES)
                            .chain(bytes.chunks(IO_CHUNK_BYTES))
                        {
                            spill
                                .write_all(chunk)
                                .map_err(|_| "capture_spill_write_failed")?;
                        }
                        Ok(spill)
                    })();
                    (result, metadata_memory)
                })
                .await;
                state.data = match result {
                    Ok((Ok(spill), _metadata)) => Data::Spill(Box::new(spill)),
                    Ok((Err(reason), _metadata)) => Data::Gap(reason),
                    Err(_) => Data::Gap("capture_spill_task_failed"),
                };
            }
            Data::Spill(mut spill) => {
                let metadata_memory = state.metadata_memory.clone();
                let permit = IO_TASKS
                    .acquire()
                    .await
                    .expect("fixed capture I/O semaphore");
                let result = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let _chunk_memory = chunk_memory;
                    let result = bytes
                        .chunks(IO_CHUNK_BYTES)
                        .try_for_each(|chunk| spill.write_all(chunk));
                    (
                        result
                            .map(|_| spill)
                            .map_err(|_| "capture_spill_write_failed"),
                        metadata_memory,
                    )
                })
                .await;
                state.data = match result {
                    Ok((Ok(spill), _metadata)) => Data::Spill(spill),
                    Ok((Err(reason), _metadata)) => Data::Gap(reason),
                    Err(_) => Data::Gap("capture_spill_task_failed"),
                };
            }
            Data::Gap(reason) => state.data = Data::Gap(reason),
            Data::Busy => state.data = Data::Gap("capture_write_interrupted"),
        }
    }

    pub(crate) fn close(&self) -> bool {
        !self.closed.swap(true, Ordering::AcqRel)
    }

    pub(crate) async fn finish(
        self,
        mut input: BodyEventInput,
        policy_gap: Option<&'static str>,
    ) -> FinishedCapture {
        self.closed.store(true, Ordering::Release);
        let state = self.state.lock().await.take();
        let Some(mut state) = state else {
            return FinishedCapture::Gap(
                input,
                BodyCaptureGap {
                    reason: "capture_already_finalized".into(),
                    body_sha256: format!("{:x}", Sha256::digest([])),
                    body_bytes: 0,
                },
                Vec::new(),
            );
        };
        let metadata = if input.metadata.get("capture_metadata").is_some() {
            input
                .metadata
                .get_mut("capture_metadata")
                .expect("checked metadata")
        } else {
            &mut input.metadata
        };
        if let Some(object) = metadata.as_object_mut() {
            let complete = object
                .get("transport_complete")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true)
                && state.eof;
            object.insert("transport_complete".into(), complete.into());
            if !state.eof {
                object.insert("capture_stream_incomplete".into(), true.into());
            }
        }
        let mut metadata_memory = state.metadata_memory.take().into_iter().collect::<Vec<_>>();
        let gap = |reason: &'static str, input, hasher: Sha256, bytes, memory| {
            FinishedCapture::Gap(
                input,
                BodyCaptureGap {
                    reason: reason.into(),
                    body_sha256: format!("{:x}", hasher.finalize()),
                    body_bytes: bytes,
                },
                memory,
            )
        };
        if let Some(reason) = policy_gap {
            return gap(reason, input, state.hasher, state.bytes, metadata_memory);
        }
        match state.data {
            Data::Inline { body, mut memory } => {
                input.body = body;
                memory.append(&mut metadata_memory);
                FinishedCapture::Inline(input, memory)
            }
            Data::Spill(spill) => {
                let fallback = input.clone();
                let permit = IO_TASKS
                    .acquire()
                    .await
                    .expect("fixed capture I/O semaphore");
                let result = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let result = (*spill).finish(input);
                    (result, metadata_memory)
                })
                .await;
                match result {
                    Ok((Ok(ready), memory)) => FinishedCapture::Ready(ready, memory),
                    Ok((Err(_), memory)) => gap(
                        "capture_spill_finalize_failed",
                        fallback,
                        state.hasher,
                        state.bytes,
                        memory,
                    ),
                    Err(_) => gap(
                        "capture_spill_task_failed",
                        fallback,
                        state.hasher,
                        state.bytes,
                        Vec::new(),
                    ),
                }
            }
            Data::Gap(reason) => gap(reason, input, state.hasher, state.bytes, metadata_memory),
            Data::Busy => gap(
                "capture_write_interrupted",
                input,
                state.hasher,
                state.bytes,
                metadata_memory,
            ),
        }
    }
}

pub(crate) async fn blocking_io<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let permit = IO_TASKS
        .acquire()
        .await
        .expect("fixed capture I/O semaphore");
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_bodylog::{BodyLoggerConfig, CaptureStage};

    fn fixture(name: &str) -> (std::path::PathBuf, BodyLogger, BodyEventInput) {
        let root =
            std::env::temp_dir().join(format!("sb-http-credit-{}-{name}", std::process::id()));
        let logger = BodyLogger::new(BodyLoggerConfig {
            state_dir: root.join("state"),
            archive_root: root.join("archive"),
            legacy_jsonl: None,
            inline_threshold_bytes: INLINE_BYTES as u64,
        })
        .unwrap();
        let input = BodyEventInput {
            request_id: name.into(),
            capture_stage: CaptureStage::ClientInbound,
            protocol: "http".into(),
            upstream: Some("https://upstream.invalid".into()),
            model: None,
            status: None,
            content_type: None,
            body: Vec::new(),
            metadata: serde_json::json!({}),
        };
        (root, logger, input)
    }

    #[tokio::test]
    async fn shared_credits_cover_inline_spill_and_persistence_lifetimes() {
        let (root, logger, input) = fixture("lifetime");
        let budget = Arc::new(MemoryBudget {
            used: AtomicUsize::new(0),
            limit: 2 * 1024 * 1024,
        });
        let capture = HttpCapture::with_budget(logger.clone(), input.clone(), budget.clone());
        for _ in 0..80 {
            capture
                .observe(Bytes::from_static(&[b'x'; IO_CHUNK_BYTES]))
                .await;
            assert!(budget.used.load(Ordering::Acquire) <= budget.limit);
        }
        let finished = capture.finish(input, None).await;
        let FinishedCapture::Ready(ready, memory) = finished else {
            panic!("expected file-backed body");
        };
        assert_eq!(ready.body_bytes(), 80 * IO_CHUNK_BYTES as u64);
        assert_eq!(budget.used.load(Ordering::Acquire), METADATA_RESERVATION);
        let record = logger.record_capture_spill(&ready).unwrap();
        logger
            .read_record_body_to(&record, &mut std::io::sink())
            .unwrap();
        assert_eq!(
            budget.used.load(Ordering::Acquire),
            METADATA_RESERVATION,
            "metadata credits must survive persistence, including retries"
        );
        drop(memory);
        drop(ready);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        logger.seal_active().unwrap();
        drop(logger);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn exhausted_shared_credits_keep_gap_identity_without_allocating_a_body() {
        let (root, logger, input) = fixture("exhausted");
        let budget = Arc::new(MemoryBudget {
            used: AtomicUsize::new(0),
            limit: METADATA_RESERVATION,
        });
        let capture = HttpCapture::with_budget(logger.clone(), input.clone(), budget.clone());
        capture
            .observe(Bytes::from_static(b"unbuffered body"))
            .await;
        let FinishedCapture::Gap(input, gap, memory) = capture.finish(input, None).await else {
            panic!("budget exhaustion must be a typed gap");
        };
        assert!(input.body.is_empty());
        assert_eq!(gap.reason, "capture_memory_budget_exhausted");
        assert_eq!(gap.body_bytes, 15);
        assert_eq!(
            gap.body_sha256,
            format!("{:x}", Sha256::digest(b"unbuffered body"))
        );
        assert!(budget.used.load(Ordering::Acquire) <= budget.limit);
        drop(memory);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        drop(logger);
        std::fs::remove_dir_all(root).unwrap();
    }
}
