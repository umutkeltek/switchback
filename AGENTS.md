# AGENTS.md — Switchback engineering guide

> Read this before writing any code. It is the source of truth for conventions and invariants.
> Claude-Code-specific notes live in `CLAUDE.md` (which defers to this file).
> Design rationale lives in `docs/` (git-ignored, private): `chatgpt_pro_architecture.md` (the spec), `deepresearch.md` (the critique), `9router-DECONSTRUCTION.md` (what to steal / avoid).

## What Switchback is

A **local-first AI execution gateway**: one Rust binary that receives every AI call (OpenAI/Anthropic-compatible HTTP), normalizes it into a **canonical typed IR**, routes it across providers / accounts / runtimes with an **explainable decision** and **fallback**, and streams the response back in the client's format. Built so it can grow team → hosted → OpenRouter-class **without a rewrite** — by hardening seams, not piling on providers.

## Golden rules (invariants — do not break these)

1. **The core never sees provider wire formats.** `sb-core` types (`AiRequest`, `AiStreamEvent`, …) are provider-agnostic. All OpenAI/Anthropic/etc. JSON lives in `sb-protocols` and adapters, translated at the edges. If you find yourself putting `"choices"` or `"chat.completion"` in `sb-core`, stop.
2. **Every request produces an explainable `RouteDecision`** (selected target, reason[], fallbacks[], rejected[] with reasons). Routing is never an opaque black box.
3. **Secrets are leases and are never logged.** Use `Secret`/`CredentialLease`; they redact in `Debug`. Logs are **metadata-only by default** (request id, model, provider, latency, tokens, error class, route reason — never prompt/response/keys).
4. **Streaming-first, one path.** Adapters always emit a normalized `Stream<AiStreamEvent>`. Non-streaming responses are produced by *collecting* that stream. Do not write a second non-streaming code path. One SSE decoder + one encoder per wire format — never three.
5. **Deterministic before clever.** v1 routing is hard-filters → ordered candidates → fallback. No ML/semantic routing in the hot path.
6. **Don't widen the provider surface faster than you harden the seams.** A new adapter is cheap only because the trait/IR are clean. Keep them clean first.

## Architecture & crate map

Acyclic crate graph (`sb-core` is the root everything depends on):

```
sb-core        canonical typed IR + config types + error taxonomy. NO deps on other sb crates.
   ├── sb-adapter      ProviderAdapter trait + AdapterError + shared HTTP/SSE helpers
   ├── sb-protocols    OpenAI <-> canonical (ingress, egress, upstream) + SSE encode/decode  ← the hub
   ├── sb-router       hard filters, candidate ordering (TARGET selection), RouteDecision
   ├── sb-credentials  multi-account auth: account selection (fill_first/round_robin) +
   │                   per-(account,model) availability locks + redacting leases + age vault
   ├── sb-compress     RTK-style fail-safe tool-result compression (never-empty/never-grow)
   └── sb-ledger       append-only usage/cost ledger (priced from the catalog; the marketplace seam)
            └── sb-adapters   mock + ComposedAdapter(WireCodec × AuthScheme): openai/anthropic/gemini/vertex codecs (dep: adapter, protocols, core)
                     └── sb-runtime  the execution runtime: immutable revisioned CompiledSnapshot
                     │               (config+registry+resolver+knobs behind arc_swap) + the Engine that owns
                     │               the TARGET×ACCOUNT attempt state machine (route → resolve → retry →
                     │               two-level fallback → hedge → budget → trace). HTTP-agnostic.
                     │               (dep: sb-store — durable revision/audit history)
   sb-store      StateStore trait + bundled-SQLite backend: config revisions + audit + durable usage
                 (no sb deps; also a sb-ledger dep — the usage sink)
                     └── sb-server   Axum app + handlers + SSE + clap CLI; HTTP ingress/egress + protocol
                                     translation over `Engine::execute` → binary `switchback`
```

