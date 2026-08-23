# `sb` — the Switchback control CLI

One front door for running your AI coding tools **through Switchback**, so every
request is observed in real time (traces + optional full bodies) while the tools
behave natively. Interactive menu (arrow keys) or plain subcommands.

```
sb                 # interactive menu: Run · Accounts · Settings · Observe · Relay
sb codex           # run Codex through your default mode (observed)
sb status          # relay / taps / accounts / trace counts
sb modes           # what each command does
sb verify native   # strict Codex/Claude native fidelity preflight
sb verify native --exercise large-payload --exercise stream --exercise websocket
sb native status   # Codex/Claude native readiness + fidelity guarantees
sb config get server.bind   # full live Switchback config, no --config needed
```

## 5-minute quickstart

Install the CLI, initialize the Switchback-owned runtime root, connect a provider lane, then launch the generated alias:

```sh
./cli/install.sh
switchback paths --json
sb connect zai --alias claudex
claudex
```

For a custom OpenAI-compatible provider, spell out its endpoint, key variable,
model, and client:

```sh
sb connect acme --openai-url https://api.acme.example/v1 --key-env ACME_API_KEY --model acme-code --alias acmex --agent codex
acmex
```

`sb connect` composes `sb lane add` + `sb lane key` + `sb modes generate`;
those commands remain the underlying plumbing for manual setup and automation.

## Quick start (clone → green)

```sh
./cli/install.sh                                  # builds the binary, initializes .switchback, links commands
export OPENROUTER_API_KEY=...                      # scout/opencode/pi lanes (taps need NO key)
switchback serve &                                  # uses the owned runtime config by default
sb doctor                                          # ✓ relay · ✓ taps · ✓ tools · ✓ catalog
sb                                                 # interactive menu
```

`install.sh` installs the current binary into the runtime's `bin/` directory and
keeps the repository's `.switchback/` tree as the local data authority. It seeds
only missing files, creates a `runtime-manifest@1`, and leaves existing secrets,
profiles, configs, and capture evidence untouched. Inspect the layout with
`switchback paths --json`. `~/.config/switchback` is only a compatibility link;
set `SWITCHBACK_RUNTIME_ROOT` to place the owned root elsewhere.

Every install writes `bin/install-provenance.json` with the installed engine's
version, source commit/path, installed path, and SHA-256. Existing legacy data
can be reviewed with `switchback setup migrate --from-current --dry-run` and
copied with `--apply`; sources are preserved and conflicts are skipped. On
macOS, `switchback setup launch-agent --plan` previews the user service, while
`--apply` backs up and writes the plist without loading or restarting it.

## Mode Taxonomy

Use `sb modes` and `sb lane list` as the live explanation. There is one front door: `sb`. Thin shortcut wrappers call back into it.

| Mode | Path | Use |
|---|---|---|
| subscription/tap | native client -> Switchback tap -> Headroom `127.0.0.1:8787` -> vendor | Everyday `codex` / `claude` with observation and native behavior. |
| native | native client -> vendor | Escape hatch: `codex-native` / `claude-native`. |
| free/gateway | client -> Switchback `:18765` -> route `scout/code` or `scout/chat` | Spend-aware work: OpenRouter free first, then NVIDIA-hosted/build, then DeepSeek/z.ai fallbacks. |
| coding-plan lane | client -> Switchback lane for a named provider | z.ai and similar subscriptions. Codex uses engine Responses->Chat; Claude uses verbatim Anthropic tap when available. |
| generated provider mode | generated wrapper -> `sb run <client> --with <provider>` | Provider modes are derived from lane/provider facts instead of hand-maintained per-tool scripts. |
| local/LM Studio | client -> Switchback `:18765` -> `local/mac-code` / `local/mac-fast` -> `127.0.0.1:1234` | Local model experiments without another routing surface. |

