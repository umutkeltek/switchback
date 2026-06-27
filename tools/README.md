# tools/ — the model + cost registry

`build-registry.ts` builds **`config/model-registry.json`**, switchback's model +
cost map: which providers serve each model and at what price, so the router can
later pick the **cheapest source** for a requested model. No hardcoded model
names — the catalog and the cross-provider price spread are derived from the
live OpenRouter catalog, so it stays current as the landscape moves.

## Refresh

```bash
curl -s https://openrouter.ai/api/v1/models -o docs/registry/openrouter-models.json
bun tools/build-registry.ts            # -> config/model-registry.json
```

## Shape (`config/model-registry.json`)

Prices are **micro-USD per 1M tokens** (integer), matching
`sb-core::catalog::Price.unit_price_micros_per_mtok`.

- `models[]` — the comprehensive catalog: every model OpenRouter knows, with
  `context_window`, modalities, `tool_calling`/`json_schema`/`vision`, and base
  input/output/cached price. (350+ entries.)
- `providers[]` — the serving providers referenced by the spread (`id`,
  `base_url`, `auth` scheme, `openai_compatible`).
- `by_model{}` — **the routing map**: `model_id -> [offering, …]` sorted cheapest
  input first. Each offering is one `(provider, price, context, caps)`. This is
  the "same model, many providers, different prices" map — e.g. an open model
  served by 15–20 hosts spanning a 5–6× price spread; the first entry is the
  cheapest source.

## How it feeds routing (later)

`by_model` is what a cost-aware router consumes: resolve a requested model to its
candidate offerings, filter by required capabilities + health, then pick the
cheapest (or hedge across the top few). The `models[]`/`providers[]` load into
`sb-core::catalog` (Provider/Model/Price entities) so the existing
capability-based router and the usage/cost ledger price requests against it.

## Cross-check

`docs/registry/oracle-deepresearch.md` (git-ignored) is the ChatGPT Deep Research
report — comprehensive current provider lineups + **direct-provider** pricing
(OpenAI/Anthropic/Azure/Bedrock, which OpenRouter only approximates) used to
verify and fill gaps in the auto-built registry.

## Model Intake Procedure

Switchback has one provider/model registry: `config/provider-registry.json`.
Do not keep a parallel model spreadsheet. New model knowledge moves through four
states:

1. `seen`: provider catalog says the model exists.
2. `declared`: registry has provider-declared capabilities, pricing, limits,
   architecture, benchmark, and provenance fields.
3. `probed`: Switchback has written local `verification.probes` receipts.
4. `promoted`: a curated route group uses the model for a task class.

Declared facts are routing hints, not certification. Probe receipts are evidence
of current local behavior, not permanent truth.

Standard intake loop:

```bash
sb registry refresh --check-drift
sb registry refresh --source openrouter --source nvidia --apply
bun tools/enrich-provider-registry.ts --fetch --apply
bun tools/enrich-provider-registry.ts --check
sb registry capabilities nvidia
sb registry benchmarks nemotron
sb registry model nvidia/nvidia/nemotron-3-ultra-550b-a55b
sb registry score long_context nvidia
sb registry score judge --limit 10
sb registry probe --model nvidia/minimaxai/minimax-m3 --all --apply
sb reload
```

Use `sb registry refresh` as the normal provider intake front door. It keeps
provider-specific catalog details in source adapters plus
`enrich-provider-registry.ts`, produces a candidate registry, reports real
drift, and writes an enrichment-run receipt. Timestamp refresh alone is not
drift. Run `--json --no-receipt` in CI/tests; run `--fail-on-drift` when a
scheduled check should stop on membership, cost, capability, context,
architecture, benchmark, or catalog-presence changes. `--apply` updates the
registry only after the drift view is acceptable.

`enrich-provider-registry.ts` also carries researched direct-provider family
facts for OpenAI/Azure OpenAI, Anthropic/Bedrock Claude, Gemini/Vertex, xAI,
DeepSeek, Z.ai, Moonshot/Kimi, Mistral, Cohere, Alibaba/Qwen, NVIDIA Build,
and third-party hosted lanes. Family facts fill structured capabilities,
limits, API-shape, architecture, determinism notes, and official source URLs;
exact model overrides refine rows where official model docs provide stronger
facts.

Independent provider rows (`groq`, `together`, `fireworks`, `deepinfra`,
`novita`, `cerebras`, `sambanova`, `hyperbolic`, `nebius`) also get
provider-level `provider_research` and `provider_catalogs.*_provider`
descriptors before Switchback has auth-backed model ingestion for that host.
Keep those as official-doc cross-checks: they describe base URL, catalog
endpoint/status, declared capabilities, determinism caveats, and routing notes.
Do not invent model rows from a provider page; add model rows only when a source
adapter or authenticated catalog fetch can preserve per-model
price/context/capability provenance.

`cerebras` and `groq` are independent-provider catalog adapters. Run them
explicitly with `sb registry refresh --source independent`, or one at a time
with cached payloads such as `--cerebras-json FILE` and `--groq-json FILE`.
They are not part of default `--source all`: Cerebras' documented public
endpoint has returned 404 from this environment, and Groq live fetches require
`GROQ_API_KEY`. The Cerebras adapter maps per-token USD prices into
`*_micros_per_mtok`; the Groq adapter keeps prices null unless the catalog
payload supplies normalized micro-USD fields. Both skip inactive/deprecated
model rows and mark `provider_catalogs.*_provider.status` as
`provider_catalog_ingested`.

Use narrow probes when a full declared probe set would waste quota or hit a
known fragile free endpoint:

```bash
sb registry probe --model openrouter/openrouter/free \
  --capability completion \
  --capability stream \
  --capability headers \
  --apply --allow-failures
```

Promotion rules:

- use `sb registry score <job-class> [filter]` as a read-only decision surface
  before changing route groups. It ranks offerings from cost, declared facts,
  local probe receipts, benchmark hints, and policy penalties; it does not mutate
  router-core or runtime config.
- cheap extraction/classification may prefer free or low-cost verified rows.
- long-context work needs observed completion/streaming plus enough provider
  context.
- deterministic/eval work needs observed seed behavior, not just a declared
  `seed` parameter.
- judge/certifier lanes cannot end on free rows; free rows can object or
  tripwire.
- multimodal work needs an image probe receipt; declared vision alone is not
  enough.

Probe receipts live under each model row's `verification.probes`. They store
metadata only: status, latency, HTTP status, selected Switchback route, safe
headers, usage counts, and observed booleans. They do not store prompt/response
bodies or secrets.
