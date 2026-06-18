# Full-System Audit Report

Date: 2026-06-18

## 1. Executive Summary

Switchback is conceptually strong: the Rust gateway has a clean canonical IR, explainable routing, typed credential leases, runtime snapshot pinning, usage/tracing, durable state, semantic config validation, and a real test suite.

The weakest area is not the core engine anymore. It is the native-client control plane around `sb`: Codex/Claude local auth files, `CODEX_HOME` session pools, transparent taps, account selection, and network interruptions. That layer has to make mutable first-party client state feel deterministic. The user-visible bug came from exactly that boundary.

Top current problems:

| ID | Severity | Status | Short Title |
|---|---|---|---|
| STATE-001 | Critical | Mitigated | Mixed-account shared Codex sessions could swap live auth under another run |
| STATE-002 | High | Mitigated | Named Codex accounts now auto-isolate unless shared is explicit |
| RELIABILITY-001 | High | Partly mitigated | Network path changes kill native streams; recovery is not productized |
| ARCH-001 | Medium | Open | Native identity/session state is split between CLI profiles and engine profiles |
| CONTRACT-001 | Medium | Open | Tap vs relay guarantees are technically clear but not failure-proof for users |
| RELIABILITY-002 | Medium | Open | Hedge losers are visible but still not cost-accounted as possible upstream work |
| DOCS-001 | Medium | Open | Native relay/token-adapter/tap wording still needs one canonical story |
| PROCESS-001 | Low | Open | Full example config validates only with placeholder provider env |

Verdict: the engine is solid enough to keep building on. The product risk is that native-client support can feel magical: it observes real clients but cannot fully control their local auth, their reconnect behavior, or a network path change. The fix is not more providers. It is a crisper identity/session state machine, safer defaults, and recovery UX.

## 2. Inferred Product Model

Target user: local power users, solo builders, small teams, and future team operators who want all AI traffic observable and routeable without rewriting clients.

Core value proposition: one local-first gateway that can run OpenAI/Anthropic-compatible workloads through canonical routing while also observing native coding clients through verbatim taps.

Main workflows:

- Configure providers/accounts/routes/tenants.
- Run regular API clients through `/v1/*`.
- Run Codex/Claude/opencode/pi through `sb`.
- Observe traces, usage, health, route decisions, native-client readiness.
- Hot-reload config and inspect control-plane state.

Core state boundaries:

- Engine provider accounts: durable-ish gateway config/vault/accounts.
- Native client auth: first-party client files such as `~/.codex/auth.json`.
- CLI session profiles: `CODEX_HOME` and `CLAUDE_CONFIG_DIR` directories.
- Transparent taps: verbatim pass-through, observed but not routed.
- Relay/runtime: canonical IR path with routing/fallback/budget/ledger.

Critical assumption: users must be able to tell which layer owns account selection. In shared Codex mode, that was not true enough.

## 3. System Map

Major modules:

- `sb-core`: provider-agnostic IR/config/catalog/error types and semantic config validation.
- `sb-protocols`: OpenAI/Responses/Anthropic/Gemini translation.
- `sb-router`: deterministic route planning and `RouteDecision`.
- `sb-credentials`: gateway account resolution, leases, vault/OAuth refresh, availability locks.
- `sb-adapters`: codec/signer/transport execution.
- `sb-runtime`: pinned-snapshot execution state machine, fallback, streaming finalization, usage/traces, embeddings.
- `sb-server`: Axum HTTP edge, handlers, control plane, dashboard, CLI subcommands, native/tap setup.
- `sb-store`: SQLite-backed revisions/audit/usage/idempotency/admission/tenant slots.
- `cli/sb`: user-facing launcher and local native-client profile/account/session orchestration.

Important request paths:

- Relay/API path: client JSON -> protocol parser -> canonical `AiRequest` -> runtime route/account/adapter -> canonical stream -> protocol egress.
- Tap path: native client -> local tap -> upstream vendor, with headers/body forwarded verbatim and metadata trace recorded.
- `sb codex --sessions shared`: CLI swaps `~/.codex/auth.json`, then launches Codex with `CODEX_HOME=~/.codex`.
- `sb codex --sessions separated`: CLI launches Codex with account-specific `CODEX_HOME`, so auth and sessions are isolated.
- `sb codex --account NAME` for a named account: now auto-selects separated mode unless `--sessions shared` is explicit.

## 4. The Concrete Bug: What Happened

A reported failure mode was: start one Codex session through `sb` as account A, then start another shared-mode Codex session as account B. The CLI swapped `~/.codex/auth.json` to B while A was still running.

Codex does not bind sessions to “Switchback account names.” It binds sessions and auth to `CODEX_HOME`. In shared mode, every account uses the same `CODEX_HOME`: `~/.codex`. So the isolation was temporal, not concurrent. Another live Codex process could later read/refresh/use the changed auth file and fail against the backend.

The current implementation mitigates this:

