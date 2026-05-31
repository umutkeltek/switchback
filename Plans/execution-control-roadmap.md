# Switchback Execution Control Roadmap

Switchback should not compete as a broader router app. It should win as the
Rust execution kernel and control plane for governed LLM calls.

The working category:

```text
AI execution gateway
```

The product promise:

```text
One endpoint for model execution, routing, credentials, failover, policy,
cost control, and traces.
```

## Strategic Position

Switchback is not a Rust clone of OmniRoute or 9router. It should support the
same useful user outcomes through cleaner primitives:

| User outcome | Switchback primitive |
| --- | --- |
| Automatic model selection | Execution profiles |
| Ordered model fallback | Route profiles |
| Multi-account routing | Credential pools |
| Sticky account behavior | Session affinity |
| Quota avoidance | Quota-aware admission |
| Token savings | Context budget optimizer |
| Provider health view | Runtime state |
| Dashboard | Operator console |
| MCP tools | Agent-operable control plane |
| Model aliases | Virtual model contracts |
| Usage database | Execution ledger |

The positioning line:

```text
Other routers optimize access. Switchback optimizes execution.
```

## Product Rules

- Keep the core provider-agnostic. Provider wire formats stay at the protocol
  and adapter edges.
- Every request must produce an explainable `RouteDecision`.
- Fallback is legal only before the first streamed byte is committed.
- Secrets remain leases and are never logged or serialized through control
  surfaces.
- Metadata-only traces are the default. No prompt or response body capture by
  default.
- Prefer official provider APIs, user-owned credentials, explicit egress policy,
  and typed adapters.
- Do not add MITM, browser-cookie providers, TLS fingerprint mimicry, cloaking,
  official-client spoofing, or arbitrary JavaScript hooks.

## First-Class Objects

### Provider Profile

A provider profile describes an upstream execution surface:

```text
provider id
protocol codec
auth method
capabilities
model catalog
cost model
egress policy
health state
quota model
```

OpenAI-compatible providers should remain mostly config. New wire formats should
enter through codecs. New auth methods through signers. New stream framing
through transports.

### Credential Pool

A credential pool owns account selection and account state:

```text
provider id
account ids
selection strategy
session affinity
cooldowns
model lockouts
quota snapshots
vault-backed secrets
tenant/account policy tags
```

The router sees compiled non-secret availability state. It never sees secrets.

### Execution Profile

An execution profile is the user-facing model contract:

```text
auto
auto/cheap
auto/fast
auto/coding
auto/private
auto/large-context
team/default
local/code
```

Profiles compile into normal route profiles and routing policies. They must not
be hidden magic paths.

### Policy

Policies define what is allowed before routing optimizes:

```text
max cost per request
max cost per time window
allowed providers
allowed credential pools
required provider class
required tool support
required schema support
private workload restrictions
egress restrictions
retry and fallback limits
```

### Runtime State

Runtime state is the adaptive input to routing:

```text
provider health
account health
model health
quota snapshots
rate-limit reset times
latency EWMA
TTFT EWMA
recent error rate
last-known-good target
config revision
pricing revision
plugin status
egress status
```

## Roadmap

### P1: Execution Profiles

Add first-class route presets:

```text
auto
auto/cheap
auto/fast
auto/coding
auto/private
auto/large-context
```

Expected behavior:

- `auto/cheap` ranks by cost first, then health and latency.
- `auto/fast` ranks by TTFT for streaming requests and total latency for
  non-streaming requests.
- `auto/coding` prefers models tagged or profiled for coding while preserving
  required tool and schema capabilities.
- `auto/private` rejects aggregator, promo, unknown-retention, or untrusted
  provider lanes when a private policy applies.
- `auto/large-context` ranks by context fit before cost or latency.

Likely files:

```text
crates/sb-core/src/config.rs
crates/sb-core/src/routing.rs
crates/sb-router/src/lib.rs
crates/sb-runtime/src/lib.rs
crates/sb-server/src/cp.rs
crates/sb-server/tests/routing_auto.rs
```

Acceptance:

- Profiles compile to explicit route plans.
- Route preview works without executing upstream calls.
- `RouteDecision` names the selected profile.
- Rejected candidates include reasons.
- Tests cover cheap, fast, coding, private, and large-context behavior.

