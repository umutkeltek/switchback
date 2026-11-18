# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
`1.0`.

## [Unreleased]

### Added

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
  and says the guard is broken, and new work does not clear it. Needs a
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
  `fail`, no wake and no boot-timeout wait). The secret never appears in a
  log, an error text or the capacity report. Off unless set.

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

- Managed lanes, rehearsal leftovers: after a failed `release_command` the
  next run waits `wake_retry_interval_secs` (a burst of requests used to
  spend the lane's whole `retry_budget` within seconds); a ComfyUI job that
  fails after it was accepted is kept as a `failed` job in `/v1/jobs`
  (metadata only; it used to be missing); a clean power-off clears a lane's
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

### Security

- `Secret` is no longer `Serialize`/`Deserialize` — it cannot cross a
  serialization boundary.

## [0.1.0]

- Initial local-first AI execution gateway: canonical IR, explainable routing +
  two-level fallback, multi-account auth + age-encrypted vault, OpenAI/Anthropic
  ingress, OpenAI/Anthropic/Gemini/Vertex upstreams, metadata-only traces + usage
  ledger, egress paths, and a read-only control plane.
