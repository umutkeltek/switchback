//! Wake-aware ordering ("prefer awake"): what one request may do with
//! targets whose machine first needs a wake. Decided from the route's
//! `prefer_awake` settings, the request's size, and its `x-switchback-wake`
//! header; the router applies the result (`RoutingPolicy::wake`).

use sb_core::{AiRequest, PreferAwakeConfig, WakePolicy, WAKE_METADATA_KEY};

/// What wake-aware ordering decided for one request.
pub(crate) struct WakeDecision {
    pub(crate) policy: WakePolicy,
    /// `max_tokens` is over `wake_for_output_tokens`: a `latency_aware`
    /// route ranks awake targets on decode speed.
    pub(crate) long_output: bool,
    /// A reason line to add, for a header value the gateway did not
    /// understand (it is then ignored).
    pub(crate) note: Option<String>,
}

/// Small request (estimated prompt under `wake_for_prompt_tokens`,
/// `max_tokens` not over `wake_for_output_tokens`): prefer awake targets.
/// Bigger, or header `always`: keep the strategy's order. Header `never`:
/// drop targets that need a wake.
pub(crate) fn decide(cfg: &PreferAwakeConfig, req: &AiRequest) -> WakeDecision {
    let max_tokens = req.max_output_tokens;
    let long_output = max_tokens.is_some_and(|tokens| tokens > cfg.wake_for_output_tokens);
    let header = req.metadata.get(WAKE_METADATA_KEY).map(String::as_str);
    let mut note = None;
    match header {
        Some("never") => {
            return WakeDecision {
                policy: WakePolicy::Never,
                long_output,
                note,
            }
        }
        Some("always") => {
            return WakeDecision {
                policy: WakePolicy::Keep {
                    why: "x-switchback-wake=always".to_string(),
                },
                long_output,
                note,
            }
        }
        None | Some("auto") => {}
        Some(other) => {
            note = Some(format!(
                "prefer_awake: x-switchback-wake={other} not understood (always, never or auto); ignored"
            ));
        }
    }
    let prompt = req.estimated_input_tokens();
    let policy = if prompt >= u64::from(cfg.wake_for_prompt_tokens) {
        WakePolicy::Keep {
            why: format!(
                "large prompt (~{prompt} tokens, wake_for_prompt_tokens {})",
                cfg.wake_for_prompt_tokens
            ),
        }
    } else if let Some(tokens) = max_tokens.filter(|_| long_output) {
        WakePolicy::Keep {
            why: format!(
                "long output (max_tokens {tokens}, wake_for_output_tokens {})",
                cfg.wake_for_output_tokens
            ),
        }
    } else {
        WakePolicy::PreferAwake
    };
    WakeDecision {
        policy,
        long_output,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::Message;

    fn cfg() -> PreferAwakeConfig {
        PreferAwakeConfig {
            enabled: true,
            wake_for_prompt_tokens: 100,
            wake_for_output_tokens: 50,
        }
    }

    fn req(prompt_chars: usize, max_tokens: Option<u32>, header: Option<&str>) -> AiRequest {
        let mut req = AiRequest::new("r", vec![Message::user("x".repeat(prompt_chars))]);
        req.max_output_tokens = max_tokens;
        if let Some(header) = header {
            req.metadata
                .insert(WAKE_METADATA_KEY.to_string(), header.to_string());
        }
        req
    }

    #[test]
    fn small_request_prefers_awake() {
        let decided = decide(&cfg(), &req(40, Some(50), None));
        assert_eq!(decided.policy, WakePolicy::PreferAwake);
        assert!(!decided.long_output, "max_tokens at the limit is not long");
        assert!(decided.note.is_none());
    }

    #[test]
    fn a_prompt_at_the_threshold_may_wake() {
        // 400 chars ~ 100 tokens: at the threshold, not under it.
        let decided = decide(&cfg(), &req(400, None, None));
        assert!(
            matches!(&decided.policy, WakePolicy::Keep { why } if why.starts_with("large prompt (~100 tokens")),
            "{:?}",
            decided.policy
        );
    }

    #[test]
    fn long_output_may_wake_and_ranks_on_decode_speed() {
        let decided = decide(&cfg(), &req(10, Some(51), None));
        assert!(decided.long_output);
        assert!(
            matches!(&decided.policy, WakePolicy::Keep { why } if why.starts_with("long output (max_tokens 51")),
            "{:?}",
            decided.policy
        );
    }

    #[test]
    fn header_overrides_size() {
        assert_eq!(
            decide(&cfg(), &req(10, None, Some("always"))).policy,
            WakePolicy::Keep {
                why: "x-switchback-wake=always".to_string()
            }
        );
        assert_eq!(
            decide(&cfg(), &req(4_000, Some(500), Some("never"))).policy,
            WakePolicy::Never
        );
        assert_eq!(
            decide(&cfg(), &req(10, None, Some("auto"))).policy,
            WakePolicy::PreferAwake
        );
    }

    #[test]
    fn an_unknown_header_value_is_ignored_and_said() {
        let decided = decide(&cfg(), &req(10, None, Some("sometimes")));
        assert_eq!(decided.policy, WakePolicy::PreferAwake);
        assert!(decided
            .note
            .is_some_and(|note| note.contains("x-switchback-wake=sometimes not understood")));
    }
}
