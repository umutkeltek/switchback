//! Speed tricks for local engines ("optimize"): the config of the
//! always-on per-request tricks (conversation stickiness, loaded-model
//! preference, big-prompt placement), the per-target settings fields that
//! go with them (`reasoning`, `reasoning_guard`, `ceiling`), and the pure
//! helpers the runtime uses (the conversation key, the reasoning guard).
//!
//! Method credit: the measuring and tuning rules follow the
//! inference-optimisation skill,
//! <https://github.com/sybil-solutions/inference-optimisation-skill>.
//!
//! Every trick is off by default; a route or a target turns it on. None of
//! them wakes a machine or makes a request wait: they only reorder targets
//! that are already up, or change what one request asks of an engine, and
//! each says so on the route decision.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::{AiRequest, ContentPart, Role};

/// Request-metadata key for a tuning run's lock id (header
/// `x-switchback-tune-lock`): a request that carries the lock of a lane
/// under a tuning run may use that lane; every other request skips it.
pub const TUNE_LOCK_METADATA_KEY: &str = "switchback_tune_lock";
/// The request header carrying a tuning lock id.
pub const TUNE_LOCK_HEADER: &str = "x-switchback-tune-lock";
/// The response header saying what the per-request tricks did.
pub const OPTIMIZE_RESPONSE_HEADER: &str = "x-switchback-optimize";

/// Server-wide speed tricks (`server.optimize`). All off by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OptimizeConfig {
    /// Keep a conversation on the machine that served it last (prefix
    /// cache reuse), while that machine is up, healthy and has room.
    pub stickiness: bool,
    /// How long a conversation stays sticky after its last request.
    pub sticky_ttl_secs: u64,
    /// Most conversations remembered (oldest forgotten first).
    pub sticky_max_conversations: usize,
    /// Among targets on one machine that is up, prefer the one whose model
    /// the engine has loaded (a swap costs seconds).
    pub loaded_aware: bool,
    /// A prompt of at least this many (estimated) tokens goes to the awake
    /// target with the highest measured prompt speed. `None` = off.
    pub big_prompt_tokens: Option<u32>,
    /// Measured or published numbers per machine (the lane's `host`, or
    /// the lane name): memory bandwidth and peak compute, for ceilings.
    pub hosts: BTreeMap<String, HostSpec>,
    /// Windows in which `sb ai tune` refuses to start and a running tune
    /// stops at its next safe point (for example while a nightly job runs).
    /// Each is `"HH:MM-HH:MM"` (in `timezone`) or `{start, end, tz}` with
    /// its own IANA time zone. A window may wrap midnight.
    pub quiet_windows: Vec<QuietWindow>,
    /// The IANA time zone (`Asia/Tokyo`) of windows that name none.
    /// Absent = UTC.
    pub timezone: Option<String>,
    /// Thresholds of the speed report's findings.
    pub report: ReportThresholds,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        OptimizeConfig {
            stickiness: false,
            sticky_ttl_secs: 1800,
            sticky_max_conversations: 10_000,
            loaded_aware: false,
            big_prompt_tokens: None,
            hosts: BTreeMap::new(),
            quiet_windows: Vec::new(),
            timezone: None,
            report: ReportThresholds::default(),
        }
    }
}

impl OptimizeConfig {
    /// Check the windows and numbers; a bad one names itself.
    pub fn validate(&self) -> Result<(), String> {
        for window in &self.quiet_windows {
            window.minutes()?;
        }
        for (host, spec) in &self.hosts {
            for (name, value) in [
                ("bandwidth_gbs", spec.bandwidth_gbs),
                ("peak_tflops", spec.peak_tflops),
            ] {
                if value.is_some_and(|v| !(v.is_finite() && v > 0.0)) {
                    return Err(format!(
                        "server.optimize.hosts.{host}.{name} must be a positive number"
                    ));
                }
            }
        }
        if self.sticky_max_conversations == 0 {
            return Err("server.optimize.sticky_max_conversations must be at least 1".into());
        }
        Ok(())
    }

    /// Every window with the time zone it is read in (`None` = UTC).
    pub fn windows(&self) -> impl Iterator<Item = (&QuietWindow, Option<&str>)> {
        self.quiet_windows
            .iter()
            .map(|window| (window, window.tz().or(self.timezone.as_deref())))
    }

