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
