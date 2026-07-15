#!/usr/bin/env bun
import { readFile, writeFile } from "node:fs/promises";

type Json = Record<string, any>;

const DEFAULT_REGISTRY = "config/provider-registry.json";
const OPENROUTER_MODELS_URL = "https://openrouter.ai/api/v1/models?output_modalities=all";
const NVIDIA_MODELS_URL = "https://integrate.api.nvidia.com/v1/models";
const CEREBRAS_PUBLIC_MODELS_URL = "https://api.cerebras.ai/public/v1/models";
const GROQ_MODELS_URL = "https://api.groq.com/openai/v1/models";
const NEURALWATT_DOCS_URL = "https://portal.neuralwatt.com/docs";
const NEURALWATT_PRICING_URL = "https://portal.neuralwatt.com/pricing";
const NEURALWATT_ENERGY_URL = "https://portal.neuralwatt.com/energy-pricing";
const FETCHED_AT = process.env.SWITCHBACK_REGISTRY_FETCHED_AT || new Date().toISOString();

const NVIDIA_OPENAI_COMPATIBLE_DEFAULTS: Json = {
  capabilities: {
    declared_by: "nvidia_openai_compatible_api",
    input_modalities: ["text"],
    output_modalities: ["text"],
    supported_parameters: ["stream", "max_tokens", "temperature", "top_p"],
    text_input: true,
    text_output: true,
    video_input: false,
    audio_input: false,
    embeddings_output: false,
    rerank_output: false,
    temperature: true,
    top_p: true,
    max_tokens: true,
    seed: false,
    json_schema: "unknown",
  },
  determinism: {
    seed_supported: false,
    temperature_supported: true,
    top_p_supported: true,
    note: "NVIDIA Build exposes OpenAI-compatible sampling controls; no catalog-level seed determinism guarantee is captured yet.",
  },
  limits: {
    free_tier_rpm_reported: 40,
    free_tier_rpm_source: "nvidia_developer_forum_reports",
    note: "Treat hosted NVIDIA Build free endpoints as prototyping lanes; verify account credits and live rate headers before unattended batches.",
  },
};

const INDEPENDENT_PROVIDER_IDS = [
  "groq",
  "together",
  "fireworks",
  "deepinfra",
  "novita",
  "cerebras",
  "sambanova",
  "hyperbolic",
  "nebius",
  "neuralwatt",
];

const INDEPENDENT_PROVIDER_RESEARCH: Record<string, Json> = {
  groq: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "mostly_openai_compatible",
      docs_url: "https://console.groq.com/docs/openai",
      models_url: "https://console.groq.com/docs/models",
      pricing_url: "https://console.groq.com/docs/models",
      rate_limits_url: "https://console.groq.com/docs/rate-limits",
      official_base_url: "https://api.groq.com/openai/v1",
      catalog_endpoint: "https://api.groq.com/openai/v1/models",
      catalog_auth: "bearer_required",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        responses_api: true,
        streaming: true,
        tool_calling: true,
        structured_outputs: true,
        prompt_caching: true,
        text_output: true,
        image_input: "model_dependent",
        audio: "speech_to_text_and_text_to_speech",
        built_in_tools: ["web_search", "code_execution", "wolfram_alpha", "mcp"],
      },
      determinism_declared: {
        seed: "not_declared",
        temperature_zero: "converted_to_1e-8",
      },
      routing_notes: [
        "Good scout lane for fast open-weight text and agentic systems; verify per-model limits from the models page before production routing.",
        "Do not treat temperature=0 as exact determinism; Groq documents conversion to a tiny nonzero value.",
      ],
    },
    provider_sources: [
      source("https://console.groq.com/docs/openai", "provider_docs", "OpenAI compatibility, base URL, unsupported parameters, Responses API."),
      source("https://console.groq.com/docs/models", "provider_catalog", "Model IDs, pricing, rate limits, context and output limits."),
      source("https://console.groq.com/docs/tool-use/overview", "provider_docs", "Tool-use surface uses JSON-schema tool definitions."),
    ],
  },
  neuralwatt: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: NEURALWATT_DOCS_URL,
      models_url: "https://portal.neuralwatt.com/models",
      pricing_url: NEURALWATT_PRICING_URL,
      rate_limits_url: NEURALWATT_DOCS_URL,
      energy_pricing_url: NEURALWATT_ENERGY_URL,
      official_base_url: "https://api.neuralwatt.com/v1",
      catalog_endpoint: "https://api.neuralwatt.com/v1/models",
      catalog_auth: "bearer_required",
      catalog_status: "pricing_page_ingested",
      capabilities_declared: {
        chat_completions: true,
        streaming: true,
        tool_calling: true,
        structured_outputs: "model_dependent",
        image_input: "model_dependent",
        energy_reporting: true,
        prompt_caching: true,
      },
      determinism_declared: {
        seed: "not_confirmed_in_provider_docs",
      },
      routing_notes: [
        "Energy pricing is a separate metered cost dimension; token price rows are not enough to estimate real request cost.",
        "Energy averages are rolling observed values from live traffic, so refresh before promoting routes that optimize for energy cost.",
      ],
    },
    provider_sources: [
      source(NEURALWATT_DOCS_URL, "provider_docs", "OpenAI-compatible base URL and chat completions examples."),
      source(NEURALWATT_PRICING_URL, "model_pricing_docs", "Token prices, cached-input prices, context windows and feature badges."),
      source(NEURALWATT_ENERGY_URL, "energy_status", "Trailing observed energy averages by model and prompt-size band."),
    ],
  },
  together: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: "https://docs.together.ai/docs/inference/openai-compatibility",
      models_url: "https://docs.together.ai/docs/serverless/models",
      pricing_url: "https://www.together.ai/pricing",
      official_base_url: "https://api.together.ai/v1",
      catalog_endpoint: "https://api.together.ai/v1/models",
      catalog_auth: "bearer_required",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        completions: true,
        streaming: true,
        vision: true,
        image_generation: true,
        text_to_speech: true,
        speech_to_text: true,
        embeddings: true,
        tool_calling: true,
        structured_outputs: true,
        reasoning_controls: true,
        logprobs: true,
        video_generation: "together_native",
        moderation: "chat_completion_model",
      },
      unsupported_openai_resources: ["assistants", "threads", "runs", "openai_shaped_batch", "openai_shaped_files"],
      determinism_declared: {
        seed: "best_effort_not_guaranteed",
      },
      routing_notes: [
        "Broad open-model host with serverless and dedicated endpoints; batch discounts are a native Together path, not OpenAI-shaped /v1 batches.",
        "Seed support is best-effort only, so deterministic eval lanes need Switchback probes.",
      ],
    },
    provider_sources: [
      source("https://docs.together.ai/docs/inference/openai-compatibility", "provider_docs", "OpenAI-compatible endpoints, base URL, capability matrix and seed caveat."),
      source("https://docs.together.ai/docs/serverless/models", "provider_catalog", "Serverless catalog, model categories, pricing and rate-limit notes."),
    ],
  },
  fireworks: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: "https://docs.fireworks.ai/guides/querying-text-models",
      models_url: "https://fireworks.ai/models",
      pricing_url: "https://fireworks.ai/pricing",
      official_base_url: "https://api.fireworks.ai/inference/v1",
      catalog_endpoint: "catalog_url_only",
      catalog_auth: "dashboard_or_api_key_dependent",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        completions: true,
        responses_api: true,
        streaming: true,
        tool_calling: true,
        structured_outputs: ["json_schema", "grammar"],
        vision: true,
        embeddings: true,
        dedicated_deployments: true,
        anthropic_messages_compatibility: true,
      },
      determinism_declared: {
        seed: "not_confirmed_in_provider_docs",
      },
      routing_notes: [
        "Strong host for open-source text models with both serverless and dedicated deployments.",
        "Structured output support is explicit, but deterministic eval routes still need probes.",
      ],
    },
    provider_sources: [
      source("https://docs.fireworks.ai/guides/querying-text-models", "provider_docs", "OpenAI-compatible text model API and base URL."),
      source("https://docs.fireworks.ai/guides/function-calling", "provider_docs", "OpenAI-compatible function/tool calling."),
      source("https://docs.fireworks.ai/structured-responses/structured-response-formatting", "provider_docs", "JSON schema and grammar structured outputs."),
    ],
  },
  deepinfra: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: "https://docs.deepinfra.com/chat/overview",
      models_url: "https://deepinfra.com/models",
      pricing_url: "https://deepinfra.com/pricing",
      official_base_url: "https://api.deepinfra.com/v1/openai",
      catalog_endpoint: "catalog_url_only",
      catalog_auth: "dashboard_or_api_key_dependent",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        streaming: true,
        completions: true,
        embeddings: true,
        reranking: true,
        image_generation: true,
        speech_recognition: true,
        text_to_speech: true,
        tool_calling: true,
        structured_outputs: true,
        vision: true,
        prompt_caching: true,
        reasoning_effort: true,
        priority_service_tier: true,
      },
      determinism_declared: {
        seed: "not_declared",
        temperature_top_p: true,
      },
      routing_notes: [
        "Useful as a price cross-check host because pricing pages expose per-model context, cached-input prices, and current token prices.",
        "Priority tier can add surcharge; score routing should separate standard and priority economics.",
      ],
    },
    provider_sources: [
      source("https://docs.deepinfra.com/chat/overview", "provider_docs", "OpenAI-compatible chat completions, base URL and supported parameter surface."),
      source("https://deepinfra.com/pricing", "model_pricing_docs", "Per-model pricing, context windows and cached-input prices."),
    ],
  },
  novita: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: "https://novita.ai/docs/api-reference/api-reference-overview",
      models_url: "https://novita.ai/docs/api-reference/model-apis-llm-list-models",
      pricing_url: "https://novita.ai/models/llm",
      official_base_url: "https://api.novita.ai/openai",
      previous_base_url: "https://api.novita.ai/v3/openai",
      catalog_endpoint: "https://api.novita.ai/openai/v1/models",
      catalog_auth: "bearer_required",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        completions: true,
        embeddings: true,
        rerank: true,
        batch: true,
        list_models: true,
        retrieve_model: true,
        tool_calling: "model_dependent",
        image_generation: true,
        image_editing: true,
        video_generation: true,
        text_to_speech: true,
        speech_recognition: true,
      },
      determinism_declared: {
        seed: "not_confirmed_in_llm_docs",
      },
      routing_notes: [
        "Official 2026 docs use https://api.novita.ai/openai for OpenAI-compatible LLM APIs; the older /v3/openai URL is retained only as provenance.",
        "List Models returns pricing and context size, so this provider is a good candidate for an auth-backed catalog adapter.",
      ],
    },
    provider_sources: [
      source("https://novita.ai/docs/api-reference/api-reference-overview", "provider_docs", "Official base URLs and LLM API endpoint groups."),
      source("https://novita.ai/docs/api-reference/model-apis-llm-list-models", "provider_catalog", "Authenticated OpenAI-compatible model list with prices and context sizes."),
      source("https://novita.ai/docs/guides/llm-function-calling", "provider_docs", "Function calling guide and supported-model framing."),
    ],
  },
  cerebras: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "mostly_openai_compatible",
      docs_url: "https://inference-docs.cerebras.ai/resources/openai",
      models_url: "https://inference-docs.cerebras.ai/models/overview",
      pricing_url: "https://inference-docs.cerebras.ai/models/overview",
      rate_limits_url: "https://inference-docs.cerebras.ai/support/rate-limits",
      official_base_url: "https://api.cerebras.ai/v1",
      catalog_endpoint: "https://api.cerebras.ai/public/v1/models",
      catalog_auth: "none_for_public_catalog",
      catalog_status: "public_catalog_available_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        streaming: true,
        function_calling: true,
        structured_outputs: true,
        json_mode: true,
        reasoning: true,
        prompt_caching: true,
        image_input: "preview_or_model_dependent",
        batch: true,
        metrics: true,
        seed: true,
      },
      determinism_declared: {
        seed: true,
        probe_required: true,
      },
      routing_notes: [
        "Public endpoint models are documented as free subject to rate limits; use as fast/free scout, not final certifier without probes.",
        "Public catalog exposes supported parameters, pricing, capabilities and architecture without an API key.",
      ],
    },
    provider_sources: [
      source("https://inference-docs.cerebras.ai/resources/openai", "provider_docs", "OpenAI compatibility and base URL."),
      source("https://inference-docs.cerebras.ai/models/overview", "provider_catalog", "Public model catalog and free public endpoint note."),
      source("https://inference-docs.cerebras.ai/api-reference/models/public-models", "provider_catalog", "No-auth public model API with pricing, capabilities and supported parameters."),
    ],
  },
  sambanova: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: "https://docs.sambanova.ai/docs/en/get-started/overview",
      models_url: "https://docs.sambanova.ai/docs/en/models/sambacloud-models",
      pricing_url: "account_or_cloud_plan_dependent",
      rate_limits_url: "https://docs.sambanova.ai/docs/en/models/rate-limits",
      official_base_url: "https://api.sambanova.ai/v1",
      catalog_endpoint: "docs_catalog_only",
      catalog_auth: "bearer_required_for_api",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        streaming: true,
        openai_client: true,
        function_calling: true,
        json_mode: true,
        responses_api: true,
        vision: "model_dependent",
        audio: true,
        embeddings: true,
      },
      determinism_declared: {
        seed: "not_confirmed_in_provider_docs",
      },
      routing_notes: [
        "SambaCloud catalog is small and explicit; free-tier rate limits include request/day and token/day concepts.",
        "Use as independent host cross-check for MiniMax, DeepSeek, Llama and GPT-OSS model availability.",
      ],
    },
    provider_sources: [
      source("https://docs.sambanova.ai/docs/en/get-started/overview", "provider_docs", "Developer guide and OpenAI compatibility pointers."),
      source("https://docs.sambanova.ai/docs/en/models/sambacloud-models", "provider_catalog", "SambaCloud model IDs, context lengths and modalities."),
      source("https://docs.sambanova.ai/docs/en/models/rate-limits", "provider_docs", "Rate-limit dimensions and free-tier token/day note."),
    ],
  },
  hyperbolic: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: "https://www.hyperbolic.ai/docs/inference/overview",
      models_url: "https://www.hyperbolic.ai/docs/inference/text-apis",
      pricing_url: "https://www.hyperbolic.ai/docs/inference/overview",
      rate_limits_url: "https://www.hyperbolic.ai/docs/inference/overview",
      official_base_url: "https://api.hyperbolic.xyz/v1",
      catalog_endpoint: "docs_catalog_only",
      catalog_auth: "bearer_required_for_api",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        completions: true,
        streaming: true,
        tool_calling: true,
        structured_outputs: true,
        json_schema_validation: true,
        batch: true,
        text_generation: true,
        image_generation: true,
        vision: true,
        audio: true,
        zero_data_retention: true,
      },
      determinism_declared: {
        seed: "not_confirmed_in_provider_docs",
        temperature_top_p: true,
      },
      routing_notes: [
        "Good independent scout host for open models; docs expose basic tier RPM and IP caps that routing can use after adapter support.",
        "Model deprecation list must be watched because docs mark several popular open models for sunset.",
      ],
    },
    provider_sources: [
      source("https://www.hyperbolic.ai/docs/inference/overview", "provider_docs", "Serverless inference overview, base URL, tiers, capabilities and deprecation notes."),
      source("https://www.hyperbolic.ai/docs/inference/text-apis", "provider_docs", "Chat completions endpoint, streaming and supported request parameters."),
      source("https://www.hyperbolic.ai/docs/inference/integrations", "provider_docs", "OpenAI SDK integration and base URL."),
    ],
  },
  nebius: {
    provider_research: {
      status: "official_docs_cross_checked",
      host_type: "independent_inference_host",
      api_shape: "openai_compatible",
      docs_url: "https://docs.tokenfactory.nebius.com/quickstart",
      models_url: "https://tokenfactory.nebius.com",
      pricing_url: "https://nebius.com/services/token-factory",
      official_base_url: "https://api.tokenfactory.nebius.com/v1",
      previous_base_url: "https://api.studio.nebius.com/v1",
      catalog_endpoint: "https://api.tokenfactory.nebius.com/v1/models",
      catalog_auth: "bearer_required",
      catalog_status: "provider_catalog_not_ingested",
      capabilities_declared: {
        chat_completions: true,
        streaming: true,
        image_inputs: true,
        vision: true,
        reasoning_models: true,
        function_calling: true,
        structured_outputs: true,
        safety_guardrails: true,
        embeddings: true,
        fine_tuning: true,
      },
      determinism_declared: {
        seed: "not_confirmed_in_official_docs",
      },
      routing_notes: [
        "Nebius public docs now point to Token Factory rather than the older AI Studio base URL; registry keeps the previous URL as provenance only.",
        "Treat as an EU-governed independent host candidate for open models after auth-backed catalog import.",
      ],
    },
    provider_sources: [
      source("https://docs.tokenfactory.nebius.com/quickstart", "provider_docs", "Token Factory quickstart, base URL and OpenAI-compatible API positioning."),
      source("https://nebius.com/services/token-factory", "provider_catalog", "Model families, multimodal/reasoning coverage, function calling and pricing positioning."),
    ],
  },
};