LM Studio is live runtime state, not a static registry row. `sb local current --json` asks the configured `mac` provider's `/v1/models`, compares served model ids with `local/mac-code` and `local/mac-fast`, and returns `loaded`/`stale` route status plus a suggested `sb local use <slot> <served-id> --reload` fix. `sb local served --json` is the same agent-friendly status surface. `sb local use` first refuses identifiers LM Studio is not currently serving, then changes only the selected route's dotted `targets` value. The validated config write is atomic; without `--reload`, active sessions keep the previous snapshot. The command prints the previous target so rollback is explicit: set that exact dotted target back, then run `sb reload` when active sessions are safe.

Current defaults: `codex` and `claude` are observed tap modes through Headroom. z.ai Claude mode uses the Headroom Anthropic tap on `127.0.0.1:8787`; z.ai Codex mode uses the direct OpenAI-compatible Switchback route because Headroom is running as an Anthropic proxy. Codex provider modes inherit `SB_CODEX_EFFORT` (`xhigh` by default). Claude provider modes seed provider-specific Claude Code settings only when missing, so `/model` and `/effort` remain usable inside Claude Code.

### Source-defined launch-profile harnesses

`config/launch-profiles.example.json` is the tracked example for the private
`switchback/launch-profiles@1` authority; `config/launch-profiles.schema.json`
describes the accepted document shape. Harness kinds are distinct contracts:
`claude-code`, `codex`, `prime-agent`, `omp`, `qwen-code`, and
`deepseek-harness`. Unknown kinds fail closed.

The direct headless renderers are deliberately narrower than Claude Code and
Prime-Agent:

- **OMP** executes `omp` with an isolated `PI_CODING_AGENT_DIR`, generated
  `models.yml`, explicit cwd, provider/model, text output, and
  `always-ask` approval. It never receives Prime flags or invented MCP flags.
- **Qwen Code** executes `qwen` in the caller's process cwd with `--prompt`,
  `stream-json`, `--approval-mode=default`, and `--safe-mode`. Its isolated
  `QWEN_HOME/settings.json` owns provider and capture headers. Hooks, skills,
  and MCP stay disabled; MCP cannot be claimed until that profile artifact
  carries concrete server definitions.
- **DeepSeek Harness** executes `dsh --profile headless`, redirects only through
  `DEEPSEEK_BASE_URL`, and pins `DSH_PERMISSION_MODE=workspace-write`. DSH
  0.1.x has no model or structured-output flag, so the renderer accepts only
  the shipped `deepseek-v4-flash` headless model and requires the exact
  unprefixed route `deepseek-v4-flash`. It records the pre-1.0 adopt-later
  warning rather than inventing flags or capability claims.

All three require `segmented_full_wire`, a matched OpenAI-compatible Switchback
tap other than bare `:18765`, and `capture_bodies: true` on that tap. A direct
profile also declares `client_credential_ref`: the gateway client key its
wrapper presents locally. This must be distinct from the provider lane's
upstream `credential_ref`; provider secrets are never forwarded through the
tap. Wrappers stamp profile, provider, model, capture endpoint/policy,
permission, workspace, output, and expected executable/version facts.
List/plan/conformance publish the canonical Compound identity in `harness`
(`oh_my_pi`, `qwen_code`, or `deepseek_harness`) and retain the renderer enum in
`harness_kind`. `sb profile doctor` checks the materialized artifacts, capture
binding/body posture, and, for a live doctor, the pinned executable version.

Plan before changing live state, then apply each intended profile explicitly:

```sh
sb profile plan omp-qwen --json
sb profile plan qwen-code-qwen --json
sb profile plan dsh-deepseek --json
# After reviewing every artifact path and capture endpoint:
sb profile apply omp-qwen --json
sb profile apply qwen-code-qwen --json
sb profile apply dsh-deepseek --json
```

### Claude provider customizations

Claude provider modes (`claude-lmstudio`, `claude-zai`, `claude-neuralwatt`, etc.) default to isolated `--bare` mode. That keeps provider auth/base URL hermetic and avoids normal Claude user settings, OAuth/keychain preflight, hooks, plugins, skills, and MCP from hijacking local/provider routes.

