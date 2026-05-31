use std::time::Instant;

use futures::StreamExt;
use sb_adapter::EventStream;
use sb_core::{AiStreamEvent, ErrorClass, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamFinish {
    Clean,
    UpstreamError(ErrorClass),
    Aborted,
}

/// Holds the stream finalizer. A clean finish or upstream error fires it and
/// DISARMS the guard; if the guard reaches `Drop` still armed, the stream was
/// dropped mid-flight (the client hung up) and it fires as `Aborted`.
struct FinishGuard<F: FnOnce(Usage, StreamFinish)> {
    usage: Usage,
    on_finish: Option<F>,
}

impl<F: FnOnce(Usage, StreamFinish)> FinishGuard<F> {
    /// Clean finish: fire and disarm.
    fn complete(&mut self) {
        if let Some(finish) = self.on_finish.take() {
            finish(std::mem::take(&mut self.usage), StreamFinish::Clean);
        }
    }

    /// Upstream stream error: fire and disarm before yielding the error.
    fn error(&mut self, class: ErrorClass) {
        if let Some(finish) = self.on_finish.take() {
            finish(
                std::mem::take(&mut self.usage),
                StreamFinish::UpstreamError(class),
            );
        }
    }
}

impl<F: FnOnce(Usage, StreamFinish)> Drop for FinishGuard<F> {
    fn drop(&mut self) {
        // Still armed at drop -> the stream never reached a terminal state.
        if let Some(finish) = self.on_finish.take() {
            finish(std::mem::take(&mut self.usage), StreamFinish::Aborted);
        }
    }
}

/// Wrap a streamed response so: (1) `on_first` fires with the elapsed ms when the
/// FIRST event arrives (time-to-first-token), and (2) `on_finish(usage, outcome)`
/// runs exactly once when the stream ends cleanly, yields an upstream error, or
/// is dropped before completion. `on_first` simply never fires if the client
/// drops before the first event.
pub(crate) fn meter_stream<G, F>(
    stream: EventStream,
    started: Instant,
    on_first: G,
    on_finish: F,
) -> EventStream
where
    G: FnOnce(f64) + Send + 'static,
    F: FnOnce(Usage, StreamFinish) + Send + 'static,
{
    let guard = FinishGuard {
        usage: Usage::default(),
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
                        guard.usage = latest.clone();
                    }
                    if let Err(error) = &item {
                        guard.error(error.class);
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

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
            move |_usage, finish| {
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
            move |_usage, finish| {
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
            move |_usage, finish| {
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
}
