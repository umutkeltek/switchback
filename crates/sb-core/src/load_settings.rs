//! Load-time settings of a target model: the ones an engine reads when it
//! loads the model (context size, GPU layers, KV cache type, ...), as
//! opposed to the request-time ones in [`crate::model_settings`].
//!
//! Names are engine-neutral; each engine control maps them to its own
//! arguments (`sb-server::engine_control`). Changing one needs the model
//! to load again.

use serde_json::{Map, Value as Json};

use crate::model_settings::{RequestLoadPolicy, SettingsError};

/// The type and range of one load-time key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LoadKind {
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
    },
    Bool,
    Enum(&'static [&'static str]),
    /// A whole number in range, or one of the words.
    IntOr {
        min: i64,
        max: i64,
        words: &'static [&'static str],
    },
    /// `on`/`off`/... or a bool (true = first word, false = second).
    Switch(&'static [&'static str]),
    /// A comma-separated list of the words.
    List(&'static [&'static str]),
    /// A file path on the engine's machine (or `off`). Needs
    /// `allow_model_path` on the lane.
    Path,
}

/// One load-time key.
#[derive(Debug, Clone, Copy)]
pub struct LoadKey {
    pub name: &'static str,
    pub kind: LoadKind,
    pub help: &'static str,
}

const CACHE_TYPES: &[&str] = &[
    "f32", "f16", "bf16", "f8_e4m3", "q8_0", "q4_0", "q4_1", "iq4_nl", "q5_0", "q5_1",
];
const SPEC_TYPES: &[&str] = &[
    "none",
    "draft-simple",
    "draft-eagle3",
    "draft-mtp",
    "draft-dflash",
    "draft-dspark",
    "draft-dflash2",
    "draft-lilicorr",
    "ngram-simple",
    "ngram-map-k",
    "ngram-map-k4v",
    "ngram-mod",
    "ngram-cache",
];

/// Every typed load-time key. Anything else goes in `engine_args`.
pub const LOAD_KEYS: &[LoadKey] = &[
    LoadKey {
        name: "ctx_size",
        kind: LoadKind::Int {
            min: 0,
            max: 16_777_216,
        },
        help: "context length in tokens (0 = the model's own)",
    },
    LoadKey {
        name: "n_gpu_layers",
        kind: LoadKind::IntOr {
            min: -1,
            max: 100_000,
            words: &["auto", "all"],
        },
        help: "layers offloaded to the GPU (number, auto or all)",
    },
    LoadKey {
        name: "cache_type_k",
        kind: LoadKind::Enum(CACHE_TYPES),
        help: "KV cache type for K",
    },
    LoadKey {
        name: "cache_type_v",
        kind: LoadKind::Enum(CACHE_TYPES),
        help: "KV cache type for V",
    },
    LoadKey {
        name: "flash_attn",
        kind: LoadKind::Switch(&["on", "off", "auto"]),
        help: "flash attention: on, off or auto",
    },
    LoadKey {
        name: "batch_size",
        kind: LoadKind::Int {
            min: 1,
            max: 1_048_576,
        },
        help: "logical batch size",
    },
    LoadKey {
        name: "ubatch_size",
        kind: LoadKind::Int {
            min: 1,
            max: 1_048_576,
        },
        help: "physical batch size",
    },
    LoadKey {
        name: "parallel",
        kind: LoadKind::Int { min: 1, max: 1024 },
        help: "parallel sequences (slots)",
    },
    LoadKey {
        name: "threads",
        kind: LoadKind::Int { min: -1, max: 4096 },
        help: "CPU threads for generation",
    },
    LoadKey {
        name: "threads_batch",
        kind: LoadKind::Int { min: -1, max: 4096 },
        help: "CPU threads for prompt processing",
    },
    LoadKey {
        name: "mlock",
        kind: LoadKind::Bool,
        help: "keep the model in RAM",
    },
    LoadKey {
        name: "mmap",
        kind: LoadKind::Bool,
        help: "memory-map the model file",
    },
    LoadKey {
        name: "load_mode",
        kind: LoadKind::Enum(&["auto", "none", "mmap", "mlock", "mmap+mlock"]),
        help: "how the model file is loaded (overrides mlock/mmap)",
    },
    LoadKey {
        name: "rope_scaling",
        kind: LoadKind::Enum(&["none", "linear", "yarn"]),
        help: "RoPE scaling method",
    },
    LoadKey {
        name: "rope_freq_base",
        kind: LoadKind::Float {
            min: 0.0,
            max: 1e12,
        },
        help: "RoPE base frequency",
    },
    LoadKey {
        name: "rope_freq_scale",
        kind: LoadKind::Float { min: 0.0, max: 1e6 },
        help: "RoPE frequency scale",
    },
    LoadKey {
        name: "yarn_orig_ctx",
        kind: LoadKind::Int {
            min: 0,
            max: 16_777_216,
        },
        help: "YaRN original context (0 = training context)",
    },
    LoadKey {
        name: "yarn_ext_factor",
        kind: LoadKind::Float {
            min: -1.0,
            max: 100.0,
        },
        help: "YaRN extrapolation mix",
    },
    LoadKey {
        name: "yarn_attn_factor",
        kind: LoadKind::Float {
            min: 0.0,
            max: 100.0,
        },
        help: "YaRN attention magnitude",
    },
    LoadKey {
        name: "yarn_beta_slow",
        kind: LoadKind::Float {
            min: 0.0,
            max: 1000.0,
        },
        help: "YaRN high correction dim",
    },
    LoadKey {
        name: "yarn_beta_fast",
        kind: LoadKind::Float {
            min: 0.0,
            max: 1000.0,
        },
        help: "YaRN low correction dim",
    },
    LoadKey {
        name: "spec_type",
        kind: LoadKind::List(SPEC_TYPES),
        help: "speculative decoding types, comma-separated",
    },
    LoadKey {
        name: "spec_draft_model",
        kind: LoadKind::Path,
        help: "draft model file for speculative decoding",
    },
    LoadKey {
        name: "draft_max",
        kind: LoadKind::Int { min: 0, max: 1024 },
        help: "most draft tokens per step",
    },
    LoadKey {
        name: "draft_min",
        kind: LoadKind::Int { min: 0, max: 1024 },
        help: "fewest draft tokens per step",
    },
    LoadKey {
        name: "mmproj",
        kind: LoadKind::Path,
        help: "vision projector file, or off",
    },
    LoadKey {
        name: "sleep_idle_seconds",
        kind: LoadKind::Int {
            min: -1,
            max: 31_536_000,
        },
        help: "seconds idle before the instance sleeps (-1 = never)",
    },
    LoadKey {
        name: "fit",
        kind: LoadKind::Switch(&["on", "off"]),
        help: "fit unset arguments to device memory",
    },
    LoadKey {
        name: "fit_ctx",
        kind: LoadKind::Int {
            min: 0,
            max: 16_777_216,
        },
        help: "smallest context fit may choose",
    },
    LoadKey {
        name: "reasoning_budget",
        kind: LoadKind::Int {
            min: -1,
            max: 10_000_000,
        },
        help: "thinking token budget (-1 = no limit)",
    },
    LoadKey {
        name: "temp",
        kind: LoadKind::Float { min: 0.0, max: 5.0 },
        help: "default temperature",
    },
    LoadKey {
        name: "top_k",
        kind: LoadKind::Int {
            min: 0,
            max: 1_000_000,
        },
        help: "default top-k",
    },
    LoadKey {
        name: "top_p",
        kind: LoadKind::Float { min: 0.0, max: 1.0 },
        help: "default top-p",
    },
    LoadKey {
        name: "min_p",
        kind: LoadKind::Float { min: 0.0, max: 1.0 },
        help: "default min-p",
    },
];

