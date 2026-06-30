# Dashboard Design Recap

Date: 2026-06-30
Status: design direction, not implemented

## Purpose

The Switchback dashboard should make a hard system adjustable by a human operator without requiring an agent to remember every lane, port, wrapper, provider, account, and profile rule.

The current dashboard is useful as a scaffold, but it is still too close to a status panel. The next dashboard should behave like an operator console: it should explain what state belongs to Switchback, what state belongs to native clients, what state belongs to Headroom/taps, what can be changed safely, and what command or draft will be applied.

## Current Surface

The current embedded dashboard at `/` is implemented in `crates/sb-server/src/dashboard.html`. It includes:

- Overview metrics.
- Setup readiness checklist.
- Native Codex and Claude Code profile readiness.
- Provider/account table.
- Route preview form.
- Metadata trace list.
- Usage and durability summary.
- Runtime toggles for cost-aware routing, latency-aware routing, and hedging.
- Command drawer and copyable setup commands.

The dashboard polls:

- `/health`
- `/v1/providers`
- `/v1/client-profiles`
- `/v1/usage`
- `/v1/usage/reconcile`
- `/v1/traces?limit=25`
- `/v1/sessions?limit=25`
- `/v1/runtime`
- `/cp/v1/runtime-state`

## Core UX Problem

Switchback has several real control planes:

| Plane | Owner | Examples | Operator confusion risk |
|---|---|---|---|
| Switchback engine | Switchback config/runtime | providers, routes, tenants, budgets, runtime knobs | User expects all route behavior to apply to taps |
| Native client auth | Codex/Claude Code local files | `${HOME}/.codex/auth.json`, `${HOME}/.claude/.credentials.json` | User expects Switchback to own subscription credentials |
| Transparent taps | Switchback tap listeners plus Headroom/vendor upstream | `:18770`, `:18771`, `:8787` | User expects fallback/retry after streamed bytes |
| Native relay | Switchback relay providers | `codex_native_relay`, `claude_code_native_relay` | User confuses fail-closed relay with direct native command |
| Local wrappers | Shell/CLI setup | `codex`, `codex-tap`, `codex-native`, `codex-free`, `claude`, `claude-native` | User cannot tell which command uses which path |
| Provider accounts | Switchback credentials resolver | API keys, vault refs, OAuth leases, availability locks | User cannot tell which account will be selected |
| Future workload jobs | Switchback job/artifact store | image/video/workflow jobs, artifacts, ComfyUI queue | User expects text traces to explain media state |

The dashboard should not hide this complexity. It should make the ownership map visible and provide guided, reversible changes.

## Product Principle

The console should answer four questions quickly:

1. What is the current traffic path?
2. Who owns the next decision or failure?
3. What can I change safely from here?
4. What evidence proves the change worked?

Every editing flow should end in one of:

- dry-run preview
- generated command
- config draft
- validated publish
- live smoke result
- explicit blocker

## Information Architecture

Use a left navigation rail with seven primary zones.

### 1. Overview

Purpose: one-screen operations picture.

Required content:

- Gateway health and revision.
- Active bind URL and auth requirement.
- Current route mode summary for Codex, Claude Code, and OpenAI-compatible API clients.
- Provider/account health summary.
- Last failure with owner label: `Switchback`, `Headroom`, `native client`, `provider`, `network`, or `operator config`.
- Recent request/job count.
- Usage and durability status.
- One prioritized next action.

Primary actions:

- Copy endpoint.
- Open route inspector.
- Open setup wizard.
- Run smoke test.

### 2. Setup

Purpose: guided readiness, not a wall of status checks.

Required content:

- Setup graph showing path from client to upstream:

```text
Client command -> local wrapper/profile -> Switchback ingress/tap -> Headroom if present -> provider/backend -> trace/usage
```

- Stepper for each target client:
  - OpenAI-compatible API client.
  - Codex observed tap.
  - Codex Switchback gateway/free lane.
  - Claude observed tap.
  - Claude Switchback gateway/free lane.
  - Future ComfyUI workflow lane.

- Each step has:
  - status
  - owner
  - exact failing check
  - fix command or config draft
  - verification command

Primary actions:

- Generate starter config.
- Add provider/account.
- Install/update local wrapper.
- Copy native client setup command.
- Validate route preview.
- Run smoke.

### 3. Lanes

Purpose: make lane names and traffic paths concrete.

Required content:

- Lane cards/table for `codex`, `codex-tap`, `codex-native`, `codex-relay`, `codex-free`, `claude`, `claude-native`, `claude-switchback`, `scout/code`, `scout/chat`, `local/mac-code`, `local/mac-fast`.
- For each lane:
  - command name
  - client protocol
  - Switchback endpoint or tap port
  - Headroom involvement
  - provider/backend target
  - auth owner
  - observability level
  - fallback semantics
  - expected smoke command

Primary actions:

- Compare lanes.
- Copy command.
- Preview route.
- Run lane doctor.

### 4. Routes

Purpose: make `RouteDecision` understandable before and after execution.

Required content:

