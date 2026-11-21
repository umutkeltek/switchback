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

Thinking that an OpenAI-compatible engine sends (llama.cpp and vLLM stream
it as `reasoning_content`, some servers as `reasoning`) is kept, on every
`engine:` value. `/v1/chat/completions` renders it back as
`reasoning_content` on the stream deltas, ahead of the answer, and on the
collected message; a response without thinking is unchanged. A trace's
`ttft_ms` counts the first thinking token, so a model that thinks for a while
before it answers shows progress at once and gets a time to first token.
`/v1/messages` and `/v1/responses` drop this thinking by default, as before:
their clients send the last answer back on the next turn, and Switchback
cannot take an unsigned `thinking` block or a `reasoning` item as input yet,
so that turn would fail with 400. `server.openai_compatible_thinking_on_all_formats:
true` shows it there too (`thinking` blocks, a `reasoning` item), for
clients that do not send it back. Thinking from other upstreams (Anthropic,
Responses) is shown on those two formats as before. Thinking is never sent
back upstream.

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

When a route lists targets on a machine that sleeps (a GPU machine woken on
request) and on one that is always on, set `prefer_awake` on the route (or
`server.prefer_awake` for every route) so a small request does not wake the
sleeping machine:

```yaml
routes:
  - name: chat
    match: { model: "chat" }
    prefer_awake:
      enabled: true                 # default false
      wake_for_prompt_tokens: 6000  # a prompt this big may wake a machine
      wake_for_output_tokens: 2000  # max_tokens above this may wake one
    targets:
      - "gpu-box/big"      # wake_on_request lane
      - "small-host/small" # always on
```

A route's `prefer_awake` merges over `server.prefer_awake` field by field: a
field the route sets replaces the server's, a field it leaves out keeps the
server's. So with `server.prefer_awake: { enabled: true }` a route that sets
only `wake_for_prompt_tokens` stays on, and `prefer_awake: { enabled: false }`
turns one route off.

A target needs a wake while its lane (with `wake_on_request: true`) is asleep,
is waking, or is powering off. A small request (estimated prompt under
`wake_for_prompt_tokens`, `max_tokens` not over `wake_for_output_tokens`) is
served by the targets that are up first; targets that need a wake stay
fallbacks in their old order and are woken only when the request reaches one
(every target before it failed, was skipped, or its lane was closed). A bigger
request keeps the route's order, so the strong machine is woken.

The prompt estimate is rough: characters / 4 over the system prompt, message
text and tool schemas, plus 1600 tokens per image. So about four images cross
the default `wake_for_prompt_tokens: 6000` and may wake the machine. An
embeddings request is sized from its `input`: characters / 4 over every input
string, plus the length of every token array, so a large batch may wake the
machine up front.

When the awake first choice is full (`max_in_flight` reached), a small
request does not spill onto a target that needs a wake, even with the default
`on_full: spill`: it waits in the full lane's queue like `on_full: queue`, and
falls back to (and wakes) the sleeper only when `queue_timeout_secs` runs out
with the awake lane still busy. The decision then says so, for example
`prefer_awake=[small-host full, queued, waited 60 s in its queue and is still
full (1 of 1 in flight); wakes gpu-box asleep (wake ~65s) for gpu-box/big]`. A
spill to another target that is up works as before.

A client can steer one request with the header `x-switchback-wake`: `always`
keeps the route's order, `never` drops every target that needs a wake (a 503
`local_lane_asleep` when nothing else can serve), `auto` is the default. The
header is read on `/v1/chat/completions`, `/v1/responses`, `/v1/messages`,
`/v1/embeddings` and route preview (which never wakes anything). The decision
says what happened in one `prefer_awake` line, in `reason` and on the
`x-switchback-route` header, for example
`prefer_awake=[gpu-box asleep (wake ~65s), chose small-host/small]`. The wake
time is measured per machine (start of the wake to its first healthy probe)
and shown as `wake_duration_ms` / `wake_duration_ewma_ms` in
`GET /v1/workloads/capacity` and under `wake` in `GET /v1/targets/stats`. An
admin key sees every machine there; a tenant key sees the wake cost only of
machines behind lanes its routes can reach. On a `latency_aware` route, a
request with `max_tokens` over `wake_for_output_tokens` ranks targets that are
up on measured decode tokens per second instead of time to first token, when
each of them has a sample.


Every operator command of a lane (wake, power-off, restart) runs for at
most `command_timeout_secs` (default 60); the wake command, which may block
until the machine answers, runs for up to `boot_timeout_secs` when that is
longer. A command still running then is stopped and counts as failed, so a
hung command never blocks the next wake.
Commands run through `sh -c` in the gateway's own environment, not in your
login shell: use absolute paths and do not rely on `HOME` (a gateway started
by a service manager often has another one). Make the wake command fail
when it cannot send: one that sends nothing but exits 0 looks to Switchback
like a machine that never boots.
A failed power-off is retried every `wake_retry_interval_secs`, up to
`retry_budget` times; after that the lane is `degraded` (level `fail`, with
`poweroff_failures` on `GET /v1/workloads/capacity` and an error in the log)
and the machine stays on until an operator acts. After its own power-off,
Switchback takes the host for up again only once a probe has failed or a
wake has run, because a host can still answer while it shuts down. A host
that still answers `boot_timeout_secs` after the power-off command returned
did not power off: it is taken for up again, with a warning in the log and
the lane's `last_error`.

A machine someone started by hand is managed too. Once the routing probe has
seen it up and Switchback routes a request to a `wake_on_request` lane on it
or to any lane of a shared `host` (or runs a media job there), the idle timer
runs after that work ends and
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

A lane of a shared host that has to come up while its machine is already up
sends no wake: another lane of the host answers a fresh probe, or the
host's wake command succeeded in this power cycle less than
`boot_timeout_secs` ago (right after a wake the engines start one after the
other, and ComfyUI often answers several seconds after a text engine). The
lane then only waits for its own engine, up to `boot_timeout_secs`; if the
engine does not answer, its `restart_command` runs when one is set, and
otherwise the lane fails like a boot timeout, saying that the host was up.
Inside one waking cycle the wake is not re-sent while another lane answers.
This matters for a wake command that toggles power, which would switch a
running machine off. The power cycle ends with Switchback's power-off; the
next lane that needs the machine then wakes it.

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
against the holder's `retry_budget`; the next run waits
`wake_retry_interval_secs` after a failed one, so a burst of waiting work
cannot spend the budget within seconds. Past it the holder is escalated (again
at every refused switch) and keeps the GPU until an operator acts or the
machine's next power-off, which restarts the engines and resets the budget.

Write each `release_command` so that it:

- succeeds when its engine holds nothing: no model loaded, or the engine not
  listening yet. Right after a wake the engines start one after the other (a
  text engine can answer several seconds before ComfyUI), and the first lane
  to take the GPU releases the others, which may still be starting. A plain
  `curl -fsS` to an engine that is not listening fails, and so does unloading
  a model that is not loaded (the llama.cpp router answers 400): either one
  blocks the switch and counts against `retry_budget`.
- fails when the engine answers but keeps its GPU memory.
- returns only once the memory is free. The llama.cpp router unloads one
  model per call (`POST /models/unload` with `{"model": "<id>"}`; without a
  body it answers 400): list the loaded models (`GET /models`), unload each,
  then wait until none is loaded. ComfyUI's `POST /free` with
  `{"unload_models": true, "free_memory": true}` returns before the memory is
  given back: wait until its process holds little GPU memory.

The capacity report shows `gpu_group`
and `holds_gpu`. The group's lanes must share one `host`. A request that waits
for the GPU keeps its admission, tenant and idempotency slots, like a request
that waits for a wake: size `server.max_concurrency` for it. See the commented
`gpu-box` example in `config/switchback.example.yaml`.

**Direct clients.** Switchback sees only the work that goes through it. On a
real machine other clients often reach the engines directly: a script on the
LAN, a tool server that talks to ComfyUI, a person in an ssh session. The
idle timer cannot see their work, so Switchback would power the machine off
under them. Give the machine a `poweroff_guard_command`: a shell command that
Switchback runs right before each idle power-off, under the same locks and
re-checks as the power-off itself. Exit 0 means the machine is free and the
power-off runs. Any other exit means busy: there is no power-off, the machine
stays up, `GET /v1/workloads/capacity` says `power-off postponed: guard
reports the machine busy` with a count (`poweroff_guard_busy`) and since when
(`poweroff_guard_busy_since_ms`), and the guard is asked again after
`wake_retry_interval_secs`. Busy may last for hours; it is not an error and
the lane stays at level `ok`. A guard that times out (`command_timeout_secs`)
or cannot run (the shell's exit 126 or 127) never counts as free: there is no
power-off, and after `retry_budget` such runs in a row the lane reports level
`warn` and says the guard is broken (`poweroff_guard_failures`), until the
guard answers again; new work does not clear it, so a machine that stays on
for a month because of a broken guard stays visible. A reload that changes or
removes the guard drops its old answers, so a fixed guard shows `ok` at once
and is asked at the next idle tick. Answer busy with any exit code but 126
and 127: the shell uses those for a command it could not run, so they count
as no answer. The guard needs a
`poweroff_command` on its lane or on another lane of its host; set it once per
host, or identically, like the power commands. For example, with
`poweroff_guard_command: "ssh executor-1 /usr/local/bin/gpu-idle-check"`, a
script on the host that answers busy while someone is logged in or while the
GPU runs a compute process other than the managed engines:

```sh
#!/bin/sh
# Exit 1 (busy) while someone is logged in or while the GPU runs a process
# that is not one of the managed engines; exit 0 (free) otherwise. A check
# that cannot run answers busy: the machine stays on.
[ -n "$(who)" ] && exit 1
apps=$(nvidia-smi --query-compute-apps=process_name --format=csv,noheader) || exit 1
others=$(printf '%s\n' "$apps" | grep -v -e llama-server -e comfyui -e '^$')
[ -n "$others" ] && exit 1
exit 0
```

Adjust the process names to the engines on your machine. A client that sends
work to an engine Switchback manages (a script that queues ComfyUI jobs) does
not start a process of its own; check the engine's own queue for that (for
ComfyUI, `GET /queue` lists running and pending jobs).

