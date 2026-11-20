# Architecture

How Switchback is built, what each piece does, and — just as importantly — what
it is **not** (yet). This is the engineering deep-dive; the [`README.md`](README.md)
is the 30-second product view, and [`AGENTS.md`](AGENTS.md) is the contributor
contract (invariants, conventions, recipes). Read `AGENTS.md` before changing code.

---

## The one-paragraph model

Switchback is one Rust binary that receives every AI call (OpenAI- or
Anthropic-compatible HTTP), normalizes it into a **provider-agnostic canonical
IR**, routes it with an **explainable `RouteDecision` + two-level fallback**,
executes it against the chosen provider/account, and streams the response back in
the *client's own* wire format. The design thesis is **harden seams, don't pile on
providers**: adding an OpenAI-shaped provider is pure config, and adding a new
wire format is one codec — because the trait and the IR stay clean.

## Crate graph

Acyclic; `sb-core` (the provider-agnostic canonical IR) is the root everything
depends on.

```
sb-core        canonical IR + config + error taxonomy + catalog + RoutingPolicy
  ├ sb-adapter      ProviderAdapter trait, AdapterError, SSE helpers
  ├ sb-protocols    OpenAI <-> canonical hub; anthropic/gemini/responses; schema downleveler
  ├ sb-router       hard filters → ordered candidates → RouteDecision (cost/latency/policy)
  ├ sb-credentials  multi-account selection, availability locks, age vault, OAuth refresh
  ├ sb-compress     RTK fail-safe tool-result compression
  ├ sb-ledger       append-only usage/cost ledger (priced from the catalog)
  └ sb-trace        per-request TraceRecord (decision + attempts + cost) + OTel spans
       └ sb-adapters   ComposedAdapter(Codec × Signer × Transport); egress pool; latency tracker
            └ sb-runtime  execution runtime: immutable revisioned CompiledSnapshot + the
            │             Engine that owns the attempt state machine (route → resolve → retry →
            │             fallback → hedge → budget → trace); hot-swappable, HTTP-agnostic
                 └ sb-server   Axum app + handlers + SSE + CLI over Engine::execute → `switchback`

sb-store      StateStore trait + bundled-SQLite backend: config revisions + audit + durable usage
              (a dep of sb-runtime and sb-ledger; has no sb-* deps of its own)
sb-plugin     Plugin trait + trusted built-ins (model_blocklist / request_tag / egress_pin); a sb-runtime dep
```

### The two load-bearing boundaries

- **The runtime boundary.** `sb-runtime::Engine::execute(req)` owns request
  execution; `sb-server` is reduced to translating the client's wire format in
  and out and rendering the outcome. The runtime does **not** depend on `axum` —
  failures flow as a wire-agnostic `ExecError`. Each request pins **one**
  `Snapshot` for its lifetime, so a config publish never tears a request across
  revisions. The principle: *one binary can stay; one topology cannot* — the same
  crate could back a separate data-plane binary without rewriting execution.

- **The credential boundary.** `sb-router` picks the *target* (provider/model);
  `sb-credentials` picks the *account* + secret and tracks availability;
  `sb-adapters` *executes* with the lease it's handed. The **`sb-runtime` Engine
  is the only place these are composed** — routing, credential resolution,
  adapter execution, fallback, budgets, and tracing are joined there, behind one
  pinned snapshot. Adapters contain no account-selection logic; the router
  contains no credential logic; `sb-server` only translates HTTP/protocol in and
  out. This separation is the seam that makes a new provider cheap.

## Request lifecycle (the hot path)

```
HTTP in → sb-protocols (ingress: client JSON → AiRequest)
        → sb-router      (hard-filter → order candidates → RouteDecision; picks the TARGET)
        → sb-credentials (resolve(provider, model) → ACCOUNT + lease; skips locked accounts)
        → sb-adapters    (canonical → upstream wire, execute with the lease, upstream → AiStreamEvent)
        → sb-protocols   (egress: AiStreamEvent → client SSE / collected JSON)
        → HTTP out       (+ metadata-only log, x-switchback-route / -request-id / -revision headers)
```