- Route preview form with model/profile/workload kind.
- Candidate list with selected, fallback, rejected, and demoted targets.
- Rejection reasons in plain language.
- Capability requirements: streaming, tools, server tools, vision, image output, reasoning summary, JSON schema, context, workload kind.
- Score breakdown: selection rank, cost, latency, TTFT, health, account availability, task fit, context fit.
- Policy controls that are safe to preview before patching.

Primary actions:

- Preview only.
- Save as draft route/profile change.
- Copy curl.
- Open matching traces.

### 5. Providers

Purpose: account and backend readiness.

Required content:

- Provider list with type, base URL, model hint, auth scheme, account count, health, certification state.
- Account list with redacted auth source, selection strategy, lockout/circuit state, allowed tenants.
- Capability matrix by provider/model/workload.
- Certification/probe evidence and last checked time.

Primary actions:

- Add provider preset as config draft.
- Set env/vault reference instructions.
- Run provider doctor/certify.
- Import/sync models.
- Disable or demote provider.

### 6. Requests And Jobs

Purpose: unified execution history.

Required content:

- Text requests, embedding calls, and future image/video/workflow jobs in one list.
- Filters: tenant, project, client profile, model, provider, workload kind, status, error class, session.
- Timeline per row:
  - accepted
  - routed
  - account leased
  - upstream started
  - first byte or job running
  - usage/artifacts
  - terminal status

Primary actions:

- Open trace/job inspector.
- Replay route preview against current config.
- Copy request id.
- Open produced artifacts.

### 7. Workflows And Artifacts

Purpose: first-class media/workflow management once workload planes exist.

Required content:

- Workflow registry with template id, version, provider, input schema, output schema, last run, failure rate.
- Job queue with progress, provider job id, cancellation state, node/stage failures.
- Artifact browser with thumbnails, media metadata, provenance, retention status.
- ComfyUI connection health and queue depth.

Primary actions:

- Preview workflow route.
- Run workflow with typed inputs.
- Cancel job.
- Open artifact.
- Copy artifact reference for later jobs.

## Editing Model

The dashboard should not directly mutate complex config in hidden ways. Use a four-stage editing pattern:

1. Inspect current state with redacted values.
2. Draft a change using `/cp/v1/drafts` or generated CLI patch.
3. Validate/preview route, admission, provider, or lane effects.
4. Publish/apply only after the UI shows exact diff and rollback path.

Safe immediate toggles:

- Runtime-only flags already exposed by `/v1/runtime`, such as cost-aware routing or latency-aware routing.
- Local UI preferences.

Draft-first edits:

- provider/account changes
- route profiles
- tenant limits
- egress profiles
- plugin changes
- workflow templates
- artifact retention policies

External-command edits:

- shell wrappers
- Headroom update/restart
- Codex/Claude native config
- LaunchAgent changes
- provider env files

For external-command edits, the dashboard should generate the command and verify the result. It should not silently mutate those files unless a future explicit local helper owns that operation.

## Design System

### Visual Theme

Restrained high-density operator console. It should feel like a flight deck for local AI routing: precise, quiet, inspectable, and fast. The UI should not look like a marketing dashboard or a generic card grid.

Density: high.

Motion: minimal and purposeful.

Shape language: compact rectangles, 4px to 8px radius, thin dividers, no decorative blobs or gradients.

### Color Palette

- **Graphite Canvas** (`#101216`): primary app background.
- **Panel Charcoal** (`#171A20`): panels, sidebars, inspector surfaces.
- **Raised Steel** (`#20252D`): buttons, active table rows, command drawer.
- **Primary Ink** (`#E7EBF0`): primary text.
- **Muted Zinc** (`#A2ABB7`): secondary text and metadata.
- **Dim Slate** (`#5F6B7A`): low-priority labels and disabled states.
- **Structure Line** (`#2A3039`): borders and dividers.
- **Signal Teal** (`#2DD4BF`): one accent for active states, focus, selected route.
- **Success Green** (`#4CC46A`): healthy/pass.
- **Warning Amber** (`#E3B341`): degraded/check.
- **Failure Red** (`#F0594E`): failed/blocked.

Do not use purple/blue neon, large gradients, pure black, or color-only status communication.

### Typography

- Interface font: system sans or `Geist` if bundled later.
- Numeric/technical font: `SF Mono`, `JetBrains Mono`, or `ui-monospace`.
- Use monospace for ports, model ids, provider ids, route ids, request ids, costs, revisions, and durations.
- Avoid hero-scale headings. The largest dashboard heading should stay compact.
- Long model/provider strings must truncate with accessible full text on hover or inspector expansion.

### Layout

- Persistent left rail for zones.
- Sticky top command/search strip.
- Primary content as split panes:
  - list/table on the left
  - inspector/timeline/diff on the right
- Bottom dock only for transient logs/smoke results, not primary navigation.
- Tables should support dense scanning, sorting, filters, and row inspectors.
- Avoid cards inside cards. Use cards only for repeated entities where framing helps.
- Mobile can become single-column read-only/operator-lite; serious config editing is desktop-first.

### Component Rules

