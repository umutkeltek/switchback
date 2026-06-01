# Switchback

[![CI](https://github.com/umutkeltek/switchback/actions/workflows/ci.yml/badge.svg)](https://github.com/umutkeltek/switchback/actions/workflows/ci.yml)
[![License: Elastic-2.0](https://img.shields.io/badge/license-Elastic--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](rust-toolchain.toml)

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
  Price ceilings and disallowed lanes are hard policy even when cost-aware
  ordering is off; `cost_unknown` and `context_unknown` decide whether missing
  price/context metadata remains eligible.
- **Health-aware routing.** Routing sees a **non-secret account-pool view**
  (usable-account count + circuit state per target) and demotes targets whose
  only accounts are locked below ones that can actually execute — the rejection
  is named in the `RouteDecision`. Visible at `GET /v1/health`.
- **Multi-account auth.** Account selection (fill-first / round-robin), per-
  `(account, model)` availability locks with cooldowns, an **age-encrypted
  vault** (key in the OS keychain), and **live OAuth refresh** that de-duplicates
  concurrent refreshes so rotating refresh tokens aren't revoked. OAuth accounts
  can use `refresh_vault` to persist rotated refresh tokens atomically back into
  the encrypted vault; env/inline refresh tokens are followed in memory only.
  Bedrock SigV4 credentials use the same account lease path, so AWS accounts
  participate in account selection, lockout, and fallback like API-key/OAuth
  accounts.
- **Hosted-mode network guard.** `server.block_private_networks: true` rejects
  private provider/proxy/token URLs during validation, refuses private DNS
  resolutions at execution time, and disables upstream redirect following in
  provider, OAuth, service-account, and proxy clients.
- **Egress control.** Route an account's upstream calls through a named
  HTTP(S)/SOCKS5 **proxy path** (toggleable, with a `doctor` reachability check),
  plus an optional per-path client identity (custom `User-Agent` + headers).
- **Text/tool IR today, explicit multimodal handling.** The current canonical IR
  models text, tools, tool results, usage, and structured-output hints. Image or
  richer multimodal request parts are rejected at ingress instead of silently
  dropped; a real multimodal IR is still future work.
- **Observability, end to end.** Metadata-only traces for routed requests
  (route decision + every attempt + egress + cost) at `GET /v1/traces`, an
  `x-switchback-request-id` header, an append-only usage/cost ledger at
  `GET /v1/usage`, `tracing` request/attempt spans, and optional **OpenTelemetry
  OTLP export** (`otel` feature). Runtime denials are traced; edge-level denials
  (auth/admission/parse) are a hardening item.
- **A control plane.** A redacted config API (`GET /v1/config`, `/v1/providers`),
  live runtime knobs (`GET`/`PATCH /v1/runtime`), atomic config **hot-reload**
  (`POST /v1/reload`) with per-request snapshot pinning (every response carries
  `x-switchback-revision`), a machine-friendly CLI
  (`switchback config show|get|validate|providers|routes`), and an embedded
  **dashboard** at `/` (no build step).
- **A declarative control plane (`/cp/v1`).** A k8s-style envelope
  (`apiVersion`/`kind`/`metadata`/`spec`) over the config — `GET
  /cp/v1/resources/{kind}` projects providers/routes/tenants/egress/plugins as
  resources — plus a **draft → validate → publish** lifecycle (atomic hot-swap,
  `If-Match` optimistic concurrency) and **`POST /cp/v1/route-preview`** (the
  explainable `RouteDecision` without executing). One API for the dashboard and
  the AI-facing CLI; YAML stays bootstrap.
- **Durable state (opt-in).** Point `server.state_store` at a SQLite file and
  every published config revision + a change **audit log** + every request's
  **usage** are persisted as metadata (no prompts/responses). Durable `/cp/v1`
  drafts also persist their proposed config body so they can survive restarts;
  keep the SQLite file protected like any config file if drafts may contain
  inline secrets. `/v1/usage` survives restarts (the ledger hydrates its totals
  from the store, hot path stays in memory); readable at `GET /v1/revisions`,
  `/v1/audit`, and `/v1/usage/events`. The shorthand
  `state_store: "/path/state.sqlite"` stays optional/fail-open; use object form
  with `required: true` when startup must fail if the store cannot be opened.
  The bundled SQLite backend records applied schema versions in
  `schema_migrations` before state grows further.
- **Idempotency.** Send `Idempotency-Key: <key>` and concurrent duplicate
  requests are rejected while the first is in flight (single-flight, also for
  streams); a reused key with a different body is a 422. Exact replay of a
  completed non-streaming response (`Idempotent-Replayed: true`) requires both
  `server.state_store` and `server.idempotency.persist_response_bodies: true`.
- **Multi-tenancy + quotas.** Map API keys to **tenants** (`api_keys:` →
  `tenants:`); usage is attributed per tenant, and a tenant's **hard limits**
  reject before upstream dispatch — `budget_usd` → 402, `max_concurrency` → 429
  (reserve-then-reconcile). Live status at `GET /v1/tenants`; spend at
  `GET /v1/usage` (`by_tenant`).
- **Admission control + backpressure.** A global `server.max_concurrency` cap
  queues bursts (bounded wait, `x-switchback-queue-ms`) and sheds with 503 past
  `admission_timeout_ms`; `server.max_response_bytes` caps the non-streaming
  collect path; the streaming path cancels the upstream when the client hangs up.
- **Plugins, two tiers.** Trusted trait-object built-ins (`plugins:` in config),
  compiled into the snapshot and run on the hot path: `model_blocklist` (reject
  by model), `request_tag` (inject metadata), `egress_pin` (pin models to an
  egress). Hooks: `pre_route` / `post_route` / `select_egress` / `post_attempt`;
  active chain at `GET /v1/plugins`. Plus optional **sandboxed Wasm** plugins
  (`type: wasm`, build with `--features wasm`) running a guest module in a
  Wasmtime sandbox with per-call fuel, timeout metadata, and `failure_mode:
  open|closed` — the public, default-off extension story.
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

## Install

**From source** (needs a stable Rust toolchain — `rustup` pins it via `rust-toolchain.toml`):

```bash
git clone https://github.com/umutkeltek/switchback
cd switchback
cargo build --release          # binary at target/release/switchback
```

**Prebuilt binaries** — each tagged release attaches archives for
linux/macOS/windows (x86_64 + aarch64) with sha256 checksums. See the
[Releases](https://github.com/umutkeltek/switchback/releases) page.

**Docker** — a multi-arch image is published to GHCR on every release:

```bash
docker run --rm -p 8765:8765 ghcr.io/umutkeltek/switchback:latest
# or build locally:
docker build -t switchback . && docker run --rm -p 8765:8765 switchback
```

The image starts with the zero-setup `config/quickstart.yaml` (mock-only) so it
serves immediately; mount your own config to go live:

```bash
docker run --rm -p 8765:8765 -v "$PWD/my-config.yaml:/config.yaml" \
  ghcr.io/umutkeltek/switchback:latest serve --config /config.yaml --bind 0.0.0.0:8765
```

## Quickstart

```bash
# zero-setup: mock-only config, no API keys, serves immediately
cargo run -p sb-server -- serve --config config/quickstart.yaml

# health + a credential-free mock round-trip:
curl -s localhost:8765/health
curl -s localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}'

# streaming:
curl -N localhost:8765/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"mock/echo","stream":true,"messages":[{"role":"user","content":"hi"}]}'

# the dashboard:
open http://localhost:8765/
```

`config/quickstart.yaml` is the zero-dependency starting point. To go live, copy
`config/switchback.example.yaml` to a local file (git-ignored) and add real
providers/keys — it documents every option: providers, multi-account, the vault,
routing toggles, egress paths, and tracing. (The example ships with an active
`bedrock` provider, so it needs real AWS credentials in the environment to
start; `quickstart.yaml` needs nothing.)

By default (no `server.api_key`/`api_keys`) the gateway is open on loopback —
fine on `127.0.0.1`. Non-loopback binds such as `0.0.0.0` must configure an API
key or explicitly set `server.allow_open_admin: true`. **Set a key to lock it
down**: once configured, every endpoint except `/` and `/health` (config,
providers, traces, usage, and the whole control plane) requires it, not just the
inference path. The embedded dashboard can send this key from its header field;
it is stored in browser local storage.

### Useful commands

For the full operator and agent-facing CLI contract, see [`CLI.md`](CLI.md).
For real provider recipes, see [`PROVIDER_SETUP.md`](PROVIDER_SETUP.md).
Use `--json` on human-default commands when an agent needs parseable stdout.

```bash
switchback init    --config switchback.yaml  # create a mock-only starter config
switchback --json doctor --config switchback.yaml  # machine-readable install/config report
switchback schema commands        # command contract for agents
switchback schema config          # common config paths for agents
switchback provider presets       # provider defaults and onboarding examples
switchback provider readiness     # readiness manifests for all provider presets
switchback provider readiness openai  # one provider readiness contract
switchback mcp --config switchback.yaml  # stdio MCP control tools
switchback provider add openai --config switchback.yaml --model "$MODEL_ID"
switchback --json provider add openai --config switchback.yaml --model "$MODEL_ID"
switchback provider models openai --config switchback.yaml
switchback provider sync-routes openai --config switchback.yaml
switchback provider test openai --config switchback.yaml  # auto-picks first discoverable model
switchback provider doctor openai --config switchback.yaml  # discovery + chat + stream + embeddings report
switchback provider certify openai --config switchback.yaml  # stable provider readiness report
switchback provider certify-all --config switchback.yaml  # certify every configured provider
switchback provider certify-all --config switchback.yaml --skip-missing-env  # certify only providers with credentials present
switchback provider matrix --config switchback.yaml  # doctor every provider; missing env keys are skipped
switchback serve   --config <file>     # run the gateway
switchback doctor  --config <file>     # config + provider + egress diagnostics
switchback route-preview --config <file> --model auto/cheap   # explain routing locally
switchback vault   init|set|list|rm    # manage the encrypted credential vault
switchback config  show|get <path>|validate|providers|routes   # introspect (JSON)
switchback config  set <path> <json>|unset <path>|patch --from-file patch.yaml|format
```

OAuth accounts support `token_vault`, `refresh_vault`, and
`client_secret_vault`. When a token endpoint returns a rotated refresh token,
Switchback writes it back only for `refresh_vault` accounts, using the same
atomic encrypted-vault write path as `switchback vault set`.

For providers without a reliable model-list endpoint, set `model_hint` on the
provider; `provider test`, `provider doctor`, and `provider matrix` use it as
their default smoke-test model.

Provider presets: `openai`, `openrouter`, `anthropic`, `gemini`, `deepseek`,
`groq`, `mistral`, `together`, `fireworks`, `cerebras`, `xai`, `nvidia`,
`ollama`, `vllm`.

OpenTelemetry export is opt-in: `cargo run -p sb-server --features otel -- serve …`
with `server.otel_endpoint` set to your OTLP/HTTP collector.

## Endpoints

`/` (dashboard) · `/health` · `/v1/models` · `/v1/chat/completions` ·
`/v1/responses` · `/v1/embeddings` · `/v1/messages` (+ `/count_tokens`) ·
`/v1/usage` (+ `/events`) · `/v1/traces` (+ `/{id}`) · `/v1/config` ·
`/v1/providers` · `/v1/runtime` (GET/PATCH) · `/v1/reload` (POST) ·
`/v1/revisions` · `/v1/audit` · `/v1/health` · `/v1/tenants` · `/v1/plugins`.

Declarative control plane: `/cp/v1` (discovery) · `/cp/v1/resources/{kind}`
(+ `/{name}`) · `/cp/v1/route-preview` · `/cp/v1/admission-preview` ·
`/cp/v1/watch` (SSE) · `/cp/v1/drafts` (+ `/{id}`, `/{id}/validate`,
`/{id}/publish`).

## Status

`v0.1.0` — the v1 surface is built and tested (the data plane, routing,
multi-account, observability, egress, and control plane described above), plus
the extracted execution runtime (`sb-runtime`, atomic hot-reload + per-request
revision pinning) and durable state (`sb-store`, SQLite config revisions + audit
+ usage events that survive restarts). AWS Bedrock (SigV4 + event-stream) is
built. Multi-tenancy, idempotency single-flight, optional durable replay,
admission, RBAC roles, and quota enforcement are implemented for a single
process; cross-node quota/idempotency coordination, fine-grained resource
scopes, billing/marketplace, DB-backed *live* config (YAML stays the bootstrap
source of truth), and learned/semantic routing remain out of scope. See
[`AGENTS.md`](AGENTS.md) for the full scope and the contribution recipes.

## Contributing

Read [`AGENTS.md`](AGENTS.md) (architecture + invariants) and
[`CONTRIBUTING.md`](CONTRIBUTING.md) (workflow + the verification bar) before
opening a PR. The short version: keep the crate graph acyclic, keep `sb-core`
provider-agnostic, every request emits a `RouteDecision`, no secrets in logs, and
claims need tool evidence (`cargo build && cargo test`, clippy clean, a `curl`
smoke for request-path changes). Found a vulnerability? See
[`SECURITY.md`](SECURITY.md) — report it privately, don't open an issue.

## License

Source-available under the [Elastic License 2.0](LICENSE) (ELv2). You can use,
copy, modify, and self-host it; you may not offer it to third parties as a
hosted/managed service or remove the licensing notices. See [`LICENSE`](LICENSE)
for the terms.
