# Full-System Audit Report

## 1. Executive summary

- Switchback is a local-first AI execution gateway: OpenAI/Anthropic-compatible ingress, canonical IR, provider/account routing, streaming normalization, usage/trace surfaces, and a control plane.
- Overall verdict: the architecture is directionally strong and the test suite is much better than a typical early v0.1 gateway, but the project is currently sharper as a local/team tool than as a hosted/multi-tenant product.
- The top problems are not Rust style problems. They are trust-boundary and coherence problems: a redaction bug leaks tenant API keys, `/v1/embeddings` bypasses the runtime, streaming failures are misclassified after the gateway has already marked the attempt successful, hedging undercounts real upstream attempts, and config validation/CI give a false sense of release readiness.
- Tests pass, but several important invariants are not actually tested: redaction of `api_keys`, cross-tenant idempotency isolation, embeddings parity with the runtime, mid-stream upstream failure accounting, and hedge loser accounting.
- Blunt verdict: keep building. This is a good project. But before adding more providers or UI, harden the trust boundary and request-execution invariants. The dangerous bugs are concentrated enough that a focused remediation pass can materially improve the whole system.

## 2. Inferred product model

- Target user: local power users, solo builders, small teams, and future hosted operators who want one AI gateway across providers/accounts/egress paths.
- Core value proposition: point existing OpenAI/Anthropic clients at Switchback and get multi-provider routing, account fallback, observability, cost controls, and local-first control without client rewrites.
- Main workflows: configure providers/accounts/routes, send chat/responses/messages/embeddings requests, route/fallback/stream responses, inspect traces/usage/health, manage runtime knobs and drafts through CLI/control plane/dashboard.
- Core entities / concepts: `Config`, `ProviderConfig`, `AccountConfig`, `CredentialLease`, `AiRequest`, `AiStreamEvent`, `RouteDecision`, `ExecutionTarget`, `UsageLedger`, `TraceRecord`, `TenantConfig`, `ApiKeyConfig`, control-plane draft.
- Critical assumptions: secrets never leave the process; every request is explainable; fallback is legal only before response commitment; usage/cost accounting is authoritative enough for budgets; YAML bootstrap and live control-plane state do not diverge silently.
- Known unknowns: no live production deployment was inspected; real provider behavior was not exercised except existing optional/skipped live-test hooks; security posture is evaluated from code and local behavior, not external penetration testing.

## 3. System map

- Major modules: `sb-core` owns provider-agnostic IR/config/error/catalog types; `sb-protocols` owns wire translation; `sb-router` plans route decisions; `sb-credentials` resolves accounts/secrets/health; `sb-adapters` executes codec/signer/transport requests; `sb-runtime` owns the main execution state machine; `sb-server` owns Axum HTTP, CLI, dashboard, and control-plane handlers; `sb-store` persists revisions/audit/usage/idempotency/drafts; `sb-ledger` records usage/cost; `sb-trace` records request traces.
- Boundaries: HTTP wire formats enter/leave in `sb-server` + `sb-protocols`; provider auth lives in `sb-credentials`; provider wire execution lives in `sb-adapters`; routing target choice lives in `sb-router`; most request execution lives in `sb-runtime`.
- Data flow: HTTP JSON -> protocol parser -> canonical `AiRequest` -> plugins/compression -> route plan -> account lease -> adapter stream -> canonical stream -> protocol egress -> HTTP response; usage and trace records are written alongside completion.
- External dependencies: upstream LLM providers, optional proxies, optional OAuth/GCP token endpoints, optional SQLite state store, optional OTel endpoint, GitHub CI/release workflows.
- Operational assumptions: default local bind is trusted; auth is opt-in; state store is optional; budgets and tenant concurrency are in-process; release binaries and Docker image are produced on tags.

## 4. Highest-risk contradictions

- The control plane claims redacted config views, but `api_keys[].key` is serialized in plaintext.
- The architecture says runtime owns request execution, but `/v1/embeddings` still has a separate execution loop outside `Engine::execute`.
- The stream path marks account/provider success before the stream is actually successful, so observability and health can say "success" while the client saw a streamed error.
- Hedging is sold as cost/resilience aware, but only the winner is accounted/traced; losing upstream requests may still execute and bill.
- Durable state is described as metadata-only, but durable control-plane drafts intentionally persist full config bodies including inline secrets.
- `config validate` and CI look like a release gate, but the example config currently fails validation and CI masks that failure with `|| true`.
- Multi-tenancy exists, but semantic config validation does not ensure API keys point at declared tenants, so a typo can silently disable quotas for that key.

