use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ProviderPreset {
    Openai,
    Openrouter,
    Anthropic,
    Gemini,
    Ollama,
    Deepseek,
    Groq,
    Mistral,
    Together,
    Fireworks,
    Cerebras,
    Xai,
    Nvidia,
    Vllm,
    Comfyui,
}

pub(crate) const PROVIDER_PRESETS: [ProviderPreset; 15] = [
    ProviderPreset::Openai,
    ProviderPreset::Openrouter,
    ProviderPreset::Anthropic,
    ProviderPreset::Gemini,
    ProviderPreset::Ollama,
    ProviderPreset::Deepseek,
    ProviderPreset::Groq,
    ProviderPreset::Mistral,
    ProviderPreset::Together,
    ProviderPreset::Fireworks,
    ProviderPreset::Cerebras,
    ProviderPreset::Xai,
    ProviderPreset::Nvidia,
    ProviderPreset::Vllm,
    ProviderPreset::Comfyui,
];

pub(crate) fn preset_defaults(
    preset: ProviderPreset,
) -> (
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
) {
    match preset {
        ProviderPreset::Openai => (
            "openai",
            "openai_compatible",
            Some("https://api.openai.com/v1"),
            Some("OPENAI_API_KEY"),
        ),
        ProviderPreset::Openrouter => (
            "openrouter",
            "openai_compatible",
            Some("https://openrouter.ai/api/v1"),
            Some("OPENROUTER_API_KEY"),
        ),
        ProviderPreset::Anthropic => ("anthropic", "anthropic", None, Some("ANTHROPIC_API_KEY")),
        ProviderPreset::Gemini => ("gemini", "gemini", None, Some("GEMINI_API_KEY")),
        ProviderPreset::Ollama => ("ollama", "openai_compatible", None, None),
        ProviderPreset::Deepseek => (
            "deepseek",
            "openai_compatible",
            Some("https://api.deepseek.com"),
            Some("DEEPSEEK_API_KEY"),
        ),
        ProviderPreset::Groq => (
            "groq",
            "openai_compatible",
            Some("https://api.groq.com/openai/v1"),
            Some("GROQ_API_KEY"),
        ),
        ProviderPreset::Mistral => (
            "mistral",
            "openai_compatible",
            Some("https://api.mistral.ai/v1"),
            Some("MISTRAL_API_KEY"),
        ),
        ProviderPreset::Together => (
            "together",
            "openai_compatible",
            Some("https://api.together.ai/v1"),
            Some("TOGETHER_API_KEY"),
        ),
        ProviderPreset::Fireworks => (
            "fireworks",
            "openai_compatible",
            Some("https://api.fireworks.ai/inference/v1"),
            Some("FIREWORKS_API_KEY"),
        ),
        ProviderPreset::Cerebras => (
            "cerebras",
            "openai_compatible",
            Some("https://api.cerebras.ai/v1"),
            Some("CEREBRAS_API_KEY"),
        ),
        ProviderPreset::Xai => (
            "xai",
            "openai_compatible",
            Some("https://api.x.ai/v1"),
            Some("XAI_API_KEY"),
        ),
        ProviderPreset::Nvidia => (
            "nvidia",
            "openai_compatible",
            Some("https://integrate.api.nvidia.com/v1"),
            Some("NVIDIA_API_KEY"),
        ),
        ProviderPreset::Vllm => ("vllm", "openai_compatible", None, None),
        ProviderPreset::Comfyui => ("comfyui", "comfyui", Some("http://127.0.0.1:8188"), None),
    }
}

pub(crate) fn preset_name(preset: ProviderPreset) -> &'static str {
    match preset {
        ProviderPreset::Openai => "openai",
        ProviderPreset::Openrouter => "openrouter",
        ProviderPreset::Anthropic => "anthropic",
        ProviderPreset::Gemini => "gemini",
        ProviderPreset::Ollama => "ollama",
        ProviderPreset::Deepseek => "deepseek",
        ProviderPreset::Groq => "groq",
        ProviderPreset::Mistral => "mistral",
        ProviderPreset::Together => "together",
        ProviderPreset::Fireworks => "fireworks",
        ProviderPreset::Cerebras => "cerebras",
        ProviderPreset::Xai => "xai",
        ProviderPreset::Nvidia => "nvidia",
        ProviderPreset::Vllm => "vllm",
        ProviderPreset::Comfyui => "comfyui",
    }
}

pub(crate) fn preset_is_local(preset: ProviderPreset) -> bool {
    matches!(
        preset,
        ProviderPreset::Ollama | ProviderPreset::Vllm | ProviderPreset::Comfyui
    )
}

pub(crate) fn preset_is_workload_executor(preset: ProviderPreset) -> bool {
    matches!(preset, ProviderPreset::Comfyui)
}

pub(crate) fn preset_model_hint(preset: ProviderPreset) -> Option<&'static str> {
    match preset {
        ProviderPreset::Openai => Some("gpt-4.1-mini"),
        ProviderPreset::Openrouter => Some("anthropic/claude-3.5-sonnet"),
        ProviderPreset::Anthropic => Some("claude-3-5-sonnet-latest"),
        ProviderPreset::Gemini => Some("gemini-1.5-flash"),
        ProviderPreset::Ollama => Some("llama3.1"),
        ProviderPreset::Deepseek => Some("deepseek-chat"),
        ProviderPreset::Groq => Some("llama-3.3-70b-versatile"),
        ProviderPreset::Mistral => Some("mistral-large-latest"),
        ProviderPreset::Together => Some("meta-llama/Llama-3.3-70B-Instruct-Turbo"),
        ProviderPreset::Fireworks => Some("accounts/fireworks/models/llama-v3p1-70b-instruct"),
        ProviderPreset::Cerebras => Some("llama3.1-8b"),
        ProviderPreset::Xai => Some("grok-3-mini"),
        ProviderPreset::Nvidia => Some("meta/llama-3.1-8b-instruct"),
        ProviderPreset::Vllm => Some("local-model"),
        ProviderPreset::Comfyui => None,
    }
}