**Machines woken outside Switchback.** A lane whose engine answers its health
probe reads `healthy` in `GET /v1/workloads/capacity` even when Switchback did
not wake its machine (another tool or a person did). On a shared machine where
one lane was just seen up, the supervisor also probes the machine's other
lanes, so each engine's state is honest. By default nothing else changes: no
idle countdown runs and the machine stays on, as before. Set
`adopt_external_wake: true` on a lane (with `host`, one lane makes the whole
machine adopt) to take such a machine as one Switchback woke: from the moment
the supervisor sees it up, its lanes count down to idle, and with no work and
no hold it powers off after `idle_timeout_secs`, after the
`poweroff_guard_command` reports it free, so a person or a direct
client the guard sees still keeps it on. The countdown starts at adoption, so
an adopted machine always runs at least `idle_timeout_secs`; a machine
Switchback just powered off is not adopted while it still answers during its
shutdown. `adopt_external_wake` needs a `poweroff_command` on the lane or on
another lane of its host, and a
`poweroff_guard_command` on the lane or on its host: the config refuses
adoption without one, since a machine woken outside Switchback may be in use
and only the guard can tell.

**Holds: keep a machine on.** An agent that works on the machine directly (a
shell, a benchmark, maintenance) takes a hold, so the idle power-off does not
run under it:

```sh
curl -s "$SB/v1/hosts/gpu-box/holds" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' \
  -d '{"ttl_secs": 7200, "reason": "benchmark", "wake": true}'
# 201 {"id":"hold_...","host":"gpu-box","reason":"benchmark",
#      "expires_at":"2026-...Z","expires_at_ms":...,"state":"waking","owner":"agents"}
curl -s "$SB/v1/hosts/gpu-box/holds" -H "authorization: Bearer $KEY"          # list
curl -s -X PATCH "$SB/v1/hosts/gpu-box/holds/hold_..." -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"ttl_secs": 3600}'                  # extend
curl -s -X DELETE "$SB/v1/hosts/gpu-box/holds/hold_..." -H "authorization: Bearer $KEY"
```

`{host}` is a lane's `host`, or the lane's name when it has a machine of its
own (an unknown one answers 404). While any hold on a machine is active its
idle power-off does not run. A hold keeps the machine on only: it takes no GPU
group, so text and media lanes keep taking turns. A hold ends by itself at
`expires_at`, or when deleted; the machine's normal idle countdown then runs
from that moment, not from when its lanes went idle. To extend a hold, `PATCH`
it with a new `ttl_secs`: it then ends that many seconds from now (shorter is
allowed). `ttl_secs` is 1 to `server.max_hold_secs` (default 14400, 4 h), else
400. `wake: true` wakes the machine in the background with its lanes' wake
command and returns at once: no wake is sent while any lane of the machine
answers (a power toggle would switch it off); the hold's `state` goes from
`waking` to `active`, or `wake_failed` with `wake_detail` (the hold still
counts). Poll the hold or the capacity report. A tenant has at most 8 active
holds (one more gets 429; keys without a tenant share one count). A client key
lists, extends and deletes only the holds its own tenant and project took;
another's answers 404, as for jobs. Operator and admin keys see all. With a
state directory (the gateway's `<state_root>`, unless
`server.artifacts.storage: memory`) holds are kept in `<state_root>/holds.json`
and survive a restart, so a restart never powers off a held machine;
otherwise they live in memory only. The capacity report's `holds.durable`
says which, and `holds.last_save_error` shows a failed write.

**Power a machine now.** An operator (or an admin) can power a managed
machine off or on, without waiting for the idle timer. A client key may
not: it gets 403.

```sh
curl -s -X POST "$SB/v1/hosts/gpu-box/power" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"action": "off"}'
# 200 {"host":"gpu-box","action":"off","powered_off":true,"already_off":false,"lanes":2,"released_holds":[]}
curl -s -X POST "$SB/v1/hosts/gpu-box/power" -H "authorization: Bearer $KEY" \
  -H 'content-type: application/json' -d '{"action": "on"}'
# 202 {"host":"gpu-box","action":"on","status":"waking","lanes":2}   (200 "status":"up" when it already runs)
```

`off` runs the machine's `poweroff_command`, exactly as the idle timer does,
and asks the same `poweroff_guard_command` first. It is refused with 409 and
a typed `reason` while the machine must stay on:

| `reason` | why |
|---|---|
| `hold_active` | a hold is active; the message names the holds. `{"action": "off", "release_holds": true}` ends the holds that were active when the request came, once every other check has passed, and names them in the answer (`released_holds`). A hold taken while the request is being checked refuses it, and then no hold ends. |
| `lane_busy` | a lane of the machine has work in flight or queued. |
| `guard_busy` | the machine's power-off guard says somebody else is using it. |
| `guard_unavailable` | the guard gave no answer; the machine stays on. |
| `uptime_too_short` | the machine has not been up long enough (see `poweroff_min_uptime_secs` below). |
| `no_poweroff_command` | the machine has no power-off command: it is always on and the gateway never powers it off. |
| `wake_in_flight` | a wake is running on the machine; it is coming up now. |
| `poweroff_in_flight` | another power-off (an operator's or the idle timer's) is already running. |
| `idle_check_in_flight` | the idle timer is checking the machine right now (its guard is still asking); the request waits up to 2 s for it first. Ask again in a moment. |

`on` wakes the machine through the same wake path a hold with `wake: true`
uses and answers at once (202): the wake runs in the background. It takes
no hold, so the idle timer still applies and the machine powers off again
once it has been idle long enough. A machine with no `wake_command` answers
409 (`no_wake_command`): it is always on.

The power-off holds the machine's wake lock from its first check until
the command has returned and every lane is marked off, as the idle
power-off does: no wake (a request's, a job's, a hold's) can bring a lane
up on a machine that is going away; it waits and wakes the machine again
afterwards. Holds and lane work are checked a last time under every
lane's lock, in the same step that marks the lanes `powering_off`, so a
request or a hold that arrives while the guard runs refuses the
power-off. A command that fails (502, `poweroff_failed`) leaves the lanes
as they were, so the idle power-off still runs later.

A machine none of whose lanes answers a fresh health probe is already
off, whoever put it to sleep: the answer is 200 with `"already_off":
true` and no command runs (one that reaches a sleeping machine fails,
and a false failure would escalate the lane and alert).

Every refusal and every action is written to the gateway log. A power-off
that fails escalates the lane and fires the same `poweroff_failed` alert
the idle power-off fires; a successful one alerts nothing.

`poweroff_min_uptime_secs` on a lane (default 0, no minimum) is the
shortest time the machine may have been up before it is powered off. An
operator's `off` is refused while the machine is younger
(`uptime_too_short`); the idle power-off waits until it is old enough. The
uptime runs from the first answer of one of the machine's lanes since
every lane was last seen down, so a machine that slept and came back, or a
wake that timed out, starts a new count. On a shared machine the
strictest lane counts.

**The capacity report.** Each lane in `GET /v1/workloads/capacity` carries
`poweroff_in_secs`: seconds until its machine's idle power-off, `null` while
the machine has work or a waiting request, is held, is off, or has no
`poweroff_command`; `0` once due (a busy guard may still postpone it). Each
lane also lists `holds`, its machine's active holds (`id`, `reason`,
`expires_at`, `expires_at_ms`, `owner` (tenant name, never a key), `state`).
A client key (it got 403 before) reads a view of its own: per lane `name`,
`host`, `state`, `level`, `queue_depth`, `in_flight`, `slots`,
`poweroff_in_secs`, and its own holds only, with `"view": "client"`; no details, errors, commands,
artifact store or alert state, and nothing of another tenant. Operator and
admin keys get the full report, which also shows `alerts`.

**Limit each lane: `max_in_flight` and its queue.** One slow local engine
must not be flooded. Give its lane a limit:

```yaml
local_executors:
  - name: llamacpp
    host: gpu-box
    # ... wake, power and health settings as above ...
    max_in_flight: 2          # at most 2 requests and media jobs at once (unset = no limit)
    queue_timeout_secs: 60    # the longest one waits for a place (default 60; 0 = never wait)
    on_full: spill            # spill (default) | queue
```

Every text or embeddings request dispatched to the lane's provider takes a
place, and so does every media job; the place is given back when the
response is complete, the stream ends, the client goes away, the attempt
fails, or the task panics or is cancelled. Work past the limit waits in the
lane's queue, oldest first: a newcomer never jumps it. A request waiting there
counts in the lane's `queue_depth`, so the machine's idle power-off waits for
it; one whose client disconnects leaves the queue at once.

What a request does when its route's first choice is on a full lane:

- `on_full: spill` (default): it goes at once to the route's next target that
  can serve it. The lane's targets are skipped with the reason (`local lane
  `llamacpp` is full (2 of 2 in flight, 0 queued), spilled to the route's next
  target`), never counted as failures. Only when no other target can serve
  does the request wait in the queue.
- `on_full: queue`: it waits in the queue first, and falls through to the next
  target only once `queue_timeout_secs` passed.

A request still waiting after `queue_timeout_secs` falls through to the next
target, or, when nothing else can serve, gets a retryable 503 with
`Retry-After: 5` and the error code `local_lane_queue_timeout`. A full lane
reached as a later fallback (or by a hedge racer other than the first
choice's own) is skipped at once, never waited for; when every target of the
route is skipped that way, the 503 carries `Retry-After: 10` (the same hint
as a lane that does not hold its GPU group), not 5. A media job always
queues, and one that waits past the timeout fails with the same code (503 on
the blocking image route).

The order of the waits differs by kind. A text or embeddings request first
waits for any wake (`wake_wait_secs`), then for a place, then for the GPU
group. A media job waits for its place before the wake and before the GPU
group: a queued job holds no GPU, so a waiting media job can still take the
group when the running work ends. Either way a request waits at most
`wake_wait_secs` + `queue_timeout_secs` + `group_wait_secs`. A reload that
changes the limit keeps the work that holds a place and the order of the
queue; a lower limit only makes new work wait.

With hedging on, the place the first choice's lane gave the request goes to
that lane's racer: one hedged request takes one place on the lane.

Known limit: a request that spilled does not come back to the lane. Once the
lane is full and the request spills, the lane's targets are skipped for the
rest of that request, so when the spill target then fails (a 5xx, a 429) the
request falls through to the route's later targets, or fails, even if the lane
has room again by then.

Each lane in `GET /v1/workloads/capacity` shows its places:

```json
"slots": {"max_in_flight": 2, "in_use": 2, "queued": 1,
          "on_full": "spill", "queue_timeout_secs": 60}
