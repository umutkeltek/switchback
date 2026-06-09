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
(from [`examples/switchback.yaml`](examples/switchback.yaml), with `__HOME__` paths
filled in). That config already includes the transparent taps (`:18770` claude /
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
sb claude --mode free
sb opencode      # uses ~/.config/opencode/opencode.json (see examples/)
sb pi            # needs: npm i -g --ignore-scripts @earendil-works/pi-coding-agent
```

## Multi-account (Codex)

Each ChatGPT account has its own login profile, and you switch between them without
the codex-multi-auth juggling:

```sh
sb login codex --account work        # browser login into a separate profile
sb codex --account work              # run as that account
sb accounts                          # list profiles + session counts
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
> history in `~/.claude/projects/` — resume those with Claude (`claude --resume`),
> not Codex.

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
