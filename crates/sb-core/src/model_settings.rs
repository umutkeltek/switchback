//! Per-model settings: the knobs an operator sets once for a target model
//! (`provider/model`) and the ones a single request may set for itself.
//!
//! A settings document has three request-time parts:
//!
//! - `defaults`: applied to every request that reaches the target;
//! - `profiles`: named sets of the same keys, layered over `defaults`;
//! - `locked`: keys a request may not override.
//!
//! Precedence per key, highest first: the request's own value (unless the
//! key is locked) > the profile the request asked for > the route's
//! profile > `defaults` > nothing (the engine's own default). A key nobody
//! set is never sent, so a target with no settings gets exactly the request
//! it got before settings existed.
//!
//! Keys are engine-neutral names; the adapters translate the few an engine
//! spells differently (see `sb-adapters::codec`). Engine-specific
//! per-request keys go under `extra` and are sent as they are.
//!
//! Load-time settings (`load`, `engine_args`) and the per-request load
//! policy (`request_load`) are validated here too; applying them is the
//! engine control's business (`sb-server::engine_control`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};

use crate::{AiRequest, ContentPart, ResponseFormat, Role};

/// Request header that names the settings profile for one request.
pub const SETTINGS_PROFILE_HEADER: &str = "x-switchback-profile";
/// Request-metadata key the gateway stores the [`SETTINGS_PROFILE_HEADER`]
/// value under (never sent upstream).
pub const SETTINGS_PROFILE_METADATA_KEY: &str = "switchback_profile";
/// Response header saying which settings profile served the request and
/// which keys a lock overrode, e.g. `profile=nothink; locked=temperature`.
pub const SETTINGS_RESPONSE_HEADER: &str = "x-switchback-settings";
/// The request body object Switchback reads and strips before dispatch.
pub const SWITCHBACK_BODY_KEY: &str = "switchback";
/// Request-metadata key the gateway stores the caller's key role under
/// (`client`, `operator`, `admin`): per-request load settings depend on it.
/// Set by the gateway after parsing, never sent upstream.
pub const SETTINGS_ROLE_METADATA_KEY: &str = "switchback_key_role";
/// Request-metadata marker: settings asked for thinking on without an
/// effort. Engines that switch thinking through a template argument read it
/// in the adapter; never sent upstream.
pub const REASONING_ENABLE_METADATA_KEY: &str = "switchback_reasoning_enable";

/// Longest profile name accepted.
const MAX_PROFILE_NAME: usize = 64;
/// Most profiles one target may carry.
const MAX_PROFILES: usize = 64;
/// Longest system prompt a settings document may carry (bytes).
const MAX_SYSTEM_PROMPT: usize = 64 * 1024;

/// The type and range of one request-time key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyKind {
    Float {
        min: f64,
        max: f64,
    },
    Int {
        min: i64,
        max: i64,
    },
    /// A string or a list of at most 16 strings.
    Stop,
    /// `text`, `json_object`, or a JSON-schema object.
    ResponseFormat,
    /// Free text.
    Text,
    /// One of a fixed list.
    Enum(&'static [&'static str]),
    /// `{enabled, effort, budget_tokens}`, merged field by field.
    Reasoning,
    /// Engine-specific keys sent as they are, merged key by key.
    Extra,
}

/// One request-time key: name, type, and what it does.
#[derive(Debug, Clone, Copy)]
pub struct RequestKey {
    pub name: &'static str,
    pub kind: KeyKind,
    pub help: &'static str,
}

const REASONING_EFFORTS: &[&str] = &["none", "minimal", "low", "medium", "high"];
const SYSTEM_PROMPT_MODES: &[&str] = &["prepend", "replace_if_absent"];

/// Every typed request-time key. Anything else must go under `extra`.
pub const REQUEST_KEYS: &[RequestKey] = &[
    RequestKey {
        name: "temperature",
        kind: KeyKind::Float { min: 0.0, max: 5.0 },
        help: "sampling temperature",
    },
    RequestKey {
        name: "top_p",
        kind: KeyKind::Float { min: 0.0, max: 1.0 },
        help: "nucleus sampling",
    },
    RequestKey {
        name: "top_k",
        kind: KeyKind::Int {
            min: 0,
            max: 1_000_000,
        },
        help: "keep the k most likely tokens (0 = off)",
    },
    RequestKey {
        name: "min_p",
        kind: KeyKind::Float { min: 0.0, max: 1.0 },
        help: "drop tokens below this share of the top token",
    },
    RequestKey {
        name: "typical_p",
        kind: KeyKind::Float { min: 0.0, max: 1.0 },
        help: "locally typical sampling (1 = off)",
    },
    RequestKey {
        name: "repeat_penalty",
        kind: KeyKind::Float {
            min: 0.0,
            max: 10.0,
        },
        help: "penalty for repeated tokens (1 = off)",
    },
    RequestKey {
        name: "repeat_last_n",
        kind: KeyKind::Int {
            min: -1,
            max: 1_000_000,
        },
        help: "how many last tokens the repeat penalty sees (-1 = context)",
    },
    RequestKey {
        name: "presence_penalty",
        kind: KeyKind::Float {
            min: -2.0,
            max: 2.0,
        },
        help: "penalty for tokens already present",
    },
    RequestKey {
        name: "frequency_penalty",
        kind: KeyKind::Float {
            min: -2.0,
            max: 2.0,
        },
        help: "penalty by token frequency",
    },
    RequestKey {
        name: "seed",
        kind: KeyKind::Int {
            min: -1,
            max: u32::MAX as i64,
        },
        help: "sampling seed (-1 = random)",
    },
    RequestKey {
        name: "max_tokens",
        kind: KeyKind::Int {
            min: 1,
            max: 10_000_000,
        },
        help: "output token limit when the request gives none",
    },
    RequestKey {
        name: "stop",
        kind: KeyKind::Stop,
        help: "stop string or list of stop strings",
    },
    RequestKey {
        name: "system_prompt",
        kind: KeyKind::Text,
        help: "system prompt added to the request (see system_prompt_mode)",
    },
    RequestKey {
        name: "system_prompt_mode",
        kind: KeyKind::Enum(SYSTEM_PROMPT_MODES),
        help: "prepend (default) or replace_if_absent",
    },
    RequestKey {
        name: "response_format",
        kind: KeyKind::ResponseFormat,
        help: "text, json_object or a json_schema object, when the request gives none",
    },
    RequestKey {
        name: "reasoning",
        kind: KeyKind::Reasoning,
        help: "{enabled, effort: none|low|medium|high, budget_tokens}",
    },
    RequestKey {
        name: "extra",
        kind: KeyKind::Extra,
        help: "engine-specific per-request keys, sent as they are",
    },
];