Opt into customizations explicitly:

```sh
claude-lmstudio --mcp # stay bare, generate provider MCP config from ~/.claude/mcp-on-demand.json (gbrain only)
claude-lmstudio --mcp=context7,gbrain # explicit selected on-demand MCPs
claude-lmstudio --mcp=all # full Claude on-demand MCP catalog; heavier
claude-lmstudio --skills # load user skills from isolated provider profile
claude-lmstudio --mcp --skills # generated MCP config + user skills
claude-lmstudio --rich # generated MCP config + user skills/commands/agents links
sb profile apply claude-zai-full # materialize the authority-owned wrapper for claude-zai --rich
claude-zai-full
claude-nvidia-build-full # wrapper for claude-nvidia-build --rich
claude-openrouter-free-full # wrapper for claude-openrouter-free --rich
```

`--mcp` does not add global always-on MCP blocks. It reads the same on-demand catalog served through mcporter/Claude (`~/.claude/mcp-on-demand.json`), writes an isolated provider config at `${SWITCHBACK_RUNTIME_ROOT}/config/claude/_providers/<provider>/switchback-mcp.generated.json`, and passes it with `--strict-mcp-config`. Bare `--mcp` loads only `gbrain`; use `--mcp=name1,name2` or `--mcp=all` deliberately.

`--skills`/`--rich` use the provider profile as Claude's `user` setting source (`CLAUDE_CONFIG_DIR=${SWITCHBACK_RUNTIME_ROOT}/config/claude/_providers/...`), not normal `~/.claude/settings.json`. This is intentionally heavier than default bare mode; local models may take longer because skill context is real prompt context.

Useful commands:

```sh
sb modes
sb lane list
sb local current --json
sb codex --mode free
sb claude --mode free
sb codex --provider zai
sb claude --provider zai
sb codex-opencode-go --fast
sb codex-lmstudio --fast
sb run codex --with zai
sb modes generate --repo
```

## Body evidence hierarchy

Switchback keeps the evidence layers separate:

- Raw evidence: protected local body blobs/index under `.switchback/state/body` plus `tap-bodies.jsonl` pointer rows.
- Readable artifact: `sb body audit latest --client claude` or `sb body audit <request_id>` writes a per-request Markdown bundle.
- Metrics row: every audit appends `.switchback/state/metrics/requests.jsonl` and refreshes `metrics/daily/YYYY-MM-DD.json`.
- Decision surface: the audit Markdown lists the largest context eaters and suggested cuts.
- Outcome loop: `sb body brief daily` or `sb body brief weekly` summarizes whether cost/latency/failure patterns improved.

Useful commands:

```sh
sb capture doctor
sb body audit latest --client claude
sb body audit <request_id> --format json
sb body brief daily
# Set this to the Switchback scout listener origin, without a trailing `/v1`.
export SWITCHBACK_SCOUT_URL="<switchback-scout-url>"
open "${SWITCHBACK_SCOUT_URL%/}/requests/<request_id>"
```

## Capture backup, proof, and reclaim

Switchback owns capture policy, local capture state, backup plans and receipts,
healing, restore, and reclaim. Compound may consume Switchback's non-secret
profile-conformance projection and record governed execution receipts; it does
not decide capture policy or certify storage independently.

The transfer tool has no machine-specific destination defaults. Set both
destination variables explicitly:

```sh
export SWITCHBACK_BACKUP_REMOTE='<ssh-host>'
export SWITCHBACK_BACKUP_REMOTE_ROOT='/mnt/<pool>/<dataset>/switchback-capture'

bun cli/switchback-capture-backup.ts --plan
bun cli/switchback-capture-backup.ts --verify-only
bun cli/switchback-capture-backup.ts
```

