# Switchback CLI Guide

The `switchback` binary is both the gateway server and the local operator tool.
It is designed for humans and for coding agents that need to inspect, validate,
and modify a local gateway without opening a dashboard.

Provider-specific recipes live in [`PROVIDER_SETUP.md`](PROVIDER_SETUP.md).

## Machine Contract

Use `--json` when a command has a human text default:

```bash
switchback --json doctor --config switchback.yaml
switchback --json lane doctor --config switchback.yaml
switchback --json lane audit codex-scout --config switchback.yaml --codex-config ~/.codex/config.toml
switchback --json lane install codex-scout --config switchback.yaml --codex-config ~/.codex/config.toml
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
switchback provider certify openai --config switchback.yaml
switchback provider certify-all --config switchback.yaml --skip-missing-env
switchback provider matrix --config switchback.yaml
switchback provider presets
switchback schema commands
switchback schema config
switchback schema mcp
switchback schema docs > CLI.generated.md
```

`schema docs` renders a generated Markdown contract from the same command,
config, MCP, and provider-readiness schemas that agents consume as JSON.

## Eval Evidence

`switchback eval` is the local evidence ledger for comparing externally-run
harnesses. It validates sanitized case/run manifests, stores metadata in
SQLite, and reports grouped outcomes. It does not execute Codex, Claude Code,
Aider, OpenHands, or any other harness.

```bash
switchback --json eval --store .switchback/eval.sqlite case validate cases/react-bug-001.json
switchback --json eval --store .switchback/eval.sqlite case import cases/react-bug-001.json
switchback --json eval convert codex-cli --input runs/codex.json --case-id react-bug-001 --case-revision rev-1 --strategy-id default --verdict pass > runs/codex-react-bug-001.json
switchback --json eval --store .switchback/eval.sqlite ingest --case cases/react-bug-001.json --result runs/codex-react-bug-001.json
switchback --json eval --store .switchback/eval.sqlite ingest --dry-run --result runs/codex-react-bug-001.json
switchback --json eval --store .switchback/eval.sqlite report --by harness --task-type coding --tag react --min-runs 3
switchback --json eval --store .switchback/eval.sqlite report --by harness,strategy,harness_version --strategy-id default --harness-version 1.0.0 --exclude-cache-hits --since-ms 1
switchback --json eval --store .switchback/eval.sqlite snapshot build --by harness,harness_version --task-type coding --tag react --min-runs 3 --output .switchback/eval-snapshot.json
switchback --json eval --store .switchback/eval.sqlite snapshot publish --snapshot .switchback/eval-snapshot.json --name current
switchback --json eval --store .switchback/eval.sqlite snapshot current --name current
```

`eval convert` currently accepts `codex-cli`, `claude-code`, and `aider`
sanitized result summaries and emits `switchback.eval.run/v1` JSON. Converter
inputs may include a `mechanical_checks` array for test/build/lint/diff-scope
summaries; those become outcome checks without storing stdout, stderr, diffs,
or logs. Run manifests may
include bounded `human_outcomes` signals (`accepted`, `edited`, `retried`,
`abandoned`, `rolled_back`) with stable evidence refs, not review bodies.
Reports can filter by `--harness`, `--harness-version`, `--strategy-id`,
cache-hit exclusion, and epoch-ms windows. `--by` must include
`harness` and may add `strategy` and `harness_version`. `eval snapshot build`
emits `switchback.eval.evidence_snapshot/v1` JSON from the same report filters
and can write it to a file. `eval snapshot publish` pins a built snapshot under
a stable store-backed name such as `current`; `eval snapshot current` reads that
pin.

Eval manifests are metadata-first. Ingest rejects raw prompts, raw responses,
inline diffs/logs, common secret fields, and unredacted absolute artifact paths.
Artifacts should be stable references plus hashes, not content bodies.

When `server.state_store` is configured and a `current` eval evidence snapshot
has been published, startup/reload pins that snapshot.
`/cp/v1/eval/snapshots` lists published snapshot metadata and
`/cp/v1/eval/snapshots/current` returns the pinned aggregate snapshot.
`/cp/v1/route-preview` filters the pinned snapshot into preview-only
`eval_evidence` rows, `eval_evidence_reasons` strings, and
`eval_evidence_snapshot_id` for configured harness candidates. This does not
change route selection.

The CLI-only 30-run kill-test pack lives at
`examples/eval/kill-test/pack.json`; see
`examples/eval/kill-test/README.md`. Snapshot
rows carry `preview_eligible`, `routing_eligible`, and `ineligible_reasons` so
weak evidence stays visible without pretending it can drive routing.

## Lane Doctor