## 5. Findings table

| ID | Severity | Confidence | Type | Area | Short title | Impact |
|---|---|---:|---|---|---|---|
| SECURITY-001 | Critical | High | SECURITY | control plane / CLI | Tenant API keys leak through redacted config | Exposes bearer keys to any caller allowed to read config |
| ARCH-001 | High | High | ARCH | embeddings | `/v1/embeddings` bypasses the runtime | Breaks routing/trace/budget/admission/plugin invariants |
| RELIABILITY-001 | High | High | RELIABILITY | streaming | Stream attempts are marked successful too early | Bad health, bad traces, no useful fallback on early stream errors |
| RELIABILITY-002 | High | Medium | RELIABILITY | hedging | Hedge losers are invisible to cost/trace/budget | Spend and provider side effects can exceed recorded reality |
| SECURITY-002 | High | High | SECURITY | idempotency / tenancy | Idempotency keys are global, not tenant-scoped | Cross-tenant replay/mismatch risk |
| DATA-001 | High | High | DATA | config / tenancy | API keys can reference nonexistent tenants | Quotas can be bypassed by config typo |
| PROCESS-001 | High | High | PROCESS | CI / examples | Example config fails validation and CI ignores it | Release gate masks broken public config |
| SECURITY-003 | High | High | SECURITY | outbound URLs | SSRF/egress allowlisting is not implemented | Hosted mode can be abused against internal networks |
| SECURITY-004 | Medium | High | SECURITY | durable drafts | Durable drafts persist full configs with secrets | At-rest secret model contradicts docs and operator expectations |
| CONTRACT-001 | Medium | High | CONTRACT | config validation | Semantic route/control-plane validation is too thin | Bad routes and references fail at request time, not validation time |
| RELIABILITY-003 | Medium | High | RELIABILITY | OAuth / service account | Token fetchers ignore configured timeouts | Auth refresh can hang outside upstream timeout policy |
| FLOW-001 | Medium | High | FLOW | dashboard / auth | Dashboard is unusable when auth is enabled | The advertised control UI cannot control protected gateways |
| DOCS-001 | Medium | High | DOCS | README / current state | Docs contradict the implementation state | Users and agents get stale scope guidance |

## 6. Detailed findings

### SECURITY-001 Critical SECURITY - Tenant API keys leak through redacted config

**Classification:** [Fact]  
**Area:** `sb-server::controlplane`, `Config::ApiKeyConfig`, CLI `config show`, `/v1/config`, `/cp/v1/resources`

**Why this matters:**  
Inbound API keys are bearer credentials. The control plane says config views are redacted, but tenant keys are emitted as plaintext. Any caller with config-read access can extract every tenant API key and impersonate tenants.

**Evidence:**
- `crates/sb-core/src/config.rs:88` defines `ApiKeyConfig { key, tenant, project }`.
- `crates/sb-server/src/controlplane.rs:20` masks `"inline" | "token" | "refresh" | "client_secret" | "api_key" | "password" | "secret"` but not `"key"`.
- `crates/sb-server/src/controlplane.rs:64` serializes the whole config and applies that key-name redactor.
- Local reproduction: `switchback config show` on a config containing `api_keys: [{ key: "sk-tenant-secret", tenant: acme }]` printed `"key": "sk-tenant-secret"`.

**What is inconsistent / illogical:**  
The code comments assert "secrets never leave the process"; the control-plane redactor misses the exact field used for inbound tenant secrets.

**Likely root cause:**  
Redaction is generic key-name based, but new secret-bearing fields were added with a different name.

**Recommended fix direction:**  
Make redaction schema-aware for `api_keys`, or rename/store inbound key material through a secret wrapper that cannot serialize raw. Add regression tests for `api_keys[].key`, CP resources, drafts, and CLI `config show`.

**Risk if left unfixed:**  
A normal operator/debug endpoint becomes a credential disclosure endpoint.

### ARCH-001 High ARCH - `/v1/embeddings` bypasses the runtime

