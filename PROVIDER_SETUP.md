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

## Multi-Auth Account Pools

A provider can carry multiple accounts with different auth types. The router
still selects only the provider/model target; `sb-credentials` selects the
account and lease. This is the intended shape for running agent clients through
whichever account is available, for example a Codex-oriented OAuth account plus
a Claude Code-oriented API-key account:

```yaml
providers:
  - id: openrouter
    type: openai_compatible
    base_url: "https://openrouter.ai/api/v1"
    selection: fill_first
    accounts:
      - id: codex-oauth
        auth:
          kind: oauth
          refresh_env: CODEX_OAUTH_REFRESH
          token_url: "https://oauth.example.com/token"
        policy_tags: ["interactive"]
      - id: claude-code-key
        auth:
          kind: api_key
          env: CLAUDE_CODE_PROVIDER_KEY
        policy_tags: ["backup"]
```

For refresh-only OAuth, omit `token_env` unless you actually want to seed a
current access token. `provider doctor` and `provider matrix` report missing
OAuth env sources before live checks run.

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

### llama.cpp

`llama-server` speaks OpenAI chat, so it is an `openai_compatible` provider:

```yaml
providers:
  - id: llamacpp
    type: openai_compatible
    base_url: "http://executor-1.example:8080/v1"
    engine: llama_cpp
```

### One request shape for every local engine (`engine:`)

Set `engine:` on an `openai_compatible` provider to name the engine behind it:
`generic` (default, forwards the request as-is), `ollama`, `llama_cpp`, or
`vllm`. The adapter then translates request controls that the engines spell
differently, so a caller writes one request:

| Caller sends | `ollama` | `llama_cpp` / `vllm` |
|---|---|---|
| `reasoning_effort: "none"` | forwarded as-is (Ollama turns thinking off) | `chat_template_kwargs.enable_thinking: false`; the top-level field is consumed (older vLLM releases reject it; current ones honor the explicit template argument first) |
| `reasoning_effort: "low"` / `"medium"` / `"high"` / `"minimal"` | forwarded as-is | forwarded, plus `chat_template_kwargs.enable_thinking: true` and `chat_template_kwargs.reasoning_effort` |
| no `reasoning_effort` (or an unknown value) | nothing added | nothing added |

A `chat_template_kwargs` key the caller sends itself is never overwritten.
Without `engine:` a llama.cpp server ignores `reasoning_effort`, so a model
keeps thinking, and a JSON-mode request can come back with empty content.

Only the Chat Completions field `reasoning_effort` is translated, on
`/v1/chat/completions` (streaming and not). A `/v1/responses` request's
`reasoning` object and the `thinking` field of an Anthropic `/v1/messages`
request are not translated yet, so neither of those fields switches thinking
off on llama.cpp or vLLM today. A caller that needs it there sends
`chat_template_kwargs` itself (Chat Completions, or `/v1/responses`, whose
unmodeled fields are forwarded as sent). `engine:` has no effect on a provider
with `wire: responses`: nothing is translated on that wire.

When two engines serve the same model on one route, set `pin_order: true` on
the route so the declared order wins even after one engine was stopped for a
while (see the `local-text` example in `config/switchback.example.yaml`).

When the engine runs on a host that is normally powered off, declare it as a
managed lane (`local_executors` entry named after the provider) with
`route_health: true` and `wake_on_request: true`, plus its `wake_command` and
`poweroff_command`. A chat or embeddings request whose first choice is that
lane, while the lane is down, then starts the wake and waits up to
`wake_wait_secs` (default 90) before falling through to the route's next
target; the engine is kept awake while any request or stream is still open,
and powered off `idle_timeout_secs` after the last one ends. Size
`wake_wait_secs` from the host: a cold host typically answers about a minute
after the wake is sent, while an engine that is already up loads a sleeping
model in seconds. Give the route a fast remote target after the lane if
clients cannot wait that long; with `wake_wait_secs: 0` the request falls
through at once while the wake keeps going for the next one.

Every operator command of a lane (wake, power-off, restart) runs for at
most `command_timeout_secs` (default 60). A command still running then is
stopped and counts as failed, so a hung command never blocks the next wake.
A failed power-off is retried every `wake_retry_interval_secs`, up to
`retry_budget` times; after that the lane is `degraded` (level `fail`, with
`poweroff_failures` on `GET /v1/workloads/capacity` and an error in the log)
and the machine stays on until an operator acts. After its own power-off,
Switchback takes the host for up again only once a probe has failed or a
wake has run, because a host can still answer while it shuts down.

A machine someone started by hand is managed too. Once the routing probe has
seen it up and Switchback routes a request to a `wake_on_request` lane on it
(or runs a media job there), the idle timer runs after that work ends and
`poweroff_command` powers the machine off. Leave `poweroff_command` unset on
a lane whose machine must stay on.

A request that waits for a wake keeps the slots it took on the way in: the
global admission slot (`server.max_concurrency`), its tenant's
`max_concurrency` slot and its `Idempotency-Key` claim, for up to
`wake_wait_secs`. Size `server.max_concurrency` and the tenant limits for the
requests you expect while a cold host boots, or lower `wake_wait_secs`.

Several engines on one machine (a text engine and ComfyUI, say) are one
lane each, and every lane names the machine with the same `host:`. Lanes on
one host share its power: one wake however many lanes ask, the machine
counts as busy while any of its lanes has work in flight or queued, and it is
powered off once, after every lane has been idle past its own
`idle_timeout_secs`; all of its lanes then go offline together. Set
`wake_command` and `poweroff_command` on one lane of the host (if several
lanes set one, it must be identical; config validation names the lane to
fix). The capacity report shows each lane's `host`. A lane without `host`
has a machine of its own, as before.

When those engines share one GPU, put their lanes in one `exclusive_group`
and give each a `release_command`, the shell command that makes that engine
free the GPU (for example a call that unloads its model). At most one lane of
the group holds the GPU. Work for the holder runs at once. Work for another
lane waits, up to that lane's `group_wait_secs` (default 120), until the
holder has nothing in flight; then the holder's `release_command` runs once
and the waiting lane takes the GPU. A text request waits only when its lane
is the route's first choice, before the first byte; its `RouteDecision` says
`local lane `llamacpp` waited 12 s for gpu group `gpu0`, took it from lane
`comfy``. If the wait runs out it falls through to the next target, or gets a
retryable 503 when nothing else can serve. A group lane reached as a later
fallback is used only while it already holds the GPU. A media job waits after
its wake and fails if the wait runs out. Fairness: while a switch is pending,
new work for the holder waits behind it, and the lane that lost the GPU gets
the next turn. After start-up or a power-off nobody is known to hold the GPU,
so the first lane to take it runs every other group lane's `release_command`
once. A failed or timed-out release does not hand the GPU over and counts
against the holder's `retry_budget`; past it the holder is escalated and
keeps the GPU until an operator acts. The capacity report shows `gpu_group`
and `holds_gpu`. The group's lanes must share one `host`. A request that waits
for the GPU keeps its admission, tenant and idempotency slots, like a request
that waits for a wake: size `server.max_concurrency` for it. See the commented
`gpu-box` example in `config/switchback.example.yaml`.

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