`SWITCHBACK_BACKUP_REMOTE_ROOT` must be an absolute `/mnt/...` path. Transfer
stages each artifact, promotes it atomically, re-verifies its checksum on the
remote, and only then submits the receipt to Switchback.

During migration from the legacy SQLite body index, the plan reports
`v2_index_missing_or_legacy_index_active` for the active legacy index and blob
archive. This is a typed, non-fatal blocker: sealed v2 segments and a frozen
legacy JSONL can still be transferred, while `legacy_complete` remains false
and `legacy_blockers` stays visible. Restart capture writers onto the v2 index
before attempting to prove the legacy index/blob tree complete.

Local reclaim is off by default. It requires
`SWITCHBACK_BACKUP_RECLAIM=1` (and optionally
`SWITCHBACK_BACKUP_KEEP_DAYS`, default `14`) plus matching remote checksum
proof. Do not delete the legacy JSONL, legacy index, or blob archive until
Switchback accepts exact remote receipts for the frozen artifacts.

## Provider lanes (third-party coding plans)

Run Codex / Claude Code on **any** provider's coding plan (z.ai GLM, etc.), observed,
without hand-editing config. `sb` surfaces the engine's own provider/vault/setup tools
and adds a thin lane layer on top:

### NeuralWatt

NeuralWatt is OpenAI-compatible (`https://api.neuralwatt.com/v1`) rather than
Anthropic-wire. Use the lane preset instead of Claude Code Router:

```sh
sb lane add neuralwatt
sb lane key neuralwatt
codex-neuralwatt
codex-neuralwatt --fast
codex-neuralwatt --kimi-code
codex-neuralwatt --qwen-fast
claude-neuralwatt
sb run codex --with neuralwatt
sb run claude --with neuralwatt
```

`claude-neuralwatt` uses the Switchback gateway route, not a verbatim Anthropic
tap. Run `sb neuralwatt-models` for aliases. Current shortcuts include `--glm`,
`--fast`, `--short`, `--kimi`, `--kimi-2.6`, `--kimi-code`, `--qwen`,
`--qwen-fast`, `--qwen397`, and `--qwen397-fast`; each maps to a
`neuralwatt/<model-id>` route in Switchback.

### NVIDIA Build

NVIDIA Build is OpenAI-compatible at `https://integrate.api.nvidia.com/v1`.
Use named free route groups for daily execution instead of remembering raw model
IDs:

```sh
codex-nvidia-build --code
codex-nvidia-build --minimax-m3-direct
claude-nvidia-build --long --rich
sb nvidia-build-models
sb run codex --with nvidia-build --multimodal
```

Routes are named `nvidia/free-code`, `nvidia/free-chat`,
`nvidia/free-long-context`, and `nvidia/free-multimodal`. The curated set
includes MiniMax M3, DeepSeek V4 Flash/Pro, GLM 5.1, Step 3.7 Flash, GPT-OSS,
and Nemotron free endpoints. Treat account rate limits such as 40 RPM as
measured policy, not doctrine; route probes should confirm current behavior
before relying on unattended batches.

### OpenRouter Free

OpenRouter free mode is separate from NVIDIA Build even when OpenRouter hosts
NVIDIA models. Use it when the free OpenRouter pool is enough and you want
Switchback to stay observed:

```sh
codex-openrouter-free --code
codex-openrouter-free --router
claude-openrouter-free --multimodal --skills
sb openrouter-free-models
sb run claude --with openrouter-free --chat --mcp=gbrain
```

Routes are named `openrouter/free-code`, `openrouter/free-chat`,
`openrouter/free-long-context`, and `openrouter/free-multimodal`, with
`openrouter/openrouter/free` left as the broad fallback router.

### Adaptive registry

Switchback's provider registry lives at `config/provider-registry.json`.
The live config points `server.cost_map` at that file, so adaptive scoring can
use provider/model cost policy tags (`free`, `promo`, `aggregator`) instead
of raw fallback order alone. The same registry now also carries model
capability, architecture, limit, benchmark, determinism, provenance, and probe
receipt fields. Declared provider facts are useful for routing; they are not
certification until a Switchback probe writes a verification receipt.