const args = new Set(process.argv.slice(2));
const valueAfter = (flag: string, fallback: string | null = null): string | null => {
  const argv = process.argv.slice(2);
  const idx = argv.indexOf(flag);
  return idx >= 0 ? argv[idx + 1] ?? fallback : fallback;
};

const registryPath = valueAfter("--registry", DEFAULT_REGISTRY)!;
const outPath = valueAfter("--out", registryPath)!;
const apply = args.has("--apply");
const checkOnly = args.has("--check");
const fetchLive = args.has("--fetch");
const openrouterPath = valueAfter("--openrouter-json");
const nvidiaPath = valueAfter("--nvidia-json");
const cerebrasPath = valueAfter("--cerebras-json");
const groqPath = valueAfter("--groq-json");

function usage(): never {
  console.log(`usage:
  bun tools/enrich-provider-registry.ts --fetch --apply
bun tools/enrich-provider-registry.ts --openrouter-json FILE --nvidia-json FILE --cerebras-json FILE --groq-json FILE --out FILE
  bun tools/enrich-provider-registry.ts --check

Options:
  --registry FILE       input registry, default config/provider-registry.json
  --out FILE            output registry, default same as input
  --fetch               fetch OpenRouter + NVIDIA public catalogs
  --openrouter-json F   use cached OpenRouter /api/v1/models response
  --nvidia-json F       use cached NVIDIA /v1/models response
  --cerebras-json F     use cached Cerebras public models response
  --groq-json F         use cached Groq /openai/v1/models response
  --apply               write output
  --check               validate only; no network required
`);
  process.exit(2);
}

if (args.has("--help") || args.has("-h")) usage();

const readJson = async (path: string): Promise<Json> => JSON.parse(await readFile(path, "utf8"));

async function fetchJson(url: string): Promise<Json> {
  const res = await fetch(url, {
    headers: {
      "accept": "application/json",
      "user-agent": "switchback-provider-registry/1.0",
    },
  });
  if (!res.ok) {
    throw new Error(`fetch ${url}: HTTP ${res.status}`);
  }
  return await res.json();
}

const toMicrosPerMtok = (perToken: string | undefined | null): number | null => {
  if (perToken == null) return null;
  const parsed = Number.parseFloat(perToken);
  if (!Number.isFinite(parsed)) return null;
  return Math.round(parsed * 1_000_000_000_000);
};

const unique = <T>(items: T[]): T[] => [...new Set(items)];

function source(url: string, kind: string, note?: string): Json {
  return { kind, source_url: url, fetched_at: FETCHED_AT, ...(note ? { note } : {}) };
}

function appendProvenance(existing: Json, item: Json) {
  const old = Array.isArray(existing.provenance) ? existing.provenance : [];
  const key = JSON.stringify([item.kind, item.source_url, item.note ?? ""]);
  const seen = new Set(old.map((x: Json) => JSON.stringify([x.kind, x.source_url, x.note ?? ""])));
  existing.provenance = seen.has(key) ? old : [...old, item];
}

function appendProviderSource(existing: Json, item: Json) {
  const old = Array.isArray(existing.provider_sources) ? existing.provider_sources : [];
  const key = JSON.stringify([item.kind, item.source_url, item.note ?? ""]);
  const seen = new Set(old.map((x: Json) => JSON.stringify([x.kind, x.source_url, x.note ?? ""])));
  existing.provider_sources = seen.has(key) ? old : [...old, item];
}

function mergeProviderResearch(provider: Json): Json {
  const research = INDEPENDENT_PROVIDER_RESEARCH[provider.id];
  if (!research) return provider;

  const providerResearch = research.provider_research || {};
  const out: Json = {
    ...provider,
    provider_research: {
      ...(provider.provider_research || {}),
      ...providerResearch,
    },
  };

  if (providerResearch.official_base_url && providerResearch.official_base_url !== provider.base_url) {
    out.previous_base_url = provider.base_url;
    out.base_url = providerResearch.official_base_url;
  }

  if (provider.id === "nebius") out.name = "Nebius Token Factory";

  for (const item of research.provider_sources || []) appendProviderSource(out, item);
  return out;
}

function independentProviderCatalogs(): Json {
  const catalogs: Json = {};
  for (const [providerId, research] of Object.entries(INDEPENDENT_PROVIDER_RESEARCH)) {
    const providerResearch = research.provider_research || {};
    catalogs[`${providerId}_provider`] = {
      source_url: providerResearch.models_url || providerResearch.docs_url,
      fetched_at: FETCHED_AT,
      provider_id: providerId,
      status: providerResearch.catalog_status,
      docs_url: providerResearch.docs_url,
      models_url: providerResearch.models_url,
      pricing_url: providerResearch.pricing_url,
      rate_limits_url: providerResearch.rate_limits_url,
      base_url: providerResearch.official_base_url,
      catalog_endpoint: providerResearch.catalog_endpoint,
      catalog_auth: providerResearch.catalog_auth,
      capabilities_declared: providerResearch.capabilities_declared,
      notes: providerResearch.routing_notes || [],
    };
  }
  return catalogs;
}

const NEURALWATT_PROVIDER: Json = {
  id: "neuralwatt",
  name: "NeuralWatt",
  base_url: "https://api.neuralwatt.com/v1",
  auth_scheme: "bearer",
  openai_compatible: "yes",
  free_tier: true,
  aggregator: false,
};

type NeuralWattModelSeed = {
  model_id: string;
  upstream_model_ids?: string[];
  display_name: string;
  provider_family: string;
  tier: string;
  context_window: number;
  input_usd_per_mtok: number;
  cached_input_usd_per_mtok: number;
  output_usd_per_mtok: number;
  features: ("R" | "V" | "T" | "J")[];
  observed_average_request_wh: number;
  typical_request_band: string;
  prompt_band_wh: Record<string, number | null>;
  prompt_band_request_share_pct: Record<string, number | null>;
  architecture?: Json;
};