```

**Measured speed of each target.** Every text usage record carries how fast
it was served, each number with its source:

- `ttft_ms`: time from dispatching the attempt to its first token (wake, queue
  and GPU waits come before the dispatch and are not in it); always `gateway`.
- `prompt_tps`: prompt tokens per second.
- `decode_tps`: generated tokens per second.
- `ttft_source`, `prompt_tps_source`, `decode_tps_source`: `engine` when the
  engine reported its own timing (a llama.cpp server's `timings`: `prompt_n`,
  `prompt_ms`, `predicted_n`, `predicted_ms`, on the response or on the last
  stream chunk), `gateway` when computed from the gateway's clocks:
  `decode = output_tokens / (end - first token)` and `prompt = input_tokens /
  ttft` (a lower bound: it includes the network and any upstream queue).

A number nothing measured is left out rather than made up: no token, no
tokens counted, or a response that arrived in one piece (no decode window;
a gateway decode window under 50 ms counts as none).
Engine timings never reach the client. They are stored with the usage
(SQLite migration 20; older rows have none) and summarized, read-only, at
`GET /v1/targets/stats` (any key, client keys included):

```sh
curl -s "$SB/v1/targets/stats?limit=2000" -H "authorization: Bearer $KEY"
# {"object":"list",
#  "window":{"limit":2000,"records":1840,"durable":true},
#  "data":[{"target":"llamacpp/qwen","provider_id":"llamacpp","model":"qwen",
#           "count":1200,"last_seen_unix":...,
#           "p50_decode_tps":41.7,"p50_prompt_tps":1630.2,"p50_ttft_ms":180,
#           "decode_sources":{"engine":1190,"gateway":10},
#           "prompt_sources":{"engine":1190,"gateway":10},"ttft_samples":1200}]}
```

The window is the most recent `limit` usage records (default 2000, at most
5000). A client or operator key sees only its own tenant's records within
that window, on targets its tenant can reach (`records` then counts those
only), and nothing of another tenant; an admin key sees every target. A
target's medians mix sources when it reported both; the
`*_sources` counts say how. Routing does not read these numbers yet.

**Alerts.** Set `server.alert_command` to hear about a problem without
watching the report:

```yaml
server:
  alert_command: ["/usr/local/bin/lane-alert", "--inbox", "/var/lib/alerts"]
  alert_after_secs: 180      # warn/fail this long before it alerts (default)
  alert_repeat_secs: 21600   # repeat while the problem stays (default 6 h)
  alert_timeout_secs: 20     # one run is stopped after this (default)
```

The command is an argv list run without a shell; its first item must be an
absolute path. After every supervisor pass Switchback checks each lane. Kinds:

| `kind` | When | Sent |
|---|---|---|
| `lane_warn`, `lane_fail` | the lane is `warn` or `fail` for `alert_after_secs` | after the delay |
| `escalated` | the lane is escalated or degraded (needs an operator) | at once |
| `wake_failed` | a wake never reached health (boot timeout) or its command failed | at once |
| `poweroff_failed` | a power-off failed after all its retries | at once, once per machine |
| `guard_broken` | the power-off guard gave no answer (exit 126, 127 or timeout) `retry_budget` times in a row, for `alert_after_secs` | after the delay, once per machine |
| `recovered` | a lane that alerted is `ok` again | once |
| `test` | `POST /v1/admin/alerts/test` | on request |

A normal wake shows `warn` for a minute or two and stays below the delay. A
lane whose only `warn` is a missing `wake_command` never alerts (that is
configuration, not an event). While a problem stays, the same kind repeats
every `alert_repeat_secs`, not more often; a new kind (a `warn` that becomes an
escalation) is sent when due.

Each event runs the command once with one JSON object on stdin and a one-line
plain summary in env `SWITCHBACK_ALERT_SUMMARY`
(`switchback escalated: llamacpp on gpu-box is fail: ...`). The payload,
schema `switchback/lane-alert@1` (a field change gets a new schema id):

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | `switchback/lane-alert@1` |
| `kind` | string | one of the kinds above |
| `lane` | string or null | the lane (`null` for `test`) |
| `host` | string or null | the lane's machine: its `host`, or the lane name |
| `level` | string | `ok`, `warn` or `fail` now |
| `previous_level` | string or null | the level before the problem; for `recovered`, the problem's level |
| `detail` | string | what the report says, redacted |
| `since` | string or null | when the problem began, RFC 3339 UTC |
| `since_ms` | number or null | the same, ms since the Unix epoch |
| `sent_at` | string | when this run started, RFC 3339 UTC |
| `gateway` | string | the gateway machine's host name |

The payload never holds a key, a credential in a URL, or a command line:
`detail` loses every configured command (wake, power-off, restart, release,
guard, alert) as `<command>`, every secret value in the config (inline keys
and the values of the env vars the config names) as `<redacted>`, a URL's
user info and query, and token-like words (the gateway's inline-secret
redaction). Exit 0 is delivered. Any other exit, a failure to start, or a run
past `alert_timeout_secs` (the program is killed) keeps the event pending and
tries again after 60 s, doubling to 1 h; at most 64 events wait (older ones
are dropped and counted). Delivery runs in its own task: a failing or hanging
command never blocks or delays lane work, a wake or a power-off. The operator
report shows `alerts.configured`, `last_sent_at`, `last_error`, `pending`,
`dropped` and `next_try_at`. `POST /v1/admin/alerts/test` (admin keys only)
sends one `kind: "test"` event now and answers
`{"sent": true, "exit_code": 0}` (`"sent": false` with the code on a non-zero
exit, 502 when the command could not run or timed out, 409 when no command is
set), to prove the path on a live gateway.

**ComfyUI behind a token proxy.** ComfyUI itself has no authentication, so a
real setup often puts it behind a proxy that wants a token and answers 401
without one. Give the `comfyui` provider the credential the same way as an
`openai_compatible` provider: `api_key_env` (or an account in `accounts:`)
and, when the proxy does not take a bearer token, `auth_scheme`:

```yaml
providers:
  - id: comfy
    type: comfyui
    base_url: "http://executor-1.example:8188"
    api_key_env: COMFY_PROXY_TOKEN
    auth_scheme: { kind: header, name: x-api-key }   # default: bearer; or { kind: query, name: token }
    workflows: [...]
```

Switchback then sends it on every ComfyUI call it makes (queueing the
prompt, polling the history, fetching each artifact, and the
`/system_stats` check of `sb provider doctor`), and the health probe of the
managed lane with the provider's id carries it too, but only when the
lane's `health_endpoint` is on the provider's `base_url` origin (scheme,
host and port); it is never sent anywhere else, and a call that carries it
follows no redirect (as the text adapters' calls do not), so a redirect to
another origin cannot take it along. The secret is a lease like
any provider key: it is not logged, not put in an error text (an error never
quotes a URL that carries it), and not shown in the capacity report. A 401
or 403 from ComfyUI reads as a refused credential: a job fails at once with
`ComfyUI refused the credential`, and a managed ComfyUI lane whose probe is
refused fails its job at once with `health endpoint answered 401: the engine
refuses the credential` (level `fail`), instead of waking the machine and
waiting out `boot_timeout_secs`. Without `api_key_env` nothing is sent, as
before.

### Model settings (`/v1/models/{model}/settings`)

Every setting of a local model can be read and changed through Switchback,
the way a desktop model runner lets you set a model's context length, GPU
offload, sampling and reasoning, and load or unload it. There is one API and
one CLI, and each setting can also be set for a single request.

Settings attach to a **target model** (`provider/model`, for example
`text/qwen3-35b`), because several public names can point at one engine
model. A route can name a profile to use (`settings_profile`).

**The settings document of a target:**

| Part | What it holds |
|---|---|
| `defaults` | request-time settings applied to every request that reaches the target |
| `profiles` | named sets of the same keys (`nothink`, `precise`, `long`, ...), layered over `defaults` |
| `locked` | keys a request may not override (`temperature`, `reasoning`, `reasoning.enabled`, `extra.mirostat`, ...) |
| `load` | load-time settings (need an engine control on the lane, below) |
| `engine_args` | any other engine argument, by its own name (operator and admin keys only) |
| `request_load` | the policy for load settings a request asks for (variants, below); absent = refused |

**Request-time keys** (typed and range-checked; anything else under
`extra`): `temperature`, `top_p`, `top_k`, `min_p`, `typical_p`,
`repeat_penalty`, `repeat_last_n`, `presence_penalty`, `frequency_penalty`,
`seed`, `max_tokens` (the default when the request gives none), `stop`,
`system_prompt` with `system_prompt_mode: prepend | replace_if_absent`,
`response_format` (the default when the request gives none), `reasoning`
`{enabled, effort: none|low|medium|high, budget_tokens}`, and `extra` for
engine-specific per-request keys sent as they are (`dry_multiplier`,
`xtc_probability`, `mirostat`, `n_probs`, `cache_prompt`,
`speculative.n_max`, ...). An unknown key outside `extra` is refused with
the list of keys. `reasoning` is translated per engine: on llama.cpp and
vLLM `enabled: false` or `effort: none` becomes
`chat_template_kwargs.enable_thinking: false`, an effort becomes
`enable_thinking: true` plus the effort; `budget_tokens` goes to vLLM as
`thinking_token_budget` and is load-time on llama.cpp (`load.reasoning_budget`),
so per request it is skipped there and the response header says so.
A `repeat_penalty` the settings set reaches vLLM as `repetition_penalty`
(a request's own `repetition_penalty` counts as its value for that key, and a
lock replaces it); nothing is renamed that the settings did not set.

**Precedence per key, highest first:** the request's own value (a plain
OpenAI or llama.cpp field, or `switchback.settings`), unless the key is
locked > the profile the request asked for > the route's
`settings_profile` > the target's `defaults` > nothing (the engine's own
default). A key nobody set is not sent: a target with no settings gets
exactly the request it got before, byte for byte.

A lock stops values that come with the request. Profiles are part of the
settings document, which only operator and admin keys write, so a profile
may set a locked key, and a client that picks that profile gets the
profile's value: the operator chose both. A request carrying its own
settings (`switchback` object or `x-switchback-profile`) bypasses the
exact-request cache receipt.

**Per request:** pick a profile with the header `x-switchback-profile: NAME`
or the body `"switchback": {"profile": "NAME"}`, and send any key for this
request only in `"switchback": {"settings": {...}}`. The `switchback` object
is taken out before dispatch and never reaches an upstream. An unknown
profile is a 400 that lists the profiles. The response header
`x-switchback-settings` says what happened, for example
`profile=nothink; locked=temperature; applied=4` (key names, never values);
the same line rides `x-switchback-route` as `settings=[...]`.

```bash
curl -s localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -H 'x-switchback-profile: nothink' \
  -d '{"model":"fast","messages":[{"role":"user","content":"hi"}],
       "switchback":{"settings":{"top_k":20,"reasoning":{"enabled":false}}}}'
