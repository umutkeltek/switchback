#!/usr/bin/env bun
// Build the DIRECT-PROVIDER cost registry from the authoritative research report
// (docs/registry/research-agent.md, dated 2026-05-30). This is the first-party
// pricing layer that COMPLEMENTS config/model-registry.json (which is derived
// from OpenRouter and only *approximates* first-party prices). Where the two
// disagree, this layer's official-page citations win.
//
// The data below mirrors the report's tables row-for-row so it stays auditable:
// diff this file against research-agent.md to verify any number. Prices in the
// source are USD per 1M tokens (as published); we emit integer MICRO-USD per
// Mtok ($5.00 -> 5_000_000) to honor the money-as-integer invariant shared with
// sb-core's Price ledger. null = not offered / not published / priced-per-host.
//
//   bun tools/build-direct-registry.ts   ->   config/provider-registry.json

const USD_MTOK_TO_MICROS = 1_000_000; // $/Mtok -> micro-USD/Mtok
const REPORT = "docs/registry/research-agent.md";
const REPORT_DATE = "2026-05-30";

const micros = (usd: number | null): number | null =>
  usd === null ? null : Math.round(usd * USD_MTOK_TO_MICROS);

type AuthScheme = "bearer" | "header:x-api-key" | "header:x-goog-api-key" | "header:api-key" | "sigv4" | "oauth2";
type Tier = "R" | "G" | "F" | "OW" | "R/G" | "G/F" | "G/R" | "R/G/F" | "OW/F" | "G (code)" | "F (code)" | "R/G (open)";

interface ProviderRow {
  id: string;
  name: string;
  base_url: string;
  auth_scheme: AuthScheme;
  openai_compatible: "native" | "yes" | "compat-endpoint" | "no";
  free_tier: boolean; // meaningful free tier for routing
}

// Third-party aggregators / fast-inference hosts (vs first-party model labs).
// Used to tag spread hosts so a router can gate them behind `allow_aggregator`.
const AGGREGATORS = new Set([
  "groq", "together", "fireworks", "deepinfra", "novita",
  "cerebras", "sambanova", "hyperbolic", "nebius", "openrouter",
]);

interface ModelRow {
  provider_id: string;
  model_id: string;
  tier: Tier;
  context_window: number | null;
  vision: boolean;
  tool_calling: boolean;
  json_schema: "native" | "tool-based" | false;
  input_usd: number | null;
  output_usd: number | null;
  cached_input_usd: number | null;
  source_url: string;
  flags?: string[]; // caveats: promo, mirrored, verify, etc.
  effective_to?: string; // for time-boxed promo prices
}