Local clients should resolve through named lanes, not remembered provider/model
strings. Human-facing lane names are owned by
`${HOME}/.codex/switchback-routing-contract.md`; this CLI document follows that
contract instead of inventing names locally. Use lane doctor to inspect the
product-facing lane contract without executing upstream calls:

```bash
switchback lane doctor --config switchback.yaml
switchback --json lane doctor --config switchback.yaml
switchback --json lane audit codex-scout --config switchback.yaml --codex-config ~/.codex/config.toml
switchback --json lane install codex-scout --config switchback.yaml --codex-config ~/.codex/config.toml
```

Human command boundaries from the routing contract:

- `codex`: everyday observed native Codex route. The shell function calls
  `codex-switchback-tap`, which points Codex at Switchback tap `:18771`; that
  tap forwards to shared Headroom `:8787`, then to the native Codex backend.
- `codex-tap`: explicit alias for the same route as `codex`.
- `codex-native`: direct heavyweight Codex escape hatch, using native config
  defaults.
- `codex-relay`: native relay development/testing route. It calls
  `codex-switchback-traced`, points Codex at Switchback `:18765`, and uses the
  `codex/native` relay path instead of the transparent tap.
- `codex-free`: explicit cheap/free scout route through Switchback
  `scout/code`.
- `codex-api` / `codex/api`: transitional Codex-compatible API lane name; not
  the interactive `codex` shell command and not native Codex.
- `oracle`: ChatGPT Pro / Oracle lane for creative product judgment and second
  opinions.
- `claude`: native Claude Code by default.
- `claude-switchback`: explicit observed, text-only Claude scout path.

The Switchback route/lane ids the report may cover:

- `scout/code`: cheap/free everyday coding through Switchback.
- `scout/chat`: cheap/free conversational work through Switchback.
- `codex/api`: transitional Codex-compatible API surface through Switchback. In
  the local scout config it is backed by the same cheap/free pool as
  `scout/code`; it is not the interactive `codex` shell command and not native
  Codex.
- `codex-native`: native relay route id, expected to fail closed until
  conformance is green; do not confuse this route id with the direct
  `codex-native` shell command.
- `pro/manual`: ChatGPT Pro / Oracle handoff lane, not an automatic router provider.

`yellow` means the lane is usable through a transition alias, such as a legacy
combo. `red` on `codex-native` is safe when native relay is not explicitly
configured; it prevents silent fallback into scout or API routing.

Use `lane audit codex-scout` to compare Codex's local `switchback-scout`
profile/provider tables to the config-derived lane contract. It checks the
expected profile, provider, model (`scout/code`), reasoning effort, base URL,
wire API, env-key name, and auth mode without printing secrets.

Use `lane install codex-scout` to write or repair those local Codex tables. The
installer creates a timestamped backup before replacing an existing config; pass
`--dry-run` to preview the repaired post-install audit without writing.

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

For a Codex + Claude Code starter with explicit native-client profiles:

```bash
switchback init --native-clients --config switchback.yaml
```

The generated comments include native token-source account shapes:
`auth: { kind: codex_oauth }` reads `CODEX_ACCESS_TOKEN` or
`${HOME}/.codex/auth.json`; `auth: { kind: claude_code_oauth }` reads
`CLAUDE_CODE_OAUTH_TOKEN` or `${HOME}/.claude/.credentials.json` at
`/claudeAiOauth/accessToken`. That adapter is not first-party subscription
relay. Use `switchback setup native-relay audit` to inspect relay readiness; the
`codex_native_relay` and `claude_code_native_relay` provider kinds intentionally
fail closed until audited native wire fixtures and adapters exist.

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

Produce a stable readiness report:

```bash
switchback provider certify openai --config switchback.yaml
```

The certification report uses schema `switchback/provider-certification@1`.
Agents should treat `ok: true` plus `verified_capabilities` containing
`route_preview`, `chat_non_stream`, and `chat_stream` as the minimum live-ready
bar for chat providers. Optional embeddings can be unsupported without failing
chat readiness.

Run diagnostics across every configured provider:

```bash
switchback provider matrix --config switchback.yaml
```

The matrix report uses schema `switchback/provider-matrix@1`, includes
`total`, `checked`, `skipped`, and `failed`, and skips providers whose required
credential environment variables are absent.

Run certification across every configured provider:

```bash
switchback provider certify-all --config switchback.yaml
```

By default, `certify-all` is a strict gate: missing credential env vars are
reported as `blocked` and make the fleet report `ok: false`. For local machines
or CI jobs that only have a subset of provider keys, use:

```bash
switchback provider certify-all --config switchback.yaml --skip-missing-env
```

That mode still live-certifies providers whose credentials are present, but
reports absent providers as `status: "skipped"` instead of failing the run.

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
switchback provider certify openai --config switchback.yaml
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
