# Switchback

**A local-first AI execution gateway.** One Rust binary that receives every AI
call (OpenAI- or Anthropic-compatible HTTP), normalizes it into a canonical typed
IR, routes it across providers / accounts / runtimes with an **explainable
decision** and **fallback**, and streams the response back in the client's own
wire format.

> **Name:** *switchback* — a road that keeps climbing by re-routing. Switching + resilience.

Point your existing OpenAI/Anthropic client at Switchback and it keeps working,
but now it's **multi-provider, multi-account, observable, and cost-aware** — with
no client code changes.

```bash
# your app, unchanged — just a different base URL
curl localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"openrouter/anthropic/claude-3.5-sonnet","messages":[{"role":"user","content":"hi"}]}'
```

---

## What it does

- **One hub, many wire formats.** OpenAI Chat Completions, OpenAI Responses,
  Anthropic Messages, Google Gemini/Vertex, and AWS Bedrock (SigV4 + binary
  event-stream) — stream **and** non-stream — translated through a single
  canonical IR and rendered back in the client's format. Adding an OpenAI-shaped
  provider (OpenRouter, Groq, Mistral, Together, DeepSeek, vLLM, …) is pure
  config; a non-bearer one is also config.
- **Explainable routing + two-level fallback.** Every request emits a
  `RouteDecision` (selected target, ordered fallbacks, rejected candidates with
  reasons). Hard capability filters (streaming / tools / JSON-schema / context),
  then fallback across **accounts** within a provider and **targets** across
  providers.
- **Cost-, latency-, and policy-aware routing** (all toggleable). Route to the
  cheapest healthy host by a blended price map, or the fastest by observed
  latency — **split into TTFT and total**, so interactive (streaming) requests
  rank on first-byte time and others on overall latency — with a `max_price`
  ceiling and `allow_free` / `allow_promo` / `allow_aggregator` lane gates.
- **Health-aware routing.** Routing sees a **non-secret account-pool view**
  (usable-account count + circuit state per target) and demotes targets whose
  only accounts are locked below ones that can actually execute — the rejection
  is named in the `RouteDecision`. Visible at `GET /v1/health`.
- **Multi-account auth.** Account selection (fill-first / round-robin), per-
  `(account, model)` availability locks with cooldowns, an **age-encrypted
  vault** (key in the OS keychain), and **live OAuth refresh** that de-duplicates
  concurrent refreshes so rotating refresh tokens aren't revoked.
- **Egress control.** Route an account's upstream calls through a named
  HTTP(S)/SOCKS5 **proxy path** (toggleable, with a `doctor` reachability check),
  plus an optional per-path client identity (custom `User-Agent` + headers).
- **Observability, end to end.** One metadata-only trace per request (route
  decision + every attempt + egress + cost) at `GET /v1/traces`, an
  `x-switchback-request-id` header, an append-only usage/cost ledger at
  `GET /v1/usage`, `tracing` request/attempt spans, and optional **OpenTelemetry
  OTLP export** (`otel` feature).
- **A control plane.** A redacted config API (`GET /v1/config`, `/v1/providers`),
  live runtime knobs (`GET`/`PATCH /v1/runtime`), atomic config **hot-reload**
  (`POST /v1/reload`) with per-request snapshot pinning (every response carries
  `x-switchback-revision`), a machine-friendly CLI
  (`switchback config show|get|validate|providers|routes`), and an embedded
  **dashboard** at `/` (no build step).
- **Durable state (opt-in).** Point `server.state_store` at a SQLite file and
  every published config revision + a change **audit log** + every request's
  **usage** are persisted (metadata only — no config body, no prompt/response).
  `/v1/usage` then survives restarts (the ledger hydrates its totals from the
  store, hot path stays in memory); readable at `GET /v1/revisions`, `/v1/audit`,
  and `/v1/usage/events`.
- **Idempotency.** Send `Idempotency-Key: <key>` and a duplicate non-streaming
  request replays the exact first response (`Idempotent-Replayed: true`); a reused
  key with a different body is a 422; a concurrent duplicate still in flight is a
  409 (single-flight, also for streams). Replay is durable when a store is set.
- **Multi-tenancy + quotas.** Map API keys to **tenants** (`api_keys:` →
  `tenants:`); usage is attributed per tenant, and a tenant's **hard limits**
  reject before upstream dispatch — `budget_usd` → 402, `max_concurrency` → 429
  (reserve-then-reconcile). Live status at `GET /v1/tenants`; spend at
  `GET /v1/usage` (`by_tenant`).
- **Admission control + backpressure.** A global `server.max_concurrency` cap
  queues bursts (bounded wait, `x-switchback-queue-ms`) and sheds with 503 past
  `admission_timeout_ms`; `server.max_response_bytes` caps the non-streaming
  collect path; the streaming path cancels the upstream when the client hangs up.
