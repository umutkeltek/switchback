#!/usr/bin/env bun
import { randomUUID } from "node:crypto";
import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

type Json = Record<string, any>;

const DEFAULT_REGISTRY = "config/provider-registry.json";
const DEFAULT_RECEIPT_DIR = `${process.env.HOME || "."}/.local/state/switchback/registry/enrichment-runs`;
const OPENROUTER_MODELS_URL = "https://openrouter.ai/api/v1/models?output_modalities=all";
const NVIDIA_MODELS_URL = "https://integrate.api.nvidia.com/v1/models";
const CEREBRAS_PUBLIC_MODELS_URL = "https://api.cerebras.ai/public/v1/models";
const GROQ_MODELS_URL = "https://api.groq.com/openai/v1/models";

type SourceId = "openrouter" | "nvidia" | "cerebras" | "groq";

type SourceAdapter = {
  id: SourceId;
  name: string;
  url: string;
  authEnv?: string;
  enrichArg: string;
  providerCatalogKeys: string[];
  providerFields: string[];
  stats: (json: Json) => Json;
};

type Options = {
  registry: string;
  out: string;
  sources: SourceId[];
  cached: Partial<Record<SourceId, string>>;
  apply: boolean;
  checkDrift: boolean;
  json: boolean;
  failOnDrift: boolean;
  writeReceipt: boolean;
  receiptDir: string;
  limit: number;
};

type SourcePayload = {
  adapter: SourceAdapter;
  path: string;
  fetched: boolean;
  stats: Json;
};

type ModelChange = {
  key: string;
  categories: string[];
  fields: string[];
  stale_probe: boolean;
};

type Drift = {
  added_models: string[];
  removed_models: string[];
  changed_models: ModelChange[];
  provider_catalog_changes: string[];
  stale_probe_rows: string[];
  summary: Json;
};

const SELECTED_NVIDIA_IDS = [
  "minimaxai/minimax-m2.7",
  "minimaxai/minimax-m3",
  "nvidia/nemotron-3-ultra-550b-a55b",
  "nvidia/nemotron-3-super-120b-a12b",
];