/// The fields of `reasoning`.
pub const REASONING_FIELDS: &[&str] = &["enabled", "effort", "budget_tokens"];

/// Body keys `extra` may not carry: the request's own structure.
const EXTRA_FORBIDDEN: &[&str] = &[
    "model",
    "messages",
    "stream",
    "tools",
    "functions",
    "prompt",
    SWITCHBACK_BODY_KEY,
];

pub fn request_key(name: &str) -> Option<&'static RequestKey> {
    REQUEST_KEYS.iter().find(|key| key.name == name)
}

/// A settings error: what is wrong, in words a client can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError(pub String);

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettingsError {}

fn err<T>(message: impl Into<String>) -> Result<T, SettingsError> {
    Err(SettingsError(message.into()))
}

fn known_keys_hint() -> String {
    REQUEST_KEYS
        .iter()
        .map(|key| key.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check one typed value against its key.
fn validate_value(path: &str, kind: KeyKind, value: &Json) -> Result<(), SettingsError> {
    match kind {
        KeyKind::Float { min, max } => {
            let Some(number) = value.as_f64() else {
                return err(format!("setting `{path}` must be a number"));
            };
            if !(min..=max).contains(&number) {
                return err(format!(
                    "setting `{path}` = {number} is out of range [{min}, {max}]"
                ));
            }
        }
        KeyKind::Int { min, max } => {
            let Some(number) = value.as_i64() else {
                return err(format!("setting `{path}` must be a whole number"));
            };
            if !(min..=max).contains(&number) {
                return err(format!(
                    "setting `{path}` = {number} is out of range [{min}, {max}]"
                ));
            }
        }
        KeyKind::Stop => match value {
            Json::String(_) => {}
            Json::Array(items) => {
                if items.len() > 16 {
                    return err(format!("setting `{path}` takes at most 16 stop strings"));
                }
                if !items.iter().all(Json::is_string) {
                    return err(format!("setting `{path}` must be a string or strings"));
                }
            }
            _ => return err(format!("setting `{path}` must be a string or strings")),
        },
        KeyKind::Text => {
            let Some(text) = value.as_str() else {
                return err(format!("setting `{path}` must be a string"));
            };
            if text.len() > MAX_SYSTEM_PROMPT {
                return err(format!(
                    "setting `{path}` is longer than {MAX_SYSTEM_PROMPT} bytes"
                ));
            }
        }
        KeyKind::Enum(values) => {
            if !value.as_str().is_some_and(|value| values.contains(&value)) {
                return err(format!(
                    "setting `{path}` must be one of {}",
                    values.join(", ")
                ));
            }
        }
        KeyKind::ResponseFormat => {
            response_format_from_setting(value)
                .map_err(|e| SettingsError(format!("setting `{path}`: {e}")))?;
        }
        KeyKind::Reasoning => {
            let Some(object) = value.as_object() else {
                return err(format!(
                    "setting `{path}` must be an object {{enabled, effort, budget_tokens}}"
                ));
            };
            for (field, field_value) in object {
                validate_reasoning_field(&format!("{path}.{field}"), field, field_value)?;
            }
        }
        KeyKind::Extra => {
            let Some(object) = value.as_object() else {
                return err(format!("setting `{path}` must be an object"));
            };
            for (field, field_value) in object {
                validate_extra_field(field, field_value)?;
            }
        }
    }
    Ok(())
}

fn validate_reasoning_field(path: &str, field: &str, value: &Json) -> Result<(), SettingsError> {
    match field {
        "enabled" if value.is_boolean() => Ok(()),
        "enabled" => err(format!("setting `{path}` must be true or false")),
        "effort"
            if value
                .as_str()
                .is_some_and(|v| REASONING_EFFORTS.contains(&v)) =>
        {
            Ok(())
        }
        "effort" => err(format!(
            "setting `{path}` must be one of {}",
            REASONING_EFFORTS.join(", ")
        )),
        "budget_tokens" => match value.as_i64() {
            Some(n) if (-1..=10_000_000).contains(&n) => Ok(()),
            _ => err(format!(
                "setting `{path}` must be a whole number from -1 (no limit) up"
            )),
        },
        other => err(format!(
            "unknown setting `reasoning.{other}`; reasoning takes {}",
            REASONING_FIELDS.join(", ")
        )),
    }
}

fn validate_extra_field(field: &str, value: &Json) -> Result<(), SettingsError> {
    if field.is_empty() || field.len() > 128 {
        return err("an `extra` key must be 1 to 128 characters");
    }
    if EXTRA_FORBIDDEN.contains(&field) {
        return err(format!("`extra.{field}` is the request's own structure"));
    }
    if request_key(field).is_some() {
        return err(format!(
            "`extra.{field}` is a typed setting; set `{field}` instead"
        ));
    }
    if value.is_null() {
        return err(format!("`extra.{field}` may not be null"));
    }
    Ok(())
}

/// Validate a settings layer (`defaults`, a profile, a request's own
/// settings). Unknown keys outside `extra` are refused.
pub fn validate_layer(layer: &Map<String, Json>) -> Result<(), SettingsError> {
    for (name, value) in layer {
        let Some(key) = request_key(name) else {
            return err(format!(
                "unknown setting `{name}`; known settings: {}; engine-specific \
                 per-request keys go under `extra`",
                known_keys_hint()
            ));
        };
        validate_value(name, key.kind, value)?;
    }
    Ok(())
}

/// A layer flattened to one value per key path: `reasoning.enabled`,
/// `extra.dry_multiplier`, `temperature`, ...
pub type FlatLayer = BTreeMap<String, Json>;

/// Flatten a validated layer: `reasoning` and `extra` expand one level so
/// layers merge field by field.
pub fn flatten_layer(layer: &Map<String, Json>) -> FlatLayer {
    let mut flat = FlatLayer::new();
    for (name, value) in layer {
        match (request_key(name).map(|key| key.kind), value) {
            (Some(KeyKind::Reasoning | KeyKind::Extra), Json::Object(object)) => {
                for (field, field_value) in object {
                    flat.insert(format!("{name}.{field}"), field_value.clone());
                }
            }
            _ => {
                flat.insert(name.clone(), value.clone());
            }
        }
    }
    flat
}

/// Split a key path into its top key and, for `reasoning`/`extra`, field.
/// `extra.speculative.n_max` splits at the first dot only.
pub fn split_path(path: &str) -> (&str, Option<&str>) {
    match path.split_once('.') {
        Some((top, rest)) => (top, Some(rest)),
        None => (path, None),
    }
}

/// Set one key path in a layer (the CLI's `key=value` and `PATCH`). `null`
/// removes it. The result is validated.
pub fn set_path(
    layer: &mut Map<String, Json>,
    path: &str,
    value: Json,
) -> Result<(), SettingsError> {
    let (top, field) = split_path(path);
    let Some(key) = request_key(top) else {
        return validate_layer(&Map::from_iter([(top.to_string(), Json::Null)]));
    };
    match (key.kind, field) {
        (KeyKind::Reasoning | KeyKind::Extra, Some(field)) => {
            let entry = layer
                .entry(top.to_string())
                .or_insert_with(|| Json::Object(Map::new()));
            let Some(object) = entry.as_object_mut() else {
                return err(format!("setting `{top}` must be an object"));
            };
            if value.is_null() {
                object.remove(field);
                if object.is_empty() {
                    layer.remove(top);
                }
            } else {
                object.insert(field.to_string(), value);
            }
        }
        (_, Some(_)) => return err(format!("setting `{top}` has no fields")),
        (_, None) => {
            if value.is_null() {
                layer.remove(top);
            } else {
                layer.insert(top.to_string(), value);
            }
        }
    }
    validate_layer(layer)
}

/// Turn a response-format setting into the canonical form. Accepts `text`,
/// `json_object`, `{type: text|json_object}`, and `{type: json_schema,
/// json_schema: {name, schema, strict}}` (or `{json_schema: {...}}`).
pub fn response_format_from_setting(value: &Json) -> Result<ResponseFormat, String> {
    let kind = match value {
        Json::String(kind) => kind.as_str(),
        Json::Object(object) => object
            .get("type")
            .and_then(Json::as_str)
            .or_else(|| object.contains_key("json_schema").then_some("json_schema"))
            .ok_or("a response_format object needs `type`")?,
        _ => return Err("must be a string or an object".to_string()),
    };
    match kind {
        "text" => Ok(ResponseFormat::Text),
        "json_object" => Ok(ResponseFormat::JsonObject),
        "json_schema" => {
            let schema = value
                .get("json_schema")
                .and_then(Json::as_object)
                .ok_or("json_schema needs a `json_schema` object")?;
            let name = schema
                .get("name")
                .and_then(Json::as_str)
                .ok_or("json_schema needs a `name`")?;
            Ok(ResponseFormat::JsonSchema {
                name: name.to_string(),
                schema: schema.get("schema").cloned().unwrap_or(Json::Null),
                strict: schema
                    .get("strict")
                    .and_then(Json::as_bool)
                    .unwrap_or(false),
            })
        }
        other => Err(format!(
            "unknown response format `{other}` (text, json_object, json_schema)"
        )),
    }
}

/// A route's or request's choice of profile name must look like a name.
pub fn validate_profile_name(name: &str) -> Result<(), SettingsError> {
    if name.is_empty()
        || name.len() > MAX_PROFILE_NAME
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return err(format!(
            "profile name `{name}` must be 1 to {MAX_PROFILE_NAME} letters, digits, '-', '_' or '.'"
        ));
    }
    Ok(())
}

/// The per-request load policy (variants): which load-time keys a request
/// may ask for, within which bounds, and how many variants may exist.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestLoadPolicy {
    /// Load-time keys a client key may set per request. Operator and admin
    /// keys may set any typed load key (still within `bounds`).
    #[serde(default)]
    pub allow: Vec<String>,
    /// Inclusive `[min, max]` per numeric load key.
    #[serde(default)]
    pub bounds: BTreeMap<String, [f64; 2]>,
    /// Most variants of this target at once (default 2).
    #[serde(default = "default_max_variants")]
    pub max_variants: u32,
    /// A variant unused this long is dropped on the next apply (default 1 h).
    #[serde(default = "default_variant_idle_ttl_secs")]
    pub variant_idle_ttl_secs: u64,
    /// Numbers for the fit check when the engine has no memory measurement
    /// of the model (the same KV math as `sb ai fit`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate: Option<FitEstimate>,
    /// GPU bytes the fit check keeps free for other engines on the same
    /// GPU that the engine does not see (an image engine, ...). Default 0.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub reserve_bytes: u64,
}