const NEURALWATT_MODELS: NeuralWattModelSeed[] = [
  {
    model_id: "glm-5.2",
    display_name: "GLM-5.2",
    provider_family: "ZhipuAI",
    tier: "R/G",
    context_window: 1_048_576,
    input_usd_per_mtok: 1.45,
    cached_input_usd_per_mtok: 0.36,
    output_usd_per_mtok: 4.5,
    features: ["R", "T"],
    observed_average_request_wh: 2.34,
    typical_request_band: "64k_256k",
    prompt_band_wh: { "0_256": 0.04427, "256_1k": 0.11706, "1k_4k": 0.10124, "4k_16k": 0.34861, "16k_64k": 1.21, "64k_256k": 2.34, "256k_1m": 4.19 },
    prompt_band_request_share_pct: { "0_256": 0.5, "256_1k": 0.5, "1k_4k": 0.9, "4k_16k": 4.4, "16k_64k": 28.7, "64k_256k": 53.4, "256k_1m": 11.7 },
  },
  {
    model_id: "glm-5.2-fast",
    display_name: "GLM-5.2 (fast)",
    provider_family: "ZhipuAI",
    tier: "G/F",
    context_window: 1_048_576,
    input_usd_per_mtok: 1.45,
    cached_input_usd_per_mtok: 0.36,
    output_usd_per_mtok: 4.5,
    features: ["T"],
    observed_average_request_wh: 1.75,
    typical_request_band: "64k_256k",
    prompt_band_wh: { "0_256": 0.02866, "256_1k": 0.0569, "1k_4k": 0.15237, "4k_16k": 0.25404, "16k_64k": 0.69825, "64k_256k": 1.75, "256k_1m": 4.42 },
    prompt_band_request_share_pct: { "0_256": 0.9, "256_1k": 1.0, "1k_4k": 1.4, "4k_16k": 6.8, "16k_64k": 38.3, "64k_256k": 45.8, "256k_1m": 5.9 },
  },
  {
    model_id: "glm-5.2-short",
    display_name: "GLM-5.2 (short)",
    provider_family: "ZhipuAI",
    tier: "R/G",
    context_window: 200_000,
    input_usd_per_mtok: 1.45,
    cached_input_usd_per_mtok: 0.36,
    output_usd_per_mtok: 4.5,
    features: ["R", "T"],
    observed_average_request_wh: 1.34,
    typical_request_band: "64k_256k",
    prompt_band_wh: { "0_256": 0.01715, "256_1k": 0.0346, "1k_4k": 0.05747, "4k_16k": 0.2124, "16k_64k": 0.78201, "64k_256k": 1.34, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 1.1, "256_1k": 0.4, "1k_4k": 1.0, "4k_16k": 6.6, "16k_64k": 37.8, "64k_256k": 53.0, "256k_1m": null },
  },
  {
    model_id: "glm-5.2-short-fast",
    display_name: "GLM-5.2 (short, fast)",
    provider_family: "ZhipuAI",
    tier: "G/F",
    context_window: 200_000,
    input_usd_per_mtok: 1.45,
    cached_input_usd_per_mtok: 0.36,
    output_usd_per_mtok: 4.5,
    features: ["T"],
    observed_average_request_wh: 0.44447,
    typical_request_band: "16k_64k",
    prompt_band_wh: { "0_256": 0.01316, "256_1k": 0.01226, "1k_4k": 0.04446, "4k_16k": 0.1561, "16k_64k": 0.44447, "64k_256k": 0.92754, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 0.2, "256_1k": 0.3, "1k_4k": 2.1, "4k_16k": 18.4, "16k_64k": 29.3, "64k_256k": 49.7, "256k_1m": null },
  },
  {
    model_id: "kimi-k2.6",
    upstream_model_ids: ["moonshotai/Kimi-K2.6"],
    display_name: "Kimi K2.6",
    provider_family: "MoonshotAI",
    tier: "R/G",
    context_window: 262_144,
    input_usd_per_mtok: 0.69,
    cached_input_usd_per_mtok: 0.17,
    output_usd_per_mtok: 3.22,
    features: ["R", "V", "T", "J"],
    observed_average_request_wh: 0.78663,
    typical_request_band: "16k_64k",
    prompt_band_wh: { "0_256": 0.09665, "256_1k": 0.10901, "1k_4k": 0.21829, "4k_16k": 0.52512, "16k_64k": 0.78663, "64k_256k": 1.37, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 0.6, "256_1k": 0.3, "1k_4k": 2.4, "4k_16k": 8.4, "16k_64k": 45.1, "64k_256k": 43.1, "256k_1m": null },
  },
  {
    model_id: "kimi-k2.6-fast",
    display_name: "Kimi K2.6 Fast",
    provider_family: "MoonshotAI",
    tier: "G/F",
    context_window: 262_144,
    input_usd_per_mtok: 0.69,
    cached_input_usd_per_mtok: 0.17,
    output_usd_per_mtok: 3.22,
    features: ["V", "T", "J"],
    observed_average_request_wh: 0.56336,
    typical_request_band: "16k_64k",
    prompt_band_wh: { "0_256": 0.03423, "256_1k": 0.07475, "1k_4k": 0.38217, "4k_16k": 0.33676, "16k_64k": 0.56336, "64k_256k": 1.16, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 7.4, "256_1k": 0.7, "1k_4k": 1.6, "4k_16k": 13.8, "16k_64k": 36.2, "64k_256k": 40.3, "256k_1m": null },
  },
  {
    model_id: "kimi-k2.7-code",
    upstream_model_ids: ["moonshotai/Kimi-K2.7-Code"],
    display_name: "Kimi K2.7 Code",
    provider_family: "MoonshotAI",
    tier: "R/G",
    context_window: 262_144,
    input_usd_per_mtok: 0.95,
    cached_input_usd_per_mtok: 0.24,
    output_usd_per_mtok: 4.0,
    features: ["R", "V", "T", "J"],
    observed_average_request_wh: 1.63,
    typical_request_band: "64k_256k",
    prompt_band_wh: { "0_256": 0.17209, "256_1k": 0.13377, "1k_4k": 0.16696, "4k_16k": 0.35377, "16k_64k": 0.8629, "64k_256k": 1.63, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 0.4, "256_1k": 1.4, "1k_4k": 2.5, "4k_16k": 5.3, "16k_64k": 38.5, "64k_256k": 51.9, "256k_1m": null },
  },
  {
    model_id: "qwen3.5-397b",
    upstream_model_ids: ["Qwen/Qwen3.5-397B-A17B-FP8"],
    display_name: "Qwen3.5 397B",
    provider_family: "Qwen",
    tier: "R/G",
    context_window: 262_144,
    input_usd_per_mtok: 0.69,
    cached_input_usd_per_mtok: 0.17,
    output_usd_per_mtok: 4.14,
    features: ["R", "T", "J"],
    observed_average_request_wh: 0.30271,
    typical_request_band: "16k_64k",
    prompt_band_wh: { "0_256": 0.03566, "256_1k": 0.13499, "1k_4k": 0.14716, "4k_16k": 0.19665, "16k_64k": 0.30271, "64k_256k": 0.42708, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 6.9, "256_1k": 1.1, "1k_4k": 2.5, "4k_16k": 6.9, "16k_64k": 42.1, "64k_256k": 40.3, "256k_1m": null },
    architecture: { source: "neuralwatt_model_id_alias", mixture_of_experts: true, parameters_total_b: 397, parameters_active_b: 17, quantization: "FP8" },
  },
  {
    model_id: "qwen3.5-397b-fast",
    upstream_model_ids: ["Qwen/Qwen3.5-397B-A17B-FP8"],
    display_name: "Qwen3.5 397B Fast",
    provider_family: "Qwen",
    tier: "G/F",
    context_window: 262_144,
    input_usd_per_mtok: 0.69,
    cached_input_usd_per_mtok: 0.17,
    output_usd_per_mtok: 4.14,
    features: ["T", "J"],
    observed_average_request_wh: 0.2863,
    typical_request_band: "16k_64k",
    prompt_band_wh: { "0_256": 0.08875, "256_1k": 0.0331, "1k_4k": 0.03378, "4k_16k": 0.09184, "16k_64k": 0.2863, "64k_256k": 0.58226, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 0.4, "256_1k": 4.4, "1k_4k": 7.0, "4k_16k": 5.9, "16k_64k": 40.8, "64k_256k": 41.5, "256k_1m": null },
    architecture: { source: "neuralwatt_model_id_alias", mixture_of_experts: true, parameters_total_b: 397, parameters_active_b: 17, quantization: "FP8" },
  },
  {
    model_id: "qwen3.6-35b",
    upstream_model_ids: ["Qwen/Qwen3.6-35B-A3B"],
    display_name: "Qwen3.6 35B",
    provider_family: "Qwen",
    tier: "R/G",
    context_window: 131_072,
    input_usd_per_mtok: 0.29,
    cached_input_usd_per_mtok: 0.07,
    output_usd_per_mtok: 1.15,
    features: ["R", "V", "T", "J"],
    observed_average_request_wh: 0.15011,
    typical_request_band: "16k_64k",
    prompt_band_wh: { "0_256": 0.04383, "256_1k": 0.07502, "1k_4k": 0.06431, "4k_16k": 0.09026, "16k_64k": 0.15011, "64k_256k": 0.21291, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 1.7, "256_1k": 2.7, "1k_4k": 5.2, "4k_16k": 6.3, "16k_64k": 55.6, "64k_256k": 28.5, "256k_1m": null },
    architecture: { source: "neuralwatt_model_id_alias", mixture_of_experts: true, parameters_total_b: 35, parameters_active_b: 3 },
  },
  {
    model_id: "qwen3.6-35b-fast",
    upstream_model_ids: ["Qwen/Qwen3.6-35B-A3B"],
    display_name: "Qwen3.6 35B Fast",
    provider_family: "Qwen",
    tier: "G/F",
    context_window: 131_072,
    input_usd_per_mtok: 0.29,
    cached_input_usd_per_mtok: 0.07,
    output_usd_per_mtok: 1.15,
    features: ["V", "T", "J"],
    observed_average_request_wh: 0.04334,
    typical_request_band: "4k_16k",
    prompt_band_wh: { "0_256": 0.03923, "256_1k": 0.00498, "1k_4k": 0.01246, "4k_16k": 0.04334, "16k_64k": 0.09201, "64k_256k": 0.22943, "256k_1m": null },
    prompt_band_request_share_pct: { "0_256": 0.3, "256_1k": 24.7, "1k_4k": 10.0, "4k_16k": 19.2, "16k_64k": 38.3, "64k_256k": 7.5, "256k_1m": null },
    architecture: { source: "neuralwatt_model_id_alias", mixture_of_experts: true, parameters_total_b: 35, parameters_active_b: 3 },
  },
];

function usdPerMtokToMicros(usd: number | null | undefined): number | null {
  return usd == null ? null : Math.round(usd * 1_000_000);
}

function neuralwattRow(seed: NeuralWattModelSeed, existing: Json = {}): Json {
  const featureSet = new Set(seed.features);
  const imageInput = featureSet.has("V");
  const toolCalling = featureSet.has("T");
  const structuredOutputs = featureSet.has("J");
  const reasoning = featureSet.has("R");
  const inputModalities = imageInput ? ["text", "image"] : ["text"];
  const supportedParameters = [
    "stream",
    "max_tokens",
    "temperature",
    "top_p",
    ...(toolCalling ? ["tools", "tool_choice"] : []),
    ...(structuredOutputs ? ["response_format"] : []),
  ];
  const provenance = (existing.provenance || []).filter(
    (item: Json) =>
      !(item?.kind === "provider_catalog" &&
        item?.source_url === NEURALWATT_PRICING_URL &&
        String(item?.note || "").startsWith("neuralwatt hosted model row")),
  );
  const row: Json = {
    ...existing,
    provenance,
    provider_id: "neuralwatt",
    model_id: seed.model_id,
    display_name: seed.display_name,
    upstream_model_ids: seed.upstream_model_ids,
    tier: existing.tier || seed.tier,
    context_window: seed.context_window,
    vision: imageInput,
    tool_calling: toolCalling,
    json_schema: structuredOutputs ? "native" : "unknown",
    input_micros_per_mtok: usdPerMtokToMicros(seed.input_usd_per_mtok),
    cached_input_micros_per_mtok: usdPerMtokToMicros(seed.cached_input_usd_per_mtok),
    output_micros_per_mtok: usdPerMtokToMicros(seed.output_usd_per_mtok),
    source_url: NEURALWATT_PRICING_URL,
    flags: unique([
      ...(existing.flags || []),
      "NeuralWatt token prices plus rolling energy averages; refresh before energy-sensitive routing",
    ]),
    capabilities: {
      ...(existing.capabilities || {}),
      declared_by: "neuralwatt_pricing_page",
      input_modalities: inputModalities,
      output_modalities: ["text"],
      supported_parameters: supportedParameters,
      text_input: true,
      text_output: true,
      image_input: imageInput,
      video_input: false,
      audio_input: false,
      embeddings_output: false,
      rerank_output: false,
      streaming: true,
      tool_calling: toolCalling,
      function_calling: toolCalling,
      structured_outputs: structuredOutputs,
      json_schema: structuredOutputs ? "native" : "unknown",
      reasoning,
      prompt_caching: true,
      energy_metered: true,
      provider_hosted: true,
      openai_compatible: true,
    },
    determinism: {
      ...(existing.determinism || {}),
      seed_supported: false,
      temperature_supported: true,
      top_p_supported: true,
      note: "NeuralWatt docs expose OpenAI-compatible sampling controls; deterministic repeatability needs Switchback probe receipt.",
    },
    limits: {
      ...(existing.limits || {}),
      context_window: seed.context_window,
      provider_context_window: seed.context_window,
    },
    architecture: {
      ...(existing.architecture || {}),
      source: "neuralwatt_pricing_page",
      architecture_type: `${seed.provider_family} hosted model`,
      provider_family: seed.provider_family,
      ...(seed.architecture || {}),
    },
    energy: {
      ...(existing.energy || {}),
      pricing_basis: {
        billing_metric: "kWh",
        energy_usd_per_kwh: 5,
        token_pricing_available: true,
        source: "neuralwatt_pricing_page",
        note: "Energy billing is flat $5/kWh; token prices are an alternate billing basis.",
      },
      observed_average_request_wh: seed.observed_average_request_wh,
      typical_request_band: seed.typical_request_band,
      observed_prompt_band_wh: seed.prompt_band_wh,
      observed_prompt_band_request_share_pct: seed.prompt_band_request_share_pct,
      observation_window_days: 7,
      source: "neuralwatt_energy_status_page",
      source_url: NEURALWATT_ENERGY_URL,
      fetched_at: FETCHED_AT,
      note: "Rolling observed live-traffic averages, not fixed model constants. Energy depends on request size, generation length, and cache hit rate.",
    },
    verification: {
      declared: true,
      probed: false,
      probes: existing.verification?.probes || {},
      pricing_seen: {
        source: "neuralwatt_pricing_page",
        pricing_seen_at: FETCHED_AT,
      },
    },
  };
  appendProvenance(row, source(NEURALWATT_DOCS_URL, "provider_docs", "OpenAI-compatible base URL and chat completions examples."));
  appendProvenance(row, source(NEURALWATT_PRICING_URL, "model_pricing_docs", "NeuralWatt token prices, context windows and feature badges."));
  appendProvenance(row, source(NEURALWATT_ENERGY_URL, "energy_status", "Trailing observed energy averages by prompt-size band."));
  return row;
}

function upsertProvider(providers: Json[], provider: Json): Json[] {
  const index = providers.findIndex((item) => item.id === provider.id);
  if (index < 0) return [...providers, provider];
  return providers.map((item) => (item.id === provider.id ? { ...provider, ...item } : item));
}

function supported(model: Json, name: string): boolean {
  return Array.isArray(model.supported_parameters) && model.supported_parameters.includes(name);
}

function jsonSchemaSupport(model: Json, existing?: string): string {
  if (supported(model, "structured_outputs")) return "native";
  if (supported(model, "response_format")) return "response_format";
  return existing || "none";
}

function parseArchitecture(description: string | undefined): Json {
  const text = description || "";
  const total =
    text.match(/(\d+(?:\.\d+)?)B[- ]parameter/i)?.[1] ||
    text.match(/(\d+(?:\.\d+)?)B total parameters/i)?.[1] ||
    text.match(/(\d+(?:\.\d+)?)B parameters/i)?.[1];
  const active =
    text.match(/(\d+(?:\.\d+)?)B active/i)?.[1] ||
    text.match(/activat(?:es|ing) (?:just )?(\d+(?:\.\d+)?)B/i)?.[1];
  const moe = /\bMoE\b|mixture-of-experts|mixture of experts|sparse mixture/i.test(text);
  return {
    ...(moe ? { mixture_of_experts: true } : {}),
    ...(total ? { parameters_total_b: Number(total) } : {}),
    ...(active ? { parameters_active_b: Number(active) } : {}),
  };
}