**The runtime boundary (Oracle critique #1):** `sb-runtime::Engine::execute(req) -> (revision,
ExecOutcome)` owns request execution; `sb-server` is reduced to translating the client's wire
format in/out and rendering the `ExecOutcome`. Each request pins ONE `Snapshot` for its lifetime
(a config publish never tears a request across revisions); the ledger + trace sinks live on the
Engine and survive hot-reloads. The runtime does NOT depend on axum — failures flow as a
wire-agnostic `ExecError`. The principle: *one binary can stay; one topology cannot* — the same
crate supports a future separate data-plane binary without rewriting execution.

**The credential boundary (separation of concerns — do not blur it):** `sb-router`
picks the *target* (provider/model); `sb-credentials` picks the *account* + secret and
tracks availability; `sb-adapters` *executes* with the lease it's handed; `sb-server` is
the only place the two are joined. Adapters must contain NO account-selection logic; the
router must contain NO credential logic.

Request lifecycle (the hot path):

```
HTTP in → sb-protocols (ingress: client JSON → AiRequest)
        → sb-router      (filter → order → RouteDecision; picks the TARGET provider/model)
        → sb-credentials (resolve(provider, model) → ACCOUNT + lease; skips locked accounts)
        → sb-adapters    (canonical → upstream wire, execute with lease, upstream stream → AiStreamEvent)
        → sb-protocols   (egress: AiStreamEvent → client SSE / collected JSON)
        → HTTP out       (+ metadata-only log, + x-switchback-route header)
   Fallback is TWO-LEVEL: account-level (rotate accounts within a provider, locking failed
   ones per-(account,model)) then target-level (across providers). Fallback is only legal
   BEFORE the first streamed byte.
```

## How to add a provider

Every real provider is `ComposedAdapter(Codec × Signer × Transport)` — you almost never write a new `ProviderAdapter` (only `mock` is bespoke). The three seams (`sb-adapters/src/{codec,signer,transport}.rs`) are independent: pick a codec for the wire, a signer for auth, a transport for framing.

- **Reuses an existing wire format** (OpenAI-shaped / Anthropic / Gemini)? It's **config**: a `providers:` entry with the right `type:` (and `auth_scheme:` if non-bearer). Zero code. The registry uses `ComposedAdapter::with_scheme` (= `SchemeSigner` + `HttpTransport`).
- **A new wire format?** Implement `WireCodec` in `codec.rs` (`url`, `request_body`, `parse_response`, `decoder`, optional `headers`/`embeddings_url`), delegating to a `sb-protocols::<format>` module. The execute loop, auth, streaming, and fallback are inherited.
- **A new auth method that signs the built request** (SigV4, …)? Implement `RequestSigner` in `signer.rs` (it sees a `SignTarget`: method/host/path/body). Bearer/header/query auth already exists as `SchemeSigner`. (Bedrock = `BedrockCodec × SigV4Signer × EventStreamTransport`; Vertex = gemini codec × Bearer + a service-account JWT minter.)
- **A new wire framing** (binary event-stream, websocket, …)? Implement `Transport` + `Framer` in `transport.rs` (framing only — the codec's `decoder` still owns semantics). `HttpTransport` (text SSE) + `EventStreamTransport` (AWS binary) exist.
- A change to streaming/tool-calls requires a streamed-fixture test (in the codec's `sb-protocols` module).

## How to add a wire protocol (e.g. Anthropic ingress)

1. New module `sb-protocols/src/<format>.rs` with `request_from_<format>`, `to_<format>_response`, `<format>_sse_event`, and (if it's also an upstream) `canonical_to_<format>_body` + `parse_<format>_stream`.
2. New ingress route in `sb-server`. New `FORMATS` entry. Keep OpenAI canonical as the hub — translate `format ↔ canonical`, never `format ↔ other_format` directly.

## Conventions

- **Errors:** `thiserror` enums per crate; map to the shared `ErrorClass` at the adapter boundary. No `unwrap()`/`expect()` in the hot path; no silent `let _ = ...` swallowing of errors that matter (9router's `.catch(()=>{})` is the anti-pattern).
- **Async:** Tokio. Adapters return `BoxStream<'static, Result<AiStreamEvent, AdapterError>>`.
- **Serde:** `#[serde(rename_all = "snake_case")]` on config; explicit field mapping for wire formats.
- **Tests:** unit tests next to code; protocol fixtures under `tests/fixtures/`. A change to streaming/tool-calls requires a test.
- **Commits:** conventional (`feat:`, `fix:`, `refactor:`, `test:`, `docs:`). Small, focused.
- **No new crate** without a reason that maps to a real seam.

