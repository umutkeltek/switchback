# Multimodal Workload Brief

Date: 2026-06-30
Status: design direction, not implemented

## Purpose

Switchback should be able to route more than LLM text calls, but it should not become a shallow proxy for every AI-shaped HTTP API. The expansion path is an AI execution gateway with typed workload classes, shared routing/accounting/observability, and workload-specific execution interfaces.

This brief defines how text generation, image generation, video generation, and ComfyUI-style workflow execution should fit Switchback without weakening the current provider-agnostic LLM IR.

## Current Evidence

- Current product contract: one local-first gateway, canonical typed IR, explainable `RouteDecision`, metadata-only traces, credential/account routing, budgets, and fallback before first streamed byte.
- Current implemented routes: `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/messages`, `/v1/messages/count_tokens`, `/v1/models`, `/v1/usage`, `/v1/traces`, control-plane `/cp/v1/*`, and dashboard `/`.
- Current canonical IR: `AiRequest` models text, tools, server tools, reasoning summaries, structured output, image input, and generated inline image stream events.
- Current routing already has capability gates for streaming, tool calling, server tools, vision input, audio input, file input, image output, reasoning summary, JSON schema, and context window.
- Current docs say richer audio/video/file/media is bounded and fail-loud today.
- Current dashboard is an embedded single-page operator console, not a multi-screen product app.

## Decision

Switchback should support general AI routing by adding workload planes, not by stuffing every modality into `AiRequest`.

The first supported workload classes should be:

| Workload class | Execution shape | First useful surface | Route decision input | Completion shape |
|---|---|---|---|---|
| Text generation | Streaming or collected response | Existing `/v1/chat/completions`, `/v1/responses`, `/v1/messages` | model, messages, tools, output constraints, tenant/project, profile | `AiStreamEvent` or collected response |
| Embeddings | Synchronous batch | Existing `/v1/embeddings` | model, input count, dimensions, tenant/project | embedding vectors and usage |
| Image generation | Mostly async job, sometimes sync compatibility response | New OpenAI-compatible `/v1/images/generations` plus canonical job API | prompt, input images, size, seed, output format, workflow/profile | artifact ids, thumbnails, metadata, usage |
| Video generation | Async job | Canonical job API first | prompt, input media, duration, fps, size, safety/profile | artifact ids, status timeline, metadata, usage |
| Workflow execution | Async job/DAG run | Canonical workflow API first; ComfyUI adapter as first implementation | workflow id/version, typed inputs, artifact refs, resource constraints | job status, node events, artifacts, logs, usage |

## Non-Goals

- Do not make arbitrary ComfyUI graph JSON a default public request body.
- Do not add provider wire fields to `sb-core::AiRequest`.
- Do not make the dashboard an all-powerful OS editor that mutates shell profiles, Headroom, Codex, and provider state without a preview.
- Do not store prompts, responses, tool arguments, or generated media bodies in metadata traces.
- Do not add hosted marketplace billing, per-user SaaS tenancy, or cloud object storage in the first local-first media slice.
- Do not silently fall back from native/subscription lanes to public API lanes for media or workflow jobs.

## Core Model

The shared execution model should be:

```text
Client protocol request
  -> protocol/workload translator
  -> typed workload request
  -> route planner emits RouteDecision
  -> account/credential lease
  -> workload adapter execution
  -> events + artifacts + usage
  -> client protocol response or job status
```

The existing text path remains:

```text
OpenAI/Anthropic/Gemini text protocol -> AiRequest -> Engine::execute -> AiStreamEvent
```

Media and workflows add:

```text
Image/video/workflow protocol -> WorkloadRequest -> JobEngine::submit -> JobEvent + Artifact
```

`JobEngine` is a working name for the execution module. The important interface is the job lifecycle, not the name.

## Typed Interfaces

Add a new `sb-core::workload` module when implementation starts. Keep the interface small.

```rust
pub enum WorkloadKind {
    TextGeneration,
    Embedding,
    ImageGeneration,
    VideoGeneration,
    WorkflowExecution,
}

pub struct WorkloadRequest {
    pub id: String,
    pub kind: WorkloadKind,
    pub target: String,
    pub tenant: Option<String>,
    pub project: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub spec: WorkloadSpec,
}

pub enum WorkloadSpec {
    Text(AiRequest),
    Embedding(EmbeddingRequest),
    Image(ImageGenerationRequest),
    Video(VideoGenerationRequest),
    Workflow(WorkflowRunRequest),
}
```

This envelope is not a replacement for `AiRequest`. It is the interface that lets the runtime route jobs without making every caller learn every workload's details.

## Job Lifecycle

