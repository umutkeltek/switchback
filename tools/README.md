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
