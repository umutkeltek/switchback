#!/usr/bin/env bun
// Builds switchback's model + cost registry from OpenRouter's public catalog
// (/models) and per-model provider endpoints (/endpoints). The output is the
// routing cost map: which providers serve each model and at what price, so the
// router can later pick the cheapest source for a requested model.
//
// No hardcoded model names — the comprehensive catalog and the cross-provider
// price spread are derived from the LIVE catalog, so it stays current as the
// model landscape moves. Prices are micro-USD per 1M tokens (integer), matching
// sb-core::catalog::Price.unit_price_micros_per_mtok. Re-run to refresh.
//
//   curl -s https://openrouter.ai/api/v1/models -o docs/registry/openrouter-models.json
//   bun tools/build-registry.ts
//
// Output: config/model-registry.json (tracked: the routing cost map)

const MODELS_JSON = "docs/registry/openrouter-models.json";
const OUT = "config/model-registry.json";
const MAX_SPREAD_FETCHES = 140; // cap endpoint requests; raise to cover more
const CONCURRENCY = 8;

// OpenRouter price strings are USD per token; *1e12 = micro-USD per 1M tokens.
const toMicrosPerMtok = (perToken: string | undefined): number | null =>
  perToken == null ? null : Math.round(parseFloat(perToken) * 1e12);

// Authors whose models are open-weights and therefore likely multi-hosted (the
// cross-provider price spread is only meaningful for these). Closed models
// (openai/anthropic/gemini) have one first-party source — their price is in the
// full catalog below.
const OPEN_AUTHORS = new Set([
  "meta-llama", "deepseek", "qwen", "mistralai", "moonshotai", "z-ai", "nvidia",
  "microsoft", "nousresearch", "01-ai", "cohere", "ai21", "minimax", "baidu",
  "stepfun", "inclusionai", "arcee-ai", "thudm", "openchat",
]);
const isOpen = (id: string) =>
  OPEN_AUTHORS.has(id.split("/")[0]) || id.startsWith("google/gemma");

type Prov = { id: string; base_url: string | null; auth: string; openai_compatible: boolean };
const PROVIDERS: Record<string, Prov> = {
  OpenAI: { id: "openai", base_url: "https://api.openai.com/v1", auth: "bearer", openai_compatible: true },
  Anthropic: { id: "anthropic", base_url: "https://api.anthropic.com", auth: "header:x-api-key", openai_compatible: false },
  "Google AI Studio": { id: "gemini", base_url: "https://generativelanguage.googleapis.com", auth: "header:x-goog-api-key", openai_compatible: false },
  "Google Vertex": { id: "vertex", base_url: null, auth: "bearer", openai_compatible: false },
  Groq: { id: "groq", base_url: "https://api.groq.com/openai/v1", auth: "bearer", openai_compatible: true },
  Together: { id: "together", base_url: "https://api.together.xyz/v1", auth: "bearer", openai_compatible: true },
  Fireworks: { id: "fireworks", base_url: "https://api.fireworks.ai/inference/v1", auth: "bearer", openai_compatible: true },
  DeepInfra: { id: "deepinfra", base_url: "https://api.deepinfra.com/v1/openai", auth: "bearer", openai_compatible: true },
  Novita: { id: "novita", base_url: "https://api.novita.ai/v3/openai", auth: "bearer", openai_compatible: true },
  Cerebras: { id: "cerebras", base_url: "https://api.cerebras.ai/v1", auth: "bearer", openai_compatible: true },
  SambaNova: { id: "sambanova", base_url: "https://api.sambanova.ai/v1", auth: "bearer", openai_compatible: true },
  Hyperbolic: { id: "hyperbolic", base_url: "https://api.hyperbolic.xyz/v1", auth: "bearer", openai_compatible: true },
  Nebius: { id: "nebius", base_url: "https://api.studio.nebius.ai/v1", auth: "bearer", openai_compatible: true },
  DeepSeek: { id: "deepseek", base_url: "https://api.deepseek.com", auth: "bearer", openai_compatible: true },
  Mistral: { id: "mistral", base_url: "https://api.mistral.ai/v1", auth: "bearer", openai_compatible: true },
  xAI: { id: "xai", base_url: "https://api.x.ai/v1", auth: "bearer", openai_compatible: true },
};
const slug = (name: string) => name.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/(^-|-$)/g, "");
const provFor = (name: string): Prov =>
  PROVIDERS[name] ?? { id: slug(name), base_url: null, auth: "bearer", openai_compatible: true };