const SOURCE_ADAPTERS: Record<SourceId, SourceAdapter> = {
  openrouter: {
    id: "openrouter",
    name: "OpenRouter public models",
    url: OPENROUTER_MODELS_URL,
    enrichArg: "--openrouter-json",
    providerCatalogKeys: ["openrouter_free"],
    providerFields: [
      "provider_catalogs.openrouter_free",
      "pricing",
      "limits",
      "capabilities",
      "determinism",
      "architecture",
      "benchmarks.openrouter",
      "provenance",
    ],
    stats: (json) => {
      const rows = Array.isArray(json.data) ? json.data : [];
      const freeRows = rows.filter((model: Json) => {
        const outputs = model.architecture?.output_modalities || [];
        const tokenFree = model.pricing?.prompt === "0" && model.pricing?.completion === "0";
        return outputs.includes("text") && (String(model.id || "").endsWith(":free") || tokenFree);
      });
      return {
        total_models: rows.length,
        free_text_models: freeRows.length,
        benchmarked_free_text_models: freeRows.filter((model: Json) => model.benchmarks).length,
        selected_free_ids: freeRows
          .map((model: Json) => model.id)
          .filter((id: string) => /nemotron|qwen|deepseek|minimax|openai|openrouter/i.test(id))
          .slice(0, 25)
          .sort(),
      };
    },
  },
  nvidia: {
    id: "nvidia",
    name: "NVIDIA Build public models",
    url: NVIDIA_MODELS_URL,
    enrichArg: "--nvidia-json",
    providerCatalogKeys: ["nvidia_build"],
    providerFields: [
      "provider_catalogs.nvidia_build",
      "limits.free_tier_rpm_reported",
      "capabilities",
      "determinism",
      "architecture",
      "benchmarks.vendor",
      "verification.catalog_seen",
      "provenance",
    ],
    stats: (json) => {
      const rows = Array.isArray(json.data) ? json.data : [];
      return {
        total_models: rows.length,
        selected_enrichment_ids: rows
          .map((model: Json) => model.id)
          .filter((id: string) => SELECTED_NVIDIA_IDS.includes(id))
          .sort(),
      };
    },
  },
  cerebras: {
    id: "cerebras",
    name: "Cerebras public models",
    url: CEREBRAS_PUBLIC_MODELS_URL,
    enrichArg: "--cerebras-json",
    providerCatalogKeys: ["cerebras_public", "cerebras_provider"],
    providerFields: [
      "provider_catalogs.cerebras_public",
      "provider_catalogs.cerebras_provider",
      "pricing",
      "limits",
      "capabilities",
      "determinism",
      "architecture",
      "verification.catalog_seen",
      "provenance",
    ],
    stats: (json) => {
      const rows = Array.isArray(json.data) ? json.data : [];
      const activeRows = rows.filter((model: Json) => model?.id && !model.deprecated);
      return {
        total_models: rows.length,
        active_models: activeRows.length,
        selected_model_ids: activeRows
          .map((model: Json) => model.id)
          .filter((id: string) => /gpt|glm|llama|qwen|deepseek|gemma/i.test(id))
          .slice(0, 25)
          .sort(),
      };
    },
  },
  groq: {
    id: "groq",
    name: "Groq OpenAI-compatible models",
    url: GROQ_MODELS_URL,
    authEnv: "GROQ_API_KEY",
    enrichArg: "--groq-json",
    providerCatalogKeys: ["groq_catalog", "groq_provider"],
    providerFields: [
      "provider_catalogs.groq_catalog",
      "provider_catalogs.groq_provider",
      "limits",
      "capabilities",
      "determinism",
      "architecture",
      "verification.catalog_seen",
      "provenance",
    ],
    stats: (json) => {
      const rows = Array.isArray(json.data) ? json.data : [];
      const activeRows = rows.filter((model: Json) => model?.id && model.active !== false);
      return {
        total_models: rows.length,
        active_models: activeRows.length,
        selected_model_ids: activeRows
          .map((model: Json) => model.id)
          .filter((id: string) => /compound|gpt|llama|qwen|deepseek|gemma|whisper|guard/i.test(id))
          .slice(0, 25)
          .sort(),
      };
    },
  },
};

const FIELD_GROUPS: Record<string, string[]> = {
  pricing: ["input_micros_per_mtok", "output_micros_per_mtok", "cached_input_micros_per_mtok"],
  context: [
    "context_window",
    "limits.context_window",
    "limits.provider_context_window",
    "limits.max_completion_tokens",
    "limits.per_request_limits",
  ],
  capabilities: [
    "vision",
    "tool_calling",
    "json_schema",
    "capabilities.input_modalities",
    "capabilities.output_modalities",
    "capabilities.supported_parameters",
    "capabilities.tool_calling",
    "capabilities.json_schema",
    "capabilities.seed",
    "capabilities.reasoning",
    "capabilities.image_input",
    "determinism.seed_supported",
  ],
  architecture: ["architecture"],
  benchmarks: ["benchmarks"],
  catalog: ["source_url", "provenance", "verification.catalog_seen"],
};

const STALE_PROBE_CATEGORIES = new Set(["pricing", "context", "capabilities", "architecture", "benchmarks"]);
const VOLATILE_KEYS = new Set(["fetched_at", "catalog_seen_at"]);

function usage(code = 2): never {
  console.log(`usage:
  bun tools/refresh-provider-registry.ts --check-drift
  sb registry refresh --check-drift
  sb registry refresh --source openrouter --source nvidia --apply
  sb registry refresh --source cerebras --cerebras-json FILE --check-drift
  sb registry refresh --source groq --groq-json FILE --check-drift

Options:
  --registry FILE       registry path, default config/provider-registry.json
  --out FILE            output registry path, default same as --registry
  --source ID           openrouter|nvidia|cerebras|groq|independent|all; repeatable, default all
  --openrouter-json F   cached OpenRouter models response
  --nvidia-json F       cached NVIDIA models response
  --cerebras-json F     cached Cerebras public models response
  --groq-json F         cached Groq /openai/v1/models response
  --check-drift         print drift summary; no registry write unless --apply
  --apply               write refreshed registry
  --json                emit JSON enrichment-run receipt
  --fail-on-drift       exit 1 when drift exists
  --receipt-dir DIR     receipt directory, default ${DEFAULT_RECEIPT_DIR}
  --no-receipt          skip local enrichment-run receipt
  --limit N             changed rows to print, default 30`);
  process.exit(code);
}