### P2: Scored Routing

Add `strategy: score` beside deterministic ordering.

Hard filters run first:

```text
protocol capability
streaming support
tools support
JSON schema support
context window
tenant policy
max price
account availability
```

Then scored factors run:

```text
health
account_availability
cost
ttft
total_latency
context_fit
task_fit
quota_headroom
recent_error_rate
stability
```

Example decision shape:

```json
{
  "profile": "auto/coding",
  "strategy": "score",
  "selected": "anthropic/claude-sonnet",
  "scores": [
    {
      "target": "anthropic/claude-sonnet",
      "score": 0.87,
      "factors": {
        "health": 1.0,
        "account_availability": 0.8,
        "ttft": 0.7,
        "cost": 0.4,
        "context_fit": 1.0,
        "task_fit": 0.9
      }
    }
  ]
}
```

Acceptance:

- Scoring math is visible in route decisions and traces.
- A hard rejection is never hidden as a low score.
- The selected target can be explained from factors alone.
- Existing deterministic routing behavior remains available.

### P3: Credential Pools And Session Affinity

Introduce explicit credential pools without blurring router and credential
responsibilities.

Config direction:

```yaml
credential_pools:
  anthropic-team:
    provider: anthropic
    strategy: sticky_round_robin
    session_affinity: true
    accounts:
      - team-main
      - team-backup
```

Session affinity sources:

```text
x-switchback-session-id
x-codex-session-id
x-session-id
metadata.session_id
```

Affinity breaks when:

```text
account fails
model is locked out
quota is exhausted
budget threshold is reached
max sticky age is reached
route policy changes
```

Acceptance:

- Same session prefers the same provider/account/model when still valid.
- Account failure releases affinity and records the reason.
- Router still sees only non-secret availability state.
- Tests cover affinity hit, failover, expiry, and policy change.

### P4: Runtime State And Resilience Visibility

Make health state visible and operator-readable.

Expose:

```text
provider circuit state
account cooldowns
model lockouts
quota snapshots
reset times
retry-after
last error class
healthy account count
recent failure counts
latency EWMA
TTFT EWMA
```

Endpoints:

```text
GET /v1/health
GET /cp/v1/runtime-state
POST /cp/v1/runtime-state/reset-lockout
```

Acceptance:

- Provider outage, account cooldown, auth revocation, quota exhaustion, and
  model lockout are distinct states.
- Reset endpoints are authenticated and audited.
- Health output remains metadata-only.
- Route preview can explain skipped accounts and demoted targets.

### P5: Quota Preflight

Add provider-specific quota probes through official APIs only.

Concept:

```rust
trait QuotaProbe {
    async fn probe(&self, account: AccountRef) -> Result<QuotaSnapshot, QuotaError>;
}
```

Snapshot fields:

```text
provider_id
account_id
model_id optional
remaining_requests
remaining_tokens
reset_at
source
observed_at
confidence
```

Acceptance:

- Quota state is stored as non-secret runtime state.
- Stale quota data is labeled with confidence or age.
- Routing can use quota headroom without accessing credentials.
- Probes fail soft unless a policy requires fresh quota state.

### P6: Agent-Operable Control Plane

Build a small MCP surface over the existing control plane.

Tools:

```text
switchback_get_health
switchback_preview_route
switchback_list_routes
switchback_list_providers
switchback_get_usage
switchback_get_trace
switchback_list_tenants
switchback_admission_preview
switchback_reload_config
switchback_explain_last_failure
switchback_get_provider_state
switchback_reset_lockout
```

Acceptance:

- MCP tools call existing HTTP/control-plane APIs instead of creating a second
  control path.
- Mutating tools require authentication.
- Tool responses are metadata-only and redact secrets.
- Route preview and trace explanation are useful from an agent workflow.

### P7: Context Budget Optimizer

Expand `sb-compress` from a safe compression seam into a useful coding-agent
optimizer.

Filters:

```text
git_diff
grep
tree
ls
read_numbered_file
build_log
test_log
search_result_list
stack_trace
large_json
duplicate_log_lines
```

Invariants:

```text
opt-in
never grow
never empty
lossiness recorded
filter names traced
original content not stored by default
```

Acceptance:

