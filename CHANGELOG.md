# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it reaches
`1.0`.

## [Unreleased]

### Added

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