Fallback is **two-level**: account-level (rotate accounts within a provider,
locking failed ones per-`(account, model)`) then target-level (across providers).
Fallback is only legal **before the first streamed byte** — once a byte is
committed to the client, a mid-stream error is surfaced, never silently retried.
Streaming is the one path: a non-streaming response is produced by *collecting*
the same `AiStreamEvent` stream.

## Design invariants

The rules the codebase must not let rot (enforced in review — see `AGENTS.md`):

- `sb-core` stays provider-agnostic: no provider wire shapes in the core.
- Every routed request emits a `RouteDecision`.
- No prompts, responses, or secrets are written to logs or traces.
- Fallback is legal only **before the first streamed byte**.
- A request executes against **one pinned snapshot revision**.
- Adapters never select accounts; the router never reads secrets.
- Streaming is the one path — non-stream responses are collected from it.
- YAML bootstraps; runtime publishes are atomic (revision check + swap under one lock).

## Failure semantics

| Failure | Behavior |
|---|---|
| Account auth failure before stream | lock the account, try the next account, then the next provider |
| Provider 5xx / timeout before stream | retry the target, then fall over per policy |
| Error **after** the first streamed byte | surfaced to the client; never silently retried |
| Per-provider failures accumulate | circuit breaker opens; the target is skipped until half-open |
| Budget / spend cap exceeded | `402` before upstream dispatch |
| Tenant concurrency exceeded | `429` before upstream dispatch |
| Global admission full | queued up to a timeout, then `503` (load shed) |
| Client disconnects mid-stream | upstream cancelled; traced as `client_aborted` (`499`) |

## Capability reference

### Wire formats & translation
OpenAI Chat Completions, OpenAI Responses, Anthropic Messages, Google
Gemini/Vertex, and AWS Bedrock (SigV4 + binary event-stream) — stream **and**
non-stream — translated through a single canonical IR and rendered back in the
client's format. The canonical IR is provider-agnostic — `sb-core` carries no
provider wire shapes — and is the **hub** of a hub-and-spoke translation: every
format translates `format ↔ canonical`, never `format ↔ other_format`. (Adapter
maturity is uneven — the OpenAI-compatible ingress is the most complete spoke
today — but translation always targets the neutral IR, never another format.)
Adding an OpenAI-shaped
provider (OpenRouter, Groq, Mistral, Together, DeepSeek, vLLM, …) is pure config;
a non-bearer one (`auth_scheme: { kind: header, name: x-api-key }`) is also config.
Every real provider rides one `ComposedAdapter(Codec × Signer × Transport)`
execute loop — only `mock` is bespoke. An `openai_compatible` provider can name
its local engine (`engine: ollama | llama_cpp | vllm`); the adapter's
`EngineChatCodec` then translates controls the engines spell differently (today
`reasoning_effort` → llama.cpp/vLLM `chat_template_kwargs.enable_thinking`) at
the edge, so the canonical request stays engine-neutral. `engine:` has no
effect on a provider with `wire: responses`, and a `/v1/responses` `reasoning`
object or an Anthropic `thinking` field is not translated yet.