```sh
sb registry
sb registry providers nvidia
sb registry costs openrouter
sb registry capabilities openrouter
sb registry benchmarks nemotron
sb registry model qwen/qwen3-coder:free
sb registry score long_context nvidia
sb registry score judge --limit 10
sb registry refresh --check-drift
sb registry refresh --source openrouter --source nvidia --apply
sb registry refresh --source cerebras --cerebras-json FILE --check-drift
sb registry refresh --source groq --groq-json FILE --check-drift
sb registry probe --model nvidia/minimaxai/minimax-m3 --all --apply
bun tools/enrich-provider-registry.ts --fetch --apply
bun tools/enrich-provider-registry.ts --check
```

Current registry v2 seed ingests all OpenRouter free models from the public
Models API and attaches public per-model benchmark objects when available.
NVIDIA Build membership comes from the public `/v1/models` list; selected
NVIDIA rows also carry official model-card/blog facts such as MiniMax M3
benchmarks and Nemotron Ultra MoE/1M-context facts. Keep route groups curated:
the registry may know a model exists without making it a default lane.
Use `sb registry probe` to turn declared facts into local Switchback receipts
under `verification.probes`; receipts store metadata only, not prompt/response
bodies. Use `sb registry score <job-class> [filter]` for read-only operator
ranking from cost, declared capabilities, probe/catalog/provenance freshness,
benchmark hints, and route policy. This is not router-core mutation; promote
separately. The full intake SOP is `tools/README.md`.

`sb registry refresh` is the provider-adapter refresh gate. It fetches or
reads OpenRouter/NVIDIA catalogs plus explicitly requested independent
catalogs such as Cerebras and Groq, delegates enrichment to
`tools/enrich-provider-registry.ts`, compares candidate registry against
current registry, and writes an enrichment-run receipt under
`${SWITCHBACK_RUNTIME_ROOT}/state/registry/enrichment-runs` unless `--no-receipt`
is set. Timestamp-only catalog refreshes are receipt metadata, not drift;
drift means membership, price, context, capability, architecture, benchmark,
or catalog-presence facts changed.

Adaptive API callers can request:

```text
auto/extract        cheap/free extraction and classification
auto/large-context  long-context pool, Nemotron Ultra/Super first
auto/judge          DeepSeek V4 Pro first, free Nemotron/OpenRouter as tripwire
```

Free models can execute or raise objections, but they still do not certify.

### Switchback as API provider

Switchback's scout gateway is OpenAI-compatible at
`${SWITCHBACK_SCOUT_URL%/}/v1`, where `SWITCHBACK_SCOUT_URL` is the configured
scout listener origin. Any worker that accepts OpenAI SDK settings can use
Switchback as its provider by setting `base_url` to that gateway, `api_key` to
the local Switchback scout key, and `model` to a Switchback route/model name
such as `auto/extract`, `auto/judge`, `nvidia/minimaxai/minimax-m3`, or
`neuralwatt/glm-5.2`.

```sh
export SWITCHBACK_SCOUT_URL="<switchback-scout-url>"
curl "${SWITCHBACK_SCOUT_URL%/}/v1/chat/completions" \
  -H "Authorization: Bearer ${SWITCHBACK_SCOUT_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"neuralwatt/glm-5.2","messages":[{"role":"user","content":"Reply OK"}]}'
```

NeuralWatt rows are generated into the same provider registry with token prices, cached-input prices, context windows, feature badges, and rolling energy metadata. Inspect them with `sb registry providers neuralwatt`, `sb registry capabilities neuralwatt`, `sb registry model neuralwatt/glm-5.2`, and `sb registry score code_patch neuralwatt`.

### OpenCode Go

