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

## Quick start (clone → green)

```sh
./cli/install.sh                                  # symlinks sb + wrappers, seeds configs
export OPENROUTER_API_KEY=...                      # scout/opencode/pi lanes (taps need NO key)
switchback serve --config ~/.config/switchback/switchback.yaml &   # or: cargo run -p sb-server -- serve --config ~/.config/switchback/switchback.yaml
sb doctor                                          # ✓ relay · ✓ taps · ✓ tools · ✓ catalog
sb                                                 # interactive menu
```

`install.sh` symlinks `sb` + the wrappers into `~/.local/bin` (repo stays the source
of truth) and seeds — only if absent — `~/.config/switchback/sb.env`,
`~/.pi/agent/models.json`, and a ready-to-run **`~/.config/switchback/switchback.yaml`**
(from [`examples/relay.example.yaml`](examples/relay.example.yaml), with `__HOME__`
paths filled in). That config already includes the transparent taps (`:18770` claude /
`:18771` codex) and the scout pool, so `sb doctor` can go green in one step. Run
`sb doctor` any time to see what's missing.

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

Current defaults: `codex` and `claude` are observed tap modes through Headroom. z.ai Claude mode uses the Headroom Anthropic tap on `127.0.0.1:8787`; z.ai Codex mode uses the direct OpenAI-compatible Switchback route because Headroom is running as an Anthropic proxy. Codex provider modes inherit `SB_CODEX_EFFORT` (`xhigh` by default). Claude provider modes seed provider-specific Claude Code settings only when missing, so `/model` and `/effort` remain usable inside Claude Code.

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
claude-zai-full # wrapper for claude-zai --rich
claude-nvidia-build-full # wrapper for claude-nvidia-build --rich
claude-openrouter-free-full # wrapper for claude-openrouter-free --rich
```

`--mcp` does not add global always-on MCP blocks. It reads the same on-demand catalog served through mcporter/Claude (`~/.claude/mcp-on-demand.json`), writes an isolated provider config at `~/.config/switchback/claude/_providers/<provider>/switchback-mcp.generated.json`, and passes it with `--strict-mcp-config`. Bare `--mcp` loads only `gbrain`; use `--mcp=name1,name2` or `--mcp=all` deliberately.

`--skills`/`--rich` use the provider profile as Claude's `user` setting source (`CLAUDE_CONFIG_DIR=~/.config/switchback/claude/_providers/...`), not normal `~/.claude/settings.json`. This is intentionally heavier than default bare mode; local models may take longer because skill context is real prompt context.

Useful commands:

```sh
sb modes
sb lane list
sb codex --mode free
sb claude --mode free
sb codex --provider zai
sb claude --provider zai
sb codex-opencode-go --fast
sb codex-lmstudio --fast
sb run codex --with zai
sb modes generate --repo
```

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
ranking from cost, declared capabilities, probe receipts, benchmark hints, and
route policy. This is not router-core mutation; promote route changes
separately. The full intake SOP is `tools/README.md`.

Adaptive API callers can request:

```text
auto/extract        cheap/free extraction and classification
auto/large-context  long-context pool, Nemotron Ultra/Super first
auto/judge          DeepSeek V4 Pro first, free Nemotron/OpenRouter as tripwire
```

Free models can execute or raise objections, but they still do not certify.

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
  engine-side through env loaded from `~/.config/switchback/sb.env`; use the
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

Shared mode keeps a credential **registry** (`~/.config/switchback/codex-auth/`) with
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
> default account, or `~/.config/switchback/claude/NAME/projects/` for a named
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
| Config dir | `~/.claude` | `~/.config/switchback/claude/NAME` |
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

`sb settings` (or edit `~/.config/switchback/sb.env`, see `examples/sb.env.example`):
default mode per tool · default Codex account · default Claude account · Codex
model · reasoning effort · gateway model · full-body capture on/off.

`sb settings` is deliberately small: it is for personal defaults and the few toggles
you change while working. The complete engine config still belongs to Switchback's
typed config CLI; `sb config ...` is a shortcut that automatically targets the live
config at `~/.config/switchback/switchback.yaml`:

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

Claude named profiles live outside the repo at `~/.config/switchback/claude/NAME`.

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