**Classification:** [Fact]  
**Area:** `sb-server::embeddings`, `sb-runtime::Engine`

**Why this matters:**  
The project promise is one runtime path with explainable decisions, fallback, budgets, traces, admission, and plugins. Embeddings currently has its own mini-orchestrator in `sb-server`, so important controls that exist for chat/responses/messages do not apply.

**Evidence:**
- `crates/sb-server/src/lib.rs:1231` starts a standalone `embeddings` handler.
- `crates/sb-server/src/lib.rs:1256` manually resolves routes.
- `crates/sb-server/src/lib.rs:1293` manually resolves accounts/fallback.
- `crates/sb-server/src/lib.rs:1324` calls `adapter.embeddings(...)` directly.
- No call to `state.engine.execute`, no `TraceRecord`, no ledger write, no tenant concurrency guard, no global admission guard, no plugin hook, no revision/request-id headers.

**What is inconsistent / illogical:**  
The architecture says `Engine::execute` owns request execution; embeddings is a second execution path.

**Likely root cause:**  
Embeddings was added as a protocol exception before the runtime extraction and never pulled into the runtime boundary.

**Recommended fix direction:**  
Add an embeddings execution method/IR variant in `sb-runtime` with the same route/account/admission/budget/trace/ledger semantics, or make embeddings explicitly out-of-scope until it can use the shared machinery.

**Risk if left unfixed:**  
Every new runtime safeguard will need to be remembered twice; some will be forgotten.

### RELIABILITY-001 High RELIABILITY - Stream attempts are marked successful too early

**Classification:** [Fact]  
**Area:** `sb-runtime` streaming path, `sb-server` SSE rendering

**Why this matters:**  
Streaming is the core product path. The runtime records account success, circuit success, trace success, and latency before the stream has produced a valid terminal event. If the upstream stream errors later, the client receives an error frame but the runtime can record the request as a client abort or successful attempt.

**Evidence:**
- `crates/sb-runtime/src/lib.rs:625` handles `Ok(stream)`.
- `crates/sb-runtime/src/lib.rs:627` calls `report_success` before the stream is consumed.
- `crates/sb-runtime/src/lib.rs:629` records circuit success before the stream is consumed.
- `crates/sb-runtime/src/lib.rs:638` records a successful trace attempt before terminal stream outcome.
- `crates/sb-server/src/lib.rs:911` turns `Err(error)` into an SSE error frame and then finishes the response.
- `crates/sb-runtime/src/lib.rs:1062` records any dropped, not-cleanly-completed metered stream as `completed=false`, which is treated at `crates/sb-runtime/src/lib.rs:691` as client abort status 499.

**What is inconsistent / illogical:**  
The client can see an upstream stream error while the resolver/circuit/trace have already been told the provider/account succeeded.

**Likely root cause:**  
The runtime treats "HTTP 200 and stream object created" as the execution commit point.

**Recommended fix direction:**  
Make `meter_stream` distinguish clean completion, upstream stream error, and client disconnect. Delay success/circuit success until terminal success, or record a provisional attempt that is finalized by the stream wrapper. Consider buffering until first canonical event if you want fallback before the first client-visible byte.

**Risk if left unfixed:**  
Health-aware routing learns bad facts, traces lie, circuit breakers stay closed on broken streams, and operators debug the wrong failure class.

### RELIABILITY-002 High RELIABILITY - Hedge losers are invisible to cost/trace/budget

**Classification:** [Inference]  
**Area:** `sb-runtime` hedging

**Why this matters:**  
Hedging intentionally sends multiple upstream calls. Even if local futures are dropped, remote providers may have received and billed loser requests. Switchback records only the winner, so the usage ledger and budgets can undercount real spend.

**Evidence:**
- `crates/sb-runtime/src/lib.rs:455` starts the hedge fast path for non-streaming requests.
- `crates/sb-runtime/src/lib.rs:459` returns on the first hedge win.
- `crates/sb-runtime/src/lib.rs:465` records usage for the winning response only.
- `crates/sb-runtime/src/lib.rs:476` records a trace attempt for the winning response only.
- `crates/sb-runtime/src/lib.rs:1195` drops remaining hedge futures after the first success.
- If all hedge attempts fail, `run_hedge` returns `None` and execution falls through to the normal sequential loop at `crates/sb-runtime/src/lib.rs:497`, potentially dispatching more attempts after already trying hedged ones.

