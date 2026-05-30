use futures::StreamExt;
use sb_adapter::{AdapterError, EventStream, PreparedRequest, ProviderAdapter};
use sb_core::{AiStreamEvent, CapabilityProfile, ErrorClass, Usage};

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

    chunks.into_iter().filter(|chunk| !chunk.is_empty()).collect()
}

#[async_trait::async_trait]
impl ProviderAdapter for MockAdapter {
    fn id(&self) -> &str {
        "mock"
    }

    fn capabilities(&self, _model: &str) -> CapabilityProfile {
        CapabilityProfile::basic_text()
    }

    async fn execute(&self, prepared: PreparedRequest) -> Result<EventStream, AdapterError> {
        if prepared
            .lease
            .as_ref()
            .map(|lease| lease.provider_account_id.as_str())
            == Some("fail-account")
        {
            return Err(
                AdapterError::new(ErrorClass::RateLimited, "mock: simulated account failure")
                    .with_status(429),
            );
        }

        let echo = format!("echo: {}", prepared.request.last_user_text().unwrap_or_default());
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
        let prepared = PreparedRequest::new(req, target, Some(CredentialLease::none("fail-account")));

        let error = match MockAdapter.execute(prepared).await {
            Ok(_) => panic!("expected mock adapter to simulate a rate-limited account failure"),
            Err(error) => error,
        };

        assert_eq!(error.class, ErrorClass::RateLimited);
    }
}
