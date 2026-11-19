use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use futures::StreamExt;
use sb_adapter::EventStream;
use sb_core::{AiStreamEvent, ErrorClass, FinishReason, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamFinish {
    Clean,
    UpstreamError(ErrorClass),
    Aborted,
}

/// Holds the stream finalizer. A clean finish or upstream error fires it and
/// DISARMS the guard; if the guard reaches `Drop` still armed, the stream was
/// dropped mid-flight (the client hung up) and it fires as `Aborted`.
struct FinishGuard<F: FnOnce(Usage, Option<FinishReason>, StreamFinish)> {
    usage: Usage,
    /// The model's own finish reason, captured off `MessageEnd` as it passes
    /// through (outcome-routing-v1 §2 needs it to classify Success vs
    /// Truncated vs Refusal vs TargetFailure). `None` if the stream ends
    /// without ever emitting one.
    finish_reason: Option<FinishReason>,
    on_finish: Option<F>,
}

impl<F: FnOnce(Usage, Option<FinishReason>, StreamFinish)> FinishGuard<F> {
    /// Clean finish: fire and disarm.
    fn complete(&mut self) {
        if let Some(finish) = self.on_finish.take() {
            finish(
                std::mem::take(&mut self.usage),
                self.finish_reason.take(),
                StreamFinish::Clean,
            );
        }
    }

    /// Upstream stream error: fire and disarm before yielding the error.
    fn error(&mut self, class: ErrorClass) {
        if let Some(finish) = self.on_finish.take() {
            finish(
                std::mem::take(&mut self.usage),
                self.finish_reason.take(),
                StreamFinish::UpstreamError(class),
            );
        }
    }
}

impl<F: FnOnce(Usage, Option<FinishReason>, StreamFinish)> Drop for FinishGuard<F> {
    fn drop(&mut self) {
        // Still armed at drop -> the stream never reached a terminal state.
        if let Some(finish) = self.on_finish.take() {
            finish(
                std::mem::take(&mut self.usage),
                self.finish_reason.take(),
                StreamFinish::Aborted,
            );
        }
    }
}

/// Wrap a streamed response so: (1) `on_first` fires with the elapsed ms when the
/// FIRST event arrives (time-to-first-token), and (2) `on_finish(usage,
/// finish_reason, outcome)` runs exactly once when the stream ends cleanly,
/// yields an upstream error, or is dropped before completion. `on_first`
/// simply never fires if the client drops before the first event.
pub(crate) fn meter_stream<G, F>(
    stream: EventStream,
    started: Instant,
    on_first: G,
    on_finish: F,
) -> EventStream
where
    G: FnOnce(f64) + Send + 'static,
    F: FnOnce(Usage, Option<FinishReason>, StreamFinish) + Send + 'static,
{
    let guard = FinishGuard {
        usage: Usage::default(),
        finish_reason: None,
        on_finish: Some(on_finish),
    };
    futures::stream::unfold(
        (stream, guard, Some(on_first), started),
        |(mut stream, mut guard, mut on_first, started)| async move {
            match stream.next().await {
                Some(item) => {
                    if let Some(first) = on_first.take() {
                        first(started.elapsed().as_millis() as f64);
                    }
                    if let Ok(AiStreamEvent::UsageDelta { usage: latest }) = &item {
                        guard.usage.replace_with(latest.clone());
                    }
                    if let Ok(AiStreamEvent::EngineTimings { timings }) = &item {
                        guard.usage.engine_timings = Some(timings.clone());
                    }
                    if let Ok(AiStreamEvent::MessageEnd { finish_reason }) = &item {
                        guard.finish_reason = Some(*finish_reason);
                    }
                    // outcome-routing-v1 F9: an in-band `Ok(AiStreamEvent::
                    // Error{class})` is just as much an upstream failure as a
                    // transport-level `Err` -- fire the SAME guard path so it
                    // finalizes as `StreamFinish::UpstreamError(class)`
                    // rather than falling through to `Clean` (if the stream
                    // then ends naturally) or `Aborted` (if the client drops
                    // afterward).
                    match &item {
                        Err(error) => guard.error(error.class),
                        Ok(AiStreamEvent::Error { class, .. }) => guard.error(*class),
                        _ => {}
                    }
                    Some((item, (stream, guard, on_first, started)))
                }
                None => {
                    guard.complete();
                    None
                }
            }
        },
    )
    .boxed()
}

/// When the first token of a response arrived, and how many token events
/// passed, filled by [`watch_first_token`].
#[derive(Clone, Default)]
pub(crate) struct FirstToken(Arc<TokenClock>);

#[derive(Default)]
struct TokenClock {
    first: OnceLock<Instant>,
    events: AtomicU64,
}

impl FirstToken {
    /// When the first token arrived (`None` = no token yet).
    pub(crate) fn get(&self) -> Option<&Instant> {
        self.0.first.get()
    }

    /// Token events seen so far. A response that came in one piece (a
    /// non-streamed upstream body) has one; a streamed one has many.
    pub(crate) fn events(&self) -> u64 {
        self.0.events.load(Ordering::Relaxed)
    }
}

/// Note the moment the first token passes through the stream: the first text,
/// reasoning or tool-call event, or an output image. Framing events
/// (`MessageStart`, usage, finish) are not tokens, so a response that
/// produced none leaves the cell empty. Shared by the streaming and the
/// collected path, so both measure on the one event pipeline.
pub(crate) fn watch_first_token(stream: EventStream) -> (EventStream, FirstToken) {
    let first = FirstToken::default();
    let seen = first.clone();
    let stream = stream
        .inspect(move |item| {
            if matches!(item, Ok(event) if is_token(event)) {
                if seen.0.first.get().is_none() {
                    let _ = seen.0.first.set(Instant::now());
                }
                seen.0.events.fetch_add(1, Ordering::Relaxed);
            }
        })
        .boxed();
    (stream, first)
}

fn is_token(event: &AiStreamEvent) -> bool {
    match event {
        AiStreamEvent::TextDelta { text } | AiStreamEvent::ReasoningDelta { text } => {
            !text.is_empty()
        }
        AiStreamEvent::ToolCallStart(_)
        | AiStreamEvent::ToolCallArgsDelta { .. }
        | AiStreamEvent::OutputImage { .. } => true,
        _ => false,
    }
}

/// Shortest decode window, first token to end on the gateway's clock, that
/// gives a decode speed. Token events counts are not network chunks: a
/// body that came in one piece with reasoning and text yields two or more
/// token events microseconds apart, which would read as a huge speed.
const MIN_GATEWAY_DECODE_MS: u64 = 50;

/// How fast one attempt was served, for its usage record: time from the
/// attempt's dispatch to its first token and from the first token to
/// `ended`, on the gateway's clock, plus any engine timings in `usage`
/// (which win). The gateway's decode window counts only for a response
/// that streamed in (two or more token events over at least
/// [`MIN_GATEWAY_DECODE_MS`]): one that came in one piece has no decode
/// window to measure, so its decode speed is left out rather than made up.
pub(crate) fn attempt_speed(
    usage: &Usage,
    first: &FirstToken,
    attempt_started: Instant,
    ended: Instant,
) -> sb_ledger::RequestSpeed {
    let first_at = first.get().copied();
    let ttft = first_at.map(|at| at.saturating_duration_since(attempt_started).as_millis() as u64);
    let decode = first_at
        .filter(|_| first.events() >= 2)
        .map(|at| ended.saturating_duration_since(at).as_millis() as u64)
        .filter(|ms| *ms >= MIN_GATEWAY_DECODE_MS);
    sb_ledger::RequestSpeed::measure(usage, ttft, decode)
}

/// Time to first token for a trace, measured from the request's start (the
/// same origin as the trace's total latency). `None` when no token came.
pub(crate) fn ttft_ms(first: &FirstToken, request_started: Instant) -> Option<u64> {
    first
        .get()
        .map(|at| at.saturating_duration_since(request_started).as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sb_adapter::AdapterError;

    use super::*;

    fn channel_stream() -> (
        futures::channel::mpsc::UnboundedSender<Result<AiStreamEvent, AdapterError>>,
        EventStream,
    ) {
        let (tx, rx) = futures::channel::mpsc::unbounded();
        (tx, rx.boxed())
    }

    #[tokio::test]
    async fn meter_stream_records_a_clean_finish() {
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();
        let (tx, stream) = channel_stream();
        let mut metered = meter_stream(
            stream,
            Instant::now(),
            |_| {},
            move |_usage, _finish_reason, finish| {
                *sink.lock().unwrap() = Some(finish);
            },
        );
        tx.unbounded_send(Ok(AiStreamEvent::TextDelta { text: "hi".into() }))
            .unwrap();
        drop(tx); // close the channel -> the stream ends cleanly
        while metered.next().await.is_some() {}
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::Clean),
            "clean finish"
        );
    }

    #[tokio::test]
    async fn meter_stream_records_an_early_drop_as_aborted() {
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();
        let (tx, stream) = channel_stream();
        let mut metered = meter_stream(
            stream,
            Instant::now(),
            |_| {},
            move |_usage, _finish_reason, finish| {
                *sink.lock().unwrap() = Some(finish);
            },
        );
        tx.unbounded_send(Ok(AiStreamEvent::TextDelta { text: "hi".into() }))
            .unwrap();
        assert!(metered.next().await.is_some());
        // The client hangs up before the stream completes (tx kept alive). The
        // FinishGuard fires synchronously on drop with completed=false.
        drop(metered);
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::Aborted),
            "early drop = aborted"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn meter_stream_records_upstream_error_before_drop() {
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();
        let (tx, stream) = channel_stream();
        let mut metered = meter_stream(
            stream,
            Instant::now(),
            |_| {},
            move |_usage, _finish_reason, finish| {
                *sink.lock().unwrap() = Some(finish);
            },
        );
        tx.unbounded_send(Err(AdapterError::new(
            ErrorClass::StreamInterrupted,
            "broken stream",
        )))
        .unwrap();

        let item = metered.next().await.expect("error item");
        assert!(item.is_err());
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::UpstreamError(ErrorClass::StreamInterrupted)),
            "upstream stream errors are not client aborts"
        );
        drop(metered);
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::UpstreamError(ErrorClass::StreamInterrupted)),
            "drop after the error must not fire a second outcome"
        );
        drop(tx);
    }

    #[tokio::test]
    async fn meter_stream_records_an_in_band_error_as_upstream_error_not_cancelled() {
        // F9: a first-in-band-error case is handled at precommit
        // (collect::precommit_tests); THIS test covers the post-commit case
        // -- an ok chunk, then an in-band Ok(AiStreamEvent::Error{..}) --
        // which meter_stream itself must detect and finalize as
        // UpstreamError, not let fall through to Aborted/Clean.
        let outcome = Arc::new(Mutex::new(None));
        let sink = outcome.clone();
        let (tx, stream) = channel_stream();
        let mut metered = meter_stream(
            stream,
            Instant::now(),
            |_| {},
            move |_usage, _finish_reason, finish| {
                *sink.lock().unwrap() = Some(finish);
            },
        );
        tx.unbounded_send(Ok(AiStreamEvent::TextDelta { text: "hi".into() }))
            .unwrap();
        tx.unbounded_send(Ok(AiStreamEvent::Error {
            message: "mid-stream in-band failure".into(),
            class: ErrorClass::ServerError,
        }))
        .unwrap();
        drop(tx);

        while metered.next().await.is_some() {}
        assert_eq!(
            *outcome.lock().unwrap(),
            Some(StreamFinish::UpstreamError(ErrorClass::ServerError)),
            "in-band error must finalize as UpstreamError with its class, not Cancelled"
        );
    }

    // A body that came in one piece with reasoning and text gives two token
    // events microseconds apart: no gateway decode speed from that, while
    // a real decode window still gives one.
    #[tokio::test]
    async fn a_one_piece_body_with_two_token_events_has_no_decode_speed() {
        let (tx, stream) = channel_stream();
        let started = Instant::now();
        let (mut watched, first) = watch_first_token(stream);
        tx.unbounded_send(Ok(AiStreamEvent::ReasoningDelta {
            text: "think".into(),
        }))
        .unwrap();
        tx.unbounded_send(Ok(AiStreamEvent::TextDelta {
            text: "answer".into(),
        }))
        .unwrap();
        drop(tx);
        while watched.next().await.is_some() {}
        assert_eq!(first.events(), 2);
        let at = *first.get().unwrap();
        let usage = Usage {
            output_tokens: 500,
            ..Usage::default()
        };

        let speed = attempt_speed(&usage, &first, started, at + Duration::from_millis(2));
        assert_eq!((speed.decode_tps, speed.decode_tps_source), (None, None));

        let speed = attempt_speed(&usage, &first, started, at + Duration::from_millis(100));
        assert_eq!(speed.decode_tps, Some(5_000.0));
    }
}