```

```python
from openai import OpenAI
client = OpenAI(base_url="http://gateway.example.invalid:8765/v1", api_key="sk-...")
client.chat.completions.create(
    model="fast",
    messages=[{"role": "user", "content": "hi"}],
    extra_headers={"x-switchback-profile": "precise"},
    extra_body={"switchback": {"settings": {"min_p": 0.05}, "load": {"ctx_size": 65536}}},
)
```

**The API.** Operator and admin keys write; client keys read only the
targets their tenant reaches, and never see load settings, engine arguments
or paths. Nothing here wakes a machine.

| Call | What it does |
|---|---|
| `GET /v1/models/{model}/settings[?profile=NAME]` | the effective view of the target(s) behind a public name or `provider/model`: every key with its value and source (`defaults`, `profile:NAME`, `route_profile:NAME`, or `engine` when nothing sets it) and whether it is locked; the profiles; the revision (also as `ETag`); for operator keys the whole document and the load view (each load key, the engine key it becomes, `pending` until reload, the engine's memory numbers and the last apply) |
| `PUT /v1/models/{model}/settings` | replace the document |
| `PATCH /v1/models/{model}/settings` | JSON merge patch: an object merges, `null` removes a key |
| `GET /v1/models/{model}/settings/history` | every change, newest first: revision, time, who (the key's tenant and role, never the key), from where (the client's user agent), which keys changed, and the document after it |
| `POST /v1/models/{model}/settings/revert {"revision": N}` | write revision N's document again as a new revision (with `If-Match`, like any write) |
| `POST /v1/models/{model}/load {"wake": false}` | preload the model (below) |
| `POST /v1/models/{model}/unload` | unload it (below) |
| `GET /v1/engines` | per engine lane: its models with status, arguments, memory, pending settings, requests admitted (`in_flight`) and running (`dispatched`), the models whose new settings wait until they are idle (`deferred`), the variants and the last apply |

A write must say which revision it edits (`If-Match: N`, quoted `"N"` or
`*` for whatever is current, or `"revision": N` in the body; 0 for a target
never written); without one it is a 428, and a stale one is a 409 with
`current_revision`. A revert is a write too: `{"revision": N}` names the
revision to write again, and `If-Match` the one it edits. It goes through
the same checks and the same apply as a PUT (and takes `apply` and `wake`
like one). The `ETag` is the quoted revision (`"3"`). Writes to one target
need a name that reaches one target. The documents are durable in the state
store (`server.state_store`) with their history; without a store they live
as long as the process (`durable: false` in the view). The history keeps the
newest `server.model_settings_history_keep` revisions of each target
(default 500); a revert reaches any of them.

**The CLI** (`sb ai`, with the same exit codes as the other commands):

```bash
sb ai settings text/qwen3-35b                  # every key: value, source, lock
sb ai settings fast --profile nothink          # what a request naming nothink gets
sb ai set text/qwen3-35b temperature=0.6 top_k=20 min_p=0.05
sb ai set text/qwen3-35b reasoning.enabled=false temperature=0.7 --profile nothink
sb ai set text/qwen3-35b seed=7 --lock
sb ai unset text/qwen3-35b top_k
sb ai settings text/qwen3-35b --history
sb ai revert text/qwen3-35b 3
sb ai chat fast "Say hi" --profile nothink --set top_k=5   # answer, tokens, speed, route
```

#### Load-time settings and the engine control

Load-time settings change how the engine loads a model, so they need the
lane's engine to be controllable. For llama.cpp in router mode (one
`llama-server` with many models, per-model arguments from a presets INI
given with `--models-preset`), give the lane an `engine:` block:

```yaml
providers:
  - id: text
    type: openai_compatible
    engine: llama_cpp
    base_url: "http://gpu-box.example.invalid:8080/v1"

local_executors:
  - name: text                                  # the provider's id
    base_url: "http://gpu-box.example.invalid:8080"
    health_endpoint: "http://gpu-box.example.invalid:8080/health"
    wake_command: "ssh gateway wake-host gpu-box"
    poweroff_command: "ssh gpu-box sudo poweroff"
    engine:
      kind: llama_cpp_router
      # base_url: "http://gpu-box.example.invalid:8080"   # default: the provider's, without /v1
      settings_command: "ssh gpu-box python3 /opt/switchback/llamacpp-preset-overlay.py --base /etc/llama/models.base.ini --out /etc/llama/models.ini"
      allow_model_path: false     # true lets load settings and engine_args name files
      apply_wait_secs: 120        # how long apply-now and unload wait for a model's requests
      load_timeout_secs: 600      # how long a load may take
```

The engine control's calls to the engine (the model listing,
`GET /models?reload=1`, `/models/load`, `/models/unload`, `/props`) carry the
provider's key, exactly as its text requests do: `api_key_env` or `api_key`,
with `auth_scheme` (bearer unless set). So a router started with `--api-key`
needs only that key on the provider. The lane's health probe stays keyless
(llama.cpp keeps `/health` open).

The gateway never writes files on a machine. On a change it runs
`settings_command` with the managed overlay as JSON on stdin (every managed
load-time setting of the lane's models and their variants), then asks an
awake router to re-read its presets (`GET /models?reload=1`), never while a
request runs on a model whose section changed (see below):

```json
{"version": 1, "lane": "text", "engine": "llama_cpp_router",
 "models": {"qwen3-35b": {"settings": {"ctx-size": "65536", "cache-type-k": "q8_0"}},
            "qwen3-35b~1a2b3c4d": {"base": "qwen3-35b", "settings": {"ctx-size": "131072"}}}}
```

`contrib/llamacpp-preset-overlay.py` is the reference command (standard
library only). It merges a hand-written base INI with the overlay into the
INI the router reads, atomically and only when it changes. It keeps every
hand-written section, key and comment, marks the keys it manages with
`; managed by switchback`, copies a variant's section from its base, and
refuses (exit 2, output left alone) to change a key the base pins with a
comment line `; switchback-pinned: ctx-size, n-gpu-layers` (in a section, or
in `[*]` for every model), and to write a section the base file lacks (a
model name that is neither a base section nor a variant of one would have
no `model =` line).

**Load keys** (typed, mapped to llama.cpp preset keys): `ctx_size`,
`n_gpu_layers` (number, `auto` or `all`), `cache_type_k`, `cache_type_v`,
`flash_attn` (`on`/`off`/`auto` or a bool), `batch_size`, `ubatch_size`,
`parallel`, `threads`, `threads_batch`, `mlock`, `mmap` (both become
`load-mode`), `load_mode`, `rope_scaling`, `rope_freq_base`,
`rope_freq_scale`, `yarn_orig_ctx`, `yarn_ext_factor`, `yarn_attn_factor`,
`yarn_beta_slow`, `yarn_beta_fast`, `spec_type`, `spec_draft_model`,
`draft_max`, `draft_min` (`spec-draft-n-max`/`-min`), `mmproj` (a path, or
`off`), `sleep_idle_seconds`, `fit`, `fit_ctx`, `reasoning_budget`, and
sampling defaults `temp`, `top_k`, `top_p`, `min_p`. `engine_args` takes other
`llama-server` arguments by their long name without dashes
(`"n-cpu-moe": 4`) from a per-model allow list: speed, memory and offload,
sampling defaults, context and RoPE, speculative decoding with local files,
local model files (`mmproj`, `lora`, ...), chat template and reasoning.
Anything else is refused by name: tools and agents that run commands
(`tools`, `agent`, `allow-unauthenticated-tools`, `tools-runtime`), MCP
servers (`mcp-servers-json`, `mcp-servers-config`), downloads from a URL or
a model hub (`mmproj-url`, `spec-draft-hf`, `hf-*`, `*-url`, the
`*-default` presets), remote back ends (`rpc`), and server-wide switches.
Arguments the router controls (`host`, `port`, `api-key`, `alias`,
`models-*`, ...) are refused, and arguments that name files need
`allow_model_path`. Load settings and engine arguments are refused for a
model the engine does not list.

```bash
sb ai set text/qwen3-35b ctx_size=65536 cache_type_k=q8_0 cache_type_v=q8_0 --load
sb ai set text/qwen3-35b engine_args.n-cpu-moe=4 --load --apply now
sb ai load text/qwen3-35b --wake
sb ai engines
sb ai hold 2h "eval run" --wake --load text/qwen3-35b
```

**When a load setting takes effect** (`apply` in the write body, `--apply` in
the CLI). The write itself always stands and answers 200; its `apply` part
says what the engine did.

The rule behind both modes: a request admitted to a model or already
running on it is never cut off. llama.cpp unloads a running model whose
preset changed when it re-reads its presets, so the gateway compares the
overlay with the one the engine last read, and every model whose section
changes is held (no new request is admitted to it or dispatched to it) for
the settings command and the re-read. A model with a request running on it
keeps its old section until it is idle (`deferred` in the answer and in
`GET /v1/engines`), and the gateway applies it by itself when its last
request ends. A change to other, idle models goes ahead at once.

- `next_load` (default): the engine re-reads a model's settings when the
  model is idle. An idle loaded model is unloaded by the re-read and loads
  with the new settings on its next request; a busy one is applied when its
  last request ends (`pending` until then).
- `now`: no new request is admitted to the model; the requests already
  admitted (including ones queued for the lane or waiting for a wake) end
  first, up to `apply_wait_secs`. Then the settings are applied and a loaded
  model is unloaded and loaded again before new requests reach it. If it is
  still busy after `apply_wait_secs`, nothing is reloaded: the answer has
  `reloaded: false, reason: model_busy, pending: true`, and the engine
  re-reads the settings when the model is idle. A sleeping machine is not
  woken unless `"wake": true`; the model simply starts with the new settings.

A failed `settings_command` (or a router that does not re-read) leaves the
write in place, shows in the answer and in `GET /v1/engines`
(`pending_apply`), and is retried when the lane comes up (a request for a
changed model that reaches the lane first waits for it), before the next
load, and before a request for a model whose new settings did not reach the
engine takes its lane place. That last retry backs off: 15 s after the first
failure, doubling with each failure in a row, at most 10 minutes; it never
runs while another apply runs, and a request for a model whose settings the
engine already has is never delayed by it. A write while the machine wakes
is kept pending too: the engine may have read its presets already, so it
re-reads them once the lane is up.

**Load and unload.** `POST /v1/models/{model}/load` preloads the model: on a
sleeping lane it is a 409 `lane_asleep` unless `"wake": true`; it takes the
lane's place and GPU group exactly like a request (exclusive groups hand the
GPU over), counts as lane activity only while it loads, and waits for the
router to report the model loaded. `POST /v1/models/{model}/unload` stops
admitting new requests to the model, waits for the ones already admitted
(including queued and waking ones) up to `apply_wait_secs`, then unloads;
if they do not end in time it is a 409 `model_busy` and nothing is unloaded.
It never powers anything off; on a sleeping machine it answers that nothing
is loaded. Chat, embeddings and audio requests all count on their model, to
the end of the answer (a stream's last byte). While an unload or an `apply:
now` holds a model, a request whose route has another target goes there at
once (the decision names the reason); one with no other target waits (up to
`apply_wait_secs`, and never less than the lane's `command_timeout_secs` plus
30 s) and then gets 503 `model_paused` with `Retry-After`. `GET /v1/engines` never wakes anything and never calls an
engine the lane lifecycle knows to be down or going down: such a lane shows
its listing from when it was last seen; the other lanes are asked at once,
each with a short timeout. A hold can name a model to load once the machine
is up (`"load": "text/qwen3-35b"` in the hold body; operator and admin
keys).

#### Load settings per request (variants)

A request may ask for load-time settings: `"switchback": {"load":
{"ctx_size": 65536}}`. The target's `request_load` policy decides:

```yaml
# PUT /v1/models/text/qwen3-35b/settings (JSON shown as YAML)
request_load:
  allow: [ctx_size]                    # what client keys may set
  bounds: {ctx_size: [4096, 131072]}   # inclusive, for every key
  max_variants: 2
  variant_idle_ttl_secs: 3600
  # estimate: {weights_bytes: N, budget_bytes: N,       # bytes; see the fit check
  #            kv_layers: 48, kv_heads: 8, head_dim: 128}
  # reserve_bytes: N     # GPU bytes kept free for another engine on the GPU