**What is inconsistent / illogical:**  
The ledger is treated as authoritative for spend caps, but one feature can create unrecorded upstream calls by design.

**Likely root cause:**  
Hedging was implemented as a latency feature before a full "attempt accounting" model existed.

**Recommended fix direction:**  
Trace every hedge attempt, including losers and failures. Record loser costs where usage is known, at least mark them as "possibly billed" when canceled before usage. Add a strict policy knob for whether hedging is allowed on providers/accounts where duplicate non-streaming calls are expensive or non-idempotent.

**Risk if left unfixed:**  
Cost controls are optimistic, traces are incomplete, and users may overspend while `/v1/usage` says they did not.

### SECURITY-002 High SECURITY - Idempotency keys are global, not tenant-scoped

**Classification:** [Fact]  
**Area:** `sb-server::idempotency`, `sb-store`

**Why this matters:**  
In a multi-tenant gateway, tenant A and tenant B can reasonably send the same `Idempotency-Key`. The store keys only on the raw idempotency key, so one tenant can collide with another tenant's replay or mismatch state.

**Evidence:**
- `crates/sb-server/src/idempotency.rs:30` extracts the raw header string.
- `crates/sb-server/src/idempotency.rs:87` looks up stored records by `key`.
- `crates/sb-server/src/idempotency.rs:97` stores rendered JSON by `key`.
- `crates/sb-store/src/lib.rs:222` creates `idempotency (key TEXT PRIMARY KEY, ...)`.
- `crates/sb-server/src/lib.rs:972` authenticates first, but `crates/sb-server/src/lib.rs:976` still uses the raw key without tenant/project/path scoping.

**What is inconsistent / illogical:**  
Tenancy is first-class for usage and quota, but idempotency state is shared globally.

**Likely root cause:**  
Idempotency was modeled as a local process/store feature and not revisited after tenants were added.

**Recommended fix direction:**  
Scope idempotency records by tenant/project/auth principal + endpoint + idempotency key. Store the scope fields explicitly or derive a composite key with a stable cryptographic hash.

**Risk if left unfixed:**  
Cross-tenant replays, false 422s, and possible response disclosure between tenants.

### DATA-001 High DATA - API keys can reference nonexistent tenants

**Classification:** [Fact]  
**Area:** `Config`, tenancy, runtime budget/concurrency

**Why this matters:**  
A typo in `api_keys[].tenant` creates a valid API key whose tenant has no configured quotas. Requests are attributed to an unknown tenant string, but `budget_usd` and `max_concurrency` are skipped because the tenant lookup returns `None`.

**Evidence:**
- `crates/sb-core/src/config.rs:111` finds an API key and returns its tenant string without validating it exists.
- `crates/sb-server/src/tenancy.rs:99` treats a missing tenant config as no concurrency limit.
- `crates/sb-runtime/src/lib.rs:371` checks budget only if `snap.config.tenant(tenant)` exists.
- `crates/sb-server/src/lib.rs:545` config validation builds adapters/resolver/catalog only; it does not validate `api_keys` -> `tenants`.

**What is inconsistent / illogical:**  
Multi-tenancy is advertised as quota-enforcing, but config typos can silently turn quotas off for a key.

**Likely root cause:**  
There is no whole-config semantic validation layer for cross references outside the optional catalog.

**Recommended fix direction:**  
Add `Config::validate_semantics()` that checks unique IDs and references: `api_keys[].tenant`, route targets, egress refs, default provider, plugin egress pins, duplicate keys, and invalid numeric limits. Use it in `config validate`, draft validate/publish, reload, and serve startup.

**Risk if left unfixed:**  
Operators think a tenant is capped while the effective key is uncapped.

### PROCESS-001 High PROCESS - Example config fails validation and CI ignores it

**Classification:** [Fact]  
**Area:** CI, public examples

**Why this matters:**  
Examples are onboarding and release assets. The main example config currently fails validation in a default environment, and CI explicitly ignores the failure.

