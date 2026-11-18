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

Operator commands are bounded by the lane's `command_timeout_secs` (on the
activator's clock seam; the shell runner kills a command dropped at its
timeout), so a hung command fails and frees the wake lock. A failed
power-off is retried after `wake_retry_interval_secs`, at most
`retry_budget` times, then the lane is `Degraded` and escalated. After its
own power-off a lane's healthy answer is not trusted until a probe has
failed once or a wake has run. A request waiting for a wake keeps its
admission, tenant and idempotency slots (documented sizing, not released:
releasing the idempotency claim would let a retry double-dispatch, and
re-taking the admission slot would add a second queue and a second 503
path after the wake).

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