pub fn load_key(name: &str) -> Option<&'static LoadKey> {
    LOAD_KEYS.iter().find(|key| key.name == name)
}

fn err<T>(message: impl Into<String>) -> Result<T, SettingsError> {
    Err(SettingsError(message.into()))
}

/// Check one load-time value against its key.
pub fn validate_load_value(name: &str, value: &Json) -> Result<(), SettingsError> {
    let Some(key) = load_key(name) else {
        return err(format!(
            "unknown load setting `{name}`; known: {}; anything else goes in engine_args",
            LOAD_KEYS
                .iter()
                .map(|key| key.name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    let ok = match key.kind {
        LoadKind::Int { min, max } => value.as_i64().is_some_and(|n| (min..=max).contains(&n)),
        LoadKind::Float { min, max } => value.as_f64().is_some_and(|n| (min..=max).contains(&n)),
        LoadKind::Bool => value.is_boolean(),
        LoadKind::Enum(words) => value.as_str().is_some_and(|v| words.contains(&v)),
        LoadKind::IntOr { min, max, words } => {
            value.as_i64().is_some_and(|n| (min..=max).contains(&n))
                || value.as_str().is_some_and(|v| words.contains(&v))
        }
        LoadKind::Switch(words) => {
            value.is_boolean() || value.as_str().is_some_and(|v| words.contains(&v))
        }
        LoadKind::List(words) => value.as_str().is_some_and(|v| {
            !v.is_empty() && v.split(',').all(|item| words.contains(&item.trim()))
        }),
        LoadKind::Path => value
            .as_str()
            .is_some_and(|v| !v.is_empty() && v.len() <= 4096 && !v.contains(['\n', '\r', '\0'])),
    };
    if ok {
        Ok(())
    } else {
        err(format!(
            "load setting `{name}` = {value} is not valid ({}: {})",
            describe(key.kind),
            key.help
        ))
    }
}

fn describe(kind: LoadKind) -> String {
    match kind {
        LoadKind::Int { min, max } => format!("whole number {min}..{max}"),
        LoadKind::Float { min, max } => format!("number {min}..{max}"),
        LoadKind::Bool => "true or false".to_string(),
        LoadKind::Enum(words) => format!("one of {}", words.join(", ")),
        LoadKind::IntOr { min, max, words } => {
            format!("whole number {min}..{max} or {}", words.join(", "))
        }
        LoadKind::Switch(words) => format!("true, false or {}", words.join(", ")),
        LoadKind::List(words) => format!("comma list of {}", words.join(", ")),
        LoadKind::Path => "a file path or off".to_string(),
    }
}

/// Validate a `load` layer.
pub fn validate_load_layer(layer: &Map<String, Json>) -> Result<(), SettingsError> {
    for (name, value) in layer {
        validate_load_value(name, value)?;
    }
    Ok(())
}

/// Whether a load layer names a file path (needs `allow_model_path`).
pub fn load_layer_paths(layer: &Map<String, Json>) -> Vec<String> {
    layer
        .iter()
        .filter(|(name, value)| {
            load_key(name).is_some_and(|key| key.kind == LoadKind::Path)
                && value.as_str() != Some("off")
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Shape check of raw engine arguments: names look like argument names,
/// values are scalars. The engine control checks the names against the
/// engine's own argument list.
pub fn validate_engine_args(args: &Map<String, Json>) -> Result<(), SettingsError> {
    for (name, value) in args {
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return err(format!("engine argument `{name}` is not an argument name"));
        }
        match value {
            Json::String(text) if text.contains(['\n', '\r', '\0']) => {
                return err(format!("engine argument `{name}` has a line break"))
            }
            Json::String(_) | Json::Number(_) | Json::Bool(_) => {}
            _ => {
                return err(format!(
                    "engine argument `{name}` must be a string, number or bool"
                ))
            }
        }
    }
    Ok(())
}

/// Validate a per-request load policy.
pub fn validate_request_load_policy(policy: &RequestLoadPolicy) -> Result<(), SettingsError> {
    for name in &policy.allow {
        if load_key(name).is_none() {
            return err(format!("allow: `{name}` is not a load setting"));
        }
        if load_key(name).is_some_and(|key| key.kind == LoadKind::Path) {
            return err(format!(
                "allow: `{name}` is a file path; requests may not set paths"
            ));
        }
    }
    for (name, [min, max]) in &policy.bounds {
        match load_key(name).map(|key| key.kind) {
            Some(LoadKind::Int { .. } | LoadKind::Float { .. } | LoadKind::IntOr { .. }) => {}
            _ => return err(format!("bounds: `{name}` is not a numeric load setting")),
        }
        if min > max {
            return err(format!("bounds: `{name}` has min above max"));
        }
    }
    if policy.max_variants == 0 || policy.max_variants > 16 {
        return err("max_variants must be 1 to 16");
    }
    if policy.variant_idle_ttl_secs < 60 {
        return err("variant_idle_ttl_secs must be at least 60");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn typed_load_values() {
        for (name, value) in [
            ("ctx_size", json!(65536)),
            ("n_gpu_layers", json!("all")),
            ("n_gpu_layers", json!(99)),
            ("cache_type_k", json!("q8_0")),
            ("flash_attn", json!(true)),
            ("flash_attn", json!("auto")),
            ("spec_type", json!("draft-mtp,ngram-mod")),
            ("mmproj", json!("off")),
        ] {
            validate_load_value(name, &value).unwrap();
        }
        for (name, value) in [
            ("ctx_size", json!(-5)),
            ("cache_type_k", json!("q3")),
            ("spec_type", json!("bogus")),
            ("n_gpu_layers", json!("some")),
            ("contextsize", json!(1)),
            ("mmproj", json!("a\nb")),
        ] {
            assert!(validate_load_value(name, &value).is_err(), "{name}={value}");
        }
    }

    #[test]
    fn engine_args_shape() {
        validate_engine_args(
            json!({"no-warmup": true, "n-cpu-moe": 4})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(validate_engine_args(json!({"a b": 1}).as_object().unwrap()).is_err());
        assert!(validate_engine_args(json!({"x": [1]}).as_object().unwrap()).is_err());
    }

    #[test]
    fn request_load_policy_checks() {
        let ok = RequestLoadPolicy {
            allow: vec!["ctx_size".into()],
            bounds: [("ctx_size".to_string(), [4096.0, 131072.0])].into(),
            max_variants: 2,
            variant_idle_ttl_secs: 3600,
        };
        validate_request_load_policy(&ok).unwrap();
        let mut bad = ok.clone();
        bad.allow = vec!["mmproj".into()];
        assert!(validate_request_load_policy(&bad).is_err());
        let mut bad = ok;
        bad.bounds.insert("cache_type_k".into(), [0.0, 1.0]);
        assert!(validate_request_load_policy(&bad).is_err());
    }
}
