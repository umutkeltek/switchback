//! Engine control: the load-time side of model settings for local text
//! engines. It loads and unloads models, applies their load-time settings,
//! and reports what each engine has loaded (`GET /v1/engines`).
//!
//! The control is engine-agnostic at its seams ([`EngineKindOps`] maps
//! load settings to an engine's own arguments and checks raw ones; the
//! HTTP calls are per kind). `llama_cpp_router` is implemented: llama.cpp
//! `llama-server` in router mode, whose per-model arguments come from a
//! presets INI. vLLM, Ollama and LM Studio slot in as further kinds.
//!
//! The gateway never writes files on a machine. A lane's
//! `engine.settings_command` gets the managed overlay (JSON on stdin) and
//! writes the presets however the operator wants (the reference script is
//! `contrib/llamacpp-preset-overlay.py`); the gateway then asks the engine
//! to re-read them.
//!
//! Safety: reads never wake a machine; only `load` and `apply: now` with
//! `wake: true` may. Unload and `apply: now` wait for the model's
//! in-flight requests (bounded) and answer 409 instead of interrupting
//! one. Nothing here powers a machine off.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use sb_core::{
    load_layer_paths, ApplyMode, Config, EngineControlKind, LaneEngineConfig, ModelSettingsDoc,
};
use sb_runtime::model_settings::SettingsEntry;

use crate::handlers::model_settings::{error, resolve_one, NamedTarget};
use crate::tenancy::Principal;
use crate::AppState;