- `cli/sb` tracks active shared runs in `~/.config/switchback/codex-auth/.runs`.
- A lock directory guards auth swaps.
- A different-account shared launch is refused while a shared run is active.
- Named accounts auto-use separated sessions by default, so concurrent agents do not enter the shared pool accidentally.
- `sb sessions status` and `sb status` now show active shared runs.
- Codex account names are validated before profile paths are built.
- CI now runs shell CLI tests.

The key design truth: shared mode can be native-compatible or concurrent-safe, but not both for different accounts. Concurrent different-account agents now get separated mode by default; shared is an explicit native-pool opt-in.

## 5. Findings

### STATE-001 Critical - Mixed-account shared Codex runs could mutate live auth

Classification: [Fact]
Area: `cli/sb`, native Codex launcher
Status: mitigated

Evidence:

- `cli/sb` shared mode uses `CODEX_HOME="$SB_MAIN_HOME"` where `SB_MAIN_HOME` defaults to `~/.codex`.
- Before this patch, `_activate_shared` copied the selected account credential into `~/.codex/auth.json` without tracking live shared runs.
- The current patch adds `SB_RUNS_FILE`, `SB_LOCK_DIR`, active-run pruning, conflict detection, registration, and unregistering.
- `cli/tests/sb_codex_shared_sessions.zsh` now reproduces and guards the failure.

Impact:

Another agent could stop or disconnect because its process was still alive but its backing auth file had been swapped to another account.

Recommendation:

Keep the guard. Treat shared mode as single-active-account. Do not add automatic account rotation to shared native Codex.

### STATE-002 High - Named accounts need separated sessions by default

Classification: [Inference]
Area: product default / CLI UX
Status: mitigated

Evidence:

- `SB_SESSION_MODE` still defaults to `shared` for native compatibility.
- `cli/sb` now computes an effective session mode per run: named Codex accounts auto-use `separated` when no explicit `--sessions` is passed.
- Explicit `--sessions shared` remains available and guarded by the shared-run registry.

Impact:

Concurrent named-account agents no longer require the user to remember the internal `CODEX_HOME` rule. The risky mode is still available for deliberate native-pool resume, but it is no longer the accidental default for named accounts.

Recommendation:

Keep this policy. Do not reintroduce automatic account switching inside the shared pool.

### RELIABILITY-001 High - Network path changes kill native streams; recovery is not productized

Classification: [Fact + Inference]
Area: tap streaming, WARP/VPN changes, CLI status
Status: partly mitigated

Evidence:

- `crates/sb-server/src/tap.rs` forwards tap responses as a raw byte stream. It records `upstream_stream_error`, `upstream_closed_before_terminal`, and `client_aborted`.
- `crates/sb-server/src/tap.rs` tests truncated SSE detection.
- A reported failure showed `Stream disconnected before completion: Transport error: network error: error decoding response body` after Cloudflare WARP was toggled.
- This patch makes `sb status` surface the latest tap stream warning from local traces.

Impact:

Once an SSE stream has started, Switchback cannot silently reconnect and stitch the native client’s response together. A VPN/interface change can break the TCP/TLS stream. The correct recovery is usually to restart/resume the affected native client session, but the product did not say that clearly.

Recommendation:

Add a native-stream recovery runbook and a stronger status surface:

- Show recent tap warning, affected model, request id, and suggested action.
- Add `sb doctor network` or `sb native doctor --network` that reports WARP/VPN/interface changes and recent stream truncations.
- Document that tap streams are verbatim and not replayable after first byte.

### ARCH-001 Medium - Native identity/session state is split across two control planes

Classification: [Fact]
Area: CLI/native setup/engine config

Evidence:

- `cli/sb` owns Codex profile directories, `codex-auth` registry, shared/separated session mode, and active run tracking.
- `crates/sb-server/src/setup_cli.rs` and `native_cli.rs` own native token-source adapters, client profiles, native status, and relay readiness.
- Engine accounts are provider/account leases; tap accounts are the native client’s own auth files.

Impact:

The implementation is defensible, but the product model is easy to confuse: “account” can mean a ChatGPT login profile, a Switchback provider account, a client profile, or a tenant key.

Recommendation:

Create one canonical “Native Client State Model” doc and make `sb status` point to it. Use four distinct words everywhere:

- profile: local client home/config directory.
- credential: auth token file or vault/env secret.
- session pool: conversation/history store.
- gateway account: provider account selected by `sb-credentials`.

### CONTRACT-001 Medium - Tap vs relay guarantees need sharper user-facing boundaries

Classification: [Fact]
Area: docs, mode picker, troubleshooting

Evidence:

- Tap forwards native headers/body unchanged and does not use canonical IR, gateway credential leases, or engine fallback.
- Relay uses the runtime and can route/fallback/budget, but is not verbatim native traffic.
- `cli/README.md` explains this, but the operational consequences are still easy to miss.

Impact:

Users can expect route/fallback behavior from tap because it is “through Switchback,” even though tap is intentionally pass-through. That creates confusion during outages.

Recommendation:

Add a small mode contract table to `sb modes`:

- Tap: observed, native auth, no Switchback retry after stream commit, no account fallback.
- Relay: routed, fallback/budget/ledger, request is reissued by gateway.
- Native: unobserved escape hatch.

### RELIABILITY-002 Medium - Hedge losers are visible but not fully spend-accounted

Classification: [Inference]
Area: `sb-runtime::hedge`

Evidence:

- `crates/sb-runtime/src/hedge.rs` races multiple non-streaming candidates and drops losers after first success.
- `crates/sb-runtime/src/execute.rs` records canceled hedge losers as `hedge_cancelled`.
- Usage/cost is still recorded from the winner’s response usage.

Impact:

If a loser request reached the upstream before local cancellation, the provider may still bill or do work. The trace now shows the canceled loser, but usage is not billing-grade for hedged duplicate work.

Recommendation:

Keep hedging opt-in. Add a trace/billing flag such as `possibly_billed=true` on canceled losers, and exclude hedging from any future billing-grade claim unless provider cancellation semantics are known.

### DOCS-001 Medium - Native relay/token-adapter/tap story is still too nuanced

Classification: [Fact + Inference]
Area: README, CLI docs, native setup docs

Evidence:

- README distinguishes native token-source adapters from first-party subscription relay.
- `cli/README.md` focuses on tap/shared/separated behavior.
- `native_cli.rs` has relay readiness/audit/status language and still notes partial conformance for full native relay.

Impact:

The pieces are technically correct, but a user can still ask “which account is being used?” and get three different answers depending on mode.

Recommendation:

Write one short decision doc:

- `tap`: client owns auth, Switchback observes.
- `native token adapter`: gateway leases local token source, not first-party verbatim.
- `native relay`: gateway reissues native-shaped calls; conformance-gated.
- `shared/separated`: only about local Codex `CODEX_HOME` and session history.

### PROCESS-001 Low - Full example config is not a zero-env validation example

Classification: [Fact]
Area: examples / CI

Evidence:

- `config/quickstart.yaml` validates locally without keys.
- `.github/workflows/ci.yml` validates `config/switchback.example.yaml` with placeholder provider env vars.
- Locally, `config/switchback.example.yaml` fails without AWS env because it includes Bedrock.

Impact:

This is no longer a CI false gate, but onboarding can still feel inconsistent if a user validates the full example without reading the provider-env assumption.

Recommendation:

Keep `quickstart.yaml` as the zero-env path. Rename or comment the full example as `switchback.full.example.yaml`, or print a clearer validation hint when only provider env vars are missing.

## 6. Recently Resolved Items From The Older Audit

The old `AUDIT_REPORT.md` contained several findings that are now fixed or materially improved:

- Tenant API key redaction now has tests in `controlplane.rs`.
- Semantic config validation exists in `Config::semantic_problems()` and is used by runtime/server/config paths.
- `/v1/embeddings` now goes through `sb-runtime::Engine::execute_embeddings`.
- Streaming finalization now distinguishes clean, upstream error, and client abort in `sb-runtime::stream`.
- Idempotency keys are scoped by endpoint/tenant/project and hashed before persistence.
- Private-network URL blocking exists behind `server.block_private_networks`.
- CI no longer ignores full example validation; it supplies placeholder env.
- Shell CLI tests are now included in CI.

## 7. Recommended Next Moves

1. Add `sb native doctor --network` or equivalent to explain WARP/VPN stream failures and recovery steps.
2. Write the native-client state model doc and link it from `sb sessions status`, `sb modes`, and `cli/README.md`.
3. Add a run-level field to traces for “possible duplicate upstream work” in hedge cancellation.
4. Add more shell CLI tests around `sb status`, active-run cleanup, and native network warnings.
5. Keep provider expansion paused until native account/session recovery is boring.

## 8. Verification Performed In This Pass

- `zsh -n cli/sb` passed.
- `for test in cli/tests/*.zsh; do zsh "$test"; done` passed.
- `git diff --check` passed.
- `cargo fmt --all --check` passed.
- `cargo clippy --workspace --all-targets` passed.
- `cargo test --workspace` passed.
- `cargo run -q -p sb-server -- config validate --config config/quickstart.yaml` passed.
- `ANTHROPIC_API_KEY=ci-placeholder OPENAI_API_KEY=ci-placeholder AWS_ACCESS_KEY_ID=ci-placeholder AWS_SECRET_ACCESS_KEY=ci-placeholder AWS_REGION=us-east-1 cargo run -q -p sb-server -- config validate --config config/switchback.example.yaml` passed.
- Targeted Rust checks passed:
  - `cargo test -q -p sb-runtime streaming_precommit_error_falls_over_before_client_commit`
  - `cargo test -q -p sb-runtime validate_config_rejects_api_keys_for_unknown_tenants`
  - `cargo test -q -p sb-server tap_warns_when_sse_stream_closes_before_terminal_event`
  - `cargo test -q -p sb-server hedge_returns_the_fast_providers_response`