**Evidence:**
- `.github/workflows/ci.yml:41` runs example validation.
- `.github/workflows/ci.yml:42` appends `|| true`.
- Local command `cargo run -q -p sb-server -- config validate --config config/switchback.example.yaml` returned `ok: false` for missing AWS Bedrock env and missing `ANTHROPIC_API_KEY`.
- `README.md:184` says to copy `config/switchback.example.yaml`; `README.md:187` notes it needs real AWS credentials.

**What is inconsistent / illogical:**  
The CI step looks like a gate but is not a gate; the public example is intentionally non-runnable unless multiple credentials exist.

**Likely root cause:**  
The example config became a full feature catalog instead of a validating example.

**Recommended fix direction:**  
Keep `quickstart.yaml` runnable. Split `switchback.example.yaml` into a validating config with providers commented out, plus `switchback.full.example.yaml` or docs snippets for real providers. Remove `|| true` from CI.

**Risk if left unfixed:**  
New users and release automation normalize broken config as acceptable.

### SECURITY-003 High SECURITY - SSRF/egress allowlisting is not implemented

**Classification:** [Fact]  
**Area:** provider URLs, proxy URLs, OAuth token URLs, service-account token URIs

**Why this matters:**  
Hosted or team deployments let config/control-plane users influence outbound URLs. Without scheme/host/IP controls, provider `base_url`, proxy `url`, OAuth `token_url`, service-account `token_uri`, and OTel endpoints can be used to hit internal networks or metadata services.

**Evidence:**
- `SECURITY.md:45` lists SSRF allow/deny-listing as not implemented.
- `crates/sb-core/src/config.rs:493` accepts arbitrary provider `base_url`.
- `crates/sb-core/src/config.rs:150` accepts arbitrary proxy `url` / `url_env`.
- `crates/sb-credentials/src/refresh.rs:80` posts to arbitrary `token_url`.
- `crates/sb-credentials/src/service_account.rs:98` posts to arbitrary service-account `token_uri`.

**What is inconsistent / illogical:**  
The project is starting to expose a declarative control plane, but URL-bearing config is still trusted like local YAML.

**Likely root cause:**  
The current product center of gravity is local-first, while hosted/team seams are already visible.

**Recommended fix direction:**  
Implement deployment-mode-aware egress policy: default local mode can allow localhost/private ranges; hosted mode should deny metadata/link-local/private ranges unless explicitly allowlisted. Validate URL schemes and resolved IPs at config compile and before request dispatch.

**Risk if left unfixed:**  
Hosted deployments are vulnerable to internal network probing and credential metadata exfiltration.

### SECURITY-004 Medium SECURITY - Durable drafts persist full configs with secrets

**Classification:** [Fact]  
**Area:** `/cp/v1/drafts`, `sb-store`, docs

**Why this matters:**  
Operators reading the README are told durable state is metadata-only. In reality, durable control-plane drafts store the full proposed config body, including inline provider secrets and tenant API keys.

**Evidence:**
- `README.md:74` says durable state persists revisions/audit/usage and is "metadata only — no config body".
- `crates/sb-store/src/lib.rs:102` says `DraftRecord.config_json` is the full proposed config including inline secrets.
- `crates/sb-server/src/cp.rs:244` says durable drafts persist full config body including inline secrets.
- `crates/sb-server/src/cp.rs:267` serializes the full `Config` and writes it through `store.put_draft`.

**What is inconsistent / illogical:**  
The persistence story is split: revisions are metadata-only, drafts are secret-bearing, but the user-facing docs do not make that operational boundary obvious.

**Likely root cause:**  
Durable drafts were added after the first state-store privacy story and did not get a matching security model.

**Recommended fix direction:**  
Either encrypt draft bodies separately, disallow inline secrets in drafts, store secret references only, or document `state_store` as secret-bearing when drafts are enabled. Add a config knob if durable drafts are optional.

**Risk if left unfixed:**  
Users place state DBs under weaker backup/access policies than they would if they knew secrets were inside.

### CONTRACT-001 Medium CONTRACT - Semantic route/control-plane validation is too thin

**Classification:** [Fact]  
**Area:** `config validate`, draft validate/publish, reload

**Why this matters:**  
Config validation should catch bad references before traffic hits them. Today validation compiles adapters and credentials but does not validate route targets, default provider, egress references, plugin egress pins, tenant references, duplicate route names, or unknown config fields.