OpenCode Go is an official API-backed subscription. The GLM/Kimi/DeepSeek/MiMo
family is OpenAI-compatible at `https://opencode.ai/zen/go/v1`, so Codex and
Claude use Switchback gateway routes such as `opencode-go/glm-5.2`.

```sh
sb lane add opencode-go
sb lane key opencode-go
sb modes generate --repo
codex-opencode-go
codex-opencode-go --fast
claude-opencode-go --kimi-code
opencode-go --fast
```

`opencode-go` uses OpenCode's native provider selector directly. MiniMax/Qwen
OpenCode Go models are Anthropic-message models; expose those only after adding
a per-model wire map.

```sh
sb lane add zai                    # preset: z.ai GLM Coding Plan (fills URLs + model)
sb lane add NAME \                 # any other provider, fully generic:
   --anthropic-url https://api.acme.ai/anthropic \
   --openai-url    https://api.acme.ai/v1 \
   --key-env ACME_API_KEY --model acme-coder [--fast-model acme-mini]
sb lane key zai                    # set the provider key (clipboard · --stdin · hidden prompt)
sb lane list                       # lanes + endpoints + key presence
sb lane doctor                     # engine lane contract (switchback lane doctor)
sb lane rm zai

sb claude --provider zai [args]    # Claude Code → verbatim Anthropic tap → provider
sb codex  --provider zai [args]    # Codex → engine (Responses→Chat) → provider
```

Two transports, picked automatically per agent:

- **Claude Code** speaks Anthropic Messages and most coding plans expose an Anthropic
  endpoint, so `sb lane` wires a **verbatim tap** (`server.taps`) — the request is
  forwarded unmodified (what subscription plans require). Key lives client-side (`sb.env`/env).
- **Codex** speaks only the Responses API now, while coding endpoints are OpenAI Chat
  Completions — so `sb lane` adds an **engine provider + route** (`switchback provider add`)
  and Codex points at the engine, which translates Responses→Chat. Key lives
  engine-side through env loaded from `${SWITCHBACK_RUNTIME_ROOT}/config/sb.env`; use the
  vault only for an explicitly approved Keychain-backed setup.

Everything is idempotent: re-running `sb lane add` reuses existing taps/providers and only
writes config when something actually changed. The underlying engine commands are also
available directly: `sb provider …`, `sb vault …`, `sb setup native …`.

## Multi-account (Codex)

Each ChatGPT account has its own login profile, and you switch between them without
the codex-multi-auth juggling:

```sh
sb login codex --account work        # browser login into a separate profile
sb codex --account work              # run as that account
sb accounts                          # list Codex accounts + Claude profiles
```

### Session mode — shared (default) vs separated