### Routing
Every request emits a `RouteDecision` (selected target, ordered fallbacks,
rejected candidates with reasons). Hard capability filters (streaming / client
tools / server-tool protocol / vision source / JSON-schema / media flags /
reasoning summaries / context window) sourced from the catalog + per-`ApiKind` defaults
run first. Then ordering is **cost-, latency-, and policy-aware** (all
toggleable): cheapest healthy host by a blended price map, or fastest by observed
latency — **split into TTFT and total**, so interactive (streaming) requests rank
on first-byte time and others on overall latency — under a `max_price` ceiling
and `allow_free` / `allow_promo` / `allow_aggregator` lane gates. Price ceilings
and disallowed lanes are hard policy even when cost-aware ordering is off.
**Health-aware**: routing sees a non-secret account-pool view (usable-account
count + circuit state per target) and demotes targets whose only accounts are
locked below ones that can execute — the demotion is named in the decision and
visible at `GET /v1/health`. Routing is deterministic given a snapshot: no
ML/semantic routing in the hot path. A route can set `pin_order: true` to keep
its declared target order: the outcome scorecard then demotes nothing on that
route and no strategy reorders it (strategy `pinned_order`; the decision cites
any ignored demotion as `outcome/pinned target=…`). Use it for routes over
local engines, where a lane stopped on purpose must not lose its place. A
provider that is also a managed local lane (`local_executors` entry with the
same name, provider not media-only) can let routing read its health with
`route_health: true` on the lane. It is off by default: without it the lane
gets no routing probe and routing treats it as any other provider. An
opted-in lane is health-probed by the capacity activator every 10s; while its
last probe failed, its targets (text and embeddings) are rejected
before dispatch with an explicit reason and nothing is counted against them
(no scorecard sample, no account lock). A rejection also starts a background
re-probe (single-flight, at most once a second), so the evidence refreshes
under traffic without a network call on the request path. If rejecting them
would leave no candidate that can serve the request (after tenant, profile and
capability filters), none is rejected.

A lane can also opt into `wake_on_request: true` (with `route_health`), which
lets text and embeddings requests manage it the way media jobs do. Every
dispatch to the lane holds a busy guard in the activator's one in-flight count
(shared with media jobs) until the collected response is complete or the
stream ends (last event, upstream error or client disconnect); the idle
countdown, and so `poweroff_command`, waits for it. When the target a route
would choose first is on such a lane and routing is about to skip it as down,
the request starts the lane's wake (single-flight across requests and media
jobs, starting with a fresh probe) and waits up to `wake_wait_secs` before
its first byte, so fallback stays possible. The decision says what happened
(`local lane … woken for this request, healthy after 52 s`, or `… wake wait
of 90 s passed, wake still running, fell through`); when no other target can
serve, the answer is a retryable 503 (`local_lane_unavailable`, with
`Retry-After`). The lane view follows every config publish: lanes added,
changed or removed by a reload take effect without a restart, and a lane that
stays keeps its in-flight work and queue.
Wake-aware ordering (`prefer_awake` on a route or `server.prefer_awake`, off
by default) keeps small requests off a machine that would first need a wake.
The runtime stamps a target on a `wake_on_request` lane that is asleep,
waking or powering off with `ExecutionTarget.wake` (machine, state, measured
wake time) and decides per request, from its estimated size and its
`x-switchback-wake` header, a `RoutingPolicy.wake`. The router applies it
after the hard filters and the strategy's ordering, as a stable partition
inside each demote rank: targets that are up first, sleepers after them in
their old order (`never` drops sleepers instead). Sleepers are no longer
skipped as "known down" on such a route; they are fallbacks, and the dispatch
loop wakes one (bounded by `wake_wait_secs`) only when the request reaches
it. A full awake first choice never spills a small request onto a sleeper:
`take_lane_before_dispatch` queues it instead (as `on_full: queue`), and the
sleeper is reached only when the queue times out. A route's `prefer_awake`
merges over the server default field by field (`PreferAwakeOverride`). The
decision's one `prefer_awake` line (the runtime's account of a wake or a
queue replaces the router's) rides `x-switchback-route`.

Operator commands are bounded by the lane's `command_timeout_secs` (the wake
command by `boot_timeout_secs` when that is longer; on the activator's clock
seam; the shell runner kills a command dropped at its
timeout), so a hung command fails and frees the wake lock. A failed
power-off is retried after `wake_retry_interval_secs`, at most
`retry_budget` times, then the lane is `Degraded` and escalated. After its
own power-off a lane's healthy answer is not trusted until a probe has
failed once, a wake has run, or `boot_timeout_secs` has passed (a host that
still answers then did not power off; that is logged and reported). A request waiting for a wake keeps its
admission, tenant and idempotency slots (documented sizing, not released:
releasing the idempotency claim would let a retry double-dispatch, and
re-taking the admission slot would add a second queue and a second 503
path after the wake).