const caps = (params: string[] = [], inputMods: string[] = []) => ({
  tool_calling: params.includes("tools"),
  json_schema: params.includes("structured_outputs") || params.includes("response_format"),
  vision: inputMods.includes("image"),
  modalities: ["text_in", "text_out", ...(inputMods.includes("image") ? ["vision_in"] : [])],
});

type Offering = {
  provider: string;
  model: string;
  context_window: number | null;
  tool_calling: boolean;
  json_schema: boolean;
  vision: boolean;
  input_micros_per_mtok: number | null;
  output_micros_per_mtok: number | null;
  cached_input_micros_per_mtok: number | null;
};

async function fetchEndpoints(modelId: string): Promise<Offering[]> {
  try {
    const res = await fetch(`https://openrouter.ai/api/v1/models/${modelId}/endpoints`);
    if (!res.ok) return [];
    const json: any = await res.json();
    const eps = json?.data?.endpoints ?? [];
    const inputMods: string[] = json?.data?.architecture?.input_modalities ?? [];
    return eps.map((e: any): Offering => ({
      provider: provFor(e.provider_name).id,
      model: e.provider_name,
      context_window: e.context_length ?? null,
      ...caps(e.supported_parameters ?? [], inputMods),
      input_micros_per_mtok: toMicrosPerMtok(e.pricing?.prompt),
      output_micros_per_mtok: toMicrosPerMtok(e.pricing?.completion),
      cached_input_micros_per_mtok: toMicrosPerMtok(e.pricing?.input_cache_read),
    }));
  } catch {
    return [];
  }
}

async function pool<T, R>(items: T[], n: number, fn: (t: T) => Promise<R>): Promise<R[]> {
  const out: R[] = new Array(items.length);
  let i = 0;
  await Promise.all(
    Array.from({ length: n }, async () => {
      while (i < items.length) {
        const idx = i++;
        out[idx] = await fn(items[idx]);
      }
    }),
  );
  return out;
}

async function main() {
  const catalog: any = JSON.parse(await Bun.file(MODELS_JSON).text());

  // (1) The comprehensive model catalog — every model + base price/caps.
  const models = catalog.data.map((m: any) => ({
    id: m.id,
    name: m.name,
    context_window: m.context_length ?? null,
    ...caps(m.supported_parameters, m.architecture?.input_modalities),
    input_micros_per_mtok: toMicrosPerMtok(m.pricing?.prompt),
    output_micros_per_mtok: toMicrosPerMtok(m.pricing?.completion),
    cached_input_micros_per_mtok: toMicrosPerMtok(m.pricing?.input_cache_read),
  }));

  // (2) The cross-provider price spread — derived from the LIVE catalog's open
  // models (no hardcoded names), keyed by the actual OpenRouter model id.
  const openIds: string[] = catalog.data
    .map((m: any) => m.id)
    .filter((id: string) => isOpen(id) && !id.endsWith(":free"))
    .slice(0, MAX_SPREAD_FETCHES);
  process.stderr.write(`fetching endpoints for ${openIds.length} open models...\n`);

  const results = await pool(openIds, CONCURRENCY, fetchEndpoints);
  const by_model: Record<string, Offering[]> = {};
  openIds.forEach((id, i) => {
    const offs = results[i];
    if (offs.length >= 2) {
      offs.sort((a, b) => (a.input_micros_per_mtok ?? 1e18) - (b.input_micros_per_mtok ?? 1e18));
      by_model[id] = offs;
    }
  });

  const providerIds = [...new Set(Object.values(by_model).flat().map((o) => o.provider))].sort();
  const providers = providerIds.map(
    (id) => Object.values(PROVIDERS).find((p) => p.id === id) ?? { id, base_url: null, auth: "bearer", openai_compatible: true },
  );

  const registry = {
    source: "openrouter /models + /endpoints",
    note: "prices = micro-USD per 1M tokens (integer). by_model = same model across providers, cheapest input first.",
    models, // comprehensive catalog (every model + base price/caps)
    providers, // serving providers referenced by the spread
    by_model, // model id -> provider offerings sorted cheapest input first (the cost-routing map)
  };
  await Bun.write(OUT, JSON.stringify(registry, null, 2));
  process.stderr.write(
    `wrote ${OUT}: ${models.length} models, ${providers.length} providers, ${Object.keys(by_model).length} multi-hosted models\n`,
  );
}

main();