- Buttons use icons where recognizable: copy, refresh, play, pause/cancel, inspect, filter, save/publish.
- Destructive or external actions require explicit label and confirmation.
- Status pills include text and symbol/dot.
- Route decisions use a deterministic visual grammar:
  - selected target: teal outline/fill
  - fallback: amber
  - rejected: red
  - demoted but possible: muted amber
  - unverified: slate + warning label
- Diffs use side-by-side or stacked structured JSON/YAML views with redacted secrets.
- Empty states should name the next command or draft action, not explain the product.

## High-Leverage Flows

### Flow 1: "Make Codex Use Switchback Correctly"

1. User opens Setup.
2. Selects Codex.
3. UI shows current command path:

```text
codex -> codex-switchback-tap -> :18771 -> Headroom :8787 -> native backend
```

4. UI shows auth owner: native Codex, not Switchback provider account.
5. UI shows observability: tap body capture if enabled, metadata trace, no Switchback fallback after first byte.
6. UI shows next failure or green smoke.
7. User copies install/repair command.
8. UI verifies listener, wrapper, route preview, and trace after smoke.

### Flow 2: "Add A Provider Without YAML Editing"

1. User opens Providers.
2. Selects preset, such as OpenRouter.
3. UI shows generated config draft with env var name, no secret value.
4. UI validates config.
5. UI runs route preview.
6. UI instructs setting env/vault ref.
7. UI runs provider certify only after user confirms live upstream check.
8. UI publishes draft and shows revision/audit record.

### Flow 3: "Why Did This Request Go There?"

1. User opens Requests.
2. Selects a trace.
3. Inspector shows inbound model/profile/workload.
4. Route graph shows selected/fallback/rejected.
5. Each rejection reason is plain and exact.
6. Score table shows cost/latency/health/account availability.
7. User clicks "Replay route preview" to compare current config.

### Flow 4: "Run A ComfyUI Workflow"

1. User opens Workflows.
2. Selects a named workflow template.
3. UI renders typed inputs from schema.
4. User chooses source artifacts or uploads local input.
5. UI previews route and admission.
6. User runs job.
7. Job inspector shows queue, running node/stage, artifacts, and terminal status.
8. Artifacts show provenance and retention.

## Data Contract Gaps

The dashboard can improve immediately with existing endpoints, but the next product-quality version needs additional server data:

| Need | Existing source | Gap |
|---|---|---|
| Lane path map | `switchback-routing-contract.md`, CLI lane doctor | No stable HTTP endpoint for lane path graph |
| Ownership labels | mixed CLI/docs/dashboard logic | No structured owner field per check |
| Setup actions | dashboard generated commands | No action schema with risk/verification metadata |
| Provider certification | CLI provider certify | No dashboard-native latest certification evidence endpoint |
| Config diffs | `/cp/v1/drafts`, resources | Needs UI-friendly diff summary |
| Media jobs | not implemented | Needs job/artifact/workflow endpoints |
| Artifact previews | not implemented | Needs authenticated thumbnail/content endpoints |

## First Redesign Slice

The smallest useful dashboard implementation should avoid adding media first. It should fix setup comprehension for the existing hard parts.

1. Replace the current single overview/setup blend with a dedicated Setup zone.
2. Add a lane path graph for Codex and Claude:
   - direct native
   - observed tap through Headroom
   - Switchback gateway/free route
   - native relay canary/fail-closed path
3. Add owner labels to each check.
4. Add "change safely" actions with generated command, dry-run/validate command, and verification command.
5. Move provider/account readiness into a clearer Providers zone.
6. Keep route preview and traces, but make the route inspector the main way to explain decisions.
7. Add tests that assert the dashboard names lane ownership, Headroom involvement, and tap-vs-relay distinction.

## Later Redesign Slice

After workload classes exist:

1. Add Requests And Jobs unified execution list.
2. Add Workflow registry.
3. Add Artifact browser.
4. Add ComfyUI connection and queue panel.
5. Add job route preview and job lifecycle timeline.

## Open Unknowns

- UNKNOWN: exact parts of the current dashboard that feel most inconvenient during daily use. Obtain by observing one real setup or repair session and marking every place the user reaches for an agent.
- UNKNOWN: whether the dashboard should remain a no-build embedded HTML file or move to a small bundled frontend. Obtain by deciding if richer tables, inspectors, command palette, and artifact grids justify a frontend build step.
- UNKNOWN: how much direct file mutation the dashboard should be allowed to do for wrappers, Headroom, and native client config. Obtain by setting an explicit safety policy for external-command edits.
- UNKNOWN: artifact thumbnail and media browsing expectations. Obtain after the first ComfyUI workflow target is selected.

## Assumptions

- The dashboard is for operators and agents configuring Switchback, not for end customers.
- Safe configuration means previewable, reversible, redacted, and verified.
- Users can understand complex routing if the UI names ownership and traffic path plainly.
- The dashboard should reduce dependence on agents for routine setup, while still producing commands and receipts agents can execute.
- The first redesign should improve existing text/native-client setup before adding image/video workflow UI.