```

Without a policy the request is refused (400 `request_load_disabled`). Client
keys may set only `allow`ed keys; operator and admin keys any typed load key;
every key stays within `bounds`; a request never sets a file path. The
gateway then makes a **variant** of the first target, `<model>~<hash8>`:
the base model's section plus the keys, in the managed overlay, applied
through `settings_command` and a preset re-read. The base must be a model
the engine lists (400 `unknown_engine_model`). The request goes to the
variant's name, which the router loads (unloading others per its
`--models-max`). The same keys reuse the same variant. Other targets of the
route leave the plan, since they could not honour the request.
`x-switchback-settings` names the variant.

Before a variant exists it must fit. With the engine's own measurement of
the base model (`GET /models` `memory`): weights and compute as measured,
the KV cache scaled to the variant's context and cache types, checked
against 95% of the GPU memory the measurement saw, minus what the other
models the engine has loaded on those GPUs use (the base stays loaded
beside the variant) and minus the policy's `reserve_bytes` (memory another
engine on the same GPU needs, such as an image engine). Without a
measurement the policy's `estimate` is used (the same KV math as
`sb ai fit`: layers × context × heads × (head_dim + value_dim) × bytes per
element; `sb ai fit --json` gives the numbers) against its `budget_bytes`
minus `reserve_bytes`. A variant that sets no context gets the base's own:
its `load.ctx_size`, else the running engine's `--ctx-size`, else the
model's trained context; never a guess. A variant that would not fit is a
409 `would_not_fit` with the numbers. When nothing can check the fit (no
measurement and no estimate, or no known context), a client key gets 409
`fit_unknown` saying what to add; an operator or admin key may go ahead,
and `x-switchback-settings` then says `fit=unchecked`.

A request holds its variant from the moment the variant is found or made,
while it still waits for its lane: at `max_variants` the least recently
used variant with no request gives way, and when every variant has one the
new request gets 409 `too_many_variants`. A variant unused for
`variant_idle_ttl_secs` leaves the overlay on the next apply. Variants live
in the gateway's memory: after a restart the first apply writes the whole
overlay without them (the reference script then drops their sections), and
`GET /v1/engines` says so until that apply. `GET /v1/engines` lists the
variants.

### Making local models fast

Switchback applies the method of the inference-optimisation skill
(<https://github.com/sybil-solutions/inference-optimisation-skill>) to its
local engines, in three layers:

1. **Per-request tricks** (always on once configured, no experiments):
   conversation stickiness, loaded-model preference, big-prompt placement
   and the reasoning guard.
2. **Watching** (read-only): ceilings per target and a speed report with
   findings, each with its evidence and one suggested action.
3. **Experiments** (`sb ai tune`): controlled A/B/A runs that keep a
   settings change only when it measurably wins.

#### Layer 1: per-request tricks

Every trick is off by default. `server.optimize` turns one on for every
route; a route's `optimize` block replaces a field for that route (a field
left out keeps the server's):

```yaml
server:
  optimize:
    stickiness: true            # keep a conversation on its machine
    sticky_ttl_secs: 1800       # how long a conversation stays sticky
    sticky_max_conversations: 10000   # at the limit, the busiest tenant forgets its oldest
    loaded_aware: true          # prefer the target whose model is loaded
    big_prompt_tokens: 8000     # a prompt this big goes to the fastest prefill
routes:
  - name: chat
    match: { model: chat }
    optimize: { stickiness: false }   # this route: no stickiness
    targets: ["gpu-box/qwen", "small-host/gemma"]
```

None of them wakes a machine or makes a request wait. They only reorder
targets that are already up, healthy and have room (a target that needs a
wake never overtakes one that is up), and a route with `pin_order: true`
keeps its order. What they did rides the response header
`x-switchback-optimize` and the route summary (`optimize=[...]` on
`x-switchback-route`), for example
`sticky=gpu-box/qwen; slot=1` or `loaded=gpu-box/b over gpu-box/a (no model swap)`.

- **Conversation stickiness.** A conversation is the session the client
  names (header `x-switchback-session`, read only while stickiness is on,
  or `x-switchback-session-id`, `metadata.session_id`), each within its
  tenant and project, else the OpenAI `user` field plus a hash of the
  system prompt and the first user message (both stay the same for every
  turn). While the target that served the conversation last is up, healthy
  and not full, the next turn goes there first, so the engine reuses its
  prefix cache. When it is asleep, full or failing, the turn goes where the
  route sends it and the header says why
  (`sticky=gpu-box/qwen not kept (gpu-box asleep (wake ~65s))`). When the
  engine says which slot served the turn (llama.cpp `id_slot`) and the
  target has `load.parallel` above 1, the next turn names that slot
  (`id_slot`), but only while no other request runs on that model, so the
  pinned slot cannot make it wait. The gateway keeps only a hash of the
  conversation, never its text.
- **Cached prompt tokens** are recorded for every request: from llama.cpp's
  `timings.cache_n` (or `tokens_cached`) and from OpenAI's
  `usage.prompt_tokens_details.cached_tokens`, into the usage record's
  `cached_input_tokens`. The speed report shows the hit rate per target.
- **Loaded-model preference** (`loaded_aware`). When a route has several
  targets on one awake machine (two names on one llama.cpp router, for
  example), the one whose model the engine has loaded goes first: on a
  router with `--models-max 1` a request for another model unloads the
  current one, which costs seconds. The gateway knows what is loaded from
  the engine control's last listing (`GET /v1/engines`, loads, applies) and
  from what it served itself; it never asks a sleeping machine. The listing stays fresh on its own: every 30 s
  the gateway asks each engine lane that is up (healthy or counting down
  to idle) for `GET /models`, with a 3 s timeout, and again right after
  every load, unload and apply it does itself; a lane that is off,
  waking, powering off or draining is never asked, and nothing is woken. Set
  `models_max` on the lane's `engine:` block to the router's
  `--models-max` so the gateway knows a load unloads another model. Every
  load a request caused counts as a swap, per target and per hour, with the
  model it replaced.
- **Big-prompt placement** (`big_prompt_tokens`). On a route with
  `latency_aware` or `prefer_awake`, a prompt of at least that many
  (estimated) tokens goes to the awake target with the best measured prompt
  speed (`big_prompt ~9000 tok -> gpu-box/qwen (10000 prompt tok/s)`). Only
  local engines behind a lane (a `local_executors` entry and a provider with
  `engine: llama_cpp`, `vllm` or `ollama`) can be picked: a cloud target
  often measures the fastest prefill, and it costs money. The
  existing ordering does not do this (a long-output request ranks on decode
  speed, a streaming one on time to first token), so it is its own step.
  Stickiness wins over it: a conversation's cached prefix is cheaper than a
  faster prefill.
- **Reasoning guard.** A model that thinks before it answers can spend a
  small `max_tokens` budget on thinking and return an empty answer with
  `finish_reason: length`. Mark the target in its settings and turn the
  guard on:

  ```bash
  sb ai set gpu-box/qwen reasoning=true reasoning_guard.min_tokens=1024
  ```

  A request whose `max_tokens` is below `min_tokens` (default 1024) then
  gets thinking off for that request (`reasoning_effort: none`, which the
  engine codec turns into `chat_template_kwargs.enable_thinking: false` on
  llama.cpp and vLLM), unless the request asked for thinking itself
  (`reasoning_effort` other than `none`, `enable_thinking: true`, or
  `reasoning` in its own `switchback.settings`). A locked `reasoning` is
  left alone. `x-switchback-settings` says
  `reasoning_guard=off (max_tokens 256 < 1024)` (or `kept (the request
  asked for thinking)`). The gateway counts the failure the guard prevents
  on every target, guard or not: an answer with no text after thinking,
  ended by the token limit.

#### Layer 2: watching (the speed report)

**Ceilings first.** Decode speed is bound by memory bandwidth: a token reads
the weights it touches (and its KV cache), so the most a target can decode
is bandwidth ÷ bytes per token. Give each local target its ceiling in its
settings document, `ceiling: {decode_tps, prefill_tps, bytes_per_token,
source}`. `sb ai fit` computes bytes per token from a model repository and
writes it (operator key):

```bash
sb ai fit unsloth/Qwen3-35B-GGUF --store gpu-box/qwen --quant Q4_K_M --bandwidth 900
# gpu-box/qwen: ceiling 3.10 GB per token, decode up to 290 tok/s (revision 7)
```

Without `--quant` it takes the quant a machine of `--machines` would load,
else the first one. A machine's bandwidth and peak compute may also live in
the config:

```yaml
server:
  optimize:
    hosts:
      gpu-box: { bandwidth_gbs: 900, peak_tflops: 160 }