function openrouterRow(model: Json, existing: Json = {}): Json {
  const inputModalities = model.architecture?.input_modalities || [];
  const outputModalities = model.architecture?.output_modalities || [];
  const params = model.supported_parameters || [];
  const prompt = toMicrosPerMtok(model.pricing?.prompt);
  const completion = toMicrosPerMtok(model.pricing?.completion);
  const cacheRead = toMicrosPerMtok(model.pricing?.input_cache_read);
  const sourceUrl = `https://openrouter.ai/${model.id}`;
  const topProvider = model.top_provider || {};

  const row: Json = {
    ...existing,
    provider_id: "openrouter",
    model_id: model.id,
    display_name: model.name,
    tier: existing.tier || "F",
    context_window: model.context_length ?? existing.context_window ?? null,
    vision: inputModalities.includes("image"),
    tool_calling: params.includes("tools"),
    json_schema: jsonSchemaSupport(model, existing.json_schema),
    input_micros_per_mtok: prompt ?? existing.input_micros_per_mtok ?? null,
    output_micros_per_mtok: completion ?? existing.output_micros_per_mtok ?? null,
    cached_input_micros_per_mtok: cacheRead ?? existing.cached_input_micros_per_mtok ?? null,
    source_url: sourceUrl,
    flags: unique([
      ...(existing.flags || []),
      "OpenRouter :free; non-SLA tripwire/execution lane only",
    ]),
    capabilities: {
      declared_by: "openrouter_models_api",
      input_modalities: inputModalities,
      output_modalities: outputModalities,
      supported_parameters: params,
      text_input: inputModalities.includes("text"),
      text_output: outputModalities.includes("text"),
      image_input: inputModalities.includes("image"),
      video_input: inputModalities.includes("video"),
      audio_input: inputModalities.includes("audio"),
      embeddings_output: outputModalities.includes("embeddings"),
      rerank_output: outputModalities.includes("rerank"),
      tool_calling: params.includes("tools"),
      tool_choice: params.includes("tool_choice"),
      structured_outputs: params.includes("structured_outputs"),
      response_format: params.includes("response_format"),
      json_schema: jsonSchemaSupport(model, existing.json_schema),
      seed: params.includes("seed"),
      reasoning: params.includes("reasoning"),
      include_reasoning: params.includes("include_reasoning"),
      temperature: params.includes("temperature"),
      top_p: params.includes("top_p"),
      stop: params.includes("stop"),
      max_tokens: params.includes("max_tokens"),
    },
    determinism: {
      seed_supported: params.includes("seed"),
      temperature_supported: params.includes("temperature"),
      top_p_supported: params.includes("top_p"),
      note: params.includes("seed")
        ? "Provider declares seed parameter; deterministic behavior still needs Switchback probe receipt."
        : "No declared seed parameter in OpenRouter catalog.",
    },
    limits: {
      context_window: model.context_length ?? null,
      provider_context_window: topProvider.context_length ?? null,
      max_completion_tokens: topProvider.max_completion_tokens ?? null,
      per_request_limits: model.per_request_limits ?? null,
      is_moderated: topProvider.is_moderated ?? null,
    },
    architecture: {
      source: "openrouter_models_api",
      tokenizer: model.architecture?.tokenizer ?? null,
      instruct_type: model.architecture?.instruct_type ?? null,
      modality: model.architecture?.modality ?? null,
      ...parseArchitecture(model.description),
    },
    verification: {
      declared: true,
      probed: false,
      probes: existing.verification?.probes || {},
    },
  };

  if (model.benchmarks) {
    row.benchmarks = {
      ...(existing.benchmarks || {}),
      openrouter: {
        source: "openrouter_models_api",
        fetched_at: FETCHED_AT,
        values: model.benchmarks,
      },
    };
  }

  appendProvenance(row, source(OPENROUTER_MODELS_URL, "api", "OpenRouter model metadata, capabilities, pricing, and per-model benchmarks."));
  return row;
}

function boolKeys(value: Json | undefined | null): string[] {
  if (!value || typeof value !== "object") return [];
  return Object.entries(value)
    .filter(([, enabled]) => enabled === true)
    .map(([key]) => key);
}

function cerebrasRow(model: Json, existing: Json = {}): Json {
  const capabilities = model.capabilities || {};
  const supportedParameters = boolKeys(model.supported_parameters);
  const inputModalities = capabilities.vision ? ["text", "image"] : ["text"];
  const outputModalities = ["text"];
  const contextWindow = model.limits?.max_context_length ?? existing.context_window ?? null;
  const maxCompletionTokens = model.limits?.max_completion_tokens ?? existing.limits?.max_completion_tokens ?? null;
  const prompt = toMicrosPerMtok(model.pricing?.prompt);
  const completion = toMicrosPerMtok(model.pricing?.completion);
  const sourceUrl = `${CEREBRAS_PUBLIC_MODELS_URL}/${model.id}`;
  const flags = unique([
    ...(existing.flags || []),
    "Cerebras public catalog; verify account tier/rate limits before production routing",
    ...(model.preview ? ["preview"] : []),
    ...(model.deprecated ? ["deprecated"] : []),
  ]);

  const row: Json = {
    ...existing,
    provider_id: "cerebras",
    model_id: model.id,
    display_name: model.name ?? existing.display_name ?? null,
    tier: existing.tier || (capabilities.reasoning ? "R" : "G"),
    context_window: contextWindow,
    vision: Boolean(capabilities.vision),
    tool_calling: Boolean(capabilities.function_calling || capabilities.tools),
    json_schema: capabilities.structured_outputs || capabilities.json_mode || capabilities.response_format ? "native" : "none",
    input_micros_per_mtok: prompt ?? existing.input_micros_per_mtok ?? null,
    output_micros_per_mtok: completion ?? existing.output_micros_per_mtok ?? null,
    cached_input_micros_per_mtok: existing.cached_input_micros_per_mtok ?? null,
    source_url: sourceUrl,
    flags,
    capabilities: {
      ...(existing.capabilities || {}),
      declared_by: "cerebras_public_models_api",
      input_modalities: inputModalities,
      output_modalities: outputModalities,
      supported_parameters: supportedParameters,
      text_input: true,
      text_output: true,
      image_input: Boolean(capabilities.vision),
      video_input: false,
      audio_input: false,
      embeddings_output: false,
      rerank_output: false,
      streaming: Boolean(capabilities.streaming),
      tool_calling: Boolean(capabilities.function_calling || capabilities.tools),
      function_calling: Boolean(capabilities.function_calling),
      structured_outputs: Boolean(capabilities.structured_outputs),
      json_mode: Boolean(capabilities.json_mode),
      tool_choice: Boolean(capabilities.tool_choice),
      parallel_tool_calls: Boolean(capabilities.parallel_tool_calls),
      response_format: Boolean(capabilities.response_format),
      reasoning: Boolean(capabilities.reasoning),
      seed: supportedParameters.includes("seed"),
      json_schema: capabilities.structured_outputs || capabilities.json_mode || capabilities.response_format ? "native" : "none",
    },
    determinism: {
      ...(existing.determinism || {}),
      seed_supported: supportedParameters.includes("seed"),
      temperature_supported: supportedParameters.includes("temperature"),
      top_p_supported: supportedParameters.includes("top_p"),
      note: supportedParameters.includes("seed")
        ? "Cerebras catalog declares seed parameter; deterministic repeatability still needs Switchback probe receipt."
        : "Cerebras catalog does not declare seed support for this model.",
    },
    limits: {
      ...(existing.limits || {}),
      context_window: contextWindow,
      provider_context_window: contextWindow,
      max_completion_tokens: maxCompletionTokens,
      requests_per_minute: model.limits?.requests_per_minute ?? null,
      tokens_per_minute: model.limits?.tokens_per_minute ?? null,
    },
    architecture: {
      ...(existing.architecture || {}),
      source: "cerebras_public_models_api",
      tokenizer: model.architecture?.tokenizer ?? null,
      instruct_type: model.architecture?.instruct_type ?? null,
      modality: model.architecture?.modality ?? null,
      quantization: model.quantization ?? null,
      hugging_face_id: model.hugging_face_id ?? null,
      owned_by: model.owned_by ?? null,
    },
    verification: {
      declared: true,
      probed: false,
      probes: existing.verification?.probes || {},
      catalog_seen: {
        source: "cerebras_public_models_api",
        catalog_seen_at: FETCHED_AT,
        deprecated: Boolean(model.deprecated),
        preview: Boolean(model.preview),
      },
    },
  };

  appendProvenance(
    row,
    source(CEREBRAS_PUBLIC_MODELS_URL, "provider_catalog", "Cerebras public model catalog with pricing, limits, capabilities, and architecture."),
  );
  return row;
}

function firstInteger(...values: unknown[]): number | null {
  for (const value of values) {
    const number = typeof value === "number" ? value : typeof value === "string" ? Number.parseInt(value, 10) : Number.NaN;
    if (Number.isInteger(number) && number >= 0) return number;
  }
  return null;
}

function groqRow(model: Json, existing: Json = {}): Json {
  const capabilities = model.capabilities || {};
  const id = String(model.id || "");
  const lowerId = id.toLowerCase();
  const supportedParameters = Array.isArray(model.supported_parameters)
    ? model.supported_parameters
    : boolKeys(model.supported_parameters);
  const inputModalities = Array.isArray(capabilities.input_modalities)
    ? capabilities.input_modalities
    : Array.isArray(model.input_modalities)
      ? model.input_modalities
      : lowerId.includes("whisper")
        ? ["audio"]
        : capabilities.vision === true
          ? ["text", "image"]
          : ["text"];
  const outputModalities = Array.isArray(capabilities.output_modalities)
    ? capabilities.output_modalities
    : Array.isArray(model.output_modalities)
      ? model.output_modalities
      : lowerId.includes("tts")
        ? ["audio"]
        : ["text"];
  const contextWindow = firstInteger(
    model.context_window,
    model.context_length,
    model.max_context_length,
    model.limits?.context_window,
    model.limits?.max_context_length,
    existing.context_window,
  );
  const maxCompletionTokens = firstInteger(
    model.max_completion_tokens,
    model.max_output_tokens,
    model.limits?.max_completion_tokens,
    existing.limits?.max_completion_tokens,
  );
  const promptMicros = firstInteger(
    model.input_micros_per_mtok,
    model.pricing?.input_micros_per_mtok,
    existing.input_micros_per_mtok,
  );
  const completionMicros = firstInteger(
    model.output_micros_per_mtok,
    model.pricing?.output_micros_per_mtok,
    existing.output_micros_per_mtok,
  );
  const cacheMicros = firstInteger(
    model.cached_input_micros_per_mtok,
    model.pricing?.cached_input_micros_per_mtok,
    existing.cached_input_micros_per_mtok,
  );
  const imageInput = inputModalities.includes("image") || capabilities.vision === true;
  const audioInput = inputModalities.includes("audio");
  const toolCalling = Boolean(
    capabilities.tool_calling === true ||
      capabilities.function_calling === true ||
      supportedParameters.includes("tools") ||
      existing.tool_calling,
  );
  const structuredOutputs = Boolean(
    capabilities.structured_outputs === true ||
      capabilities.json_schema === true ||
      supportedParameters.includes("response_format"),
  );

  const row: Json = {
    ...existing,
    provider_id: "groq",
    model_id: id,
    display_name: model.name ?? existing.display_name ?? id,
    tier: existing.tier || (lowerId.includes("whisper") || lowerId.includes("tts") ? "S" : "G"),
    context_window: contextWindow,
    vision: imageInput,
    tool_calling: toolCalling,
    json_schema: structuredOutputs ? "native" : existing.json_schema ?? "unknown",
    input_micros_per_mtok: promptMicros,
    output_micros_per_mtok: completionMicros,
    cached_input_micros_per_mtok: cacheMicros,
    source_url: GROQ_MODELS_URL,
    flags: unique([
      ...(existing.flags || []),
      "Groq catalog row; probe capabilities, latency, and rate limits before promotion",
    ]),
    capabilities: {
      ...(existing.capabilities || {}),
      declared_by: "groq_models_api",
      catalog_sparse: true,
      input_modalities: inputModalities,
      output_modalities: outputModalities,
      supported_parameters: supportedParameters,
      text_input: inputModalities.includes("text"),
      text_output: outputModalities.includes("text"),
      image_input: imageInput,
      video_input: inputModalities.includes("video"),
      audio_input: audioInput,
      audio_output: outputModalities.includes("audio"),
      embeddings_output: outputModalities.includes("embeddings"),
      rerank_output: outputModalities.includes("rerank"),
      tool_calling: toolCalling,
      function_calling: Boolean(capabilities.function_calling === true),
      structured_outputs: structuredOutputs,
      json_schema: structuredOutputs ? "native" : existing.capabilities?.json_schema ?? "unknown",
    },
    determinism: {
      ...(existing.determinism || {}),
      seed_supported: Boolean(capabilities.seed === true || supportedParameters.includes("seed")),
      temperature_supported: true,
      top_p_supported: true,
      note:
        "Groq model catalog does not prove deterministic repeatability; provider docs note temperature=0 is converted to a tiny nonzero value.",
    },
    limits: {
      ...(existing.limits || {}),
      context_window: contextWindow,
      provider_context_window: contextWindow,
      max_completion_tokens: maxCompletionTokens,
    },
    architecture: {
      ...(existing.architecture || {}),
      source: "groq_models_api",
      object: model.object ?? null,
      owned_by: model.owned_by ?? null,
      created: model.created ?? null,
      active: model.active ?? null,
    },
    verification: {
      declared: true,
      probed: false,
      probes: existing.verification?.probes || {},
      catalog_seen: {
        source: "groq_models_api",
        catalog_seen_at: FETCHED_AT,
        active: model.active ?? null,
      },
    },
  };

  appendProvenance(row, source(GROQ_MODELS_URL, "provider_catalog", "Groq authenticated OpenAI-compatible model catalog."));
  return row;
}

