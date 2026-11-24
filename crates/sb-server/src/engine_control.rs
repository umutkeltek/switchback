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

use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// The `llama-server` arguments `engine_args` may set per model: speed,
/// memory and offload, sampling defaults, context and RoPE, speculative
/// decoding from local files, chat template and reasoning. Anything else
/// (tools and agents that run commands, MCP servers, downloads from a URL
/// or a model hub, remote back ends, server-wide switches) is refused by
/// name, as is any argument that makes the engine write a file
/// (`lookup-cache-dynamic`). File arguments also need
/// `engine.allow_model_path`.
const SAFE_ARGS: &[&str] = &[
    // speed, memory, offload
    "batch-size",
    "ubatch-size",
    "ctx-size",
    "cache-type-k",
    "cache-type-v",
    "cache-type-k-draft",
    "cache-type-v-draft",
    "cache-ram",
    "cache-reuse",
    "cache-idle-slots",
    "no-cache-idle-slots",
    "cache-prompt",
    "no-cache-prompt",
    "cont-batching",
    "no-cont-batching",
    "context-shift",
    "no-context-shift",
    "ctx-checkpoints",
    "checkpoint-min-step",
    "swa-checkpoints",
    "swa-full",
    "cpu-mask",
    "cpu-mask-batch",
    "cpu-mask-draft",
    "cpu-mask-batch-draft",
    "cpu-range",
    "cpu-range-batch",
    "cpu-range-draft",
    "cpu-strict",
    "cpu-strict-batch",
    "cpu-strict-draft",
    "cpu-strict-batch-draft",
    "cpu-moe",
    "cpu-moe-draft",
    "n-cpu-moe",
    "n-cpu-moe-draft",
    "n-cpu-ffn",
    "device",
    "device-draft",
    "main-gpu",
    "split-mode",
    "tensor-split",
    "gpu-layers",
    "gpu-layers-draft",
    "n-gpu-layers",
    "n-gpu-layers-draft",
    "override-tensor",
    "override-tensor-draft",
    "flash-attn",
    "fit",
    "fit-ctx",
    "fit-target",
    "kv-offload",
    "no-kv-offload",
    "kv-unified",
    "no-kv-unified",
    "kv-unified-per-slot",
    "load-mode",
    "moe-cache-cpu-share",
    "moe-cache-mib",
    "mtmd-batch-max-tokens",
    "numa",
    "op-offload",
    "no-op-offload",
    "repack",
    "no-repack",
    "parallel",
    "threads",
    "threads-batch",
    "threads-draft",
    "threads-batch-draft",
    "poll",
    "poll-batch",
    "poll-draft",
    "poll-batch-draft",
    "prio",
    "prio-batch",
    "prio-draft",
    "prio-batch-draft",
    "warmup",
    "no-warmup",
    "check-tensors",
    "keep",
    "n-predict",
    "predict",
    "sleep-idle-seconds",
    "image-min-tokens",
    "image-max-tokens",
    "mmproj-offload",
    "no-mmproj-offload",
    "mmproj-device",
    "pooling",
    "embedding",
    "embeddings",
    "embd-normalize",
    "rerank",
    "reranking",
    // context and RoPE
    "rope-freq-base",
    "rope-freq-scale",
    "rope-scale",
    "rope-scaling",
    "yarn-attn-factor",
    "yarn-beta-fast",
    "yarn-beta-slow",
    "yarn-ext-factor",
    "yarn-orig-ctx",
    // sampling defaults
    "temp",
    "temperature",
    "top-k",
    "top-p",
    "min-p",
    "typical",
    "typical-p",
    "top-n-sigma",
    "top-nsigma",
    "xtc-probability",
    "xtc-threshold",
    "repeat-last-n",
    "repeat-penalty",
    "presence-penalty",
    "frequency-penalty",
    "dry-allowed-length",
    "dry-base",
    "dry-multiplier",
    "dry-penalty-last-n",
    "dry-sequence-breaker",
    "dynatemp-exp",
    "dynatemp-range",
    "mirostat",
    "mirostat-ent",
    "mirostat-lr",
    "samplers",
    "sampler-seq",
    "sampling-seq",
    "seed",
    "ignore-eos",
    "logit-bias",
    "adaptive-decay",
    "adaptive-target",
    "backend-sampling",
    "grammar",
    "grammar-file",
    "json-schema",
    "json-schema-file",
    // speculative decoding from local files
    "draft",
    "draft-max",
    "draft-min",
    "draft-n",
    "draft-n-min",
    "draft-p-min",
    "draft-p-split",
    "model-draft",
    "spec-type",
    "spec-draft-model",
    "spec-draft-backend-sampling",
    "no-spec-draft-backend-sampling",
    "spec-draft-cpu-mask",
    "spec-draft-cpu-mask-batch",
    "spec-draft-cpu-moe",
    "spec-draft-cpu-range",
    "spec-draft-cpu-strict",
    "spec-draft-cpu-strict-batch",
    "spec-draft-device",
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
    "lookup-cache-static",
    // local model files
    "mmproj",
    "no-mmproj",
    "no-mmproj-auto",
    "lora",
    "lora-scaled",
    "lora-init-without-apply",
    "control-vector",
    "control-vector-scaled",
    "control-vector-layer-range",
    // chat template and reasoning
    "chat-template",
    "chat-template-file",
    "chat-template-kwargs",
    "jinja",
    "no-jinja",
    "reasoning",
    "reasoning-budget",
    "reasoning-budget-message",
    "reasoning-effort",
    "reasoning-format",
    "reasoning-preserve",
    "no-reasoning-preserve",
    "prefill-assistant",
    "no-prefill-assistant",
    "skip-chat-parsing",
    "no-skip-chat-parsing",
    "special",
    "escape",
    "no-escape",
    "reverse-prompt",
    // preset-only keys
    "load-on-startup",
    "stop-timeout",
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

// ---------------------------------------------------------------- context

/// What the engine control works with: the runtime (snapshot, settings
/// registry), the lanes, and its own state. Cheap to clone. Held by the
/// runtime's variant seam with the engine weakly, so no cycle.
#[derive(Clone)]
pub(crate) struct EngineCtx {
    pub engine: Arc<sb_runtime::Engine>,
    pub capacity: Arc<crate::activator::CapacityActivator>,
    pub engines: Arc<EngineControls>,
}

impl EngineCtx {
    fn snapshot(&self) -> Arc<sb_runtime::Snapshot> {
        self.engine.snapshot()
    }
}

impl From<&AppState> for EngineCtx {
    fn from(state: &AppState) -> Self {
        EngineCtx {
            engine: state.engine.clone(),
            capacity: state.capacity.clone(),
            engines: state.engines.clone(),
        }
    }
}

// ---------------------------------------------------------------- seams

/// Runs a lane's `settings_command` with the overlay on stdin.
#[async_trait]
pub trait SettingsCommandRunner: Send + Sync {
    /// `Ok(short stdout)` on exit 0, `Err(why)` otherwise.
    async fn run(&self, command: &str, input: &[u8], timeout: Duration) -> Result<String, String>;

    /// [`Self::run`] with its failure redacted as an alert is (see
    /// [`redact_output`]): every one of `secrets`, URL credentials and
    /// token-shaped words become `<redacted>`. The default scrubs `run`'s answer; the
    /// shell runner scrubs before it cuts the excerpt, so no part of a
    /// secret is left at the cut.
    async fn run_scrubbed(
        &self,
        command: &str,
        input: &[u8],
        timeout: Duration,
        secrets: &[String],
    ) -> Result<String, String> {
        self.run(command, input, timeout)
            .await
            .map_err(|why| redact_output(&why, secrets))
    }
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
        self.run_scrubbed(command, input, timeout, &secret_values(None))
            .await
    }

    async fn run_scrubbed(
        &self,
        command: &str,
        input: &[u8],
        timeout: Duration,
        secrets: &[String],
    ) -> Result<String, String> {
        use std::process::Stdio;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // Its own process group, so the timeout stops what the command
        // started too (an `ssh`, a script's children), not only `sh`.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start: {e}"))?;
        let group = child.id().and_then(|pid| i32::try_from(pid).ok());
        if let Some(mut stdin) = child.stdin.take() {
            let input = input.to_vec();
            // A command that does not read stdin may close it early; that
            // is its business, not a failure.
            tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
            });
        }
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        async fn read_all(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
            let mut buffer = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buffer).await;
            }
            buffer
        }
        let run = async {
            let (out, err, status) = tokio::join!(read_all(stdout), read_all(stderr), child.wait());
            (out, err, status)
        };
        let (out, err, status) = match tokio::time::timeout(timeout, run).await {
            Ok(done) => done,
            Err(_) => {
                if let Some(group) = group {
                    // SAFETY: a plain kill(2) of the process group the
                    // command leads; no memory is touched.
                    unsafe {
                        libc::kill(-group, libc::SIGKILL);
                    }
                }
                let _ = child.kill().await;
                return Err(format!("timed out after {} s", timeout.as_secs()));
            }
        };
        let status = status.map_err(|e| format!("failed: {e}"))?;
        if status.success() {
            Ok(tail(&out))
        } else {
            let code = status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            Err(format!(
                "exit {code}: {}",
                refusal_reason(&err, &out, secrets)
            ))
        }
    }
}

/// Why a settings command failed, for the error the API and the CLI show:
/// its stderr, then its stdout (a control script that refuses a change
/// prints the reason there), each on one line with control characters
/// removed, joined by ` | `, each redacted (see [`redact_output`]) before
/// the cut, within 300 bytes. Each stream keeps at least half the room
/// and its last bytes, so a long ssh warning on stderr cannot push out the
/// script's reason on stdout, and the reverse.
fn refusal_reason(stderr: &[u8], stdout: &[u8], secrets: &[String]) -> String {
    const LIMIT: usize = 300;
    const JOIN: &str = " | ";
    let err = redact_output(&one_line(stderr), secrets);
    let out = redact_output(&one_line(stdout), secrets);
    if err.is_empty() || out.is_empty() {
        return last_bytes(if err.is_empty() { &out } else { &err }, LIMIT);
    }
    let room = LIMIT - JOIN.len();
    let out_room = room - err.len().min(room / 2);
    let err_room = room - out.len().min(out_room);
    format!(
        "{}{JOIN}{}",
        last_bytes(&err, err_room),
        last_bytes(&out, out_room)
    )
}

/// The text on one line: a newline or tab ends a word; other control
/// characters go.
fn one_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut line = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            if c.is_whitespace() && !line.ends_with(' ') {
                line.push(' ');
            }
        } else {
            line.push(c);
        }
    }
    line.trim().to_string()
}

/// The last `limit` bytes of `text`, cut on a character boundary.
fn last_bytes(text: &str, limit: usize) -> String {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].trim_start().to_string()
}

/// Values a settings command's output must never show: every secret the
/// alerts redact from `config` (see [`crate::alerts::collect_secrets`]:
/// `server.api_key`, `api_keys[].key`, a provider's `api_key`, an account's
/// `auth` key material, and the value of every env var such a field names),
/// the value of every env var `config` names in any `*_env` field, and of
/// any env var whose name holds KEY, TOKEN, SECRET or PASSWORD and whose
/// value is longer than 20 characters. Values are trimmed (a key file read
/// with its newline still matches the key printed on one line); one
/// shorter than 6 characters is left out, so a test key such as `k` does
/// not blank every `k` of the text. The command's own environment is not
/// touched.
fn secret_values(config: Option<&sb_core::Config>) -> Vec<String> {
    let mut found = BTreeSet::new();
    let mut names = Vec::new();
    if let Some(config) = config {
        if let Ok(tree) = serde_json::to_value(config) {
            crate::alerts::collect_secrets(&tree, None, false, &mut found);
            env_names(&tree, &mut names);
        }
    }
    found.extend(names.iter().filter_map(|name| std::env::var(name).ok()));
    for (name, value) in std::env::vars() {
        let name = name.to_ascii_uppercase();
        if value.trim().len() > 20
            && ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                .iter()
                .any(|word| name.contains(word))
        {
            found.insert(value);
        }
    }
    let mut values: Vec<String> = found
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| value.len() >= 6)
        .collect();
    // Longest first, so a value that holds another is replaced whole.
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values.dedup();
    values
}