fn is_zero_u64(n: &u64) -> bool {
    *n == 0
}

/// What a model needs, for the fit check of a variant when the engine has
/// not measured it: weights, the KV cache shape (`sb ai fit --json` gives
/// `model.kv_layers`, `kv_heads`, `head_dim`, `value_dim`), and the bytes
/// the machine has for it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FitEstimate {
    pub weights_bytes: u64,
    pub budget_bytes: u64,
    pub kv_layers: u64,
    pub kv_heads: u64,
    pub head_dim: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_dim: Option<u64>,
}

impl FitEstimate {
    /// KV cache bytes at `ctx` tokens with the given cache types:
    /// layers × ctx × heads × (head_dim × K bytes + value_dim × V bytes).
    pub fn kv_bytes(&self, ctx: u64, cache_k: &str, cache_v: &str) -> u64 {
        let value_dim = self.value_dim.unwrap_or(self.head_dim) as f64;
        let per_token_head = self.head_dim as f64 * crate::cache_type_bytes(cache_k)
            + value_dim * crate::cache_type_bytes(cache_v);
        (self.kv_layers as f64 * ctx as f64 * self.kv_heads as f64 * per_token_head) as u64
    }
}

fn default_max_variants() -> u32 {
    2
}

fn default_variant_idle_ttl_secs() -> u64 {
    3600
}

