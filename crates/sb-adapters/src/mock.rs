use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream, PreparedRequest, ProviderAdapter};
use sb_core::{AiStreamEvent, CapabilityProfile, ContentPart, ErrorClass, Usage};

pub struct MockAdapter;

fn split_text_chunks(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let total_chars = text.chars().count();
    if total_chars < 6 {
        return vec![text.to_string()];
    }

    let mut cuts = Vec::new();
    for target in [total_chars / 3, (total_chars * 2) / 3] {
        if target == 0 || target >= total_chars {
            continue;
        }

        if let Some((idx, _)) = text.char_indices().nth(target) {
            if !cuts.contains(&idx) {
                cuts.push(idx);
            }
        }
    }

    cuts.sort_unstable();

    let mut start = 0usize;
    let mut chunks = Vec::new();
    for cut in cuts {
        if cut > start {
            chunks.push(text[start..cut].to_string());
            start = cut;
        }
    }
    if start < text.len() {
        chunks.push(text[start..].to_string());
    }

    chunks
        .into_iter()
        .filter(|chunk| !chunk.is_empty())
        .collect()
}

#[async_trait::async_trait]
impl ProviderAdapter for MockAdapter {
    fn id(&self) -> &str {
        "mock"
    }

    fn capabilities(&self, _model: &str) -> CapabilityProfile {
        // Mock is the universal test stand-in — it accepts any request so a test
        // can route anything to it. Simulate a *limited* provider via a catalog
        // entry, not by restricting the mock.
        CapabilityProfile {
            vision_in: true,
            parallel_tool_calls: true,
            json_schema: true,
            ..CapabilityProfile::default()
        }
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        if prepared
            .lease
            .as_ref()
            .map(|lease| lease.provider_account_id.as_str())
            == Some("fail-account")
        {
            return Err(AdapterError::new(
                ErrorClass::RateLimited,
                "mock: simulated account failure",
            )
            .with_status(429));
        }

        if prepared
            .lease
            .as_ref()
            .map(|lease| lease.provider_account_id.as_str())
            == Some("stream-fail-account")
        {
            return Ok(futures::stream::iter(vec![Err(AdapterError::new(
                ErrorClass::StreamInterrupted,
                "mock: simulated pre-commit stream failure",
            ))])
            .boxed());
        }

        if prepared
            .lease
            .as_ref()
            .map(|lease| lease.provider_account_id.as_str())
            == Some("mid-stream-fail-account")
        {
            // Unlike `stream-fail-account`, the first event succeeds (so
            // `precommit_stream` commits this stream to the client) and the
            // failure only arrives afterward, exercising
            // `StreamFinish::UpstreamError` rather than a precommit failure.
            return Ok(futures::stream::iter(vec![
                Ok(AiStreamEvent::MessageStart {
                    id: prepared.request.id.clone(),
                    model: prepared.target.model.clone(),
                }),
                Err(AdapterError::new(
                    ErrorClass::ServerError,
                    "mock: simulated mid-stream upstream error",
                )),
            ])
            .boxed());
        }

        // Deterministic MODEL-keyed fixtures (outcome-routing-v1 §8 live
        // smoke): unlike the account-keyed fixtures above, these key off
        // `target.model` so two DISTINCT targets on the same (or different)
        // provider(s) can be given stable, always-reproducing outcomes
        // regardless of which account resolved the lease — needed to build a
        // route group of targets with different scorecard histories.
        if prepared.target.model == "always-error" {
            // A precommit failure (returned before any stream is built), same
            // as the account-keyed fixtures above: legal to fall over to the
            // next account/target for both streaming and non-streaming
            // requests. Classifies as `OutcomeClass::TargetFailure` (scoreable).
            //
            // `ProviderOverloaded` (not `ServerError`): both are equally
            // scoreable TargetFailure for the scorecard, but the credential
            // layer's per-(account,model) cooldown (`sb-credentials::
            // availability::cooldown_for`) locks a failing account for a
            // fixed 30s on `ServerError` vs. a short exponential backoff
            // starting at 2s on `ProviderOverloaded`/`RateLimited` — needed
            // so a scorecard live-smoke test can observe the account pool
            // recover (and the router's tiered-demotion rank fall through to
            // the scorecard) without a 30s+ real sleep.
            return Err(AdapterError::new(
                ErrorClass::ProviderOverloaded,
                "mock: simulated deterministic target failure",
            )
            .with_status(503));
        }

        if prepared.target.model == "quality-judge" {
            // Live-quality smoke fixture. A valid rubric response is possible
            // only when BOTH recursion guards made it through the ordinary
            // canonical execution path. Material is borrowed only to select a
            // deterministic score; it is never logged or retained.
            let guarded = prepared
                .request
                .metadata
                .get("task_type")
                .map(String::as_str)
                == Some("judge")
                && prepared
                    .request
                    .metadata
                    .get("internal_origin")
                    .map(String::as_str)
                    == Some("quality_eval");
            let material = prepared
                .request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                });
            let mut quality_pass = false;
            let mut quality_fail = false;
            for text in material {
                quality_pass |= text.contains("QUALITY_PASS");
                quality_fail |= text.contains("QUALITY_FAIL");
            }
            let judgment = if !guarded {
                "invalid"
            } else if quality_fail {
                r#"{"gradable":true,"score":0,"reason_code":"incorrect"}"#
            } else if quality_pass {
                r#"{"gradable":true,"score":4,"reason_code":"pass"}"#
            } else {
                r#"{"gradable":true,"score":3,"reason_code":"pass"}"#
            };
            let events = vec![
                Ok(AiStreamEvent::MessageStart {
                    id: prepared.request.id.clone(),
                    model: prepared.target.model.clone(),
                }),
                Ok(AiStreamEvent::TextDelta {
                    text: judgment.to_string(),
                }),
                Ok(AiStreamEvent::UsageDelta {
                    usage: Usage {
                        input_tokens: 8,
                        output_tokens: 8,
                        ..Usage::default()
                    },
                }),
                Ok(AiStreamEvent::MessageEnd {
                    finish_reason: sb_core::FinishReason::Stop,
                }),
            ];
            return Ok(futures::stream::iter(events).boxed());
        }

        if prepared.target.model == "hedge-fast" {
            // outcome-routing-v1 F6: a small but REAL async yield (not an
            // instant synchronous return) so tokio's executor actually
            // interleaves both hedge racers' futures -- without this, a
            // zero-latency mock response can resolve the winner on its very
            // first poll before the loser's future is ever polled at all,
            // which would never exercise the started-but-canceled path this
            // fixture pair exists to test.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let echo = format!(
                "echo: {}",
                prepared.request.last_user_text().unwrap_or_default()
            );
            let mut events = vec![Ok(AiStreamEvent::MessageStart {
                id: prepared.request.id.clone(),
                model: prepared.target.model.clone(),
            })];
            for chunk in split_text_chunks(&echo) {
                events.push(Ok(AiStreamEvent::TextDelta { text: chunk }));
            }
            events.push(Ok(AiStreamEvent::UsageDelta {
                usage: Usage {
                    input_tokens: 8,
                    output_tokens: 8,
                    ..Usage::default()
                },
            }));
            events.push(Ok(AiStreamEvent::MessageEnd {
                finish_reason: sb_core::FinishReason::Stop,
            }));
            return Ok(futures::stream::iter(events).boxed());
        }

        if prepared.target.model == "hedge-slow" {
            // outcome-routing-v1 F6: deliberately slow (but otherwise
            // healthy) so a hedge race's OTHER (fast) candidate wins first
            // -- this one is still in-flight when `run_hedge` returns,
            // exercising the started-but-canceled racer path.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let echo = format!(
                "echo: {}",
                prepared.request.last_user_text().unwrap_or_default()
            );
            let mut events = vec![Ok(AiStreamEvent::MessageStart {
                id: prepared.request.id.clone(),
                model: prepared.target.model.clone(),
            })];
            for chunk in split_text_chunks(&echo) {
                events.push(Ok(AiStreamEvent::TextDelta { text: chunk }));
            }
            events.push(Ok(AiStreamEvent::UsageDelta {
                usage: Usage {
                    input_tokens: 8,
                    output_tokens: 8,
                    ..Usage::default()
                },
            }));
            events.push(Ok(AiStreamEvent::MessageEnd {
                finish_reason: sb_core::FinishReason::Stop,
            }));
            return Ok(futures::stream::iter(events).boxed());
        }

        if prepared
            .lease
            .as_ref()
            .map(|lease| lease.provider_account_id.as_str())
            == Some("retry-fail-then-succeed-account")
        {
            // outcome-routing-v1 F5: fails with a retryable error class the
            // first two times it's dispatched (same account, same lease --
            // exercising execute.rs's same-target retry loop), then
            // succeeds on the third dispatch. A static counter is fine
            // here: this account id is unique to one test.
            static ATTEMPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let attempt = ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt < 2 {
                return Err(AdapterError::new(
                    ErrorClass::Timeout,
                    "mock: simulated transient timeout (will succeed on retry)",
                ));
            }
            let echo = format!(
                "echo: {}",
                prepared.request.last_user_text().unwrap_or_default()
            );
            let mut events = vec![Ok(AiStreamEvent::MessageStart {
                id: prepared.request.id.clone(),
                model: prepared.target.model.clone(),
            })];
            for chunk in split_text_chunks(&echo) {
                events.push(Ok(AiStreamEvent::TextDelta { text: chunk }));
            }
            events.push(Ok(AiStreamEvent::UsageDelta {
                usage: Usage {
                    input_tokens: 8,
                    output_tokens: 8,
                    ..Usage::default()
                },
            }));
            events.push(Ok(AiStreamEvent::MessageEnd {
                finish_reason: sb_core::FinishReason::Stop,
            }));
            return Ok(futures::stream::iter(events).boxed());
        }

        if prepared
            .lease
            .as_ref()
            .map(|lease| lease.provider_account_id.as_str())
            == Some("mid-stream-inband-error-account")
        {
            // outcome-routing-v1 F9: first event succeeds (precommit
            // commits this stream to the client), but the upstream failure
            // arrives as an in-band `Ok(AiStreamEvent::Error)` item rather
            // than a transport-level `Err` -- exercises meter_stream's
            // detection of in-band errors post-commit.
            return Ok(futures::stream::iter(vec![
                Ok(AiStreamEvent::MessageStart {
                    id: prepared.request.id.clone(),
                    model: prepared.target.model.clone(),
                }),
                Ok(AiStreamEvent::TextDelta {
                    text: "partial".to_string(),
                }),
                Ok(AiStreamEvent::Error {
                    message: "mock: simulated in-band upstream error".to_string(),
                    class: ErrorClass::ServerError,
                }),
            ])
            .boxed());
        }

        if prepared.target.model == "always-truncated" {
            // Always succeeds, but with `FinishReason::Length` instead of
            // `Stop` — classifies as `OutcomeClass::Truncated` (scoreable, but
            // not a failure): a real response, not an error.
            let echo = format!(
                "echo: {}",
                prepared.request.last_user_text().unwrap_or_default()
            );
            let mut events = vec![Ok(AiStreamEvent::MessageStart {
                id: prepared.request.id.clone(),
                model: prepared.target.model.clone(),
            })];
            for chunk in split_text_chunks(&echo) {
                events.push(Ok(AiStreamEvent::TextDelta { text: chunk }));
            }
            events.push(Ok(AiStreamEvent::UsageDelta {
                usage: Usage {
                    input_tokens: 8,
                    output_tokens: 8,
                    ..Usage::default()
                },
            }));
            events.push(Ok(AiStreamEvent::MessageEnd {
                finish_reason: sb_core::FinishReason::Length,
            }));
            return Ok(futures::stream::iter(events).boxed());
        }

        let echo = format!(
            "echo: {}",
            prepared.request.last_user_text().unwrap_or_default()
        );
        let mut events = vec![Ok(AiStreamEvent::MessageStart {
            id: prepared.request.id.clone(),
            model: prepared.target.model.clone(),
        })];

        for chunk in split_text_chunks(&echo) {
            events.push(Ok(AiStreamEvent::TextDelta { text: chunk }));
        }

        events.push(Ok(AiStreamEvent::UsageDelta {
            usage: Usage {
                input_tokens: 8,
                output_tokens: 8,
                ..Usage::default()
            },
        }));
        events.push(Ok(AiStreamEvent::MessageEnd {
            finish_reason: sb_core::FinishReason::Stop,
        }));

        Ok(futures::stream::iter(events).boxed())
    }

    async fn embeddings(
        &self,
        body: serde_json::Value,
        target: sb_core::ExecutionTarget,
        _lease: Option<sb_core::CredentialLease>,
        _egress_id: Option<String>,
    ) -> Result<serde_json::Value, AdapterError> {
        let inputs = match body.get("input") {
            Some(serde_json::Value::String(input)) => vec![input.clone()],
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .filter_map(|value| value.as_str().map(ToOwned::to_owned))
                .collect(),
            _ => Vec::new(),
        };
        let token_count = inputs.len();
        let data = inputs
            .iter()
            .enumerate()
            .map(|(index, _)| {
                serde_json::json!({
                    "object": "embedding",
                    "index": index,
                    "embedding": [0.1, 0.2, 0.3, 0.4]
                })
            })
            .collect::<Vec<_>>();

        Ok(serde_json::json!({
            "object": "list",
            "data": data,
            "model": target.model,
            "usage": {
                "prompt_tokens": token_count,
                "total_tokens": token_count
            }
        }))
    }

    async fn list_models(
        &self,
        _lease: Option<sb_core::CredentialLease>,
        _egress_id: Option<String>,
    ) -> Result<Vec<String>, AdapterError> {
        Ok(vec!["echo".to_string(), "embed".to_string()])
    }

    fn classify_error(&self, _status: Option<u16>, _body: &str) -> ErrorClass {
        ErrorClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use sb_core::{AiRequest, CredentialLease, ExecutionTarget, ExecutionTargetKind, Message};

    #[tokio::test]
    async fn mock_echoes_last_user_text() {
        let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
        let target = ExecutionTarget::new("mock", "echo", ExecutionTargetKind::ModelApi);
        let prepared = PreparedRequest::new(req, target, None);

        let mut stream = MockAdapter.execute(prepared).await.unwrap();
        let mut text = String::new();

        while let Some(item) = stream.next().await {
            if let Ok(AiStreamEvent::TextDelta { text: delta }) = item {
                text.push_str(&delta);
            }
        }

        assert!(text.contains("echo: hi"));
    }

    #[tokio::test]
    async fn mock_fail_account_returns_rate_limited_error() {
        let req = AiRequest::new("mock/echo", vec![Message::user("hi")]);
        let target = ExecutionTarget::new("mock", "echo", ExecutionTargetKind::ModelApi);
        let prepared =
            PreparedRequest::new(req, target, Some(CredentialLease::none("fail-account")));

        let error = match MockAdapter.execute(prepared).await {
            Ok(_) => panic!("expected mock adapter to simulate a rate-limited account failure"),
            Err(error) => error,
        };

        assert_eq!(error.class, ErrorClass::RateLimited);
    }

    #[tokio::test]
    async fn mock_always_error_model_fails_deterministically_regardless_of_account() {
        let req = AiRequest::new("mock/always-error", vec![Message::user("hi")]);
        let target = ExecutionTarget::new("mock", "always-error", ExecutionTargetKind::ModelApi);
        let prepared = PreparedRequest::new(req, target, None);

        let error = match MockAdapter.execute(prepared).await {
            Ok(_) => panic!("expected the always-error model to fail deterministically"),
            Err(error) => error,
        };

        assert_eq!(error.class, ErrorClass::ProviderOverloaded);
    }

    #[tokio::test]
    async fn mock_quality_judge_requires_both_recursion_guards() {
        let target = ExecutionTarget::new("mock", "quality-judge", ExecutionTargetKind::ModelApi);
        let mut guarded = AiRequest::new("mock/quality-judge", vec![Message::user("QUALITY_PASS")]);
        guarded.metadata.insert("task_type".into(), "judge".into());
        guarded
            .metadata
            .insert("internal_origin".into(), "quality_eval".into());
        let mut stream = MockAdapter
            .execute(PreparedRequest::new(guarded, target.clone(), None))
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let Ok(AiStreamEvent::TextDelta { text: delta }) = item {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, r#"{"gradable":true,"score":4,"reason_code":"pass"}"#);

        let unguarded = AiRequest::new("mock/quality-judge", vec![Message::user("QUALITY_PASS")]);
        let mut stream = MockAdapter
            .execute(PreparedRequest::new(unguarded, target, None))
            .await
            .unwrap();
        let mut text = String::new();
        while let Some(item) = stream.next().await {
            if let Ok(AiStreamEvent::TextDelta { text: delta }) = item {
                text.push_str(&delta);
            }
        }
        assert_eq!(text, "invalid");
    }

    #[tokio::test]
    async fn mock_always_truncated_model_finishes_with_length() {
        let req = AiRequest::new("mock/always-truncated", vec![Message::user("hi")]);
        let target =
            ExecutionTarget::new("mock", "always-truncated", ExecutionTargetKind::ModelApi);
        let prepared = PreparedRequest::new(req, target, None);

        let mut stream = MockAdapter.execute(prepared).await.unwrap();
        let mut finish_reason = None;
        while let Some(item) = stream.next().await {
            if let Ok(AiStreamEvent::MessageEnd { finish_reason: fr }) = item {
                finish_reason = Some(fr);
            }
        }

        assert_eq!(finish_reason, Some(sb_core::FinishReason::Length));
    }

    #[tokio::test]
    async fn mock_embeddings_supports_array_and_string_inputs() {
        let target = ExecutionTarget::new("mock", "embed", ExecutionTargetKind::ModelApi);

        let array_body = serde_json::json!({ "input": ["hello", "world"] });
        let array_response = MockAdapter
            .embeddings(array_body, target.clone(), None, None)
            .await
            .unwrap();
        let array_data = array_response["data"].as_array().unwrap();
        assert_eq!(array_data.len(), 2);
        for (index, entry) in array_data.iter().enumerate() {
            assert_eq!(entry["index"], serde_json::json!(index));
            assert!(!entry["embedding"].as_array().unwrap().is_empty());
        }

        let string_body = serde_json::json!({ "input": "hello" });
        let string_response = MockAdapter
            .embeddings(string_body, target, None, None)
            .await
            .unwrap();
        assert_eq!(string_response["data"].as_array().unwrap().len(), 1);
    }
}