Media and workflow execution need a lifecycle distinct from text streaming:

| State | Meaning | Required operator visibility |
|---|---|---|
| `accepted` | Request passed auth, validation, and idempotency checks | request id, tenant/project, revision |
| `queued` | Waiting for local/remote executor capacity | queue position or reason, timeout budget |
| `routing` | Switchback is choosing target/account | `RouteDecision` preview |
| `leased` | Credential/account selected | provider/account ids, redacted auth source |
| `running` | Upstream job has started | adapter job id, elapsed time, progress if available |
| `artifact_ready` | One output artifact is available | artifact id, type, dimensions/duration, checksum |
| `succeeded` | Terminal success | usage/cost, artifacts, trace id |
| `failed` | Terminal failure | error class, retry/fallback legality, failed adapter stage |
| `cancelled` | Operator/client cancelled before terminal state | cancellation source, upstream cancellation status |

Fallback is legal only before the adapter has committed provider-side work that cannot be cancelled safely. For ComfyUI, that usually means before queue submission or before the first non-cancellable node starts. The adapter must report the commit point explicitly in the trace.

## Artifact Model

Artifacts are first-class outputs, not trace bodies.

Minimum fields:

| Field | Meaning |
|---|---|
| `artifact_id` | Stable Switchback id |
| `job_id` | Parent job |
| `kind` | `image`, `video`, `audio`, `file`, `thumbnail`, `metadata` |
| `media_type` | MIME type such as `image/png` |
| `bytes` | Size in bytes when known |
| `sha256` | Content digest |
| `storage_ref` | Local store path or backend reference, never a raw secret URL |
| `width`, `height` | Images/videos when known |
| `duration_ms`, `fps` | Video/audio when known |
| `created_at_ms` | Creation time |
| `retention` | Retention policy label |
| `provenance` | provider, model/workflow, route decision id, source artifact ids |

Default storage should be local-first. Use the existing Switchback state-directory style and metadata-only SQLite patterns. Large binary bodies should not go into the main usage/config database.

Recommended first local layout:

```text
state_dir/artifacts/index.sqlite
state_dir/artifacts/blobs/sha256/ab/abcdef1234567890
state_dir/artifacts/thumbs/art_01HXAMPLE.webp
```

`state_dir` resolves the same way other Switchback local state paths resolve; on a typical local install it is `${HOME}/.local/state/switchback`.

Generated artifact access should require the same auth posture as `/v1/traces` and `/v1/usage`. A future public sharing link system is out of scope.

## Capability Model

The current `CapabilityProfile` booleans are enough for fail-loud gating, but image/video/workflow routing needs richer facts than `image_out: true`.

Add workload capability records instead of expanding one struct forever:

```rust
pub struct WorkloadCapability {
    pub kind: WorkloadKind,
    pub modes: Vec<String>,
    pub input_artifact_kinds: Vec<ArtifactKind>,
    pub output_artifact_kinds: Vec<ArtifactKind>,
    pub max_pixels: Option<u64>,
    pub max_duration_ms: Option<u64>,
    pub supports_seed: bool,
    pub supports_negative_prompt: bool,
    pub supports_control_image: bool,
    pub supports_batch: bool,
    pub sync_supported: bool,
    pub async_required: bool,
}
```

Existing `CapabilityProfile` can keep text-era gates. `WorkloadCapability` becomes the structured source for media/workflow route decisions and dashboard compatibility matrices.

## API Surface

Add compatibility surfaces only where they map cleanly to canonical job semantics.

### OpenAI-Compatible Image Compatibility

`POST /v1/images/generations`

- Accept OpenAI-shaped image generation requests.
- Translate to `WorkloadSpec::Image`.
- If the selected adapter can complete quickly, return OpenAI-compatible response.
- If async execution is required, return a Switchback job envelope with a clear compatibility extension field.
- Never pretend a queued job is a completed image response.

### Canonical Jobs

`POST /v1/jobs`

Submit image, video, or workflow execution.

`GET /v1/jobs/{id}`

Read status, route decision, progress, usage, artifacts, and terminal result.

`GET /v1/jobs/{id}/events`

SSE event stream for job lifecycle, progress, node updates, artifacts, and terminal event.

`POST /v1/jobs/{id}/cancel`

Cancel if the adapter can safely cancel or mark local interest cancelled if upstream is already committed.

`GET /v1/artifacts/{id}`

Fetch artifact metadata or content depending on `Accept` header.

`GET /v1/artifacts/{id}/thumb`

Fetch a small preview suitable for dashboard grids.

### Canonical Workflows

`GET /v1/workflows`

List configured workflow templates, input schema, output schema, versions, and supported adapters.

`POST /v1/workflows/{id}/preview`

