//! Pure refresh timing over non-secret observations. Collectors own IO and
//! identity binding; a refresh plan never grants capacity or renews credentials.
use super::*;
use time::OffsetDateTime;

#[derive(Args, Debug)]
pub(crate) struct RefreshArgs {
    /// Bounded metadata-only input, keyed by collector/account.
    #[arg(long)]
    observations_json: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    key: String,
    #[serde(default)]
    sampled_at_ms: Option<i64>,
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    resets_at_ms: Option<i64>,
    #[serde(default)]
    last_attempt_at_ms: Option<i64>,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    needs_login: bool,
    #[serde(default = "enabled")]
    enabled: bool,
}

fn plan(observation: &Observation, now: i64) -> Value {
    let o = observation;
    let stopped = if !o.enabled {
        Some("disabled")
    } else if o.needs_login {
        Some("needs_login")
    } else {
        None
    };
    if let Some(reason) = stopped {
        return json!({"key":o.key,"due":false,"reason":reason,"next_refresh_at_ms":null});
    }
    let last = o
        .last_attempt_at_ms
        .filter(|t| *t >= 0 && *t <= now.saturating_add(30_000));
    let sample = o
        .sampled_at_ms
        .filter(|t| *t >= 0 && *t <= now.saturating_add(30_000));
    let used = o
        .used_percent
        .filter(|n| n.is_finite() && (0.0..=100.0).contains(n));
    let interval = match used {
        Some(n) if n >= 90.0 => 120_000,
        Some(n) if n >= 70.0 => 300_000,
        Some(_) => 900_000,
        None => 120_000,
    };
    let (mut next, mut reason) = match (sample, used) {
        (Some(t), Some(_)) => (t.saturating_add(interval), "sample_interval"),
        _ => (now, "missing_observation"),
    };
    if let (Some(sample), Some(reset)) = (sample, o.resets_at_ms) {
        if reset > sample && reset < next {
            next = reset;
            reason = "quota_reset";
        }
    }
    if let Some(last) = last {
        let backoff = if o.consecutive_failures == 0 {
            120_000
        } else {
            (120_000i64 << (o.consecutive_failures - 1).min(4)).min(1_800_000)
        };
        if last.saturating_add(backoff) > next {
            next = last.saturating_add(backoff);
            reason = if o.consecutive_failures > 0 {
                "failure_backoff"
            } else {
                "minimum_interval"
            };
        }
    }
    json!({"key":o.key,"due":next <= now,"reason":reason,"next_refresh_at_ms":next})
}

pub(super) fn run(args: RefreshArgs, json_output: bool) -> anyhow::Result<()> {
    if args.observations_json.len() > 65_536 {
        anyhow::bail!("refresh observations exceed limit");
    }
    let observations: Vec<Observation> = serde_json::from_str(&args.observations_json)?;
    if observations.len() > 128 {
        anyhow::bail!("too many refresh collectors");
    }
    let mut keys = BTreeSet::new();
    for o in &observations {
        if o.key.is_empty()
            || o.key.len() > 256
            || o.key.chars().any(char::is_control)
            || !keys.insert(&o.key)
        {
            anyhow::bail!("invalid or duplicate refresh collector");
        }
    }
    let now = i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000)?;
    emit(
        &json!({"schema":"switchback/usage-refresh-plan@1","generated_at_ms":now,
        "collectors":observations.iter().map(|o|plan(o,now)).collect::<Vec<_>>(),
        "purpose":"refresh_timing_not_admission_authority"}),
        json_output,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const NOW: i64 = 1_800_000_000_000;
    fn observation() -> Observation {
        Observation {
            key: "fixture/one".into(),
            sampled_at_ms: Some(NOW - 100_000),
            used_percent: Some(20.0),
            resets_at_ms: None,
            last_attempt_at_ms: None,
            consecutive_failures: 0,
            needs_login: false,
            enabled: true,
        }
    }
    #[test]
    fn refresh_policy_adapts_without_refreshing_sample_timestamps() {
        let mut o = observation();
        assert_eq!(plan(&o, NOW)["next_refresh_at_ms"], NOW + 800_000);
        o.used_percent = Some(95.0);
        assert_eq!(plan(&o, NOW)["next_refresh_at_ms"], NOW + 20_000);
        assert_eq!(plan(&o, NOW + 20_000)["due"], true);
        assert_eq!(o.sampled_at_ms, Some(NOW - 100_000));
    }
    #[test]
    fn refresh_policy_backoff_caps_and_survives_elapsed_resets() {
        let mut o = observation();
        o.resets_at_ms = Some(NOW - 10_000);
        o.last_attempt_at_ms = Some(NOW - 30_000);
        o.consecutive_failures = u32::MAX;
        let p = plan(&o, NOW);
        assert_eq!(p["reason"], "failure_backoff");
        assert_eq!(p["next_refresh_at_ms"], NOW + 1_770_000);
        assert_eq!(p["due"], false);
    }
    #[test]
    fn refresh_policy_checks_reset_without_a_retry_storm() {
        let mut o = observation();
        o.resets_at_ms = Some(NOW - 1);
        assert_eq!(plan(&o, NOW)["due"], true);
        o.last_attempt_at_ms = Some(NOW - 1);
        assert_eq!(plan(&o, NOW)["due"], false);
        assert_eq!(plan(&o, NOW + 120_000)["due"], true);
    }
    #[test]
    fn refresh_policy_stops_automatic_auth_retries() {
        let mut o = observation();
        o.needs_login = true;
        assert_eq!(plan(&o, NOW)["reason"], "needs_login");
        assert_eq!(plan(&o, NOW)["due"], false);
        o.needs_login = false;
        o.enabled = false;
        assert_eq!(plan(&o, NOW)["reason"], "disabled");
    }
    #[test]
    fn refresh_policy_unknown_and_clock_skew_never_invent_a_sample() {
        let mut o = observation();
        o.sampled_at_ms = Some(NOW + 1_000_000);
        o.last_attempt_at_ms = Some(NOW + 1_000_000);
        assert_eq!(plan(&o, NOW)["due"], true);
        o.sampled_at_ms = Some(NOW);
        o.used_percent = Some(f64::NAN);
        assert_eq!(plan(&o, NOW)["reason"], "missing_observation");
    }
}