    /// The window that holds the moment, given `local_minute`: minutes
    /// since local midnight in a time zone (`None` = UTC), or `None` when
    /// the zone is unknown (then the window is taken to hold, so a bad
    /// zone never lets a tune run into a nightly job). Its label names
    /// the zone.
    pub fn quiet_window_at(
        &self,
        mut local_minute: impl FnMut(Option<&str>) -> Option<u32>,
    ) -> Option<String> {
        self.windows().find_map(|(window, tz)| {
            let (start, end) = window.minutes().ok()?;
            let inside = local_minute(tz).is_none_or(|now| minute_in_window(now, start, end));
            inside.then(|| window.label(tz))
        })
    }
}

/// One quiet window: `"HH:MM-HH:MM"`, or `{start, end, tz}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QuietWindow {
    Text(String),
    Spec(QuietWindowSpec),
}

/// `{start: "00:30", end: "03:00", tz: "Asia/Tokyo"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuietWindowSpec {
    pub start: String,
    pub end: String,
    /// IANA time zone; absent = `server.optimize.timezone`, else UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
}

impl From<&str> for QuietWindow {
    fn from(text: &str) -> Self {
        QuietWindow::Text(text.to_string())
    }
}

impl QuietWindow {
    /// Start and end, minutes since midnight.
    pub fn minutes(&self) -> Result<(u32, u32), String> {
        match self {
            QuietWindow::Text(text) => parse_window(text),
            QuietWindow::Spec(spec) => parse_window(&format!("{}-{}", spec.start, spec.end)),
        }
    }

    /// The window's own time zone, when it names one.
    pub fn tz(&self) -> Option<&str> {
        match self {
            QuietWindow::Text(_) => None,
            QuietWindow::Spec(spec) => spec.tz.as_deref(),
        }
    }

    /// `00:30-03:00 Asia/Tokyo` (`UTC` when no zone applies).
    pub fn label(&self, tz: Option<&str>) -> String {
        let span = match self {
            QuietWindow::Text(text) => text.trim().to_string(),
            QuietWindow::Spec(spec) => format!("{}-{}", spec.start.trim(), spec.end.trim()),
        };
        format!("{span} {}", tz.unwrap_or("UTC"))
    }
}

/// `HH:MM-HH:MM` to minutes since midnight.
pub fn parse_window(window: &str) -> Result<(u32, u32), String> {
    let bad = || format!("quiet window `{window}` must be HH:MM-HH:MM");
    let (start, end) = window.trim().split_once('-').ok_or_else(bad)?;
    let minute = |text: &str| -> Result<u32, String> {
        let (h, m) = text.trim().split_once(':').ok_or_else(bad)?;
        let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
        if h > 23 || m > 59 {
            return Err(bad());
        }
        Ok(h * 60 + m)
    };
    Ok((minute(start)?, minute(end)?))
}

/// Whether a minute of the day falls in `[start, end)`, wrapping midnight
/// when `end <= start`.
pub fn minute_in_window(minute: u32, start: u32, end: u32) -> bool {
    if start < end {
        (start..end).contains(&minute)
    } else {
        minute >= start || minute < end
    }
}

/// Published or measured numbers of one machine.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostSpec {
    /// Memory bandwidth in GB/s (decode is bound by it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth_gbs: Option<f64>,
    /// Peak matrix compute in TFLOPS (prefill is bound by it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_tflops: Option<f64>,
}

/// When the speed report raises a finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReportThresholds {
    /// Decode below this share of the ceiling (percent).
    pub decode_ceiling_pct: f64,
    /// More model swaps than this in the last hour is thrash.
    pub swaps_per_hour: u32,
    /// Prefix-cache hit rate below this (percent) on repeated system
    /// prompts.
    pub cache_hit_pct: f64,
    /// Fewest requests before cache and acceptance findings count.
    pub min_requests: u32,
    /// Speculative acceptance below this (percent).
    pub acceptance_pct: f64,
    /// A model unused for this many days is a removal candidate.
    pub unused_days: u32,
    /// Decode or TTFT worse by more than this (percent) after a settings
    /// revision is a regression.
    pub regression_pct: f64,
    /// Fewest samples on each side of a revision for a regression.
    pub regression_samples: u32,
}