function parseArgs(argv: string[]): Options {
  const options: Options = {
    registry: DEFAULT_REGISTRY,
    out: "",
    sources: [],
    cached: {},
    apply: false,
    checkDrift: false,
    json: false,
    failOnDrift: false,
    writeReceipt: true,
    receiptDir: DEFAULT_RECEIPT_DIR,
    limit: 30,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      const value = argv[++i];
      if (!value) throw new Error(`${arg} requires a value`);
      return value;
    };

    if (arg === "--help" || arg === "-h") usage(0);
    else if (arg === "--registry") options.registry = next();
    else if (arg.startsWith("--registry=")) options.registry = arg.slice("--registry=".length);
    else if (arg === "--out") options.out = next();
    else if (arg.startsWith("--out=")) options.out = arg.slice("--out=".length);
    else if (arg === "--source") options.sources.push(...parseSources(next()));
    else if (arg.startsWith("--source=")) options.sources.push(...parseSources(arg.slice("--source=".length)));
    else if (arg === "--openrouter-json") options.cached.openrouter = next();
    else if (arg.startsWith("--openrouter-json=")) options.cached.openrouter = arg.slice("--openrouter-json=".length);
    else if (arg === "--nvidia-json") options.cached.nvidia = next();
    else if (arg.startsWith("--nvidia-json=")) options.cached.nvidia = arg.slice("--nvidia-json=".length);
    else if (arg === "--cerebras-json") options.cached.cerebras = next();
    else if (arg.startsWith("--cerebras-json=")) options.cached.cerebras = arg.slice("--cerebras-json=".length);
    else if (arg === "--groq-json") options.cached.groq = next();
    else if (arg.startsWith("--groq-json=")) options.cached.groq = arg.slice("--groq-json=".length);
    else if (arg === "--check-drift") options.checkDrift = true;
    else if (arg === "--apply") options.apply = true;
    else if (arg === "--json") options.json = true;
    else if (arg === "--fail-on-drift") options.failOnDrift = true;
    else if (arg === "--receipt-dir") options.receiptDir = next();
    else if (arg.startsWith("--receipt-dir=")) options.receiptDir = arg.slice("--receipt-dir=".length);
    else if (arg === "--no-receipt") options.writeReceipt = false;
    else if (arg === "--limit") options.limit = Number.parseInt(next(), 10);
    else if (arg.startsWith("--limit=")) options.limit = Number.parseInt(arg.slice("--limit=".length), 10);
    else throw new Error(`unknown argument: ${arg}`);
  }

  if (options.sources.length === 0) options.sources = ["openrouter", "nvidia"];
  options.sources = [...new Set(options.sources)];
  if (!options.out) options.out = options.registry;
  if (!Number.isFinite(options.limit) || options.limit < 1) throw new Error("--limit must be >= 1");
  return options;
}

function parseSources(raw: string): SourceId[] {
  return raw.split(",").flatMap((part) => {
    const source = part.trim().toLowerCase();
    if (!source) return [];
    if (source === "all") return ["openrouter", "nvidia"];
    if (source === "independent") return ["cerebras", "groq"];
    if (source === "openrouter" || source === "nvidia" || source === "cerebras" || source === "groq") return [source];
    throw new Error(`unknown source: ${source}`);
  });
}

async function readJson(path: string): Promise<Json> {
  return JSON.parse(await readFile(path, "utf8"));
}

async function fetchJson(adapter: SourceAdapter): Promise<Json> {
  const headers: Record<string, string> = { accept: "application/json" };
  if (adapter.authEnv) {
    const token = process.env[adapter.authEnv];
    if (!token) throw new Error(`source ${adapter.id} requires ${adapter.authEnv} or ${adapter.enrichArg} FILE`);
    headers.authorization = `Bearer ${token}`;
  }
  const response = await fetch(adapter.url, { headers });
  if (!response.ok) throw new Error(`fetch failed ${response.status}: ${adapter.url}`);
  return response.json();
}