// --- Part 2: Providers (report lines 315-339) -------------------------------
const PROVIDERS: ProviderRow[] = [
  { id: "openai", name: "OpenAI", base_url: "https://api.openai.com/v1", auth_scheme: "bearer", openai_compatible: "native", free_tier: false },
  { id: "anthropic", name: "Anthropic (Claude)", base_url: "https://api.anthropic.com", auth_scheme: "header:x-api-key", openai_compatible: "compat-endpoint", free_tier: false },
  { id: "gemini", name: "Google Gemini API", base_url: "https://generativelanguage.googleapis.com", auth_scheme: "header:x-goog-api-key", openai_compatible: "compat-endpoint", free_tier: true }, // Flash/Flash-Lite free quota
  { id: "vertex", name: "Google Vertex AI", base_url: "https://{region}-aiplatform.googleapis.com", auth_scheme: "oauth2", openai_compatible: "compat-endpoint", free_tier: false },
  { id: "azure-openai", name: "Azure OpenAI / Foundry", base_url: "https://{resource}.openai.azure.com", auth_scheme: "header:api-key", openai_compatible: "yes", free_tier: false },
  { id: "bedrock", name: "AWS Bedrock", base_url: "https://bedrock-runtime.{region}.amazonaws.com", auth_scheme: "sigv4", openai_compatible: "no", free_tier: false },
  { id: "mistral", name: "Mistral AI", base_url: "https://api.mistral.ai/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // free experimentation tier
  { id: "cohere", name: "Cohere", base_url: "https://api.cohere.com/v2", auth_scheme: "bearer", openai_compatible: "no", free_tier: false },
  { id: "deepseek", name: "DeepSeek", base_url: "https://api.deepseek.com", auth_scheme: "bearer", openai_compatible: "yes", free_tier: false },
  { id: "xai", name: "xAI (Grok)", base_url: "https://api.x.ai/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: false },
  { id: "moonshot", name: "Moonshot (Kimi)", base_url: "https://api.moonshot.ai/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: false },
  { id: "zai", name: "Z.ai (GLM)", base_url: "https://api.z.ai/api/paas/v4", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // GLM Flash free
  { id: "alibaba", name: "Alibaba (Qwen)", base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true },
  { id: "groq", name: "Groq", base_url: "https://api.groq.com/openai/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // every model, no card
  { id: "together", name: "Together AI", base_url: "https://api.together.xyz/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // $5 credit
  { id: "fireworks", name: "Fireworks AI", base_url: "https://api.fireworks.ai/inference/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // $1 credit
  { id: "deepinfra", name: "DeepInfra", base_url: "https://api.deepinfra.com/v1/openai", auth_scheme: "bearer", openai_compatible: "yes", free_tier: false },
  { id: "novita", name: "Novita AI", base_url: "https://api.novita.ai/v3/openai", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true },
  { id: "cerebras", name: "Cerebras", base_url: "https://api.cerebras.ai/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // Llama/Qwen/gpt-oss 1M tok/day
  { id: "sambanova", name: "SambaNova", base_url: "https://api.sambanova.ai/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // persistent free
  { id: "hyperbolic", name: "Hyperbolic", base_url: "https://api.hyperbolic.xyz/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // Llama 4
  { id: "nebius", name: "Nebius AI Studio", base_url: "https://api.studio.nebius.com/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: false },
  { id: "openrouter", name: "OpenRouter", base_url: "https://openrouter.ai/api/v1", auth_scheme: "bearer", openai_compatible: "yes", free_tier: true }, // 28+ free models
];

// --- Part 1: Direct-provider models (report lines 30-231) -------------------
// Pricing URLs reused per provider to keep rows compact.
const U = {
  openai: "https://developers.openai.com/api/docs/pricing",
  openaiSecondary: "https://devtk.ai/en/blog/openai-api-pricing-guide-2026/",
  anthropic: "https://platform.claude.com/docs/en/about-claude/pricing",
  gemini: "https://ai.google.dev/gemini-api/docs/pricing",
  vertex: "https://cloud.google.com/vertex-ai/generative-ai/pricing",
  azure: "https://azure.microsoft.com/en-us/pricing/details/azure-openai/",
  bedrock: "https://aws.amazon.com/bedrock/pricing/",
  mistral: "https://mistral.ai/pricing/",
  cohere: "https://cohere.com/pricing",
  deepseek: "https://api-docs.deepseek.com/quick_start/pricing",
  xai43: "https://docs.x.ai/developers/models/grok-4.3",
  xai: "https://docs.x.ai/developers/models",
  moonshot: "https://tokenmix.ai/blog/kimi-k2-api-pricing",
  zai: "https://docs.z.ai/guides/overview/pricing",
  qwen: "https://pricepertoken.com/pricing-page/provider/qwen",
} as const;

const MODELS: ModelRow[] = [
  // OpenAI
  m("openai", "gpt-5.5", "G/R", 400_000, true, true, "native", 5.0, 30.0, 0.5, U.openai, ["~2x hike over GPT-5; batch/flex ~halve"]),
  m("openai", "gpt-5.5-pro", "R", 400_000, true, true, "native", 30.0, 180.0, null, U.openai),
  m("openai", "gpt-5.4", "G", 400_000, true, true, "native", 2.5, 15.0, 0.25, U.openai),
  m("openai", "gpt-5.4-pro", "R", 400_000, true, true, "native", 30.0, 180.0, null, U.openai),
  m("openai", "gpt-5.4-mini", "F", 400_000, true, true, "native", 0.75, 4.5, 0.075, U.openai),
  m("openai", "gpt-5.4-nano", "F", 400_000, true, true, "native", 0.2, 1.25, 0.02, U.openai),
  m("openai", "gpt-5", "G", 400_000, true, true, "native", 1.25, 10.0, 0.125, U.openaiSecondary, ["secondary-source: verify"]),
  m("openai", "gpt-5-mini", "F", 400_000, true, true, "native", 0.25, 2.0, null, U.openaiSecondary, ["secondary-source: verify"]),
  m("openai", "gpt-5-nano", "F", 128_000, true, true, "native", 0.05, 0.4, null, U.openaiSecondary, ["secondary-source: verify"]),
  m("openai", "gpt-5.2-codex", "G (code)", 400_000, true, true, "native", 1.75, 14.0, 0.175, U.openaiSecondary, ["secondary-source: verify"]),
  m("openai", "o3", "R", 200_000, true, true, "native", 2.0, 8.0, null, U.openaiSecondary, ["secondary-source: verify"]),
  m("openai", "o3-pro", "R", 200_000, true, true, "native", 20.0, 80.0, null, U.openaiSecondary, ["secondary-source: verify"]),
  m("openai", "gpt-oss-120b", "OW", 128_000, false, true, "native", null, null, null, U.openai, ["open-weight: priced per host"]),
  m("openai", "gpt-oss-20b", "OW", 128_000, false, true, "native", null, null, null, U.openai, ["open-weight: priced per host"]),

  // Anthropic — output = 5x input across the line; all 1M context
  m("anthropic", "claude-opus-4-8", "R/G", 1_000_000, true, true, "tool-based", 5.0, 25.0, 0.5, U.anthropic, ["NextOpus, live flagship"]),
  m("anthropic", "claude-opus-4-7", "R/G", 1_000_000, true, true, "tool-based", 5.0, 25.0, 0.5, U.anthropic),
  m("anthropic", "claude-opus-4-6", "R/G", 1_000_000, true, true, "tool-based", 5.0, 25.0, 0.5, U.anthropic),
  m("anthropic", "claude-opus-4-5", "R/G", 1_000_000, true, true, "tool-based", 5.0, 25.0, 0.5, U.anthropic),
  m("anthropic", "claude-sonnet-4-6", "G", 1_000_000, true, true, "tool-based", 3.0, 15.0, 0.3, U.anthropic),
  m("anthropic", "claude-sonnet-4-5", "G", 1_000_000, true, true, "tool-based", 3.0, 15.0, 0.3, U.anthropic),
  m("anthropic", "claude-haiku-4-5", "F", 200_000, true, true, "tool-based", 1.0, 5.0, 0.1, U.anthropic),

  // Google Gemini Developer API — Pro rows use the <=200K tier price
  m("gemini", "gemini-3.1-pro-preview", "R/G", 1_000_000, true, true, "native", 2.0, 12.0, 0.2, U.gemini, [">200K tier: 4.00/18.00"]),
  m("gemini", "gemini-3.5-flash", "G/F", 1_000_000, true, true, "native", 1.5, 9.0, 0.15, U.gemini),
  m("gemini", "gemini-3-flash-preview", "F", 1_000_000, true, true, "native", 0.5, 3.0, 0.05, U.gemini),
  m("gemini", "gemini-3.1-flash-lite", "F", 1_000_000, true, true, "native", 0.25, 1.5, 0.025, U.gemini),
  m("gemini", "gemini-2.5-pro", "G", 1_000_000, true, true, "native", 1.25, 10.0, 0.125, U.gemini, [">200K tier: 2.50/15.00"]),
  m("gemini", "gemini-2.5-flash", "F", 1_000_000, true, true, "native", 0.3, 2.5, 0.03, U.gemini),
  m("gemini", "gemini-2.5-flash-lite", "F", 1_000_000, true, true, "native", 0.1, 0.4, 0.01, U.gemini),

  // Azure OpenAI — Global-Standard parity with OpenAI list price
  m("azure-openai", "gpt-5.5", "G/R", 400_000, true, true, "native", 5.0, 30.0, 0.5, U.azure, ["mirrored from OpenAI list: verify"]),
  m("azure-openai", "gpt-5.4", "G", 400_000, true, true, "native", 2.5, 15.0, 0.25, U.azure, ["mirrored from OpenAI list: verify"]),
  m("azure-openai", "gpt-5", "G", 400_000, true, true, "native", 1.25, 10.0, 0.125, U.azure, ["mirrored: verify"]),

  // AWS Bedrock — Claude at parity with Anthropic direct
  m("bedrock", "anthropic.claude-opus-4-6", "R/G", 1_000_000, true, true, "tool-based", 5.0, 25.0, 0.5, U.bedrock),
  m("bedrock", "anthropic.claude-sonnet-4-6", "G", 1_000_000, true, true, "tool-based", 3.0, 15.0, 0.3, U.bedrock),
  m("bedrock", "anthropic.claude-haiku-4-5", "F", 200_000, true, true, "tool-based", 1.0, 5.0, 0.1, U.bedrock),
  m("bedrock", "amazon.nova-pro", "G", 300_000, true, true, "native", 0.8, 3.2, null, U.bedrock),
  m("bedrock", "amazon.nova-lite", "F", 300_000, true, true, "native", 0.06, 0.24, null, U.bedrock),
  m("bedrock", "amazon.nova-micro", "F", 128_000, false, true, "native", 0.035, 0.14, null, U.bedrock),
  m("bedrock", "meta.llama3-3-70b", "OW", 128_000, false, true, "native", 0.72, 0.72, null, U.bedrock),

  // Mistral — Large 3 corrected to 0.50/1.50 (two independent Oracle reports
  // cite mistral.ai + Bedrock parity; the agent's 2.00/6.00 was the medium tier).
  m("mistral", "mistral-large-3", "G", 256_000, true, true, "native", 0.5, 1.5, null, U.mistral, ["agent read 2.00/6.00; Oracle x2 + Bedrock parity = 0.50/1.50"]),
  m("mistral", "mistral-medium-3-5", "G/F", 256_000, true, true, "native", 1.5, 7.5, null, U.mistral, ["cache-hit = 10% of input"]),
  m("mistral", "mistral-small-4", "F", 256_000, false, true, "native", 0.15, 0.6, null, U.mistral),
  m("mistral", "ministral-14b", "OW/F", 256_000, true, true, "native", 0.2, 0.2, null, U.mistral),
  m("mistral", "mistral-small", "F", 128_000, true, true, "native", 0.2, 0.6, null, U.mistral),
  m("mistral", "codestral", "F (code)", 256_000, false, true, "native", 0.3, 0.9, null, U.mistral),
  m("mistral", "ministral-8b", "OW/F", 128_000, false, true, "native", 0.1, 0.1, null, U.mistral),
  m("mistral", "open-mistral-nemo", "OW/F", 128_000, false, true, "native", 0.02, 0.04, null, U.mistral),

  // Cohere
  m("cohere", "command-a", "G", 256_000, true, true, "native", 2.5, 10.0, null, U.cohere),
  m("cohere", "command-r-plus", "G", 128_000, false, true, "native", 2.5, 10.0, null, U.cohere),
  m("cohere", "command-r7b", "F", 128_000, false, true, "native", null, null, null, U.cohere, ["low, unspecified"]),

  // DeepSeek
  m("deepseek", "deepseek-v4-flash", "R/G/F", 1_000_000, false, true, "native", 0.14, 0.28, 0.0028, U.deepseek, ["aka deepseek-chat/-reasoner"]),
  { ...m("deepseek", "deepseek-v4-pro", "R/G", 1_000_000, false, true, "native", 0.435, 0.87, 0.003625, U.deepseek, ["75%-off promo; post-promo ~1.74/3.48/0.0145"]), effective_to: "2026-05-31T15:59:00Z" },

  // xAI (Grok)
  m("xai", "grok-4.3", "R/G", 1_000_000, true, true, "native", 1.25, 2.5, null, U.xai43, ["aggressively-priced flagship"]),
  m("xai", "grok-4", "R", 256_000, true, true, "native", 3.0, 15.0, null, U.xai, ["legacy"]),
  m("xai", "grok-4.1-fast", "F", 2_000_000, true, true, "native", 0.2, 0.5, 0.05, U.xai, ["bills at 4.3 rates now"]),

  // Moonshot (Kimi) — open-weights, also multi-hosted (see spread)
  m("moonshot", "kimi-k2.6", "R/G", 256_000, false, true, "native", 0.95, 4.0, 0.16, U.moonshot),
  m("moonshot", "kimi-k2.5", "G/F", 256_000, false, true, "native", 0.6, 3.0, 0.1, U.moonshot, ["value pick"]),

  // Z.ai (GLM) — open weights, multi-hosted
  m("zai", "glm-5.1", "R/G", 200_000, true, true, "native", 1.4, 4.4, 0.26, U.zai),
  m("zai", "glm-5", "R/G", 200_000, true, true, "native", 1.0, 3.2, 0.2, U.zai),
  m("zai", "glm-4.7", "G", 200_000, true, true, "native", 0.6, 2.2, 0.11, U.zai),
  m("zai", "glm-4.7-flashx", "F", 200_000, false, true, "native", 0.07, 0.4, 0.01, U.zai),
  m("zai", "glm-4.7-flash", "OW/F", 200_000, false, true, "native", 0.0, 0.0, 0.0, U.zai, ["FREE to registered users"]),
  m("zai", "glm-4.5-air", "OW/F", 128_000, false, true, "native", 0.2, 1.1, 0.03, U.zai),
  m("zai", "glm-4.5-flash", "OW/F", 128_000, false, true, "native", 0.0, 0.0, 0.0, U.zai, ["FREE to registered users"]),

  // Alibaba (Qwen)
  m("alibaba", "qwen3.7-max", "R/G", 256_000, false, true, "native", 2.5, 7.5, null, U.qwen, ["output estimate; verify on DashScope"]),
  m("alibaba", "qwen3.6-plus", "G", 1_000_000, true, true, "native", 0.325, 1.95, null, "https://openrouter.ai/qwen/qwen3.6-plus"),
  m("alibaba", "qwen3.5-plus", "G/F", 1_000_000, true, true, "native", 0.3, 1.8, null, U.qwen),
];

// --- Part 3: Open-weights cross-provider price spread (report lines 349-357) -
interface SpreadHost { provider_id: string; input_usd: number | null; output_usd: number | null; note?: string }
interface Spread { model: string; hosts: SpreadHost[]; note: string }

const SPREAD: Spread[] = [
  { model: "gpt-oss-120b", note: "Tight band; Groq adds cache-hit discount + speed.", hosts: [
    { provider_id: "together", input_usd: 0.15, output_usd: 0.6 },
    { provider_id: "groq", input_usd: 0.15, output_usd: 0.6, note: "cache 0.075" },
    { provider_id: "fireworks", input_usd: 0.2, output_usd: null },
    { provider_id: "sambanova", input_usd: 0.22, output_usd: 0.59, note: "Oracle-verified" },
    { provider_id: "cerebras", input_usd: 0.35, output_usd: 0.75, note: "Oracle-verified" },
  ]},
  { model: "gpt-oss-20b", note: "~1.5x spread on input.", hosts: [
    { provider_id: "together", input_usd: 0.05, output_usd: 0.2 },
    { provider_id: "groq", input_usd: 0.075, output_usd: 0.3 },
  ]},
  { model: "llama-3.3-70b", note: "~9x input spread for identical weights — host choice dominates.", hosts: [
    { provider_id: "cerebras", input_usd: 0.1, output_usd: null, note: "fastest" },
    { provider_id: "hyperbolic", input_usd: 0.4, output_usd: null },
    { provider_id: "groq", input_usd: 0.59, output_usd: 0.79 },
    { provider_id: "bedrock", input_usd: 0.72, output_usd: 0.72 },
    { provider_id: "fireworks", input_usd: 0.9, output_usd: 0.9 },
  ]},
  { model: "deepseek-v4-pro", note: "Direct API ~4-5x cheaper than aggregator markup (promo).", hosts: [
    { provider_id: "deepseek", input_usd: 0.435, output_usd: 0.87, note: "promo to 2026-05-31 15:59 UTC" },
    { provider_id: "novita", input_usd: 1.6, output_usd: 3.2, note: "Oracle-verified" },
    { provider_id: "deepinfra", input_usd: 1.74, output_usd: 3.48, note: "Oracle-verified; = DeepSeek post-promo regular" },
    { provider_id: "together", input_usd: 2.1, output_usd: 4.4 },
  ]},
  { model: "deepseek-v4-flash", note: "Direct and Novita tie on headline; DeepSeek has 10x cheaper cache.", hosts: [
    { provider_id: "deepseek", input_usd: 0.14, output_usd: 0.28, note: "cache-hit 0.0028" },
    { provider_id: "novita", input_usd: 0.14, output_usd: 0.28, note: "cache-hit 0.028 (Oracle-verified)" },
  ]},
  { model: "glm-5", note: "Z.ai direct ties with Bedrock on headline token price.", hosts: [
    { provider_id: "zai", input_usd: 1.0, output_usd: 3.2, note: "cache 0.20" },
    { provider_id: "bedrock", input_usd: 1.0, output_usd: 3.2, note: "Oracle-verified (US regions)" },
  ]},
  { model: "mistral-large-3", note: "Direct Mistral ties with Bedrock; both 0.50/1.50 (Oracle x2).", hosts: [
    { provider_id: "mistral", input_usd: 0.5, output_usd: 1.5, note: "cache-hit = 10% input" },
    { provider_id: "bedrock", input_usd: 0.5, output_usd: 1.5, note: "Oracle-verified" },
  ]},
  { model: "kimi-k2.6", note: "Direct cheaper on input; aggregators add convenience.", hosts: [
    { provider_id: "moonshot", input_usd: 0.95, output_usd: 4.0 },
    { provider_id: "together", input_usd: 1.2, output_usd: 4.5 },
  ]},
  { model: "glm-5.1", note: "Aggregator can undercut the first party.", hosts: [
    { provider_id: "fireworks", input_usd: 0.9, output_usd: null },
    { provider_id: "together", input_usd: 1.4, output_usd: 4.4 },
    { provider_id: "zai", input_usd: 1.4, output_usd: 4.4 },
  ]},
];

function m(
  provider_id: string, model_id: string, tier: Tier, context_window: number | null,
  vision: boolean, tool_calling: boolean, json_schema: ModelRow["json_schema"],
  input_usd: number | null, output_usd: number | null, cached_input_usd: number | null,
  source_url: string, flags?: string[],
): ModelRow {
  return { provider_id, model_id, tier, context_window, vision, tool_calling, json_schema, input_usd, output_usd, cached_input_usd, source_url, flags };
}

// --- Emit -------------------------------------------------------------------
const models = MODELS.map((r) => ({
  provider_id: r.provider_id,
  model_id: r.model_id,
  tier: r.tier,
  context_window: r.context_window,
  vision: r.vision,
  tool_calling: r.tool_calling,
  json_schema: r.json_schema,
  input_micros_per_mtok: micros(r.input_usd),
  output_micros_per_mtok: micros(r.output_usd),
  cached_input_micros_per_mtok: micros(r.cached_input_usd),
  source_url: r.source_url,
  ...(r.flags?.length ? { flags: r.flags } : {}),
  ...(r.effective_to ? { effective_to: r.effective_to } : {}),
}));

// by_model: every host of a model_id, cheapest input first — the routing signal.
const byModel: Record<string, { provider_id: string; input_micros_per_mtok: number | null; output_micros_per_mtok: number | null; source: "direct" | "spread"; note?: string }[]> = {};
const push = (model: string, e: (typeof byModel)[string][number]) => {
  (byModel[model] ??= []).push(e);
};
for (const r of MODELS) {
  push(r.model_id, { provider_id: r.provider_id, input_micros_per_mtok: micros(r.input_usd), output_micros_per_mtok: micros(r.output_usd), source: "direct" });
}
for (const s of SPREAD) {
  for (const h of s.hosts) {
    push(s.model, { provider_id: h.provider_id, input_micros_per_mtok: micros(h.input_usd), output_micros_per_mtok: micros(h.output_usd), source: "spread", ...(h.note ? { note: h.note } : {}) });
  }
}
for (const k of Object.keys(byModel)) {
  byModel[k].sort((a, b) => (a.input_micros_per_mtok ?? Infinity) - (b.input_micros_per_mtok ?? Infinity));
}

// Models a router must NOT send fresh production traffic to (Oracle-verified
// deprecations/retirements across providers). Kept as data so routing can warn.
const DEPRECATED = [
  { model: "gemini-2.0-flash", providers: ["gemini", "vertex"], status: "deprecated", note: "shutdown soon (Oracle: oracle-providers.md)" },
  { model: "claude-3-7-sonnet", providers: ["anthropic", "bedrock", "vertex"], status: "retired", note: "retired across all three surfaces" },
];

// Cheapest rows are often cheapest only because they are batch/free/promo/
// aggregator lanes. A router should gate them behind explicit policy flags
// rather than treating them as default interactive pricing (Oracle insight).
const ROUTING_POLICY_FLAGS = {
  allow_batch: "Vertex Flex/Batch (e.g. gemini-2.5-pro 0.625/5.00 vs 1.25/10.00 standard) — batch semantics only",
  allow_promo: "time-boxed promo pricing (e.g. deepseek-v4-pro until 2026-05-31 15:59 UTC) — expires",
  allow_aggregator: "third-party hosts of open weights (Together/Fireworks/Novita/...) vs first-party",
  allow_free: "free tiers / free routes (Groq, Cerebras, OpenRouter :free, Z.ai Flash) — non-SLA only",
};

const out = {
  schema: "switchback/provider-registry@1",
  generated: REPORT_DATE,
  sources: [
    { file: REPORT, kind: "agent (WebSearch/WebFetch)", role: "primary breadth: ~60 models, current generation" },
    { file: "docs/registry/oracle-cost-map.md", kind: "ChatGPT Deep Research (Pro)", role: "corroboration + aggregator/Bedrock concrete rows" },
    { file: "docs/registry/oracle-providers.md", kind: "ChatGPT Deep Research (Pro)", role: "deprecations + Vertex batch tiers + policy flags" },
  ],
  money: "integer micro-USD per 1M tokens (USD/Mtok * 1e6); null = not offered / priced-per-host",
  note: "Direct first-party authoritative layer. Complements config/model-registry.json (OpenRouter-derived). On conflict, this layer's official-page citations win. Cross-check `flags` rows before treating any price as billing-grade. See docs/registry/RECONCILIATION.md for cross-source agreement + discrepancies.",
  counts: { providers: PROVIDERS.length, models: models.length, multi_hosted: SPREAD.length },
  routing_policy_flags: ROUTING_POLICY_FLAGS,
  deprecated: DEPRECATED,
  providers: PROVIDERS.map((p) => ({ ...p, aggregator: AGGREGATORS.has(p.id) })),
  models,
  spread: SPREAD.map((s) => ({
    model: s.model,
    note: s.note,
    hosts: s.hosts.map((h) => ({ provider_id: h.provider_id, input_micros_per_mtok: micros(h.input_usd), output_micros_per_mtok: micros(h.output_usd), ...(h.note ? { note: h.note } : {}) })),
  })),
  by_model: byModel,
};

const path = "config/provider-registry.json";
await Bun.write(path, JSON.stringify(out, null, 2) + "\n");
console.log(`wrote ${path}: ${out.counts.providers} providers, ${out.counts.models} direct models, ${out.counts.multi_hosted} multi-hosted spreads`);
console.log(`by_model keys: ${Object.keys(byModel).length}`);