**Evidence:**
- `crates/sb-server/src/lib.rs:545` validates by building `AdapterRegistry`, `CredentialResolver`, and optional catalog.
- `crates/sb-runtime/src/lib.rs:323` uses the same thin validation for control-plane drafts.
- `crates/sb-runtime/src/lib.rs:1230` only discovers unknown route targets at request time.
- Config structs in `crates/sb-core/src/config.rs` do not use `deny_unknown_fields`, so typos are generally ignored by serde.

**What is inconsistent / illogical:**  
The control plane has draft validation, but important cross-reference errors remain runtime surprises.

**Likely root cause:**  
Semantic validation is distributed across compile paths rather than centralized on `Config`.

**Recommended fix direction:**  
Add a first-class semantic validator and call it from CLI validate, serve startup, reload, draft validate, and draft publish. Prefer collecting all problems, not returning the first one.

**Risk if left unfixed:**  
Bad configs pass validation and fail only under live traffic.

### RELIABILITY-003 Medium RELIABILITY - Token fetchers ignore configured timeouts

**Classification:** [Fact]  
**Area:** OAuth refresh, GCP service-account minting

**Why this matters:**  
Provider request clients honor configured connect/read timeouts, but auth token HTTP clients use plain `reqwest::Client::new()`. A stuck token endpoint can block request execution outside the gateway's upstream timeout policy.

**Evidence:**
- `crates/sb-credentials/src/refresh.rs:48` constructs `reqwest::Client::new()`.
- `crates/sb-credentials/src/refresh.rs:80` sends the refresh request.
- `crates/sb-credentials/src/service_account.rs:74` constructs `reqwest::Client::new()`.
- `crates/sb-credentials/src/service_account.rs:98` sends the token exchange request.
- Upstream request clients use configured timeouts in `crates/sb-adapters/src/egress.rs:188`.

**What is inconsistent / illogical:**  
The main provider transport has timeout policy; the auth transport does not, even though auth refresh is in the request path.

**Likely root cause:**  
Auth refresh was implemented in `sb-credentials` without reusing server timeout config.

**Recommended fix direction:**  
Pass timeout settings into the credential resolver's production fetchers, or provide an `AuthHttpPolicy` shared with egress. Add tests with a stalled token endpoint.

**Risk if left unfixed:**  
Requests can hang on credential refresh while the user expects timeout-bound behavior.

### FLOW-001 Medium FLOW - Dashboard is unusable when auth is enabled

**Classification:** [Fact]  
**Area:** embedded dashboard, auth middleware

**Why this matters:**  
The dashboard is advertised as a control plane, but the security model protects all `/v1/*` and `/cp/v1/*` APIs when a key is configured. The dashboard shell remains public and has no way to send `Authorization: Bearer ...`, so it cannot fetch or patch protected data.

**Evidence:**
- `crates/sb-server/src/lib.rs:627` exempts `/` and `/health` from auth.
- `crates/sb-server/src/dashboard.html:88` fetches APIs without auth headers.
- `crates/sb-server/src/dashboard.html:93` patches `/v1/runtime` without auth headers.
- `README.md:191` says every endpoint except `/` and `/health` requires a key when configured.

**What is inconsistent / illogical:**  
The UI remains reachable but cannot operate the protected control plane it is meant to visualize.

**Likely root cause:**  
The dashboard was added as a local-only convenience before endpoint auth became comprehensive.

**Recommended fix direction:**  
Either put the dashboard behind auth too, or add a local browser token-entry/session model that never stores the key insecurely. At minimum, show a clear "API key required" state instead of generic fetch errors.

**Risk if left unfixed:**  
Users think the dashboard is broken as soon as they secure the gateway.

### DOCS-001 Medium DOCS - Docs contradict the implementation state

**Classification:** [Fact]  
**Area:** README, docs current-state

**Why this matters:**  
Agents and users rely on docs to know what is built, out of scope, and safe. The docs currently contain both "built" and "out of scope" claims for the same surfaces.

**Evidence:**
- `README.md:80` describes idempotency as built.
- `README.md:84` describes multi-tenancy and quotas as built.
- `README.md:223` says the v1 surface is built.
- `README.md:228` then says `multi-tenancy/RBAC` and `idempotency/quota state` are out of scope.
- `docs/CURRENT-STATE.md:45` says multi-tenancy/RBAC, dashboard UI, persistence/DB, and Bedrock are out of v1, while the code and README now include those surfaces.