Validate typed inputs and show route decision without executing.

`POST /v1/workflows/{id}/runs`

Submit a workflow run. Returns job envelope.

## ComfyUI Adapter Boundary

ComfyUI should be a workflow adapter, not a general-purpose protocol that leaks into the text IR.

Provider config direction:

```yaml
providers:
  - id: comfy-local
    type: comfyui
    base_url: "http://127.0.0.1:8188"
    accounts:
      - id: local
        auth: { kind: none }

workflows:
  - id: product-shot
    version: "2026-06-30"
    provider: comfy-local
    graph_ref: "${HOME}/.config/switchback/workflows/product-shot.json"
    inputs:
      prompt: { type: string, required: true }
      source_image: { type: artifact_ref, required: false, accepts: ["image/png", "image/jpeg"] }
      seed: { type: integer, required: false }
    outputs:
      image: { type: artifact, media_type: "image/png" }
```

Adapter responsibilities:

- Load configured workflow templates from local files or durable config drafts.
- Bind caller inputs into named graph nodes.
- Reject unknown inputs and missing required inputs before queue submission.
- Submit to ComfyUI queue.
- Poll or subscribe for progress.
- Capture produced images/videos/files into the artifact store.
- Record node-level failures as job events.
- Surface upstream job id in metadata without exposing local filesystem paths by default.
- Respect queue/admission limits.

Default caller behavior should run named templates. Raw graph submission can exist later behind an explicit `allow_raw_graph: true` provider/workflow setting.

## Dashboard Implications

The dashboard must show a unified execution story:

- Text requests and media jobs share trace ids, route decisions, tenant attribution, usage, and provider/account health.
- Media jobs need a job queue view and artifact browser.
- Workflows need a template registry view with input schemas and preview/run actions.
- ComfyUI failures should be legible as node/stage failures, not generic 500s.
- The UI must show where state lives: Switchback config, provider account, local native auth store, Headroom/tap path, or artifact store.

## First Implementation Slice

The smallest useful slice is image generation with one local ComfyUI workflow template.

1. Add `WorkloadKind`, `WorkloadRequest`, `ImageGenerationRequest`, `WorkflowRunRequest`, `JobRecord`, `JobEvent`, and `ArtifactRecord` types in `sb-core`.
2. Add local artifact metadata/blob storage using Switchback state directory.
3. Add `ProviderKind::ComfyUi` and a ComfyUI workflow adapter that executes named templates only.
4. Add `/v1/jobs`, `/v1/jobs/{id}`, `/v1/jobs/{id}/events`, `/v1/artifacts/{id}`, and `/v1/workflows`.
5. Add `POST /v1/images/generations` as compatibility ingress that maps to the image job path.
6. Extend route decisions to include workload kind and job id.
7. Add dashboard job queue/artifact/workflow panes.
8. Add tests with a mock workflow adapter before real ComfyUI live probes.

## Verification Bar

- Unit tests for workload request validation.
- Router tests proving image/workflow capability rejection reasons are explicit.
- Mock workflow adapter tests for accepted, queued, running, artifact, succeeded, failed, and cancelled states.
- HTTP tests for `/v1/jobs`, `/v1/jobs/{id}/events`, `/v1/artifacts/{id}`, `/v1/workflows`, and `/v1/images/generations`.
- Redaction tests proving traces do not store prompts, raw graph bodies, generated media bodies, or secrets.
- Dashboard tests proving job/workflow navigation renders without a live ComfyUI instance.
- Live optional probe against local ComfyUI only when `COMFYUI_BASE_URL` is set.

## Open Unknowns

- UNKNOWN: first target ComfyUI workflows and node graphs. Obtain by exporting the actual ComfyUI workflows the operator wants Switchback to run first.
- UNKNOWN: artifact retention policy and storage volume location. Obtain by deciding default retention days, max bytes, and whether artifacts should live under Switchback state or the existing protected observability volume.
- UNKNOWN: whether hosted image/video APIs should be included in the first slice or deferred behind local ComfyUI. Obtain by listing the first three backends that need to route through Switchback.
- UNKNOWN: whether OpenAI image compatibility should return async job envelopes or block until complete for local workflows. Obtain by testing expected client compatibility requirements.

## Assumptions

- Switchback remains local-first and metadata-only by default.
- Users need a guided operator UX more than they need raw access to every provider knob.
- ComfyUI is valuable as a local workflow executor, not as a generic graph upload endpoint.
- Text generation remains the most mature and latency-sensitive path; media/workflow should not slow or destabilize it.
- The first media implementation should reuse existing auth, route decision, ledger, trace, admission, and control-plane patterns before adding new crates.
