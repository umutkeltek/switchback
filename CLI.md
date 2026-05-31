# Switchback CLI Guide

The `switchback` binary is both the gateway server and the local operator tool.
It is designed for humans and for coding agents that need to inspect, validate,
and modify a local gateway without opening a dashboard.

Provider-specific recipes live in [`PROVIDER_SETUP.md`](PROVIDER_SETUP.md).

## Machine Contract

Use `--json` when a command has a human text default:

```bash
switchback --json doctor --config switchback.yaml
switchback --json provider add openai --config switchback.yaml --model gpt-4.1-mini
switchback --json vault list --config switchback.yaml
```

Commands that are already machine-oriented always print JSON to stdout:

```bash
switchback route-preview --config switchback.yaml --model auto/cheap
switchback config show --config switchback.yaml
switchback config get server.bind --config switchback.yaml
switchback config validate --config switchback.yaml
switchback config providers --config switchback.yaml
switchback config routes --config switchback.yaml
switchback config set server.bind '"127.0.0.1:8765"' --config switchback.yaml
switchback config unset server.default_provider --config switchback.yaml
switchback config patch --from-file patch.yaml --config switchback.yaml
switchback config format --config switchback.yaml
switchback provider models openai --config switchback.yaml
switchback provider test openai --config switchback.yaml
switchback provider doctor openai --config switchback.yaml
switchback provider matrix --config switchback.yaml
switchback provider presets
switchback schema commands
switchback schema config
switchback schema mcp
```

CLI output rules:

- Machine data goes to stdout.
- Diagnostics, missing-path errors, and command parser errors go to stderr.
- A non-zero exit status means the command did not complete its requested action.
- Secrets are never printed by config inspection commands; config output is redacted.

## First Local Run

Create a starter config:

```bash
switchback init --config switchback.yaml
```

Start the gateway:

```bash
switchback serve --config switchback.yaml
```

Smoke test it:

```bash
curl -s localhost:8765/health
curl -s localhost:8765/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}'
```

## Provider Onboarding

Add a provider preset:

```bash
switchback provider add openai --config switchback.yaml --model gpt-4.1-mini
```

The provider command writes config only. It references secrets through env vars
by default, for example `OPENAI_API_KEY`; it does not write API keys into YAML.

Discover upstream models:

```bash
switchback provider models openai --config switchback.yaml
```

Import exact provider/model routes:

```bash
switchback provider sync-routes openai --config switchback.yaml
```

Run a tiny request through one provider:

```bash
switchback provider test openai --config switchback.yaml
switchback provider test openai --config switchback.yaml --stream
```

Run a fuller provider diagnostic:

```bash
switchback provider doctor openai --config switchback.yaml
```

Run diagnostics across every configured provider:

```bash
switchback provider matrix --config switchback.yaml
```

Providers without a reliable model-list endpoint should set `model_hint` in the
provider config. `provider test`, `provider doctor`, and `provider matrix` use
that model when discovery is unavailable.

Current presets:

```text
openai, openrouter, anthropic, gemini, deepseek, groq, mistral, together,
fireworks, cerebras, xai, nvidia, ollama, vllm
```

Inspect preset defaults and examples:

```bash
switchback provider presets
```

## Config Inspection

Show the effective redacted config:

```bash
switchback config show --config switchback.yaml
```

Read one dotted path:

```bash
switchback config get server.bind --config switchback.yaml
switchback config get providers.0.id --config switchback.yaml
```

Validate the config using the same compile checks as runtime publish:

```bash
switchback config validate --config switchback.yaml
```

List providers and routes:

```bash
switchback config providers --config switchback.yaml
switchback config routes --config switchback.yaml
```

Set one value by dotted path. The value must be valid JSON, so strings are
quoted:

```bash
switchback config set server.bind '"127.0.0.1:8765"' --config switchback.yaml
switchback config set server.cost_aware true --config switchback.yaml
switchback config set providers.0.model_hint '"gpt-4.1-mini"' --config switchback.yaml
```

Remove one value:

```bash
switchback config unset server.default_provider --config switchback.yaml
```

Deep-merge a YAML or JSON patch file:

```bash
cat > patch.yaml <<'YAML'
server:
  cost_aware: true
  latency_aware: true
YAML
switchback config patch --from-file patch.yaml --config switchback.yaml
```

Rewrite the file in Switchback's canonical YAML formatting:

```bash
switchback config format --config switchback.yaml
```

All config writer commands validate before saving and replace the file
atomically from the same directory. A failed write leaves the previous config in
place.

## Route Preview

Preview routing without starting the server or executing upstream calls:

```bash
switchback route-preview --config switchback.yaml --model auto/coding
switchback route-preview --config switchback.yaml --model auto/fast --stream
```

The output includes the selected target, fallbacks, rejections, scores, and the
candidate list. This is the fastest way for an agent to answer "why will this
model go there?"

## Vault

Initialize the encrypted vault:

```bash
switchback vault init --config switchback.yaml
```

Set a secret:

```bash
printf '%s' "$OPENAI_API_KEY" | switchback vault set openai-key --config switchback.yaml
```

List and remove secret names:

```bash
switchback vault list --config switchback.yaml
switchback vault rm openai-key --config switchback.yaml
```

The vault command never prints secret values.

## Agent Workflows

Discover the local command/config/MCP contract:

```bash
switchback schema commands
switchback schema config
switchback schema mcp
```

Bootstrap and inspect:

```bash
switchback --json init --config switchback.yaml
switchback config validate --config switchback.yaml
switchback --json doctor --config switchback.yaml
```

Add and test a provider:

```bash
switchback --json provider add openai --config switchback.yaml --model gpt-4.1-mini
switchback config set providers.1.model_hint '"gpt-4.1-mini"' --config switchback.yaml
switchback provider test openai --config switchback.yaml
switchback route-preview --config switchback.yaml --model openai/gpt-4.1-mini
```

Discover and import provider models:

```bash
switchback provider models openai --config switchback.yaml
switchback provider sync-routes openai --config switchback.yaml
switchback config routes --config switchback.yaml
```

Check the whole local installation:

```bash
switchback config validate --config switchback.yaml
switchback --json doctor --config switchback.yaml
switchback provider matrix --config switchback.yaml
```

Serve after validation:

```bash
switchback serve --config switchback.yaml
```

Run the MCP stdio bridge:

```bash
switchback mcp --config switchback.yaml
```
