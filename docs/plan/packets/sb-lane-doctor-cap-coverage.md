# Packet · sb provider/lane doctor capability coverage

**Work id:** `work_ef79811f300b` (Compound, bridge envelope — change lands in switchback)
**Branch:** `feat/lane-doctor-cap-coverage-20260728`
**Base SHA:** `5158a3d5db09208fa2864034b096230a8d3ffdca` (switchback `main`)
**Surface:** `crates/sb-server/src/{provider_cli/doctor.rs, lane_cli.rs}` + a new test in `crates/sb-server/tests/cli.rs`
**Related (NOT in scope):** `work_3316a5f7ccaa` — Compound-side model-capability claim mutation cross-system bridge. Different surface, different move_type. Cite in commit message, do not depend on.

## Objective

Make `RouteRequire` first-class in the doctor surface so a route whose targets are all blind to a non-null capability field can no longer pass `sb provider doctor` or `sb lane doctor` as green. Eliminate the "landed ≠ working" pattern the 2026-07-28 incident surfaced: a `wpcom/gpt-5.6-sol` route with `vision_in`-blind fallbacks was checked green, then a real vision request hard-failed with `no eligible target` mid-session.

## Verified seams (file:line, exact)

- `crates/sb-server/src/provider_cli/doctor.rs:244` — `provider_doctor_config` calls `engine.preview_route(&req)` once with a non-vision request. No capability-pressure loop.
- `crates/sb-server/src/provider_cli/doctor.rs:271-313` — non-stream / stream / embeddings checks. Vision/audio/json_schema absent.
- `crates/sb-server/src/lane_cli.rs:555-602` — `lane_doctor_report` enumerates exactly 5 hardcoded lanes (`scout/code`, `scout/chat`, `codex/api`, `codex-native`, `pro/manual`). User-defined routes like `wpcom/gpt-5.6-sol` are invisible.
- `crates/sb-core/src/config.rs:2864-2892` — `RouteRequire` already supports `vision_in`, `tool_calling`, `server_tools`, `server_tool_protocols`, `streaming`, `json_schema`, `min_context_tokens`, `audio_in`, `file_in`, `image_out`, `reasoning_summary`. The router already filters on these (`crates/sb-router/src/lib.rs:705-810`). The doctor does not.
- `crates/sb-core/src/routing.rs:297-322` — `RouteDecision { selected, fallbacks, rejected[], reason[] }`. The doctor's `preview_route` already returns this. A `selected: None` is the canonical "no eligible target" signal.
- `crates/sb-server/src/lane_cli.rs:740-762` — `lane_from_targets` constructs `LaneReport`. Extension point for `capability_coverage` field.
- `crates/sb-server/tests/cli.rs:1879` — existing `lane_doctor_json_reports_lane_identity_and_transition_warnings` test; LANE_CFG constant at line 25.

## Settled design decisions

1. **Probe via `engine.preview_route`, not via fresh request shape.** The router already knows the rejection reasons. Building a vision-bearing `AiRequest`, calling `preview_route`, then asserting `decision.selected.is_some()` is the canonical signal — same code path the runtime will execute. No new probe protocol.
2. **Per-capability probe loop in provider doctor.** After the existing `route_preview` check (~line 268), iterate `route.require` fields. For each non-null field, mutate a clone of `req` to press that capability (`req.stream = true` for `streaming`; append an `Image` content part for `vision_in`; append a `ToolSpec` for `tool_calling`; etc.). For each, run `preview_route` and assert `selected.is_some()`. Emit one **required** check per non-null capability.
3. **Data-driven lane enumeration.** `lane_doctor_report` keeps the existing 5 stable lanes for backward compatibility, then enumerates `cfg.routes` and `cfg.combos` and emits one `LaneReport` per route with `state`, `primary_target`, `fallback_count`, and `capability_coverage`. For `require.vision_in == Some(true)`, count targets whose capability set includes `vision_in` (read from `candidate.capabilities` via the same path the router uses); state is `red` if coverage is `0/N`, `yellow` if `coverage < N` (i.e. partial), `green` if `coverage == N`.
4. **`capability_coverage` field on `LaneReport` is the doctor load-bearing addition.** A consumer running `sb lane doctor --json` and parsing `capability_coverage.vision_in` gets the same answer the runtime will give.
5. **Skip `audio_in`/`file_in`/`image_out`/`reasoning_summary` content probes for now.** No `ContentPart` carries audio/file; `image_out`/`reasoning_summary` are response-side. The doctor emits a `capability::<field>: status=unsupported` check for those fields so consumers see the gap is acknowledged, not silently absent.