/// Every long argument name of `llama-server` (from its README's argument
/// tables). Raw `engine_args` keys must be one of these (or a preset-only
/// key).
pub(crate) const LLAMA_SERVER_ARGS: &[&str] = &[
    "adaptive-decay",
    "adaptive-target",
    "agent",
    "alias",
    "allow-unauthenticated-tools",
    "api-key",
    "api-key-file",
    "api-prefix",
    "backend-sampling",
    "batch-size",
    "cache-idle-slots",
    "cache-list",
    "cache-prompt",
    "cache-ram",
    "cache-reuse",
    "cache-type-k",
    "cache-type-k-draft",
    "cache-type-v",
    "cache-type-v-draft",
    "chat-template",
    "chat-template-file",
    "chat-template-kwargs",
    "check-tensors",
    "checkpoint-min-step",
    "completion-bash",
    "connect",
    "connect-code",
    "cont-batching",
    "context-shift",
    "control-vector",
    "control-vector-layer-range",
    "control-vector-scaled",
    "cors-credentials",
    "cors-headers",
    "cors-methods",
    "cors-origins",
    "cpu-mask",
    "cpu-mask-batch",
    "cpu-mask-batch-draft",
    "cpu-mask-draft",
    "cpu-moe",
    "cpu-moe-draft",
    "cpu-range",
    "cpu-range-batch",
    "cpu-range-draft",
    "cpu-strict",
    "cpu-strict-batch",
    "cpu-strict-batch-draft",
    "cpu-strict-draft",
    "ctx-checkpoints",
    "ctx-size",
    "device",
    "device-draft",
    "docker-repo",
    "draft",
    "draft-max",
    "draft-min",
    "draft-n",
    "draft-n-min",
    "draft-p-min",
    "draft-p-split",
    "dry-allowed-length",
    "dry-base",
    "dry-multiplier",
    "dry-penalty-last-n",
    "dry-sequence-breaker",
    "dynatemp-exp",
    "dynatemp-range",
    "embd-gemma-default",
    "embd-normalize",
    "embedding",
    "embeddings",
    "escape",
    "fim-qwen-1",
    "fim-qwen-14b-spec",
    "fim-qwen-30b-default",
    "fim-qwen-3b-default",
    "fim-qwen-7b-default",
    "fim-qwen-7b-spec",
    "fit",
    "fit-ctx",
    "fit-target",
    "flash-attn",
    "frequency-penalty",
    "gpt-oss-120b-default",
    "gpt-oss-20b-default",
    "gpu-layers",
    "gpu-layers-draft",
    "grammar",
    "grammar-file",
    "help",
    "hf-file",
    "hf-repo",
    "hf-repo-draft",
    "hf-token",
    "host",
    "ignore-eos",
    "image-max-tokens",
    "image-min-tokens",
    "jinja",
    "json-schema",
    "json-schema-file",
    "keep",
    "kv-offload",
    "kv-unified",
    "kv-unified-per-slot",
    "lazy-mode",
    "list-devices",
    "load-mode",
    "log-colors",
    "log-disable",
    "log-file",
    "log-jsonl",
    "log-prefix",
    "log-prompts-dir",
    "log-timestamps",
    "log-verbose",
    "log-verbosity",
    "logit-bias",
    "lookup-cache-dynamic",
    "lookup-cache-static",
    "lora",
    "lora-init-without-apply",
    "lora-scaled",
    "main-gpu",
    "mcp-servers-config",
    "mcp-servers-json",
    "media-allow-private-urls",
    "media-path",
    "metrics",
    "min-p",
    "mirostat",
    "mirostat-ent",
    "mirostat-lr",
    "mmproj",
    "mmproj-auto",
    "mmproj-device",
    "mmproj-offload",
    "mmproj-url",
    "model",
    "model-draft",
    "model-url",
    "models-autoload",
    "models-dir",
    "models-max",
    "models-memory-margin",
    "models-preset",
    "moe-cache-cpu-share",
    "moe-cache-mib",
    "mtmd-batch-max-tokens",
    "n-cpu-ffn",
    "n-cpu-moe",
    "n-cpu-moe-draft",
    "n-gpu-layers",
    "n-gpu-layers-draft",
    "n-predict",
    "no-agent",
    "no-cache-idle-slots",
    "no-cache-prompt",
    "no-cont-batching",
    "no-context-shift",
    "no-cors-credentials",
    "no-escape",
    "no-host",
    "no-jinja",
    "no-kv-offload",
    "no-kv-unified",
    "no-log-jsonl",
    "no-log-prefix",
    "no-log-timestamps",
    "no-mmproj",
    "no-mmproj-auto",
    "no-mmproj-offload",
    "no-models-autoload",
    "no-op-offload",
    "no-perf",
    "no-prefill-assistant",
    "no-reasoning-preserve",
    "no-repack",
    "no-skip-chat-parsing",
    "no-slots",
    "no-spec-draft-backend-sampling",
    "no-ui",
    "no-ui-mcp-proxy",
    "no-warmup",
    "no-webui",
    "no-webui-mcp-proxy",
    "numa",
    "offline",
    "op-offload",
    "override-kv",
    "override-tensor",
    "override-tensor-draft",
    "parallel",
    "path",
    "perf",
    "ple",
    "poll",
    "poll-batch",
    "poll-batch-draft",
    "poll-draft",
    "pooling",
    "port",
    "predict",
    "prefill-assistant",
    "presence-penalty",
    "prio",
    "prio-batch",
    "prio-batch-draft",
    "prio-draft",
    "props",
    "reasoning",
    "reasoning-budget",
    "reasoning-budget-message",
    "reasoning-effort",
    "reasoning-format",
    "reasoning-preserve",
    "repack",
    "repeat-last-n",
    "repeat-penalty",
    "rerank",
    "reranking",
    "reuse-port",
    "reverse-prompt",
    "rope-freq-base",
    "rope-freq-scale",
    "rope-scale",
    "rope-scaling",
    "rpc",
    "sampler-seq",
    "samplers",
    "sampling-seq",
    "seed",
    "shard",
    "skip-chat-parsing",
    "sleep-idle-seconds",
    "slot-prompt-similarity",
    "slot-save-path",
    "slots",
    "spec-default",
    "spec-draft-backend-sampling",
    "spec-draft-cpu-mask",
    "spec-draft-cpu-mask-batch",
    "spec-draft-cpu-moe",
    "spec-draft-cpu-range",
    "spec-draft-cpu-strict",
    "spec-draft-cpu-strict-batch",
    "spec-draft-device",
    "spec-draft-hf",
    "spec-draft-model",
    "spec-draft-n-cpu-moe",
    "spec-draft-n-max",
    "spec-draft-n-min",
    "spec-draft-n-ubatch",
    "spec-draft-ncmoe",
    "spec-draft-ngl",
    "spec-draft-override-tensor",
    "spec-draft-p-min",
    "spec-draft-p-split",
    "spec-draft-poll",
    "spec-draft-poll-batch",
    "spec-draft-prio",
    "spec-draft-prio-batch",
    "spec-draft-sampling",
    "spec-draft-threads",
    "spec-draft-threads-batch",
    "spec-draft-type-k",
    "spec-draft-type-v",
    "spec-ngram-map-k-min-hits",
    "spec-ngram-map-k-size-m",
    "spec-ngram-map-k-size-n",
    "spec-ngram-map-k4v-min-hits",
    "spec-ngram-map-k4v-size-m",
    "spec-ngram-map-k4v-size-n",
    "spec-ngram-min-hits",
    "spec-ngram-mod-n-match",
    "spec-ngram-mod-n-max",
    "spec-ngram-mod-n-min",
    "spec-ngram-simple-min-hits",
    "spec-ngram-simple-size-m",
    "spec-ngram-simple-size-n",
    "spec-ngram-size-m",
    "spec-ngram-size-n",
    "spec-synth-len",
    "spec-synth-rates",
    "spec-type",
    "special",
    "split-mode",
    "spm-infill",
    "sse-ping-interval",
    "ssl-cert-file",
    "ssl-key-file",
    "swa-checkpoints",
    "swa-full",
    "tags",
    "temp",
    "temperature",
    "tensor-split",
    "threads",
    "threads-batch",
    "threads-batch-draft",
    "threads-draft",
    "threads-http",
    "timeout",
    "tools",
    "tools-runtime",
    "top-k",
    "top-n-sigma",
    "top-nsigma",
    "top-p",
    "tts-lang",
    "tts-voice",
    "typical",
    "typical-p",
    "ubatch-size",
    "ui",
    "ui-config",
    "ui-config-file",
    "ui-mcp-proxy",
    "usage",
    "verbose",
    "verbosity",
    "version",
    "video-ffmpeg-dir",
    "video-fps",
    "video-timestamp-interval",
    "vision-gemma-12b-default",
    "vision-gemma-4b-default",
    "warmup",
    "webui",
    "webui-config",
    "webui-config-file",
    "webui-mcp-proxy",
    "xtc-probability",
    "xtc-threshold",
    "yarn-attn-factor",
    "yarn-beta-fast",
    "yarn-beta-slow",
    "yarn-ext-factor",
    "yarn-orig-ctx",
];

/// Keys only a preset section may carry (not command-line arguments).
const PRESET_ONLY: &[&str] = &["load-on-startup", "stop-timeout"];

/// Arguments the router controls itself (or that would change the server,
/// not a model): never accepted in `engine_args`.
const ROUTER_CONTROLLED: &[&str] = &[
    "host",
    "port",
    "api-key",
    "api-key-file",
    "alias",
    "hf-repo",
    "hf-file",
    "hf-token",
    "hf-repo-draft",
    "model-url",
    "docker-repo",
    "models-dir",
    "models-max",
    "models-preset",
    "models-autoload",
    "no-models-autoload",
    "models-memory-margin",
    "ssl-cert-file",
    "ssl-key-file",
    "path",
    "api-prefix",
    "reuse-port",
    "help",
    "version",
    "list-devices",
    "completion-bash",
    "log-file",
    "log-prompts-dir",
    "slot-save-path",
    "media-path",
    "offline",
    "webui",
    "no-webui",
    "webui-config",
    "webui-config-file",
    "ui",
    "no-ui",
    "ui-config",
    "ui-config-file",
    "tags",
];