Lanes that name the same `host` share one `Host` in the activator: its wake
lock (one wake for the machine however many lanes ask), its power commands
(set on one lane, or identical) and its power-off decision. The machine is
idle only when no lane has work in flight or queued and every lane's own
idle countdown has passed; one power-off then takes every lane offline. A
lane without `host` has a `Host` of its own, so it behaves as before.
The `Host` also remembers when its wake command last succeeded in the
current power cycle (until our power-off, or a waking cycle that timed out):
a lane that must come up while that is recent, or while another lane of the
host answers a fresh probe, waits for its own engine (and its
`restart_command`) and never sends the wake to a running machine.

Lanes of one host may also share an `exclusive_group` (one GPU). The `Host`
keeps each group's holder, the holder's work count and at most one pending
switch. Text work waits for the group only for a route's first choice, in the
same pre-dispatch step as the wake (bounded by the lane's `group_wait_secs`,
before the first byte, explained on the `RouteDecision`); the guard taken then
is the one the dispatch uses. Any other dispatch (a fallback target, a hedge
racer) takes the group only if its lane already holds it and no switch is
pending, otherwise the target is skipped. A media job waits after its wake. A
switch waits until the holder has nothing in flight, runs the holder's
`release_command` once (every other lane's when the holder is unknown after
start-up or a power-off) and hands the group over; while it is pending new
holder work waits behind it. So a request waits at most `wake_wait_secs` plus
`group_wait_secs`, and a media job its wake plus `group_wait_secs`.
Work that bypasses Switchback is invisible to the idle timer, so a host may
carry a `poweroff_guard_command` (set once per host, like the power
commands). `Host::tick` asks it last, under the tick and wake locks and
before the re-check that marks the lanes powering off: exit 0 lets the
power-off run; any other exit (busy) or no answer (timeout, cannot run)
keeps the machine up and spaces the next question by
`wake_retry_interval_secs`. Only the guard's own runs change its state (busy
streak, failure streak), so new work never hides a broken guard.
A lane's health probe is a `HealthProbe::check` that may carry the
credential of the provider with the lane's id (`comfyui` only, leased from
the engine's live resolver through `LaneCredentials`, and only to a
`health_endpoint` on the provider's origin); for such a lane a 401/403 is
`ProbeAnswer::Refused`, which fails the job at once instead of starting a
wake. `scheme_auth` attaches that credential with the adapters'
`SchemeSigner`, for ComfyUI calls, the probe and the provider doctor.

A lane may set `max_in_flight` (with `queue_timeout_secs` and `on_full`).
Its places live in `sb-server::lane_slots::LaneSlots`, owned by the lane's
executor (kept across reloads): a counter and a FIFO queue of tickets on the
activator's clock seam. A ticket leaves the queue on drop (client gone, wait
over) and a `SlotPermit` gives its place back on drop, so error, panic
unwinding and task abort all free it. Every dispatch to such a lane holds a
permit inside its `LaneBusy` guard: the route's first choice takes it in the
pre-dispatch step (`Engine::take_lane_before_dispatch`, the same step and
bound order as the GPU group: place first, then group, so a queued request
never holds the GPU), any other dispatch only without waiting
(`LaneStatus::try_begin_request` refuses on a full lane and the target is
skipped). With `on_full: spill` the step first asks `LaneStatus::lane_full`;
when the lane is full and the plan without the lane's targets selects another
provider, those targets are skipped with the reason and nothing waits; else the
request queues. A wait that runs out is `LaneAcquire::QueueTimeout`: the
targets are skipped, and the plan's 503 when nothing else serves is typed
`local_lane_queue_timeout` with `Retry-After`. Media jobs take a place first
in `run_job_noted` (before wake and group) and fail with
`JobError::QueueTimeout`. Waiting is counted in the lane's `waiting`, which
the host's idle check already reads, so a queue keeps the machine on. The
capacity report shows the lane's `slots`. The hedge race runs before the
dispatch loop, so the first choice's held guard is lent to it
(`LaneGpuNote::lend_to_hedge`): that lane's racer uses it instead of asking
for a second place, and an unused guard comes back for the loop.