```

When a host has no `bandwidth_gbs`, the report calibrates an effective
bandwidth from the best measured target on that host (decode × bytes per
token) and says `calibrated`: that target then reads 100%, and every other
target on the host is measured against it.

**The report** (`GET /v1/optimize/report[?model=NAME]`, `sb ai doctor
[MODEL] [--json]`) lists, per local target (every target of a lane in
`local_executors`, plus models the engine control has listed): measured
decode (median over the usage window), prompt speed and time to first token,
the ceiling and the share of it, model swaps in the last hour and day,
prefix-cache hits (tokens and requests, and how often a repeated system
prompt hit the cache), speculative acceptance (`draft_n_accepted ÷
draft_n`), empty answers after thinking, reasoning-guard uses, lane queue
waits, wakes and their measured cost, and when it was last used. Then the
findings, each with its evidence and one suggested action:

| `kind` | when (defaults in `server.optimize.report`) | suggested action |
|---|---|---|
| `decode_below_ceiling` | decode below 40% of the ceiling, 6 samples or more | likely layers on the CPU or a slow KV type; run `sb ai tune` |
| `swap_thrash` | more than 6 swaps to the target in the last hour | point both names at one model, or raise the router's models-max if memory allows (`sb ai fit`) |
| `cache_cold` | 10 or more requests repeated a system prompt and under 10% hit the cache | turn on stickiness (or, with it on, check `cache_prompt` and the slot count) |
| `low_acceptance` | speculative acceptance under 50% over 10 requests or more | try a shorter draft (`draft_max`) or no MTP |
| `empty_after_thinking` | empty answers after thinking and no reasoning guard | turn on the reasoning guard |
| `unused` | not used for 14 days (or never, in a usage window older than that) | removal candidate; Switchback deletes nothing |
| `regression` | decode or time to first token worse by more than 10% after the latest settings revision than under the revision before it, with 6 samples or more on each side; requests a tuning run sent (while its lock held the lane) do not count | `sb ai revert TARGET N-1` |

```bash
sb ai doctor
# target         decode* prompt  ttft   ceiling  swaps/h  cache  accept  empty  waits  wakes
# gpu-box/qwen   118     9850    190ms  41%      9        3%     71%     0      2      4
# * decode: all requests, median (any concurrency, not a single-stream c1 figure)
# host gpu-box: 900 GB/s (configured)
# findings
#   gpu-box/qwen: 9 model swaps to gpu-box/qwen in the last hour (limit 6), replacing gemma
#     -> point both names at one model, or raise the router's models-max if memory allows (check with `sb ai fit`)
```

The report never wakes a machine and never calls an engine: it reads the
usage ledger, the settings and their history, the evidence the gateway
keeps, the lane reports and the engine control's last listing. Operator and
admin keys see everything; a client key sees the targets its tenant reaches,
measured from its own requests only, with no gateway-wide counters and no
findings drawn from them (like `/v1/targets/stats`). The report holds no
paths and no engine arguments. Decode in the report is a median over all
requests (single-user requests dominate on a home machine); `sb ai tune`
measures concurrency separately.

#### Layer 3: experiments (`sb ai tune`)

`sb ai tune` runs the skill's loop on one local model, through the
gateway's own APIs (operator key):

```bash
sb ai tune gpu-box/qwen [--levers LIST] [--budget 30m] [--wake]
           [--workload chat|read|mixed] [--concurrency 1,2,3] [--samples 6]
           [--dry-run] [--history] [--json]
```

The method, exactly:

1. **Baseline**, then the **ceiling** (from the speed report).
2. **One change per experiment**, derived from the current best settings,
   applied with `apply: now` (the model reloads once nothing is in flight).
3. **Control, candidate, control.** Each run is at least 6 samples, and
   the median counts; prompts are fresh (a new first line every request), so
   no run reads the prefix cache; the first request after a load is dropped.
4. **Keep it only if** the candidate beats both neighbouring controls on its
   metric and no other measured metric is more than 3% worse than the mean
   of the two controls. When the two controls differ by more than 3% on one
   of those other metrics, that metric is held to the worse control
   instead. A win of more than 20% is repeated once first, and the repeat
   must win again by more than the noise (5%). A run whose two controls
   differ by more than 3% on the judged metric itself is noisy: it is
   repeated once the same way, the repeat decides, and a repeat that is
   noisy again changes nothing.
5. **Gates:** every answer must be non-empty, not looping and not failed; a
   change claimed lossless must give the same greedy answer at temperature 0;
   a change to the KV cache or the context must find two needles (at about
   50% and 100% of the context) exactly; a change to `parallel` must survive
   a concurrent soak at full per-slot context.
6. **Stop** after three levers in a row inside the noise (a plateau), at
   the end of `--budget`, or on Ctrl-C, SIGTERM or SIGHUP (all three
   restore the best settings, then let the lock and hold go).

The levers for `llama_cpp_router`, by expected payoff (the catalogue is data
in `cli/ai_tune.py`, so other engine kinds can add their own):

| lever | settings | metric | gate | notes |
|---|---|---|---|---|
| `spec_type` | `spec_type` | decode | greedy | MTP when the model has an MTP head or a draft file; +22% to +76% measured on large GPUs, can lose on bandwidth-poor chips |
| `draft_max` | `draft_max` 1 to 4 | weighted aggregate | greedy | swept at every `--concurrency`: there is no per-request draft length, so the value with the best workload-weighted aggregate wins; the table per concurrency is in the report |
| `flash_attn` | `flash_attn` | decode | greedy | |
| `ubatch_size` | `batch_size`, `ubatch_size` (512, 1024, 2048) | prefill | greedy | |
| `parallel` | `parallel` 1..N, `ctx_size` = N × per-slot context | aggregate | soak | fit check first (`request_load.estimate`) |
| `cache_type` | `cache_type_k`, `cache_type_v` f16 / q8_0 | decode | needle | known to lose on models with few KV layers |
| `n_gpu_layers` | `n_gpu_layers: all` against `fit: on` | decode | greedy | |
| `threads` | `threads` | decode | greedy | only when named (Mac) |
| `mmap` | `mmap` on/off | load time | none | |
| `sleep_idle_seconds` | — | — | — | advice from the measured swaps |

Whether MTP or a draft model is known comes from `GET /v1/engines`
`models[].speculative`: first the settings the gateway writes into the
preset overlay (`load.spec_type`, `load.spec_draft_model`, or `engine_args`
`spec-type` / `spec-draft-model`), then a loaded instance's `status.args`
(`--spec-type`, `--spec-draft-model`, `-md`), and only then the engine's
`sidecars`. A real llama.cpp router lists `sidecars` only for cached Hugging
Face models, never for preset ones. The engine build comes from `GET /props`
`build_info` (for example `b12359-b1b770d90`).

**Safety.** `sb ai tune` refuses to start (exit 1, with the reason) when
the machine is asleep and `--wake` is not given, when the lane has a request
in flight or queued, inside a quiet window (`server.optimize.quiet_windows`, see below), or when a lever it was asked for would change a locked load key
(`locked: ["load.ctx_size"]`, or `sb ai set MODEL ctx_size=65536 --load
--lock`; without `--levers` such a lever is skipped and said). While it
runs it holds the machine on (a host hold, so the idle timer cannot power it
off) and takes the lane's **tuning lock**: other chat, embeddings and audio
requests skip that lane (to another target, or a retryable 503
`lane_tuning` when nothing else can serve), and only the run's own requests
(header `x-switchback-tune-lock`) reach it. Ctrl-C or the end of the budget never cuts a request off: the
requests in flight finish, the best known settings are applied again (the
reload waits for the model to drain), and the lock and the hold are let go.

```
POST   /v1/optimize/locks {"target", "ttl_secs", "reason", "hold_id"}   201 {…, "hold"} | 409 quiet_window | lane_busy | locked
GET    /v1/optimize/locks
PATCH  /v1/optimize/locks/{id} {"ttl_secs"}                  200 {…, "quiet_window"} | 404 (lock gone)
DELETE /v1/optimize/locks/{id}
```

**Quiet windows** hold off tuning while something else needs the machine
(a nightly job, for example). Each window is a plain `"HH:MM-HH:MM"`
string or `{start, end, tz}` with an IANA time zone; a plain string uses
`server.optimize.timezone`, else UTC. A window may wrap midnight; an
unknown zone is a config error.

```yaml
server:
  optimize:
    timezone: Asia/Tokyo                 # for plain windows; absent = UTC
    quiet_windows:
      - "00:30-03:00"                         # Asia/Tokyo (from timezone)
      - {start: "12:00", end: "12:30", tz: UTC}
```

A window is checked when a run starts and again during the run: between
measurements (never during one) the tuner renews its lock with `PATCH`,
whose answer names the window in force. When one begins, the run stops at
that safe point, applies the best known settings again, and lets the lock
and the hold go (exit 0, `stopped: the quiet window … began`). Tuning locks
live in the gateway's memory: after a restart the heartbeat gets 404, and
the run stops the same way (exit 1).

A tuner that dies cannot reach into a window either. A new lock, and the
run's hold (`hold_id`), end no later than the next window's start, whatever
`ttl_secs` asked for; the answer's `hold` shows the hold after that cap. A
heartbeat (`PATCH`) keeps the same cap. The tuner takes its hold with
`"until_quiet_window": true` (`POST /v1/hosts/{host}/holds`), so the hold
ends by the next window's start from the moment it exists, even when the run
is cut off before it takes its lock. And
while a window is in force every tuning lock is set aside, so the window's
work reaches the lane even if a lock is still on record.

**Busy and paused models.** The tuner writes each candidate with `apply:
now` and the revision it edits in `If-Match` (a quoted ETag). When the
model still serves a request the gateway answers 200 with `apply.pending:
true` (`reason: model_busy`); the tuner then waits until `GET /v1/engines`
shows nothing pending for the model, and never measures before. Still
pending after 10 minutes: the run stops. A request answered 503
`model_paused` (an apply or unload holds the model) is asked again a few
seconds later, never counted as a failed answer.

**The ledger.** Every run of every experiment is a row in the gateway's
store (`GET /v1/optimize/experiments?model=TARGET`, `sb ai tune MODEL
--history`): model, engine build (the engine's `build_info`), model file
identity (a hash of its path, never the path), settings revision, lever,
value, metrics, samples, verdict and reason. A rejected change is skipped
next time unless the engine build or the model file changed, and the run
says so either way. The ledger keeps 180 days and at most 20,000 rows
(pruned at start-up and daily). Without a state store the ledger lives as long as the
process. The accepted winner is written as one settings revision with the
note `tune <run id>` (the history shows `put: tune tune_…`), so `sb ai
revert MODEL REVISION` undoes the whole run; the summary prints that
command.

An example session:

```
$ sb ai tune gpu-box/qwen --concurrency 1,2 --budget 45m
sb ai tune gpu-box/qwen: run tune_3f9a0c21d4, engine b12464, 6 samples per run, budget 45 min
  spec_type {"spec_type": "draft-mtp"}: +48%, a surprising win; repeating it once
  spec_type {"spec_type": "draft-mtp"}: kept (decode +48% over both controls; repeated: decode +44% over both controls)
  draft_max {"draft_max": 2}: kept (weighted +7% over both controls)
  flash_attn {"flash_attn": "on"}: rejected (decode 321.0 did not beat both controls (318.4, 324.9))
  ubatch_size {"ubatch_size": 1024, "batch_size": 2048}: rejected (prefill 9930.1 did not beat both controls (9871.2, 10044.0))
  cache_type {"cache_type_k": "q8_0", "cache_type_v": "q8_0"}: rejected (decode 170.2 did not beat both controls (322.0, 319.8))