/// When a changed load-time setting takes effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyMode {
    /// Written now; a loaded model keeps its old arguments until it next
    /// loads.
    #[default]
    NextLoad,
    /// Written, then the model is unloaded and loaded again once nothing is
    /// in flight on it.
    Now,
}

/// The settings document of one target model (`provider/model`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsDoc {
    /// Request-time settings for every request that reaches the target.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub defaults: Map<String, Json>,
    /// Named sets of request-time settings, layered over `defaults`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, Map<String, Json>>,
    /// Keys a request may not override (`temperature`, `reasoning`,
    /// `reasoning.enabled`, `extra.mirostat`, ...).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locked: Vec<String>,
    /// Load-time settings (context size, GPU layers, KV cache type, ...).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub load: Map<String, Json>,
    /// Raw engine arguments (preset key -> value) for anything `load` does
    /// not type. Operator and admin keys only.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub engine_args: Map<String, Json>,
    /// Per-request load settings (variants). Absent = refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_load: Option<RequestLoadPolicy>,
    /// The model thinks before it answers (a reasoning model). Turns on
    /// what `reasoning_guard` does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reasoning: bool,
    /// For a `reasoning: true` target: a request with a small `max_tokens`
    /// gets thinking off unless it asked for thinking itself. Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_guard: Option<crate::ReasoningGuard>,
    /// The target's speed ceiling on its machine (decode and prefill
    /// tokens per second, bytes read per token), for the speed report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<crate::Ceiling>,
}

impl ModelSettingsDoc {
    pub fn is_empty(&self) -> bool {
        *self == ModelSettingsDoc::default()
    }

    /// Validate the request-time parts. Load-time parts are validated by
    /// [`crate::load_settings::validate_load_layer`] where the engine kind
    /// is known; [`Self::validate`] calls the engine-neutral checks.
    pub fn validate_request_parts(&self) -> Result<(), SettingsError> {
        validate_layer(&self.defaults).map_err(|e| SettingsError(format!("defaults: {e}")))?;
        if self.profiles.len() > MAX_PROFILES {
            return err(format!("at most {MAX_PROFILES} profiles"));
        }
        for (name, layer) in &self.profiles {
            validate_profile_name(name)?;
            validate_layer(layer).map_err(|e| SettingsError(format!("profile `{name}`: {e}")))?;
        }
        for path in &self.locked {
            // `load.KEY`: a load-time key `sb ai tune` must never change.
            if let Some(load) = path.strip_prefix("load.") {
                if crate::load_settings::load_key(load).is_none() {
                    return err(format!(
                        "locked key `{path}`: `{load}` is not a load setting"
                    ));
                }
                continue;
            }
            let (top, field) = split_path(path);
            let Some(key) = request_key(top) else {
                return err(format!(
                    "locked key `{path}` is not a setting; known settings: {}",
                    known_keys_hint()
                ));
            };
            match (key.kind, field) {
                (KeyKind::Reasoning, Some(field)) if !REASONING_FIELDS.contains(&field) => {
                    return err(format!("locked key `{path}`: reasoning has no `{field}`"));
                }
                (KeyKind::Reasoning | KeyKind::Extra, _) | (_, None) => {}
                (_, Some(_)) => return err(format!("locked key `{path}`: `{top}` has no fields")),
            }
        }
        Ok(())
    }

    /// Validate the whole document (request-time and load-time parts).
    pub fn validate(&self) -> Result<(), SettingsError> {
        self.validate_request_parts()?;
        crate::load_settings::validate_load_layer(&self.load)
            .map_err(|e| SettingsError(format!("load: {e}")))?;
        crate::load_settings::validate_engine_args(&self.engine_args)
            .map_err(|e| SettingsError(format!("engine_args: {e}")))?;
        if let Some(policy) = &self.request_load {
            crate::load_settings::validate_request_load_policy(policy)
                .map_err(|e| SettingsError(format!("request_load: {e}")))?;
        }
        if let Some(guard) = &self.reasoning_guard {
            if guard.min_tokens == 0 || guard.min_tokens > 1_000_000 {
                return err("reasoning_guard.min_tokens must be 1 to 1000000");
            }
        }
        if let Some(ceiling) = &self.ceiling {
            ceiling.validate().map_err(SettingsError)?;
        }
        Ok(())
    }

    /// The reasoning guard in force: the target thinks and its guard is on.
    pub fn active_reasoning_guard(&self) -> Option<&crate::ReasoningGuard> {
        self.reasoning_guard
            .as_ref()
            .filter(|guard| self.reasoning && guard.enabled)
    }

    /// Whether `path` (or the key it belongs to) is locked.
    pub fn is_locked(&self, path: &str) -> bool {
        let (top, _) = split_path(path);
        self.locked
            .iter()
            .any(|locked| locked == path || locked == top)
    }

    pub fn profile_names(&self) -> Vec<String> {
        self.profiles.keys().cloned().collect()
    }

    /// Whether the document has any request-time setting at all.
    pub fn has_request_settings(&self) -> bool {
        !self.defaults.is_empty() || !self.profiles.is_empty()
    }
}

/// Apply a JSON merge patch (RFC 7386) to a settings document: an object
/// merges key by key, `null` removes a key, anything else replaces.
pub fn merge_patch(target: &mut Json, patch: &Json) {
    match patch {
        Json::Object(patch) => {
            if !target.is_object() {
                *target = Json::Object(Map::new());
            }
            let Some(object) = target.as_object_mut() else {
                return;
            };
            for (key, value) in patch {
                if value.is_null() {
                    object.remove(key);
                } else {
                    merge_patch(object.entry(key.clone()).or_insert(Json::Null), value);
                }
            }
        }
        other => *target = other.clone(),
    }
}

/// What one request asked of the settings layer: the `switchback` body
/// object plus the [`SETTINGS_PROFILE_HEADER`] header.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Request-time settings for this request only (same schema as
    /// `defaults`).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub settings: Map<String, Json>,
    /// Load-time settings for this request (variants).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub load: Map<String, Json>,
}

impl RequestSettings {
    pub fn is_empty(&self) -> bool {
        self.profile.is_none() && self.settings.is_empty() && self.load.is_empty()
    }
}