**What is inconsistent / illogical:**  
The repo's public and private current-state documents do not agree with the actual code.

**Likely root cause:**  
Rapid implementation outpaced current-state and status doc maintenance.

**Recommended fix direction:**  
Make `README.md` status precise: distinguish built local/team features from hosted-hardening gaps. Update or archive `docs/CURRENT-STATE.md` after each architecture tranche.

**Risk if left unfixed:**  
Future agents build from stale assumptions, and users cannot tell which warnings still matter.

## 7. Missing safeguards

- Redaction tests for `api_keys[].key`, control-plane resource projections, draft reads, and CLI `config show`.
- Semantic config validator for all cross references and duplicate IDs.
- Tenant-scoped idempotency keys.
- Streaming error finalization that distinguishes upstream error from client abort.
- Hedge attempt accounting for winners, losers, cancellations, and total hedge failure.
- Runtime-owned embeddings path with trace/ledger/budget/admission/plugin parity.
- Hosted-mode outbound URL policy and SSRF defenses.
- Configured timeouts for OAuth and service-account token exchanges.
- Clear at-rest security policy for durable drafts.
- CI gate that actually fails on invalid examples.
- Dashboard authentication UX.
- `cargo audit` availability in local developer workflow; the command was not installed in this environment.

## 8. Remediation order

1. Fix `SECURITY-001` immediately: redact `api_keys[].key`, add tests, and audit all config projections.
2. Add `Config::validate_semantics()` and wire it into serve/reload/draft/CLI validation; this addresses `DATA-001`, `CONTRACT-001`, and helps prevent another redaction-class miss.
3. Move `/v1/embeddings` into the runtime or temporarily narrow the product claim around embeddings.
4. Repair streaming finalization so trace/health/circuit success reflects terminal stream outcome.
5. Make hedging accounting explicit before encouraging it as a spend-aware feature.
6. Split the public example config so CI can validate it without `|| true`.
7. Decide the durable-draft secret policy and update README/SECURITY accordingly.
8. Implement hosted-mode SSRF controls before any hosted/team deployment.
9. Add dashboard auth UX after the API trust boundary is fixed.

## 9. Fast wins vs structural repairs

Fast wins:
- Add `"key"` context-aware redaction for `api_keys` and regression tests.
- Remove `|| true` from CI after making `switchback.example.yaml` validate.
- Update README status and `docs/CURRENT-STATE.md`.
- Add timeout-configured reqwest clients for OAuth/service-account token exchange.
- Add dashboard "API key required" UX.

Structural repairs:
- Central semantic config validation.
- Runtime-owned embeddings path.
- Streaming terminal-outcome accounting.
- Hedge accounting/reservation policy.
- Hosted-mode egress/SSRF policy.

Things to simplify or delete:
- Do not keep `switchback.example.yaml` as both a live runnable config and a feature encyclopedia. Split those roles.
- Do not keep duplicate execution loops. Embeddings should not remain a special server-side orchestrator.
- Do not expose richer privacy modes as operationally meaningful until they enforce behavior.

## 10. Open questions / unknowns

- Is hosted/team deployment an active near-term target, or should the repo explicitly label hosted mode as blocked until SSRF/idempotency/tenant isolation hardening lands?
- Should durable control-plane drafts support inline secrets at all, or should drafts require vault/env references?
- Should hedging be default-disabled forever unless the operator opts into "possible duplicate billing" per provider/account?

## Verification performed

- `cargo test --workspace` passed.
- `cargo clippy --workspace --all-targets` passed.
- `cargo fmt --all --check` passed.
- `cargo build --workspace --release` passed.
- `cargo build -p sb-server --features wasm,otel` passed.
- `cargo run -q -p sb-server -- config validate --config config/quickstart.yaml` passed.
- `cargo run -q -p sb-server -- config validate --config config/switchback.example.yaml` failed with missing Bedrock AWS env and missing Anthropic API key; CI currently ignores this with `|| true`.
- Live quickstart smoke passed: `/health`, non-streaming `/v1/chat/completions`, and streaming `/v1/chat/completions` against `mock/echo`.
- `cargo audit` could not be run because `cargo-audit` is not installed locally.