Codex binds sessions to `CODEX_HOME`, not to the account credential. **Native Codex
already works the "shared" way** — one `~/.codex`, one session pool, one active
`auth.json`; switching accounts means re-logging-in in place. `sb` mirrors that and
adds a credential registry so you don't have to re-login each time. `sb settings →
Session mode`, or `--sessions` per run:

| `SB_SESSION_MODE` | Behaviour |
|---|---|
| **shared** (default) | native-safe default: the `default` account uses your `~/.codex` pool; named accounts auto-use separated `CODEX_HOME` unless you explicitly pass `--sessions shared` |
| **separated** | strict isolation: each account = its own `CODEX_HOME` → isolated auth + sessions |

Shared mode keeps a credential **registry** (`${SWITCHBACK_RUNTIME_ROOT}/config/codex-auth/`) with
timestamped backups, and saves refreshed tokens back per account so refresh keeps
working. In shared mode `~/.codex/auth.json` reflects the **last-used** account — see
it with `sb sessions status`, restore the default with `sb sessions reset`.
Because the native `~/.codex` pool has only one live `auth.json`, explicit shared
mode is single-active-account for concurrent work: `sb` refuses to start a
different-account shared run while another shared Codex run is active. Named
accounts are auto-separated by default, so concurrent agents do not collide unless
you deliberately opt into the shared pool.

```sh
sb sessions status                     # mode + live credential + active shared runs
sb codex --account work                     # auto-separated named account
sb codex --sessions shared --account work   # deliberate shared-pool run as 'work'
sb sessions reset                      # put the default account back in ~/.codex
```

In separated mode, sessions are stored per account, so **resume is per account**:

```sh
sb codex --account work resume --last                  # most recent
sb codex resume --all --include-non-interactive        # absolutely everything
# or inside the Codex TUI: /resume
```

> **Codex hides resume sessions two ways:** by current folder (`--all` disables it)
> **and** by excluding non-interactive/`codex exec` sessions
> (`--include-non-interactive` re-includes them). If the picker looks empty, your
> sessions are under another folder or were non-interactive. The menu's resume step
> offers **EVERYTHING** (both flags), all-folders-interactive, and this-folder.
>
> Note: `codex resume` only sees **Codex** sessions. Claude Code keeps its own
> history under its active Claude config directory: `~/.claude/projects/` for the
> default account, or `${SWITCHBACK_RUNTIME_ROOT}/config/claude/NAME/projects/` for a named
> profile. Resume those with Claude (`sb claude --account NAME --resume`), not Codex.

The tap never stores your credentials — your own client holds and refreshes them;
the tap only forwards and observes. (Gateway-side multi-account with automatic
failover, for the relay path, is a planned addition.)

## Manual Claude profiles

Claude Code subscription auth stays native. `sb claude --account NAME` launches the
real `claude` binary with a profile-specific `CLAUDE_CONFIG_DIR`; it does not proxy
or reshape Claude subscription traffic.

```sh
sb claude init --account personal                 # isolated profile
sb claude init --account work --copy-user-memory  # copy ~/.claude/CLAUDE.md once
sb claude init --account lab --link-user-memory --link-agents
sb claude accounts
sb claude doctor --account personal
sb claude --account personal --print "status"
```

Profile behavior:

| Item | Default Claude account | Named Claude profile |
|---|---|---|
| Config dir | `~/.claude` | `${SWITCHBACK_RUNTIME_ROOT}/config/claude/NAME` |
| Local transcripts | `~/.claude/projects/` | profile `projects/` directory |
| User memory | `~/.claude/CLAUDE.md` | profile-local unless copied/linked |
| User agents | `~/.claude/agents` | profile-local unless linked |
| Project memory | repo `CLAUDE.md` | same native Claude discovery |

This is manual account selection, not automatic account rotation. If a named profile
does not exist, `sb claude --account NAME` refuses and tells you to initialize it.
Use `sb claude doctor --account NAME` to see exactly which config, memory, agents,
history directory, and native binary Claude will use.

For live transcript diagnostics, `sb watch claude` automatically follows the newest
Claude transcript across `~/.claude` and all named profiles. Add `--account NAME` to
scope it to one profile.

Remote mode accepts an optional `SB_CLAUDE_PERMISSION_MODE` launch input and forwards
it to Claude Code. An explicit `--permission-mode` argument takes precedence. This is
an opaque adapter input: Switchback does not read or evaluate harness policy.

## Heartbeat: sb pulse

`sb pulse` is the cron-safe Switchback heartbeat. Its default fast tier is read-only,
makes no external provider calls, and composes engine, tap, Mode D, Headroom,
LaunchAgent, body archive, lane, registry, usage-flow, and config-drift checks. It
replaces manually remembering to run the scattered doctor and stress checks.

Every run atomically writes `.switchback/state/pulse/last.json` and appends to the
1000-line-capped `.switchback/state/pulse/history.jsonl`. Use `--json` for receipt JSON
on stdout and `--strict` to exit non-zero on warnings. `--live` adds synthetic native
large-payload/stream exercises, one tiny free-lane `scout/code` completion, and the
config-only Codex stress check. Pulse never reloads or restarts services and never
prompts for input.

## Observe

```sh
sb status            # relay/taps/accounts/defaults + native fidelity
sb doctor            # readiness, including native tap/auth warnings
sb verify native     # strict relay/tap/fidelity preflight with exit code
sb verify native --exercise large-payload --exercise stream --exercise websocket
sb native status     # raw engine-native readiness report
sb profiles list     # native profile modes and guarantees
sb profiles env NAME # env/header hints for one profile
sb claude doctor --account personal  # exact native Claude profile paths
sb usage             # request + cost totals from the gateway ledger
sb traces            # recent routed requests
sb watch             # live-tail tap traces + captured bodies
sb watch claude      # live-tail newest Claude transcript across all profiles
sb watch claude --account personal  # live-tail newest transcript in one profile
```

## Settings (remembered in `sb.env`)

`sb settings` (or edit `${SWITCHBACK_RUNTIME_ROOT}/config/sb.env`, see `examples/sb.env.example`):
default mode per tool · default Codex account · default Claude account · Codex
model · reasoning effort · gateway model · full-body capture on/off.

`sb settings` is deliberately small: it is for personal defaults and the few toggles
you change while working. The complete engine config still belongs to Switchback's
typed config CLI; `sb config ...` is a shortcut that automatically targets the live
config at `${SWITCHBACK_RUNTIME_ROOT}/config/switchback.yaml`:

```sh
sb config show
sb config get server.taps
sb config set server.cost_aware true
sb config validate
```

Use the rule of thumb: `sb settings` for everyday defaults, `sb config` for all
gateway/server/provider/account settings.

## Files

```
cli/
  sb                       the CLI (zsh, fzf-driven menu + subcommands)
  wrappers/                the per-mode launchers sb execs
    codex-switchback-tap       Mode B tap     (requires_openai_auth so OAuth refreshes)
    codex-switchback-traced    Mode C relay
    codex-switchback-scout     free scout
    claude-switchback-scout    free scout
  examples/                opencode.json · pi-models.json · sb.env.example
  install.sh               symlink into ~/.local/bin