## Falsifier test list (red-without, green-with)

- **(F1) `route_capability_coverage` check red-without.** Construct a route whose targets all declare `vision_in: false` (mock provider with `capabilities: { vision_in: false }`). Run `sb provider doctor --config <temp> --provider mock --model vision-blinder`. Assert the new `route_capability_coverage.vision_in` check is `failed`, status `red`. **Today: passes green (no such check exists).**
- **(F2) `route_capability_coverage` check green-with.** Same route, but flip ONE target's `capabilities.vision_in` to `true`. Re-run. Assert the check is `ok`, status `green`.
- **(F3) `lane doctor` reports data-driven rows.** With a route named `wpcom/gpt-5.6-sol` whose targets are vision-blind, run `sb lane doctor --json`. Assert a row exists with `id: "route/wpcom/gpt-5.6-sol"`, `state: "red"`, `capability_coverage.vision_in: "0/4"`. **Today: row does not exist; doctor reports green.**
- **(F4) Scratch mutation flips red → green.** From the F3 config, flip one target to `vision_in: true`. Assert the row flips to `state: "green"`, `capability_coverage.vision_in: "1/4"`.
- **(F5) Teeth.** Delete the F1 assertion line. Confirm the test still goes green (proving the assertion is load-bearing, not documentation).

## Hard file boundaries (touch ONLY / do NOT touch)

- **Touch ONLY:**
  - `crates/sb-server/src/provider_cli/doctor.rs` (extend `provider_doctor_config`)
  - `crates/sb-server/src/lane_cli.rs` (extend `lane_doctor_report`, extend `LaneReport`, add `capability_coverage` helper)
  - `crates/sb-server/tests/cli.rs` (add F1–F5)
  - `crates/sb-server/src/lane_cli.rs` test surface if needed for capability_count helper
- **Do NOT touch:**
  - `crates/sb-router/`, `crates/sb-core/` (router + core already correct; this slice is doctor-only)
  - `crates/sb-server/src/{serve,cp,handlers}/` (HTTP / control-plane surface; out of scope)
  - `crates/sb-server/src/{lane_profile_cli,provider_cli/provider.rs}` (lane profile + provider binary wiring; orthogonal)
  - Any test fixture under `crates/sb-server/tests/fixtures/` (use inline test config per existing `LANE_CFG` pattern)

## Privacy law

- All test fixtures use `mock` provider and `.invalid` hostnames (none required here, but if any base URLs surface in tests, follow switchback's existing pattern).
- No real credentials, no real paths, no real account names in this slice.
- The change is destined public. No `docs_private/` content is touched.

## Verification the executor runs

```bash
# From worktree: .worktrees/lane-doctor-cap-coverage-20260728/
cargo build -p sb-server
cargo test -p sb-server --test cli -- lane_doctor route_capability_coverage
cargo test -p sb-server --test cli   # full CLI test suite, ensure no regression
```

## Handoff report format

- One paragraph summary in the commit body: "wire RouteRequire into sb provider/lane doctor; data-driven lane enumeration; failing test with scratch-mutation proof of teeth (work_ef79811f300b)".
- List the falsifiers that passed (F1–F5) with the specific command + observed output.
- Cite the incident (2026-07-28 `wpcom/gpt-5.6-sol` vision-blind fallback hard-fail) as the trigger.
- Note that `work_3316a5f7ccaa` (Compound-side cross-system capability mutation) is the related-but-distinct follow-up; do not block on it.

## Boundary

- This slice is doctor-surface-only. It does NOT mutate route authority, does NOT change the router, does NOT touch `work_3316a5f7ccaa`'s surface.
- A future slice (out of scope here) can add a `sb route-preview --require vision_in=true --model <route>` operator-facing CLI verb that reuses the same probe loop. Mention in the commit body as the obvious next step; do not implement.
