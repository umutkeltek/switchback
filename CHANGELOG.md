# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
`1.0`.

## [Unreleased]

### Added

- **Speed tricks, layer 2: the speed report** (`GET /v1/optimize/report`,
  `sb ai doctor [MODEL] [--json]`): per local target, measured decode,
  prompt speed and time to first token against its ceiling, swaps, cache
  hits, speculative acceptance, empty answers after thinking, queue waits,
  wakes and last use, plus findings with evidence and one action each
  (decode below 40% of the ceiling, swap thrash, cold cache on repeated
  prompts, low acceptance, empty answers after thinking, unused for 14 days,
  a regression after a settings revision). Never wakes a machine or calls an
  engine; client keys get a tenant-scoped view. Ceilings live in a target's
  settings (`ceiling`); `sb ai fit REPO --store TARGET [--quant Q]
  [--bandwidth GBS]` writes bytes per token (and the decode ceiling);
  `server.optimize.hosts` holds machine bandwidth, else the report
  calibrates it from the best measured target and says so.
- **Speed tricks, layer 3: `sb ai tune`**: the skill's hill-climbing loop on
  one local model through the gateway (baseline, ceiling, one change at a
  time, control/candidate/control with at least 6 samples each, keep only
  what beats both controls with no other metric more than 3% worse than the
  controls' mean (than the worse control, when the two differ by more than
  3% on that metric), repeat a win over 20% once (the repeat must clear the
  5% noise) and a run whose controls differ by more than 3% on the judged
  metric (noisy),
  greedy/needle/soak gates, plateau after three flat
  levers). The lever catalogue for `llama_cpp_router` is data. Safety:
  refuses a sleeping machine without `--wake`, a busy lane, a quiet window
  (`server.optimize.quiet_windows`) and a locked load key (`locked:
  ["load.KEY"]`); holds the machine and takes the lane's tuning lock
  (`/v1/optimize/locks`, header `x-switchback-tune-lock`; other requests go
  elsewhere); Ctrl-C, SIGTERM, SIGHUP and the budget end restore the best settings without
  cutting a request off. Every run is a ledger row in the store
  (`/v1/optimize/experiments`, `sb ai tune --history`; SQLite migration
  22); a rejected change is skipped until the engine build or model file
  changes. The winner is one settings revision noted `tune <run id>`
  (settings writes take an optional `note`), so `sb ai revert` undoes it.
  `GET /v1/engines` now shows each lane's engine build (`GET /props`
  `build_info`), each model file's identity (a hash, never the path), its
  sidecar file names, and `speculative` (spec type and whether MTP or a
  draft model is known, from the preset/overlay keys or a loaded
  instance's `status.args`; a real router lists `sidecars` only for cached
  Hugging Face models). Quiet windows take an IANA time zone
  (`{start, end, tz}`, or `server.optimize.timezone` for plain strings;
  plain strings stay UTC) and are checked during a run too: the lock
  heartbeat (`PATCH /v1/optimize/locks/{id}`) names a window that began,
  and the run stops at its next safe point, restores the best settings and
  lets the lock and hold go; a lock lost to a restart (404) stops it the
  same way. The tuner follows the settings safety shapes: writes carry a
  quoted `If-Match`, a held-back apply (`apply.pending: true`) is waited
  for and never measured as applied, and a 503 `model_paused` is retried.
  The speed report labels its decode figure "all requests, median"; the
  tuner's own decode row is measured at concurrency 1 and says so.
- **Loaded-model freshness**: the engine control polls `GET /models` every
  30 s (3 s timeout) on engine lanes that are up (never off, waking,
  powering off or draining ones; never a wake) and refreshes the listing
  after every load, unload and apply it does.