impl Default for ReportThresholds {
    fn default() -> Self {
        ReportThresholds {
            decode_ceiling_pct: 40.0,
            swaps_per_hour: 6,
            cache_hit_pct: 10.0,
            min_requests: 10,
            acceptance_pct: 50.0,
            unused_days: 14,
            regression_pct: 10.0,
            regression_samples: 6,
        }
    }
}

/// A route's own switches (`routes[].optimize`): a field set replaces the
/// server's, a field left out keeps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteOptimizeOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stickiness: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loaded_aware: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub big_prompt_tokens: Option<u32>,
}

/// The switches in force for one route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RouteOptimize {
    pub stickiness: bool,
    pub loaded_aware: bool,
    pub big_prompt_tokens: Option<u32>,
}

impl RouteOptimize {
    pub fn resolve(server: &OptimizeConfig, route: Option<&RouteOptimizeOverride>) -> Self {
        let route = route.copied().unwrap_or_default();
        RouteOptimize {
            stickiness: route.stickiness.unwrap_or(server.stickiness),
            loaded_aware: route.loaded_aware.unwrap_or(server.loaded_aware),
            big_prompt_tokens: route.big_prompt_tokens.or(server.big_prompt_tokens),
        }
    }

    pub fn any(&self) -> bool {
        self.stickiness || self.loaded_aware || self.big_prompt_tokens.is_some()
    }
}

/// The reasoning guard of a target that thinks (`reasoning: true` in its
/// settings): a request with a small `max_tokens` gets thinking off, so the
/// budget is not spent on thinking with an empty answer at the end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReasoningGuard {
    pub enabled: bool,
    /// `max_tokens` below this turns thinking off (default 1024).
    pub min_tokens: u32,
}

impl Default for ReasoningGuard {
    fn default() -> Self {
        ReasoningGuard {
            enabled: true,
            min_tokens: 1024,
        }
    }
}

/// The theoretical speed limit of a target on its machine (the skill's
/// "ceilings first"): decode is memory-bound (bandwidth / bytes per
/// token), prefill compute-bound.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ceiling {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_tps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefill_tps: Option<f64>,
    /// Bytes read per generated token (weights touched + KV), from
    /// `sb ai fit REPO --store TARGET`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_per_token: Option<u64>,
    /// Where the numbers came from (`sb ai fit org/repo`, `measured`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl Ceiling {
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("decode_tps", self.decode_tps),
            ("prefill_tps", self.prefill_tps),
        ] {
            if value.is_some_and(|v| !(v.is_finite() && v > 0.0)) {
                return Err(format!("ceiling.{name} must be a positive number"));
            }
        }
        if self.bytes_per_token == Some(0) {
            return Err("ceiling.bytes_per_token must be positive".into());
        }
        if self.source.as_ref().is_some_and(|s| s.len() > 200) {
            return Err("ceiling.source is at most 200 characters".into());
        }
        Ok(())
    }
}

/// FNV-1a 64 of some text, as 16 hex digits (stable across runs).
pub fn fnv_hex(parts: &[&str]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0u8)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

fn text_of(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if let ContentPart::Text { text } = part {
            out.push_str(text);
        }
    }
    out
}

/// The key of the conversation a request belongs to, for stickiness: the
/// session the client named (`x-switchback-session` and the other session
/// headers, or `metadata.session_id`) when given; else the OpenAI `user`
/// field plus a hash of the system prompt and the first user message (both
/// stay the same for every turn of one conversation). `None` when the
/// request has no user message. Never holds prompt text.
pub fn conversation_key(req: &AiRequest, session: Option<&str>) -> Option<String> {
    if let Some(session) = session.filter(|s| !s.is_empty()) {
        return Some(format!("session:{}", fnv_hex(&[session])));
    }
    let first_user = req
        .messages
        .iter()
        .find(|message| matches!(message.role, Role::User))
        .map(|message| text_of(&message.content))
        .filter(|text| !text.is_empty())?;
    let mut system = req.system.clone().unwrap_or_default();
    for message in &req.messages {
        if matches!(message.role, Role::System) {
            system.push_str(&text_of(&message.content));
        }
    }
    let user = req
        .passthrough
        .get("user")
        .and_then(Json::as_str)
        .unwrap_or("");
    Some(format!("conv:{}", fnv_hex(&[user, &system, &first_user])))
}

