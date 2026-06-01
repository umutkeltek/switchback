# Provider Setup Guide

Switchback provider setup is CLI-native: add the preset, set the env var or
vault reference, discover models when supported, import routes, then run a live
doctor check.

## Agent Acceptance Contract

For an agent, a provider is ready only after this ladder succeeds:

```bash
switchback schema commands
switchback provider presets
switchback --json provider add <preset> --config switchback.yaml --model <model>
switchback config validate --config switchback.yaml
switchback provider certify <provider-id> --config switchback.yaml --model <model>
switchback route-preview --config switchback.yaml --model <provider-id>/<model>
switchback provider certify-all --config switchback.yaml --skip-missing-env
```

`provider certify` is the per-provider readiness gate. It returns the stable
schema `switchback/provider-certification@1` with:

- `ok` and `status`: certification result.
- `summary`: required/optional pass/fail counts.
- `verified_capabilities`: capabilities proven by live checks, such as
  `model_discovery`, `route_preview`, `chat_non_stream`, `chat_stream`, and
  `embeddings`.
- `checks`: named check records with `required`, `status`, and `detail`.
- `missing_env`: credential env vars that must be set before retrying.
- `next_commands`: the next CLI commands an agent can run.

`provider matrix` is the fleet report. It returns
`switchback/provider-matrix@1`, includes `total`, `checked`, `skipped`, and
`failed`, skips providers with missing credential env vars, and embeds each
available provider's doctor report.

`provider certify-all` is the fleet readiness gate. By default it is strict:
missing credential env vars are reported as `blocked` and make `ok: false`. Use
`--skip-missing-env` for local or CI smoke runs where only some provider keys
are present; providers with keys are live-certified, and absent providers are
reported as `status: "skipped"`.

## Common Flow

```bash
switchback provider presets
switchback --json provider add openai --config switchback.yaml --model gpt-4.1-mini
export OPENAI_API_KEY=...
switchback provider certify openai --config switchback.yaml
switchback provider doctor openai --config switchback.yaml
switchback provider test openai --config switchback.yaml
switchback route-preview --config switchback.yaml --model openai/gpt-4.1-mini
switchback provider certify-all --config switchback.yaml --skip-missing-env
```

For providers with a model-list endpoint:

```bash
switchback provider models openai --config switchback.yaml
switchback provider sync-routes openai --config switchback.yaml
switchback config routes --config switchback.yaml
```

Patch examples live under `examples/provider-patches/` and can be applied with:

```bash
switchback config patch --from-file examples/provider-patches/agent-routing.yaml --config switchback.yaml
```

For providers without reliable model discovery, set `model_hint`:

```bash
switchback config set providers.0.model_hint '"gpt-4.1-mini"' --config switchback.yaml
```

## Official API Providers

### OpenAI

```bash
switchback --json provider add openai --config switchback.yaml --model gpt-4.1-mini
export OPENAI_API_KEY=...
switchback provider doctor openai --config switchback.yaml
switchback provider certify openai --config switchback.yaml
```

### Anthropic

```bash
switchback --json provider add anthropic --config switchback.yaml --model claude-3-5-sonnet-latest
export ANTHROPIC_API_KEY=...
switchback provider doctor anthropic --config switchback.yaml
switchback provider certify anthropic --config switchback.yaml
```

### Gemini

```bash
switchback --json provider add gemini --config switchback.yaml --model gemini-1.5-flash
export GEMINI_API_KEY=...
switchback provider doctor gemini --config switchback.yaml
switchback provider certify gemini --config switchback.yaml
```

### OpenRouter

```bash
switchback --json provider add openrouter --config switchback.yaml --model anthropic/claude-3.5-sonnet
export OPENROUTER_API_KEY=...
switchback provider doctor openrouter --config switchback.yaml
```

## OpenAI-Compatible Providers

These presets use the OpenAI-compatible adapter path:

```text
deepseek, groq, mistral, together, fireworks, cerebras, xai, nvidia
```

Example:

```bash
switchback --json provider add groq --config switchback.yaml --model llama-3.3-70b-versatile
export GROQ_API_KEY=...
switchback provider doctor groq --config switchback.yaml
```

## Local Providers

### Ollama

```bash
ollama serve
switchback --json provider add ollama --config switchback.yaml --model llama3.1
switchback provider test ollama --config switchback.yaml
```

### vLLM

```bash
python -m vllm.entrypoints.openai.api_server --model <model>
switchback --json provider add vllm --config switchback.yaml --model local-model
switchback provider test vllm --config switchback.yaml
```

Override the local endpoint when needed:

```bash
switchback --json provider add vllm --config switchback.yaml \
  --base-url "$VLLM_BASE_URL" \
  --model local-model \
  --force
```

## Agent-Operable Setup

An agent can discover the command and config contract directly:

```bash
switchback schema commands
switchback schema config
switchback schema mcp
```

An MCP-capable agent can use the local stdio bridge:

```bash
switchback mcp --config switchback.yaml
```

The first MCP tool set includes config validation/show/get, route preview,
provider presets, provider certification, and doctor output. Config and route
preview tools are dry-run; provider certification/doctor execute small upstream
calls by design.

Recommended agent loop:

```bash
switchback provider presets
switchback --json provider add <preset> --config switchback.yaml --model <model>
switchback config validate --config switchback.yaml
switchback provider certify <provider-id> --config switchback.yaml --model <model>
switchback provider certify-all --config switchback.yaml --skip-missing-env
```

Accept the provider only when `provider certify` returns `ok: true` and
`verified_capabilities` contains at least `route_preview`, `chat_non_stream`,
and `chat_stream`.

## Guardrails

- Provider setup uses official/provider-compatible APIs and user-owned
  credentials.
- Secrets should live in env vars or the encrypted vault, not inline YAML.
- `provider doctor` may execute small upstream chat/stream checks. Use
  `route-preview`, `schema`, and MCP config tools when you need dry-run-only
  inspection.