pub(crate) fn provider_readiness_manifest_json(preset: ProviderPreset) -> serde_json::Value {
    let (id, provider_type, base_url, api_key_env) = preset_defaults(preset);
    let model_hint = preset_model_hint(preset);
    if preset_is_workload_executor(preset) {
        return serde_json::json!({
            "schema": "switchback/provider-readiness@1",
            "preset": preset_name(preset),
            "default_provider_id": id,
            "provider_type": provider_type,
            "provider_role": "workload_executor",
            "workload_class": "image_video_workflow",
            "local": preset_is_local(preset),
            "default_base_url": base_url,
            "model_hint": model_hint,
            "credential_contract": {
                "required": api_key_env.is_some(),
                "api_key_env": api_key_env,
                "source": match api_key_env {
                    Some(_) => "env_or_account",
                    None => "none_or_local_runtime",
                },
            },
            "required_checks": [
                "config",
                "workflow_registry",
                "image_generation",
                "job_status",
                "artifact_fetch"
            ],
            "optional_checks": [
                "job_events",
                "comfyui_live_probe"
            ],
            "capability_contract": {
                "chat_non_stream": "unsupported",
                "chat_stream": "unsupported",
                "embeddings": "unsupported",
                "image_generation": "required",
                "video_generation": "future",
                "workflow_queue": "future"
            },
            "e2e_commands": [
                format!("switchback provider add {id} --config switchback.yaml"),
                "curl -s http://127.0.0.1:8765/v1/workflows".to_string(),
                "curl -s http://127.0.0.1:8765/v1/images/generations -H 'content-type: application/json' -d '{\"prompt\":\"smoke test\",\"model\":\"mock/image\"}'".to_string()
            ],
        });
    }
    serde_json::json!({
        "schema": "switchback/provider-readiness@1",
        "preset": preset_name(preset),
        "default_provider_id": id,
        "provider_type": provider_type,
        "provider_role": "model_api",
        "local": preset_is_local(preset),
        "default_base_url": base_url,
        "model_hint": model_hint,
        "credential_contract": {
            "required": api_key_env.is_some(),
            "api_key_env": api_key_env,
            "source": match api_key_env {
                Some(_) => "env_or_account",
                None => "none_or_local_runtime",
            },
        },
        "required_checks": [
            "credentials",
            "config",
            "model_resolution",
            "route_preview",
            "chat_non_stream",
            "chat_stream"
        ],
        "optional_checks": [
            "model_discovery",
            "embeddings"
        ],
        "capability_contract": {
            "chat_non_stream": "required",
            "chat_stream": "required",
            "embeddings": "optional"
        },
        "e2e_commands": [
            match model_hint {
                Some(model) => format!("switchback provider add {id} --config switchback.yaml --model {model}"),
                None => format!("switchback provider add {id} --config switchback.yaml"),
            },
            format!("switchback provider models {id} --config switchback.yaml"),
            format!("switchback provider doctor {id} --config switchback.yaml"),
            format!("switchback provider certify {id} --config switchback.yaml")
        ],
    })
}

pub(crate) fn provider_readiness_manifests_json(
    preset: Option<ProviderPreset>,
) -> serde_json::Value {
    match preset {
        Some(preset) => provider_readiness_manifest_json(preset),
        None => serde_json::json!({
            "schema": "switchback/provider-readiness-manifests@1",
            "manifests": PROVIDER_PRESETS
                .iter()
                .map(|preset| provider_readiness_manifest_json(*preset))
                .collect::<Vec<_>>(),
        }),
    }
}

pub(crate) fn provider_presets_json() -> serde_json::Value {
    let presets = PROVIDER_PRESETS
        .iter()
        .map(|preset| {
            let (id, provider_type, base_url, api_key_env) = preset_defaults(*preset);
            let model_hint = preset_model_hint(*preset);
            serde_json::json!({
                "id": id,
                "preset": preset_name(*preset),
                "type": provider_type,
                "provider_role": if preset_is_workload_executor(*preset) {
                    "workload_executor"
                } else {
                    "model_api"
                },
                "workload_class": if preset_is_workload_executor(*preset) {
                    Some("image_video_workflow")
                } else {
                    None
                },
                "base_url": base_url,
                "api_key_env": api_key_env,
                "local": preset_is_local(*preset),
                "model_hint": model_hint,
                "add_example": match model_hint {
                    Some(model) => format!("switchback provider add {id} --config switchback.yaml --model {model}"),
                    None => format!("switchback provider add {id} --config switchback.yaml"),
                },
                "test_example": format!("switchback provider test {id} --config switchback.yaml"),
                "sync_routes_example": format!("switchback provider sync-routes {id} --config switchback.yaml"),
                "readiness_manifest": provider_readiness_manifest_json(*preset),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "schema": "switchback/provider-presets@1",
        "presets": presets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_presets_include_local_comfyui_workflow_executor() {
        let presets = provider_presets_json();
        let comfy = presets["presets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|preset| preset["id"] == "comfyui")
            .expect("comfyui preset");

        assert_eq!(comfy["type"], "comfyui");
        assert_eq!(comfy["base_url"], "http://127.0.0.1:8188");
        assert_eq!(comfy["local"], true);
    }
}