/// Whether the request itself asked for thinking (not a profile or the
/// target's defaults): `reasoning_effort` other than `none`,
/// `chat_template_kwargs.enable_thinking: true`, or a `reasoning` key in its
/// own `switchback.settings`.
pub fn request_asks_reasoning(
    req: &AiRequest,
    own_settings: Option<&serde_json::Map<String, Json>>,
) -> bool {
    let effort_on = req
        .passthrough
        .get("reasoning_effort")
        .and_then(Json::as_str)
        .is_some_and(|effort| effort != "none");
    let kwargs_on = req
        .passthrough
        .get("chat_template_kwargs")
        .and_then(|kwargs| kwargs.get("enable_thinking"))
        .and_then(Json::as_bool)
        == Some(true);
    let own = own_settings
        .and_then(|settings| settings.get("reasoning"))
        .is_some_and(|reasoning| {
            reasoning.get("enabled").and_then(Json::as_bool) != Some(false)
                && reasoning.get("effort").and_then(Json::as_str) != Some("none")
        });
    effort_on || kwargs_on || own
}

/// Turn thinking off on a prepared request (the reasoning guard): the
/// engine codec translates `reasoning_effort: none` per engine (llama.cpp
/// and vLLM get `chat_template_kwargs.enable_thinking: false`).
pub fn turn_thinking_off(req: &mut AiRequest) {
    req.metadata.remove(crate::REASONING_ENABLE_METADATA_KEY);
    if let Some(kwargs) = req
        .passthrough
        .get_mut("chat_template_kwargs")
        .and_then(Json::as_object_mut)
    {
        kwargs.remove("enable_thinking");
        kwargs.remove("reasoning_effort");
    }
    req.passthrough
        .insert("reasoning_effort".to_string(), Json::from("none"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use serde_json::json;

    #[test]
    fn windows_parse_and_wrap_midnight() {
        assert_eq!(parse_window("00:30-03:00").unwrap(), (30, 180));
        assert!(parse_window("25:00-01:00").is_err());
        assert!(parse_window("0030-0300").is_err());
        assert!(minute_in_window(60, 30, 180));
        assert!(!minute_in_window(180, 30, 180));
        // 23:00-01:00 wraps.
        assert!(minute_in_window(23 * 60 + 30, 23 * 60, 60));
        assert!(minute_in_window(10, 23 * 60, 60));
        assert!(!minute_in_window(120, 23 * 60, 60));
        let cfg = OptimizeConfig {
            quiet_windows: vec!["00:30-03:00".into()],
            ..OptimizeConfig::default()
        };
        assert_eq!(
            cfg.quiet_window_at(|_| Some(45)).as_deref(),
            Some("00:30-03:00 UTC")
        );
        assert_eq!(cfg.quiet_window_at(|_| Some(200)), None);
    }

    #[test]
    fn windows_name_their_zone_and_an_unknown_zone_holds() {
        let cfg: OptimizeConfig = serde_json::from_value(json!({
            "timezone": "Asia/Tokyo",
            "quiet_windows": [
                "01:00-02:00",
                {"start": "10:00", "end": "11:00", "tz": "America/New_York"},
                {"start": "20:00", "end": "21:00"}
            ]
        }))
        .unwrap();
        cfg.validate().unwrap();
        let zones: Vec<_> = cfg.windows().map(|(_, tz)| tz).collect();
        assert_eq!(
            zones,
            [
                Some("Asia/Tokyo"),
                Some("America/New_York"),
                Some("Asia/Tokyo")
            ]
        );
        // Each window is read in its own zone's local minute.
        let at = |istanbul: u32, new_york: u32| {
            cfg.quiet_window_at(|tz| match tz {
                Some("America/New_York") => Some(new_york),
                _ => Some(istanbul),
            })
        };
        assert_eq!(at(90, 0).as_deref(), Some("01:00-02:00 Asia/Tokyo"));
        assert_eq!(
            at(0, 10 * 60 + 5).as_deref(),
            Some("10:00-11:00 America/New_York")
        );
        assert_eq!(at(10 * 60 + 5, 0), None);
        // A zone that cannot be read holds (fail closed).
        assert!(cfg.quiet_window_at(|_| None).is_some());
        let bad: Result<OptimizeConfig, _> =
            serde_json::from_value(json!({"quiet_windows": [{"start": "1:00"}]}));
        assert!(bad.is_err());
        let typo: Result<OptimizeConfig, _> = serde_json::from_value(
            json!({"quiet_windows": [{"start": "01:00", "end": "02:00", "zone": "UTC"}]}),
        );
        assert!(typo.is_err());
    }

    #[test]
    fn route_switches_override_the_server_field_by_field() {
        let server = OptimizeConfig {
            stickiness: true,
            big_prompt_tokens: Some(8000),
            ..OptimizeConfig::default()
        };
        let route = RouteOptimizeOverride {
            loaded_aware: Some(true),
            stickiness: Some(false),
            ..RouteOptimizeOverride::default()
        };
        let resolved = RouteOptimize::resolve(&server, Some(&route));
        assert!(!resolved.stickiness);
        assert!(resolved.loaded_aware);
        assert_eq!(resolved.big_prompt_tokens, Some(8000));
        assert!(!RouteOptimize::resolve(&OptimizeConfig::default(), None).any());
    }

    #[test]
    fn conversation_key_is_stable_across_turns_and_never_holds_text() {
        let mut turn1 = AiRequest::new("m", vec![Message::user("plan my trip to the coast")]);
        turn1.system = Some("You are terse.".into());
        let mut turn2 = turn1.clone();
        turn2.messages.push(Message::user("and the hotel?"));
        let k1 = conversation_key(&turn1, None).unwrap();
        assert_eq!(k1, conversation_key(&turn2, None).unwrap());
        assert!(!k1.contains("trip"));
        // Another first message or another user: another conversation.
        let other = AiRequest::new("m", vec![Message::user("something else")]);
        assert_ne!(k1, conversation_key(&other, None).unwrap());
        let mut by_user = turn1.clone();
        by_user.passthrough.insert("user".into(), json!("u-7"));
        assert_ne!(k1, conversation_key(&by_user, None).unwrap());
        // A named session wins.
        assert!(conversation_key(&turn1, Some("s-1"))
            .unwrap()
            .starts_with("session:"));
        assert!(conversation_key(&AiRequest::new("m", vec![]), None).is_none());
    }

    #[test]
    fn explicit_reasoning_is_detected_from_the_request_only() {
        let mut req = AiRequest::new("m", vec![Message::user("hi")]);
        assert!(!request_asks_reasoning(&req, None));
        req.passthrough
            .insert("reasoning_effort".into(), json!("high"));
        assert!(request_asks_reasoning(&req, None));
        req.passthrough
            .insert("reasoning_effort".into(), json!("none"));
        assert!(!request_asks_reasoning(&req, None));
        req.passthrough.remove("reasoning_effort");
        req.passthrough.insert(
            "chat_template_kwargs".into(),
            json!({"enable_thinking": true}),
        );
        assert!(request_asks_reasoning(&req, None));
        let req = AiRequest::new("m", vec![Message::user("hi")]);
        let own = json!({"reasoning": {"effort": "low"}});
        assert!(request_asks_reasoning(&req, own.as_object()));
        let own = json!({"reasoning": {"enabled": false}});
        assert!(!request_asks_reasoning(&req, own.as_object()));
    }

    #[test]
    fn turning_thinking_off_replaces_template_kwargs() {
        let mut req = AiRequest::new("m", vec![Message::user("hi")]);
        req.passthrough.insert(
            "chat_template_kwargs".into(),
            json!({"enable_thinking": true, "x": 1}),
        );
        turn_thinking_off(&mut req);
        assert_eq!(req.passthrough["reasoning_effort"], json!("none"));
        assert_eq!(req.passthrough["chat_template_kwargs"], json!({"x": 1}));
    }

    #[test]
    fn ceiling_and_host_numbers_are_checked() {
        assert!(Ceiling {
            decode_tps: Some(-1.0),
            ..Ceiling::default()
        }
        .validate()
        .is_err());
        let mut cfg = OptimizeConfig::default();
        cfg.hosts.insert(
            "gpu-box".into(),
            HostSpec {
                bandwidth_gbs: Some(0.0),
                peak_tflops: None,
            },
        );
        assert!(cfg.validate().is_err());
    }
}