/// Take the `switchback` object out of a request (from the OpenAI-shaped
/// passthrough and from every protocol-scoped passthrough) and the profile
/// header out of its metadata, so none of it can reach an upstream. The
/// body's profile wins over the header's. Returns `None` when the request
/// asked nothing.
pub fn take_request_settings(
    req: &mut AiRequest,
) -> Result<Option<RequestSettings>, SettingsError> {
    let mut bodies = Vec::new();
    if let Some(value) = req.passthrough.remove(SWITCHBACK_BODY_KEY) {
        bodies.push(value);
    }
    for passthrough in req.protocol_passthrough.values_mut() {
        if let Some(value) = passthrough.remove(SWITCHBACK_BODY_KEY) {
            bodies.push(value);
        }
    }
    let header_profile = req.metadata.remove(SETTINGS_PROFILE_METADATA_KEY);
    let mut out = RequestSettings::default();
    for body in bodies {
        let Json::Object(object) = body else {
            return err("`switchback` must be an object {profile, settings, load}");
        };
        for (key, value) in object {
            match key.as_str() {
                "profile" => match value {
                    Json::String(name) => out.profile = Some(name),
                    Json::Null => {}
                    _ => return err("`switchback.profile` must be a string"),
                },
                "settings" => match value {
                    Json::Object(settings) => {
                        validate_layer(&settings)
                            .map_err(|e| SettingsError(format!("switchback.settings: {e}")))?;
                        out.settings.extend(settings);
                    }
                    Json::Null => {}
                    _ => return err("`switchback.settings` must be an object"),
                },
                "load" => match value {
                    Json::Object(load) => out.load.extend(load),
                    Json::Null => {}
                    _ => return err("`switchback.load` must be an object"),
                },
                other => {
                    return err(format!(
                    "unknown key `switchback.{other}`; `switchback` takes profile, settings, load"
                ))
                }
            }
        }
    }
    if out.profile.is_none() {
        out.profile = header_profile.filter(|name| !name.is_empty());
    }
    if let Some(profile) = &out.profile {
        validate_profile_name(profile)?;
    }
    Ok((!out.is_empty()).then_some(out))
}

/// Where an effective value came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum SettingSource {
    /// The request's `switchback.settings`.
    Request,
    /// The profile the request asked for.
    RequestProfile(String),
    /// The route's profile.
    RouteProfile(String),
    /// The target's `defaults`.
    Defaults,
}

impl SettingSource {
    pub fn label(&self) -> String {
        match self {
            SettingSource::Request => "request".to_string(),
            SettingSource::RequestProfile(name) => format!("profile:{name}"),
            SettingSource::RouteProfile(name) => format!("route_profile:{name}"),
            SettingSource::Defaults => "defaults".to_string(),
        }
    }
}

/// The layered request-time values of one target for one request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayeredSettings {
    pub values: BTreeMap<String, (Json, SettingSource)>,
    /// The request's profile, when this target has it.
    pub profile: Option<String>,
    /// The route's profile, when this target has it.
    pub route_profile: Option<String>,
}

/// Layer a target's settings for one request: `defaults` < route profile <
/// request profile < the request's own `switchback.settings` (a locked key's
/// request value is dropped). A profile the target lacks is skipped; the
/// caller decides whether that is an error (see [`profile_known`]).
pub fn layer_settings(
    doc: Option<&ModelSettingsDoc>,
    route_profile: Option<&str>,
    request: Option<&RequestSettings>,
) -> LayeredSettings {
    let mut out = LayeredSettings::default();
    if let Some(doc) = doc {
        for (path, value) in flatten_layer(&doc.defaults) {
            out.values.insert(path, (value, SettingSource::Defaults));
        }
        if let Some((name, layer)) = route_profile.and_then(|name| doc.profiles.get_key_value(name))
        {
            out.route_profile = Some(name.clone());
            for (path, value) in flatten_layer(layer) {
                out.values
                    .insert(path, (value, SettingSource::RouteProfile(name.clone())));
            }
        }
        if let Some((name, layer)) = request
            .and_then(|request| request.profile.as_deref())
            .and_then(|name| doc.profiles.get_key_value(name))
        {
            out.profile = Some(name.clone());
            for (path, value) in flatten_layer(layer) {
                out.values
                    .insert(path, (value, SettingSource::RequestProfile(name.clone())));
            }
        }
    }
    if let Some(request) = request {
        for (path, value) in flatten_layer(&request.settings) {
            if doc.is_some_and(|doc| doc.is_locked(&path)) {
                continue;
            }
            out.values.insert(path, (value, SettingSource::Request));
        }
    }
    out
}

/// What applying settings to one request did, for the decision, the trace
/// and the [`SETTINGS_RESPONSE_HEADER`] header. Metadata only: key names,
/// never values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedSettings {
    pub profile: Option<String>,
    pub route_profile: Option<String>,
    /// Keys whose value the settings set (sorted).
    pub applied: Vec<String>,
    /// Keys where a lock replaced or dropped the request's own value.
    pub locked: Vec<String>,
    /// Keys the engine cannot take per request (e.g. llama.cpp's reasoning
    /// budget), left out.
    pub skipped: Vec<String>,
}

impl AppliedSettings {
    pub fn is_empty(&self) -> bool {
        self.profile.is_none()
            && self.route_profile.is_none()
            && self.applied.is_empty()
            && self.locked.is_empty()
            && self.skipped.is_empty()
    }

    /// `profile=nothink; locked=temperature,top_k; applied=3`.
    pub fn header_value(&self) -> String {
        let mut parts = Vec::new();
        if let Some(profile) = &self.profile {
            parts.push(format!("profile={profile}"));
        }
        if let Some(profile) = &self.route_profile {
            parts.push(format!("route_profile={profile}"));
        }
        if !self.locked.is_empty() {
            parts.push(format!("locked={}", self.locked.join(",")));
        }
        if !self.skipped.is_empty() {
            parts.push(format!("skipped={}", self.skipped.join(",")));
        }
        parts.push(format!("applied={}", self.applied.len()));
        parts.join("; ")
    }
}

