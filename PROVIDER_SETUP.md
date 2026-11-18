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
to it with a warning.

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