/// The env var names a config tree names: string values of `*_env` and
/// `env` fields.
fn env_names(tree: &Value, names: &mut Vec<String>) {
    match tree {
        Value::Object(map) => {
            for (key, value) in map {
                match value {
                    Value::String(name) if key == "env" || key.ends_with("_env") => {
                        if !name.is_empty() {
                            names.push(name.clone());
                        }
                    }
                    other => env_names(other, names),
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| env_names(item, names)),
        _ => {}
    }
}

/// `text` redacted as an alert's detail is (the same
/// [`crate::alerts::redact_text`]): every one of `secrets`, the userinfo
/// and query of every URL, and token-shaped words (`Bearer …`, `sk-…`)
/// become `<redacted>`.
fn redact_output(text: &str, secrets: &[String]) -> String {
    crate::alerts::redact_text(text, secrets, &[])
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
            if !SAFE_ARGS.contains(&name.as_str()) {
                return Err(format!(
                    "engine argument `{name}` is not allowed per model: it can run tools or \
                     commands, fetch files from a URL or a model hub, reach other servers or \
                     change the whole server (allowed: speed, memory and offload, sampling \
                     defaults, context and RoPE, speculative decoding with local files, chat \
                     template and reasoning)"
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

/// A model's preset section as the overlay writes it: its load settings,
/// then its raw engine arguments (which win on the same name). The one
/// function behind both the overlay and the check of a reloaded model's
/// arguments, so the check expects what the engine was given.
fn model_preset(ops: &dyn EngineKindOps, doc: &ModelSettingsDoc) -> BTreeMap<String, String> {
    ops.preset_pairs(&doc.load, &doc.engine_args)
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
    /// Per lane: the last apply did not reach the engine (the command or
    /// the re-read failed). Retried when the lane comes up, before a load
    /// and before a request reaches the engine.
    failed_apply: BTreeMap<String, bool>,
    /// Per lane: models whose new section waits until nothing is
    /// dispatched on them (retried when the last request on one ends).
    deferred: BTreeMap<String, BTreeSet<String>>,
    /// Per lane: the model sections the engine last read (or reads when it
    /// starts). Absent until the first apply since the gateway started.
    applied: BTreeMap<String, Map<String, Value>>,
    /// Per lane: when the last apply ran and whether the machine was up.
    last_attempt: BTreeMap<String, (i64, bool)>,
    /// Per lane: failed applies in a row (the retry before a request backs
    /// off with it).
    apply_failures: BTreeMap<String, u32>,
    /// Per lane: the engine's last model listing, and when.
    models: BTreeMap<String, (i64, Vec<Value>)>,
    /// Per target: the settings revision whose load part last changed.
    load_changed: HashMap<String, u64>,
    /// Per target: the settings revision it was last loaded with by us.
    loaded_with: HashMap<String, u64>,
    /// Per lane: variants by name.
    variants: BTreeMap<String, BTreeMap<String, Variant>>,
    /// Per lane: the engine's build (`GET /props` `build_info`), last seen.
    builds: BTreeMap<String, String>,
}

/// How soon a failed apply is first retried before a request; each failure
/// in a row doubles it, up to [`APPLY_RETRY_MAX_MS`]. (A lane that comes up
/// retries at once, from its up hook.)
const APPLY_RETRY_MS: i64 = 15_000;
const APPLY_RETRY_MAX_MS: i64 = 600_000;

/// The wait before the next retry of a failed apply after `failures` in a
/// row.
fn apply_backoff_ms(failures: u32) -> i64 {
    let doublings = failures.saturating_sub(1).min(16);
    APPLY_RETRY_MS
        .saturating_mul(1i64 << doublings)
        .min(APPLY_RETRY_MAX_MS)
}

/// Whether an apply reached the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reached {
    /// The engine has the sections (or reads them when it starts).
    Yes,
    /// The command or the re-read failed: pending, retried with backoff.
    Failed,
    /// Written while the machine wakes: the engine may have read its
    /// presets already, so it re-reads them once the lane is up (pending,
    /// not a failure).
    WhenUp,
}

/// The request path's part of an apply: the models a request waits to
/// reach, when the first gate such a request may wait at closed (the
/// holds' bound counts from then), and the caller's own holds on them
/// (a retry takes them before the apply). Each of those holds opens with
/// the apply's own hold of the same model.
struct Settle {
    models: BTreeSet<String>,
    started: tokio::time::Instant,
    holds: Vec<sb_runtime::model_settings::ModelGate>,
}

impl Settle {
    /// An apply no request waits for (a settings write, a load).
    fn none() -> Self {
        Settle {
            models: BTreeSet::new(),
            started: tokio::time::Instant::now(),
            holds: Vec::new(),
        }
    }
}

/// What is left of a request-path apply once the lane's apply lock is let
/// go: the holds of the rewritten models that still run their old
/// process, kept until it is gone (see [`EngineControls::settle`]).
struct Settling {
    gates: BTreeMap<String, SettleHold>,
    started: tokio::time::Instant,
    /// The apply's `at_ms`: the settle's note goes on this apply's record
    /// only, never on a later one.
    at_ms: Value,
}

/// One settling model's holds, and its mark in [`SettleMarks`] (taken off
/// when its gate opens).
struct SettleHold {
    _gates: Vec<sb_runtime::model_settings::ModelGate>,
    _mark: SettleMark,
}

/// The models whose gate a settle holds closed (see
/// [`EngineControls::settle`]), by `lane/model` key: a load waits for them
/// (see [`load_target`]), since the settle's first pass unloads a model it
/// finds loaded.
#[derive(Default)]
struct SettleMarks {
    held: Mutex<HashMap<String, usize>>,
    opened: tokio::sync::Notify,
}

impl SettleMarks {
    fn mark(self: &Arc<Self>, key: String) -> SettleMark {
        *self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default() += 1;
        SettleMark {
            marks: self.clone(),
            key,
        }
    }

    fn holds(&self, key: &str) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }
}

/// One model's mark in [`SettleMarks`]; dropping it takes the mark off.
struct SettleMark {
    marks: Arc<SettleMarks>,
    key: String,
}

impl Drop for SettleMark {
    fn drop(&mut self) {
        {
            let mut held = self.marks.held.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(count) = held.get_mut(&self.key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    held.remove(&self.key);
                }
            }
        }
        self.marks.opened.notify_waiters();
    }
}

/// How long a request-path apply may keep a model's gate closed after its
/// first hold: the lane's gate wait (what a waiting request allows) less
/// a second, and never longer than the lane's `load_timeout_secs`.
fn settle_bound(gate_wait: Duration, load_timeout: Duration) -> Duration {
    gate_bound(gate_wait).min(load_timeout)
}

/// The longest the gateway keeps a model's gate closed for a request that
/// waits at it: the lane's gate wait less a second, so the gate is open
/// before the request's own wait (which began a moment after the gate
/// closed) runs out.
fn gate_bound(gate_wait: Duration) -> Duration {
    gate_wait.saturating_sub(Duration::from_secs(1))
}

/// How long the engine's re-read of its presets may take (part of what an
/// apply can hold a model: see [`sb_runtime::model_settings::gate_wait`]).
const REREAD_TIMEOUT: Duration = Duration::from_secs(30);

/// The lane's gate wait (see [`sb_runtime::model_settings::gate_wait`]).
fn lane_gate_wait(state: &EngineCtx, lane: &EngineLane) -> Duration {
    sb_runtime::model_settings::gate_wait(&state.snapshot(), &lane.name)
        .unwrap_or(Duration::from_secs(1))
}

/// How often a settle lists the engine's models.
const SETTLE_POLL: Duration = Duration::from_millis(100);

/// Whether the lane's lifecycle state is offline (`false` for a lane
/// outside the lifecycle).
fn lane_offline(state: &EngineCtx, lane: &EngineLane) -> bool {
    use crate::activator::LaneState;
    state
        .capacity
        .executor(&lane.name)
        .is_some_and(|executor| matches!(executor.report().state, LaneState::Offline))
}

/// Why the lane's machine is going away, if it is: the gateway knows it is
/// off, a power-off runs on it, or the lane went offline since
/// `was_offline` was read (a lane that routing probes may read offline
/// while its engine answers). Nothing but a wake may connect to it then.
fn machine_going_off(
    state: &EngineCtx,
    lane: &EngineLane,
    was_offline: bool,
) -> Option<&'static str> {
    use crate::activator::LaneState;
    let executor = state.capacity.executor(&lane.name)?;
    if executor.machine_known_off() {
        return Some("off");
    }
    match executor.report().state {
        LaneState::PoweringOff => Some("powering off"),
        LaneState::Offline if !was_offline => Some("off"),
        _ => None,
    }
}

/// The engine controls of every lane: shared state plus the seams.
pub struct EngineControls {
    runner: Arc<dyn SettingsCommandRunner>,
    client: reqwest::Client,
    state: Mutex<ControlState>,
    /// One apply at a time per lane (the overlay is one document per lane,
    /// and overlapping writes of it would race).
    apply_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    settle_marks: Arc<SettleMarks>,
}

impl Default for EngineControls {
    fn default() -> Self {
        Self::with_runner(Arc::new(ShellSettingsRunner))
    }
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
            apply_locks: Mutex::new(HashMap::new()),
            settle_marks: Arc::default(),
        }
    }

    fn apply_lock(&self, lane: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.apply_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(lane.to_string())
            .or_default()
            .clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ControlState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the lane's machine is up. Never wakes: a lane in the
    /// lifecycle answers from its state; one outside it, or one whose state
    /// the gateway has not seen yet, gets one health GET (a sleeping machine
    /// does not answer it). A machine the gateway knows is off is never
    /// called: it is asleep.
    async fn awake(&self, state: &EngineCtx, lane: &EngineLane) -> Awake {
        use crate::activator::LaneState;
        if let Some(executor) = state.capacity.executor(&lane.name) {
            if executor.machine_known_off() {
                return Awake::Asleep("asleep");
            }
            match executor.report().state {
                LaneState::Healthy | LaneState::Draining | LaneState::IdleCountdown => {
                    return Awake::Up
                }
                LaneState::Waking => return Awake::Asleep("waking"),
                LaneState::PoweringOff => return Awake::Asleep("powering off"),
                // A lane routing probes: trust what its probe last saw.
                LaneState::Offline => match executor.report().observed_healthy {
                    Some(true) => return Awake::Up,
                    Some(false) => return Awake::Asleep("asleep"),
                    None => {}
                },
                LaneState::Degraded => {}
            }
        }
        match self
            .client
            .get(format!("{}/health", lane.base))
            .timeout(Duration::from_secs(1))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => Awake::Up,
            _ => Awake::Asleep("asleep"),
        }
    }

    /// One call to the engine, with the lane provider's credential: the key
    /// its text requests carry (`api_key_env` / `api_key` with
    /// `auth_scheme`), see [`crate::scheme_auth::engine_call_scheme`]. Every
    /// engine-control call (listing, re-read, load, unload, `/props`) goes
    /// through here.
    async fn call(
        &self,
        state: &EngineCtx,
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
            match crate::scheme_auth::SchemeCredential::resolve_for_engine(
                &snap.resolver,
                provider,
                &lane.name,
            )
            .await
            {
                Ok(Some(found)) => {
                    request = found.apply(request);
                    credential = Some(found);
                }
                Ok(None) => {}
                // The error names the provider, never the secret.
                Err(error) => tracing::warn!(
                    lane = %lane.name,
                    %error,
                    "engine call sent without the provider's credential"
                ),
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
    async fn models(&self, state: &EngineCtx, lane: &EngineLane) -> Result<Vec<Value>, String> {
        self.models_within(state, lane, Duration::from_secs(10))
            .await
    }

    async fn models_within(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        timeout: Duration,
    ) -> Result<Vec<Value>, String> {
        let listing = self
            .call(state, lane, reqwest::Method::GET, "/models", None, timeout)
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

    /// Take the engine's model listing again after the gateway loaded,
    /// unloaded or applied something on the lane (it just talked to the
    /// engine, so the machine is up). A failure keeps the last listing.
    async fn refresh_listing(&self, state: &EngineCtx, lane: &EngineLane) {
        if let Err(e) = self.models_within(state, lane, LISTING_TIMEOUT).await {
            tracing::debug!(lane = %lane.name, error = %e, "engine listing not refreshed");
        }
    }

    /// One pass of the listing poller: `GET /models` (short timeout) on
    /// every engine lane whose machine is up and not busy
    /// ([`listing_pollable`]). Never wakes, and never calls a lane that is
    /// off, waking, powering off or draining.
    pub(crate) async fn poll_listings(&self, state: &EngineCtx) {
        let snap = state.snapshot();
        let lanes: Vec<EngineLane> = snap
            .config
            .local_executors
            .iter()
            .filter_map(|lane| engine_lane(&snap.config, &lane.name))
            .filter(|lane| listing_pollable(state, &lane.name))
            .collect();
        futures::future::join_all(lanes.iter().map(|lane| self.refresh_listing(state, lane))).await;
    }

    /// The engine's build (`GET /props` `build_info`, e.g. `b12359-b1b770d90`),
    /// kept for the tuning ledger. Best effort: a failure keeps the last one.
    async fn refresh_build(&self, state: &EngineCtx, lane: &EngineLane) {
        if let Ok(props) = self
            .call(
                state,
                lane,
                reqwest::Method::GET,
                "/props",
                None,
                Duration::from_secs(2),
            )
            .await
        {
            if let Some(build) = props["build_info"].as_str() {
                self.lock()
                    .builds
                    .insert(lane.name.clone(), build.chars().take(80).collect());
            }
        }
    }

    pub(crate) fn cached_models(&self, lane: &str) -> Option<(i64, Vec<Value>)> {
        self.lock().models.get(lane).cloned()
    }

    /// The engine's model listing read now (bounded by `timeout`) for
    /// token-pool admission, when a model's numbers are not known yet. It
    /// is not kept as the engine control's listing: the engine control's own
    /// decisions (applies, loads, the loaded-model view) see only the
    /// listings they and the poller take. The caller checks that the
    /// machine is up.
    pub(crate) async fn read_listing_for_pool(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        timeout: Duration,
    ) -> Result<Vec<Value>, String> {
        let listing = self
            .call(state, lane, reqwest::Method::GET, "/models", None, timeout)
            .await?;
        Ok(listing
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// The engine's own token count of `text` for engine model `model`
    /// (llama.cpp `POST /tokenize`, routed by the router to the model),
    /// bounded by `timeout`. `None` when the call failed or answered no
    /// token list.
    pub(crate) async fn count_tokens(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        model: &str,
        text: String,
        timeout: Duration,
    ) -> Option<u64> {
        let body = json!({"model": model, "content": text, "add_special": false});
        match self
            .call(
                state,
                lane,
                reqwest::Method::POST,
                "/tokenize",
                Some(body),
                timeout,
            )
            .await
        {
            Ok(answer) => answer["tokens"]
                .as_array()
                .map(|tokens| tokens.len() as u64),
            Err(e) => {
                tracing::debug!(lane = %lane.name, model, error = %e, "engine token count failed; using the estimate");
                None
            }
        }
    }

    /// A model's preset section as the overlay writes it (`key -> value`,
    /// without dashes) from its settings document.
    pub(crate) fn preset_of(
        kind: EngineControlKind,
        doc: &ModelSettingsDoc,
    ) -> BTreeMap<String, String> {
        model_preset(kind_ops(kind), doc)
    }

    /// The managed overlay of a lane: every managed load-time setting of
    /// its models, and its variants (base section plus their keys).
    pub(crate) fn overlay(&self, state: &EngineCtx, lane: &EngineLane) -> Value {
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
            let pairs = model_preset(ops, &entry.doc);
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

    /// Apply the lane's load settings (under the lane's apply lock).
    pub(crate) async fn apply_lane(&self, state: &EngineCtx, lane: &EngineLane) -> Value {
        let _guard = self.apply_lock(&lane.name).lock_owned().await;
        // No request waits at a gate for this apply: nothing settles.
        let (result, _) = self.apply_locked(state, lane, Settle::none()).await;
        result
    }

    /// Wait, until `deadline`, while a settle holds `key`'s gate (its first
    /// pass unloads a model it finds loaded). `false`: still held then.
    async fn wait_settled(&self, key: &str, deadline: tokio::time::Instant) -> bool {
        loop {
            let opened = self.settle_marks.opened.notified();
            tokio::pin!(opened);
            opened.as_mut().enable();
            if !self.settle_marks.holds(key) {
                return true;
            }
            if tokio::time::timeout_at(deadline, opened).await.is_err() {
                return !self.settle_marks.holds(key);
            }
        }
    }

    /// Write the lane's overlay through its `settings_command`, then ask an
    /// awake engine to re-read its presets. Never wakes, and never makes the
    /// engine re-read a preset a request is using: llama.cpp unloads a
    /// running model whose preset changed, so every model whose section
    /// changes is held first (no request is admitted to or dispatched on
    /// it), and one with a request dispatched keeps its old section until
    /// it is idle (`deferred`; retried when its last request ends). The
    /// caller holds the lane's apply lock.
    ///
    /// `settle`: the request path's part (see [`Settle`]). Every hold it
    /// carries opens here, together with the apply's own holds, except
    /// those of the models this apply rewrote that may still run their old
    /// process: they come back as [`Settling`], for the caller to pass to
    /// [`Self::settle`] once it has let go of the apply lock.
    async fn apply_locked(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        settle: Settle,
    ) -> (Value, Option<Settling>) {
        let (result, settling) = self.apply_locked_once(state, lane, settle).await;
        // The engine re-read its presets (and unloaded models whose preset
        // changed): the loaded-model view follows at once. Every hold but
        // the settling ones is open by now, so this listing delays no
        // waiting request; a settle takes the listing again itself, once
        // its own gates are open.
        if result["reloaded"] == json!(true) && settling.is_none() {
            self.refresh_listing(state, lane).await;
        }
        // A held-back model whose last request ended while this apply ran
        // found nothing held back yet (its idle hook came too early): retry
        // it now, after this apply lets go of the lock.
        let registry = state.engine.model_settings();
        let idle_now = self.lock().deferred.get(&lane.name).is_some_and(|models| {
            models
                .iter()
                .any(|model| registry.dispatched(&format!("{}/{model}", lane.name)) == 0)
        });
        if idle_now {
            state.engines.retry_in_background(state.clone(), &lane.name);
        }
        (result, settling)
    }

    async fn apply_locked_once(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        settle: Settle,
    ) -> (Value, Option<Settling>) {
        use sb_runtime::model_settings::GateKind;
        self.gc_variants(state, lane);
        let overlay = self.overlay(state, lane);
        let desired = overlay["models"].as_object().cloned().unwrap_or_default();
        let names: Vec<String> = desired.keys().cloned().collect();
        let mut result = json!({"at_ms": now_ms(), "models": names});
        let awake = self.awake(state, lane).await;
        let up = matches!(awake, Awake::Up);
        self.lock()
            .last_attempt
            .insert(lane.name.clone(), (now_ms(), up));
        let Some(command) = lane.engine.settings_command.clone() else {
            result["ok"] = json!(false);
            result["error"] = json!("the lane has no engine.settings_command");
            self.finish_apply(lane, &mut result, Reached::Failed, BTreeSet::new());
            return (result, None);
        };
        let last = self.lock().applied.get(&lane.name).cloned();
        let changed: BTreeSet<String> = match &last {
            Some(last) => desired
                .keys()
                .chain(last.keys())
                .filter(|model| desired.get(*model) != last.get(*model))
                .cloned()
                .collect(),
            None => desired.keys().cloned().collect(),
        };
        if last.is_some() && changed.is_empty() {
            result["ok"] = json!(true);
            result["reloaded"] = json!(false);
            result["note"] = json!("nothing changed for the engine");
            self.finish_apply(lane, &mut result, Reached::Yes, BTreeSet::new());
            return (result, None);
        }
        // Hold every model whose section changes. A model with a request
        // dispatched on it keeps its old section until it is idle.
        let registry = state.engine.model_settings();
        let mut held = Vec::new();
        let mut deferred = BTreeSet::new();
        for model in &changed {
            let gate = registry.close(&format!("{}/{model}", lane.name), GateKind::Hold);
            if gate.busy() > 0 {
                deferred.insert(model.clone());
            } else {
                held.push(gate);
            }
        }
        // The models whose section this apply writes (a load right after
        // it must wait for the engine to end their old process).
        result["changed"] = json!(changed.difference(&deferred).collect::<Vec<_>>());
        let Some(last) = last else {
            if !deferred.is_empty() {
                // The first apply since the gateway started: what the engine
                // runs for a busy model is unknown, so nothing is written
                // until it is idle.
                drop(held);
                result["ok"] = json!(true);
                result["reloaded"] = json!(false);
                result["note"] = json!(format!(
                    "held back: {} serve(s) requests; applied when idle",
                    deferred.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
                self.finish_apply(lane, &mut result, Reached::Yes, deferred);
                return (result, None);
            }
            return self
                .write_and_reread(
                    state, lane, &command, &overlay, desired, held, awake, result, deferred, settle,
                )
                .await;
        };
        let mut applied = desired;
        for model in &deferred {
            match last.get(model) {
                Some(section) => {
                    applied.insert(model.clone(), section.clone());
                }
                None => {
                    applied.remove(model);
                }
            }
        }
        if applied == last {
            drop(held);
            result["ok"] = json!(true);
            result["reloaded"] = json!(false);
            result["note"] = json!(format!(
                "held back: {} serve(s) requests; applied when idle",
                deferred.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
            self.finish_apply(lane, &mut result, Reached::Yes, deferred);
            return (result, None);
        }
        self.write_and_reread(
            state, lane, &command, &overlay, applied, held, awake, result, deferred, settle,
        )
        .await
    }

    /// Run the settings command with `models` as the overlay's models,
    /// then make an awake engine re-read its presets, while `held` keeps
    /// every changed model free of requests.
    #[allow(clippy::too_many_arguments)]
    async fn write_and_reread(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        command: &str,
        overlay: &Value,
        models: Map<String, Value>,
        held: Vec<sb_runtime::model_settings::ModelGate>,
        awake: Awake,
        mut result: Value,
        deferred: BTreeSet<String>,
        settle: Settle,
    ) -> (Value, Option<Settling>) {
        let mut document = overlay.clone();
        document["models"] = Value::Object(models.clone());
        let input = serde_json::to_vec(&document).unwrap_or_default();
        let secrets = secret_values(Some(&state.snapshot().config));
        if let Err(e) = self
            .runner
            .run_scrubbed(command, &input, lane.command_timeout, &secrets)
            .await
        {
            drop(held);
            result["ok"] = json!(false);
            result["error"] = json!(format!("settings_command failed: {e}"));
            self.finish_apply(lane, &mut result, Reached::Failed, deferred);
            return (result, None);
        }
        let reached = match awake {
            Awake::Up => match self
                .call(
                    state,
                    lane,
                    reqwest::Method::GET,
                    "/models?reload=1",
                    None,
                    REREAD_TIMEOUT,
                )
                .await
            {
                Ok(_) => {
                    result["ok"] = json!(true);
                    result["reloaded"] = json!(true);
                    self.lock().applied.insert(lane.name.clone(), models);
                    Reached::Yes
                }
                Err(e) => {
                    // The file changed but the engine did not re-read it:
                    // `applied` stays as it was, so the next apply checks
                    // these models again before any re-read.
                    result["ok"] = json!(false);
                    result["reloaded"] = json!(false);
                    result["error"] = json!(format!(
                        "settings written; the engine did not re-read them: {e}"
                    ));
                    Reached::Failed
                }
            },
            // Waking: the engine may be starting and have read its presets
            // before this write. `applied` stays as it was, so the apply
            // when the lane is up writes and re-reads these models again.
            Awake::Asleep(why @ "waking") => {
                result["ok"] = json!(true);
                result["reloaded"] = json!(false);
                result["note"] = json!(format!(
                    "settings written; the machine is {why}, so the engine re-reads them once \
                     it is up"
                ));
                Reached::WhenUp
            }
            Awake::Asleep(why) => {
                result["ok"] = json!(true);
                result["reloaded"] = json!(false);
                result["note"] = json!(format!(
                    "settings written; the machine is {why} and the engine reads them when it starts"
                ));
                self.lock().applied.insert(lane.name.clone(), models);
                Reached::Yes
            }
        };
        // Every hold opens now, the caller's included, but those of the
        // rewritten models a request waits for: they stay closed until the
        // model's old process is gone (`Self::settle`, run by the caller
        // once it lets go of the apply lock).
        let mut settling = None;
        if result["reloaded"] == json!(true) {
            let rewritten: BTreeSet<String> = held
                .iter()
                .filter_map(|gate| split_target(gate.key()).map(|(_, m)| m.to_string()))
                .filter(|model| settle.models.contains(model))
                .collect();
            let mut gates: BTreeMap<String, Vec<sb_runtime::model_settings::ModelGate>> =
                BTreeMap::new();
            for gate in held.into_iter().chain(settle.holds) {
                match split_target(gate.key()).map(|(_, m)| m.to_string()) {
                    Some(model) if rewritten.contains(&model) => {
                        gates.entry(model).or_default().push(gate);
                    }
                    _ => drop(gate),
                }
            }
            if !gates.is_empty() {
                // Marked before the apply lock is let go: a load that took
                // it next waits for these (see `load_target`).
                let gates = gates
                    .into_iter()
                    .map(|(model, gates)| {
                        let mark = self.settle_marks.mark(format!("{}/{model}", lane.name));
                        (
                            model,
                            SettleHold {
                                _gates: gates,
                                _mark: mark,
                            },
                        )
                    })
                    .collect();
                settling = Some(Settling {
                    gates,
                    started: settle.started,
                    at_ms: result["at_ms"].clone(),
                });
            }
        } else {
            drop(held);
            drop(settle);
        }
        self.finish_apply(lane, &mut result, reached, deferred);
        (result, settling)
    }

    /// The rest of a request-path apply, run once the caller has let go of
    /// the lane's apply lock: end the old process of each settling model and
    /// open its gate as soon as the engine no longer lists that process
    /// running. Each model opens on its own, so a model that does not run
    /// never waits for another's exit. Bounded by what a request waiting at
    /// the gate allows ([`settle_bound`], counted from the first hold), and
    /// ended at once by a machine that is going off. What did not settle is
    /// noted on the apply's record (`last_apply.settle`).
    ///
    /// It runs outside the apply lock, so a settings write or a request with
    /// its own load settings (`ensure_variant`) never waits for an old
    /// process to exit, as on an apply with no settle. That is safe: the
    /// settle changes no apply state (what the engine read and the apply's
    /// record are written before it), and it unloads only models whose
    /// gates it holds; a later apply that rewrites one of them closes its
    /// own hold on it.
    async fn settle(&self, state: &EngineCtx, lane: &EngineLane, settling: Settling) {
        let Settling {
            mut gates,
            started,
            at_ms,
        } = settling;
        let bound = settle_bound(
            lane_gate_wait(state, lane),
            Duration::from_secs(lane.engine.load_timeout_secs),
        );
        let was_offline = lane_offline(state, lane);
        let (note, off) = self
            .settle_models(state, lane, &mut gates, started + bound, bound, was_offline)
            .await;
        // Every gate is open from here: nothing below delays a request.
        drop(gates);
        if let Some(note) = note {
            let mut st = self.lock();
            if let Some(record) = st.last_apply.get_mut(&lane.name) {
                if record["at_ms"] == at_ms {
                    record["settle"] = json!(note);
                }
            }
        }
        if !off {
            self.refresh_listing(state, lane).await;
        }
    }

    /// The settle's loop (see [`Self::settle`]): each pass first checks the
    /// machine is not going off, then the deadline, then lists the engine's
    /// models and opens the gate of every model that no longer runs. The
    /// first pass unloads each model that is still loaded. The `bool` is
    /// whether it stopped for a machine going off.
    async fn settle_models(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        gates: &mut BTreeMap<String, SettleHold>,
        deadline: tokio::time::Instant,
        bound: Duration,
        was_offline: bool,
    ) -> (Option<String>, bool) {
        let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
        let names =
            |gates: &BTreeMap<String, _>| gates.keys().cloned().collect::<Vec<_>>().join(", ");
        let mut first = true;
        loop {
            if gates.is_empty() {
                return (None, false);
            }
            // Checked before every engine call: a machine that is going
            // away is never connected to.
            if let Some(why) = machine_going_off(state, lane, was_offline) {
                let names = names(gates);
                tracing::info!(lane = %lane.name, models = %names, "engine control: the machine is {why}; reopening the gate without waiting for the old process");
                return (
                    Some(format!(
                        "the machine is {why}: stopped waiting for the old process of {names}"
                    )),
                    true,
                );
            }
            if tokio::time::Instant::now() >= deadline {
                let names = names(gates);
                tracing::warn!(lane = %lane.name, models = %names, "engine control: old process still runs after a re-read; reopening its gate");
                return (
                    Some(format!(
                        "the old process of {names} still ran {} s after the re-read",
                        bound.as_secs()
                    )),
                    false,
                );
            }
            let listing = match tokio::time::timeout(left(), self.models(state, lane)).await {
                Ok(Ok(listing)) => listing,
                Ok(Err(e)) if first => {
                    return (
                        Some(format!("could not list the engine's models: {e}")),
                        false,
                    )
                }
                Err(_) if first => {
                    return (
                        Some("the engine's listing did not answer in time".to_string()),
                        false,
                    )
                }
                _ => {
                    tokio::time::sleep(SETTLE_POLL.min(left())).await;
                    continue;
                }
            };
            if std::mem::take(&mut first) {
                for model in gates.keys() {
                    let status = model_status(&listing, model);
                    if !matches!(status.as_deref(), Some("loaded" | "sleeping" | "loading")) {
                        continue;
                    }
                    if machine_going_off(state, lane, was_offline).is_some() {
                        break;
                    }
                    // The re-read may end it or keep it (an engine that saw
                    // no change): unload it, so it surely loads again with
                    // its new section. After a gateway restart and a failed
                    // apply (what the engine read is unknown) this unloads
                    // every loaded model the apply held, changed or not: the
                    // gateway cannot tell which section changed, since the
                    // listing shows only the checked arguments, and a change
                    // outside them (or a key taken out) ends the process
                    // just the same.
                    let unload = self.call(
                        state,
                        lane,
                        reqwest::Method::POST,
                        "/models/unload",
                        Some(json!({"model": model})),
                        left().max(Duration::from_millis(1)),
                    );
                    if let Err(e) = unload.await {
                        tracing::warn!(lane = %lane.name, model = %model, error = %e, "engine control: unload after a re-read failed");
                    }
                }
            }
            // A model whose process no longer runs opens now.
            gates.retain(|model, _| process_running(model_status(&listing, model).as_deref()));
            if !gates.is_empty() {
                tokio::time::sleep(SETTLE_POLL.min(left())).await;
            }
        }
    }

    fn finish_apply(
        &self,
        lane: &EngineLane,
        result: &mut Value,
        reached: Reached,
        deferred: BTreeSet<String>,
    ) {
        let failed = reached != Reached::Yes;
        if !deferred.is_empty() {
            result["deferred"] = json!(deferred.iter().collect::<Vec<_>>());
        }
        result["pending"] = json!(failed || !deferred.is_empty());
        let mut st = self.lock();
        st.last_apply.insert(lane.name.clone(), result.clone());
        st.failed_apply.insert(lane.name.clone(), failed);
        st.deferred.insert(lane.name.clone(), deferred);
        let failures = st.apply_failures.entry(lane.name.clone()).or_default();
        match reached {
            Reached::Yes => *failures = 0,
            Reached::Failed => *failures = failures.saturating_add(1),
            Reached::WhenUp => {}
        }
    }

    /// The lane has an apply that did not fully reach the engine.
    fn pending_apply(&self, lane: &str) -> bool {
        let st = self.lock();
        st.failed_apply.get(lane).copied().unwrap_or(false)
            || st
                .deferred
                .get(lane)
                .is_some_and(|models| !models.is_empty())
    }

    /// A failed apply is due again before a request: its backoff (15 s,
    /// doubling with each failure in a row, at most 10 min) has passed. A
    /// lane that comes up retries at once from its up hook instead.
    fn failed_apply_due(&self, lane: &str) -> bool {
        let st = self.lock();
        if !st.failed_apply.get(lane).copied().unwrap_or(false) {
            return false;
        }
        let failures = st.apply_failures.get(lane).copied().unwrap_or(0);
        match st.last_attempt.get(lane) {
            Some((at, _)) => now_ms() - at >= apply_backoff_ms(failures),
            None => true,
        }
    }

    /// The models of a lane whose section the engine does not have yet:
    /// those an apply that did not reach the engine changed (against what
    /// the engine last read; every managed model when that is unknown), and
    /// those held back while busy.
    fn pending_models(&self, state: &EngineCtx, lane: &EngineLane) -> BTreeSet<String> {
        let overlay = self.overlay(state, lane);
        let desired = overlay["models"].as_object().cloned().unwrap_or_default();
        let st = self.lock();
        let mut out = BTreeSet::new();
        if st.failed_apply.get(&lane.name).copied().unwrap_or(false) {
            match st.applied.get(&lane.name) {
                Some(applied) => out.extend(
                    desired
                        .keys()
                        .chain(applied.keys())
                        .filter(|model| desired.get(*model) != applied.get(*model))
                        .cloned(),
                ),
                None => out.extend(desired.keys().cloned()),
            }
        }
        if let Some(deferred) = st.deferred.get(&lane.name) {
            out.extend(deferred.iter().cloned());
        }
        out
    }

    /// Run the lane's apply again in the background when it is pending
    /// (the lane came up, a held-back model went idle). When no apply is
    /// running, the lane's apply lock and a hold on every pending model are
    /// taken here, before this returns: a request for one of them that
    /// reaches the lane first waits (bounded) for its new section instead of
    /// being served with the old one. The pending models are read once the
    /// lock is taken (an apply that just ended may have changed them). The
    /// lock is let go before the settle (see [`Self::settle`]).
    pub(crate) fn retry_in_background(self: &Arc<Self>, state: EngineCtx, lane_name: &str) {
        use sb_runtime::model_settings::GateKind;
        if !self.pending_apply(lane_name) {
            return;
        }
        let Some(lane) = engine_lane(&state.snapshot().config, lane_name) else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let lock = self.apply_lock(&lane.name);
        let taken = match lock.clone().try_lock_owned() {
            Ok(guard) => {
                let started = tokio::time::Instant::now();
                // Requests may wait for these models' new sections.
                let models = self.pending_models(&state, &lane);
                let registry = state.engine.model_settings();
                let holds: Vec<_> = models
                    .iter()
                    .map(|model| registry.close(&format!("{}/{model}", lane.name), GateKind::Hold))
                    .collect();
                Some((
                    guard,
                    Settle {
                        models,
                        started,
                        holds,
                    },
                ))
            }
            Err(_) => None,
        };
        let controls = self.clone();
        runtime.spawn(async move {
            let (guard, settle) = match taken {
                Some(taken) => taken,
                None => {
                    let guard = lock.lock_owned().await;
                    // No hold was taken here: the apply's own holds are the
                    // first a request may meet.
                    let settle = Settle {
                        models: controls.pending_models(&state, &lane),
                        started: tokio::time::Instant::now(),
                        holds: Vec::new(),
                    };
                    (guard, settle)
                }
            };
            if !controls.pending_apply(&lane.name) {
                return;
            }
            let (result, settling) = controls.apply_locked(&state, &lane, settle).await;
            drop(guard);
            tracing::info!(lane = %lane.name, ok = %result["ok"], pending = %result["pending"], "engine control: pending apply retried");
            if let Some(settling) = settling {
                controls.settle(&state, &lane, settling).await;
            }
        });
    }

    /// Before a request for `model` takes its lane's place: when an apply
    /// failed to reach the engine, `model`'s own section is among what it
    /// did not deliver, its backoff has passed and no apply is running (the
    /// lock is never waited for), run it now, so the model loads with its
    /// current settings, and let the model's old process end before the
    /// request goes on (the settle, after the lock is let go). A model whose
    /// section the engine has is never delayed.
    pub(crate) async fn before_dispatch(&self, state: &EngineCtx, provider_id: &str, model: &str) {
        if !self.failed_apply_due(provider_id) {
            return;
        }
        let Some(lane) = engine_lane(&state.snapshot().config, provider_id) else {
            return;
        };
        if !self.pending_models(state, &lane).contains(model) {
            return;
        }
        let lock = self.apply_lock(&lane.name);
        let Ok(guard) = lock.try_lock() else {
            return;
        };
        if !self.failed_apply_due(&lane.name) || !self.pending_models(state, &lane).contains(model)
        {
            return;
        }
        let settle = Settle {
            models: BTreeSet::from([model.to_string()]),
            started: tokio::time::Instant::now(),
            holds: Vec::new(),
        };
        let (_, settling) = self.apply_locked(state, &lane, settle).await;
        drop(guard);
        if let Some(settling) = settling {
            self.settle(state, &lane, settling).await;
        }
    }

    /// The last request dispatched on `key` (`lane/model`) ended: retry an
    /// apply held back for that model.
    pub(crate) fn model_idle(self: &Arc<Self>, state: EngineCtx, key: &str) {
        let Some((lane, model)) = split_target(key) else {
            return;
        };
        let held_back = self
            .lock()
            .deferred
            .get(lane)
            .is_some_and(|models| models.contains(model));
        if held_back {
            self.retry_in_background(state, lane);
        }
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

    /// Unload a model and wait until the engine says its process ended.
    async fn unload_model(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        model: &str,
    ) -> Result<(), String> {
        self.call(
            state,
            lane,
            reqwest::Method::POST,
            "/models/unload",
            Some(json!({"model": model})),
            Duration::from_secs(60),
        )
        .await?;
        self.wait_unloaded(state, lane, model).await
    }

    /// Wait (at most the lane's `load_timeout_secs`) until the engine's
    /// listing no longer shows `model` running. llama.cpp's router answers
    /// an unload at once and ends the model's process after it; a load sent
    /// before the process exits gets 400 "model is already running".
    async fn wait_unloaded(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        model: &str,
    ) -> Result<(), String> {
        let bound = lane.engine.load_timeout_secs;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(bound);
        loop {
            let models = self.models(state, lane).await?;
            if !process_running(model_status(&models, model).as_deref()) {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("`{model}` still runs {bound} s after its unload"));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Load a model: through the lane's place and GPU group like a request
    /// (held only while loading), then wait for the engine to say loaded.
    /// `running_is_loaded`: a model that already runs counts as loaded
    /// (llama.cpp answers 400 "model is already running"); false where the
    /// caller made sure the old process ended, so a model that still runs is
    /// one whose old settings stayed.
    async fn load_model(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        model: &str,
        running_is_loaded: bool,
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
        if let Err(e) = self
            .call(
                state,
                lane,
                reqwest::Method::POST,
                "/models/load",
                Some(json!({"model": model})),
                Duration::from_secs(30),
            )
            .await
        {
            // llama.cpp answers 400 "model is already running" to a load of
            // a model that runs. Every unload here waits for the model's
            // process to end, so a model that runs is the loaded one: the
            // listing below says so. Not after an apply's unload: there the
            // process that runs is the old one, with the old settings.
            if !running_is_loaded || !e.contains("already running") {
                return Err(e);
            }
        }
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

    /// `apply: now`: drain the model (no new request is admitted; the ones
    /// admitted, queued and waking included, end first, bounded by
    /// `apply_wait_secs`), apply, then unload and load it again if it was
    /// loaded, all before the gate reopens. A model still busy at the bound
    /// is not touched: the apply then holds its new section back until it
    /// is idle (`reason: model_busy`, pending).
    async fn apply_now(
        &self,
        state: &EngineCtx,
        lane: &EngineLane,
        target: &str,
        model: &str,
        wake: bool,
    ) -> Value {
        use sb_runtime::model_settings::GateKind;
        let _woken = match self.awake(state, lane).await {
            Awake::Up => None,
            Awake::Asleep(why) if !wake => {
                let mut value = self.apply_lane(state, lane).await;
                value["note"] = json!(format!(
                    "the machine is {why}; the model starts with the new settings when it next loads"
                ));
                return value;
            }
            Awake::Asleep(_) => match self.wake(state, lane).await {
                Ok(guard) => Some(guard),
                Err(e) => {
                    let mut value = self.apply_lane(state, lane).await;
                    value["ok"] = json!(false);
                    value["error"] = json!(e);
                    return value;
                }
            },
        };
        let registry = state.engine.model_settings();
        let gate = registry.close(&format!("{}/{model}", lane.name), GateKind::Drain);
        if let Err(busy) = gate
            .wait_idle(Duration::from_secs(lane.engine.apply_wait_secs))
            .await
        {
            drop(gate);
            let mut value = self.apply_lane(state, lane).await;
            value["reloaded"] = json!(false);
            value["reason"] = json!("model_busy");
            value["pending"] = json!(true);
            value["busy"] = json!(busy);
            return value;
        }
        let was_loaded = match self.models(state, lane).await {
            Ok(models) => matches!(
                model_status(&models, model).as_deref(),
                Some("loaded" | "sleeping" | "loading")
            ),
            Err(e) => {
                let mut value = self.apply_lane(state, lane).await;
                value["ok"] = json!(false);
                value["error"] = json!(e);
                return value;
            }
        };
        let mut value = self.apply_lane(state, lane).await;
        if value["ok"] != json!(true) {
            return value;
        }
        if !was_loaded {
            value["note"] =
                json!("the model is not loaded; it loads with the new settings next time");
            return value;
        }
        // The re-read unloads a running model whose preset changed. llama.cpp's
        // router answers an unload at once and ends the model's process after
        // it; until then it lists the model as running and refuses a load (400
        // "model is already running"). So unload here only a model that still
        // runs, and load only once its process ended.
        let still = match self.models(state, lane).await {
            Ok(models) => model_status(&models, model),
            Err(e) => {
                value["ok"] = json!(false);
                value["error"] = json!(e);
                return value;
            }
        };
        let ended = if matches!(still.as_deref(), Some("loaded" | "sleeping" | "loading")) {
            self.unload_model(state, lane, model).await
        } else if process_running(still.as_deref()) {
            self.wait_unloaded(state, lane, model).await
        } else {
            Ok(())
        };
        if let Err(e) = ended {
            value["ok"] = json!(false);
            value["error"] = json!(e);
            return value;
        }
        match self.load_model(state, lane, model, false).await {
            Ok(row) => {
                let wanted = registry
                    .get(target)
                    .map(|e| model_preset(kind_ops(lane.engine.kind), &e.doc))
                    .unwrap_or_default();
                match args_mismatch(&wanted, &row) {
                    Some(why) => {
                        value["ok"] = json!(false);
                        value["reason"] = json!("old_settings");
                        value["error"] =
                            json!(format!("the engine still runs the old settings: {why}"));
                    }
                    None => {
                        self.mark_loaded(target, registry.revision(target));
                        value["reloaded_model"] = json!(true);
                        value["note"] = json!("reloaded with the new settings");
                    }
                }
            }
            Err(e) => {
                value["ok"] = json!(false);
                value["error"] = json!(e);
                self.refresh_listing(state, lane).await;
            }
        }
        drop(gate);
        value
    }

    /// Wake the lane's machine and wait for it (bounded by its boot
    /// timeout). The guard counts as lane activity while held.
    async fn wake(
        &self,
        state: &EngineCtx,
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

/// Whether a listing status means the engine still runs the model's
/// process (`unloading`: a router that names the time between an unload and
/// the process's exit).
fn process_running(status: Option<&str>) -> bool {
    matches!(
        status,
        Some("loaded" | "sleeping" | "loading" | "unloading")
    )
}

/// The load settings checked against a loaded model's argument list, by
/// preset key, with every name llama-server takes for them. The others
/// (`load-mode`, `fit`, file paths, ...) are not listed the same way, so
/// they are not checked.
const CHECKED_ARGS: &[(&str, &[&str])] = &[
    ("ctx-size", &["--ctx-size", "-c"]),
    ("n-gpu-layers", &["--n-gpu-layers", "--gpu-layers", "-ngl"]),
    ("cache-type-k", &["--cache-type-k", "-ctk"]),
    ("cache-type-v", &["--cache-type-v", "-ctv"]),
    ("flash-attn", &["--flash-attn", "-fa"]),
    ("batch-size", &["--batch-size", "-b"]),
    ("ubatch-size", &["--ubatch-size", "-ub"]),
    ("parallel", &["--parallel", "-np"]),
    ("threads", &["--threads", "-t"]),
    ("threads-batch", &["--threads-batch", "-tb"]),
    ("spec-type", &["--spec-type"]),
    (
        "spec-draft-n-max",
        &["--spec-draft-n-max", "--draft-max", "--draft", "--draft-n"],
    ),
    (
        "spec-draft-n-min",
        &["--spec-draft-n-min", "--draft-min", "--draft-n-min"],
    ),
];

/// Why a loaded model's arguments (`status.args`, the router's command
/// line for it) differ from the load settings `wanted` (preset keys to
/// values): `None` when every checked key matches, or when the engine
/// lists no arguments (nothing to check).
fn args_mismatch(wanted: &BTreeMap<String, String>, row: &Value) -> Option<String> {
    let args: Vec<&str> = row["status"]["args"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if args.is_empty() {
        return None;
    }
    // Each wanted key under the checked argument it is: its preset key, or
    // another long name of it (`gpu-layers` for `n-gpu-layers`, as raw
    // engine arguments may spell it).
    let mut checked: BTreeMap<&str, (&String, &String, &[&str])> = BTreeMap::new();
    let mut unsure = BTreeSet::new();
    for (key, want) in wanted {
        let long = format!("--{key}");
        let Some((name, names)) = CHECKED_ARGS
            .iter()
            .find(|(name, names)| name == key || names.contains(&long.as_str()))
        else {
            continue;
        };
        if let Some((_, other, _)) = checked.insert(name, (key, want, names)) {
            // Two spellings of one argument with different values: which
            // one the engine keeps is its own choice, so neither is checked.
            if !same_arg(other, Some(want)) {
                unsure.insert(*name);
            }
        }
    }
    let wrong: Vec<String> = checked
        .iter()
        .filter(|(name, _)| !unsure.contains(**name))
        .filter_map(|(_, (key, want, names))| {
            let seen = arg_value(&args, names);
            (!same_arg(want, seen.as_deref())).then(|| {
                format!(
                    "{key} {want} (it runs {})",
                    seen.as_deref().unwrap_or("without it")
                )
            })
        })
        .collect();
    (!wrong.is_empty()).then(|| wrong.join(", "))
}

/// A flag's last value in an argument list (`--name value` or
/// `--name=value`); a flag with no value after it reads "on".
fn arg_value(args: &[&str], names: &[&str]) -> Option<String> {
    let mut out = None;
    for (i, arg) in args.iter().enumerate() {
        for name in names {
            if arg == name {
                out = Some(match args.get(i + 1) {
                    Some(v) if !v.starts_with('-') || v.parse::<f64>().is_ok() => v.to_string(),
                    _ => "on".to_string(),
                });
            } else if let Some(v) = arg.strip_prefix(&format!("{name}=")) {
                out = Some(v.to_string());
            }
        }
    }
    out
}

/// A wanted value and the one an argument list shows mean the same:
/// equal numbers, the same switch word (on/true/enabled), or "auto"/"all"
/// that a number may stand for. Absent is the same only as "auto".
fn same_arg(want: &str, seen: Option<&str>) -> bool {
    let want = want.trim().to_ascii_lowercase();
    let Some(seen) = seen.map(|v| v.trim().to_ascii_lowercase()) else {
        return want == "auto";
    };
    if let (Ok(a), Ok(b)) = (want.parse::<f64>(), seen.parse::<f64>()) {
        return a == b;
    }
    let switch = |v: &str| match v {
        "true" | "on" | "enabled" => "on".to_string(),
        "false" | "off" | "disabled" => "off".to_string(),
        other => other.to_string(),
    };
    let (want, seen) = (switch(&want), switch(&seen));
    want == seen
        || (matches!(want.as_str(), "auto" | "all") && seen.parse::<f64>().is_ok())
        || (matches!(seen.as_str(), "auto" | "all") && want.parse::<f64>().is_ok())
}

/// A model's status in an engine listing.
fn model_status(models: &[Value], model: &str) -> Option<String> {
    models
        .iter()
        .find(|row| row["id"] == model)
        .and_then(|row| row["status"]["value"].as_str())
        .map(str::to_string)
}

/// A model row's sidecar files (projector, draft, MTP head) by file name
/// only, never their paths: `sb ai tune` reads them to know an MTP head or a
/// draft exists.
fn sidecar_names(sidecars: &Value) -> Value {
    let names: Vec<Value> = sidecars
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str()
                .or_else(|| item["path"].as_str())
                .or_else(|| item["name"].as_str())
        })
        .map(|path| json!(path.rsplit(['/', '\\']).next().unwrap_or(path)))
        .collect();
    Value::Array(names)
}

/// Speculative decoding on one model, for `sb ai tune`: the type in force
/// (`spec_type`) and whether an MTP head (`mtp`) or a draft model
/// (`draft_model`) is known, with the sources that said so. First the
/// settings the gateway writes into the preset overlay (`load.spec_type`,
/// `load.spec_draft_model`, `engine_args` `spec-type` / `spec-draft-model`),
/// then a loaded instance's `status.args`, then the engine's `sidecars`
/// (a real router lists them only for cached Hugging Face models, never
/// for preset models).
fn speculative_view(doc: Option<&ModelSettingsDoc>, row: &Value) -> Value {
    let mut spec_type: Option<String> = None;
    let (mut mtp, mut draft) = (false, false);
    let mut sources: Vec<&str> = Vec::new();
    let mut found = |source: &'static str, ty: Option<String>, has_draft: bool| {
        let ty = ty.filter(|ty| !ty.trim().is_empty());
        if ty.is_none() && !has_draft {
            return;
        }
        sources.push(source);
        mtp |= ty.as_deref().is_some_and(|ty| ty.contains("mtp"));
        draft |= has_draft;
        if spec_type.is_none() {
            spec_type = ty;
        }
    };
    if let Some(doc) = doc {
        let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
        found(
            "settings",
            text(doc.load.get("spec_type")).or_else(|| text(doc.engine_args.get("spec-type"))),
            doc.load.contains_key("spec_draft_model")
                || doc.engine_args.contains_key("spec-draft-model")
                || doc.engine_args.contains_key("model-draft"),
        );
    }
    let args: Vec<&str> = row["status"]["args"]
        .as_array()
        .map(|args| args.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let flag = |names: &[&str]| -> Option<String> {
        args.iter().enumerate().find_map(|(i, arg)| {
            names.iter().find_map(|name| {
                if arg == name {
                    Some(args.get(i + 1).copied().unwrap_or_default().to_string())
                } else {
                    arg.strip_prefix(&format!("{name}=")).map(str::to_string)
                }
            })
        })
    };
    found(
        "args",
        flag(&["--spec-type"]),
        flag(&["--spec-draft-model", "--model-draft", "-md"]).is_some(),
    );
    let names: Vec<String> = sidecar_names(&row["sidecars"])
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_lowercase)
        .collect();
    let side_mtp = names.iter().any(|name| name.contains("mtp"));
    let side_draft = names.iter().any(|name| name.contains("draft"));
    if side_mtp || side_draft {
        sources.push("sidecars");
        mtp |= side_mtp;
        draft |= side_draft;
    }
    json!({
        "spec_type": spec_type,
        "mtp": mtp,
        "draft_model": draft,
        "sources": sources,
    })
}

/// `memory` of a model row, summed per measurement: bytes on GPU devices
/// and on the host, and the context it was measured at. `context_bytes` is
/// the GPU part of `bytes` that grows with the context (the KV cache), and
/// `total_bytes` the memory of the GPU devices the measurement saw (the
/// cards' size, so a client can tell how much room a setting leaves).
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
        let mut context = 0u64;
        let mut total = 0u64;
        for row in part["devices"].as_array().into_iter().flatten() {
            let bytes = ["model", "context", "compute"]
                .iter()
                .filter_map(|key| row[*key].as_u64())
                .sum::<u64>();
            if row["kind"] == "host" {
                host += bytes;
            } else {
                device += bytes;
                context += row["context"].as_u64().unwrap_or(0);
                total += row["total"].as_u64().unwrap_or(0);
            }
        }
        out[name] = json!({"n_ctx": part["n_ctx"], "bytes": device, "host_bytes": host,
                           "context_bytes": context, "total_bytes": total});
    }
    out
}

// ---------------------------------------------------------------- variants

/// A short stable hash (FNV-1a, 64 bit) of a load layer: the variant's
/// name suffix.
fn load_hash(load: &Map<String, Value>) -> String {
    let canonical: BTreeMap<&String, &Value> = load.iter().collect();
    let text = serde_json::to_string(&canonical).unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")[..8].to_string()
}

/// The numbers behind a fit verdict.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fit {
    pub needed_bytes: u64,
    pub budget_bytes: u64,
    pub ctx: u64,
    pub source: &'static str,
}

/// What the fit check could say about a variant.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FitCheck {
    /// The variant changes nothing that sizes the model's memory.
    NotNeeded,
    Checked(Fit),
    /// Nothing to check it with: no measurement, no estimate, or no known
    /// context for the model. The text says what is missing.
    Unknown(String),
}

fn cache_type<'a>(
    variant: &'a Map<String, Value>,
    base: &'a Map<String, Value>,
    key: &str,
) -> &'a str {
    variant
        .get(key)
        .or_else(|| base.get(key))
        .and_then(Value::as_str)
        .unwrap_or("f16")
}

/// The context a running model was started with, from its argument list
/// (`--ctx-size N` / `-c N`; 0 means the model's trained context).
fn args_ctx(row: &Value) -> Option<u64> {
    let args = row["status"]["args"].as_array()?;
    let mut iter = args.iter().filter_map(Value::as_str);
    while let Some(arg) = iter.next() {
        let value = match arg {
            "--ctx-size" | "-c" => iter.next(),
            other => other.strip_prefix("--ctx-size="),
        };
        if let Some(value) = value {
            return value.parse().ok();
        }
    }
    None
}

/// The model's trained context, from the engine's `max` measurement.
fn trained_ctx(row: &Value) -> Option<u64> {
    row["memory"]["max"]["n_ctx"].as_u64().filter(|n| *n > 0)
}

/// GPU bytes of one model row's measurement on the named devices
/// (`effective` when the engine has it, else `min`).
fn row_gpu_bytes(row: &Value, devices: &BTreeSet<String>) -> u64 {
    let memory = &row["memory"];
    let part = ["effective", "min"]
        .iter()
        .map(|name| &memory[*name])
        .find(|part| part["devices"].is_array());
    let Some(part) = part else {
        return 0;
    };
    part["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|device| device["kind"] != "host")
        .filter(|device| {
            device["name"]
                .as_str()
                .is_some_and(|name| devices.contains(name))
        })
        .map(|device| {
            ["model", "context", "compute"]
                .iter()
                .filter_map(|key| device[*key].as_u64())
                .sum::<u64>()
        })
        .sum()
}

/// Whether a variant fits on its machine.
///
/// From the engine's own memory measurement of the base model when there
/// is one: weights and compute as measured, the KV cache scaled from the
/// measured context to the variant's (and by the cache types), against 95%
/// of the GPU memory the measurement saw, minus what the other models the
/// engine has loaded on those GPUs use (the base stays loaded beside the
/// variant) and the policy's `reserve_bytes`. Else from the policy's
/// estimate (the `sb ai fit` KV math) against its `budget_bytes`. The
/// variant's context, when it sets none, is the base's own (its load
/// settings, else the running engine's arguments, else the model's trained
/// context), never a guess.
pub(crate) fn variant_fit(
    listing: Option<&[Value]>,
    base_model: &str,
    base_load: &Map<String, Value>,
    variant_load: &Map<String, Value>,
    policy: &sb_core::RequestLoadPolicy,
) -> FitCheck {
    let changes_size = ["ctx_size", "cache_type_k", "cache_type_v", "parallel"]
        .iter()
        .any(|key| variant_load.contains_key(*key));
    if !changes_size {
        return FitCheck::NotNeeded;
    }
    let row = listing.and_then(|models| models.iter().find(|row| row["id"] == base_model));
    let positive = |value: Option<&Value>| value.and_then(Value::as_u64).filter(|n| *n > 0);
    let ctx = positive(variant_load.get("ctx_size"))
        .or_else(|| positive(base_load.get("ctx_size")))
        .or_else(|| row.and_then(args_ctx).filter(|n| *n > 0))
        .or_else(|| row.and_then(trained_ctx));
    let (k_new, v_new) = (
        sb_core::cache_type_bytes(cache_type(variant_load, base_load, "cache_type_k")),
        sb_core::cache_type_bytes(cache_type(variant_load, base_load, "cache_type_v")),
    );
    let (k_base, v_base) = (
        sb_core::cache_type_bytes(cache_type(&Map::new(), base_load, "cache_type_k")),
        sb_core::cache_type_bytes(cache_type(&Map::new(), base_load, "cache_type_v")),
    );
    let measured = row
        .map(|row| &row["memory"])
        .filter(|memory| memory["status"] == "measured")
        .map(|memory| &memory["min"])
        .filter(|min| min["n_ctx"].as_u64().is_some_and(|n| n > 0));
    if let Some(min) = measured {
        let n_ctx = min["n_ctx"].as_u64().unwrap_or(1);
        let (mut fixed, mut context, mut total) = (0u64, 0u64, 0u64);
        let mut devices = BTreeSet::new();
        for device in min["devices"].as_array().into_iter().flatten() {
            if device["kind"] == "host" {
                continue;
            }
            fixed +=
                device["model"].as_u64().unwrap_or(0) + device["compute"].as_u64().unwrap_or(0);
            context += device["context"].as_u64().unwrap_or(0);
            total += device["total"].as_u64().unwrap_or(0);
            if let Some(name) = device["name"].as_str() {
                devices.insert(name.to_string());
            }
        }
        if total == 0 {
            return FitCheck::NotNeeded; // no GPU seen: host memory is not limited here
        }
        let Some(ctx) = ctx else {
            return FitCheck::Unknown(format!(
                "the context of `{base_model}` is unknown; set load.ctx_size on it"
            ));
        };
        let others: u64 = listing
            .into_iter()
            .flatten()
            .filter(|other| {
                matches!(
                    other["status"]["value"].as_str(),
                    Some("loaded" | "loading")
                )
            })
            .map(|other| row_gpu_bytes(other, &devices))
            .sum();
        let scale = (k_new + v_new) / (k_base + v_base);
        let kv = (context as f64 / n_ctx as f64 * ctx as f64 * scale) as u64;
        return FitCheck::Checked(Fit {
            needed_bytes: fixed + kv,
            budget_bytes: (total / 100 * 95)
                .saturating_sub(others)
                .saturating_sub(policy.reserve_bytes),
            ctx,
            source: "engine measurement",
        });
    }
    let Some(estimate) = policy.estimate.as_ref() else {
        return FitCheck::Unknown(format!(
            "the engine has not measured `{base_model}` and its request_load policy has no \
             estimate; load the model once (POST /v1/models/{{model}}/load) or add \
             request_load.estimate (numbers from `sb ai fit --json`)"
        ));
    };
    let Some(ctx) = ctx else {
        return FitCheck::Unknown(format!(
            "the context of `{base_model}` is unknown; set load.ctx_size on it"
        ));
    };
    let k = cache_type(variant_load, base_load, "cache_type_k");
    let v = cache_type(variant_load, base_load, "cache_type_v");
    FitCheck::Checked(Fit {
        needed_bytes: estimate.weights_bytes + estimate.kv_bytes(ctx, k, v),
        budget_bytes: estimate.budget_bytes.saturating_sub(policy.reserve_bytes),
        ctx,
        source: "estimate",
    })
}

/// A variant idle for its whole TTL: unused since it was made or last
/// used (whichever is later).
fn variant_expired(now_ms: i64, last_used_ms: Option<i64>, created_ms: i64, ttl_secs: u64) -> bool {
    let last = last_used_ms.unwrap_or(created_ms).max(created_ms);
    now_ms - last >= (ttl_secs as i64).saturating_mul(1000)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

impl EngineControls {
    /// Drop variants unused for their target's `variant_idle_ttl_secs`
    /// (or whose target no longer allows variants). Never one a request is
    /// admitted to (queued ones included: `ensure_variant` admits the
    /// request under the same lock). They leave the overlay on this apply.
    fn gc_variants(&self, state: &EngineCtx, lane: &EngineLane) {
        let registry = state.engine.model_settings();
        let now = now_ms();
        let mut st = self.lock();
        let Some(variants) = st.variants.get_mut(&lane.name) else {
            return;
        };
        variants.retain(|name, variant| {
            let key = format!("{}/{name}", lane.name);
            if registry.in_flight(&key) > 0 {
                return true;
            }
            let Some(policy) = registry
                .get(&variant.target)
                .and_then(|entry| entry.doc.request_load.clone())
            else {
                return false;
            };
            !variant_expired(
                now,
                registry.last_used_ms(&key),
                variant.created_ms,
                policy.variant_idle_ttl_secs,
            )
        });
    }

    /// Find or make the variant of `target` with `load`. `Ok` carries the
    /// request's admission on it, taken under the same lock the eviction
    /// and the idle sweep look at, so a variant a request waits for is
    /// never removed.
    pub(crate) async fn ensure_variant(
        &self,
        state: &EngineCtx,
        target: &sb_core::ExecutionTarget,
        load: &Map<String, Value>,
        policy: &sb_core::RequestLoadPolicy,
        role: &str,
    ) -> Result<sb_runtime::model_settings::VariantLease, sb_runtime::ExecError> {
        use sb_runtime::model_settings::VariantLease;
        use sb_runtime::ExecError;
        let snap = state.snapshot();
        let Some(lane) = engine_lane(&snap.config, &target.provider_id) else {
            return Err(ExecError::new(
                400,
                "no_engine_control",
                format!("`{}` has no engine control for load settings", target.id),
                None,
            ));
        };
        let name = format!("{}~{}", target.model, load_hash(load));
        let key = format!("{}/{name}", lane.name);
        let registry = state.engine.model_settings();
        let wait = Duration::from_secs(lane.engine.apply_wait_secs.max(1));
        let deadline = tokio::time::Instant::now() + wait;
        // An existing variant: admit the request under the lock (or wait
        // while its gate is closed).
        loop {
            let found = {
                let st = self.lock();
                st.variants
                    .get(&lane.name)
                    .is_some_and(|variants| variants.contains_key(&name))
                    .then(|| registry.try_admit(&key))
            };
            match found {
                Some(Some(admission)) => {
                    return Ok(VariantLease {
                        name,
                        admission,
                        note: None,
                    })
                }
                Some(None) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Some(None) => {
                    return Err(sb_runtime::ExecError::new(
                        503,
                        "model_paused",
                        format!("variant `{name}` is being reloaded; retry"),
                        None,
                    )
                    .with_retry_after(Duration::from_secs(5)))
                }
                None => break,
            }
        }
        // A new one: the base must be one the engine lists, and it must fit.
        let base_load = registry
            .get(&target.id)
            .map(|entry| entry.doc.load.clone())
            .unwrap_or_default();
        let listing = match self.awake(state, &lane).await {
            Awake::Up => self.models(state, &lane).await.ok(),
            Awake::Asleep(_) => self.cached_models(&lane.name).map(|(_, models)| models),
        };
        if let Some(models) = &listing {
            if !models.iter().any(|row| row["id"] == target.model.as_str()) {
                return Err(ExecError::new(
                    400,
                    "unknown_engine_model",
                    format!(
                        "the engine of lane `{}` does not list `{}`; a variant needs a model it \
                         has",
                        lane.name, target.model
                    ),
                    None,
                ));
            }
        }
        let mut note = None;
        match variant_fit(listing.as_deref(), &target.model, &base_load, load, policy) {
            FitCheck::NotNeeded => {}
            FitCheck::Checked(fit) if fit.needed_bytes > fit.budget_bytes => {
                return Err(ExecError::new(
                    409,
                    "would_not_fit",
                    format!(
                        "variant of `{}` would not fit: needs {} ({} bytes) at ctx {}, the \
                         machine has {} ({} bytes) for it ({})",
                        target.id,
                        gib(fit.needed_bytes),
                        fit.needed_bytes,
                        fit.ctx,
                        gib(fit.budget_bytes),
                        fit.budget_bytes,
                        fit.source
                    ),
                    None,
                ));
            }
            FitCheck::Checked(_) => {}
            FitCheck::Unknown(why) if role == "client" => {
                return Err(ExecError::new(
                    409,
                    "fit_unknown",
                    format!(
                        "cannot check that the variant of `{}` fits: {why}",
                        target.id
                    ),
                    None,
                ));
            }
            FitCheck::Unknown(_) => note = Some("fit=unchecked".to_string()),
        }
        // Make room under max_variants and register the variant with the
        // request admitted to it, in one step under the lock.
        let admission = {
            let mut st = self.lock();
            let variants = st.variants.entry(lane.name.clone()).or_default();
            if let Some(admission) = variants
                .contains_key(&name)
                .then(|| registry.try_admit(&key))
                .flatten()
            {
                // Another request made it meanwhile.
                return Ok(VariantLease {
                    name,
                    admission,
                    note,
                });
            }
            let mine: Vec<(String, i64)> = variants
                .values()
                .filter(|v| v.target == target.id && v.name != name)
                .map(|v| {
                    let key = format!("{}/{}", lane.name, v.name);
                    (
                        v.name.clone(),
                        registry.last_used_ms(&key).unwrap_or(v.created_ms),
                    )
                })
                .collect();
            if mine.len() >= policy.max_variants as usize {
                let idle = mine
                    .iter()
                    .filter(|(n, _)| registry.in_flight(&format!("{}/{n}", lane.name)) == 0)
                    .min_by_key(|(_, last)| *last)
                    .map(|(n, _)| n.clone());
                match idle {
                    Some(evict) => {
                        variants.remove(&evict);
                    }
                    None => {
                        return Err(ExecError::new(
                            409,
                            "too_many_variants",
                            format!(
                                "`{}` already has {} variant(s) with requests admitted \
                                 (max_variants {})",
                                target.id,
                                mine.len(),
                                policy.max_variants
                            ),
                            None,
                        ))
                    }
                }
            }
            let Some(admission) = registry.try_admit(&key) else {
                return Err(ExecError::new(
                    503,
                    "model_paused",
                    format!("variant `{name}` is being reloaded; retry"),
                    None,
                )
                .with_retry_after(Duration::from_secs(5)));
            };
            variants.insert(
                name.clone(),
                Variant {
                    name: name.clone(),
                    target: target.id.clone(),
                    base_model: target.model.clone(),
                    load: load.clone(),
                    created_ms: now_ms(),
                },
            );
            admission
        };
        let result = self.apply_lane(state, &lane).await;
        // Written only when the engine reads the variant's section now (or
        // when it starts); an apply held back whole leaves it out.
        let written = self
            .lock()
            .applied
            .get(&lane.name)
            .is_some_and(|models| models.contains_key(&name));
        if result["ok"] != json!(true) || !written {
            drop(admission);
            if let Some(variants) = self.lock().variants.get_mut(&lane.name) {
                if registry.in_flight(&key) == 0 {
                    variants.remove(&name);
                }
            }
            return Err(ExecError::new(
                503,
                "variant_apply_failed",
                format!(
                    "could not set up the variant of `{}`: {}",
                    target.id,
                    result["error"]
                        .as_str()
                        .or(result["note"].as_str())
                        .unwrap_or("unknown error")
                ),
                None,
            ));
        }
        Ok(VariantLease {
            name,
            admission,
            note,
        })
    }
}

/// The runtime's variant seam over the engine control. Holds the engine
/// weakly (the engine holds this).
pub(crate) struct EngineVariants {
    pub engine: std::sync::Weak<sb_runtime::Engine>,
    pub capacity: Arc<crate::activator::CapacityActivator>,
    pub engines: Arc<EngineControls>,
}

impl EngineVariants {
    fn ctx(&self) -> Option<EngineCtx> {
        Some(EngineCtx {
            engine: self.engine.upgrade()?,
            capacity: self.capacity.clone(),
            engines: self.engines.clone(),
        })
    }
}

impl sb_runtime::model_settings::ModelVariants for EngineVariants {
    fn ensure_variant<'a>(
        &'a self,
        target: &'a sb_core::ExecutionTarget,
        load: &'a Map<String, Value>,
        policy: &'a sb_core::RequestLoadPolicy,
        role: &'a str,
    ) -> sb_runtime::BoxFuture<
        'a,
        Result<sb_runtime::model_settings::VariantLease, sb_runtime::ExecError>,
    > {
        Box::pin(async move {
            let Some(ctx) = self.ctx() else {
                return Err(sb_runtime::ExecError::new(
                    503,
                    "shutting_down",
                    "the gateway is shutting down",
                    None,
                ));
            };
            ctx.engines
                .ensure_variant(&ctx, target, load, policy, role)
                .await
        })
    }

    fn before_dispatch<'a>(
        &'a self,
        provider_id: &'a str,
        model: &'a str,
    ) -> sb_runtime::BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(ctx) = self.ctx() {
                ctx.engines.before_dispatch(&ctx, provider_id, model).await;
            }
        })
    }
}

/// How often the listing poller asks an up engine for its loaded models.
pub(crate) const LISTING_POLL: Duration = Duration::from_secs(30);
/// How long one listing call may take (poller and refresh after an op).
const LISTING_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether the lifecycle says the lane's machine is up and quiet enough to
/// be asked for its listing: healthy or idle-counting, and its last probe
/// did not see it down. Off, waking, powering off, draining, degraded and
/// unmanaged lanes are never polled.
fn listing_pollable(state: &EngineCtx, lane: &str) -> bool {
    use crate::activator::LaneState;
    state.capacity.executor(lane).is_some_and(|executor| {
        let report = executor.report();
        matches!(report.state, LaneState::Healthy | LaneState::IdleCountdown)
            && report.observed_healthy != Some(false)
    })
}

/// Keep the loaded-model view fresh: every `every`, one
/// [`EngineControls::poll_listings`] pass. Stops when the engine is gone.
pub(crate) fn spawn_listing_poller(app: &AppState, every: Duration) -> bool {
    // No lane with an engine control: nothing to poll, so no task.
    if !app
        .snapshot()
        .config
        .local_executors
        .iter()
        .any(|lane| lane.engine.is_some())
    {
        return false;
    }
    let weak = Arc::downgrade(&app.engine);
    let (capacity, engines) = (app.capacity.clone(), app.engines.clone());
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let Some(engine) = weak.upgrade() else {
                return;
            };
            let ctx = EngineCtx {
                engine,
                capacity: capacity.clone(),
                engines: engines.clone(),
            };
            engines.poll_listings(&ctx).await;
        }
    });
    true
}

/// Wire the engine control's retries: when a lane comes up, and when the
/// last request dispatched on a held-back model ends.
pub(crate) fn install_hooks(
    engine: &Arc<sb_runtime::Engine>,
    capacity: &Arc<crate::activator::CapacityActivator>,
    engines: &Arc<EngineControls>,
) {
    let weak = Arc::downgrade(engine);
    let (cap, controls) = (capacity.clone(), engines.clone());
    capacity.attach_lane_up_hook(Arc::new(move |lane: &str| {
        if let Some(engine) = weak.upgrade() {
            let ctx = EngineCtx {
                engine,
                capacity: cap.clone(),
                engines: controls.clone(),
            };
            controls.retry_in_background(ctx, lane);
        }
    }));
    let weak = Arc::downgrade(engine);
    let (cap, controls) = (capacity.clone(), engines.clone());
    engine
        .model_settings()
        .set_idle_hook(Arc::new(move |key: &str| {
            if let Some(engine) = weak.upgrade() {
                let ctx = EngineCtx {
                    engine,
                    capacity: cap.clone(),
                    engines: controls.clone(),
                };
                controls.model_idle(ctx, key);
            }
        }));
}

/// The runtime's loaded-model view over the engine control's last listing
/// of each lane (`GET /models`, taken by `GET /v1/engines`, loads and
/// applies). Never calls a machine.
pub(crate) struct ListingView(pub Arc<EngineControls>);

impl sb_runtime::optimize::EngineView for ListingView {
    fn loaded_models(&self, provider_id: &str) -> Option<(i64, Vec<String>)> {
        let (at, models) = self.0.cached_models(provider_id)?;
        Some((
            at,
            models
                .iter()
                .filter(|row| matches!(row["status"]["value"].as_str(), Some("loaded" | "loading")))
                .filter_map(|row| row["id"].as_str().map(str::to_string))
                .collect(),
        ))
    }
}

// ---------------------------------------------------------------- settings API hooks

/// Engine checks of a settings document before it is written: load
/// settings need an engine control with a `settings_command`; raw
/// arguments must be the engine's and on its per-model allow list; paths
/// need `allow_model_path`; and the model must be one the engine lists
/// (when a listing is known: the last one seen, or a fresh one from an
/// awake engine; never a wake).
pub(crate) async fn validate_for_target(
    state: &EngineCtx,
    target: &str,
    doc: &ModelSettingsDoc,
) -> Result<(), String> {
    if doc.load.is_empty() && doc.engine_args.is_empty() && doc.request_load.is_none() {
        return Ok(());
    }
    let snap = state.snapshot();
    let Some((lane, model)) = split_target(target)
        .and_then(|(provider, model)| Some((engine_lane(&snap.config, provider)?, model)))
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
    kind_ops(lane.engine.kind).check_args(&doc.engine_args, lane.engine.allow_model_path)?;
    let controls = &state.engines;
    let listing = match controls.cached_models(&lane.name) {
        Some((_, models)) => Some(models),
        None => match controls.awake(state, &lane).await {
            Awake::Up => controls.models(state, &lane).await.ok(),
            Awake::Asleep(_) => None,
        },
    };
    if let Some(models) = listing {
        if !models.iter().any(|row| row["id"] == model) {
            return Err(format!(
                "the engine of lane `{}` does not list `{model}`; load settings need a model it \
                 has (its models: {})",
                lane.name,
                models
                    .iter()
                    .filter_map(|row| row["id"].as_str())
                    .filter(|id| !id.contains('~'))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    Ok(())
}

/// The load-time part of a target's view (operator keys only). Uses the
/// last engine listing only: a settings read never calls a machine.
pub(crate) fn add_load_view(
    state: &EngineCtx,
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
            // What an unload may take here (a client waits longer): the
            // wait for the model's requests, then for its process to exit.
            "apply_wait_secs": lane.engine.apply_wait_secs,
            "load_timeout_secs": lane.engine.load_timeout_secs,
        },
        "settings": settings,
        "engine_args": doc.map(|doc| json!(doc.engine_args)).unwrap_or_else(|| json!({})),
        "pending": pending,
        "memory": row.as_ref().map(|row| memory_summary(&row["memory"])).unwrap_or(Value::Null),
        "effective": row.as_ref().map(crate::engine_truth::effective).unwrap_or(Value::Null),
        "last_apply": controls.lock().last_apply.get(&lane.name).cloned(),
    });
}

/// After a settings write: apply changed load settings (`next_load`: the
/// engine re-reads a model's settings when the model is idle; `now`:
/// drain, apply, reload). `None` = the load part did not change. The
/// write itself always stands; the value says what the engine did.
pub(crate) async fn after_settings_write(
    state: &EngineCtx,
    target: &str,
    before: Option<&ModelSettingsDoc>,
    entry: &SettingsEntry,
    apply: ApplyMode,
    wake: bool,
) -> Option<Value> {
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
    let mut value = match apply {
        ApplyMode::NextLoad => controls.apply_lane(state, &lane).await,
        ApplyMode::Now => controls.apply_now(state, &lane, target, model, wake).await,
    };
    value["mode"] = json!(match apply {
        ApplyMode::NextLoad => "next_load",
        ApplyMode::Now => "now",
    });
    let held_back = value["deferred"]
        .as_array()
        .is_some_and(|models| models.iter().any(|m| m == model));
    let message = if value["reason"] == "old_settings" {
        // The apply reached the engine and the model loaded again, so
        // nothing is pending to retry: the engine's own presets or the
        // settings command kept the old values.
        format!(
            "revision {} written and `{target}` reloaded, but {}; check what the settings \
             command writes and the engine's base presets",
            entry.revision,
            value["error"].as_str().unwrap_or("its arguments differ")
        )
    } else if value["ok"] != json!(true) {
        format!(
            "revision {} written; applying it failed ({}); it is retried when the lane comes up, \
             before the next load and before a request reaches the engine",
            entry.revision,
            value["error"].as_str().unwrap_or("unknown error")
        )
    } else if value["reason"] == "model_busy" {
        format!(
            "revision {} written, but `{target}` still had {} request(s) after {} s; it was not \
             reloaded, and the engine re-reads its settings when it is idle (pending)",
            entry.revision, value["busy"], lane.engine.apply_wait_secs
        )
    } else if held_back {
        format!(
            "revision {} written; `{target}` serves requests, so the engine re-reads its settings \
             when it is idle (pending)",
            entry.revision
        )
    } else {
        match apply {
            ApplyMode::NextLoad => format!(
                "revision {} written; the engine re-reads a model's settings when the model is \
                 idle (a loaded idle model is unloaded now and loads with them on its next request)",
                entry.revision
            ),
            ApplyMode::Now => format!(
                "revision {} written; {}",
                entry.revision,
                value["note"].as_str().unwrap_or("applied")
            ),
        }
    };
    value["message"] = json!(message);
    Some(value)
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
    app: &AppState,
    principal: &Principal,
    name: &str,
    body: Value,
) -> Response {
    let state = &EngineCtx::from(app);
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
    let named = match resolve_one(app, principal, name) {
        Ok(named) => named,
        Err(response) => return response,
    };
    match load_target(state, &named.target, body.wake).await {
        Ok(value) => Json(value).into_response(),
        Err(response) => response,
    }
}

/// Whether `row` (the model's listing row now) shows a process that is
/// not the one `old` showed (its row from before the apply), started with
/// the load settings `wanted`: loaded, sleeping or loading, with listed
/// arguments that match `wanted` (the check `apply: now` makes, see
/// [`args_mismatch`]). The arguments must tell the two apart: the old ones
/// miss `wanted`, or the new ones differ from them. A change only to keys
/// the check does not compare (load mode, fit, rope, raw arguments) leaves
/// the old process looking like the new one, so it never counts; nor does
/// a row with no listed arguments, or no `old` row.
fn runs_new_args(wanted: &BTreeMap<String, String>, old: Option<&Value>, row: &Value) -> bool {
    let Some(old) = old else {
        return false;
    };
    let running = matches!(
        row["status"]["value"].as_str(),
        Some("loaded" | "sleeping" | "loading")
    );
    let args = listed_args(row);
    running
        && !args.is_empty()
        && args_mismatch(wanted, row).is_none()
        && (args_mismatch(wanted, old).is_some() || args != listed_args(old))
}

/// A listing row's arguments (`status.args`; empty when none are listed).
fn listed_args(row: &Value) -> Vec<&str> {
    row["status"]["args"]
        .as_array()
        .map(|args| args.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Load a target's model (waking only when `wake`). A pending apply of
/// the lane runs first, so the model loads with its current settings.
/// `Err` is the response.
///
/// It never closes a gate a request waits at, and never unloads: a request
/// for the model is never held or ended because of it.
pub(crate) async fn load_target(
    state: &EngineCtx,
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
    let registry = state.engine.model_settings();
    let key = format!("{}/{model}", lane.name);
    let deadline = tokio::time::Instant::now() + gate_bound(lane_gate_wait(state, &lane));
    if controls.pending_apply(&lane.name) {
        // The model's row before the apply (one listing, or the last one):
        // only arguments that differ from it tell a new process from the
        // old one.
        let old_row = match controls.models_within(state, &lane, LISTING_TIMEOUT).await {
            Ok(models) => Some(models),
            Err(_) => controls.cached_models(&lane.name).map(|(_, models)| models),
        }
        .map(|models| {
            models
                .into_iter()
                .find(|row| row["id"] == model)
                .unwrap_or(Value::Null)
        });
        let applied = controls.apply_lane(state, &lane).await;
        let rewrote = applied["reloaded"] == json!(true)
            && applied["changed"]
                .as_array()
                .is_some_and(|models| models.iter().any(|m| m == model));
        if rewrote {
            wait_old_process(state, &lane, target, model, old_row.as_ref()).await?;
        }
    }
    // An apply that ended just before (this one's wait for the lock, or one
    // that ran without this load) may leave a settle holding this model: its
    // first pass unloads the model it finds loaded, so load only after it.
    if !controls.wait_settled(&key, deadline).await {
        tracing::warn!(model = %key, "engine control: a settle still holds the model at the load's bound; loading it anyway");
    }
    let outcome = controls.load_model(state, &lane, model, true).await;
    match outcome {
        Ok(row) => {
            // Loaded with the current settings only when the engine has this
            // model's section: an apply that failed again, or held it back,
            // leaves it loaded with the old ones (still pending).
            if !controls.pending_models(state, &lane).contains(model) {
                controls.mark_loaded(target, registry.revision(target));
            }
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

/// After an apply rewrote `model`'s section: the re-read made the engine
/// end the model's old process, and llama.cpp ends it only after answering.
/// A load before then meets the old process (400 "model is already
/// running"), and the old settings would count as loaded. So wait, at most
/// the lane's `load_timeout_secs` from here (no gate is held), until the
/// listing shows the model not running (the old process ended), or running
/// a new process with the new arguments (a request's own load got there
/// first; see [`runs_new_args`], `old` is the model's row from before the
/// apply). A machine that is going away ends the wait at once (503) with no
/// listing after it. Never unloads: past the bound the load fails and the
/// old process is left alone.
async fn wait_old_process(
    state: &EngineCtx,
    lane: &EngineLane,
    target: &str,
    model: &str,
    old: Option<&Value>,
) -> Result<(), Response> {
    let controls = &state.engines;
    let wanted = state
        .engine
        .model_settings()
        .get(target)
        .map(|entry| model_preset(kind_ops(lane.engine.kind), &entry.doc))
        .unwrap_or_default();
    let started = tokio::time::Instant::now();
    let until = started + Duration::from_secs(lane.engine.load_timeout_secs);
    let was_offline = lane_offline(state, lane);
    let timed_out = || {
        error(
            StatusCode::GATEWAY_TIMEOUT,
            "old_process_running",
            format!(
                "`{model}` still runs its old settings {} s after the engine was told \
                 to end it; it was not loaded and nothing was stopped (requests are still \
                 served); load it again later",
                started.elapsed().as_secs()
            ),
        )
    };
    loop {
        // Checked before every listing: a machine that is going away is
        // never connected to.
        if let Some(why) = machine_going_off(state, lane, was_offline) {
            return Err(error(
                StatusCode::SERVICE_UNAVAILABLE,
                "machine_going_off",
                format!(
                    "the machine of lane `{}` is {why}: stopped waiting for the old process \
                     of `{model}`; it was not loaded and nothing was stopped",
                    lane.name
                ),
            ));
        }
        let models = match tokio::time::timeout_at(until, controls.models(state, lane)).await {
            Ok(listing) => {
                listing.map_err(|e| error(StatusCode::BAD_GATEWAY, "engine_error", e))?
            }
            Err(_) => return Err(timed_out()),
        };
        let row = models.iter().find(|row| row["id"] == model);
        let status = row.and_then(|row| row["status"]["value"].as_str());
        if !process_running(status) || row.is_some_and(|row| runs_new_args(&wanted, old, row)) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= until {
            return Err(timed_out());
        }
        tokio::time::sleep(
            Duration::from_millis(250)
                .min(until.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

/// `POST /v1/models/{model}/unload`: closes the model's admission gate
/// (new requests wait), waits for the requests already admitted (queued
/// and waking ones included; bounded by `apply_wait_secs`), and only then
/// unloads; 409 `model_busy` when they do not end in time. Never wakes or
/// powers anything off.
pub(crate) async fn unload_endpoint(app: &AppState, principal: &Principal, name: &str) -> Response {
    use sb_runtime::model_settings::GateKind;
    let state = &EngineCtx::from(app);
    let named = match resolve_one(app, principal, name) {
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
    let gate = state
        .engine
        .model_settings()
        .close(&format!("{}/{model}", lane.name), GateKind::Drain);
    if let Err(busy) = gate
        .wait_idle(Duration::from_secs(lane.engine.apply_wait_secs))
        .await
    {
        return error(
            StatusCode::CONFLICT,
            "model_busy",
            format!(
                "`{}` still has {busy} request(s) after {} s; unload refused",
                named.target, lane.engine.apply_wait_secs
            ),
        );
    }
    let outcome = controls.unload_model(state, &lane, model).await;
    drop(gate);
    match outcome {
        Ok(_) => {
            controls.refresh_listing(state, &lane).await;
            Json(json!({"target": named.target, "model": model, "status": "unloaded"}))
                .into_response()
        }
        Err(e) => error(StatusCode::BAD_GATEWAY, "engine_error", e),
    }
}

/// `GET /v1/models/{model}/engine-props` (operator/admin): the model's own
/// numbers from its process's `/props` (one slot's context, the slot count,
/// whether it reads pictures) beside the truth view of its arguments. Only
/// for a model the engine lists as loaded, only on an awake machine, and
/// never from the background: a call to the model's process counts as
/// activity. 409 when the machine sleeps or the model is not loaded.
pub(crate) async fn engine_props_endpoint(
    app: &AppState,
    principal: &Principal,
    name: &str,
) -> Response {
    let state = &EngineCtx::from(app);
    let named = match resolve_one(app, principal, name) {
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
        return error(
            StatusCode::CONFLICT,
            "lane_asleep",
            format!("lane `{}` is {why}; nothing was called", lane.name),
        );
    }
    let models = match controls.models_within(state, &lane, LISTING_TIMEOUT).await {
        Ok(models) => models,
        Err(e) => return error(StatusCode::BAD_GATEWAY, "engine_error", e),
    };
    let Some(row) = models.iter().find(|row| row["id"] == model) else {
        return error(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("the engine of lane `{}` does not list `{model}`", lane.name),
        );
    };
    if row["status"]["value"] != "loaded" {
        return error(
            StatusCode::CONFLICT,
            "model_not_loaded",
            format!(
                "`{}` is {}; its process has no props to read (load it first)",
                named.target,
                row["status"]["value"].as_str().unwrap_or("not loaded")
            ),
        );
    }
    let path = format!("/props?model={}", urlencoding_component(model));
    let props = match controls
        .call(
            state,
            &lane,
            reqwest::Method::GET,
            &path,
            None,
            Duration::from_secs(5),
        )
        .await
    {
        Ok(props) => props,
        Err(e) => return error(StatusCode::BAD_GATEWAY, "engine_error", e),
    };
    let mut out = crate::engine_truth::from_props(&props);
    out["target"] = json!(named.target);
    out["effective"] = crate::engine_truth::effective(row);
    Json(out).into_response()
}

/// A model id as one query value (ids are names: letters, digits and a
/// few marks; anything else is percent-encoded).
fn urlencoding_component(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// One lane's part of `GET /v1/engines`.
async fn engine_lane_view(state: &EngineCtx, lane: EngineLane) -> Value {
    let registry = state.engine.model_settings();
    let controls = &state.engines;
    let (status, models, error_text) = match controls.awake(state, &lane).await {
        Awake::Up => {
            // The engine's build, for the tuning ledger (best effort).
            controls.refresh_build(state, &lane).await;
            match controls
                .models_within(state, &lane, Duration::from_secs(3))
                .await
            {
                Ok(models) => ("up", Some((now_ms(), models)), None),
                Err(e) => ("up", controls.cached_models(&lane.name), Some(e)),
            }
        }
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
            // The model file's identity: a hash of its path, never the
            // path, so the tuning ledger can tell a new file apart.
            let file = row["path"]
                .as_str()
                .map(|path| sb_core::fnv_hex(&[path])[..12].to_string());
            let doc = registry.get(&target).map(|entry| entry.doc);
            json!({
                "id": id,
                "target": target,
                "file": file,
                "sidecars": sidecar_names(&row["sidecars"]),
                "speculative": speculative_view(doc.as_deref(), row),
                "status": status,
                "failed": row["status"]["failed"],
                "args": row["status"]["args"],
                "effective": crate::engine_truth::effective(row),
                "memory": memory_summary(&row["memory"]),
                "settings_revision": registry.revision(&target),
                "pending": controls.load_pending(&target, status),
                "in_flight": registry.in_flight(&target),
                "dispatched": registry.dispatched(&target),
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
    let (last_apply, pending_apply, deferred, overlay_known) = {
        let st = controls.lock();
        (
            st.last_apply.get(&lane.name).cloned(),
            st.failed_apply.get(&lane.name).copied().unwrap_or(false)
                || st
                    .deferred
                    .get(&lane.name)
                    .is_some_and(|models| !models.is_empty()),
            st.deferred
                .get(&lane.name)
                .map(|models| models.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
            st.applied.contains_key(&lane.name),
        )
    };
    let mut view = json!({
        "lane": lane.name,
        "kind": kind_name(lane.engine.kind),
        "state": status,
        "seen_at_ms": models.as_ref().map(|(at, _)| *at),
        "error": error_text,
        "models": rows,
        "variants": variants,
        "last_apply": last_apply,
        "pending_apply": pending_apply,
        "deferred": deferred,
        "settings_command": lane.engine.settings_command.is_some(),
        "build": controls.lock().builds.get(&lane.name).cloned(),
    });
    if !overlay_known {
        view["note"] = json!(
            "no apply since the gateway started: the first apply writes the whole overlay, and \
             variant sections written before the start leave it"
        );
    }
    view
}

/// `GET /v1/engines` (operator/admin): per engine lane, the engine's models
/// with status, arguments, memory, pending settings, variants and the last
/// apply. Never wakes: a lane the lifecycle knows to be down or going down
/// is never called and shows its last known listing; the others are asked
/// at once, each with a short timeout.
pub(crate) async fn engines_endpoint(
    axum::extract::State(app): axum::extract::State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Response {
    let state = EngineCtx::from(&app);
    if !principal.is_operator_or_admin() {
        return crate::tenancy::forbidden();
    }
    let snap = state.snapshot();
    let lanes: Vec<EngineLane> = snap
        .config
        .local_executors
        .iter()
        .filter_map(|lane| engine_lane(&snap.config, &lane.name))
        .collect();
    let views =
        futures::future::join_all(lanes.into_iter().map(|lane| engine_lane_view(&state, lane)))
            .await;
    Json(json!({"object": "list", "data": views})).into_response()
}

/// After a hold's wake: load the model the hold named (operator keys).
pub(crate) fn preload_after_hold(app: &AppState, target: String, wake: bool) {
    let state = EngineCtx::from(app);
    tokio::spawn(async move {
        match load_target(&state, &target, wake).await {
            Ok(_) => tracing::info!(target_model = %target, "hold preload: loaded"),
            Err(_) => tracing::warn!(target_model = %target, "hold preload: not loaded"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(context: u64, total: u64) -> Value {
        json!({"id": "m", "status": {"value": "unloaded", "args": ["llama-server", "--ctx-size", "4096"]},
            "memory": {"status": "measured", "min": {"n_ctx": 4096, "devices": [
            {"kind": "device", "name": "CUDA0", "total": total, "model": 10u64 << 30,
             "context": context, "compute": 1u64 << 30},
            {"kind": "host", "name": "Host", "model": 1u64 << 30, "context": 0, "compute": 0}]},
            "max": {"n_ctx": 262144}}})
    }

    fn policy(estimate: Option<sb_core::FitEstimate>) -> sb_core::RequestLoadPolicy {
        sb_core::RequestLoadPolicy {
            allow: vec![],
            bounds: Default::default(),
            max_variants: 2,
            variant_idle_ttl_secs: 3600,
            estimate,
            reserve_bytes: 0,
        }
    }

    fn checked(fit: FitCheck) -> Fit {
        match fit {
            FitCheck::Checked(fit) => fit,
            other => panic!("not checked: {other:?}"),
        }
    }

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn fit_scales_the_measured_kv_cache() {
        // 4096 tokens took 1 GiB of KV: 65536 take 16 GiB, plus 11 GiB.
        let rows = vec![measured(1u64 << 30, 24u64 << 30)];
        let load = map(json!({"ctx_size": 65536}));
        let fit = checked(variant_fit(
            Some(&rows),
            "m",
            &Map::new(),
            &load,
            &policy(None),
        ));
        assert_eq!(fit.needed_bytes, 27 << 30);
        assert_eq!(fit.budget_bytes, (24u64 << 30) / 100 * 95);
        assert!(fit.needed_bytes > fit.budget_bytes);
        // q8_0 K and V halve it (a bit more than half).
        let load = map(json!({"ctx_size": 65536, "cache_type_k": "q8_0", "cache_type_v": "q8_0"}));
        let fit = checked(variant_fit(
            Some(&rows),
            "m",
            &Map::new(),
            &load,
            &policy(None),
        ));
        assert_eq!(
            fit.needed_bytes,
            (11u64 << 30) + ((16u64 << 30) as f64 * 17.0 / 32.0) as u64
        );
        // No size change: nothing to check.
        let load = map(json!({"flash_attn": true}));
        assert_eq!(
            variant_fit(Some(&rows), "m", &Map::new(), &load, &policy(None)),
            FitCheck::NotNeeded
        );
    }

    #[test]
    fn fit_counts_the_other_loaded_models_and_the_reserve() {
        // The base itself is loaded (it stays beside the variant), and so is
        // another model on the same GPU; one on another GPU does not count.
        let mut base = measured(1u64 << 30, 24u64 << 30);
        base["status"]["value"] = json!("loaded");
        let other = json!({"id": "other", "status": {"value": "loaded"}, "memory": {"status": "measured",
            "min": {"n_ctx": 4096, "devices": [{"kind": "device", "name": "CUDA0", "model": 2u64 << 30, "context": 0, "compute": 0}]}}});
        let elsewhere = json!({"id": "far", "status": {"value": "loaded"}, "memory": {"status": "measured",
            "min": {"n_ctx": 4096, "devices": [{"kind": "device", "name": "CUDA1", "model": 9u64 << 30, "context": 0, "compute": 0}]}}});
        let rows = vec![base, other, elsewhere];
        let load = map(json!({"ctx_size": 8192}));
        let mut with_reserve = policy(None);
        with_reserve.reserve_bytes = 1 << 30;
        let fit = checked(variant_fit(
            Some(&rows),
            "m",
            &Map::new(),
            &load,
            &with_reserve,
        ));
        let base_bytes = (10u64 << 30) + (1u64 << 30) + (1u64 << 30);
        assert_eq!(
            fit.budget_bytes,
            (24u64 << 30) / 100 * 95 - base_bytes - (2u64 << 30) - (1u64 << 30)
        );
    }

    #[test]
    fn fit_takes_the_base_context_never_a_guess() {
        // The variant sets only cache types: the context is the running
        // engine's own (--ctx-size 4096 here), then the trained one.
        let rows = vec![measured(1u64 << 30, 24u64 << 30)];
        let load = map(json!({"cache_type_k": "q8_0"}));
        let fit = checked(variant_fit(
            Some(&rows),
            "m",
            &Map::new(),
            &load,
            &policy(None),
        ));
        assert_eq!(fit.ctx, 4096);
        let mut trained = rows.clone();
        trained[0]["status"]["args"] = json!(["llama-server", "--ctx-size", "0"]);
        let fit = checked(variant_fit(
            Some(&trained),
            "m",
            &Map::new(),
            &load,
            &policy(None),
        ));
        assert_eq!(fit.ctx, 262144);
        // The base's own load settings come first.
        let base = map(json!({"ctx_size": 32768}));
        let fit = checked(variant_fit(Some(&rows), "m", &base, &load, &policy(None)));
        assert_eq!(fit.ctx, 32768);
    }

    #[test]
    fn fit_uses_the_estimate_without_a_measurement() {
        let estimate = sb_core::FitEstimate {
            weights_bytes: 20 << 30,
            budget_bytes: 24 << 30,
            kv_layers: 48,
            kv_heads: 8,
            head_dim: 128,
            value_dim: None,
        };
        let load = map(json!({"ctx_size": 32768}));
        let fit = checked(variant_fit(
            None,
            "m",
            &Map::new(),
            &load,
            &policy(Some(estimate.clone())),
        ));
        assert_eq!(fit.needed_bytes, (20u64 << 30) + (6u64 << 30));
        assert_eq!(fit.source, "estimate");
        // Neither a measurement nor an estimate: unknown, saying how to fix.
        let FitCheck::Unknown(why) = variant_fit(None, "m", &Map::new(), &load, &policy(None))
        else {
            panic!("expected unknown");
        };
        assert!(why.contains("estimate"), "{why}");
        // An estimate but no known context (only cache types change).
        let load = map(json!({"cache_type_k": "q8_0"}));
        assert!(matches!(
            variant_fit(None, "m", &Map::new(), &load, &policy(Some(estimate))),
            FitCheck::Unknown(_)
        ));
    }

    #[test]
    fn unsafe_engine_arguments_are_refused_by_name() {
        for name in [
            "tools",
            "agent",
            "allow-unauthenticated-tools",
            "tools-runtime",
            "mcp-servers-json",
            "mcp-servers-config",
            "mmproj-url",
            "spec-draft-hf",
            "spec-default",
            "mmproj-auto",
            "media-allow-private-urls",
            "rpc",
            "video-ffmpeg-dir",
            "ui-mcp-proxy",
            "gpt-oss-20b-default",
            // writes a cache file at a path it is given
            "lookup-cache-dynamic",
        ] {
            let args = map(json!({name: true}));
            let e = LlamaCppRouter.check_args(&args, true).unwrap_err();
            assert!(e.contains(&format!("`{name}`")), "{name}: {e}");
        }
        for name in ["hf-repo-draft", "model-url", "hf-token"] {
            let e = LlamaCppRouter
                .check_args(&map(json!({name: "x"})), true)
                .unwrap_err();
            assert!(e.contains("router's own"), "{name}: {e}");
        }
        let ok = map(
            json!({"n-cpu-moe": 4, "temp": 0.6, "spec-draft-n-max": 8, "chat-template": "chatml"}),
        );
        LlamaCppRouter.check_args(&ok, false).unwrap();
        // A local draft model is a file: allowed only with allow_model_path.
        let draft = map(json!({"model-draft": "/m/d.gguf"}));
        assert!(LlamaCppRouter.check_args(&draft, false).is_err());
        LlamaCppRouter.check_args(&draft, true).unwrap();
    }

    #[test]
    fn a_refusal_reason_is_one_bounded_clean_line() {
        // stdout when stderr is empty, stderr when stdout is, both (stderr
        // first) when both have text.
        assert_eq!(
            refusal_reason(b"", b"refused: key X is pinned\n", &[]),
            "refused: key X is pinned"
        );
        assert_eq!(refusal_reason(b" \n", b"out", &[]), "out");
        assert_eq!(refusal_reason(b"err\n", b" \n", &[]), "err");
        assert_eq!(refusal_reason(b"err\n", b"out", &[]), "err | out");
        // One line, control characters removed.
        assert_eq!(
            refusal_reason(b"", b"line one\r\nline\ttwo\x1b[31m red\x07\n", &[]),
            "line one line two[31m red"
        );
        // The last 300 bytes, cut on a character boundary.
        let long = format!("{}{}", "a".repeat(400), "\u{e9}".repeat(200));
        let reason = refusal_reason(b"", long.as_bytes(), &[]);
        assert!(reason.len() <= 300, "{}", reason.len());
        assert!(reason.ends_with('\u{e9}'));
        assert!(!reason.contains('a'));
        assert_eq!(refusal_reason(b"", b"", &[]), "");
    }

    // A long ssh warning on stderr cannot hide the script's reason on
    // stdout, nor a long stdout the reason on stderr: each keeps its end
    // within the same 300 bytes.
    #[test]
    fn a_refusal_reason_shows_stderr_then_stdout_within_one_budget() {
        let warning = format!(
            "Warning: {} added to the list of known hosts.",
            "w".repeat(400)
        );
        let reason = refusal_reason(warning.as_bytes(), b"refused: key X is pinned\n", &[]);
        assert!(reason.len() <= 300, "{}", reason.len());
        assert!(reason.ends_with(" | refused: key X is pinned"), "{reason}");
        assert!(reason.contains("known hosts."), "{reason}");
        let noise = "n".repeat(500);
        let reason = refusal_reason(b"fatal: no such model\n", noise.as_bytes(), &[]);
        assert!(reason.len() <= 300, "{}", reason.len());
        assert!(reason.starts_with("fatal: no such model | "), "{reason}");
    }

    // A command that prints a key: the excerpt shows `<redacted>` in its
    // place, and the command still sees its environment (it printed it).
    #[tokio::test]
    async fn a_refusal_excerpt_never_shows_a_known_secret() {
        let named = "SB_TEST_SCRUB_NAMED_ENV";
        let named_value = "named-test-value-1";
        let long = "SB_TEST_SCRUB_LONG_TOKEN";
        let long_value = "long-test-token-abcdefghijklmnop";
        let unrelated = "SB_TEST_SCRUB_PLAIN";
        // Test-only variables with names no other test reads.
        std::env::set_var(named, named_value);
        std::env::set_var(long, long_value);
        std::env::set_var(unrelated, "plain-value-not-a-secret-at-all");
        let cfg = sb_core::Config::from_yaml(&format!(
            r#"
providers:
  - id: text
    type: openai_compatible
    base_url: "http://127.0.0.1:1/v1"
    api_key_env: {named}
"#
        ))
        .unwrap();
        let secrets = secret_values(Some(&cfg));
        assert!(secrets.iter().any(|value| value == named_value));
        assert!(secrets.iter().any(|value| value == long_value));
        assert!(!secrets.iter().any(|value| value.starts_with("plain-value")));

        let command = format!(
            "cat >/dev/null; echo \"warning: key ${named}\" >&2; \
             echo \"refused: token ${long} and ${unrelated}\"; exit 2"
        );
        let error = ShellSettingsRunner
            .run_scrubbed(&command, b"{}", Duration::from_secs(10), &secrets)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "exit 2: warning: key <redacted> | refused: token <redacted> and plain-value-not-a-secret-at-all"
        );
        // Without the config, a long value of a KEY/TOKEN variable is still
        // scrubbed.
        let error = ShellSettingsRunner
            .run(&command, b"{}", Duration::from_secs(10))
            .await
            .unwrap_err();
        assert!(!error.contains(long_value), "{error}");
        // Any runner's failure is scrubbed too.
        struct Echo;
        #[async_trait]
        impl SettingsCommandRunner for Echo {
            async fn run(&self, _: &str, _: &[u8], _: Duration) -> Result<String, String> {
                Err("exit 2: named-test-value-1".to_string())
            }
        }
        let error = Echo
            .run_scrubbed("x", b"{}", Duration::from_secs(1), &secrets)
            .await
            .unwrap_err();
        assert_eq!(error, "exit 2: <redacted>");
    }

    // The redaction is the alerts' own: a key written in the config file
    // (no env var holds it) never shows, not inside a `curl` command line
    // nor on its own; a value read with its trailing newline still matches
    // the key printed on one line; a URL loses its `user:pass@`.
    #[tokio::test]
    async fn a_refusal_excerpt_is_redacted_as_an_alert_is() {
        let file_key = "fake-config-file-key-abcdef";
        let newline_env = "SB_TEST_SCRUB_NEWLINE_ENV";
        let newline_value = "fake-newline-value-qrstuv";
        // A test-only variable no other test reads.
        std::env::set_var(newline_env, format!("{newline_value}\n"));
        let cfg = sb_core::Config::from_yaml(&format!(
            r#"
providers:
  - id: up
    type: openai_compatible
    base_url: "http://127.0.0.1:1/v1"
    api_key: "{file_key}"
  - id: other
    type: openai_compatible
    base_url: "http://127.0.0.1:2/v1"
    api_key_env: {newline_env}
"#
        ))
        .unwrap();
        let secrets = secret_values(Some(&cfg));
        assert!(secrets.iter().any(|value| value == file_key));
        assert!(
            secrets.iter().any(|value| value == newline_value),
            "trimmed"
        );
        let userinfo = ["user", "pw4u"].join(":");
        let command = format!(
            "cat >/dev/null; \
             echo 'curl -s http://engine.example -H \"Authorization: Bearer {file_key}\"' >&2; \
             echo \"config key={file_key} env ${newline_env}\"; \
             echo 'engine at http://{userinfo}@engine.example/health refused'; exit 2"
        );
        let error = ShellSettingsRunner
            .run_scrubbed(&command, b"{}", Duration::from_secs(10), &secrets)
            .await
            .unwrap_err();
        assert!(!error.contains(file_key), "{error}");
        assert!(!error.contains(newline_value), "{error}");
        assert!(!error.contains(&userinfo), "{error}");
        assert!(!error.contains("pw4u"), "{error}");
        assert!(error.contains("config key=<redacted>"), "{error}");
        assert!(
            error.contains("http://engine.example/health refused"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_refused_change_shows_the_reason_the_command_printed_on_stdout() {
        let runner = ShellSettingsRunner;
        let error = runner
            .run(
                "cat >/dev/null; echo 'refused: key X is pinned'; exit 2",
                b"{}",
                Duration::from_secs(10),
            )
            .await
            .unwrap_err();
        assert_eq!(error, "exit 2: refused: key X is pinned");
        // The success path still answers with the short stdout.
        let ok = runner
            .run("cat >/dev/null; echo done", b"{}", Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(ok, "done");
    }

    #[tokio::test]
    async fn a_timed_out_settings_command_is_killed_with_what_it_started() {
        let dir = std::env::temp_dir().join(format!(
            "sb-settings-runner-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("child.pid");
        // The command starts a child of its own, then waits on it.
        let command = format!(
            "sh -c 'echo $$ > {}; exec sleep 30' & wait",
            pid_file.display()
        );
        let e = ShellSettingsRunner
            .run(&command, b"{}", Duration::from_millis(500))
            .await
            .unwrap_err();
        assert!(e.contains("timed out"), "{e}");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 only checks that the process exists.
            alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive, "the command's child {pid} survived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct OkRunner;

    #[async_trait]
    impl SettingsCommandRunner for OkRunner {
        async fn run(&self, _: &str, _: &[u8], _: Duration) -> Result<String, String> {
            Ok(String::new())
        }
    }

    /// A machine that is down (nothing answers on port 1), so the control
    /// never calls an engine; the settings command always succeeds.
    fn asleep_ctx() -> EngineCtx {
        asleep_ctx_with(Arc::new(OkRunner))
    }

    fn asleep_ctx_with(runner: Arc<dyn SettingsCommandRunner>) -> EngineCtx {
        let cfg = sb_core::Config::from_yaml(
            r#"
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "http://127.0.0.1:1/v1"
    accounts:
      - id: a
        auth: { kind: api_key, inline: "k" }
local_executors:
  - name: text
    base_url: "http://127.0.0.1:1"
    health_endpoint: "http://127.0.0.1:1/health"
    engine:
      kind: llama_cpp_router
      settings_command: "true"
      apply_wait_secs: 1
"#,
        )
        .unwrap();
        let registry = sb_adapters::AdapterRegistry::from_config(&cfg).unwrap();
        let resolver = sb_credentials::CredentialResolver::from_config(&cfg).unwrap();
        let app = AppState::new(
            Arc::new(cfg),
            Arc::new(registry),
            Arc::new(resolver),
            Arc::new(sb_ledger::UsageLedger::in_memory()),
        );
        EngineCtx {
            engine: app.engine.clone(),
            capacity: app.capacity.clone(),
            engines: Arc::new(EngineControls::with_runner(runner)),
        }
    }

    /// A settings command that succeeds; while `block` is set, its next run
    /// says it started and waits for `release`.
    #[derive(Default)]
    struct BlockingRunner {
        block: std::sync::atomic::AtomicBool,
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl SettingsCommandRunner for BlockingRunner {
        async fn run(&self, _: &str, _: &[u8], _: Duration) -> Result<String, String> {
            if self.block.swap(false, std::sync::atomic::Ordering::SeqCst) {
                self.started.notify_one();
                self.release.notified().await;
            }
            Ok(String::new())
        }
    }

    fn load_doc(ctx_size: u64) -> ModelSettingsDoc {
        serde_json::from_value(json!({"load": {"ctx_size": ctx_size}})).unwrap()
    }

    /// A model held back while busy whose last request ends while the apply
    /// that held it back still runs (its idle hook finds nothing held back
    /// yet) is applied once that apply ends, not left pending.
    #[tokio::test]
    async fn a_model_that_goes_idle_during_the_apply_that_held_it_back_is_applied() {
        let runner = Arc::new(BlockingRunner::default());
        let ctx = asleep_ctx_with(runner.clone());
        install_hooks(&ctx.engine, &ctx.capacity, &ctx.engines);
        let registry = ctx.engine.model_settings();
        let actor = Default::default();
        registry
            .write("text/m", load_doc(8192), 0, &actor, "put")
            .unwrap();
        registry
            .write("text/other", load_doc(4096), 0, &actor, "put")
            .unwrap();
        let lane = engine_lane(&ctx.snapshot().config, "text").unwrap();
        let first = ctx.engines.apply_lane(&ctx, &lane).await;
        assert_eq!(first["pending"], json!(false), "{first}");
        // A request is dispatched on `m`.
        let mut request = registry.try_admit("text/m").unwrap();
        request.dispatch(Duration::from_secs(1)).await.unwrap();
        // Both models change; the apply holds `m` back and blocks in its command.
        registry
            .write("text/m", load_doc(16384), 1, &actor, "put")
            .unwrap();
        registry
            .write("text/other", load_doc(2048), 1, &actor, "put")
            .unwrap();
        runner
            .block
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let apply = tokio::spawn({
            let (ctx, lane) = (ctx.clone(), lane.clone());
            async move { ctx.engines.apply_lane(&ctx, &lane).await }
        });
        runner.started.notified().await;
        // The request on `m` ends while that apply still runs.
        drop(request);
        runner.release.notify_one();
        let held_back = apply.await.unwrap();
        assert_eq!(held_back["deferred"], json!(["m"]), "{held_back}");
        let mut pending = true;
        for _ in 0..200 {
            pending = ctx.engines.pending_apply("text");
            if !pending {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!pending, "`m` stayed held back after it went idle");
        let applied = ctx.engines.lock().applied["text"].clone();
        assert_eq!(applied["m"]["settings"]["ctx-size"], json!("16384"));
    }

    /// Failed applies in a row back off: 15 s, doubling, at most 10 min.
    #[test]
    fn a_failing_apply_backs_off() {
        assert_eq!(apply_backoff_ms(0), 15_000);
        assert_eq!(apply_backoff_ms(1), 15_000);
        assert_eq!(apply_backoff_ms(2), 30_000);
        assert_eq!(apply_backoff_ms(4), 120_000);
        assert_eq!(apply_backoff_ms(6), 480_000);
        assert_eq!(apply_backoff_ms(7), 600_000);
        assert_eq!(apply_backoff_ms(u32::MAX), 600_000);
    }

    #[tokio::test]
    async fn a_variant_a_request_waits_for_is_never_evicted() {
        let ctx = asleep_ctx();
        let policy: sb_core::RequestLoadPolicy = serde_json::from_value(json!({
            "allow": ["ctx_size"], "max_variants": 1, "variant_idle_ttl_secs": 3600,
            "estimate": {"weights_bytes": 1, "budget_bytes": 1u64 << 40,
                         "kv_layers": 1, "kv_heads": 1, "head_dim": 1}}))
        .unwrap();
        ctx.engine
            .model_settings()
            .write(
                "text/m",
                serde_json::from_value(json!({"request_load": policy.clone()})).unwrap(),
                0,
                &Default::default(),
                "put",
            )
            .unwrap();
        let target =
            sb_core::ExecutionTarget::new("text", "m", sb_core::ExecutionTargetKind::ModelApi);
        // Request A has its variant but has not reached the engine yet.
        let a = ctx
            .engines
            .ensure_variant(
                &ctx,
                &target,
                &map(json!({"ctx_size": 8192})),
                &policy,
                "client",
            )
            .await
            .unwrap();
        // Request B needs another variant; max_variants 1 and A's is in use.
        let e = ctx
            .engines
            .ensure_variant(
                &ctx,
                &target,
                &map(json!({"ctx_size": 16384})),
                &policy,
                "client",
            )
            .await
            .unwrap_err();
        assert_eq!(e.status, 409);
        assert_eq!(e.error_type, "too_many_variants");
        // Nor does the sweep, which drops the variants of a target that no
        // longer allows them, take it while A waits.
        let registry = ctx.engine.model_settings();
        registry
            .write("text/m", Default::default(), 1, &Default::default(), "put")
            .unwrap();
        let lane = engine_lane(&ctx.snapshot().config, "text").unwrap();
        ctx.engines.gc_variants(&ctx, &lane);
        assert!(ctx.engines.lock().variants["text"].contains_key(&a.name));
        registry
            .write(
                "text/m",
                serde_json::from_value(json!({"request_load": policy.clone()})).unwrap(),
                2,
                &Default::default(),
                "put",
            )
            .unwrap();
        // A done: B's variant replaces it.
        let a_name = a.name.clone();
        drop(a);
        let b = ctx
            .engines
            .ensure_variant(
                &ctx,
                &target,
                &map(json!({"ctx_size": 16384})),
                &policy,
                "client",
            )
            .await
            .unwrap();
        let variants = &ctx.engines.lock().variants["text"];
        assert!(variants.contains_key(&b.name) && !variants.contains_key(&a_name));
    }

    #[test]
    fn variants_expire_after_their_idle_ttl() {
        let hour = 3_600_000;
        assert!(!variant_expired(10 * hour, None, 10 * hour - 1000, 3600));
        assert!(variant_expired(10 * hour, None, 9 * hour, 3600));
        // Used recently: kept even though it was made long ago.
        assert!(!variant_expired(
            10 * hour,
            Some(10 * hour - 5),
            2 * hour,
            3600
        ));
        assert!(variant_expired(10 * hour, Some(8 * hour), 2 * hour, 3600));
    }

    #[test]
    fn variant_names_are_stable() {
        let a = json!({"ctx_size": 65536, "cache_type_k": "q8_0"})
            .as_object()
            .unwrap()
            .clone();
        let b = json!({"cache_type_k": "q8_0", "ctx_size": 65536})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(load_hash(&a), load_hash(&b));
        assert_eq!(load_hash(&a).len(), 8);
        assert_ne!(
            load_hash(&a),
            load_hash(&json!({"ctx_size": 1}).as_object().unwrap().clone())
        );
    }

    #[test]
    fn sidecars_are_named_without_their_paths() {
        let names =
            sidecar_names(&json!(["/models/q/mtp-head.gguf", {"path": "C:\\m\\mmproj.gguf"}, 3]));
        assert_eq!(names, json!(["mtp-head.gguf", "mmproj.gguf"]));
        assert_eq!(sidecar_names(&Value::Null), json!([]));
    }

    #[test]
    fn a_settle_is_bounded_by_the_gate_wait_less_a_second_and_the_load_timeout() {
        let s = Duration::from_secs;
        // `load_timeout_secs` above the gate wait: the gate wait less a
        // second, so the gate opens before a waiting request gives up.
        assert_eq!(settle_bound(s(31), s(60)), s(30));
        assert_eq!(settle_bound(s(33), s(33)), s(32));
        // Below it: the load timeout.
        assert_eq!(settle_bound(s(90), s(5)), s(5));
        assert_eq!(settle_bound(s(1), s(5)), s(0));
        assert_eq!(gate_bound(s(31)), s(30));
    }

    fn args_row(args: &[&str]) -> Value {
        json!({"status": {"value": "loaded", "args": args}})
    }

    fn doc_of(value: Value) -> ModelSettingsDoc {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn the_args_check_expects_the_preset_the_overlay_writes() {
        // A raw engine argument wins over the load setting for the same flag.
        let doc = doc_of(json!({"load": {"ctx_size": 8192}, "engine_args": {"ctx-size": 16384}}));
        let wanted = model_preset(&LlamaCppRouter, &doc);
        assert_eq!(wanted["ctx-size"], "16384");
        let row = args_row(&["llama-server", "--ctx-size", "16384"]);
        assert_eq!(args_mismatch(&wanted, &row), None);
        let row = args_row(&["llama-server", "--ctx-size", "8192"]);
        assert_eq!(
            args_mismatch(&wanted, &row).as_deref(),
            Some("ctx-size 16384 (it runs 8192)")
        );
    }

    #[test]
    fn the_args_check_reads_short_flags_and_equals_forms() {
        let wanted = model_preset(
            &LlamaCppRouter,
            &doc_of(json!({"load": {"ctx_size": 16384, "n_gpu_layers": 99,
                                    "flash_attn": true, "cache_type_k": "q8_0"},
                           "engine_args": {"ubatch-size": 512}})),
        );
        for args in [
            vec![
                "llama-server",
                "-c",
                "16384",
                "-ngl",
                "99",
                "-fa",
                "on",
                "-ctk",
                "q8_0",
                "-ub",
                "512",
            ],
            vec![
                "llama-server",
                "--ctx-size=16384",
                "--n-gpu-layers=99",
                "--flash-attn=on",
                "--cache-type-k=q8_0",
                "--ubatch-size=512",
            ],
            vec![
                "llama-server",
                "-c",
                "4096",
                "--ctx-size=16384",
                "--gpu-layers",
                "99",
                "--flash-attn",
                "--cache-type-k",
                "q8_0",
                "-ub=512",
            ],
        ] {
            assert_eq!(args_mismatch(&wanted, &args_row(&args)), None, "{args:?}");
        }
        let row = args_row(&[
            "llama-server",
            "-c=4096",
            "-ngl",
            "99",
            "-fa",
            "on",
            "-ctk",
            "q8_0",
            "-ub",
            "256",
        ]);
        assert_eq!(
            args_mismatch(&wanted, &row).as_deref(),
            Some("ctx-size 16384 (it runs 4096), ubatch-size 512 (it runs 256)")
        );
    }

    #[test]
    fn the_args_check_knows_raw_arguments_by_their_other_long_names() {
        // `gpu-layers` is `n-gpu-layers`; `draft-max` is `spec-draft-n-max`.
        let doc = doc_of(json!({"engine_args": {"gpu-layers": 40, "draft-max": 8}}));
        let wanted = model_preset(&LlamaCppRouter, &doc);
        let row = args_row(&["llama-server", "-ngl", "40", "--spec-draft-n-max", "8"]);
        assert_eq!(args_mismatch(&wanted, &row), None);
        let row = args_row(&["llama-server", "-ngl", "20", "--draft-max", "8"]);
        assert_eq!(
            args_mismatch(&wanted, &row).as_deref(),
            Some("gpu-layers 40 (it runs 20)")
        );
        // Two spellings with different values: the engine picks, so neither
        // is checked.
        let doc = doc_of(json!({"load": {"n_gpu_layers": 99}, "engine_args": {"gpu-layers": 40}}));
        let wanted = model_preset(&LlamaCppRouter, &doc);
        let row = args_row(&["llama-server", "-ngl", "40"]);
        assert_eq!(args_mismatch(&wanted, &row), None);
    }

    #[test]
    fn llama_preset_pairs_map_load_keys() {
        let load = json!({"ctx_size": 8192, "flash_attn": false, "mmap": false, "mlock": true,
                          "mmproj": "off", "draft_max": 8, "n_gpu_layers": "all"});
        let pairs = LlamaCppRouter.preset_pairs(load.as_object().unwrap(), &Map::new());
        assert_eq!(pairs["ctx-size"], "8192");
        assert_eq!(pairs["flash-attn"], "off");
        assert_eq!(pairs["load-mode"], "mlock");
        assert_eq!(pairs["no-mmproj"], "true");
        assert_eq!(pairs["spec-draft-n-max"], "8");
        assert_eq!(pairs["n-gpu-layers"], "all");
    }
}
