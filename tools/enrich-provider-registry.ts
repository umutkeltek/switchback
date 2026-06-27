#!/usr/bin/env bun
import { readFile, writeFile } from "node:fs/promises";

type Json = Record<string, any>;

const DEFAULT_REGISTRY = "config/provider-registry.json";
const OPENROUTER_MODELS_URL = "https://openrouter.ai/api/v1/models?output_modalities=all";
const NVIDIA_MODELS_URL = "https://integrate.api.nvidia.com/v1/models";
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

function usage(): never {
  console.log(`usage:
  bun tools/enrich-provider-registry.ts --fetch --apply
  bun tools/enrich-provider-registry.ts --openrouter-json FILE --nvidia-json FILE --out FILE
  bun tools/enrich-provider-registry.ts --check

Options:
  --registry FILE       input registry, default config/provider-registry.json
  --out FILE            output registry, default same as input
  --fetch               fetch OpenRouter + NVIDIA public catalogs
  --openrouter-json F   use cached OpenRouter /api/v1/models response
  --nvidia-json F       use cached NVIDIA /v1/models response
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

const NVIDIA_OVERRIDES: Record<string, Json> = {
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

function validateRegistry(registry: Json): string[] {
  const problems: string[] = [];
  const seen = new Set<string>();
  for (const model of registry.models || []) {
    const key = `${model.provider_id}/${model.model_id}`;
    if (seen.has(key)) problems.push(`duplicate model row: ${key}`);
    seen.add(key);
    for (const field of ["input_micros_per_mtok", "output_micros_per_mtok", "cached_input_micros_per_mtok"]) {
      if (model[field] != null && (!Number.isInteger(model[field]) || model[field] < 0)) {
        problems.push(`${key}: ${field} must be non-negative integer or null`);
      }
    }
    const enrichedProvider = model.provider_id === "openrouter" || model.provider_id === "nvidia";
    if (enrichedProvider && model.input_micros_per_mtok === 0 && model.output_micros_per_mtok === 0) {
      if (!model.capabilities) problems.push(`${key}: free row missing capabilities`);
      if (!model.provenance || model.provenance.length === 0) problems.push(`${key}: free row missing provenance`);
    }
  }
  return problems;
}

async function main() {
  const registry = await readJson(registryPath);

  if (!checkOnly) {
    let openrouter: Json | null = null;
    let nvidia: Json | null = null;

    if (openrouterPath) openrouter = await readJson(openrouterPath);
    if (nvidiaPath) nvidia = await readJson(nvidiaPath);
    if (fetchLive) {
      openrouter = await fetchJson(OPENROUTER_MODELS_URL);
      nvidia = await fetchJson(NVIDIA_MODELS_URL);
    }
    if (!openrouter && !nvidia) {
      throw new Error("no input catalogs supplied; use --fetch, --openrouter-json, --nvidia-json, or --check");
    }

    registry.schema = "switchback/provider-registry@2";
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
      },
    };

    const rows = new Map<string, Json>();
    for (const row of registry.models || []) rows.set(`${row.provider_id}/${row.model_id}`, row);

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