## Build / run / test

```bash
cargo build                                  # whole workspace
cargo test                                   # all crates
cargo run -p sb-server -- serve --config config/switchback.example.yaml
# smoke (mock adapter, no creds):
curl -s localhost:8765/health
curl -s localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}'
curl -N localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock/echo","stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

## v1 scope (do not exceed without asking)

In: OpenAI-compatible `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/models`, `/v1/usage`, `/health`, plus Anthropic ingress `/v1/messages` (+ `/v1/messages/count_tokens`) — stream+non-stream throughout, rendered back in the client's own wire format; mock + openai_compatible + anthropic + gemini adapters (three distinct upstream wire formats through one hub); multi-account YAML config; **capability-filtered, explainable routing** (hard-filters on streaming/tools/json-schema/context, sourced from realistic per-api-kind defaults + the catalog's per-model facts — so the filter is real, not a no-op) + two-level (target × account) fallback; metadata-only logs; **encrypted credential vault** (age-encrypted file + OS-keychain key, `vault` CLI, `auth.vault` source — §13.4 "day-one" gap closed); **RTK-style tool-result compression** (`sb-compress`, opt-in `compress_tool_results`, fail-safe never-grow/never-empty + catch_unwind passthrough); **typed data-model seams** (`sb-core::catalog` — distinct provider/model/account/credential/price entities with tenant scope, FK-by-id, referential-integrity `validate()`, and a price ledger with history; §13.3, surfaced by `doctor`); **Gemini adapter** (`sb-protocols::gemini` + `sb-adapters::gemini` — GenerateContent, `x-goog-api-key`, model-in-URL, tool-result-by-name correlation since Gemini has no tool-call ids); **capability negotiation** (catalog `Model.capability_profile()` + `ApiKind::default_capabilities()` feed the router; `RouteRequire.json_schema` + request-inferred structured-output requirement); **JSON-Schema downleveler** (`sb-protocols::schema` — `downlevel(schema, &SchemaCaps)`: anyOf→best-branch, const→string-enum, type-arrays→first, $ref/additionalProperties stripped, empty-object→placeholder; capability-driven, applied to Gemini tool schemas so complex tools work instead of 400-ing; audit §9.8); **usage/cost ledger** (`sb-ledger` — append-only, in-memory + optional JSONL sink + optional durable SQLite sink (`sb-store`, hydrates the summary base at startup so `/v1/usage` survives restarts while budgets stay in-memory), per-request usage priced from the catalog ledger in integer micro-USD, metered through streaming too; `GET /v1/usage` summary + `server.usage_log` sink); **AuthScheme seam** (`sb-core::AuthScheme` — bearer/header/query composed from config, now applied by `sb-adapters::SchemeSigner`; the request-signing case is `SigV4Signer` (Oracle #5); audit §9.6). Adding an OpenAI-shaped provider (OpenRouter/Groq/Mistral/Together/DeepSeek/NIM/vLLM…) is now pure config — `type: openai_compatible` + base_url; one that authenticates with a non-bearer header is *also* pure config (`auth_scheme: { kind: header, name: x-api-key }`); **WireCodec collapse** (every real provider rides one `ComposedAdapter` execute loop — thin codecs; only `mock` is bespoke); **Vertex** (`VertexCodec` = Gemini wire on GCP's project URL + Bearer token — a new cloud provider as a codec + auth, no new adapter); **Gemini structured output** (`response_format` → `responseSchema` via the downleveler); **Vertex service-account JWT auto-refresh** (`ServiceAccountMinter` mints/refreshes a token from a GCP key); **AWS Bedrock** (SigV4 request-signing via `sb-adapters::sigv4` + binary `event-stream` framing via `sb-adapters::event_stream`, reusing the Anthropic wire — originally a dedicated adapter, now expressed on the composed path as `ComposedAdapter(BedrockCodec × SigV4Signer × EventStreamTransport)`; see the Oracle #5 note below). Resilience: **same-target retry**, a **provider circuit breaker** (`sb-credentials::breaker`), **spend-cap budgets** (global + per-provider), and **request hedging**. Control plane: a **live runtime overlay** (`PATCH /v1/runtime`) + redacted config API + dashboard + `config` CLI. **Execution runtime** (`sb-runtime` — Oracle #1): the TARGET×ACCOUNT attempt state machine extracted out of the HTTP edge into `Engine::execute`, over an immutable revisioned `CompiledSnapshot` hot-swapped atomically (per-request snapshot pinning; `POST /v1/reload`; `x-switchback-revision` header). **Durable state** (`sb-store` — Oracle #2): a `StateStore` trait + bundled-SQLite backend persisting config revisions + an audit log + per-request usage events (metadata only — no config body, no prompt/response), opt-in via `server.state_store`. The ledger dual-writes usage and hydrates its in-memory totals from the store at startup, so `/v1/usage` survives restarts while budgets stay in-memory on the hot path. Surfaced at `GET /v1/revisions`, `/v1/audit`, `/v1/usage/events`. **Idempotency** (`sb-server::idempotency` — Oracle #2/#7): `Idempotency-Key` gives durable full-replay of non-streaming responses (key + request-body fingerprint; reused key + different body → 422) and per-process single-flight (concurrent duplicate → 409, streams included via a drop-guard held for the stream's life). Replay needs `state_store`; streams are not stored for replay. **Adapter seam** (`sb-adapters::{codec,signer,transport}` — Oracle #5): the `ComposedAdapter` execute loop is now `Codec × Signer × Transport` — codec=wire translation, signer=auth (`SchemeSigner` bearer/header/query, or `SigV4Signer` over the built request), transport=framing (`HttpTransport` text-SSE / `EventStreamTransport` AWS binary). `with_scheme` keeps simple providers a one-liner; Bedrock rides the same loop. The old `apply_auth` + bespoke Bedrock adapter are gone. **Admission control** (`sb-server::admission` — Oracle #8): a global `server.max_concurrency` semaphore queues bursts (bounded `admission_timeout_ms` wait, `x-switchback-queue-ms` header) and sheds with 503 past the timeout; the permit is held through the response/stream. `server.max_response_bytes` caps the non-streaming collect path (`collect_response` aborts → 502). Surfaced at `GET /v1/health` (`admission`). Composes with #4's per-tenant concurrency (global = gateway protection, per-tenant = fairness).

 **Health-aware routing** (Oracle #3): `sb-credentials::pool_health` exposes a NON-secret account-pool view (usable-account count + circuit state) that `sb-runtime` stamps onto each candidate (`ExecutionTarget.healthy_accounts`); the router DEMOTES targets with no healthy accounts below executable ones (stable, named in the decision, never hard-rejects so a last resort survives) — surfaced at `GET /v1/health`. **TTFT/throughput split**: `LatencyTracker` keeps a second EWMA for time-to-first-token (streamed responses only); latency-aware routing ranks interactive (streaming) requests on TTFT, others on total latency.

 **Multi-tenancy + quotas** (Oracle #4): `tenants:` + `api_keys:` config — an API key resolves to a `Principal` (tenant + project) at the edge (`sb-server::tenancy`); usage is attributed per tenant (`AiRequest.tenant`, `UsageRecord/UsageEvent.tenant`, `LedgerSummary.by_tenant`). Hard limits reject BEFORE upstream dispatch: per-tenant `budget_usd` → 402 (`sb-runtime`, reconciled via `record_usage`), `max_concurrency` → 429 (in-memory reserve-then-reconcile guard held through the response/stream). Surfaced at `GET /v1/tenants` + `/v1/usage` `by_tenant`. (Account provenance / resale gating is intentionally NOT built.)

Out (seams only, not implementations): billing/marketplace, RBAC, dashboard UI, MCP/A2A, learned/semantic routing. **Persistence:** the control-plane revision/audit store + durable usage + idempotency replay now exist (`sb-store`, SQLite); still out: DB-backed *live* config (YAML stays the bootstrap source of truth), durable cross-node quota counters (per-tenant concurrency is per-process), and any raw prompt/response storage.
