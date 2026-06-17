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

## The mode taxonomy

Every tool reaches the model one of four ways. `sb` lets you pick per run, or set a
default (`sb settings`) so plain `sb codex` just does the right thing.

| Mode | Path | Ban risk | Notes |
|---|---|---|---|
| **tap** | client → tap (verbatim + observe) → vendor | very low | native request, your own auth, nothing re-shaped. Default for Codex. |
| **relay** (Mode C) | client → canonical engine → vendor | higher | re-issues through Switchback's IR; gives RouteDecision/usage/failover. |
| **native** | client → vendor | none | untouched escape hatch. |
| **free** | client → scout pool | n/a | free/cheap models. |

| Tool | Default | Available modes |
|---|---|---|
| `codex` | tap | tap · relay · native · free |
| `claude` | native | native · free — *Anthropic forbids proxying a subscription, so observe native via hooks* |
| `opencode` | gateway | via `:18765` (OpenAI-compatible), observed |
| `pi` | gateway | via `:18765` (custom provider in `~/.pi/agent/models.json`), observed |

```sh
sb codex --mode relay --account work resume --last
sb claude --account personal --print "hi"   # native Claude with a named profile
sb claude --mode free
sb opencode      # uses ~/.config/opencode/opencode.json (see examples/)
sb pi            # needs: npm i -g --ignore-scripts @earendil-works/pi-coding-agent
```

## Provider lanes (third-party coding plans)

Run Codex / Claude Code on **any** provider's coding plan (z.ai GLM, etc.), observed,
without hand-editing config. `sb` surfaces the engine's own provider/vault/setup tools
and adds a thin lane layer on top:

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
  and Codex points at the engine, which translates Responses→Chat. Key lives engine-side
  (its env, or the encrypted vault: `sb vault set …` + an `auth.vault` provider).

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
| **shared** (default) | matches native Codex: all accounts use your `~/.codex` pool → resume any session from any account; `sb` swaps `~/.codex/auth.json` to the chosen account's credential per run |
| **separated** | opt-in isolation: each account = its own `CODEX_HOME` → isolated sessions; resume is per account |

Shared mode keeps a credential **registry** (`~/.config/switchback/codex-auth/`) with
timestamped backups, and saves refreshed tokens back per account so refresh keeps
working. In shared mode `~/.codex/auth.json` reflects the **last-used** account — see
it with `sb sessions status`, restore the default with `sb sessions reset`.

```sh
sb sessions status                     # mode + which credential is live + registry
sb codex --sessions shared --account work   # one run on the shared pool as 'work'
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
