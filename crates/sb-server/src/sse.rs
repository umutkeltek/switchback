use std::collections::VecDeque;
use std::convert::Infallible;

use futures::StreamExt;
use sb_adapter::EventStream;
use sb_core::AiStreamEvent;

/// An OpenAI-compatible SSE error frame, emitted mid-stream so a
/// truncated-by-error response is visible to the client.
pub(crate) fn openai_error_frame(message: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({"error": {"message": message, "type": "upstream_error"}})
    )
}

/// A Responses API SSE error frame.
pub(crate) fn responses_error_frame(message: &str) -> String {
    format!(
        "event: response.failed\ndata: {}\n\n",
        serde_json::json!({"type":"response.failed","response":{"status":"failed","error":{"message":message}}})
    )
}

/// An Anthropic SSE error frame, surfaced mid-stream so a failure never
/// masquerades as a clean completion.
pub(crate) fn anthropic_error_frame(message: &str) -> String {
    format!(
        "event: error\ndata: {}\n\n",
        serde_json::json!({"type":"error","error":{"type":"api_error","message":message}})
    )
}

/// Render a canonical event stream as an SSE body in a wire format. `encode`
/// maps each event to frames; `error_frame` surfaces a mid-stream failure
/// (never swallowed); `done` is the optional terminator.
pub(crate) fn body<F, G>(
    stream: EventStream,
    encode: F,
    error_frame: G,
    done: Option<String>,
) -> axum::body::Body
where
    F: FnMut(&AiStreamEvent) -> Vec<String> + Send + 'static,
    G: Fn(&str) -> String + Send + 'static,
{
    let sse = futures::stream::unfold(
        (
            stream,
            encode,
            error_frame,
            VecDeque::<String>::new(),
            done,
            false,
            false,
        ),
        |(mut stream, mut encode, error_frame, mut pending, done, mut done_sent, mut finished)| async move {
            loop {
                if let Some(frame) = pending.pop_front() {
                    return Some((
                        Ok::<String, Infallible>(frame),
                        (
                            stream,
                            encode,
                            error_frame,
                            pending,
                            done,
                            done_sent,
                            finished,
                        ),
                    ));
                }
                if finished {
                    if !done_sent {
                        done_sent = true;
                        if let Some(frame) = done.clone() {
                            return Some((
                                Ok(frame),
                                (
                                    stream,
                                    encode,
                                    error_frame,
                                    pending,
                                    done,
                                    done_sent,
                                    finished,
                                ),
                            ));
                        }
                    }
                    return None;
                }
                match stream.next().await {
                    Some(Ok(AiStreamEvent::Error { message, .. })) => {
                        pending.push_back(error_frame(&message));
                        finished = true;
                    }
                    Some(Ok(event)) => pending.extend(encode(&event)),
                    Some(Err(error)) => {
                        pending.push_back(error_frame(&error.message));
                        finished = true;
                    }
                    None => finished = true,
                }
            }
        },
    );
    axum::body::Body::from_stream(sse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_error_frame_is_visible_and_well_formed() {
        let frame = openai_error_frame("upstream exploded mid-stream");

        assert!(frame.starts_with("data: "));
        assert!(frame.ends_with("\n\n"));
        let json: serde_json::Value =
            serde_json::from_str(frame.trim_start_matches("data: ").trim()).unwrap();
        assert_eq!(json["error"]["type"], "upstream_error");
        assert_eq!(json["error"]["message"], "upstream exploded mid-stream");
    }
}