- **Speed tricks for local engines, layer 1** (`server.optimize`,
  `routes[].optimize`, all off by default; method from
  <https://github.com/sybil-solutions/inference-optimisation-skill>):
  conversation stickiness (`x-switchback-session`, or the `user` field plus
  a hash of the opening) keeps a conversation on the machine that served
  it while that machine is up, healthy and has room, and names its engine
  slot (`id_slot`) when the target runs `parallel > 1`; loaded-model
  preference puts the target whose model is loaded first among targets on
  one awake machine (`engine.models_max` tells the gateway the router's
  limit); big-prompt placement (`big_prompt_tokens`) sends a big prompt to
  the awake target with the best measured prompt speed; the reasoning guard
  (`reasoning: true` and `reasoning_guard` in a target's settings) turns
  thinking off for a request with a small `max_tokens` unless the request
  asked for thinking. None of them wakes a machine or makes a request wait,
  and a pinned route keeps its order. `x-switchback-optimize` (and
  `optimize=[...]` on `x-switchback-route`) says what they did. Cached
  prompt tokens are recorded on every usage record (llama.cpp
  `timings.cache_n`/`tokens_cached`, OpenAI
  `usage.prompt_tokens_details.cached_tokens`), and per target the gateway
  counts cache hits, speculative acceptance (`draft_n`,
  `draft_n_accepted`), model swaps a request caused (per hour, with the
  model replaced), empty answers after thinking, reasoning-guard uses and
  lane queue waits. `sb ai set` writes `reasoning=true`,
  `reasoning_guard.*` and `ceiling.*`, and `--load --lock` locks a load key.
- **Model settings** (`/v1/models/{model}/settings`, `sb ai settings|set|unset|revert|chat`):
  every setting of a local model through one API. Per target model
  (`provider/model`): request-time `defaults` (temperature, top_p, top_k,
  min_p, typical_p, repeat penalties, presence/frequency penalties, seed,
  max_tokens, stop, system prompt, response format, normalized `reasoning`,
  and `extra` engine keys), named `profiles`, and `locked` keys.
  Precedence: the request's own value (unless locked) > request profile
  (`x-switchback-profile` or `"switchback": {"profile"}`) > route
  `settings_profile` > defaults > the engine's default; a key nobody set is
  never sent. `"switchback": {"settings": {...}}` sets keys for one request
  and never reaches an upstream. Revisioned and durable in the state store
  with history (who, when, which keys) and revert; a stale revision is a 409.
  `x-switchback-settings` says which profile served and what a lock
  overrode. Client keys read only their tenant's targets, never load
  settings or engine arguments.
- **Engine control** for llama.cpp in router mode (`local_executors[].engine:
  {kind: llama_cpp_router, settings_command}`): typed load-time settings
  (ctx_size, n_gpu_layers, KV cache types, flash_attn, batch sizes, parallel,
  threads, mlock/mmap, RoPE/YaRN, speculative decoding, mmproj, sleep, fit,
  reasoning budget, sampling defaults) and checked raw `engine_args`, applied
  through the operator's `settings_command` (reference:
  `contrib/llamacpp-preset-overlay.py`) and a preset reload, on the next load
  or now (once nothing is in flight). `POST /v1/models/{model}/load|unload`,
  `GET /v1/engines`, `sb ai load|unload|engines`, `sb ai set --load`, and a
  hold that names a model to load after its wake. Nothing wakes a machine
  unless asked (`wake: true`); unload never interrupts a request.
- **Load settings per request** (`"switchback": {"load": {"ctx_size": 65536}}`,
  `sb ai chat --load`): within the target's `request_load` policy (allowed
  keys for client keys, bounds, `max_variants`, `variant_idle_ttl_secs`), the
  gateway serves the request from a variant `<model>~<hash8>`, after a fit
  check against the router's memory measurement (or the policy's estimate);
  409 `would_not_fit` with the numbers otherwise.
- **Wake-aware routing** (`prefer_awake` on a route or `server.prefer_awake`:
  `enabled`, `wake_for_prompt_tokens`, `wake_for_output_tokens`; off by
  default): a small request goes to a target whose machine is up, and a
  target on a sleeping `wake_on_request` lane stays a fallback, woken only if
  the request reaches it. Bigger requests keep the route's order. Request
  header `x-switchback-wake: always | never | auto`; `never` with only
  sleepers left answers 503 `local_lane_asleep`. The decision line
  (`prefer_awake=[gpu-box asleep (wake ~65s), chose small-host/small]`) rides
  `x-switchback-route`. Each machine's wake time is measured and shown in
  `GET /v1/workloads/capacity` (`wake_duration_ms`, `wake_duration_ewma_ms`)
  and `GET /v1/targets/stats` (`wake`; a tenant key sees only machines behind
  lanes it can reach). On a `latency_aware` route a long-output request ranks
  awake targets on measured decode tokens/s. A route's `prefer_awake` merges
  over the server default field by field. A full awake lane makes a small
  request queue instead of spilling onto a machine that needs a wake; it
  falls back to the sleeper only after `queue_timeout_secs`. Embeddings are
  sized from their `input` and read `x-switchback-wake` too. Each image counts
  as 1600 prompt tokens toward `wake_for_prompt_tokens`.
- **A limit and queue per local lane** (`max_in_flight`, `queue_timeout_secs`,
  `on_full: spill | queue` on a `local_executors` entry): work past the limit
  waits in a FIFO queue or, by default, spills to the route's next target; a
  wait past the timeout is a retryable 503 `local_lane_queue_timeout`. Counts
  show as `slots` in `GET /v1/workloads/capacity`. Works with GPU groups,
  holds, wake on request and idle power-off; a queued request keeps the
  machine on.