Each text attempt also measures its speed (`sb-ledger::RequestSpeed`): the
OpenAI codec turns a llama.cpp `timings` object into `Usage.engine_timings`
(non-streamed body) or an `AiStreamEvent::EngineTimings` (last stream chunk,
a separate event so no egress encoder emits it; the stream meter and the
collector fold it into the usage). `stream::attempt_speed` takes the attempt
start, the first-token time and token-event count from `watch_first_token`,
and the end; engine numbers win, gateway numbers fill the rest, each tagged
with its source. A gateway decode window needs two or more token events over
at least 50 ms (a body that came in one piece yields its token events
microseconds apart). The record carries it into the ledger, the JSONL line and
the store (migration 20: six nullable `usage` columns; the previous binary
names its columns in every statement, so it keeps working on a migrated
file). `GET /v1/targets/stats` folds the most recent usage records into
per-target medians (`sb_ledger::target_speed_stats`). Routing does not read
them in this slice.

Media work runs as jobs (`sb-server::workloads`). Every job, from
`POST /v1/jobs` (background, answered 202 at once) or
`/v1/images/generations` (which waits for it), is one task created by
`WorkloadStore::start`: the job exists as `accepted` before its task runs,
the task takes the managed lane with `run_job_noted` (wake, GPU group,
self-heal), so a background job holds its lane exactly as a blocking
request does, and progress is written to the job as events. A cancel marks
the job `cancelled` under the store lock and aborts the task, which drops
the lane's in-flight slot, queue slot and GPU permit; a ComfyUI prompt it
queued is removed from ComfyUI's queue and interrupted. Artifacts go to
`sb-server::artifact_store`: bodies on disk content addressed by sha256
(or in memory, capped), one metadata file per artifact with its owner
(tenant, project), job records beside them, and a size and age cap applied
at write and at start-up. A client key reads only what its own tenant and
project made. A video input image is uploaded once (`POST /v1/uploads`) as
an artifact and sent to ComfyUI's upload by the adapter.

### Multi-account auth
Account selection (fill-first / round-robin), per-`(account, model)` availability
locks with cooldowns, an **age-encrypted vault** (key in the OS keychain or
`SWITCHBACK_VAULT_KEY`), and mixed account auth: API keys, static or refreshable
OAuth bearer tokens, Vertex-style service-account minting, and Bedrock SigV4.
**Live OAuth refresh** de-duplicates concurrent refreshes so rotating refresh
tokens aren't revoked. `refresh_vault` persists rotated refresh tokens atomically
back into the encrypted vault; env/inline refresh tokens are followed in memory
only. All methods resolve to a redacting `CredentialLease`, so account selection,
lockout, and fallback work the same way across API-key/OAuth/service-account/SigV4
providers. `/v1/providers` surfaces only non-secret auth kinds/source labels for
operators.

### Resilience
Same-target retry, a per-provider **circuit breaker**, spend-cap budgets (global +
per-provider), and request **hedging** (non-streaming only). A failed attempt
locks its account per error class and records the breaker, so fallback never
re-picks a known-bad account.

### Observability
Metadata-only traces for routed requests (route decision + every attempt + egress
+ cost + `ttft_ms`, the time from the request's start to the first token of the
response served, on streamed and collected responses alike) at `GET /v1/traces`
(+ `/{id}`), session rollups at `GET /v1/sessions`,
route replay previews at `GET /v1/traces/{id}/route-preview`, an
`x-switchback-request-id` header, an append-only usage/cost ledger at
`GET /v1/usage`, `tracing` request/attempt spans, and optional **OpenTelemetry
OTLP export** (`otel` feature). Route replay previews re-run routing against the
current snapshot from trace metadata only (model, stream flag, tenant/project,
session id); they never reconstruct prompts because prompts/responses are not
stored. This is deliberately Langfuse-adjacent execution observability, not a
full prompt/eval/dataset product. Logs and traces are metadata only — never
prompts, responses, or secrets.

