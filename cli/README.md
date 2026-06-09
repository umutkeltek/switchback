# `sb` — the Switchback control CLI

One front door for running your AI coding tools **through Switchback**, so every
request is observed in real time (traces + optional full bodies) while the tools
behave natively. Interactive menu (arrow keys) or plain subcommands.

```
sb                 # interactive menu: Run · Accounts · Settings · Observe · Relay
sb codex           # run Codex through your default mode (observed)
sb status          # relay / taps / accounts / trace counts
sb modes           # what each command does
```

## Install

```sh
./cli/install.sh                     # symlinks sb + wrappers into ~/.local/bin
# ensure ~/.local/bin is on your PATH
```

Symlinks (not copies) — the repo stays the source of truth. It seeds
`~/.config/switchback/sb.env` and `~/.pi/agent/models.json` only if absent.

**Prerequisite:** a running Switchback relay on `127.0.0.1:18765` with the
transparent taps enabled (`:18770` claude, `:18771` codex). The taps come from
`server.taps` in your `switchback.yaml`:

```yaml
server:
  bind: "127.0.0.1:18765"
  taps:
    - { id: claude-tap, bind: "127.0.0.1:18770", upstream: "https://api.anthropic.com",            capture_bodies: false }
    - { id: codex-tap,  bind: "127.0.0.1:18771", upstream: "https://chatgpt.com/backend-api/codex", capture_bodies: false }
```

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
sb claude --mode free
sb opencode      # uses ~/.config/opencode/opencode.json (see examples/)
sb pi            # needs: npm i -g --ignore-scripts @earendil-works/pi-coding-agent
```

## Multi-account (Codex)

Each ChatGPT account is its own profile (`CODEX_HOME`) holding **its own login and
its own sessions** — no rotation juggling.

```sh
sb login codex --account work        # browser login into a separate profile
sb codex --account work              # run as that account
sb accounts                          # list profiles + session counts
```

Sessions are stored per account, so **resume is per account**:

```sh
sb codex --account work resume --last    # continue that account's most recent session
# or inside the Codex TUI: /resume
```

The tap never stores your credentials — your own client holds and refreshes them;
the tap only forwards and observes. (Gateway-side multi-account with automatic
failover, for the relay path, is a planned addition.)

## Observe

```sh
sb status            # relay/taps/accounts/defaults
sb usage             # request + cost totals from the gateway ledger
sb traces            # recent routed requests
sb watch             # live-tail tap traces + captured bodies
sb watch claude      # live-tail the newest Claude session transcript
```

## Settings (remembered in `sb.env`)

`sb settings` (or edit `~/.config/switchback/sb.env`, see `examples/sb.env.example`):
default mode per tool · default account · Codex model · reasoning effort · gateway
model · full-body capture on/off.

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

Requires `zsh` and (for the menu) [`fzf`](https://github.com/junegunn/fzf); without
fzf the menu falls back to a numbered prompt.