const NVIDIA_OVERRIDES: Record<string, Json> = {
  "minimaxai/minimax-m2.7": {
    tool_calling: true,
    json_schema: "unknown",
    capabilities: {
      declared_by: "nvidia_model_card",
      input_modalities: ["text"],
      output_modalities: ["text"],
      text_input: true,
      text_output: true,
      image_input: false,
      tool_calling: true,
      reasoning: true,
      note: "NVIDIA Build model card positions MiniMax M2.7 for coding, reasoning, and office tasks; API sample uses max_tokens=8192.",
    },
    limits: { max_completion_tokens: 8_192 },
    architecture: {
      source: "nvidia_model_card",
      architecture_type: "large text-to-text model",
      parameters_total_b: 230,
    },
    provenance: [
      source("https://build.nvidia.com/minimaxai/minimax-m2.7", "model_card", "MiniMax M2.7 NVIDIA Build model card, free endpoint, parameter count, and task positioning."),
      source("https://docs.api.nvidia.com/nim/reference/minimaxai-minimax-m2.7", "api_reference", "NVIDIA NIM API reference lists minimaxai/minimax-m2.7 chat completion endpoint."),
    ],
  },
  "minimaxai/minimax-m3": {
    context_window: 1_000_000,
    vision: true,
    tool_calling: true,
    json_schema: "unknown",
    capabilities: {
      declared_by: "vendor_blog",
      input_modalities: ["text", "image"],
      output_modalities: ["text"],
      text_input: true,
      text_output: true,
      image_input: true,
      tool_calling: true,
      reasoning: true,
      note: "NVIDIA serving examples expose MiniMax M3 tool-call and reasoning parsers; model blog states native multimodality.",
    },
    architecture: {
      source: "minimax_model_blog",
      attention: "MiniMax Sparse Attention (MSA)",
      context_method: "1M context sparse attention",
    },
    benchmarks: {
      vendor: {
        source: "minimax_model_blog",
        fetched_at: FETCHED_AT,
        values: {
          "SWE-Bench Pro": 59.0,
          "Terminal-Bench 2.1": 66.0,
          "SWE-fficiency": 34.8,
          "KernelBench Hard": 28.8,
          "MCP Atlas": 74.2,
        },
      },
    },
    provenance: [
      source("https://www.minimax.io/blog/minimax-m3", "model_card", "MiniMax M3 context, multimodality, and coding benchmark claims."),
      source("https://developer.nvidia.com/blog/deploy-long-context-reasoning-and-agentic-workflows-with-minimax-m3-on-nvidia-accelerated-infrastructure/", "vendor_blog", "NVIDIA Build availability and serving parser examples."),
    ],
  },
  "nvidia/nemotron-3-ultra-550b-a55b": {
    context_window: 1_000_000,
    vision: false,
    tool_calling: true,
    json_schema: "native",
    capabilities: {
      declared_by: "nvidia_model_card",
      input_modalities: ["text"],
      output_modalities: ["text"],
      text_input: true,
      text_output: true,
      image_input: false,
      tool_calling: true,
      reasoning: true,
      reasoning_controls: ["enable_thinking=True/False"],
    },
    architecture: {
      source: "nvidia_model_card",
      architecture_type: "Mamba2-Transformer Hybrid LatentMoE with MTP",
      mixture_of_experts: true,
      parameters_total_b: 550,
      parameters_active_b: 55,
      quantization: "NVFP4",
    },
    benchmarks: {
      vendor: {
        source: "nvidia_model_card",
        fetched_at: FETCHED_AT,
        values: {
          "Terminal Bench 2.1": 53.9,
          "SWE-Bench Verified": 69.7,
          "TauBench V3 Average": 70.3,
          "GPQA (no tools)": 87.9,
          "RULER 1M": 94.0,
        },
      },
    },
    provenance: [
      source("https://build.nvidia.com/nvidia/nemotron-3-ultra-550b-a55b/modelcard", "model_card", "NVIDIA model facts and benchmark table."),
      source("https://vllm.ai/blog/2026-06-04-nemotron-3-ultra-vllm", "deployment_note", "Architecture, modality, and tool-calling positioning."),
    ],
  },
  "nvidia/nemotron-3-super-120b-a12b": {
    context_window: 1_000_000,
    vision: false,
    tool_calling: true,
    json_schema: "native",
    capabilities: {
      declared_by: "nvidia_model_card",
      input_modalities: ["text"],
      output_modalities: ["text"],
      text_input: true,
      text_output: true,
      image_input: false,
      tool_calling: true,
      reasoning: true,
    },
    architecture: {
      source: "nvidia_model_card",
      architecture_type: "Hybrid Mamba-Transformer MoE",
      mixture_of_experts: true,
      parameters_total_b: 120,
      parameters_active_b: 12,
    },
    provenance: [
      source("https://build.nvidia.com/nvidia/nemotron-3-super-120b-a12b/modelcard", "model_card", "NVIDIA model facts."),
    ],
  },
};

function mergeNvidiaOverrides(row: Json, nvidiaIds: Set<string>): Json {
  const override = NVIDIA_OVERRIDES[row.model_id] || {};
  const contextWindow = override.context_window ?? row.context_window ?? null;
  const toolCalling = Boolean(override.tool_calling ?? row.tool_calling);
  const jsonSchema = override.json_schema ?? row.json_schema ?? "unknown";
  const supportedParameters = [
    ...(NVIDIA_OPENAI_COMPATIBLE_DEFAULTS.capabilities.supported_parameters || []),
    ...(toolCalling ? ["tools"] : []),
    ...(jsonSchema !== "unknown" && jsonSchema !== "none" ? ["response_format"] : []),
  ];
  const defaultCaps = {
    ...NVIDIA_OPENAI_COMPATIBLE_DEFAULTS.capabilities,
    supported_parameters: [...new Set(supportedParameters)],
    image_input: Boolean(override.vision ?? row.vision),
    tool_calling: toolCalling,
    json_schema: jsonSchema,
  };
  const out = {
    ...row,
    ...override,
    capabilities: { ...(row.capabilities || {}), ...defaultCaps, ...(override.capabilities || {}) },
    determinism: {
      ...(row.determinism || {}),
      ...NVIDIA_OPENAI_COMPATIBLE_DEFAULTS.determinism,
      ...(override.determinism || {}),
    },
    limits: {
      ...(row.limits || {}),
      ...NVIDIA_OPENAI_COMPATIBLE_DEFAULTS.limits,
      context_window: contextWindow,
      provider_context_window: contextWindow,
      ...(override.limits || {}),
    },
    architecture: { ...(row.architecture || {}), ...(override.architecture || {}) },
    benchmarks: { ...(row.benchmarks || {}), ...(override.benchmarks || {}) },
    verification: {
      declared: true,
      probed: false,
      probes: row.verification?.probes || {},
      catalog_seen: nvidiaIds.has(row.model_id),
      catalog_seen_at: nvidiaIds.has(row.model_id) ? FETCHED_AT : row.verification?.catalog_seen_at,
    },
  };

  appendProvenance(out, source(NVIDIA_MODELS_URL, "api", "NVIDIA public model catalog membership."));
  for (const item of override.provenance || []) appendProvenance(out, item);
  return out;
}

function directCaps(kind: string, overrides: Json = {}): Json {
  const base: Json = {
    declared_by: kind,
    input_modalities: ["text"],
    output_modalities: ["text"],
    text_input: true,
    text_output: true,
    image_input: false,
    video_input: false,
    audio_input: false,
    embeddings_output: false,
    rerank_output: false,
    tool_calling: true,
    json_schema: "native",
    streaming: true,
    max_tokens: true,
  };
  return { ...base, ...overrides };
}

function directLimits(contextWindow: number, maxOutputTokens?: number, extra: Json = {}): Json {
  return {
    context_window: contextWindow,
    provider_context_window: contextWindow,
    ...(maxOutputTokens ? { max_completion_tokens: maxOutputTokens } : {}),
    ...extra,
  };
}

function directDeterminism(seedSupported = false, note?: string): Json {
  return {
    seed_supported: seedSupported,
    temperature_supported: true,
    top_p_supported: true,
    note: note || "Provider supports sampling controls; deterministic repeatability needs Switchback probe receipt.",
  };
}

function familyResearch(row: Json): Json | null {
  const key = `${row.provider_id}/${row.model_id}`;
  const context = row.context_window || row.limits?.context_window || row.limits?.provider_context_window;
  const isReasoning = /\bR\b/.test(String(row.tier || "")) || /(^o\d|gpt-5|grok|glm-5|deepseek-v4|kimi-k2|qwen3\.7)/i.test(row.model_id || "");
  const textImage = row.vision ? ["text", "image"] : ["text"];
  const sourceUrl = row.source_url;
  const common = (declaredBy: string, sourceKind: string, note: string, extra: Json = {}) => {
    const provenanceUrl = extra.source_url || sourceUrl;
    return {
    source_url: provenanceUrl || sourceUrl,
    capabilities: directCaps(declaredBy, {
      input_modalities: textImage,
      image_input: Boolean(row.vision),
      tool_calling: Boolean(row.tool_calling),
      json_schema: row.json_schema || "unknown",
      reasoning: isReasoning,
      ...extra.capabilities,
    }),
    determinism: directDeterminism(false, extra.determinism_note),
    limits: context ? directLimits(context, extra.max_output_tokens, extra.limits || {}) : extra.limits || {},
    architecture: {
      source: declaredBy,
      architecture_type: extra.architecture_type || "provider-hosted model",
      ...(extra.architecture || {}),
    },
    provenance: provenanceUrl ? [source(provenanceUrl, sourceKind, note)] : [],
  };
  };

  if (row.provider_id === "openai") {
    return common("openai_models_api_docs", "model_docs", "OpenAI model, pricing, tool and structured-output documentation.", {
      source_url: "https://developers.openai.com/api/docs/models",
      max_output_tokens: /gpt-5|5\.4|5\.5/.test(row.model_id) ? 128_000 : undefined,
      architecture_type: /gpt-oss/.test(row.model_id) ? "open-weight OpenAI model" : "proprietary OpenAI reasoning/frontier model",
      capabilities: {
        input_modalities: ["text", "image"],
        image_input: true,
        structured_outputs: true,
        supported_tools: ["functions", "web_search", "file_search", "computer_use"],
        reasoning_controls: isReasoning ? ["none", "low", "medium", "high", "xhigh"] : undefined,
      },
    });
  }

  if (row.provider_id === "azure-openai") {
    return common("azure_openai_openai_model_docs", "provider_docs", "Azure OpenAI serving of OpenAI model family; deployment and regional availability are Azure-specific.", {
      source_url: "https://learn.microsoft.com/azure/ai-foundry/openai/concepts/models",
      max_output_tokens: 128_000,
      architecture_type: "Azure-hosted OpenAI model",
      capabilities: {
        input_modalities: ["text", "image"],
        image_input: true,
        structured_outputs: true,
        supported_tools: ["functions", "web_search", "file_search", "computer_use"],
      },
    });
  }

  if (row.provider_id === "anthropic") {
    return common("anthropic_models_api_docs", "model_docs", "Anthropic model, pricing, tool-use, vision and context documentation.", {
      source_url: "https://platform.claude.com/docs/en/about-claude/models/overview",
      max_output_tokens: /haiku/.test(row.model_id) ? 64_000 : 128_000,
      architecture_type: "proprietary Claude model",
      capabilities: {
        input_modalities: ["text", "image"],
        image_input: true,
        json_schema: "tool-based",
        extended_thinking: !/haiku/.test(row.model_id),
      },
    });
  }

  if (row.provider_id === "bedrock") {
    if (row.model_id.startsWith("anthropic.")) {
      return common("bedrock_anthropic_docs", "provider_docs", "Amazon Bedrock-hosted Anthropic model; endpoint and region behavior are Bedrock-specific.", {
        source_url: "https://docs.aws.amazon.com/bedrock/latest/userguide/models-supported.html",
        max_output_tokens: /haiku/.test(row.model_id) ? 64_000 : 128_000,
        architecture_type: "Bedrock-hosted Claude model",
        capabilities: {
          input_modalities: ["text", "image"],
          image_input: true,
          json_schema: "tool-based",
          extended_thinking: !/haiku/.test(row.model_id),
        },
      });
    }
    if (row.model_id.startsWith("meta.")) {
      return common("bedrock_meta_docs", "provider_docs", "Amazon Bedrock-hosted Meta Llama model.", {
        source_url: "https://docs.aws.amazon.com/bedrock/latest/userguide/models-supported.html",
        architecture_type: "Bedrock-hosted open-weight Llama model",
        capabilities: { input_modalities: ["text"], image_input: false, json_schema: row.json_schema || "native" },
      });
    }
  }

  if (row.provider_id === "gemini" || row.provider_id === "vertex") {
    return common(row.provider_id === "vertex" ? "vertex_gemini_docs" : "gemini_api_docs", "model_pricing_docs", "Gemini multimodal, tool and structured-output model documentation.", {
      source_url: "https://ai.google.dev/gemini-api/docs/pricing",
      max_output_tokens: /flash-lite/.test(row.model_id) ? 65_535 : undefined,
      architecture_type: "Google Gemini multimodal model",
      capabilities: {
        input_modalities: ["text", "image", "audio", "video"],
        image_input: true,
        audio_input: true,
        video_input: true,
        grounding_tools: ["google_search", "google_maps"],
        thinking_budget: /2\.5|3/.test(row.model_id),
      },
    });
  }

  if (row.provider_id === "xai") {
    return common("xai_model_docs", "model_docs", "xAI model pricing, context, function calling, structured outputs and reasoning documentation.", {
      source_url: "https://docs.x.ai/developers/models",
      architecture_type: "proprietary Grok model",
      capabilities: {
        input_modalities: textImage,
        structured_outputs: true,
        function_calling: true,
        reasoning_controls: isReasoning ? ["none", "low", "medium", "high"] : undefined,
      },
    });
  }

  if (row.provider_id === "deepseek") {
    return common("deepseek_api_docs", "release_notes", "DeepSeek V4 release and pricing documentation.", {
      source_url: "https://api-docs.deepseek.com/news/news260424",
      architecture_type: "DeepSeek MoE model",
      capabilities: { cache_discount: true, reasoning: true },
    });
  }

  if (row.provider_id === "nvidia") {
    return common("nvidia_build_catalog", "provider_catalog", "NVIDIA Build hosted model row; exact architecture comes from per-model card override when available.", {
      source_url: NVIDIA_MODELS_URL,
      architecture_type: "NVIDIA Build hosted model",
      capabilities: {
        input_modalities: textImage,
        provider_hosted: true,
        openai_compatible: true,
      },
      determinism_note: "NVIDIA Build exposes OpenAI-compatible sampling controls; no catalog-level seed determinism guarantee captured yet.",
    });
  }

  if (row.provider_id === "zai") {
    return common("zai_pricing_docs", "model_pricing_docs", "Z.ai GLM pricing and language-model documentation.", {
      source_url: "https://docs.z.ai/guides/overview/pricing",
      max_output_tokens: /glm-5/.test(row.model_id) ? 128_000 : undefined,
      architecture_type: "Z.ai GLM agentic language model",
      capabilities: { function_calling: true, thinking_modes: /glm-5/.test(row.model_id) },
    });
  }

  if (row.provider_id === "moonshot") {
    return common("kimi_api_docs", "model_docs", "Kimi long-context, multimodal and OpenAI-compatible API documentation.", {
      source_url: "https://platform.kimi.ai/docs/guide/kimi-k2-6-quickstart",
      architecture_type: "Moonshot Kimi long-horizon coding model",
      capabilities: {
        input_modalities: ["text", "image", "video"],
        image_input: true,
        video_input: true,
        thinking_modes: ["enabled", "disabled"],
        multi_step_tool_invocation: true,
      },
    });
  }

  if (row.provider_id === "mistral") {
    return common("mistral_pricing_docs", "model_pricing_docs", "Mistral model pricing, coding and agentic model documentation.", {
      source_url: "https://mistral.ai/pricing/",
      architecture_type: /ministral|open-mistral/.test(row.model_id) ? "Mistral open-weight model" : "Mistral premier model",
      capabilities: { coding: /code|codestral|devstral/.test(row.model_id), agentic: true },
    });
  }

  if (row.provider_id === "cohere") {
    return common("cohere_model_docs", "model_docs", "Cohere Command model RAG, citation and tool-use documentation.", {
      source_url: "https://docs.cohere.com/docs/models",
      max_output_tokens: row.model_id === "command-r7b" ? 4_000 : 8_000,
      architecture_type: row.model_id === "command-r7b" ? "Cohere open-weight 7B command model" : "Cohere enterprise command model",
      capabilities: { rag: true, citations: true, multilingual: true, structured_outputs: true },
    });
  }

  if (row.provider_id === "alibaba") {
    return common("alibaba_model_studio_docs", "model_pricing_docs", "Alibaba Model Studio Qwen pricing and Qwen agent-model documentation.", {
      source_url: "https://www.alibabacloud.com/help/en/model-studio/model-pricing",
      architecture_type: "Alibaba Qwen agent model",
      capabilities: { prompt_caching: true, tool_calling: true, reasoning: isReasoning },
    });
  }

  if (INDEPENDENT_PROVIDER_IDS.includes(row.provider_id)) {
    if (row.provider_id === "cerebras" && String(row.source_url || "").startsWith(CEREBRAS_PUBLIC_MODELS_URL)) {
      return null;
    }
    if (row.provider_id === "groq" && String(row.source_url || "").startsWith(GROQ_MODELS_URL)) {
      return null;
    }
    if (row.provider_id === "neuralwatt" && String(row.source_url || "").startsWith(NEURALWATT_PRICING_URL)) {
      return null;
    }
    return common(`${row.provider_id}_provider_catalog`, "provider_catalog", `${row.provider_id} hosted model row; provider-specific serving behavior must be probed through Switchback.`, {
      architecture_type: "third-party hosted open/frontier model",
      capabilities: { provider_hosted: true },
    });
  }

  return DIRECT_PROVIDER_RESEARCH[key] ? null : null;
}