- **Plugins.** Trusted built-in plugins (`plugins:` in config), compiled into the
  snapshot and run on the hot path: `model_blocklist` (reject by model),
  `request_tag` (inject metadata), `egress_pin` (pin models to an egress). Hooks:
  `pre_route` / `post_route` / `select_egress` / `post_attempt`. Active chain at
  `GET /v1/plugins`.
- **Adaptive model pass-through.** A model the gateway has never heard of is
  forwarded verbatim to a default provider — add a model with no rebuild.
- **RTK-style tool-result compression** (opt-in, fail-safe: never grows, never
  empties).

## Architecture

Acyclic crate graph; `sb-core` (the provider-agnostic canonical IR) is the root
everything depends on:

```
sb-core        canonical IR + config + error taxonomy + catalog + RoutingPolicy
  ├ sb-adapter      ProviderAdapter trait, AdapterError, SSE helpers
  ├ sb-protocols    OpenAI <-> canonical hub; anthropic/gemini/responses; schema downleveler
  ├ sb-router       hard filters → ordered candidates → RouteDecision (cost/latency/policy)
  ├ sb-credentials  multi-account selection, availability locks, age vault, OAuth refresh
  ├ sb-compress     RTK fail-safe tool-result compression
  ├ sb-ledger       append-only usage/cost ledger (priced from the catalog)
  └ sb-trace        per-request TraceRecord (decision + attempts + cost) + OTel spans
       └ sb-adapters   ComposedAdapter(WireCodec × AuthScheme); egress pool; latency tracker
            └ sb-runtime  the execution runtime: immutable revisioned CompiledSnapshot + the
            │             Engine that owns the attempt state machine (route → resolve → retry →
            │             fallback → hedge → budget → trace); hot-swappable, HTTP-agnostic
                 └ sb-server   Axum app + handlers + SSE + CLI over Engine::execute → `switchback`

sb-store      StateStore trait + bundled-SQLite backend: config revisions + audit log + durable usage (sb-runtime & sb-ledger dep)
sb-plugin     Plugin trait + trusted built-ins (model_blocklist / request_tag / egress_pin); a sb-runtime dep
```

The **credential boundary** is the load-bearing seam: `sb-router` picks the
*target* (provider/model), `sb-credentials` picks the *account* + secret and
tracks availability, `sb-adapters` *executes* with the lease it's handed, and
`sb-server` is the only place the two are joined. Conventions and invariants live
in [`AGENTS.md`](AGENTS.md) — read it before contributing.

## Quickstart

```bash
cargo build
cargo run -p sb-server -- serve --config config/switchback.example.yaml

# health + a credential-free mock round-trip (no API keys needed):
curl -s localhost:8765/health
curl -s localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}'

# streaming:
curl -N localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock/echo","stream":true,"messages":[{"role":"user","content":"hi"}]}'

# the dashboard:
open http://localhost:8765/
```

Copy `config/switchback.example.yaml` to a local file (git-ignored) and add real
providers/keys. The example documents every option: providers, multi-account,
the vault, routing toggles, egress paths, and tracing.

### Useful commands

```bash
switchback serve   --config <file>     # run the gateway
switchback doctor  --config <file>     # config + provider + egress diagnostics
switchback vault   init|set|list|rm    # manage the encrypted credential vault
switchback config  show|get <path>|validate|providers|routes   # introspect (JSON)
```

OpenTelemetry export is opt-in: `cargo run -p sb-server --features otel -- serve …`
with `server.otel_endpoint` set to your OTLP/HTTP collector.

## Endpoints

`/` (dashboard) · `/health` · `/v1/models` · `/v1/chat/completions` ·
`/v1/responses` · `/v1/embeddings` · `/v1/messages` (+ `/count_tokens`) ·
`/v1/usage` (+ `/events`) · `/v1/traces` (+ `/{id}`) · `/v1/config` ·
`/v1/providers` · `/v1/runtime` (GET/PATCH) · `/v1/reload` (POST) ·
`/v1/revisions` · `/v1/audit` · `/v1/health` · `/v1/tenants` · `/v1/plugins`.

## Status

`v0.1.0` — the v1 surface is built and tested (the data plane, routing,
multi-account, observability, egress, and control plane described above), plus
the extracted execution runtime (`sb-runtime`, atomic hot-reload + per-request
revision pinning) and durable state (`sb-store`, SQLite config revisions + audit
+ usage events that survive restarts). AWS Bedrock (SigV4 + event-stream) is
built. Out of scope for now (seams only): billing/marketplace, multi-tenancy/
RBAC, DB-backed *live* config (YAML stays the bootstrap source of truth),
idempotency/quota state, learned/semantic routing. See [`AGENTS.md`](AGENTS.md)
for the full scope and the contribution recipes.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
