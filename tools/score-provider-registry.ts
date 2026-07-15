#!/usr/bin/env bun
import { readFile } from "node:fs/promises";

type Json = Record<string, any>;

const DEFAULT_REGISTRY = "config/provider-registry.json";

type Options = {
  registry: string;
  jobClass: string;
  filter?: string;
  limit: number;
  json: boolean;
  requireProbed: boolean;
  includeRetired: boolean;
};

type ScoreRow = {
  rank: number;
  score: number;
  offering_id: string;
  provider_id: string;
  model_id: string;
  price: string;
  context_window: number | null;
  freshness: Json;
  observed: Json;
  declared: Json;
  probe_status: string;
  reasons: string[];
  penalties: string[];
};

const JOB_ALIASES: Record<string, string> = {
  extract: "extract",
  classify: "extract",
  "classify-extract": "extract",
  "long-context": "long_context",
  long_context: "long_context",
  context: "long_context",
  judge: "judge",
  verifier: "judge",
  certify: "judge",
  "tool-agent": "tool_agent",
  tool_agent: "tool_agent",
  agent: "tool_agent",
  vision: "vision_review",
  "vision-review": "vision_review",
  vision_review: "vision_review",
  code: "code_patch",
  coding: "code_patch",
  "code-patch": "code_patch",
  code_patch: "code_patch",
  deterministic: "deterministic_eval",
  "deterministic-eval": "deterministic_eval",
  deterministic_eval: "deterministic_eval",
  tripwire: "cheap_tripwire",
  "cheap-tripwire": "cheap_tripwire",
  cheap_tripwire: "cheap_tripwire",
};

const JOBS = [
  "extract",
  "long_context",
  "judge",
  "tool_agent",
  "vision_review",
  "code_patch",
  "deterministic_eval",
  "cheap_tripwire",
];

function usage(code = 2): never {
  console.log(`usage:
  bun tools/score-provider-registry.ts JOB_CLASS [filter]
  sb registry score JOB_CLASS [filter]

Job classes:
  ${JOBS.join(", ")}

Options:
  --registry FILE       registry path, default config/provider-registry.json
  --limit N             rows to print, default 20
  --json                emit JSON
  --require-probed      exclude rows without local probe receipts
  --include-retired     include rows with effective_to in the past
`);
  process.exit(code);
}

function parseArgs(argv: string[]): Options {
  const options: Options = {
    registry: DEFAULT_REGISTRY,
    jobClass: "",
    limit: 20,
    json: false,
    requireProbed: false,
    includeRetired: false,
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
    else if (arg === "--limit") options.limit = Number.parseInt(next(), 10);
    else if (arg.startsWith("--limit=")) options.limit = Number.parseInt(arg.slice("--limit=".length), 10);
    else if (arg === "--json") options.json = true;
    else if (arg === "--require-probed") options.requireProbed = true;
    else if (arg === "--include-retired") options.includeRetired = true;
    else if (!arg.startsWith("-") && !options.jobClass) options.jobClass = normalizeJob(arg);
    else if (!arg.startsWith("-") && !options.filter) options.filter = arg.toLowerCase();
    else throw new Error(`unknown argument: ${arg}`);
  }

  if (!options.jobClass) usage();
  if (!JOBS.includes(options.jobClass)) {
    throw new Error(`unknown job class: ${options.jobClass}. Use one of: ${JOBS.join(", ")}`);
  }
  if (!Number.isFinite(options.limit) || options.limit < 1) throw new Error("--limit must be >= 1");
  return options;
}

function normalizeJob(raw: string): string {
  return JOB_ALIASES[raw.toLowerCase()] || raw.toLowerCase();
}

async function readJson(path: string): Promise<Json> {
  return JSON.parse(await readFile(path, "utf8"));
}

function offeringId(row: Json): string {
  return row.offering_id || `${row.provider_id}/${row.model_id}`;
}

function isFree(row: Json): boolean {
  return row.input_micros_per_mtok === 0 && row.output_micros_per_mtok === 0;
}

function priceText(row: Json): string {
  const input = row.input_micros_per_mtok;
  const output = row.output_micros_per_mtok;
  const money = (v: unknown) => {
    if (v == null) return "?";
    if (v === 0) return "$0";
    return `$${Number(v) / 1_000_000}`;
  };
  return `${money(input)}/${money(output)}`;
}

function priceScore(row: Json): number {
  const input = row.input_micros_per_mtok;
  const output = row.output_micros_per_mtok;
  if (input === 0 && output === 0) return 1;
  if (input == null || output == null) return 0.2;
  const usd = (Number(input) + Number(output)) / 2_000_000;
  if (usd <= 0.25) return 0.9;
  if (usd <= 1) return 0.75;
  if (usd <= 3) return 0.55;
  if (usd <= 10) return 0.3;
  return 0.1;
}

