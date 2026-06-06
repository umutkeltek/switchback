# Native Relay Spec

This spec is product-level Switchback behavior. Local machine lane names and shell
wrappers are operator setup details and do not define the product contract.

## Purpose

Switchback should be able to observe and control first-party coding-client
traffic without turning private native auth stores into generic public API keys.
The relay must keep three modes distinct:

| Mode | Traffic path | Switchback tracking | Promotion state |
|---|---|---|---|
| Direct native | Client talks directly to its vendor | Imported metadata only | Available outside Switchback |
| Switchback ingress | Client talks to Switchback; Switchback uses configured provider accounts | Full route/attempt/usage trace | Available today |
| Native relay | Client talks to Switchback; Switchback leases local native OAuth and calls the first-party upstream | Full route/attempt/usage trace | Partial implementation, fail-closed |

The product promise is not "use any token anywhere." The promise is: one local
gateway can route official-compatible traffic, lease credentials at the last
responsible moment, explain every routing decision, and record metadata without
logging prompts, responses, or secrets by default.

## Scope

In scope:

- Codex through OpenAI Responses-compatible ingress and `codex_native_relay`.
- Claude Code through Anthropic Messages-compatible ingress and
  `claude_code_native_relay`.
- Read-only native token-source leases from documented local files or explicit
  environment variables.
- Metadata-only trace, usage, latency, cost, and error recording for relay
  traffic that enters Switchback.
- A reversible canary profile/wrapper for each client after conformance is
  green.

Out of scope:

- Browser cookie replay, root CA interception, MITM, or host cloaking.
- Turning subscription credentials into team/shared hosted API credentials.
- Silent fallback from native relay to scout/free/API routes.
- Raw prompt/response retention by default.
- Refresh-token write-back to native client stores. If write-back is needed, it
  must be a separate opt-in spec.

## Current Implementation

Implemented:

- Provider kinds: `codex_native_relay`, `claude_code_native_relay`.
- Account auth kinds: `codex_oauth`, `claude_code_oauth`.
- Codex native relay HTTP Responses slice, including `chatgpt-account-id`.
- Claude Code native relay non-stream Messages slice, including native
  attribution header.
- Fake-upstream server tests for both relay paths.
- `switchback setup native-relay plan`, `audit`, and sanitized `capture`.

Not promotion-green:

- The fixture manifest is still `partial_capture`.
- The audit command reports `relay_implemented: false` until the full fixture
  gate is satisfied.
- Codex WebSocket relay conformance remains open.

## Conformance Matrix

Relay promotion requires every required fixture row for both clients. The matrix
has 16 cells: 8 fixture categories times 2 clients. As of this spec, 2 cells are
captured: non-stream Codex and non-stream Claude Code.

| Fixture | Codex | Claude Code | Required outcome |
|---|---:|---:|---|
| `model_list` | Missing | Missing | Models endpoint shape is known and non-secret |
| `non_stream_request_response` | Captured | Captured | Request, response, headers, and status are fixture-backed |
| `stream_request_first_byte_and_finish` | Missing | Missing | First byte, deltas, terminal event, and usage are fixture-backed |
| `tool_call_and_tool_result` | Missing | Missing | Tool call ids/results round-trip or are explicitly unsupported |
| `token_count` | Missing | Missing | Count endpoint behavior is known or fail-closed |
| `expired_token_or_refresh_failure` | Missing | Missing | Auth failure class and retry/fallback behavior are deterministic |
| `client_abort_before_first_byte` | Missing | Missing | No fallback after unsafe boundary; trace records abort |
| `client_abort_after_first_byte` | Missing | Missing | Stream guard closes cleanly; trace records abort |

Promotion criteria:

- All 16 conformance cells have sanitized fixtures or a documented fail-closed
  unsupported result.
- `cargo test -p sb-protocols --test native_relay_fixtures` passes.
- `cargo test -p sb-server --test native_relay` passes.
- `switchback setup native-relay audit --json` reports relay-ready status.
- A canary request records a trace with `client_profile`, route decision,
  selected provider/account, usage, latency, and no leaked secret material.

## OAuth Token Source Contract

Read order:

- Codex: `CODEX_ACCESS_TOKEN`, configured vault secret, then
  `${HOME}/.codex/auth.json` at `/tokens/access_token`.
- Claude Code: `CLAUDE_CODE_OAUTH_TOKEN`, configured vault secret, then
  `${HOME}/.claude/.credentials.json` at `/claudeAiOauth/accessToken`.

Rules:

- Tokens are read at lease time, not copied into product config.
- Token values are never printed by setup, audit, doctor, config, trace, or
  control-plane output.
- Native relay accounts are personal/local scope unless a provider explicitly
  allows broader use.
- Missing or malformed native auth is an account-resolution failure, not a
  reason to fall back to scout/free routes.
- Refresh-token persistence into native stores is forbidden in this spec.

## Telemetry Contract

For traffic that enters Switchback, the relay records:

- `request_id`, `client_profile`, `client_protocol`, inbound model, route name,
  selected target, fallback list, and rejected candidates.
- Provider id, account id, egress id, status, latency, time-to-first-token where
  applicable, usage, cost, and error class.
- Session id headers when present, using the profile's known header list.

The relay does not record by default:

- Raw prompts.
- Raw responses.
- Tool arguments or tool outputs.
- OAuth tokens, cookies, account ids carried as secret headers, or request
  headers not explicitly approved for metadata.

Direct-native traffic that bypasses Switchback can only be imported as
metadata-only records from local client logs. Imported records must be labeled as
`transport=client_native_import` so they are not confused with gateway traces.

## Canary Lane Contract

Native relay adoption must be additive and reversible:

- Product route ids: `codex-native` and `claude-code-native`.
- Suggested local wrapper names: `codex-switchback-native` and
  `claude-switchback-native`.
- Existing direct commands stay direct until the operator explicitly changes
  them.
- Canary config must require a local API key or trusted loopback-only bind.
- Canary routes must target only the native relay provider for that client.
- Fallback to scout, free, or public API providers is disabled in native canary
  routes.

Rollback:

- Remove or stop using the canary wrapper/profile.
- Leave native client auth stores untouched.
- Keep direct native clients usable throughout the rollout.

## Product/Setup Boundary

Product docs define provider kinds, auth kinds, conformance gates, telemetry,
and safety invariants.

Local setup docs define shell functions, LaunchAgents, ports, local profile
names, and machine-specific expectations.

Do not encode a personal machine's command aliases into Switchback's product
contract. Do not encode product relay safety gates only in a local shell setup.