### Control plane
A redacted config API (`GET /v1/config`, `/v1/providers`), live runtime knobs
(`GET`/`PATCH /v1/runtime`), atomic config **hot-reload** (`POST /v1/reload`) with
per-request snapshot pinning (every response carries `x-switchback-revision`), a
machine-friendly CLI, and an embedded **dashboard** at `/` (no build step). The
**declarative `/cp/v1`** surface projects the config as k8s-style envelopes
(`apiVersion`/`kind`/`metadata`/`spec`) for providers/routes/tenants/egress/
plugins, with a **draft → validate → publish** lifecycle (atomic hot-swap,
`If-Match` optimistic concurrency enforced atomically with the swap),
`POST /cp/v1/route-preview` (the `RouteDecision` without executing), and
`GET /cp/v1/watch` (SSE revision stream). The API is authoritative; YAML stays
bootstrap.

### Model settings and engine control
Per-model settings are state, not config: a `ModelSettingsRegistry` on the
`sb-runtime` Engine (beside the ledger and the scorecard, so a publish keeps
it) holds one revisioned document per target model (`provider/model`):
request-time `defaults`, `profiles` and `locked` keys, load-time `load` and
`engine_args`, and the `request_load` policy. With a state store the
documents and their change history are durable (`model_settings`,
`model_settings_history`; a compare-and-set write in one transaction, so two
gateways sharing a store cannot both win). The types, validation and the
layering live in `sb-core` (`model_settings`, `load_settings`), engine-neutral.

On the request path the runtime takes the `switchback` body object and the
`x-switchback-profile` header out of the request first, so neither reaches
an upstream. After routing it builds a settings plan: for each candidate
target, the request that target's attempt sends, with defaults < route
profile < request profile < the request's own values layered and locked
keys forced. Targets with no settings are absent from the plan and their
attempts send the request unchanged. Typed keys map onto the canonical
request (typed fields or the OpenAI-shaped passthrough); the adapter's
`EngineChatCodec` translates the few an engine spells differently
(reasoning, `repeat_penalty` on vLLM). The plan also counts each attempt in
flight per engine model, held as long as the lane's busy guard, which is
what unload and apply-now wait for. The decision carries a `settings` line
(key names only) that rides `x-switchback-route` and `x-switchback-settings`.