async function loadSources(options: Options): Promise<SourcePayload[]> {
  const dir = await mkdtemp(join(tmpdir(), "switchback-registry-refresh-"));
  const payloads: SourcePayload[] = [];

  for (const source of options.sources) {
    const adapter = SOURCE_ADAPTERS[source];
    const cached = options.cached[source];
    if (cached) {
      const json = await readJson(cached);
      payloads.push({ adapter, path: cached, fetched: false, stats: adapter.stats(json) });
      continue;
    }

    const json = await fetchJson(adapter);
    const path = join(dir, `${source}.json`);
    await writeFile(path, JSON.stringify(json, null, 2) + "\n");
    payloads.push({ adapter, path, fetched: true, stats: adapter.stats(json) });
  }

  return payloads;
}

async function buildCandidateRegistry(options: Options, sources: SourcePayload[], fetchedAt: string): Promise<Json> {
  const enrichPath = fileURLToPath(new URL("./enrich-provider-registry.ts", import.meta.url));
  const args = ["bun", enrichPath, "--registry", options.registry];
  for (const source of sources) args.push(source.adapter.enrichArg, source.path);

  const proc = Bun.spawn(args, {
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, SWITCHBACK_REGISTRY_FETCHED_AT: fetchedAt },
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);

  if (exitCode !== 0) throw new Error(`enrichment failed: ${stderr || stdout}`);
  return JSON.parse(stdout);
}

function modelKey(row: Json): string {
  return `${row.provider_id}/${row.model_id}`;
}

function byModel(registry: Json): Map<string, Json> {
  return new Map((registry.models || []).map((row: Json) => [modelKey(row), row]));
}

function getPath(obj: Json, path: string): unknown {
  return path.split(".").reduce((value: any, part) => value?.[part], obj);
}

function normalizeForCompare(value: unknown): unknown {
  if (Array.isArray(value)) return value.map((item) => normalizeForCompare(item));
  if (value && typeof value === "object") {
    const out: Json = {};
    for (const key of Object.keys(value as Json).sort()) {
      if (VOLATILE_KEYS.has(key)) continue;
      out[key] = normalizeForCompare((value as Json)[key]);
    }
    return out;
  }
  return value ?? null;
}

function stable(value: unknown): string {
  return JSON.stringify(normalizeForCompare(value));
}

function providerCatalogChanges(before: Json, after: Json, sources: SourcePayload[]): string[] {
  const keys = [...new Set(sources.flatMap((source) => source.adapter.providerCatalogKeys))].sort();
  return keys.filter((key) => stable(before.provider_catalogs?.[key]) !== stable(after.provider_catalogs?.[key]));
}

function diffModels(before: Json, after: Json, sources: SourcePayload[]): Drift {
  const beforeMap = byModel(before);
  const afterMap = byModel(after);
  const added = [...afterMap.keys()].filter((key) => !beforeMap.has(key)).sort();
  const removed = [...beforeMap.keys()].filter((key) => !afterMap.has(key)).sort();
  const changed: ModelChange[] = [];

  for (const key of [...afterMap.keys()].sort()) {
    const oldRow = beforeMap.get(key);
    const newRow = afterMap.get(key);
    if (!oldRow || !newRow) continue;

    const fields: string[] = [];
    const categories: string[] = [];
    for (const [category, paths] of Object.entries(FIELD_GROUPS)) {
      const changedPaths = paths.filter((path) => stable(getPath(oldRow, path)) !== stable(getPath(newRow, path)));
      if (changedPaths.length > 0) {
        categories.push(category);
        fields.push(...changedPaths);
      }
    }

    if (categories.length > 0) {
      const stale = Boolean(
        oldRow.verification?.probed &&
          categories.some((category) => STALE_PROBE_CATEGORIES.has(category)),
      );
      changed.push({ key, categories, fields, stale_probe: stale });
    }
  }

  const staleProbeRows = changed.filter((row) => row.stale_probe).map((row) => row.key);
  const catalogChanges = providerCatalogChanges(before, after, sources);

  return {
    added_models: added,
    removed_models: removed,
    changed_models: changed,
    provider_catalog_changes: catalogChanges,
    stale_probe_rows: staleProbeRows,
    summary: {
      added_models: added.length,
      removed_models: removed.length,
      changed_models: changed.length,
      provider_catalog_changes: catalogChanges.length,
      stale_probe_rows: staleProbeRows.length,
      before_models: beforeMap.size,
      after_models: afterMap.size,
    },
  };
}