/// Whether the request already carries its own value for a key path.
fn request_has(req: &AiRequest, path: &str) -> bool {
    let (top, field) = split_path(path);
    match (top, field) {
        ("temperature", None) => req.temperature.is_some() || req.passthrough.contains_key(top),
        ("max_tokens", None) => {
            req.max_output_tokens.is_some()
                || req.passthrough.contains_key("max_tokens")
                || req.passthrough.contains_key("max_completion_tokens")
        }
        ("response_format", None) => {
            req.response_format.is_some() || req.passthrough.contains_key(top)
        }
        ("system_prompt" | "system_prompt_mode", None) => false,
        ("reasoning", Some("enabled" | "effort")) => {
            req.passthrough.contains_key("reasoning_effort")
                || req
                    .passthrough
                    .get("chat_template_kwargs")
                    .and_then(Json::as_object)
                    .is_some_and(|kwargs| kwargs.contains_key("enable_thinking"))
        }
        ("reasoning", Some(_)) => false,
        ("extra", Some(field)) => req.passthrough.contains_key(field),
        (top, _) => req.passthrough.contains_key(top),
    }
}

/// Drop the request's own value for a key path (a lock is about to set it).
fn clear_request_value(req: &mut AiRequest, path: &str) {
    let (top, field) = split_path(path);
    match (top, field) {
        ("temperature", None) => {
            req.temperature = None;
            req.passthrough.remove(top);
        }
        ("max_tokens", None) => {
            req.max_output_tokens = None;
            req.passthrough.remove("max_tokens");
            req.passthrough.remove("max_completion_tokens");
        }
        ("response_format", None) => {
            req.response_format = None;
            req.passthrough.remove(top);
        }
        ("reasoning", Some("enabled" | "effort")) => {
            req.passthrough.remove("reasoning_effort");
            if let Some(kwargs) = req
                .passthrough
                .get_mut("chat_template_kwargs")
                .and_then(Json::as_object_mut)
            {
                kwargs.remove("enable_thinking");
                kwargs.remove("reasoning_effort");
            }
        }
        ("extra", Some(field)) => {
            req.passthrough.remove(field);
        }
        (top, None) => {
            req.passthrough.remove(top);
        }
        _ => {}
    }
}

fn system_text_present(req: &AiRequest) -> bool {
    req.system.as_deref().is_some_and(|s| !s.is_empty())
        || req
            .messages
            .iter()
            .any(|message| matches!(message.role, Role::System))
}

fn apply_system_prompt(req: &mut AiRequest, prompt: &str, mode: &str) {
    if mode == "replace_if_absent" {
        if !system_text_present(req) {
            req.system = Some(prompt.to_string());
        }
        return;
    }
    // prepend
    match &mut req.system {
        Some(existing) if !existing.is_empty() => {
            *existing = format!("{prompt}\n\n{existing}");
        }
        _ => {
            if let Some(message) = req
                .messages
                .iter_mut()
                .find(|message| matches!(message.role, Role::System))
            {
                message.content.insert(
                    0,
                    ContentPart::Text {
                        text: format!("{prompt}\n\n"),
                    },
                );
            } else {
                req.system = Some(prompt.to_string());
            }
        }
    }
}

/// The engine family applying the settings, for the few keys an engine
/// cannot take per request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsEngine {
    /// Any OpenAI-shaped upstream with no named engine.
    Generic,
    LlamaCpp,
    Vllm,
    Ollama,
}

/// Apply layered settings to a request bound for one target. A key the
/// request already carries keeps the request's value unless the key is
/// locked; a key nobody set is not touched. Returns what was done.
pub fn apply_layered(
    req: &mut AiRequest,
    doc: Option<&ModelSettingsDoc>,
    layered: &LayeredSettings,
    engine: SettingsEngine,
) -> AppliedSettings {
    let mut applied = AppliedSettings {
        profile: layered.profile.clone(),
        route_profile: layered.route_profile.clone(),
        ..AppliedSettings::default()
    };
    let locked = |path: &str| doc.is_some_and(|doc| doc.is_locked(path));
    let mut reasoning: Map<String, Json> = Map::new();
    let mut reasoning_locked = false;
    let mut reasoning_from_request_layer = false;
    let mut system_prompt: Option<String> = None;
    let mut system_mode = "prepend".to_string();

    for (path, (value, source)) in &layered.values {
        let (top, field) = split_path(path);
        let is_locked = locked(path);
        let from_request = *source == SettingSource::Request;
        match (top, field) {
            ("system_prompt", None) => {
                system_prompt = value.as_str().map(str::to_string);
                continue;
            }
            ("system_prompt_mode", None) => {
                if let Some(mode) = value.as_str() {
                    system_mode = mode.to_string();
                }
                continue;
            }
            ("reasoning", Some(field)) => {
                reasoning.insert(field.to_string(), value.clone());
                reasoning_locked |= is_locked;
                reasoning_from_request_layer |= from_request;
                continue;
            }
            _ => {}
        }
        // vLLM reads `repetition_penalty`: a `repeat_penalty` the settings
        // set goes out under that name, and a request's own
        // `repetition_penalty` counts as its value for the key (a lock
        // replaces it). Nothing is renamed that the settings did not set.
        let vllm_name = (engine == SettingsEngine::Vllm && path == "repeat_penalty")
            .then_some("repetition_penalty");
        let request_owns = request_has(req, path)
            || vllm_name.is_some_and(|name| req.passthrough.contains_key(name));
        if request_owns {
            if is_locked {
                clear_request_value(req, path);
                applied.locked.push(path.clone());
            } else if !from_request {
                continue; // the request's own value wins
            } else {
                clear_request_value(req, path);
            }
            if let Some(name) = vllm_name {
                req.passthrough.remove(name);
            }
        }
        match vllm_name {
            Some(name) => {
                req.passthrough.insert(name.to_string(), value.clone());
            }
            None => set_value(req, path, value),
        }
        applied.applied.push(path.clone());
    }

    if let Some(prompt) = system_prompt {
        apply_system_prompt(req, &prompt, &system_mode);
        applied.applied.push("system_prompt".to_string());
    }

    if !reasoning.is_empty() {
        let request_owns = request_has(req, "reasoning.enabled");
        let take = !request_owns || reasoning_locked || reasoning_from_request_layer;
        if take {
            if request_owns {
                clear_request_value(req, "reasoning.enabled");
                if reasoning_locked {
                    applied.locked.push("reasoning".to_string());
                }
            }
            apply_reasoning(req, &reasoning, engine, &mut applied);
        }
    }
    applied.applied.sort();
    applied.applied.dedup();
    applied.locked.sort();
    applied.locked.dedup();
    applied
}