function contextWindow(row: Json): number | null {
  return row.limits?.provider_context_window || row.context_window || null;
}

function contextScore(row: Json, target: number): number {
  const ctx = contextWindow(row);
  if (!ctx) return 0;
  return Math.max(0, Math.min(1, ctx / target));
}

function caps(row: Json): Json {
  return row.capabilities || {};
}

function observed(row: Json): Json {
  return row.verification?.observed_capabilities || {};
}

function hasObserved(row: Json, key: string): boolean | undefined {
  const value = observed(row)[key];
  return typeof value === "boolean" ? value : undefined;
}

function capBool(row: Json, key: string, fallback?: boolean): boolean {
  const seen = hasObserved(row, key);
  if (seen !== undefined) return seen;
  if (key === "text_output") return Boolean(caps(row).text_output ?? (caps(row).output_modalities || []).includes("text") ?? true);
  if (key === "streaming") return Boolean(caps(row).streaming ?? true);
  if (key === "tool_calling") return Boolean(caps(row).tool_calling ?? row.tool_calling);
  if (key === "image_input") return Boolean(caps(row).image_input ?? row.vision ?? (caps(row).input_modalities || []).includes("image"));
  if (key === "seed_supported") return Boolean(caps(row).seed ?? row.determinism?.seed_supported);
  return Boolean(caps(row)[key] ?? fallback);
}

function jsonSchemaScore(row: Json): number {
  const seen = observed(row).json_schema;
  if (seen === "native") return 1;
  if (seen === false) return 0;
  const declared = String(caps(row).json_schema || row.json_schema || "none");
  if (declared === "native") return 0.8;
  if (declared === "response_format") return 0.5;
  return 0;
}

function reasoningScore(row: Json): number {
  if (caps(row).reasoning) return 1;
  const tier = String(row.tier || "");
  if (/\bR\b/.test(tier) && !/\bF\b/.test(tier)) return 1;
  if (/\bR\b/.test(tier)) return 0.75;
  if (/reason/i.test(row.model_id || "")) return 0.7;
  return 0;
}

function benchmarkScore(row: Json, kind: "code" | "judge" | "agentic"): number {
  const benches = row.benchmarks || {};
  const aa = benches.openrouter?.values?.artificial_analysis || {};
  if (kind === "code" && Number.isFinite(aa.coding_index)) return clamp01(aa.coding_index / 60);
  if (kind === "agentic" && Number.isFinite(aa.agentic_index)) return clamp01(aa.agentic_index / 40);
  if (kind === "judge" && Number.isFinite(aa.intelligence_index)) return clamp01(aa.intelligence_index / 60);

  const vendor = benches.vendor?.values || {};
  const values = Object.entries(vendor)
    .filter(([, value]) => Number.isFinite(value))
    .map(([, value]) => Number(value));
  if (values.length === 0) return 0;
  return clamp01(Math.max(...values) / 100);
}

function probeHealth(row: Json): number {
  const verification = row.verification || {};
  if (!verification.probed) return 0.35;
  const probes = verification.probes || {};
  const latest = Object.values(probes)
    .map((slot: any) => slot?.latest)
    .filter(Boolean);
  if (latest.length === 0) return 0.35;
  const passed = latest.filter((receipt: any) => receipt.status === "pass").length;
  return clamp01(passed / latest.length);
}

function probeStatus(row: Json): string {
  const verification = row.verification || {};
  if (!verification.probed) return "declared";
  const latest = Object.entries(verification.probes || {})
    .map(([capability, slot]: [string, any]) => `${capability}:${slot?.latest?.status || "?"}`);
  return latest.join(",") || "probed";
}

type Freshness = { at: string | null; age_days: number | null; source: string; state: string };

function timestampCandidate(at: unknown, source: string): { at: string; source: string; time: number } | null {
  if (typeof at !== "string" || at.length === 0) return null;
  const time = Date.parse(at);
  if (!Number.isFinite(time)) return null;
  return { at, source, time };
}

function freshnessEvidence(row: Json): Freshness {
  const candidates = [
    timestampCandidate(row.verification?.last_probe_at, "probe"),
    timestampCandidate(row.verification?.catalog_seen?.catalog_seen_at, "catalog"),
    timestampCandidate(row.verification?.catalog_seen_at, "catalog"),
    ...(Array.isArray(row.provenance)
      ? row.provenance.map((item: Json) => timestampCandidate(item?.fetched_at, "provenance"))
      : []),
  ].filter(Boolean) as { at: string; source: string; time: number }[];

  if (candidates.length === 0) return { at: null, age_days: null, source: "none", state: "missing" };
  const best = candidates.sort((a, b) => b.time - a.time)[0];
  const ageDays = (Date.now() - best.time) / 86_400_000;
  if (!Number.isFinite(ageDays) || ageDays < 0) {
    return { at: best.at, age_days: null, source: best.source, state: "invalid" };
  }
  const rounded = Math.round(ageDays * 10) / 10;
  const state = ageDays <= 7 ? "fresh" : ageDays <= 30 ? "aging" : "stale";
  return { at: best.at, age_days: rounded, source: best.source, state };
}