- **Measured speed per request and per target**: usage records carry
  `ttft_ms`, `prompt_tps` and `decode_tps`, each with its source (`engine`
  from a llama.cpp server's `timings`, else `gateway` clocks); store
  migration 20 adds them (rollback-safe). `GET /v1/targets/stats` shows
  per-target p50 decode and prompt speed, p50 time to first token, count and
  last seen; a client key sees only its own tenant's records on targets it
  can reach. Routing does not use them yet.

- **Public model ids for workflows** (`model_id`, `aliases` on a ComfyUI
  workflow): callers see and send a name by job (`image/flux`, `video/i2v`)
  instead of `<provider>/<workflow>`, in requests, `GET /v1/workflows`, job
  and artifact records and usage. Aliases (an old id, while callers move)
  resolve on every request path, are never listed, and are recorded under the
  public id with the sent name kept as `requested_model`. Load refuses a
  name used twice (across workflows, routes, combos) naming both owners.
  Without the keys nothing changes. See "Renaming a lane or model without
  breaking callers" in `PROVIDER_SETUP.md`.
- **`sb ai`**: images, video, machine status, models and holds through a
  Switchback gateway (`sb ai image|video|status|models|hold|release`). Config
  from `SWITCHBACK_AI_URL` / `SWITCHBACK_AI_KEY` or
  `${XDG_CONFIG_HOME:-~/.config}/switchback/ai.env`; the key goes only into
  the `Authorization` header; `status` never wakes a machine; Ctrl-C cancels a
  video job. See `cli/README.md`.
- **Local-lane alerts** (`server.alert_command`, `alert_after_secs`,
  `alert_repeat_secs`, `alert_timeout_secs`): a lane in `warn` or `fail` for
  180 s, an escalation, a power-off whose retries ran out, a power-off guard
  that stays broken and a wake that never came up each run an operator
  program (argv, no shell) with one JSON event on stdin (schema
  `switchback/lane-alert@1`) and a summary in `SWITCHBACK_ALERT_SUMMARY`;
  one `recovered` event follows, and a lasting problem repeats every 6 h.
  Delivery runs in its own task and never delays lane work, a wake or a
  power-off; failures stay pending and are retried with a growing gap. The
  detail never holds a key, a URL credential or a command line.
  `POST /v1/admin/alerts/test` (admin keys) proves the path.
- **Host holds** (`/v1/hosts/{host}/holds`, `server.max_hold_secs`): an agent
  keeps a managed machine on for a bounded time (create, list, extend with
  `PATCH`, delete); the idle power-off waits, and the countdown runs from the
  last hold's end. `wake: true` wakes the machine in the background (never
  against a shared machine that answers). Eight active holds per tenant; a
  client key sees only its own. Kept in `<state_root>/holds.json` across a
  restart.
- **Adopting a machine woken outside Switchback**
  (`local_executors[].adopt_external_wake`): such a machine counts down to
  idle and powers off like one Switchback woke, after the power-off guard (a
  guard is required: the config refuses adoption without one).
  Off by default.
- **Capacity report**: `poweroff_in_secs` and the machine's holds per lane;
  a client key reads a read-only view (it got 403).
- **Workflow binding transforms** (`values`, `values_strict`, `round_up`,
  `multiple_of`): map and round what a caller sends; what was sent and what
  was used are kept on the job's `running` event and the artifacts'
  provenance.