const DIRECT_PROVIDER_RESEARCH: Record<string, Json> = {
  "opencode-go/glm-5.2": {
    input_micros_per_mtok: 1_400_000,
    cached_input_micros_per_mtok: 260_000,
    output_micros_per_mtok: 4_400_000,
    source_url: "https://opencode.ai/docs/zen/",
    capabilities: {
      declared_by: "opencode_zen_docs",
      input_modalities: ["text"],
      output_modalities: ["text"],
      text_input: true,
      text_output: true,
      image_input: false,
      video_input: false,
      audio_input: false,
      embeddings_output: false,
      rerank_output: false,
      tool_calling: true,
      json_schema: "unknown",
      streaming: true,
      max_tokens: true,
      reasoning: true,
      provider_hosted: true,
      openai_compatible: true,
      note: "OpenCode Go includes GLM-5.2; OpenCode Zen publishes the pay-as-you-go model price table used for top-up accounting.",
    },
    limits: { context_window: 1_000_000, provider_context_window: 1_000_000 },
    architecture: { source: "opencode_go_data", architecture_type: "OpenCode Go hosted GLM coding model" },
    provenance: [
      source("https://opencode.ai/docs/go/", "provider_docs", "OpenCode Go subscription lane, API-key flow, usage limits, and included models."),
      source("https://opencode.ai/docs/zen/", "model_pricing_docs", "OpenCode Zen per-1M token pricing table lists GLM 5.2 at $1.40 input, $4.40 output, $0.26 cached read."),
      source("https://opencode.ai/data/zhipu/glm-5-2", "usage_data", "OpenCode Go GLM-5.2 usage page lists 1M context and $1.40/$4.40 per 1M token cost."),
    ],
  },
  "nvidia/minimaxai/minimax-m2.7": {
    source_url: "https://integrate.api.nvidia.com/v1/models",
    capabilities: {
      declared_by: "nvidia_model_card",
      input_modalities: ["text"],
      output_modalities: ["text"],
      text_input: true,
      text_output: true,
      image_input: false,
      video_input: false,
      audio_input: false,
      embeddings_output: false,
      rerank_output: false,
      tool_calling: true,
      json_schema: "unknown",
      streaming: true,
      max_tokens: true,
      reasoning: true,
      provider_hosted: true,
      openai_compatible: true,
      note: "NVIDIA Build model card positions MiniMax M2.7 for coding, reasoning, and office tasks; API sample uses max_tokens=8192.",
    },
    limits: { max_completion_tokens: 8_192 },
    architecture: { source: "nvidia_model_card", architecture_type: "large text-to-text model", parameters_total_b: 230 },
    provenance: [
      source("https://build.nvidia.com/minimaxai/minimax-m2.7", "model_card", "MiniMax M2.7 NVIDIA Build model card, free endpoint, parameter count, and task positioning."),
      source("https://docs.api.nvidia.com/nim/reference/minimaxai-minimax-m2.7", "api_reference", "NVIDIA NIM API reference lists minimaxai/minimax-m2.7 chat completion endpoint."),
    ],
  },
  "openai/gpt-5.4": {
    context_window: 1_000_000,
    input_micros_per_mtok: 2_500_000,
    cached_input_micros_per_mtok: 250_000,
    output_micros_per_mtok: 15_000_000,
    source_url: "https://developers.openai.com/api/docs/models",
    capabilities: directCaps("openai_models_api_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      reasoning: true,
      reasoning_controls: ["none", "low", "medium", "high", "xhigh"],
      supported_tools: ["functions", "web_search", "file_search", "computer_use"],
      structured_outputs: true,
    }),
    limits: directLimits(1_000_000, 128_000),
    architecture: { source: "openai_models_api_docs", architecture_type: "proprietary frontier reasoning model" },
    provenance: [source("https://developers.openai.com/api/docs/models", "model_docs", "GPT-5.4 model context, max output, reasoning, and tool surface.")],
  },
  "openai/gpt-5.4-mini": {
    context_window: 400_000,
    input_micros_per_mtok: 750_000,
    cached_input_micros_per_mtok: 75_000,
    output_micros_per_mtok: 4_500_000,
    source_url: "https://developers.openai.com/api/docs/models",
    capabilities: directCaps("openai_models_api_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      reasoning: true,
      reasoning_controls: ["none", "low", "medium", "high", "xhigh"],
      supported_tools: ["functions", "web_search", "file_search", "computer_use"],
      structured_outputs: true,
    }),
    limits: directLimits(400_000, 128_000),
    architecture: { source: "openai_models_api_docs", architecture_type: "proprietary small frontier reasoning model" },
    provenance: [source("https://developers.openai.com/api/docs/models", "model_docs", "GPT-5.4 mini model context, max output, reasoning, and tool surface.")],
  },
  "openai/gpt-5.5": {
    context_window: 1_000_000,
    input_micros_per_mtok: 5_000_000,
    cached_input_micros_per_mtok: 500_000,
    output_micros_per_mtok: 30_000_000,
    source_url: "https://developers.openai.com/api/docs/models",
    capabilities: directCaps("openai_models_api_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      reasoning: true,
      reasoning_controls: ["none", "low", "medium", "high", "xhigh"],
      supported_tools: ["functions", "web_search", "file_search", "computer_use"],
      structured_outputs: true,
    }),
    limits: directLimits(1_000_000, 128_000),
    architecture: { source: "openai_models_api_docs", architecture_type: "proprietary frontier reasoning model" },
    provenance: [source("https://developers.openai.com/api/docs/models", "model_docs", "GPT-5.5 model context, max output, reasoning, and tool surface.")],
  },
  "anthropic/claude-opus-4-5": {
    capabilities: directCaps("anthropic_models_pricing_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      json_schema: "tool-based",
      reasoning: true,
      extended_thinking: true,
    }),
    limits: directLimits(1_000_000, 128_000),
    architecture: { source: "anthropic_models_overview", architecture_type: "proprietary Claude 4 family model" },
    provenance: [source("https://platform.claude.com/docs/en/about-claude/models/overview", "model_docs", "Claude 4 family context, output, tool and vision capability.")],
  },
  "anthropic/claude-opus-4-6": {
    capabilities: directCaps("anthropic_models_pricing_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      json_schema: "tool-based",
      reasoning: true,
      extended_thinking: true,
      adaptive_thinking: true,
    }),
    limits: directLimits(1_000_000, 128_000, { batch_max_completion_tokens: 300_000 }),
    architecture: { source: "anthropic_models_overview", architecture_type: "proprietary Claude 4 family model" },
    benchmarks: { vendor: { source: "anthropic_news", fetched_at: FETCHED_AT, values: { "Terminal-Bench 2.0": 65.4 } } },
    provenance: [source("https://platform.claude.com/docs/en/about-claude/models/overview", "model_docs", "Claude Opus 4.6+ context, output, tool, vision, and thinking capability.")],
  },
  "anthropic/claude-opus-4-7": {
    capabilities: directCaps("anthropic_models_pricing_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      json_schema: "tool-based",
      reasoning: true,
      extended_thinking: true,
      adaptive_thinking: true,
    }),
    limits: directLimits(1_000_000, 128_000),
    architecture: { source: "anthropic_models_overview", architecture_type: "proprietary Claude 4 family model" },
    provenance: [source("https://platform.claude.com/docs/en/about-claude/models/overview", "model_docs", "Claude Opus 4.7 context, output, tool, vision, and thinking capability.")],
  },
  "anthropic/claude-opus-4-8": {
    capabilities: directCaps("anthropic_models_pricing_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      json_schema: "tool-based",
      reasoning: true,
      adaptive_thinking: true,
      default_effort: "high",
    }),
    limits: directLimits(1_000_000, 128_000),
    architecture: { source: "anthropic_models_overview", architecture_type: "proprietary Claude 4 family model" },
    provenance: [source("https://platform.claude.com/docs/en/about-claude/models/overview", "model_docs", "Claude Opus 4.8 context, output, tool, vision, and thinking capability.")],
  },
  "anthropic/claude-sonnet-4-6": {
    capabilities: directCaps("anthropic_models_pricing_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      json_schema: "tool-based",
      reasoning: true,
      extended_thinking: true,
    }),
    limits: directLimits(1_000_000, 128_000, { batch_max_completion_tokens: 300_000 }),
    architecture: { source: "anthropic_models_overview", architecture_type: "proprietary Claude 4 family model" },
    provenance: [source("https://platform.claude.com/docs/en/about-claude/models/overview", "model_docs", "Claude Sonnet 4.6 context, output, tool, vision, and thinking capability.")],
  },
  "anthropic/claude-haiku-4-5": {
    capabilities: directCaps("anthropic_models_pricing_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      json_schema: "tool-based",
      reasoning: false,
    }),
    limits: directLimits(200_000, 64_000),
    architecture: { source: "anthropic_models_overview", architecture_type: "proprietary Claude 4 family model" },
    provenance: [source("https://platform.claude.com/docs/en/about-claude/models/overview", "model_docs", "Claude Haiku 4.5 context, output, tool and vision capability.")],
  },
  "gemini/gemini-2.5-flash": {
    capabilities: directCaps("gemini_api_pricing_docs", {
      input_modalities: ["text", "image", "audio", "video"],
      image_input: true,
      audio_input: true,
      video_input: true,
      reasoning: true,
      thinking_budget: true,
      grounding_tools: ["google_search", "google_maps"],
    }),
    limits: directLimits(1_000_000),
    determinism: directDeterminism(false, "Gemini exposes sampling controls; deterministic seed behavior needs Switchback probe receipt."),
    architecture: { source: "gemini_api_docs", architecture_type: "proprietary hybrid reasoning multimodal model" },
    provenance: [source("https://ai.google.dev/gemini-api/docs/pricing", "model_pricing_docs", "Gemini 2.5 Flash multimodal pricing, context, thinking budgets, grounding.")],
  },
  "gemini/gemini-2.5-flash-lite": {
    capabilities: directCaps("gemini_api_pricing_docs", {
      input_modalities: ["text", "image", "audio", "video"],
      image_input: true,
      audio_input: true,
      video_input: true,
      reasoning: true,
      thinking_budget: true,
      grounding_tools: ["google_search", "google_maps"],
    }),
    limits: directLimits(1_048_576, 65_535),
    determinism: directDeterminism(false, "Gemini exposes sampling controls; deterministic seed behavior needs Switchback probe receipt."),
    architecture: { source: "gemini_api_docs", architecture_type: "proprietary small multimodal model" },
    provenance: [source("https://ai.google.dev/gemini-api/docs/pricing", "model_pricing_docs", "Gemini 2.5 Flash-Lite multimodal pricing, context, and grounding.")],
  },
  "xai/grok-4.3": {
    input_micros_per_mtok: 1_250_000,
    cached_input_micros_per_mtok: 200_000,
    output_micros_per_mtok: 2_500_000,
    source_url: "https://docs.x.ai/developers/models/grok-4.3",
    capabilities: directCaps("xai_model_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      reasoning: true,
      reasoning_controls: ["none", "low", "medium", "high"],
      structured_outputs: true,
      function_calling: true,
    }),
    limits: directLimits(1_000_000),
    architecture: { source: "xai_model_docs", architecture_type: "proprietary Grok reasoning model" },
    provenance: [source("https://docs.x.ai/developers/models/grok-4.3", "model_docs", "Grok 4.3 context, pricing, structured outputs, function calling, configurable reasoning.")],
  },
  "xai/grok-4.1-fast": {
    capabilities: directCaps("xai_model_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      reasoning: true,
      structured_outputs: true,
      function_calling: true,
    }),
    limits: directLimits(2_000_000),
    architecture: { source: "xai_model_docs", architecture_type: "proprietary Grok fast model" },
    provenance: [source("https://docs.x.ai/developers/models", "model_docs", "xAI model list and pricing surface.")],
  },
  "deepseek/deepseek-v4-pro": {
    capabilities: directCaps("deepseek_api_docs", {
      reasoning: true,
      cache_discount: true,
    }),
    limits: directLimits(1_000_000),
    architecture: { source: "deepseek_v4_release", architecture_type: "MoE", mixture_of_experts: true, parameters_total_b: 1600, parameters_active_b: 49 },
    provenance: [source("https://api-docs.deepseek.com/news/news260424", "release_notes", "DeepSeek V4 Pro 1M context and MoE parameter facts.")],
  },
  "deepseek/deepseek-v4-flash": {
    capabilities: directCaps("deepseek_api_docs", {
      reasoning: true,
      cache_discount: true,
    }),
    limits: directLimits(1_000_000),
    architecture: { source: "deepseek_v4_release", architecture_type: "MoE", mixture_of_experts: true, parameters_total_b: 284, parameters_active_b: 13 },
    provenance: [source("https://api-docs.deepseek.com/news/news260424", "release_notes", "DeepSeek V4 Flash 1M context and MoE parameter facts.")],
  },
  "zai/glm-5.2": {
    input_micros_per_mtok: 1_400_000,
    cached_input_micros_per_mtok: 260_000,
    output_micros_per_mtok: 4_400_000,
    source_url: "https://docs.z.ai/guides/overview/pricing",
    capabilities: directCaps("zai_glm52_docs", {
      reasoning: true,
      thinking_modes: true,
      function_calling: true,
      context_caching: true,
      structured_outputs: true,
      mcp: true,
    }),
    limits: directLimits(1_000_000, 128_000),
    architecture: { source: "zai_glm52_docs", architecture_type: "proprietary long-horizon coding model" },
    provenance: [
      source("https://docs.z.ai/guides/overview/pricing", "model_pricing_docs", "GLM-5.2 token pricing table."),
      source("https://docs.z.ai/guides/llm/glm-5.2", "model_docs", "GLM-5.2 context, max output, function calling, caching, structured output, and MCP capabilities."),
    ],
  },
  "zai/glm-5.1": {
    input_micros_per_mtok: 1_400_000,
    cached_input_micros_per_mtok: 260_000,
    output_micros_per_mtok: 4_400_000,
    source_url: "https://docs.z.ai/guides/llm/glm-5.1",
    capabilities: directCaps("zai_glm51_docs", {
      reasoning: true,
      thinking_modes: true,
      function_calling: true,
    }),
    limits: directLimits(200_000, 128_000),
    architecture: { source: "zai_glm51_docs", architecture_type: "proprietary long-horizon agent model" },
    provenance: [source("https://docs.z.ai/guides/llm/glm-5.1", "model_docs", "GLM-5.1 context, max output, function calling and long-horizon positioning.")],
  },
  "zai/glm-5": {
    input_micros_per_mtok: 1_000_000,
    cached_input_micros_per_mtok: 200_000,
    output_micros_per_mtok: 3_200_000,
    source_url: "https://docs.z.ai/guides/overview/pricing",
    capabilities: directCaps("zai_pricing_docs", { reasoning: true, function_calling: true }),
    limits: directLimits(200_000),
    architecture: { source: "zai_blog", architecture_type: "proprietary agentic coding model", attention: "DeepSeek Sparse Attention" },
    provenance: [source("https://docs.z.ai/guides/overview/pricing", "pricing_docs", "Z.ai GLM model pricing table.")],
  },
  "moonshot/kimi-k2.6": {
    capabilities: directCaps("kimi_k26_docs", {
      input_modalities: ["text", "image", "video"],
      image_input: true,
      video_input: true,
      reasoning: true,
      thinking_modes: ["enabled", "disabled"],
      multi_step_tool_invocation: true,
    }),
    limits: directLimits(256_000),
    architecture: { source: "kimi_k26_docs", architecture_type: "native multimodal long-horizon coding model" },
    provenance: [source("https://platform.kimi.ai/docs/guide/kimi-k2-6-quickstart", "model_docs", "Kimi K2.6 multimodal, context, thinking and tool capability.")],
  },
  "moonshot/kimi-k2.5": {
    capabilities: directCaps("kimi_k26_docs", {
      input_modalities: ["text", "image", "video"],
      image_input: true,
      video_input: true,
      reasoning: true,
      thinking_modes: ["enabled", "disabled"],
      multi_step_tool_invocation: true,
    }),
    limits: directLimits(256_000),
    architecture: { source: "kimi_k26_docs", architecture_type: "native multimodal long-horizon coding model" },
    provenance: [source("https://platform.kimi.ai/docs/guide/kimi-k2-6-quickstart", "model_docs", "Kimi K2.5/K2.6 family context, multimodal and thinking capability.")],
  },
  "mistral/codestral": {
    capabilities: directCaps("mistral_pricing_docs", {
      input_modalities: ["text"],
      coding: true,
      fill_in_middle: true,
      json_schema: "native",
    }),
    limits: directLimits(256_000),
    architecture: { source: "mistral_pricing_docs", architecture_type: "proprietary coding model" },
    provenance: [source("https://mistral.ai/pricing/", "pricing_docs", "Codestral coding model pricing and positioning.")],
  },
  "mistral/ministral-14b": {
    input_micros_per_mtok: 200_000,
    output_micros_per_mtok: 200_000,
    source_url: "https://mistral.ai/pricing/",
    capabilities: directCaps("mistral_pricing_docs", { agentic: true, lightweight: true }),
    limits: directLimits(256_000),
    architecture: { source: "mistral_pricing_docs", architecture_type: "open-weight dense/small model", parameters_total_b: 14 },
    provenance: [source("https://mistral.ai/pricing/", "pricing_docs", "Ministral 3 14B pricing and agentic positioning.")],
  },
  "mistral/ministral-8b": {
    input_micros_per_mtok: 150_000,
    output_micros_per_mtok: 150_000,
    source_url: "https://mistral.ai/pricing/",
    capabilities: directCaps("mistral_pricing_docs", { agentic: true, lightweight: true }),
    limits: directLimits(128_000),
    architecture: { source: "mistral_pricing_docs", architecture_type: "open-weight dense/small model", parameters_total_b: 8 },
    provenance: [source("https://mistral.ai/pricing/", "pricing_docs", "Ministral 3 8B pricing and agentic positioning.")],
  },
  "cohere/command-a": {
    capabilities: directCaps("cohere_model_docs", {
      input_modalities: ["text"],
      tool_calling: true,
      citations: true,
      rag: true,
      multilingual: true,
    }),
    limits: directLimits(256_000, 8_000),
    architecture: { source: "cohere_model_docs", architecture_type: "enterprise command model" },
    provenance: [source("https://docs.cohere.com/docs/models", "model_docs", "Command A context and agent/RAG/tool positioning.")],
  },
  "cohere/command-r-plus": {
    capabilities: directCaps("cohere_model_docs", {
      input_modalities: ["text"],
      tool_calling: true,
      citations: true,
      rag: true,
      multilingual: true,
    }),
    limits: directLimits(128_000),
    architecture: { source: "cohere_model_docs", architecture_type: "enterprise RAG/tool-use model" },
    provenance: [source("https://docs.cohere.com/docs/command-r-plus", "model_docs", "Command R+ RAG and multi-step tool-use positioning.")],
  },
  "cohere/command-r7b": {
    input_micros_per_mtok: 37_500,
    output_micros_per_mtok: 150_000,
    capabilities: directCaps("cohere_model_docs", {
      input_modalities: ["text", "image"],
      image_input: true,
      tool_calling: true,
      structured_outputs: true,
      citations: true,
      rag: true,
      reasoning: true,
      multilingual: true,
    }),
    limits: directLimits(128_000, 4_000),
    architecture: { source: "cohere_model_docs", architecture_type: "open-weight dense model", parameters_total_b: 7 },
    provenance: [source("https://docs.cohere.com/docs/command-r7b", "model_docs", "Command R7B pricing, context, max output and capability list.")],
  },
  "alibaba/qwen3.7-max": {
    capabilities: directCaps("qwen_model_docs", {
      input_modalities: ["text"],
      reasoning: true,
      tool_calling: true,
      prompt_caching: true,
    }),
    limits: directLimits(1_000_000),
    architecture: { source: "qwen_blog", architecture_type: "proprietary agent foundation model" },
    provenance: [source("https://qwen.ai/blog?id=qwen3.7", "model_blog", "Qwen3.7-Max agent foundation positioning.")],
  },
  "bedrock/amazon.nova-pro": {
    capabilities: directCaps("bedrock_nova_docs", {
      input_modalities: ["text", "image", "video"],
      image_input: true,
      video_input: true,
      json_schema: "native",
    }),
    limits: directLimits(300_000),
    architecture: { source: "bedrock_docs", architecture_type: "Amazon Nova multimodal model" },
    provenance: [source("https://docs.aws.amazon.com/bedrock/latest/userguide/models-supported.html", "model_catalog", "Bedrock supported model family and modality catalog.")],
  },
  "bedrock/amazon.nova-lite": {
    capabilities: directCaps("bedrock_nova_docs", {
      input_modalities: ["text", "image", "video"],
      image_input: true,
      video_input: true,
      json_schema: "native",
    }),
    limits: directLimits(300_000),
    architecture: { source: "bedrock_docs", architecture_type: "Amazon Nova multimodal model" },
    provenance: [source("https://docs.aws.amazon.com/bedrock/latest/userguide/models-supported.html", "model_catalog", "Bedrock supported model family and modality catalog.")],
  },
  "bedrock/amazon.nova-micro": {
    capabilities: directCaps("bedrock_nova_docs", {
      input_modalities: ["text"],
      image_input: false,
      video_input: false,
      json_schema: "native",
    }),
    limits: directLimits(128_000),
    architecture: { source: "bedrock_docs", architecture_type: "Amazon Nova text model" },
    provenance: [source("https://docs.aws.amazon.com/bedrock/latest/userguide/models-supported.html", "model_catalog", "Bedrock supported model family and modality catalog.")],
  },
};