The engine side lives in `sb-server::engine_control`: a lane with
`engine: {kind: llama_cpp_router, settings_command}` gets load, unload,
apply and a status view. Engine kinds plug in behind `EngineKindOps` (load
keys to engine arguments, raw argument checks) and the kind's HTTP calls;
llama.cpp's router is the first. The gateway never writes files on a
machine: it pipes the lane's managed overlay to `settings_command` and asks
an awake engine to re-read its presets. Reads never wake a machine; load and
apply-now go through the activator (`ensure_ready` only when asked to wake,
`acquire_for_request` for the lane's place and GPU group). Per-request load
settings reach it through the runtime's `ModelVariants` seam: the runtime
checks the policy, the engine control fit-checks, adds a `<model>~<hash8>`
variant to the overlay and applies it, and the attempt names the variant.

### Speed tricks ("optimize")

Three layers, after the method of
<https://github.com/sybil-solutions/inference-optimisation-skill>.
Layer 1 runs on the request path. `sb-core::optimize` holds the config
(`server.optimize`, `routes[].optimize`), the per-target settings fields
(`reasoning`, `reasoning_guard`, `ceiling`) and the pure helpers (the
conversation key, a hash that never holds prompt text; the reasoning-guard
edit). `sb-runtime::optimize::OptimizeState` lives on the Engine beside the
settings registry (state, not config): where each conversation was served
last and on which slot, what each engine lane has loaded (from the engine
control's last listing through the `EngineView` seam, and from what the
gateway served), and per-target evidence (cache hits, speculative
acceptance, swaps, empty answers after thinking, guard uses, queue waits).
After resolving candidates the runtime computes `Hints` once (which targets
are up, healthy and not full; their machine; loaded or not; measured prompt
speed; the sticky target) and stores them on the `CandidateResolution`;
`plan_candidates` applies them after the router with `apply_hints`, so the
wake, lane-place and real plans of one request agree. Only targets that are
up move forward, so wake-aware ordering is never crossed; `pin_order` is
kept. The reasoning guard is part of the settings plan (it edits the
request one target's attempts send). Attempts report back through an
`Observer` from the collect path and from the stream finalizer (a content
watcher tells an empty answer after thinking).

Layer 2 is read-only: `sb-server::optimize::build_report` is a pure
function over the usage ledger, the settings documents and their latest
revision time, the runtime's per-target evidence, the lane reports and the
host numbers (`server.optimize.hosts`); the handler gathers those without
calling a machine. Layer 3 lives in the CLI (`cli/ai_tune.py`, standard
library): it drives the gateway's own APIs (settings writes with `apply:
now` and a `note`, load/unload, chat requests carrying the lane's tuning
lock) and keeps its ledger in the store (`optimize_experiments`, migration
22, through `StateStore::optimize_experiment_*`; in memory without a
store). The tuning lock is runtime state on `OptimizeState`: after
resolving, candidates on a locked lane leave a request that does not carry
the lock's id (a typed 503 `lane_tuning` when none is left), for chat,
embeddings and audio alike; while a quiet window is in force, locks are set
aside. A new lock and the run's hold end no later than the next quiet
window's start. The lock lives in memory; the tuner's
heartbeat (`PATCH`) learns from a 404 that it is gone (a restart) and from
`quiet_window` that a quiet window began (zones evaluated with `jiff`), and
stops at its next safe point.

The optimizer sits on top of the model-settings safety layer and never
around it. The ordering pass runs before `admit_first_choice`, which plans
with the same hints, so the request is admitted on the model it will
actually reach first; a first choice at a closed gate leaves the plan as a
rejected target, whatever the hints said. Stickiness pins an engine slot
only when no other admitted request is on the model (the request's own
admission does not count). Embeddings and audio take the same admissions
and have no ordering tricks. The loaded-model view comes from the engine
control's listing, kept fresh by a poller (`GET /models` every 30 s with a
3 s timeout, only on lanes the lifecycle has healthy or idle-counting and
whose last probe did not see them down) and by a refresh after every load,
unload and apply.

### Durable state (opt-in)
Point `server.state_store` at a SQLite file and every published config revision +
a change **audit log** + every request's **usage** are persisted as metadata (no
prompts/responses), in **WAL mode** so readers don't block the writer. When a
store is configured, `/v1/usage` and budget checks read the live durable rollup.
Durable usage events are de-duplicated by `request_id` (first-writer-wins), so
replayed writes don't double-count. The same store coordinates idempotency
in-flight claims, global admission slots, and tenant concurrency slots across
processes sharing it; active request guards renew owner-scoped leases until
completion, and TTL knobs reclaim abandoned rows after a crash. `GET /v1/usage`
reports a `durability` block (`memory_only` / `durable` / `degraded` /
`post_commit_failed`); `GET /v1/usage/reconcile` compares served totals with
durable events and returns `ok` / `degraded` / `inconsistent` + a `billing_grade`
flag. Readable at `GET /v1/revisions`, `/v1/audit`, `/v1/usage/events`.

### Multi-tenancy + quotas
Map API keys to **tenants** (`api_keys:` → `tenants:`) via inline keys, `key_env`,
or `key_hash: sha256:<hex>`; usage is attributed per tenant. Tenants may restrict
`allowed_routes` / `allowed_providers` / `allowed_accounts`, and hard limits
reject before upstream dispatch — `budget_usd` → 402, `max_concurrency` → 429.
Tenant-scoped operator keys see only their allowed slice in the read APIs; global
drafts and publish/reload/runtime mutation stay admin-only.

### Admission control + backpressure
A global `server.max_concurrency` cap queues bursts (bounded wait,
`x-switchback-queue-ms`) and sheds with 503 past `admission_timeout_ms`.
`server.max_response_bytes` caps the non-streaming collect path; the streaming
path cancels the upstream when the client hangs up (traced as `client_aborted`).

### Plugins (two tiers)
Trusted trait-object built-ins (`plugins:` in config), compiled into the snapshot
and run on the hot path with panic isolation: `model_blocklist`, `request_tag`,
`egress_pin`. Hooks: `pre_route` / `post_route` / `select_egress` /
`post_attempt`; active chain at `GET /v1/plugins`. Plus optional **sandboxed
Wasm** plugins (`type: wasm`, build with `--features wasm`) in a Wasmtime sandbox
with per-call fuel + epoch-interrupted wall-clock timeout + `failure_mode:
open|closed` — the public, default-off extension story.

### Schema downlevel, pass-through, compression
Gemini/Vertex schema rewrites emit trace warnings (anyOf → best branch, const →
enum, `$ref` dropped, depth-bounded against adversarial nesting);
`server.strict_schema_downlevel: true` rejects high-lossiness downlevels before
dispatch. A model the gateway has never heard of is forwarded verbatim to a
default provider (flagged `unverified` in the decision). **RTK-style tool-result
compression** is opt-in and fail-safe (never grows, never empties).

## Scope & limits — what it is *not* (yet)

Switchback is a **single-binary, local-first / team gateway**. The seams are
deliberately shaped so it can grow toward hosted scale without a rewrite, but the
hosted machinery is intentionally **not** built. Be honest about these in any
deployment:

- **Single-host coordination, not a hosted cluster.** `sb-store` is bundled
  SQLite (WAL); the intended target is local and single-host/team deployments.
  Cross-process coordination (admission / tenant / idempotency slots, durable
  usage) on a *shared* SQLite file is possible only where filesystem locking
  semantics are known and tested — it is **not** the recommended cluster mode and
  **not** a hosted multi-node backend. Multi-node hosted deployments are out of
  scope; that path is **Postgres** (control + data plane) plus likely
  **Redis/etcd** for distributed admission and rate limits.
- **Usage is internal accounting, not billing infrastructure.** Durable usage +
  reconciliation give accurate internal cost attribution and a `billing_grade`
  honesty flag. They are **not** a billing system: no provider-invoice
  reconciliation, idempotent billing events, pricing-version snapshots, or
  external audit export. Use it for team accounting; don't resell it as billing.
- **Tenancy is gateway-level isolation, not hosted-grade multi-tenancy.** API-key
  → tenant scoping, route/provider/account restrictions, quotas, and per-tenant
  views are real. There is **no** org/project/user hierarchy, row-level store
  filtering, per-tenant secrets, admin delegation, or SSO/SAML/OIDC. It's a team
  gateway, not a multi-tenant SaaS.
- **Text, tools, reasoning summaries, and image-input IR; richer media remains
  bounded.** The canonical IR models text, client tool calls/results,
  provider-executed server tools, reasoning summaries, citations, image input,
  generated inline images, usage, and structured-output hints. Audio/video/file
  input, computer-use state, and provider-specific hosted-tool payloads are
  admitted only when a protocol edge and target capability explicitly support
  them; otherwise they fail loud or are gated out by routing.

  First-class image/video/workflow execution should follow the typed job,
  artifact, and workflow direction in
  [`MULTIMODAL_WORKLOAD_BRIEF.md`](MULTIMODAL_WORKLOAD_BRIEF.md)
  rather than dilute `AiRequest`.

Also out of v1 scope (seams only, not implementations): a hosted billing
marketplace, fine-grained resource permissions, DB-backed *live* config (YAML
stays the bootstrap source of truth), and learned/semantic routing.

**API stability.** Switchback is `v0.1.0`. Data-plane (ingress) compatibility is
the priority; control-plane schemas, config shape, and plugin APIs may change
before `v1.0`.

See [`AGENTS.md`](AGENTS.md) for the invariants you must not break and the recipes
for adding a provider, a wire protocol, or a plugin.