```

Claude named profiles live inside the owned runtime at `${SWITCHBACK_RUNTIME_ROOT}/config/claude/NAME`.

Requires `zsh` and (for the menu) [`fzf`](https://github.com/junegunn/fzf); without
fzf the menu falls back to a numbered prompt.

## Current shortcuts

The live default is observed tap for both subscription CLIs: `codex` and `claude` go through Switchback taps and Headroom. Only `codex-native` and `claude-native` bypass that path.

Provider shortcuts:

```sh
sb codex-zai [args]        # Codex -> Switchback engine relay -> z.ai OpenAI-compatible route
sb claude-zai [args]       # Claude Code -> verbatim Anthropic tap -> Headroom route-only -> z.ai
sb codex-zai-direct [args] # Codex -> Switchback engine relay -> z.ai, no Headroom
sb claude-zai-direct [args] # Claude Code -> z.ai direct API, no Headroom/Switchback capture
sb codex-lmstudio [args]   # Codex -> Switchback gateway -> local/mac-code
sb claude-lmstudio [args]  # Claude Code -> Switchback gateway -> local/mac-code
sb opencode-lmstudio [args] # OpenCode direct LM Studio provider
sb codex-opencode-go [args] # Codex -> Switchback gateway -> OpenCode Go
sb claude-opencode-go [args] # Claude Code -> Switchback gateway -> OpenCode Go
sb opencode-go [args]       # OpenCode direct OpenCode Go provider

sb run codex --with zai [args]
sb run codex --with zai-direct [args]
sb run codex --with opencode-go [args]
sb run claude --with zai-direct [args]
sb run claude --with opencode-go [args]
sb run claude --with lmstudio [args]
sb run codex --with LANE [args]
```

Tap means the native client wire request is forwarded unchanged while observed. Relay/gateway means Switchback receives the client request, normalizes or translates it, chooses the configured route, and then calls the provider.