function stalenessPenalty(freshness: Freshness): number {
  if (freshness.state === "missing") return 0.08;
  if (freshness.state === "invalid") return 0.02;
  if (freshness.state === "fresh") return 0;
  if (freshness.state === "aging") return 0.04;
  return 0.1;
}

function active(row: Json, includeRetired: boolean): boolean {
  if (includeRetired) return true;
  return !(row.effective_to && row.effective_to <= new Date().toISOString());
}

function searchable(row: Json): string {
  return [
    offeringId(row),
    row.model_id,
    row.display_name,
    row.provider_id,
    ...(row.flags || []),
    ...(caps(row).input_modalities || []),
    ...(caps(row).output_modalities || []),
    ...(caps(row).supported_parameters || []),
    row.architecture?.architecture_type,
  ]
    .filter(Boolean)
    .join(" ")
    .toLowerCase();
}

function score(row: Json, jobClass: string): Omit<ScoreRow, "rank"> {
  const reasons: string[] = [];
  const penalties: string[] = [];
  const add = (label: string, value: number, weight: number): number => {
    const points = clamp01(value) * weight;
    if (points >= weight * 0.65) reasons.push(label);
    return points;
  };
  let raw = 0;

  if (jobClass === "extract") {
    raw += add("cheap", priceScore(row), 28);
    raw += add("text", capBool(row, "text_output") ? 1 : 0, 22);
    raw += add("stream", capBool(row, "streaming") ? 1 : 0, 10);
    raw += add("json", jsonSchemaScore(row), 12);
    raw += add("probe", probeHealth(row), 16);
    raw += add("context", contextScore(row, 32_000), 12);
  } else if (jobClass === "long_context") {
    raw += add("context", contextScore(row, 1_000_000), 35);
    raw += add("text", capBool(row, "text_output") ? 1 : 0, 15);
    raw += add("stream", capBool(row, "streaming") ? 1 : 0, 10);
    raw += add("tools", capBool(row, "tool_calling") ? 1 : 0, 10);
    raw += add("cheap", priceScore(row), 10);
    raw += add("probe", probeHealth(row), 10);
    raw += add("reasoning", reasoningScore(row), 10);
  } else if (jobClass === "judge") {
    raw += add("benchmark", Math.max(benchmarkScore(row, "judge"), benchmarkScore(row, "agentic")), 24);
    raw += add("reasoning", reasoningScore(row), 18);
    raw += add("tools", capBool(row, "tool_calling") ? 1 : 0, 10);
    raw += add("json", jsonSchemaScore(row), 10);
    raw += add("probe", probeHealth(row), 12);
    raw += add("context", contextScore(row, 1_000_000), 14);
    raw += add("cost", priceScore(row), 6);
    raw += add("not-free", isFree(row) ? 0.2 : 1, 12);
    if (isFree(row)) {
      raw = Math.min(raw, 52);
      penalties.push("free_not_certifier");
    }
  } else if (jobClass === "tool_agent") {
    raw += add("tools", capBool(row, "tool_calling") ? 1 : 0, 28);
    raw += add("json", jsonSchemaScore(row), 18);
    raw += add("stream", capBool(row, "streaming") ? 1 : 0, 10);
    raw += add("agentic", benchmarkScore(row, "agentic"), 12);
    raw += add("context", contextScore(row, 128_000), 12);
    raw += add("probe", probeHealth(row), 14);
    raw += add("cheap", priceScore(row), 6);
  } else if (jobClass === "vision_review") {
    raw += add("vision", capBool(row, "image_input") ? 1 : 0, 35);
    raw += add("text", capBool(row, "text_output") ? 1 : 0, 15);
    raw += add("json", jsonSchemaScore(row), 10);
    raw += add("context", contextScore(row, 64_000), 10);
    raw += add("probe", probeHealth(row), 20);
    raw += add("cheap", priceScore(row), 10);
  } else if (jobClass === "code_patch") {
    raw += add("coding", benchmarkScore(row, "code"), 28);
    raw += add("tools", capBool(row, "tool_calling") ? 1 : 0, 16);
    raw += add("context", contextScore(row, 128_000), 16);
    raw += add("json", jsonSchemaScore(row), 8);
    raw += add("probe", probeHealth(row), 14);
    raw += add("cheap", priceScore(row), 10);
    raw += add("reasoning", reasoningScore(row), 8);
  } else if (jobClass === "deterministic_eval") {
    raw += add("seed", capBool(row, "seed_supported") ? 1 : 0, 35);
    raw += add("probe", probeHealth(row), 20);
    raw += add("json", jsonSchemaScore(row), 15);
    raw += add("not-free", isFree(row) ? 0.3 : 1, 15);
    raw += add("cheap", priceScore(row), 10);
    raw += add("context", contextScore(row, 32_000), 5);
    if (isFree(row)) penalties.push("free_determinism_weaker");
  } else if (jobClass === "cheap_tripwire") {
    raw += add("free", isFree(row) ? 1 : priceScore(row), 35);
    raw += add("text", capBool(row, "text_output") ? 1 : 0, 20);
    raw += add("stream", capBool(row, "streaming") ? 1 : 0, 10);
    raw += add("context", contextScore(row, 32_000), 10);
    raw += add("probe", probeHealth(row), 15);
    raw += add("tools", capBool(row, "tool_calling") ? 1 : 0, 10);
  }

  const freshness = freshnessEvidence(row);
  const stale = stalenessPenalty(freshness);
  if (freshness.state === "missing") penalties.push("evidence_missing");
  else if (stale > 0) penalties.push(`${freshness.source}_stale`);
  if (stale > 0 && (row.input_micros_per_mtok != null || row.output_micros_per_mtok != null)) {
    penalties.push("price_evidence_stale");
  }
  if (hasObserved(row, "text_output") === false && jobClass !== "cheap_tripwire") penalties.push("observed_text_fail");
  if (hasObserved(row, "streaming") === false) penalties.push("observed_stream_fail");
  const scoreValue = Math.max(0, Math.round((raw - stale * 100) * 10) / 10);

  return {
    score: scoreValue,
    offering_id: offeringId(row),
    provider_id: row.provider_id,
    model_id: row.model_id,
    price: priceText(row),
    context_window: contextWindow(row),
    freshness,
    observed: observed(row),
    declared: {
      text_output: capBool(row, "text_output"),
      streaming: capBool(row, "streaming"),
      tool_calling: capBool(row, "tool_calling"),
      json_schema: caps(row).json_schema || row.json_schema || "none",
      image_input: capBool(row, "image_input"),
      seed_supported: capBool(row, "seed_supported"),
      reasoning: reasoningScore(row) > 0,
    },
    probe_status: probeStatus(row),
    reasons: unique(reasons),
    penalties: unique(penalties),
  };
}