fn apply_reasoning(
    req: &mut AiRequest,
    reasoning: &Map<String, Json>,
    engine: SettingsEngine,
    applied: &mut AppliedSettings,
) {
    let enabled = reasoning.get("enabled").and_then(Json::as_bool);
    let effort = reasoning.get("effort").and_then(Json::as_str);
    let effort = match (enabled, effort) {
        (Some(false), _) | (_, Some("none")) => Some("none"),
        (_, Some(effort)) => Some(effort),
        (Some(true), None) => None,
        (None, None) => None,
    };
    match effort {
        Some(effort) => {
            req.passthrough
                .insert("reasoning_effort".to_string(), Json::from(effort));
            applied.applied.push("reasoning.effort".to_string());
        }
        None if enabled == Some(true) => {
            req.metadata.insert(
                REASONING_ENABLE_METADATA_KEY.to_string(),
                "true".to_string(),
            );
            applied.applied.push("reasoning.enabled".to_string());
        }
        None => {}
    }
    if let Some(budget) = reasoning.get("budget_tokens") {
        match engine {
            // llama.cpp takes the thinking budget at load time only
            // (`reasoning_budget` under `load`).
            SettingsEngine::LlamaCpp | SettingsEngine::Ollama | SettingsEngine::Generic => {
                applied.skipped.push("reasoning.budget_tokens".to_string());
            }
            SettingsEngine::Vllm => {
                req.passthrough
                    .insert("thinking_token_budget".to_string(), budget.clone());
                applied.applied.push("reasoning.budget_tokens".to_string());
            }
        }
    }
}

/// Write one key path into the request.
fn set_value(req: &mut AiRequest, path: &str, value: &Json) {
    let (top, field) = split_path(path);
    match (top, field) {
        ("max_tokens", None) => {
            if let Some(n) = value.as_u64().and_then(|n| u32::try_from(n).ok()) {
                req.max_output_tokens = Some(n);
            }
        }
        ("response_format", None) => {
            if let Ok(format) = response_format_from_setting(value) {
                req.response_format = Some(format);
            }
        }
        ("extra", Some(field)) => {
            req.passthrough.insert(field.to_string(), value.clone());
        }
        // `temperature` goes through the passthrough so the exact number
        // reaches the engine (the typed field is f32).
        (top, None) => {
            req.passthrough.insert(top.to_string(), value.clone());
        }
        _ => {}
    }
}