function mergeDirectResearch(row: Json): Json {
  const exact = DIRECT_PROVIDER_RESEARCH[`${row.provider_id}/${row.model_id}`] || {};
  const family = familyResearch(row) || {};
  const research: Json = {
    ...family,
    ...exact,
    capabilities: { ...(family.capabilities || {}), ...(exact.capabilities || {}) },
    determinism: { ...(family.determinism || {}), ...(exact.determinism || {}) },
    limits: { ...(family.limits || {}), ...(exact.limits || {}) },
    architecture: { ...(family.architecture || {}), ...(exact.architecture || {}) },
    benchmarks: { ...(family.benchmarks || {}), ...(exact.benchmarks || {}) },
    provenance: [...(family.provenance || []), ...(exact.provenance || [])],
  };
  if (Object.keys(research.benchmarks || {}).length === 0) delete research.benchmarks;
  if (!family.capabilities && !exact.capabilities) return row;
  const out: Json = {
    ...row,
    ...research,
    capabilities: { ...(row.capabilities || {}), ...(research.capabilities || {}) },
    determinism: { ...(row.determinism || {}), ...(research.determinism || directDeterminism(false)) },
    limits: { ...(row.limits || {}), ...(research.limits || {}) },
    architecture: { ...(row.architecture || {}), ...(research.architecture || {}) },
    verification: { declared: true, probed: false, probes: row.verification?.probes || {}, ...(row.verification || {}) },
  };
  if (row.benchmarks || research.benchmarks) out.benchmarks = { ...(row.benchmarks || {}), ...(research.benchmarks || {}) };
  for (const item of research.provenance || []) appendProvenance(out, item);
  return out;
}