function hasDrift(drift: Drift): boolean {
  return (
    drift.summary.added_models > 0 ||
    drift.summary.removed_models > 0 ||
    drift.summary.changed_models > 0 ||
    drift.summary.provider_catalog_changes > 0
  );
}

function printDrift(drift: Drift, options: Options, sources: SourcePayload[]) {
  console.log("registry refresh drift");
  console.log(`sources: ${sources.map((source) => `${source.adapter.id}${source.fetched ? ":fetched" : ":cached"}`).join(", ")}`);
  console.log(
    `models: +${drift.summary.added_models} -${drift.summary.removed_models} changed=${drift.summary.changed_models} stale_probes=${drift.summary.stale_probe_rows}`,
  );
  if (drift.provider_catalog_changes.length > 0) console.log(`catalogs: ${drift.provider_catalog_changes.join(", ")}`);

  const printList = (label: string, rows: string[]) => {
    if (rows.length === 0) return;
    console.log(`${label}:`);
    for (const row of rows.slice(0, options.limit)) console.log(`  ${row}`);
    if (rows.length > options.limit) console.log(`  ... ${rows.length - options.limit} more`);
  };

  printList("added", drift.added_models);
  printList("removed", drift.removed_models);
  if (drift.changed_models.length > 0) {
    console.log("changed:");
    for (const row of drift.changed_models.slice(0, options.limit)) {
      console.log(`  ${row.key} [${row.categories.join(",")}]${row.stale_probe ? " stale_probe" : ""}`);
    }
    if (drift.changed_models.length > options.limit) {
      console.log(`  ... ${drift.changed_models.length - options.limit} more`);
    }
  }
}

async function writeRunReceipt(options: Options, receipt: Json): Promise<string> {
  await mkdir(options.receiptDir, { recursive: true });
  const stamp = receipt.started_at.replace(/[:.]/g, "-");
  const path = join(options.receiptDir, `${stamp}-${receipt.run_id}.json`);
  receipt.receipt_path = path;
  await writeFile(path, JSON.stringify(receipt, null, 2) + "\n");
  return path;
}

async function main() {
  const startedAt = new Date().toISOString();
  const runId = randomUUID();
  const options = parseArgs(process.argv.slice(2));
  const before = await readJson(options.registry);
  const sources = await loadSources(options);
  const after = await buildCandidateRegistry(options, sources, startedAt);
  const drift = diffModels(before, after, sources);

  const receipt: Json = {
    run_id: runId,
    started_at: startedAt,
    finished_at: new Date().toISOString(),
    registry: options.registry,
    out: options.out,
    applied: options.apply,
    sources: sources.map((source) => ({
      id: source.adapter.id,
      name: source.adapter.name,
      url: source.adapter.url,
      auth_env: source.adapter.authEnv ?? null,
      fetched: source.fetched,
      path: source.path,
      provider_fields: source.adapter.providerFields,
      stats: source.stats,
    })),
    drift,
    command: process.argv.slice(2),
    receipt_path: null,
  };

  if (options.apply) {
    await mkdir(dirname(options.out), { recursive: true });
    await writeFile(options.out, JSON.stringify(after, null, 2) + "\n");
  }

  let receiptPath: string | null = null;
  if (options.writeReceipt) receiptPath = await writeRunReceipt(options, receipt);

  if (options.json) {
    console.log(JSON.stringify(receipt, null, 2));
  } else {
    printDrift(drift, options, sources);
    if (options.apply) console.log(`wrote ${options.out}`);
    if (receiptPath) console.log(`receipt: ${receiptPath}`);
  }

  if (options.failOnDrift && hasDrift(drift)) process.exit(1);
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