- Each filter has golden tests.
- Compression metadata records before/after bytes and estimated tokens saved.
- A failed filter passes through the original content.
- Tool-result compression remains separate from prompt storage.

### P8: Operator UX And Onboarding

Build boring setup flows before product sprawl.

CLI targets:

```text
switchback init
switchback provider add openai
switchback provider test openai
switchback route add auto/coding
switchback dashboard
switchback pricing import
switchback doctor
```

Operator console views:

```text
route decision viewer
route preview form
provider health
account cooldowns
usage by tenant/provider/model
live traces
config revision history
plugin status
quota reset calendar
pricing editor
```

Acceptance:

- A new local user can add one provider and complete a mock or real request
  without hand-editing every config field.
- Dashboard remains an operator console, not a general AI workspace.
- Provider setup never encourages unofficial browser-cookie or cloaking flows.

### P9: Store Migrations And Golden Protocol Corpus

Add migration discipline as persisted state grows.

Likely tables:

```text
route_decisions
provider_health
account_state
model_lockouts
quota_snapshots
latency_samples
runtime_events
```

Protocol fixtures:

```text
OpenAI Chat -> IR -> Anthropic
Anthropic -> IR -> OpenAI
Gemini -> IR -> OpenAI
Responses -> IR -> Chat
tool calls
tool results
reasoning
usage
JSON schema
schema downlevel warnings
streaming errors
images rejected or preserved explicitly
```

Acceptance:

- Persisted schema changes are versioned and tested.
- Fixtures cover stream and non-stream paths.
- Lossy protocol conversions produce explicit warnings.
- No fixture requires raw production prompt/response storage.

## Next Three Commits

### Commit 1: `feat: add auto execution profiles`

Scope:

```text
crates/sb-core/src/config.rs
crates/sb-core/src/routing.rs
crates/sb-router/src/lib.rs
crates/sb-runtime/src/lib.rs
crates/sb-server/tests/routing_auto.rs
```

Goal:

```text
model="auto/cheap" routes cheapest
model="auto/fast" routes lowest TTFT or latency
model="auto/coding" prefers coding-capable targets
```

Acceptance:

- Profile resolution is explicit and test-covered.
- `RouteDecision` includes profile id and route reasoning.
- No provider wire-format leaks into `sb-core`.

### Commit 2: `feat: explain route score factors`

Scope:

```text
crates/sb-core/src/routing.rs
crates/sb-router/src/lib.rs
crates/sb-trace/src/lib.rs
crates/sb-server/tests/traces.rs
```

Goal:

```text
RouteDecision includes per-candidate score factors.
```

Acceptance:

- Factors are stable, serializable, and redaction-safe.
- Traces can answer why the selected target won.
- Deterministic routing still emits useful non-score reasoning.

### Commit 3: `feat: expose account and model lockout state`

Scope:

```text
crates/sb-credentials/src/availability.rs
crates/sb-credentials/src/resolver.rs
crates/sb-server/src/controlplane.rs
crates/sb-server/tests/health.rs
```

Goal:

```text
/v1/health or /cp/v1/runtime-state exposes provider/account/model availability.
```

Acceptance:

- Cooldown, quota exhaustion, auth revocation, and provider breaker states are
  distinct.
- Reset-at and retry-after are visible when known.
- Responses contain no secrets.
- Health tests cover lockout visibility.

## Non-Goals

- Do not build a bypass router.
- Do not add browser-cookie provider access.
- Do not add TLS fingerprint spoofing.
- Do not add MITM or official-client spoofing.
- Do not run arbitrary user JavaScript in-process.
- Do not chase provider count at the expense of adapter discipline.
- Do not make the dashboard the product before the execution gateway is solid.
- Do not store raw prompts or responses by default.

## Vocabulary

Use these names in docs, code, and product language:

```text
execution gateway
execution profile
provider profile
credential pool
route planner
resilience planner
runtime state
context budget optimizer
policy lane
admission control
usage ledger
trace ledger
operator console
agent-operable control plane
hybrid local/cloud routing
```

Avoid these names:

```text
free tier router
token saver router
account rotator
stealth
cloaking
spoofing
bypass
unlimited
subscription pooling
MITM
fingerprint
```

The practical formula:

```text
Outcome parity
+ architecture superiority
+ positioning separation
= Switchback's wedge
```