function unique<T>(items: T[]): T[] {
  return [...new Set(items)];
}

function clamp01(value: number): number {
  if (!Number.isFinite(value)) return 0;
  return Math.max(0, Math.min(1, value));
}

function short(text: string, len: number): string {
  return text.length <= len ? text : `${text.slice(0, Math.max(0, len - 3))}...`;
}

function freshnessText(freshness: Json): string {
  if (!freshness?.at) return "missing";
  const age = freshness.age_days == null ? "?" : `${freshness.age_days}d`;
  return `${freshness.state}:${freshness.source}:${age}`;
}

function printTable(jobClass: string, rows: ScoreRow[]) {
  console.log(`job_class: ${jobClass}`);
  console.log("rank score offering price ctx fresh probe reasons penalties");
  for (const row of rows) {
    console.log(
      `${String(row.rank).padStart(2)} ` +
        `${String(row.score).padStart(5)} ` +
        `${short(row.offering_id, 48).padEnd(48)} ` +
        `${short(row.price, 15).padEnd(15)} ` +
        `${String(row.context_window || "?").padEnd(8)} ` +
        `${short(freshnessText(row.freshness), 22).padEnd(22)} ` +
        `${short(row.probe_status || "-", 22).padEnd(22)} ` +
        `${short(row.reasons.join(","), 42).padEnd(42)} ` +
      `${short(row.penalties.join(","), 36)}`,
    );
  }
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const registry = await readJson(options.registry);
  const rows = (registry.models || [])
    // This scorer's job classes are text/vision-input request classes. Media
    // generation rows have different billing/capability semantics and must not
    // be ranked as if unit prices were token prices.
    .filter((row: Json) => (row.pricing_unit || "token_metered") === "token_metered")
    .filter((row: Json) => active(row, options.includeRetired))
    .filter((row: Json) => !options.requireProbed || row.verification?.probed)
    .filter((row: Json) => !options.filter || searchable(row).includes(options.filter!))
    .map((row: Json) => score(row, options.jobClass))
    .sort((a: ScoreRow, b: ScoreRow) => b.score - a.score || a.offering_id.localeCompare(b.offering_id))
    .slice(0, options.limit)
    .map((row: Omit<ScoreRow, "rank">, index: number) => ({ ...row, rank: index + 1 }));

  if (options.json) {
    console.log(JSON.stringify({ registry: options.registry, job_class: options.jobClass, rows }, null, 2));
  } else {
    printTable(options.jobClass, rows);
  }
}

main().catch((error) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exit(1);
});