/// Arguments that name a file on the engine's machine: only with
/// `engine.allow_model_path` on the lane.
const PATH_ARGS: &[&str] = &[
    "model",
    "model-draft",
    "mmproj",
    "spec-draft-model",
    "lora",
    "lora-scaled",
    "control-vector",
    "control-vector-scaled",
    "chat-template-file",
    "grammar-file",
    "json-schema-file",
    "lookup-cache-static",
    "lookup-cache-dynamic",
];

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- seams

/// Runs a lane's `settings_command` with the overlay on stdin.
#[async_trait]
pub trait SettingsCommandRunner: Send + Sync {
    /// `Ok(short stdout)` on exit 0, `Err(why)` otherwise.
    async fn run(&self, command: &str, input: &[u8], timeout: Duration) -> Result<String, String>;
}

/// `sh -c COMMAND` with the overlay on stdin, killed at its timeout.
pub struct ShellSettingsRunner;

fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let start = text
        .char_indices()
        .rev()
        .nth(299)
        .map(|(index, _)| index)
        .unwrap_or(0);
    text[start..].to_string()
}

#[async_trait]
impl SettingsCommandRunner for ShellSettingsRunner {
    async fn run(&self, command: &str, input: &[u8], timeout: Duration) -> Result<String, String> {
        use std::process::Stdio;
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.to_vec();
            // A command that does not read stdin may close it early; that
            // is its business, not a failure.
            tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
            });
        }
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| format!("timed out after {} s", timeout.as_secs()))?
            .map_err(|e| format!("failed: {e}"))?;
        if output.status.success() {
            Ok(tail(&output.stdout))
        } else {
            let code = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            Err(format!("exit {code}: {}", tail(&output.stderr)))
        }
    }
}

// ---------------------------------------------------------------- engine kinds

/// What differs per engine kind: how load settings become the engine's own
/// arguments, and which raw arguments it accepts.
pub(crate) trait EngineKindOps {
    /// The engine's per-model arguments for a load layer plus raw ones.
    fn preset_pairs(
        &self,
        load: &Map<String, Value>,
        args: &Map<String, Value>,
    ) -> BTreeMap<String, String>;
    /// Check raw engine arguments.
    fn check_args(&self, args: &Map<String, Value>, allow_paths: bool) -> Result<(), String>;
    /// The argument name a load key becomes (for the settings view).
    fn preset_key(&self, load_key: &str) -> Option<&'static str>;
}

pub(crate) struct LlamaCppRouter;