metric                     baseline        now    ceiling  how measured
decode tok/s (c1, median)     218.0      323.1      410.0  median of 6, warm, fresh prompts, through the gateway (client clock)
time to first token ms        182.0      179.0          -  median of 6, warm, fresh prompts, through the gateway (client clock)
draft_max by concurrency (aggregate tok/s):
  {"draft_max": 1}: c1 290, c2 455
  {"draft_max": 2}: c1 318, c2 471
  {"draft_max": 3}: c1 322, c2 431
  {"draft_max": 4}: c1 309, c2 398
kept: spec_type {"spec_type": "draft-mtp"} (+44%); draft_max {"draft_max": 2} (+7%)
stopped: plateau: 3 levers in a row inside the noise
not verified: greedy equality on one fixed prompt only; needle checks at two depths, not a full long-context evaluation; quality beyond these checks (reasoning, tools) was not measured; numbers include the network and the gateway between this machine and the engine
undo with: sb ai revert gpu-box/qwen 12
```

`--dry-run` prints the plan (levers, candidate values, gates, what would be
skipped) and changes nothing. `--json` prints the summary as JSON. The
numbers are measured on the client's clock through the gateway, the way a
client sees them; the engine's own timings stay in the speed report.

### Speech to text and text to speech (`/v1/audio/*`)

The gateway carries OpenAI's three audio endpoints, so any OpenAI client
works against it:

- `POST /v1/audio/transcriptions` and `POST /v1/audio/translations`:
  `multipart/form-data` with `file` and `model` (both required), and
  `language`, `prompt`, `response_format` (`json` by default, `text`, `srt`,
  `vtt`, `verbose_json`, `diarized_json`), `temperature`,
  `timestamp_granularities[]` and `stream`. Other fields (`num_speakers`,
  `min_speakers`, `max_speakers`, ...) reach the engine unchanged.
- `POST /v1/audio/speech`: JSON (`model`, `input`, `voice`,
  `response_format`, `speed`, other fields unchanged); the answer is the
  audio bytes, streamed.

`model` resolves through routes and aliases like any model. Only targets
whose provider declares `capabilities: { audio_in: true }` (speech to text
and translation) or `capabilities: { audio_out: true }` (text to speech) are
chosen; any other target is rejected with the reason (`audio input required
but target does not support it`), and with no audio target left the request
is refused with 400. The gateway forwards the form to the target's
`{base_url}/audio/transcriptions` (or `/audio/translations`,
`/audio/speech`) with the provider's own auth, changing only `model` (to the
target's own model id), and gives back the engine's status, content type and
bytes: `text/plain` for `text`, `srt` and `vtt`, JSON otherwise, or the
engine's event stream for `stream=true`.

An upload larger than `server.audio_max_upload_bytes` (default 25 MiB, as
OpenAI; `max_request_bytes` does not apply here) is refused with 413 and the
error type `audio_too_large`, before any target is chosen. The file is read
once and shared by every attempt, so fallback to a second target sends the
same bytes without a second copy. Audio requests go through admission,
tenants, budgets, plugins, local lanes (wake, `max_in_flight` with spill or
queue, GPU groups, idle power-off), account and target fallback and traces,
like chat. They are never cached, and no audio, transcript or input text is
stored: traces and usage carry metadata only.

Each served request writes one usage record with `workload_kind`
`audio_transcription`, `audio_translation` or `audio_speech`, the target,
the latency and, when known, the amount: seconds of audio
(`pricing_unit: per_second`) from the answer's `duration`
(`verbose_json`, `diarized_json`) or a `usage` object of type `duration`,
and characters of input for speech (`per_character`). When the cost map
prices the target's model in that same unit (`pricing_unit: per_second` or
`per_character` with `unit_price_micros`), the record carries the cost and
budgets count it. A streamed transcript or speech is recorded when its last
byte has gone out, like a streamed chat answer: an engine that breaks off
mid-answer is a failed attempt traced with the error's status, a client that
hangs up is traced `client_aborted` (499), and neither writes usage.
`GET /v1/models` marks audio models with `"kind": "audio_transcription"` or
`"kind": "audio_speech"`; a combo gets a kind when every target of every
member has it.

A speech-to-text engine on an always-on machine: a provider that takes audio,
a lane with no wake or power-off command (the machine is never powered off)
and a limit of one transcription at a time, and two routes. `sb ai transcribe`
uses the route `transcribe`, and `sb ai transcribe --speakers` the route
`diarize`:

```yaml
providers:
  - id: speech
    type: openai_compatible
    base_url: "http://small-host.example.invalid:8300/v1"
    capabilities: { audio_in: true }
    accounts:
      - { id: local, auth: { kind: api_key, env: SPEECH_ENGINE_KEY } }

local_executors:
  - name: speech                 # the provider id
    host: small-host
    base_url: "http://small-host.example.invalid:8300"
    health_endpoint: "http://small-host.example.invalid:8300/health"
    route_health: true           # skip it while it is down
    max_in_flight: 1             # one long file at a time
    on_full: queue
    queue_timeout_secs: 600
    # no wake_command, no poweroff_command: the machine stays on

routes:
  - name: transcribe
    match: { model: transcribe }
    targets: ["speech/whisper-large-v3"]
  - name: diarize                # the same engine; it diarizes on diarized_json
    match: { model: diarize }
    targets: ["speech/whisper-large-v3"]
```

A second engine on `gpu-box` behind the first target of a route works as for
text: put it in `targets` after the first, and the request falls over to it
when the first fails or, with `on_full: spill`, while the first lane is full.
A text-to-speech engine is the same with `capabilities: { audio_out: true }`.

The `diarized_json` answer the CLI reads (other fields pass through):

```json
{"task": "transcribe", "language": "en", "duration": 70.0, "text": "hi there all good",
 "segments": [
   {"id": 0, "speaker": "SPEAKER_00", "start": 0.4, "end": 2.0, "text": "hi there"},
   {"id": 1, "speaker": "SPEAKER_01", "start": 63.2, "end": 65.0, "text": "all good"}
 ]}
```

`sb ai transcribe FILE --speakers` prints one line per turn (segments in a
row from the same speaker are one turn): `[00:00:00] SPEAKER_00: hi there`.
`duration` (or `"usage": {"type": "duration", "seconds": 70.0}`) is what the
usage record counts.

### Media jobs, video and artifacts

**Background jobs.** `POST /v1/jobs` starts a job and answers `202` at once
with its id and state (`accepted`):

```json
{"kind": "image_generation", "workflow": "comfy/txt2img",
 "inputs": {"prompt": "a lighthouse at dusk", "seed": 7, "size": "768x512"}}
```

`workflow` is an id from `GET /v1/workflows`; `model` instead resolves the
way `/v1/images/generations` resolves it (give one of the two). `inputs`
takes `prompt`, `seed`, `size`, and `image` (an artifact id), `frames` and
`fps` where the workflow binds them. Nothing else: an unknown field is
refused with 422, an input the workflow does not bind or a workflow of
another kind with 400, before the job exists. Follow the job with
`GET /v1/jobs/{id}` (state, events, artifact ids) or
`GET /v1/jobs/{id}/events`. `/v1/images/generations` runs the same job and
waits for it, as before.

A job on a managed lane holds the lane busy from its wait to its end,
exactly like a blocking request: no idle power-off, the GPU group rules
apply, and it counts in the host's work. While it waits for a wake or the
GPU group it is `queued` with a `waiting_for_lane` event that names the
lane, its state and its GPU group. `POST /v1/jobs/{id}/cancel` stops it:
the job is `cancelled` at once, a job still waiting is dropped, the lane's
slots and GPU group are released, and a prompt it already queued in ComfyUI
is removed from ComfyUI's queue (`POST /queue` with `{"delete": [id]}`)
and interrupted (`POST /interrupt` with `{"prompt_id": id}`), with the
provider's credential. A ComfyUI older than the targeted interrupt
interrupts whatever it runs. `server.jobs.max_pending_per_tenant` (default
16) bounds the background jobs one tenant has queued or running; one more
gets 429. Jobs are not resumed after a restart: on a disk artifact store a
job that had not finished is `failed` with error class `restart`.

**Video.** A ComfyUI workflow of `kind: video_generation` runs only as a
background job (`/v1/images/generations` refuses it). For image to video,
upload the first frame, then name it:

```sh
curl -s "$SB/v1/uploads" -H "authorization: Bearer $KEY" \
  -H 'content-type: image/png' --data-binary @frame.png
# {"artifact_id":"art_...","media_type":"image/png",...}
curl -s "$SB/v1/jobs" -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"kind":"video_generation","workflow":"comfy/img2vid",
       "inputs":{"prompt":"waves roll in","image":"art_...","frames":49,"fps":16}}'
```

Switchback uploads the frame to ComfyUI (`POST /upload/image`) with the
provider's credential and binds the name ComfyUI gives it at the
workflow's `image` binding. The video is read from ComfyUI's history
(`images` for core video nodes, `gifs` for common video node packs,
`videos`, `video`), stored as an artifact with its media type
(`video/mp4`, ...) and frame rate, and recorded in the ledger as
`video_generation`, one `per_video` unit per video. `timeout_secs` on a
workflow bounds one job, ComfyUI queue included (default 900); give a
video workflow more. A commented `img2vid` workflow is in
`config/switchback.example.yaml`.

**Workflows: image in, image out.** A ComfyUI workflow of
`kind: workflow_execution` is the shape for a job that takes an image (and
an optional text) and gives an image back — a segmentation, an inpainting, a
background removal. It runs on the media lane like any other media job
(GPU sharing, wake, queue) and its output becomes an artifact, exactly like
a generated image:

```sh
curl -s "$SB/v1/uploads" -H "authorization: Bearer $KEY" \
  -H 'content-type: image/png' --data-binary @photo.png
# {"artifact_id":"art_...","media_type":"image/png",...}
curl -s "$SB/v1/jobs" -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"kind":"workflow_execution","workflow":"segment/sam3",
       "inputs":{"image":"art_...","prompt":"the cat"}}'
```

The image goes to ComfyUI (`POST /upload/image`) and the name ComfyUI gives
it is bound at the workflow's `image` binding. The `prompt` is optional and
is bound at the workflow's `prompt` binding: with no text the graph keeps
its own default. A workflow job needs no prompt (an image job does: an
empty one is refused with 400). The output is read from ComfyUI's history
(`images`), stored as an artifact and recorded in the ledger as
`workflow_execution`.

A segmentation workflow looks like this. The node ids and the two node
types below are **placeholders**: the graph is whatever the operator
exports from ComfyUI, and the ids are whatever that export names.

```yaml
providers:
  - id: media
    type: comfyui
    base_url: "http://executor-1.example:8188"
    workflows:
      - id: sam3
        kind: workflow_execution
        model_id: segment/sam3
        timeout_secs: 300
        # The graph below is an EXAMPLE shape, not a real SAM 3 graph:
        # "ExampleSegmenter" stands for whatever node runs the model on
        # the machine. Replace the graph, the node ids and the bindings
        # with the ones your ComfyUI export has.
        graph:
          "10": { class_type: LoadImage, inputs: { image: "" } }
          "11": { class_type: CLIPTextEncode, inputs: { text: "" } }
          "12": { class_type: ExampleSegmenter, inputs: { image: ["10", 0], text: ["11", 0] } }
          "20": { class_type: SaveImage, inputs: { images: ["12", 0], filename_prefix: "mask" } }
        bindings:
          image:  { path: ["10", "inputs", "image"] }   # the uploaded file's name
          prompt: { path: ["11", "inputs", "text"] }    # the optional text
        output_node_ids: ["20"]                         # the node that saves the mask
```

The `path` of a binding is the place in the graph the value goes; the
parent nodes must exist in the graph already. `output_node_ids` names the
nodes whose output is the artifact; without it every output of the graph is
taken. `GET /v1/workflows` lists the workflow inputs it binds: `image` as a
required artifact, and `prompt` as optional text where the workflow binds
one. A workflow job is checked before any machine wakes: a workflow that
binds `image` needs one (400 without it), and a `prompt` the workflow does
not bind is refused (400) instead of failing later at binding.

**Public model ids.** A workflow's id for callers is `<provider id>/<workflow
id>` by default, so the provider's name shows in every caller's model name.
Two optional keys on a workflow give it a name by job instead:

```yaml
providers:
  - id: media
    type: comfyui
    base_url: "http://executor-1.example:8188"
    workflows:
      - id: flux
        kind: image_generation
        model_id: image/flux        # the id callers see and send
        aliases: [media/flux]       # other ids that still resolve
        graph: {...}
      - id: i2v
        kind: video_generation
        model_id: video/i2v
        graph: {...}
```

- `model_id`: the public id. It replaces `<provider>/<workflow>` everywhere a
  caller sees or sends a workflow id: `model` on `/v1/images/generations`,
  `workflow` or `model` on `POST /v1/jobs`, `GET /v1/workflows`, the job's
  `target` and route decision, the artifact's `provenance.model`, and the
  usage record's `model`. Once it is set, the automatic id no longer resolves
  unless it is listed in `aliases`. Any non-empty name without whitespace or
  an empty path part; slashes are allowed.
- `aliases`: other ids for the same workflow, accepted on every request path
  but never listed in `GET /v1/workflows` or `/v1/models`. A request through
  an alias runs and is recorded under the public id; the job keeps the name
  sent as `requested_model` (also in its route decision reasons and
  `accepted` event), and so does its usage record, so you can see when
  nobody sends an old name any more.

Config load refuses a name used twice: every `model_id` and alias must be
unique across all providers' workflows and differ from the automatic id of
any workflow without `model_id`, from every route `name` and `match.model`,
every combo, the built-in profiles (`auto`, `auto/...`), the mock image
(`mock-image`, `mock/image`) and any `<fal provider>/...` name. The error
names both owners. An alias equal to the workflow's own public id is refused
too. Credential accounts of a ComfyUI provider are still matched by the
provider's own `<provider>/<workflow>` name, whatever public id it has.

**Renaming a lane or model without breaking callers.** Text routes target
`<provider>/<model>`, and a `local_executors` lane is named after its
provider, so renaming a text lane is config alone: change the provider `id`,
the lane `name` and every route target together, and callers that send the
route's name (`match.model`) see no change. Rename the provider id
everywhere else it is named too (tenant `allowed_providers` and
`allowed_accounts`, `server.budget.per_provider_usd`, client profile
accounts, combos); load refuses a config that still names the old id. Usage
already recorded and the routing scorecard stay under the old provider id
and target. A workflow's id carries its provider's name, so rename a media
provider like this:

1. Rename the provider and its lane (`id: media`, `name: media`).
2. On each workflow set `model_id` to the name by job (`image/flux`).
3. List each old id under `aliases` (`aliases: [old-provider/flux]`), so
   callers that still send it keep working while they move.
4. Watch `requested_model` over days, not one look: `GET /v1/usage`
   (`recent`) and `GET /v1/usage/events` show only the latest records of
   all traffic. With `server.state_store`, every record is in its `usage`
   table: `SELECT requested_model, COUNT(*), MAX(created_at) FROM usage
   WHERE requested_model IS NOT NULL GROUP BY requested_model`. When an old
   name has not been sent for as long as your slowest client takes to call,
   remove it from `aliases`.

**Binding transforms.** A workflow binding can turn what a caller sends into
what the graph needs, so model-specific rules stay in the config instead of in
every client. All three are optional; a binding without them binds exactly as
before.

```yaml
bindings:
  aspect: { path: ["5", "inputs", "aspect_ratio"],
            values: { "16:9": "16:9 (Widescreen)", "1:1": "1:1 (Square)" } }
  frames: { path: ["13", "inputs", "length"], round_up: { step: 8, offset: 1 } }
  width:  { path: ["3", "inputs", "width"],  multiple_of: 32 }
  height: { path: ["3", "inputs", "height"], multiple_of: 32 }
```

- `values`: a map from a sent value to the graph's value. A value in the map
  is replaced. A value not in it passes through unchanged (a caller that
  already sends the full label keeps working), unless `values_strict: true`:
  then it is refused with 400 `` `aspect` must be one of: 16:9, 1:1 ``.
- `round_up: {step, offset}`: a whole number is raised to the smallest
  `k * step + offset` at least the value (`offset` defaults to 0). Step 17,
  offset 5: 48 becomes 56, and 56 stays 56.
- `multiple_of: N`: a whole number is rounded to the nearest multiple of `N`,
  at least `N`; useful on the `width` and `height` a `size` gives.

A rounding binding refuses anything but a whole number of 0 or more (a number,
or text such as `"48"`) with 400. Transforms are checked when the job or image
request arrives, so a refused value never makes a job. Each transformed input
is kept on the job's `running` event and on its artifacts' `provenance` as
`input.<name>.sent` and `input.<name>.used`, so a caller sees what was
rendered. Config validation refuses a zero `step` or `multiple_of`, both
roundings on one binding, and `values_strict` without `values`.

**Artifacts.** Generated media and uploads are kept on disk under the
runtime state directory (`<state_root>/artifacts`, see
`switchback paths --json`), named by their sha256, with one metadata file
each (the record and the tenant and project that may read it; never a
prompt or graph). Directories are `0700`, files `0600`. They survive a
restart. Retention runs at every write and at start-up: older than
`server.artifacts.max_age_secs` (default 7 days) goes, then the oldest go
until the total fits `server.artifacts.max_bytes` (default 2 GiB). An
artifact larger than the cap is refused. `GET /v1/artifacts/{id}` streams
it with its media type and length; one removed by retention answers 404
`artifact_evicted`. `storage: memory` keeps artifacts in process memory
(lost at restart, hard cap 256 MiB by default) for a deployment without a
writable state directory; a disk directory that cannot be used falls back
to it with a warning at start-up, and `GET /v1/workloads/capacity` shows
`artifacts.storage: process_memory` with the reason in
`artifacts.fallback`. The store claims its directory with a
`.switchback-artifacts` marker file and refuses (falls back to memory) a
directory that holds anything else, so its start-up clean-up never touches
another program's files. An upload (`POST /v1/uploads`) is streamed into
the store, capped by `server.artifacts.max_upload_bytes` (default 32 MiB,
413 above), and must be the PNG, JPEG or WebP image its `Content-Type`
names (415 otherwise). An artifact is served only with an image or video
media type from an allow-list, with `X-Content-Type-Options: nosniff`;
anything else is served as an `application/octet-stream` attachment.

Where `<state_root>` is: `$SWITCHBACK_RUNTIME_ROOT/state` when that variable
is set; else `$SWITCHBACK_ROOT/.switchback/state`; else
`$HOME/.switchback/state` when `$HOME/.switchback/manifest.json` exists;
else `.switchback/state` under the working directory. A system service
(for example under systemd with `ProtectSystem=strict`, a working directory
of `/` and a `StateDirectory=`) usually matches none of the first three,
and `/.switchback` is not writable, so artifacts would stay in memory.
Point the store at the service's state directory explicitly:

```yaml
server:
  artifacts:
    dir: /var/lib/switchback/artifacts   # inside the unit's StateDirectory
```

(or set `SWITCHBACK_RUNTIME_ROOT` for the unit, which also moves every
other runtime path), and check `artifacts.storage: disk` in
`GET /v1/workloads/capacity` after the restart.

**Who reads what.** A client key may create jobs and uploads and read and
cancel what its own tenant and project made: `GET /v1/jobs` lists its own
jobs; another tenant's job or artifact answers 404, not 403. Operator and
admin keys read every job.

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