- **Background jobs** (`POST /v1/jobs`, `server.jobs.max_pending_per_tenant`):
  a job request names a workload kind, a workflow from `/v1/workflows` (or a
  model that resolves as on `/v1/images/generations`) and typed inputs
  (`prompt`, `seed`, `size`, and `image`, `frames`, `fps` where the workflow
  binds them); it answers 202 at once with the job id and state. Unknown
  fields get 422; an unbound input, an unknown workflow or one of another
  kind get 400 before the job exists. A job on a managed lane holds the
  lane busy from its wait to its end like a blocking request, and a wait
  for a wake or the GPU group shows as state `queued` with a
  `waiting_for_lane` event. `POST /v1/jobs/{id}/cancel` stops a running
  ComfyUI job for real (removed from ComfyUI's queue and interrupted, with
  the provider's credential) and drops a waiting one; the lane's slots and
  GPU group are released at once. At most 16 background jobs per tenant are
  queued or running (`server.jobs.max_pending_per_tenant`); one more gets
  429 `rate_limit_error`. On a disk artifact store, a job that had not
  finished at a restart is `failed` with error class `restart`; jobs are
  not resumed.
- **ComfyUI video** (`kind: video_generation`, `timeout_secs`,
  `POST /v1/uploads`): a video workflow runs as a background job. An input
  image is uploaded once to `POST /v1/uploads` (the body is the image) and
  named by its artifact id; Switchback sends it to ComfyUI's upload with the
  provider's credential and binds the name ComfyUI returns. Video outputs are
  read from the history (`images` with `animated`, `gifs`, `videos`,
  `video`), stored with their media type and frame rate, and recorded as
  `video_generation` with `per_video` units. `timeout_secs` on a ComfyUI
  workflow bounds one job (default 900 s, as before).
- **Artifact store settings** (`server.artifacts`: `storage`, `dir`,
  `max_bytes`, `max_age_secs`): see Fixed.
- **Local engine lanes for text** (`engine:`, `pin_order`, `route_health`): one
  request shape for every local engine, and routing that understands an engine
  that is switched off on purpose. All three are off unless set.
  - `engine: generic | ollama | llama_cpp | vllm` on an `openai_compatible`
    provider. For `llama_cpp` and `vllm`, a Chat Completions `reasoning_effort`
    becomes `chat_template_kwargs` (`none` turns thinking off with
    `enable_thinking: false`); a `chat_template_kwargs` key the caller sent always
    wins. `generic` (the default) and `ollama` forward the request unchanged.
  - `pin_order: true` on a route keeps the declared target order. Outcome
    demotion and cost/latency ordering are off for that route, and the
    `RouteDecision` reports `strategy: pinned_order` with the reason.
  - `route_health: true` on a `local_executors` lane. The lane is health-probed in
    the background, and its targets are skipped before dispatch while it is known
    down, with a reason in the `RouteDecision` and no failure counted. A lane that
    is the only target able to serve is kept.
- **Local lanes that wake on request** (`wake_on_request`, `wake_wait_secs`,
  trace `ttft_ms`): text and embeddings requests can wake a powered-off
  managed lane and keep it awake while they run, and every trace records its
  time to first token. Off unless set.
  - `wake_on_request: true` on a `local_executors` lane (needs
    `route_health: true`). A request in flight on the lane, to its last
    streamed byte or the client's disconnect, holds the lane busy in one
    count shared with media jobs: no idle poweroff while it runs, and the idle
    timer starts when the last one ends.
  - When routing would skip the lane as down and its target would be chosen
    first, the request starts one shared wake (a fresh probe first, so a lane
    that is already back is used at once) and waits up to `wake_wait_secs`
    (default 90) before its first byte. It is then served by the lane, or falls
    through with the reason in `RouteDecision`, or gets a 503
    `local_lane_unavailable` with `Retry-After` when nothing else can serve.
  - Managed lanes follow config reloads: lanes added, changed or removed take
    effect without a restart, and a lane that stays keeps its in-flight work.
  - Trace records carry an optional `ttft_ms`, from the request's start to the
    first token, on streamed and collected responses.
- **Managed lane hardening** (`command_timeout_secs`): every operator command
  (wake, power-off, restart) is bounded (default 60 s; the wake command by
  `boot_timeout_secs` when that is longer) and a timed-out command
  counts as failed and frees the wake lock; a failed power-off is retried
  after `wake_retry_interval_secs`, at most `retry_budget` times, then the
  lane is `degraded` with `poweroff_failures` in the capacity report and an
  error in the log; after its own power-off a lane is up again only after a
  failed probe, a wake, or `boot_timeout_secs` (then with a warning that the
  power-off did not take effect); `wake_on_request` without a `wake_command` is now a
  config error.
- **Lanes share a machine** (`host:` on `local_executors`): lanes that name
  the same host share its power. One wake for the machine however many lanes
  ask; the machine stays on while any lane has work in flight or queued; one
  power-off after the last lane has been idle past its timeout, and every
  lane goes offline with it. Power commands are set on one lane of the host
  (or identical); `GET /v1/workloads/capacity` shows each lane's `host`. Off
  unless set.
- **One GPU, several engines** (`exclusive_group`, `release_command`,
  `group_wait_secs` on `local_executors`): lanes of one host that share a GPU
  take turns. Work for the holder runs at once; work for another lane waits
  (text: the route's first choice only, before the first byte; media: after
  its wake) until the holder has nothing in flight, then the holder's
  `release_command` runs once and the waiting lane takes the GPU. A pending
  switch holds new holder work back, so neither side starves. The
  `RouteDecision` (or the job's decision) says what happened; a wait that runs
  out falls through or answers a retryable 503 (`Retry-After: 10`); a failed
  release does not hand the GPU over and counts against `retry_budget` (reset
  by the machine's next power-off). The capacity report shows `gpu_group` and
  `holds_gpu`. Off unless set.
- **A power-off guard for work Switchback cannot see**
  (`poweroff_guard_command` on `local_executors`): a shell command asked
  right before each idle power-off, under the same locks and re-checks.
  Exit 0 lets the power-off run; any other exit means busy: no power-off,
  the machine stays up, the capacity report says `power-off postponed: guard
  reports the machine busy` with `poweroff_guard_busy` (a count) and
  `poweroff_guard_busy_since_ms`, and the guard is asked again after
  `wake_retry_interval_secs`. Busy is not an error (level `ok`). A guard that
  times out or cannot run never counts as free; after `retry_budget` such
  runs in a row the lane reports level `warn` with `poweroff_guard_failures`
  and says the guard is broken, and new work does not clear it; a reload
  that changes or removes the guard drops its old answers. Exit 126 and 127
  are the shell's own "could not run" and never an answer. Needs a
  `poweroff_command`; with `host`, set once per host or identical. Off unless
  set.
- **ComfyUI behind a token proxy** (`api_key_env`, `api_key`, `auth_scheme`
  on a `comfyui` provider, as for `openai_compatible`): the credential is
  leased from the credential resolver and sent on every ComfyUI call
  (prompt, history, artifact download, the provider doctor's
  `/system_stats`) and on the health probe of the managed lane with the
  provider's id, only when its `health_endpoint` shares the provider's
  `base_url` origin. A 401 or 403 reads as a refused credential: the job
  fails at once (a managed lane: `health endpoint answered 401`, level
  `fail`, no wake and no boot-timeout wait). A call that carries it follows
  no redirect. The secret never appears in a log, an error text, a failed
  job or the capacity report. Off unless set.

- **Lane doctor capability coverage** (`sb lane doctor` + `sb provider doctor`):
  the doctor surface now honors `RouteRequire`. `sb lane doctor` enumerates
  every route and combo in the config (was previously hardcoded to 5 stable
  lanes) and emits `capability_requirements` + `capability_coverage` per row;
  a route whose non-null `require` fields have zero satisfying targets flips
  to `state: red`. `sb provider doctor` runs capability-pressure probes
  (vision / tools / streaming / json_schema) against every capability-bearing
  route via `engine.preview_route`, surfacing the same class of bug the
  2026-07-28 `wpcom/gpt-5.6-sol` vision-blind-fallback hard-fail exposed
  (work_ef79811f300b). Related-but-distinct: `work_3316a5f7ccaa` (Compound-side
  model-capability claim mutation cross-system bridge) is the future slice.

- **Execution runtime** (`sb-runtime`): the TARGET×ACCOUNT attempt state machine
  extracted out of the HTTP edge into `Engine::execute`, over an immutable
  revisioned `CompiledSnapshot` hot-swapped atomically (per-request snapshot
  pinning; `POST /v1/reload`; `x-switchback-revision` header).
- **Durable state** (`sb-store`, bundled SQLite, opt-in `server.state_store`):
  config revisions + audit log + per-request usage events + idempotency replay +
  control-plane drafts. `/v1/usage` survives restarts.
- **Idempotency**: `Idempotency-Key` → durable full-replay (non-streaming) +
  per-process single-flight (409 on concurrent duplicate; 422 on key reuse with a
  different body).
- **Health-aware routing**: non-secret account-pool view demotes targets with no
  healthy accounts; latency split into TTFT vs total (interactive ranks on TTFT).
- **Multi-tenancy + quotas**: `tenants:` + `api_keys:`; per-tenant `budget_usd`
  (402) and `max_concurrency` (429) enforced before dispatch; usage attributed
  per tenant; `GET /v1/tenants`.
- **Adapter seam** `Codec × Signer × Transport`: SigV4 signing + AWS event-stream
  transport, so **AWS Bedrock** rides the one composed loop.
- **Admission control + backpressure**: global `server.max_concurrency` (queue +
  503 shed, `x-switchback-queue-ms`), `server.max_response_bytes` collect cap,
  `client_aborted` traces on disconnect.
- **Plugins**: tier-1 trait-object built-ins (`model_blocklist`, `request_tag`,
  `egress_pin`) on the hot path; tier-2 sandboxed **Wasm** plugins behind the
  off-by-default `wasm` feature. `GET /v1/plugins`.
- **Declarative control plane** `/cp/v1`: k8s-style resource envelopes,
  draft → validate → publish (atomic hot-swap, `If-Match`), `route-preview`,
  `admission-preview`, and a `watch` SSE stream.
- **Endpoint auth middleware**: when a key is configured, all `/v1/*` and
  `/cp/v1/*` endpoints require it (not just inference).
- **CI** (`.github/workflows/ci.yml`): fmt, clippy (`-D warnings`), test, release
  build, optional-feature job, and `cargo audit`.
- **Distribution**: `Dockerfile` (multi-stage, non-root) + release workflow
  (cross-platform binaries with checksums + multi-arch GHCR image on `v*` tags),
  `config/quickstart.yaml` (zero-setup mock config), and community-health files
  (`CONTRIBUTING`, `SECURITY`, `CODE_OF_CONDUCT`, issue/PR templates, `deny.toml`).

### Changed

- **Cached prompt tokens are counted once.** For OpenAI chat, Responses,
  Gemini and llama.cpp upstreams, `input_tokens` in usage and ledger rows
  now excludes the cached prompt tokens, which `cached_input_tokens`
  carries (the Anthropic convention the cost code already assumed). Rows
  written before this change counted the cached tokens in both, so they
  overstate input tokens and cost. The prompt speed the gateway measures
  itself (prompt tokens over time to first token) now counts only the
  uncached tokens. Clients still see the whole prompt in their own wire
  format: `prompt_tokens` (OpenAI chat), `input_tokens` (Responses) and
  `promptTokenCount` (Gemini) include the cached part, and an Anthropic
  client gets it as `cache_read_input_tokens` when there is one.
- `/v1/images/generations` runs as a job task and waits for it: the answer
  is the same, the job is listed (`accepted`, then its progress) while it
  runs, it can be cancelled by id, and a client that disconnects no longer
  abandons the job (it finishes and keeps its artifact). A video workflow
  is refused there (400). A ComfyUI job's events are now `accepted`,
  `waiting_for_lane` (managed lanes), `running` (with ComfyUI's prompt id;
  was `queued`), `history_polled`, `artifact_ready`, `succeeded`.
- A client key may cancel the jobs its own tenant and project made
  (`POST /v1/jobs/{id}/cancel`); another's answers 404.
- A managed lane's wait for a wake is counted in `queue_depth` by a slot
  released on drop: a job cancelled or a client gone mid-wait no longer
  stays counted (and no longer keeps the machine from powering off).
- The job list keeps at most 10 000 jobs (oldest finished first), and at
  most 256 failed ones.

- Managed lanes, rehearsal leftovers: after a failed `release_command` the
  next run waits `wake_retry_interval_secs` (a burst of requests used to
  spend the lane's whole `retry_budget` within seconds); a ComfyUI job that
  fails after it was accepted is kept as a `failed` job in `/v1/jobs`
  (metadata only, the newest 256; it used to be missing); a clean power-off clears a lane's
  `last_error` from the last power cycle unless the lane is escalated (the
  next job's start already cleared it); the GPU switch log line names the
  lane that gives the GPU up as `lane` and the one that takes it as `taker`
  (it named the taker as `lane`).
- Thinking from OpenAI-compatible engines is no longer dropped (bug fix, on
  by default). The adapter reads `reasoning_content` (llama.cpp, vLLM) or
  `reasoning` from stream deltas and from a collected response into the
  canonical thinking event, and `/v1/chat/completions` renders it as
  `reasoning_content` on stream deltas and on the collected message, only
  when the upstream produced any. `/v1/messages` and `/v1/responses` drop
  it by default as before (their clients send it back next turn, and that
  input would fail with 400); `server.openai_compatible_thinking_on_all_formats:
  true` renders it there as thinking blocks and a reasoning item. Before, a stream that only thought showed the client nothing
  (23 s in a rehearsal) and its trace had no `ttft_ms`; the first thinking
  token now counts. A response without thinking is byte-for-byte unchanged.

- ComfyUI image jobs wait up to 15 minutes for ComfyUI to finish (was about
  30 s, so a cold model load after a wake or a slow workflow failed with
  502 while ComfyUI kept rendering, and the lane counted as idle). A job
  that ComfyUI reports as failed ends the request at once, naming the
  failing node and exception type.
- Lanes of a shared `host` no longer send the wake command to a machine
  that is already up. A lane that has to come up while another lane of the
  host answers, or while the host's wake ran less than `boot_timeout_secs`
  ago in this power cycle, waits for its own engine (then runs its
  `restart_command` if set) instead; a re-send inside a waking cycle is
  skipped while another lane answers. A rehearsal showed the ComfyUI lane
  running the wake command 6 s after the text lane's wake had brought the
  machine up, which a power-toggle wake would undo. Lanes without `host`
  are unchanged.
- Managed lanes (`local_executors`): operator commands no longer run
  unbounded. A power-off or restart command still running after
  `command_timeout_secs` (default 60) is stopped and counts as failed (a
  power-off is then retried); a wake command gets `boot_timeout_secs`
  (default 180) when that is longer, so a wake that blocks until the
  machine answers keeps working.
- **License: Apache-2.0 → Elastic License 2.0** (source-available).
- Egress identity headers can no longer set or override auth; auth is applied
  last (the lease always wins).
- Routing and ledger now price from a single function
  (`AdapterRegistry::cost_micros`: cost-map index → catalog fallback).

### Fixed

- **The menu bar item is placed where macOS keeps it visible**: the item had
  no stored place, so macOS put it at the left end of the status items, the
  first place it hides when a browser's wide menus are in front (frame x=613
  on a 1920-point menu bar, reported hidden). Before it creates the item the
  app now writes its place, 280 points from the right end, beside the
  system's own items (macOS reads it only at creation, which is why writing
  it while the app ran did nothing). A place the owner chose by
  Command-dragging the item is kept; an older install whose item sits
  further left gets the new place once. A test copy measured x=1520,
  visible, with a browser in front.

- **No connection to a machine the gateway knows is off**: the supervisor
  probed every routed lane's health endpoint every 10 s, and a lane on a
  machine that adopts outside wakes as well, so a powered-off machine kept
  getting connection attempts to its engine port. Once the gateway powers a
  machine (it has a wake command) and a probe has confirmed it off (every
  lane offline, no work, no wake running, no engine answering, a probe of
  each routed lane since our last power-off that got no answer at all and
  that routing reads as down), nothing but its wake path connects to it:
  no supervisor probe, no refresh probe when routing skips it, no
  engine-control health call, no look by an operator's power-off (it
  answers `"known_off": true`, and `sb ai off` prints "known off, not
  checked; nothing to do"). A request's, job's or hold's wake and an
  operator's power-on still look first and wake it. Only no answer
  (connection failed or timed out) counts: an engine that answers 503, or
  any other status, never makes its machine known off. A lane probed before
  a reload routed it is probed again before its machine is known off. A
  wake of ours that ran less than twice the lane's boot timeout ago, with
  no power-off since, keeps the machine looked at: a machine that boots
  slower than its boot timeout is taken up when it answers (on any
  machine) and powered off when idle. A machine with `adopt_external_wake`
  that is known off gets one plain health probe every 15 minutes, never a
  wake, so one started by hand is adopted and powered off when idle; it is
  also adopted at the next look of its wake path. A machine without
  `adopt_external_wake` gets no look at all while known off. A machine
  whose state the gateway has not seen yet (just after a start) is looked
  at once, as before.

- **A refused settings change says why**: when a lane's `settings_command`
  refused a change it exits non-zero and prints its reason on stdout, and
  the gateway showed only `exit 2:`. The error (the apply answer of a
  settings write, `last_apply` in `/v1/engines`, and what `sb ai set ...
  --load` prints) now carries the command's stderr, then its stdout
  (joined by ` | `), within 300 bytes on one line with control characters
  removed; each keeps at least half the room, so an ssh warning cannot
  hide the script's reason. Known secret values are replaced by
  `[redacted]` before the cut: the value of every env var the gateway
  config names (`api_key_env`, an account's `env` and every other `*_env`),
  and any value over 20 characters of an env var whose name holds KEY,
  TOKEN, SECRET or PASSWORD. The command's environment is not changed. A
  command that succeeds is read as before.

- **Revert to revision 0 means "no saved settings"**: `sb ai revert MODEL 0`
  and `POST /v1/models/{model}/settings/revert {"revision": 0}` answered 404,
  so the only way back to the engine's own values was `sb ai unset` with
  every key. Revision 0 now reverts to an empty document (no defaults,
  profiles, load settings or locks) written as a new revision, so the
  history stays; load settings are applied the same way an unset applies
  them, so a lane's `settings_command` puts the engine's config back to its
  base. Against an older gateway, `sb ai revert MODEL 0` writes the empty
  document itself, against the revision it read first: a write that landed
  in between makes it a conflict ("the settings changed meanwhile"), never
  wiped.

- **A settings name shared with its first target writes that target**: a
  route named like one of its targets (`mac/x` -> `mac/x`, `box/y`) answered
  `ambiguous_model` to settings writes, load and unload, and `sb ai set`
  said "name one" although the name was already the target's own id. The
  exact target is now used; a route whose name is none of its targets stays
  ambiguous.

- **The engine control sends the provider's key**: its calls to a local
  engine (the model listing, `GET /models?reload=1`, `/models/load`,
  `/models/unload`, `/props`) carry the key the provider's text requests
  carry (`api_key_env` or `api_key`, with `auth_scheme`, bearer by default),
  for `openai_compatible` with any `engine` and for `comfyui`. A llama.cpp
  router started with `--api-key` answered them 401, so `GET /v1/engines`,
  loads, unloads and applies failed. Health probes, workloads and the
  provider doctor send what they sent before.
- **`GET /v1/workflows` lists only the inputs a workflow accepts**: `size`
  and `seed` show where the workflow binds them (`width` and `height`,
  `seed`); an image-in workflow such as a segmentation no longer offers
  inputs that answer 400.
- **A lane whose engine answers reads `healthy`** in the capacity report,
  also when Switchback did not wake its machine (it read `offline`).
- **A job task that panics** frees its pending slot and fails its job
  (error class `internal`) instead of leaking both.
- Generated artifacts no longer stay in process memory until a restart (a
  slow leak that ended in an out-of-memory restart on a long-running
  gateway). They are kept on disk under the runtime state directory
  (`<state_root>/artifacts`), content addressed by sha256, with one
  metadata file each (record and owner; no prompt or graph), directories
  `0700` and files `0600`, and they survive a restart. Retention runs at
  every write and at start-up: older than `server.artifacts.max_age_secs`
  (default 7 days) goes, then the oldest until the total fits
  `server.artifacts.max_bytes` (default 2 GiB). `storage: memory` keeps
  them in memory under a hard cap (default 256 MiB); an unusable directory
  falls back to it with a warning. `GET /v1/artifacts/{id}` streams from
  disk with its length; an evicted artifact answers 404 `artifact_evicted`.
- A client key can read its own jobs and artifacts. It could create an
  image but was refused (403) on `GET /v1/artifacts/{id}`, so every caller
  needed an admin key. `GET /v1/jobs`, `/v1/jobs/{id}`, its events and
  `/v1/artifacts/{id}` now show a client what its own tenant and project
  made; another tenant (or project) gets 404, not 403. Operator and admin
  keys read everything, as before.
- Media jobs, review fixes before release:
  - An artifact is served only with an image or video media type from an
    allow-list, with `X-Content-Type-Options: nosniff`; an upstream
    `text/html` or `image/svg+xml` answer is never served back as such
    (anything off the list is an `application/octet-stream` attachment).
  - `POST /v1/uploads` streams into the store, is capped by
    `server.artifacts.max_upload_bytes` (default 32 MiB, 413 above) even
    when `server.max_request_bytes` is unset, and must be the PNG, JPEG or
    WebP image its `Content-Type` names (415 otherwise).
  - An image model or workflow name that does not resolve answers 400; it
    no longer gets the mock's 1x1 placeholder image (also on `main`). Only
    `mock/image` reaches the mock.
  - A `size` or `seed` the ComfyUI workflow does not bind answers 400 on
    both image endpoints instead of being dropped.
  - A cancel that lands while ComfyUI is queueing the prompt still removes
    and interrupts it; a cancel interrupts only when `GET /queue` lists its
    own prompt as running (an older ComfyUI interrupts whatever runs); the
    lane is released after ComfyUI is told.
  - A blocking `/v1/images/generations` request whose client disconnects
    cancels its job (`source: client_gone`): only `POST /v1/jobs` outlives
    its caller.
  - A job on a lane with no wake command no longer stays `queued` forever:
    a background job fails after its workflow timeout; the blocking
    endpoint still answers `queued` and its record ends `failed`
    (`wake_unconfigured`).
  - The artifact store claims its directory with a `.switchback-artifacts`
    marker and refuses a directory holding other data, so its start-up
    clean-up never deletes another program's files.
  - `GET /v1/workloads/capacity` reports `artifacts` (storage, caps, bytes,
    and `fallback`, why a configured disk store is in memory). A system
    service with no runtime-root variable and a working directory of `/`
    falls back to memory: set `server.artifacts.dir` (see
    PROVIDER_SETUP.md).

- A provider whose credential rides in the URL (`auth_scheme: { kind:
  query }`) no longer shows the credential when a call fails before an
  answer (connection refused, timeout). The error text of a chat call
  (stream and non-stream), an embeddings call and a model listing quoted
  the full URL, query included, and that text reached the client's error
  reply and the output of `sb provider test`, `sb provider models` and
  `sb provider doctor`. The error text now drops the URL and keeps the
  error kind (`error sending request`).

### Security

- `Secret` is no longer `Serialize`/`Deserialize` — it cannot cross a
  serialization boundary.

## [0.1.0]

- Initial local-first AI execution gateway: canonical IR, explainable routing +
  two-level fallback, multi-account auth + age-encrypted vault, OpenAI/Anthropic
  ingress, OpenAI/Anthropic/Gemini/Vertex upstreams, metadata-only traces + usage
  ledger, egress paths, and a read-only control plane.
