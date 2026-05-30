# AGENTS.md — Switchback engineering guide

> Read this before writing any code. It is the source of truth for conventions and invariants.
> Claude-Code-specific notes live in `CLAUDE.md` (which defers to this file).
> Design rationale lives in `docs/` (git-ignored, private): `chatgpt_pro_architecture.md` (the spec), `deepresearch.md` (the critique), `9router-DECONSTRUCTION.md` (what to steal / avoid).

## What Switchback is

A **local-first AI execution gateway**: one Rust binary that receives every AI call (OpenAI/Anthropic-compatible HTTP), normalizes it into a **canonical typed IR**, routes it across providers / accounts / runtimes with an **explainable decision** and **fallback**, and streams the response back in the client's format. Built so it can grow team → hosted → OpenRouter-class **without a rewrite** — by hardening seams, not piling on providers.

It is **not** a free/unlimited arbitrage rig. See "Forbidden" below.

## Golden rules (invariants — do not break these)

1. **The core never sees provider wire formats.** `sb-core` types (`AiRequest`, `AiStreamEvent`, …) are provider-agnostic. All OpenAI/Anthropic/etc. JSON lives in `sb-protocols` and adapters, translated at the edges. If you find yourself putting `"choices"` or `"chat.completion"` in `sb-core`, stop.
2. **Every request produces an explainable `RouteDecision`** (selected target, reason[], fallbacks[], rejected[] with reasons). Routing is never an opaque black box.
3. **Secrets are leases and are never logged.** Use `Secret`/`CredentialLease`; they redact in `Debug`. Logs are **metadata-only by default** (request id, model, provider, latency, tokens, error class, route reason — never prompt/response/keys).
4. **Streaming-first, one path.** Adapters always emit a normalized `Stream<AiStreamEvent>`. Non-streaming responses are produced by *collecting* that stream. Do not write a second non-streaming code path. One SSE decoder + one encoder per wire format — never three.
5. **Deterministic before clever.** v1 routing is hard-filters → ordered candidates → fallback. No ML/semantic routing in the hot path.
6. **Don't widen the provider surface faster than you harden the seams.** A new adapter is cheap only because the trait/IR are clean. Keep them clean first.

## Forbidden (never add to this codebase)

- Subscription **impersonation** (using a vendor's official OAuth client_id, spoofing CLI/IDE fingerprint headers to pass as the official client).
- **MITM / TLS interception**, hosts-file rewriting, root-CA installation, or anti-bot/JA3 fingerprint spoofing.
- **Free-tier pooling / quota bypass / credential resale.**
- Reverse-engineered private provider protocols as **core** behavior.

These are the things the 9router deconstruction flagged as the trap. If such a capability is ever wanted, it is an **explicit, default-off, local-only, clearly-labeled plugin behind a trait boundary** — proposed to the maintainer first, never merged into core.

## Architecture & crate map

Acyclic crate graph (`sb-core` is the root everything depends on):

```
sb-core        canonical typed IR + config types + error taxonomy. NO deps on other sb crates.
   ├── sb-adapter      ProviderAdapter trait + AdapterError + shared HTTP/SSE helpers
   ├── sb-protocols    OpenAI <-> canonical (ingress, egress, upstream) + SSE encode/decode  ← the hub
   ├── sb-router       hard filters, candidate ordering (TARGET selection), RouteDecision
   ├── sb-credentials  multi-account auth: account selection (fill_first/round_robin) +
   │                   per-(account,model) availability locks + redacting leases + age vault
   └── sb-compress     RTK-style fail-safe tool-result compression (never-empty/never-grow)
            └── sb-adapters   concrete adapters: mock, openai_compatible, anthropic (dep: adapter, protocols, core)
                     └── sb-server   Axum app + handlers + SSE + clap CLI; orchestrates the
                                     TARGET × ACCOUNT two-level fallback → binary `switchback`
```

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

## How to add a provider adapter

1. Implement `ProviderAdapter` (in `sb-adapters/src/<name>.rs`). Reuse `sb-protocols::openai` if the provider is OpenAI-shaped — do **not** re-implement OpenAI<->canonical mapping.
2. Declare its `CapabilityProfile` (per model where it matters).
3. Map upstream errors to `ErrorClass` in `classify_error` (drives fallback/cooldown).
4. Register it in the adapter registry. Add a config `type:` variant if needed.
5. Add unit tests (request mapping + a streamed fixture). Adapters are not "done" without a streamed-response test.

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

In: OpenAI-compatible `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/models`, `/health`, plus Anthropic ingress `/v1/messages` (+ `/v1/messages/count_tokens`) — stream+non-stream throughout, rendered back in the client's own wire format; mock + openai_compatible + anthropic adapters; multi-account YAML config; explainable routing + two-level (target × account) fallback; metadata-only logs; **encrypted credential vault** (age-encrypted file + OS-keychain key, `vault` CLI, `auth.vault` source — §13.4 "day-one" gap closed); **RTK-style tool-result compression** (`sb-compress`, opt-in `compress_tool_results`, fail-safe never-grow/never-empty + catch_unwind passthrough); **typed data-model seams** (`sb-core::catalog` — distinct provider/model/account/credential/price entities with tenant scope, FK-by-id, referential-integrity `validate()`, and a price ledger with history; §13.3, surfaced by `doctor`). Next candidate: a second concrete upstream beyond openai_compatible/anthropic (OpenRouter first-class, or Gemini's distinct wire format).

Out (seams only, not implementations): billing/marketplace, multi-tenancy/RBAC, dashboard UI, MCP/A2A, learned/semantic routing, persistence/DB, any arbitrage/impersonation.