fn scalar(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn switch_word(value: &Value) -> String {
    match value {
        Value::Bool(true) => "on".to_string(),
        Value::Bool(false) => "off".to_string(),
        other => scalar(other),
    }
}

/// `load-mode` from `load_mode`, or from `mlock`/`mmap` (mmap on unless
/// set off).
fn load_mode(load: &Map<String, Value>) -> Option<String> {
    if let Some(mode) = load.get("load_mode").and_then(Value::as_str) {
        return Some(mode.to_string());
    }
    let mlock = load.get("mlock").and_then(Value::as_bool);
    let mmap = load.get("mmap").and_then(Value::as_bool);
    if mlock.is_none() && mmap.is_none() {
        return None;
    }
    let mode = match (mmap.unwrap_or(true), mlock.unwrap_or(false)) {
        (true, true) => "mmap+mlock",
        (true, false) => "mmap",
        (false, true) => "mlock",
        (false, false) => "none",
    };
    Some(mode.to_string())
}

impl EngineKindOps for LlamaCppRouter {
    fn preset_key(&self, load_key: &str) -> Option<&'static str> {
        Some(match load_key {
            "ctx_size" => "ctx-size",
            "n_gpu_layers" => "n-gpu-layers",
            "cache_type_k" => "cache-type-k",
            "cache_type_v" => "cache-type-v",
            "flash_attn" => "flash-attn",
            "batch_size" => "batch-size",
            "ubatch_size" => "ubatch-size",
            "parallel" => "parallel",
            "threads" => "threads",
            "threads_batch" => "threads-batch",
            "mlock" | "mmap" | "load_mode" => "load-mode",
            "rope_scaling" => "rope-scaling",
            "rope_freq_base" => "rope-freq-base",
            "rope_freq_scale" => "rope-freq-scale",
            "yarn_orig_ctx" => "yarn-orig-ctx",
            "yarn_ext_factor" => "yarn-ext-factor",
            "yarn_attn_factor" => "yarn-attn-factor",
            "yarn_beta_slow" => "yarn-beta-slow",
            "yarn_beta_fast" => "yarn-beta-fast",
            "spec_type" => "spec-type",
            "spec_draft_model" => "spec-draft-model",
            "draft_max" => "spec-draft-n-max",
            "draft_min" => "spec-draft-n-min",
            "mmproj" => "mmproj",
            "sleep_idle_seconds" => "sleep-idle-seconds",
            "fit" => "fit",
            "fit_ctx" => "fit-ctx",
            "reasoning_budget" => "reasoning-budget",
            "temp" => "temp",
            "top_k" => "top-k",
            "top_p" => "top-p",
            "min_p" => "min-p",
            _ => return None,
        })
    }

    fn preset_pairs(
        &self,
        load: &Map<String, Value>,
        args: &Map<String, Value>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (name, value) in load {
            match name.as_str() {
                "mlock" | "mmap" | "load_mode" => {}
                "flash_attn" | "fit" => {
                    out.insert(name.replace('_', "-"), switch_word(value));
                }
                "mmproj" if value.as_str() == Some("off") => {
                    out.insert("no-mmproj".to_string(), "true".to_string());
                }
                other => {
                    if let Some(key) = self.preset_key(other) {
                        out.insert(key.to_string(), scalar(value));
                    }
                }
            }
        }
        if let Some(mode) = load_mode(load) {
            out.insert("load-mode".to_string(), mode);
        }
        for (name, value) in args {
            out.insert(name.clone(), scalar(value));
        }
        out
    }

    fn check_args(&self, args: &Map<String, Value>, allow_paths: bool) -> Result<(), String> {
        for name in args.keys() {
            if ROUTER_CONTROLLED.contains(&name.as_str()) {
                return Err(format!(
                    "engine argument `{name}` is the router's own; it cannot be set per model"
                ));
            }
            if !LLAMA_SERVER_ARGS.contains(&name.as_str()) && !PRESET_ONLY.contains(&name.as_str())
            {
                return Err(format!(
                    "`{name}` is not a llama-server argument (use the long name without dashes, \
                     e.g. n-cpu-moe)"
                ));
            }
            if PATH_ARGS.contains(&name.as_str()) && !allow_paths {
                return Err(format!(
                    "engine argument `{name}` names a file; the lane needs engine.allow_model_path"
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn kind_ops(kind: EngineControlKind) -> &'static dyn EngineKindOps {
    match kind {
        EngineControlKind::LlamaCppRouter => &LlamaCppRouter,
    }
}

fn kind_name(kind: EngineControlKind) -> &'static str {
    match kind {
        EngineControlKind::LlamaCppRouter => "llama_cpp_router",
    }
}

// ---------------------------------------------------------------- lanes

/// A lane with an engine control.
#[derive(Debug, Clone)]
pub(crate) struct EngineLane {
    pub name: String,
    pub engine: LaneEngineConfig,
    /// The engine's root URL (no trailing `/`).
    pub base: String,
    pub command_timeout: Duration,
}

/// The engine lane of a provider: the `local_executors` entry named like it
/// with an `engine:` block.
pub(crate) fn engine_lane(config: &Config, provider: &str) -> Option<EngineLane> {
    let lane = config
        .local_executors
        .iter()
        .find(|lane| lane.name == provider)?;
    let engine = lane.engine.clone()?;
    let base = engine.base_url.clone().or_else(|| {
        config
            .providers
            .iter()
            .find(|p| p.id == provider)
            .and_then(|p| match &p.kind {
                sb_core::ProviderKind::OpenaiCompatible { base_url, .. } => Some(base_url.clone()),
                _ => None,
            })
    })?;
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base).to_string();
    Some(EngineLane {
        name: lane.name.clone(),
        engine,
        base,
        command_timeout: Duration::from_secs(lane.command_timeout_secs.max(1)),
    })
}

fn split_target(target: &str) -> Option<(&str, &str)> {
    target
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
}

enum Awake {
    Up,
    Asleep(&'static str),
}

/// What a variant is: a target model with extra load settings, under its
/// own name in the engine (`<model>~<hash8>`).
#[derive(Debug, Clone)]
pub(crate) struct Variant {
    pub name: String,
    pub target: String,
    pub base_model: String,
    pub load: Map<String, Value>,
    pub created_ms: i64,
}

#[derive(Default)]
struct ControlState {
    /// Per lane: the last apply's result.
    last_apply: BTreeMap<String, Value>,
    /// Per lane: an apply that did not reach the engine (retried before the
    /// next load or apply).
    pending_apply: BTreeMap<String, bool>,
    /// Per lane: the engine's last model listing, and when.
    models: BTreeMap<String, (i64, Vec<Value>)>,
    /// Per target: the settings revision whose load part last changed.
    load_changed: HashMap<String, u64>,
    /// Per target: the settings revision it was last loaded with by us.
    loaded_with: HashMap<String, u64>,
    /// Per lane: variants by name.
    variants: BTreeMap<String, BTreeMap<String, Variant>>,
}

/// The engine controls of every lane: shared state plus the seams.
pub struct EngineControls {
    runner: Arc<dyn SettingsCommandRunner>,
    client: reqwest::Client,
    state: Mutex<ControlState>,
    /// One apply at a time (the overlay is one document per lane, and
    /// overlapping writes of it would race).
    apply_lock: tokio::sync::Mutex<()>,
}

impl Default for EngineControls {
    fn default() -> Self {
        Self::with_runner(Arc::new(ShellSettingsRunner))
    }
}

/// The outcome of applying load settings after a write.
pub(crate) struct ApplyOutcome {
    pub value: Value,
    /// `apply: now` could not reload because the model stayed busy.
    pub conflict: bool,
}

enum ReloadError {
    Busy(usize),
    Failed(String),
}

impl EngineControls {
    pub fn with_runner(runner: Arc<dyn SettingsCommandRunner>) -> Self {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        EngineControls {
            runner,
            client,
            state: Mutex::new(ControlState::default()),
            apply_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the lane's machine is up. Never wakes: a lane in the
    /// lifecycle answers from its state; one outside it gets one health GET
    /// (a sleeping machine does not answer it).
    async fn awake(&self, state: &AppState, lane: &EngineLane) -> Awake {
        use crate::activator::LaneState;
        if let Some(executor) = state.capacity.executor(&lane.name) {
            match executor.report().state {
                LaneState::Healthy | LaneState::Draining | LaneState::IdleCountdown => {
                    return Awake::Up
                }
                LaneState::Waking => return Awake::Asleep("waking"),
                LaneState::PoweringOff => return Awake::Asleep("powering off"),
                LaneState::Offline | LaneState::Degraded => {}
            }
        }
        match self
            .client
            .get(format!("{}/health", lane.base))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => Awake::Up,
            _ => Awake::Asleep("asleep"),
        }
    }

    /// One call to the engine, with the lane provider's credential.
    async fn call(
        &self,
        state: &AppState,
        lane: &EngineLane,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, String> {
        let snap = state.snapshot();
        let mut request = self
            .client
            .request(method, format!("{}{path}", lane.base))
            .timeout(timeout);
        let mut credential = None;
        if let Some(provider) = snap.config.providers.iter().find(|p| p.id == lane.name) {
            if let Ok(Some(found)) =
                crate::scheme_auth::SchemeCredential::resolve(&snap.resolver, provider, &lane.name)
                    .await
            {
                request = found.apply(request);
                credential = Some(found);
            }
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| crate::scheme_auth::error_text(e, credential.as_ref()))?;
        let status = response.status();
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("the engine answered {status}: {}", tail(&bytes)));
        }
        if bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| format!("the engine's answer is not JSON: {e}"))
    }

    /// The engine's model listing (cached for when the machine sleeps).
    async fn models(&self, state: &AppState, lane: &EngineLane) -> Result<Vec<Value>, String> {
        let listing = self
            .call(
                state,
                lane,
                reqwest::Method::GET,
                "/models",
                None,
                Duration::from_secs(10),
            )
            .await?;
        let models = listing
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.lock()
            .models
            .insert(lane.name.clone(), (now_ms(), models.clone()));
        Ok(models)
    }

    fn cached_models(&self, lane: &str) -> Option<(i64, Vec<Value>)> {
        self.lock().models.get(lane).cloned()
    }

    /// The managed overlay of a lane: every managed load-time setting of
    /// its models, and its variants (base section plus their keys).
    pub(crate) fn overlay(&self, state: &AppState, lane: &EngineLane) -> Value {
        let ops = kind_ops(lane.engine.kind);
        let mut models = Map::new();
        let mut bases: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for (target, entry) in state.engine.model_settings().all() {
            let Some((provider, model)) = split_target(&target) else {
                continue;
            };
            if provider != lane.name {
                continue;
            }
            let pairs = ops.preset_pairs(&entry.doc.load, &entry.doc.engine_args);
            if !pairs.is_empty() {
                models.insert(model.to_string(), json!({"settings": pairs}));
            }
            bases.insert(model.to_string(), pairs);
        }
        let variants: Vec<Variant> = self
            .lock()
            .variants
            .get(&lane.name)
            .map(|variants| variants.values().cloned().collect())
            .unwrap_or_default();
        for variant in variants {
            let mut pairs = bases.get(&variant.base_model).cloned().unwrap_or_default();
            pairs.extend(ops.preset_pairs(&variant.load, &Map::new()));
            models.insert(
                variant.name.clone(),
                json!({"base": variant.base_model, "settings": pairs}),
            );
        }
        json!({
            "version": 1,
            "lane": lane.name,
            "engine": kind_name(lane.engine.kind),
            "models": models,
        })
    }

    /// Write the lane's overlay through its `settings_command`, then ask an
    /// awake engine to re-read its presets. Never wakes.
    pub(crate) async fn apply_lane(&self, state: &AppState, lane: &EngineLane) -> Value {
        let _guard = self.apply_lock.lock().await;
        let overlay = self.overlay(state, lane);
        let names: Vec<String> = overlay["models"]
            .as_object()
            .map(|models| models.keys().cloned().collect())
            .unwrap_or_default();
        let mut result = json!({"at_ms": now_ms(), "models": names});
        let Some(command) = lane.engine.settings_command.clone() else {
            result["ok"] = json!(false);
            result["error"] = json!("the lane has no engine.settings_command");
            self.finish_apply(lane, result.clone(), true);
            return result;
        };
        let input = serde_json::to_vec(&overlay).unwrap_or_default();
        if let Err(e) = self
            .runner
            .run(&command, &input, lane.command_timeout)
            .await
        {
            result["ok"] = json!(false);
            result["error"] = json!(format!("settings_command failed: {e}"));
            self.finish_apply(lane, result.clone(), true);
            return result;
        }
        match self.awake(state, lane).await {
            Awake::Up => match self
                .call(
                    state,
                    lane,
                    reqwest::Method::GET,
                    "/models?reload=1",
                    None,
                    Duration::from_secs(30),
                )
                .await
            {
                Ok(_) => {
                    result["ok"] = json!(true);
                    result["reloaded"] = json!(true);
                    self.finish_apply(lane, result.clone(), false);
                }
                Err(e) => {
                    result["ok"] = json!(false);
                    result["reloaded"] = json!(false);
                    result["error"] = json!(format!(
                        "settings written; the engine did not re-read them: {e}"
                    ));
                    self.finish_apply(lane, result.clone(), true);
                }
            },
            Awake::Asleep(why) => {
                result["ok"] = json!(true);
                result["reloaded"] = json!(false);
                result["note"] = json!(format!(
                    "settings written; the machine is {why} and the engine reads them when it starts"
                ));
                self.finish_apply(lane, result.clone(), false);
            }
        }
        result
    }

    fn finish_apply(&self, lane: &EngineLane, result: Value, pending: bool) {
        let mut st = self.lock();
        st.last_apply.insert(lane.name.clone(), result);
        st.pending_apply.insert(lane.name.clone(), pending);
    }

    fn pending_apply(&self, lane: &str) -> bool {
        self.lock()
            .pending_apply
            .get(lane)
            .copied()
            .unwrap_or(false)
    }

    fn mark_load_changed(&self, target: &str, revision: u64) {
        self.lock()
            .load_changed
            .insert(target.to_string(), revision);
    }

    fn mark_loaded(&self, target: &str, revision: u64) {
        self.lock().loaded_with.insert(target.to_string(), revision);
    }

    /// Loaded with older load settings than the current ones.
    fn load_pending(&self, target: &str, status: Option<&str>) -> bool {
        if !matches!(status, Some("loaded" | "sleeping" | "loading")) {
            return false;
        }
        let st = self.lock();
        match st.load_changed.get(target) {
            Some(changed) => st
                .loaded_with
                .get(target)
                .is_none_or(|loaded| loaded < changed),
            None => false,
        }
    }

    /// Wait (bounded) until nothing is in flight on an engine model.
    async fn wait_idle(&self, state: &AppState, key: &str, secs: u64) -> Result<(), usize> {
        let registry = state.engine.model_settings();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        loop {
            let busy = registry.in_flight(key);
            if busy == 0 {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(busy);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Load a model: through the lane's place and GPU group like a request
    /// (held only while loading), then wait for the engine to say loaded.
    async fn load_model(
        &self,
        state: &AppState,
        lane: &EngineLane,
        model: &str,
    ) -> Result<Value, String> {
        let _busy = match state.capacity.executor(&lane.name) {
            Some(executor) => match executor.acquire_for_request().await {
                sb_runtime::LaneAcquire::Ready { busy, .. } => busy,
                sb_runtime::LaneAcquire::NotReady { detail, .. }
                | sb_runtime::LaneAcquire::QueueTimeout { detail, .. } => {
                    return Err(format!("the lane is not free to load: {detail}"))
                }
            },
            None => None,
        };
        self.call(
            state,
            lane,
            reqwest::Method::POST,
            "/models/load",
            Some(json!({"model": model})),
            Duration::from_secs(30),
        )
        .await?;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(lane.engine.load_timeout_secs);
        loop {
            let models = self.models(state, lane).await?;
            if let Some(row) = models.iter().find(|row| row["id"] == model) {
                match row["status"]["value"].as_str() {
                    Some("loaded" | "sleeping") => return Ok(row.clone()),
                    Some("unloaded") if row["status"]["failed"] == json!(true) => {
                        return Err(format!(
                            "the engine could not load `{model}` (exit code {})",
                            row["status"]["exit_code"]
                        ))
                    }
                    _ => {}
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "`{model}` is still not loaded after {} s",
                    lane.engine.load_timeout_secs
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// `apply: now`: unload and load again a model that is loaded, once
    /// nothing is in flight on it.
    async fn reload_model(
        &self,
        state: &AppState,
        lane: &EngineLane,
        target: &str,
        model: &str,
        wake: bool,
    ) -> Result<String, ReloadError> {
        let _woken = match self.awake(state, lane).await {
            Awake::Up => None,
            Awake::Asleep(why) if !wake => {
                return Ok(format!(
                "the machine is {why}; the model starts with the new settings when it next loads"
            ))
            }
            Awake::Asleep(_) => Some(self.wake(state, lane).await.map_err(ReloadError::Failed)?),
        };
        let models = self
            .models(state, lane)
            .await
            .map_err(ReloadError::Failed)?;
        let status = models
            .iter()
            .find(|row| row["id"] == model)
            .and_then(|row| row["status"]["value"].as_str())
            .map(str::to_string);
        if !matches!(status.as_deref(), Some("loaded" | "sleeping" | "loading")) {
            return Ok("the model is not loaded; it loads with the new settings next time".into());
        }
        self.wait_idle(state, target, lane.engine.apply_wait_secs)
            .await
            .map_err(ReloadError::Busy)?;
        self.call(
            state,
            lane,
            reqwest::Method::POST,
            "/models/unload",
            Some(json!({"model": model})),
            Duration::from_secs(60),
        )
        .await
        .map_err(ReloadError::Failed)?;
        self.load_model(state, lane, model)
            .await
            .map_err(ReloadError::Failed)?;
        self.mark_loaded(target, state.engine.model_settings().revision(target));
        Ok("reloaded with the new settings".to_string())
    }

    /// Wake the lane's machine and wait for it (bounded by its boot
    /// timeout). The guard counts as lane activity while held.
    async fn wake(
        &self,
        state: &AppState,
        lane: &EngineLane,
    ) -> Result<crate::activator::LaneJobGuard, String> {
        let executor = state
            .capacity
            .executor(&lane.name)
            .ok_or_else(|| format!("lane `{}` has no managed machine to wake", lane.name))?;
        executor.ensure_ready().await.map_err(|e| match e {
            crate::activator::EnsureError::WakeUnconfigured => {
                "the lane has no wake_command".to_string()
            }
            crate::activator::EnsureError::BootTimeout { elapsed_ms, .. } => {
                format!("the machine did not come up within {} s", elapsed_ms / 1000)
            }
            crate::activator::EnsureError::WakeFailed(e) => format!("wake failed: {e}"),
            crate::activator::EnsureError::Refused(detail) => detail,
        })
    }
}

/// `memory` of a model row, summed per measurement: bytes on GPU devices
/// and on the host, and the context it was measured at.
fn memory_summary(memory: &Value) -> Value {
    if !memory.is_object() {
        return Value::Null;
    }
    let mut out = json!({"status": memory.get("status")});
    for name in ["min", "max", "effective"] {
        let Some(part) = memory.get(name).filter(|part| part.is_object()) else {
            continue;
        };
        let mut device = 0u64;
        let mut host = 0u64;
        for row in part["devices"].as_array().into_iter().flatten() {
            let bytes = ["model", "context", "compute"]
                .iter()
                .filter_map(|key| row[*key].as_u64())
                .sum::<u64>();
            if row["kind"] == "host" {
                host += bytes;
            } else {
                device += bytes;
            }
        }
        out[name] = json!({"n_ctx": part["n_ctx"], "bytes": device, "host_bytes": host});
    }
    out
}

// ---------------------------------------------------------------- settings API hooks

/// Engine checks of a settings document before it is written: load
/// settings need an engine control with a `settings_command`; raw
/// arguments must be the engine's; paths need `allow_model_path`.
pub(crate) fn validate_for_target(
    state: &AppState,
    target: &str,
    doc: &ModelSettingsDoc,
) -> Result<(), String> {
    if doc.load.is_empty() && doc.engine_args.is_empty() && doc.request_load.is_none() {
        return Ok(());
    }
    let snap = state.snapshot();
    let Some(lane) =
        split_target(target).and_then(|(provider, _)| engine_lane(&snap.config, provider))
    else {
        return Err(format!(
            "`{target}` has no engine control: load settings need its lane to have \
             `engine: {{kind: llama_cpp_router, settings_command: ...}}`"
        ));
    };
    if lane.engine.settings_command.is_none() {
        return Err(format!(
            "lane `{}` has no engine.settings_command, so load settings cannot be applied",
            lane.name
        ));
    }
    let paths = load_layer_paths(&doc.load);
    if !paths.is_empty() && !lane.engine.allow_model_path {
        return Err(format!(
            "load settings {} name files; lane `{}` needs engine.allow_model_path",
            paths.join(", "),
            lane.name
        ));
    }
    kind_ops(lane.engine.kind).check_args(&doc.engine_args, lane.engine.allow_model_path)
}

/// The load-time part of a target's view (operator keys only). Uses the
/// last engine listing only: a settings read never calls a machine.
pub(crate) fn add_load_view(
    state: &AppState,
    named: &NamedTarget,
    doc: Option<&ModelSettingsDoc>,
    view: &mut Value,
) {
    let snap = state.snapshot();
    let Some((provider, model)) = split_target(&named.target) else {
        return;
    };
    let Some(lane) = engine_lane(&snap.config, provider) else {
        return;
    };
    let controls = &state.engines;
    let cached = controls.cached_models(&lane.name);
    let row = cached
        .as_ref()
        .and_then(|(_, models)| models.iter().find(|row| row["id"] == model).cloned());
    let status = row
        .as_ref()
        .and_then(|row| row["status"]["value"].as_str())
        .map(str::to_string);
    let pending = controls.load_pending(&named.target, status.as_deref());
    let ops = kind_ops(lane.engine.kind);
    let mut settings = Map::new();
    if let Some(doc) = doc {
        for (key, value) in &doc.load {
            settings.insert(
                key.clone(),
                json!({
                    "value": value,
                    "engine_key": ops.preset_key(key),
                    "pending": pending,
                }),
            );
        }
    }
    view["load"] = json!({
        "engine": {
            "lane": lane.name,
            "kind": kind_name(lane.engine.kind),
            "status": status.as_deref().unwrap_or("unknown"),
            "seen_at_ms": cached.as_ref().map(|(at, _)| *at),
        },
        "settings": settings,
        "engine_args": doc.map(|doc| json!(doc.engine_args)).unwrap_or_else(|| json!({})),
        "pending": pending,
        "memory": row.as_ref().map(|row| memory_summary(&row["memory"])).unwrap_or(Value::Null),
        "last_apply": controls.lock().last_apply.get(&lane.name).cloned(),
    });
}

/// After a settings write: apply changed load settings (`next_load` or
/// `now`). `None` = the load part did not change.
pub(crate) async fn after_settings_write(
    state: &AppState,
    target: &str,
    before: Option<&ModelSettingsDoc>,
    entry: &SettingsEntry,
    apply: ApplyMode,
    wake: bool,
) -> Option<ApplyOutcome> {
    let doc = &entry.doc;
    let changed = before
        .is_none_or(|before| before.load != doc.load || before.engine_args != doc.engine_args);
    let any = !doc.load.is_empty()
        || !doc.engine_args.is_empty()
        || before.is_some_and(|before| !before.load.is_empty() || !before.engine_args.is_empty());
    if !changed || !any {
        return None;
    }
    let (provider, model) = split_target(target)?;
    let lane = engine_lane(&state.snapshot().config, provider)?;
    let controls = &state.engines;
    controls.mark_load_changed(target, entry.revision);
    let mut value = controls.apply_lane(state, &lane).await;
    value["mode"] = json!(match apply {
        ApplyMode::NextLoad => "next_load",
        ApplyMode::Now => "now",
    });
    if value["ok"] != json!(true) {
        value["message"] = json!(format!(
            "revision {} written; applying it failed ({}); it is retried before the next load",
            entry.revision,
            value["error"].as_str().unwrap_or("unknown error")
        ));
        return Some(ApplyOutcome {
            value,
            conflict: false,
        });
    }
    let mut conflict = false;
    let message = match apply {
        ApplyMode::NextLoad => format!(
            "revision {} written; a loaded model keeps its old load settings until it loads again \
             (pending until reload)",
            entry.revision
        ),
        ApplyMode::Now => match controls
            .reload_model(state, &lane, target, model, wake)
            .await
        {
            Ok(note) => format!("revision {} written; {note}", entry.revision),
            Err(ReloadError::Busy(count)) => {
                conflict = true;
                format!(
                    "revision {} written, but `{target}` still has {count} request(s) in flight \
                     after {} s; it was not reloaded (pending until reload)",
                    entry.revision, lane.engine.apply_wait_secs
                )
            }
            Err(ReloadError::Failed(e)) => {
                value["ok"] = json!(false);
                value["error"] = json!(e.clone());
                format!("revision {} written; reloading failed: {e}", entry.revision)
            }
        },
    };
    value["message"] = json!(message);
    Some(ApplyOutcome { value, conflict })
}

// ---------------------------------------------------------------- endpoints

fn no_engine(target: &str) -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "no_engine_control",
        format!("`{target}` has no engine control (its lane needs an `engine:` block)"),
    )
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadBody {
    #[serde(default)]
    wake: bool,
}

/// `POST /v1/models/{model}/load {"wake": false}`: preload a model.
pub(crate) async fn load_endpoint(
    state: &AppState,
    principal: &Principal,
    name: &str,
    body: Value,
) -> Response {
    let body: LoadBody = match serde_json::from_value(body) {
        Ok(body) => body,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("{e}; send {{\"wake\": true|false}}"),
            )
        }
    };
    let named = match resolve_one(state, principal, name) {
        Ok(named) => named,
        Err(response) => return response,
    };
    match load_target(state, &named.target, body.wake).await {
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

/// Load a target's model (waking only when `wake`). `Err` is the response.
pub(crate) async fn load_target(
    state: &AppState,
    target: &str,
    wake: bool,
) -> Result<Value, Response> {
    let snap = state.snapshot();
    let Some((provider, model)) = split_target(target) else {
        return Err(no_engine(target));
    };
    let Some(lane) = engine_lane(&snap.config, provider) else {
        return Err(no_engine(target));
    };
    let controls = &state.engines;
    let _woken = match controls.awake(state, &lane).await {
        Awake::Up => None,
        Awake::Asleep(why) if !wake => {
            return Err(error(
                StatusCode::CONFLICT,
                "lane_asleep",
                format!(
                    "lane `{}` is {why}; send \"wake\": true to wake it and load `{model}`",
                    lane.name
                ),
            ))
        }
        Awake::Asleep(_) => match controls.wake(state, &lane).await {
            Ok(guard) => Some(guard),
            Err(e) => return Err(error(StatusCode::SERVICE_UNAVAILABLE, "wake_failed", e)),
        },
    };
    if controls.pending_apply(&lane.name) {
        controls.apply_lane(state, &lane).await;
    }
    match controls.load_model(state, &lane, model).await {
        Ok(row) => {
            controls.mark_loaded(target, state.engine.model_settings().revision(target));
            Ok(json!({
                "target": target,
                "model": model,
                "lane": lane.name,
                "status": row["status"]["value"],
                "memory": memory_summary(&row["memory"]),
            }))
        }
        Err(e) => Err(error(StatusCode::BAD_GATEWAY, "engine_error", e)),
    }
}

/// `POST /v1/models/{model}/unload`: refused while a request is in flight
/// on the model; never wakes or powers anything off.
pub(crate) async fn unload_endpoint(
    state: &AppState,
    principal: &Principal,
    name: &str,
) -> Response {
    let named = match resolve_one(state, principal, name) {
        Ok(named) => named,
        Err(response) => return response,
    };
    let snap = state.snapshot();
    let Some((provider, model)) = split_target(&named.target) else {
        return no_engine(&named.target);
    };
    let Some(lane) = engine_lane(&snap.config, provider) else {
        return no_engine(&named.target);
    };
    let controls = &state.engines;
    if let Awake::Asleep(why) = controls.awake(state, &lane).await {
        return Json(json!({
            "target": named.target,
            "model": model,
            "status": "asleep",
            "message": format!("the machine is {why}; nothing is loaded"),
        }))
        .into_response();
    }
    let busy = state.engine.model_settings().in_flight(&named.target);
    if busy > 0 {
        return error(
            StatusCode::CONFLICT,
            "model_busy",
            format!(
                "`{}` has {busy} request(s) in flight; unload refused",
                named.target
            ),
        );
    }
    match controls
        .call(
            state,
            &lane,
            reqwest::Method::POST,
            "/models/unload",
            Some(json!({"model": model})),
            Duration::from_secs(60),
        )
        .await
    {
        Ok(_) => Json(json!({"target": named.target, "model": model, "status": "unloaded"}))
            .into_response(),
        Err(e) => error(StatusCode::BAD_GATEWAY, "engine_error", e),
    }
}

/// `GET /v1/engines` (operator/admin): per engine lane, the engine's models
/// with status, arguments, memory, pending settings, variants and the last
/// apply. Never wakes: a sleeping lane shows its last known listing.
pub(crate) async fn engines_endpoint(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let snap = state.snapshot();
    let registry = state.engine.model_settings();
    let controls = &state.engines;
    let mut lanes = Vec::new();
    for lane_cfg in &snap.config.local_executors {
        let Some(lane) = engine_lane(&snap.config, &lane_cfg.name) else {
            continue;
        };
        let (status, models, error_text) = match controls.awake(&state, &lane).await {
            Awake::Up => match controls.models(&state, &lane).await {
                Ok(models) => ("up", Some((now_ms(), models)), None),
                Err(e) => ("up", controls.cached_models(&lane.name), Some(e)),
            },
            Awake::Asleep(why) => (why, controls.cached_models(&lane.name), None),
        };
        let rows: Vec<Value> = models
            .as_ref()
            .map(|(_, models)| models.as_slice())
            .unwrap_or(&[])
            .iter()
            .map(|row| {
                let id = row["id"].as_str().unwrap_or("");
                let target = format!("{}/{id}", lane.name);
                let status = row["status"]["value"].as_str();
                json!({
                    "id": id,
                    "target": target,
                    "status": status,
                    "failed": row["status"]["failed"],
                    "args": row["status"]["args"],
                    "memory": memory_summary(&row["memory"]),
                    "settings_revision": registry.revision(&target),
                    "pending": controls.load_pending(&target, status),
                    "in_flight": registry.in_flight(&target),
                })
            })
            .collect();
        let variants: Vec<Value> = controls
            .lock()
            .variants
            .get(&lane.name)
            .map(|variants| {
                variants
                    .values()
                    .map(|v| {
                        let key = format!("{}/{}", lane.name, v.name);
                        json!({
                            "name": v.name,
                            "target": v.target,
                            "load": v.load,
                            "created_ms": v.created_ms,
                            "last_used_ms": registry.last_used_ms(&key),
                            "in_flight": registry.in_flight(&key),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (last_apply, pending_apply) = {
            let st = controls.lock();
            (
                st.last_apply.get(&lane.name).cloned(),
                st.pending_apply.get(&lane.name).copied().unwrap_or(false),
            )
        };
        lanes.push(json!({
            "lane": lane.name,
            "kind": kind_name(lane.engine.kind),
            "state": status,
            "seen_at_ms": models.as_ref().map(|(at, _)| *at),
            "error": error_text,
            "models": rows,
            "variants": variants,
            "last_apply": last_apply,
            "pending_apply": pending_apply,
            "settings_command": lane.engine.settings_command.is_some(),
        }));
    }
    Json(json!({"object": "list", "data": lanes})).into_response()
}

/// After a hold's wake: load the model the hold named (operator keys).
pub(crate) fn preload_after_hold(state: &AppState, target: String, wake: bool) {
    let state = state.clone();
    tokio::spawn(async move {
        match load_target(&state, &target, wake).await {
            Ok(_) => tracing::info!(target_model = %target, "hold preload: loaded"),
            Err(_) => tracing::warn!(target_model = %target, "hold preload: not loaded"),
        }
    });
}