/// The names of every profile across documents, for an "unknown profile"
/// error.
pub fn profile_known<'a>(
    docs: impl IntoIterator<Item = Option<&'a ModelSettingsDoc>>,
    profile: &str,
) -> (bool, BTreeSet<String>) {
    let mut names = BTreeSet::new();
    let mut known = false;
    for doc in docs.into_iter().flatten() {
        known |= doc.profiles.contains_key(profile);
        names.extend(doc.profiles.keys().cloned());
    }
    (known, names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Message;
    use serde_json::json;

    fn doc(value: Json) -> ModelSettingsDoc {
        let doc: ModelSettingsDoc = serde_json::from_value(value).unwrap();
        doc.validate().unwrap();
        doc
    }

    fn req() -> AiRequest {
        AiRequest::new("text/m", vec![Message::user("hi")])
    }

    fn request(value: Json) -> RequestSettings {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn unknown_key_is_refused_with_the_list() {
        let bad: ModelSettingsDoc =
            serde_json::from_value(json!({"defaults": {"tempreature": 0.5}})).unwrap();
        let e = bad.validate().unwrap_err().0;
        assert!(e.contains("unknown setting `tempreature`"), "{e}");
        assert!(e.contains("extra"), "{e}");
    }

    #[test]
    fn ranges_and_types_are_checked() {
        for (layer, needle) in [
            (json!({"temperature": 9.0}), "out of range"),
            (json!({"top_k": "x"}), "whole number"),
            (json!({"stop": [1]}), "string"),
            (json!({"reasoning": {"effort": "huge"}}), "must be one of"),
            (json!({"reasoning": {"other": 1}}), "reasoning.other"),
            (json!({"extra": {"temperature": 1}}), "typed setting"),
            (json!({"extra": {"messages": []}}), "own structure"),
            (json!({"system_prompt_mode": "append"}), "must be one of"),
        ] {
            let e = validate_layer(layer.as_object().unwrap()).unwrap_err().0;
            assert!(e.contains(needle), "{layer}: {e}");
        }
    }

    #[test]
    fn unknown_document_field_is_refused() {
        assert!(serde_json::from_value::<ModelSettingsDoc>(json!({"defualts": {}})).is_err());
    }

    #[test]
    fn precedence_request_over_request_profile_over_route_profile_over_defaults() {
        let doc = doc(json!({
            "defaults": {"temperature": 0.1, "top_k": 10, "min_p": 0.01, "seed": 1},
            "profiles": {
                "route": {"temperature": 0.2, "top_k": 20, "min_p": 0.02},
                "asked": {"temperature": 0.3, "top_k": 30}
            }
        }));
        let request = request(json!({"profile": "asked", "settings": {"temperature": 0.4}}));
        let layered = layer_settings(Some(&doc), Some("route"), Some(&request));
        let value = |k: &str| layered.values[k].clone();
        assert_eq!(value("temperature"), (json!(0.4), SettingSource::Request));
        assert_eq!(
            value("top_k"),
            (json!(30), SettingSource::RequestProfile("asked".into()))
        );
        assert_eq!(
            value("min_p"),
            (json!(0.02), SettingSource::RouteProfile("route".into()))
        );
        assert_eq!(value("seed"), (json!(1), SettingSource::Defaults));
    }

    #[test]
    fn request_plain_value_wins_unless_locked() {
        let doc = doc(json!({
            "defaults": {"temperature": 0.6, "top_k": 20},
            "locked": ["top_k"]
        }));
        let mut r = req();
        r.temperature = Some(0.9);
        r.passthrough.insert("top_k".into(), json!(99));
        let layered = layer_settings(Some(&doc), None, None);
        let applied = apply_layered(&mut r, Some(&doc), &layered, SettingsEngine::LlamaCpp);
        assert_eq!(r.temperature, Some(0.9), "request's own temperature kept");
        assert!(!r.passthrough.contains_key("temperature"));
        assert_eq!(r.passthrough["top_k"], json!(20), "lock wins");
        assert_eq!(applied.locked, vec!["top_k".to_string()]);
        assert_eq!(applied.header_value(), "locked=top_k; applied=1");
    }

    #[test]
    fn locked_key_drops_the_request_settings_value() {
        let doc = doc(json!({"defaults": {"temperature": 0.6}, "locked": ["temperature"]}));
        let request = request(json!({"settings": {"temperature": 1.5}}));
        let layered = layer_settings(Some(&doc), None, Some(&request));
        assert_eq!(layered.values["temperature"].0, json!(0.6));
    }

    #[test]
    fn nothing_set_touches_nothing() {
        let mut r = req();
        r.passthrough.insert("top_p".into(), json!(0.5));
        let before = serde_json::to_value(&r).unwrap();
        let layered = layer_settings(None, None, None);
        let applied = apply_layered(&mut r, None, &layered, SettingsEngine::LlamaCpp);
        assert!(applied.is_empty());
        assert_eq!(serde_json::to_value(&r).unwrap(), before);
    }

    #[test]
    fn reasoning_translates_to_effort_or_marker() {
        let off = doc(json!({"defaults": {"reasoning": {"enabled": false}}}));
        let mut r = req();
        apply_layered(
            &mut r,
            Some(&off),
            &layer_settings(Some(&off), None, None),
            SettingsEngine::LlamaCpp,
        );
        assert_eq!(r.passthrough["reasoning_effort"], json!("none"));

        let on = doc(json!({"defaults": {"reasoning": {"enabled": true, "budget_tokens": 512}}}));
        let mut r = req();
        let applied = apply_layered(
            &mut r,
            Some(&on),
            &layer_settings(Some(&on), None, None),
            SettingsEngine::LlamaCpp,
        );
        assert_eq!(r.metadata[REASONING_ENABLE_METADATA_KEY], "true");
        assert!(!r.passthrough.contains_key("reasoning_effort"));
        assert_eq!(applied.skipped, vec!["reasoning.budget_tokens".to_string()]);

        // The request's own reasoning_effort wins over a default.
        let mut r = req();
        r.passthrough
            .insert("reasoning_effort".into(), json!("high"));
        apply_layered(
            &mut r,
            Some(&off),
            &layer_settings(Some(&off), None, None),
            SettingsEngine::LlamaCpp,
        );
        assert_eq!(r.passthrough["reasoning_effort"], json!("high"));
    }

    #[test]
    fn system_prompt_modes() {
        let pre = doc(json!({"defaults": {"system_prompt": "Be brief."}}));
        let mut r = req();
        r.system = Some("You help.".into());
        apply_layered(
            &mut r,
            Some(&pre),
            &layer_settings(Some(&pre), None, None),
            SettingsEngine::Generic,
        );
        assert_eq!(r.system.as_deref(), Some("Be brief.\n\nYou help."));

        let absent = doc(json!({"defaults": {
            "system_prompt": "Be brief.", "system_prompt_mode": "replace_if_absent"}}));
        let mut r = req();
        r.system = Some("You help.".into());
        apply_layered(
            &mut r,
            Some(&absent),
            &layer_settings(Some(&absent), None, None),
            SettingsEngine::Generic,
        );
        assert_eq!(r.system.as_deref(), Some("You help."));
        let mut r = req();
        apply_layered(
            &mut r,
            Some(&absent),
            &layer_settings(Some(&absent), None, None),
            SettingsEngine::Generic,
        );
        assert_eq!(r.system.as_deref(), Some("Be brief."));
    }

    #[test]
    fn extra_keys_pass_as_they_are() {
        let d =
            doc(json!({"defaults": {"extra": {"dry_multiplier": 0.8, "speculative.n_max": 8}}}));
        let mut r = req();
        apply_layered(
            &mut r,
            Some(&d),
            &layer_settings(Some(&d), None, None),
            SettingsEngine::LlamaCpp,
        );
        assert_eq!(r.passthrough["dry_multiplier"], json!(0.8));
        assert_eq!(r.passthrough["speculative.n_max"], json!(8));
    }

    #[test]
    fn take_request_settings_strips_and_reads_header() {
        let mut r = req();
        r.passthrough.insert(
            "switchback".into(),
            json!({"settings": {"temperature": 0.2}}),
        );
        r.metadata
            .insert(SETTINGS_PROFILE_METADATA_KEY.into(), "nothink".into());
        let taken = take_request_settings(&mut r).unwrap().unwrap();
        assert!(!r.passthrough.contains_key("switchback"));
        assert!(!r.metadata.contains_key(SETTINGS_PROFILE_METADATA_KEY));
        assert_eq!(taken.profile.as_deref(), Some("nothink"));
        assert_eq!(taken.settings["temperature"], json!(0.2));

        let mut r = req();
        r.passthrough
            .insert("switchback".into(), json!({"settngs": {}}));
        assert!(take_request_settings(&mut r).is_err());
        assert!(
            !r.passthrough.contains_key("switchback"),
            "stripped even on error"
        );
    }

    #[test]
    fn set_path_and_merge_patch() {
        let mut layer = Map::new();
        set_path(&mut layer, "reasoning.enabled", json!(false)).unwrap();
        set_path(&mut layer, "extra.speculative.n_max", json!(4)).unwrap();
        set_path(&mut layer, "temperature", json!(0.6)).unwrap();
        assert_eq!(
            Json::Object(layer.clone()),
            json!({"reasoning": {"enabled": false}, "extra": {"speculative.n_max": 4}, "temperature": 0.6})
        );
        set_path(&mut layer, "reasoning.enabled", Json::Null).unwrap();
        assert!(!layer.contains_key("reasoning"));
        assert!(set_path(&mut layer, "bogus", json!(1)).is_err());

        let mut target = json!({"defaults": {"temperature": 0.6, "top_k": 20}});
        merge_patch(
            &mut target,
            &json!({"defaults": {"top_k": null, "min_p": 0.05}}),
        );
        assert_eq!(
            target,
            json!({"defaults": {"temperature": 0.6, "min_p": 0.05}})
        );
    }
}