function stripEmptyContainers(row: Json): Json {
  const out = { ...row };
  if (out.benchmarks && Object.keys(out.benchmarks).length === 0) delete out.benchmarks;
  if (out.architecture && Object.keys(out.architecture).length === 0) delete out.architecture;
  if (out.capabilities && Object.keys(out.capabilities).length === 0) delete out.capabilities;
  return out;
}

function validateRegistry(registry: Json): string[] {
  const problems: string[] = [];
  const pricingUnits = new Set([
    "per_image",
    "per_megapixel",
    "per_second",
    "per_video",
    "token_metered",
    "credits",
    "quota",
  ]);
  if (registry.schema !== "switchback/provider-registry@3") {
    problems.push(`schema must be switchback/provider-registry@3, got ${registry.schema || "missing"}`);
  }
  const seen = new Set<string>();
  for (const model of registry.models || []) {
    const key = `${model.provider_id}/${model.model_id}`;
    const pricingUnit = model.pricing_unit || "token_metered";
    const mediaPriced = pricingUnit !== "token_metered";
    if (seen.has(key)) problems.push(`duplicate model row: ${key}`);
    seen.add(key);
    if (!pricingUnits.has(pricingUnit)) problems.push(`${key}: unsupported pricing_unit ${pricingUnit}`);
    if (mediaPriced && (!Number.isInteger(model.unit_price_micros) || model.unit_price_micros < 0)) {
      problems.push(`${key}: unit_price_micros must be a non-negative integer for ${pricingUnit}`);
    }
    for (const field of ["input_micros_per_mtok", "output_micros_per_mtok", "cached_input_micros_per_mtok"]) {
      if (model[field] != null && (!Number.isInteger(model[field]) || model[field] < 0)) {
        problems.push(`${key}: ${field} must be non-negative integer or null`);
      }
    }
    if (!model.capabilities || Object.keys(model.capabilities).length === 0) problems.push(`${key}: missing capabilities`);
    if (!mediaPriced && (!model.limits || Object.keys(model.limits).length === 0)) problems.push(`${key}: missing limits`);
    if (!mediaPriced && (!model.architecture || Object.keys(model.architecture).length === 0)) problems.push(`${key}: missing architecture`);
    if (!model.provenance || model.provenance.length === 0) problems.push(`${key}: missing provenance`);
    if (mediaPriced && model.provider_id === "fal" && model.source_url !== "https://fal.ai/pricing") problems.push(`${key}: media seed missing fal pricing source_url`);
    if (mediaPriced && model.provider_id === "fal" && !String(model.note || "").includes("seeded 2026-07-15, re-verify via refresh")) {
      problems.push(`${key}: media seed note must require refresh verification`);
    }
    if (model.benchmarks && Object.keys(model.benchmarks).length === 0) problems.push(`${key}: empty benchmarks object`);
    const enrichedProvider = model.provider_id === "openrouter" || model.provider_id === "nvidia";
    if (enrichedProvider && model.input_micros_per_mtok === 0 && model.output_micros_per_mtok === 0) {
      if (!model.capabilities) problems.push(`${key}: free row missing capabilities`);
      if (!model.provenance || model.provenance.length === 0) problems.push(`${key}: free row missing provenance`);
    }
  }
  const providers = new Map((registry.providers || []).map((provider: Json) => [provider.id, provider]));
  for (const providerId of INDEPENDENT_PROVIDER_IDS) {
    const provider = providers.get(providerId);
    if (!provider) continue;
    const research = provider.provider_research || {};
    if (research.status !== "official_docs_cross_checked") problems.push(`${providerId}: provider research not cross-checked`);
    if (!research.docs_url) problems.push(`${providerId}: provider research missing docs_url`);
    if (!research.models_url) problems.push(`${providerId}: provider research missing models_url`);
    if (!research.official_base_url) problems.push(`${providerId}: provider research missing official_base_url`);
    if (!Array.isArray(provider.provider_sources) || provider.provider_sources.length === 0) {
      problems.push(`${providerId}: provider research missing sources`);
    }
    const catalog = registry.provider_catalogs?.[`${providerId}_provider`];
    if (!catalog) problems.push(`${providerId}: missing provider catalog descriptor`);
  }
  return problems;
}

async function main() {
  const registry = await readJson(registryPath);

  if (!checkOnly) {
    let openrouter: Json | null = null;
    let nvidia: Json | null = null;
    let cerebras: Json | null = null;
    let groq: Json | null = null;

    if (openrouterPath) openrouter = await readJson(openrouterPath);
    if (nvidiaPath) nvidia = await readJson(nvidiaPath);
    if (cerebrasPath) cerebras = await readJson(cerebrasPath);
    if (groqPath) groq = await readJson(groqPath);
    if (fetchLive) {
      openrouter = await fetchJson(OPENROUTER_MODELS_URL);
      nvidia = await fetchJson(NVIDIA_MODELS_URL);
    }
    registry.schema = "switchback/provider-registry@3";
    registry.generated = FETCHED_AT.slice(0, 10);
    registry.metadata_contract = {
      status: "declared provider facts + optional Switchback probe receipts",
      money: registry.money,
      fields: {
        capabilities: "Provider/catalog-declared modalities and API parameters.",
        determinism: "Declared knobs such as seed; deterministic behavior still needs probe receipt.",
        architecture: "Dense/MoE/parameter facts with source; absent means unknown, not dense.",
        benchmarks: "Vendor or third-party benchmark values, never treated as local certification.",
      verification: "Switchback probe receipts go here; declared facts alone are not proof.",
        freshness: "Catalog, provenance, and probe timestamps are evidence freshness; model availability, prices, and capabilities can change.",
        provider_research: "Provider-level official docs cross-checks for hosts that do not yet have ingested model rows.",
    },
  };
    registry.providers = upsertProvider(registry.providers || [], NEURALWATT_PROVIDER).map((provider: Json) => mergeProviderResearch(provider));
    registry.provider_catalogs = {
      ...(registry.provider_catalogs || {}),
      ...independentProviderCatalogs(),
    };

    const rows = new Map<string, Json>();
    for (const row of registry.models || []) rows.set(`${row.provider_id}/${row.model_id}`, row);

    for (const seed of NEURALWATT_MODELS) {
      const key = `neuralwatt/${seed.model_id}`;
      rows.set(key, neuralwattRow(seed, rows.get(key)));
    }
    registry.provider_catalogs = {
      ...(registry.provider_catalogs || {}),
      neuralwatt_pricing: {
        source_url: NEURALWATT_PRICING_URL,
        energy_status_url: NEURALWATT_ENERGY_URL,
        fetched_at: FETCHED_AT,
        provider_id: "neuralwatt",
        status: "pricing_page_ingested",
        energy_usd_per_kwh: 5,
        observation_window_days: 7,
        total_models: NEURALWATT_MODELS.length,
        model_ids: NEURALWATT_MODELS.map((model) => model.model_id).sort(),
      },
    };

    if (openrouter?.data) {
      const freeRows = openrouter.data.filter((m: Json) => {
        const output = m.architecture?.output_modalities || [];
        const textOutput = output.includes("text");
        const tokenFree = m.pricing?.prompt === "0" && m.pricing?.completion === "0";
        return textOutput && (m.id?.endsWith(":free") || tokenFree);
      });
      const freeIds = new Set(freeRows.map((m: Json) => m.id));
      for (const [key, row] of [...rows.entries()]) {
        const generatedFromOpenRouter = (row.provenance || []).some(
          (p: Json) => p.source_url === OPENROUTER_MODELS_URL,
        );
        if (row.provider_id === "openrouter" && generatedFromOpenRouter && !freeIds.has(row.model_id)) {
          rows.delete(key);
        }
      }
      for (const model of freeRows) {
        const key = `openrouter/${model.id}`;
        rows.set(key, openrouterRow(model, rows.get(key)));
      }
      registry.provider_catalogs = {
        ...(registry.provider_catalogs || {}),
        openrouter_free: {
          source_url: OPENROUTER_MODELS_URL,
          fetched_at: FETCHED_AT,
          total_models: openrouter.data.length,
          free_models: freeRows.length,
          benchmarked_free_models: freeRows.filter((m: Json) => m.benchmarks).length,
          model_ids: freeRows.map((m: Json) => m.id).sort(),
        },
      };
    }

  if (nvidia?.data) {
    const nvidiaIds = new Set<string>(nvidia.data.map((m: Json) => m.id));
    for (const [key, row] of rows) {
      if (row.provider_id === "nvidia") rows.set(key, mergeNvidiaOverrides(row, nvidiaIds));
    }
      registry.provider_catalogs = {
        ...(registry.provider_catalogs || {}),
        nvidia_build: {
          source_url: NVIDIA_MODELS_URL,
          fetched_at: FETCHED_AT,
          total_models: nvidia.data.length,
          model_ids: nvidia.data.map((m: Json) => m.id).sort(),
      },
    };
  }

    if (cerebras?.data) {
      const cerebrasRows = cerebras.data.filter((m: Json) => m?.id && !m.deprecated);
      const cerebrasIds = new Set(cerebrasRows.map((m: Json) => m.id));
    for (const [key, row] of [...rows.entries()]) {
      const generatedFromCerebras = (row.provenance || []).some(
        (p: Json) => p.source_url === CEREBRAS_PUBLIC_MODELS_URL,
      );
      if (row.provider_id === "cerebras" && generatedFromCerebras && !cerebrasIds.has(row.model_id)) {
        rows.delete(key);
      }
    }
    for (const model of cerebrasRows) {
      const key = `cerebras/${model.id}`;
      rows.set(key, cerebrasRow(model, rows.get(key)));
    }
    registry.provider_catalogs = {
      ...(registry.provider_catalogs || {}),
      cerebras_public: {
        source_url: CEREBRAS_PUBLIC_MODELS_URL,
        fetched_at: FETCHED_AT,
        total_models: cerebras.data.length,
        active_models: cerebrasRows.length,
        model_ids: cerebrasRows.map((m: Json) => m.id).sort(),
      },
      cerebras_provider: {
        ...(registry.provider_catalogs?.cerebras_provider || {}),
        status: "provider_catalog_ingested",
        source_url: CEREBRAS_PUBLIC_MODELS_URL,
        fetched_at: FETCHED_AT,
        total_models: cerebras.data.length,
        active_models: cerebrasRows.length,
        model_ids: cerebrasRows.map((m: Json) => m.id).sort(),
      },
      };
    }

    if (groq?.data) {
      const groqRows = groq.data.filter((m: Json) => m?.id && m.active !== false);
      const groqIds = new Set(groqRows.map((m: Json) => m.id));
      for (const [key, row] of [...rows.entries()]) {
        const generatedFromGroq = (row.provenance || []).some(
          (p: Json) => p.source_url === GROQ_MODELS_URL,
        );
        if (row.provider_id === "groq" && generatedFromGroq && !groqIds.has(row.model_id)) rows.delete(key);
      }
      for (const model of groqRows) {
        const key = `groq/${model.id}`;
        rows.set(key, groqRow(model, rows.get(key)));
      }
      registry.provider_catalogs = {
        ...(registry.provider_catalogs || {}),
        groq_catalog: {
          source_url: GROQ_MODELS_URL,
          fetched_at: FETCHED_AT,
          total_models: groq.data.length,
          active_models: groqRows.length,
          model_ids: groqRows.map((m: Json) => m.id).sort(),
        },
        groq_provider: {
          ...(registry.provider_catalogs?.groq_provider || {}),
          status: "provider_catalog_ingested",
          source_url: GROQ_MODELS_URL,
          fetched_at: FETCHED_AT,
          total_models: groq.data.length,
          active_models: groqRows.length,
          model_ids: groqRows.map((m: Json) => m.id).sort(),
        },
      };
    }

    for (const [key, row] of rows) rows.set(key, mergeDirectResearch(row));
for (const [key, row] of rows) rows.set(key, stripEmptyContainers(row));

registry.models = [...rows.values()].sort((a, b) => {
      const ap = a.provider_id || "";
      const bp = b.provider_id || "";
      if (ap !== bp) return ap.localeCompare(bp);
      return String(a.model_id || "").localeCompare(String(b.model_id || ""));
    });
    registry.counts = {
      ...(registry.counts || {}),
      providers: registry.providers?.length || 0,
      models: registry.models.length,
  free_models: registry.models.filter((m: Json) => m.input_micros_per_mtok === 0 && m.output_micros_per_mtok === 0).length,
  benchmarked_models: registry.models.filter((m: Json) => m.benchmarks).length,
  enriched_models: registry.models.filter((m: Json) => m.capabilities || m.architecture || m.benchmarks).length,
  enriched_providers: (registry.providers || []).filter((p: Json) => p.provider_research).length,
  };
  }

  const problems = validateRegistry(registry);
  if (problems.length > 0) {
    for (const problem of problems) console.error(problem);
    process.exit(1);
  }

  if (checkOnly) {
    console.log(`provider-registry OK: models=${registry.models?.length ?? 0} free=${registry.counts?.free_models ?? "?"} benchmarked=${registry.counts?.benchmarked_models ?? "?"}`);
    return;
  }

  const body = JSON.stringify(registry, null, 2) + "\n";
  if (apply) {
    await writeFile(outPath, body);
    console.log(`wrote ${outPath}`);
  } else {
    process.stdout.write(body);
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
