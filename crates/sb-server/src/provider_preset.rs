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
}

pub(crate) const PROVIDER_PRESETS: [ProviderPreset; 14] = [
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
    }
}

pub(crate) fn preset_is_local(preset: ProviderPreset) -> bool {
    matches!(preset, ProviderPreset::Ollama | ProviderPreset::Vllm)
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
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "schema": "switchback/provider-presets@1",
        "presets": presets,
    })
}
