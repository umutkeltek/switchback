#!/usr/bin/env bun
import { createHash, randomUUID } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";

type Json = Record<string, any>;

const DEFAULT_REGISTRY = "config/provider-registry.json";
const DEFAULT_GATEWAY = process.env.SB_GATEWAY || "http://127.0.0.1:18765";
const DEFAULT_TIMEOUT_MS = 30_000;
const MAX_HISTORY = 9;

type Capability = "completion" | "stream" | "tools" | "json_schema" | "vision" | "seed" | "headers";

type Options = {
  registry: string;
  out: string;
  gateway: string;
  apiKey?: string;
  models: string[];
  filter?: string;
  route?: string;
  capabilities: Capability[];
  limit?: number;
  apply: boolean;
  dryRun: boolean;
  allowFailures: boolean;
  timeoutMs: number;
};

type ProbeResult = {
  capability: Capability;
  receipt: Json;
  pass: boolean;
};

function usage(code = 2): never {
  console.log(`usage:
  bun tools/probe-provider-registry.ts --model PROVIDER/MODEL --apply
  bun tools/probe-provider-registry.ts --filter nvidia --capability completion --capability stream --apply
  bun tools/probe-provider-registry.ts --model nvidia/minimaxai/minimax-m3 --all --apply
  sb registry probe --model nvidia/minimaxai/minimax-m3 --all --apply

Options:
  --registry FILE       registry path, default config/provider-registry.json
  --out FILE            output registry path, default same as --registry
  --gateway URL         Switchback gateway, default SB_GATEWAY or http://127.0.0.1:18765
  --api-key-env NAME    read Switchback API key from env NAME, default SB_API_KEY/SWITCHBACK_API_KEY
  --model ID           registry row key, usually provider/model; repeatable
  --filter TEXT         select rows whose provider/model/display/capability text contains TEXT
  --route MODEL         override request model route; only valid with one selected row
  --capability NAME     completion|stream|tools|json_schema|vision|seed|headers; repeatable
  --all                 run declared probe set: completion+stream plus declared tools/json/vision/seed
  --limit N             cap selected rows
  --timeout-ms N        per request timeout, default ${DEFAULT_TIMEOUT_MS}
  --dry-run             print selected rows and planned probes; no network
  --apply               write probe receipts into registry
  --allow-failures      exit 0 even when a probe writes a failing receipt
`);
  process.exit(code);
}

function parseArgs(argv: string[]): Options {
  const options: Options = {
    registry: DEFAULT_REGISTRY,
    out: DEFAULT_REGISTRY,
    gateway: DEFAULT_GATEWAY,
    apiKey: process.env.SB_API_KEY || process.env.SWITCHBACK_API_KEY,
    models: [],
    capabilities: [],
    apply: false,
    dryRun: false,
    allowFailures: false,
    timeoutMs: DEFAULT_TIMEOUT_MS,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      const value = argv[++i];
      if (!value) throw new Error(`${arg} requires a value`);
      return value;
    };

    if (arg === "--help" || arg === "-h") usage(0);
    if (arg === "--registry") options.registry = next();
    else if (arg.startsWith("--registry=")) options.registry = arg.slice("--registry=".length);
    else if (arg === "--out") options.out = next();
    else if (arg.startsWith("--out=")) options.out = arg.slice("--out=".length);
    else if (arg === "--gateway") options.gateway = next();
    else if (arg.startsWith("--gateway=")) options.gateway = arg.slice("--gateway=".length);
    else if (arg === "--api-key-env") {
      const envName = next();
      options.apiKey = process.env[envName];
    } else if (arg === "--model") options.models.push(next());
    else if (arg.startsWith("--model=")) options.models.push(arg.slice("--model=".length));
    else if (arg === "--filter") options.filter = next().toLowerCase();
    else if (arg.startsWith("--filter=")) options.filter = arg.slice("--filter=".length).toLowerCase();
    else if (arg === "--route") options.route = next();
    else if (arg.startsWith("--route=")) options.route = arg.slice("--route=".length);
    else if (arg === "--capability") options.capabilities.push(...parseCapabilities(next()));
    else if (arg.startsWith("--capability=")) options.capabilities.push(...parseCapabilities(arg.slice("--capability=".length)));
    else if (arg === "--all" || arg === "--declared") options.capabilities = [];
    else if (arg === "--limit") options.limit = Number.parseInt(next(), 10);
    else if (arg.startsWith("--limit=")) options.limit = Number.parseInt(arg.slice("--limit=".length), 10);
    else if (arg === "--timeout-ms") options.timeoutMs = Number.parseInt(next(), 10);
    else if (arg.startsWith("--timeout-ms=")) options.timeoutMs = Number.parseInt(arg.slice("--timeout-ms=".length), 10);
    else if (arg === "--apply") options.apply = true;
    else if (arg === "--dry-run") options.dryRun = true;
    else if (arg === "--allow-failures") options.allowFailures = true;
    else if (!arg.startsWith("-") && !options.filter) options.filter = arg.toLowerCase();
    else throw new Error(`unknown argument: ${arg}`);
  }

  options.out = options.out === DEFAULT_REGISTRY ? options.registry : options.out;
  if (!Number.isFinite(options.timeoutMs) || options.timeoutMs < 1000) {
    throw new Error("--timeout-ms must be >= 1000");
  }
  if (options.limit != null && (!Number.isFinite(options.limit) || options.limit < 1)) {
    throw new Error("--limit must be >= 1");
  }
  return options;
}

function parseCapabilities(raw: string): Capability[] {
  return raw
    .split(",")
    .map((x) => x.trim())
    .filter(Boolean)
    .flatMap((x) => {
      if (x === "all" || x === "declared") return [];
      if (["completion", "stream", "tools", "json_schema", "vision", "seed", "headers"].includes(x)) {
        return [x as Capability];
      }
      throw new Error(`unknown capability: ${x}`);
    });
}

async function readJson(path: string): Promise<Json> {
  return JSON.parse(await readFile(path, "utf8"));
}

function rowKey(row: Json): string {
  return `${row.provider_id}/${row.model_id}`;
}

function rowSearchText(row: Json): string {
  const caps = row.capabilities || {};
  const arch = row.architecture || {};
  return [
    rowKey(row),
    row.model_id,
    row.display_name,
    ...(row.flags || []),
    ...(caps.input_modalities || []),
    ...(caps.output_modalities || []),
    ...(caps.supported_parameters || []),
    arch.architecture_type,
    arch.attention,
    arch.context_method,
  ]
    .filter(Boolean)
    .join(" ")
    .toLowerCase();
}

function selectRows(registry: Json, options: Options): Json[] {
  const models = registry.models || [];
  const selected = models.filter((row: Json) => {
    const key = rowKey(row);
    if (options.models.length > 0 && !options.models.includes(key) && !options.models.includes(row.model_id)) {
      return false;
    }
    if (options.filter && !rowSearchText(row).includes(options.filter)) return false;
    return true;
  });
  return options.limit ? selected.slice(0, options.limit) : selected;
}

function declaredCapabilities(row: Json, explicit: Capability[]): Capability[] {
  if (explicit.length > 0) return unique(explicit);

  const caps = row.capabilities || {};
  const determinism = row.determinism || {};
  const planned: Capability[] = ["completion", "stream", "headers"];
  if (caps.tool_calling || row.tool_calling) planned.push("tools");
  if ((caps.json_schema || row.json_schema) && !["none", "unknown"].includes(String(caps.json_schema || row.json_schema))) {
    planned.push("json_schema");
  }
  if (caps.image_input || row.vision || (caps.input_modalities || []).includes("image")) planned.push("vision");
  if (caps.seed || determinism.seed_supported) planned.push("seed");
  return unique(planned);
}

function unique<T>(items: T[]): T[] {
  return [...new Set(items)];
}

function targetModel(row: Json, options: Options): string {
  return options.route || rowKey(row);
}

function safeHeaders(headers: Headers): Json {
  const out: Json = {};
  for (const [key, value] of headers.entries()) {
    const k = key.toLowerCase();
    if (
      k.startsWith("x-switchback-") ||
      k.startsWith("x-ratelimit-") ||
      k === "retry-after" ||
      k === "openrouter-processing-ms"
    ) {
      out[k] = value;
    }
  }
  return out;
}

function redact(text: unknown): string {
  return String(text ?? "")
    .replace(/Bearer\s+[A-Za-z0-9._~+/=-]+/gi, "Bearer [REDACTED]")
    .replace(/\b(sk|sb|or|nvapi)-[A-Za-z0-9._-]{12,}\b/g, "$1-[REDACTED]")
    .slice(0, 400);
}

function contentHash(text: string): string {
  return createHash("sha256").update(text).digest("hex").slice(0, 16);
}

async function fetchWithTimeout(url: string, init: RequestInit, timeoutMs: number): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    return await fetch(url, { ...init, signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

function baseHeaders(options: Options): HeadersInit {
  return {
    "content-type": "application/json",
    ...(options.apiKey ? { authorization: `Bearer ${options.apiKey}` } : {}),
  };
}

function baseBody(model: string): Json {
  return {
    model,
    messages: [{ role: "user", content: "Switchback probe. Reply with exactly: SB_PROBE_OK" }],
    max_tokens: 24,
    temperature: 0,
  };
}

function makeReceiptBase(capability: Capability, row: Json, routeModel: string, startedAt: string): Json {
  return {
    probe_id: randomUUID(),
    capability,
    status: "fail",
    started_at: startedAt,
    finished_at: null,
    registry_row: rowKey(row),
    request_model: routeModel,
    gateway: "switchback",
  };
}

async function requestJson(options: Options, body: Json, receipt: Json): Promise<{ response?: Response; json?: Json; text?: string }> {
  const started = performance.now();
  try {
    const response = await fetchWithTimeout(`${options.gateway.replace(/\/$/, "")}/v1/chat/completions`, {
      method: "POST",
      headers: baseHeaders(options),
      body: JSON.stringify(body),
    }, options.timeoutMs);
    const text = await response.text();
    const elapsed = Math.round(performance.now() - started);
    receipt.elapsed_ms = elapsed;
    receipt.http_status = response.status;
    receipt.headers = safeHeaders(response.headers);
    receipt.switchback_route = response.headers.get("x-switchback-route") || undefined;
    let json: Json | undefined;
    try {
      json = text ? JSON.parse(text) : undefined;
    } catch {
      receipt.error_class = "non_json_response";
      receipt.error_message = redact(text);
    }
    return { response, json, text };
  } catch (error) {
    receipt.elapsed_ms = Math.round(performance.now() - started);
    receipt.error_class = error instanceof Error && error.name === "AbortError" ? "timeout" : "fetch_error";
    receipt.error_message = redact(error instanceof Error ? error.message : String(error));
    return {};
  }
}

function finishReceipt(receipt: Json, startedAt: string, pass: boolean, observed: Json = {}) {
  receipt.finished_at = new Date().toISOString();
  receipt.status = pass ? "pass" : "fail";
  receipt.observed = observed;
  receipt.duration_ms = Date.parse(receipt.finished_at) - Date.parse(startedAt);
}

function extractMessage(json: Json | undefined): Json {
  return json?.choices?.[0]?.message || {};
}

function contentFrom(json: Json | undefined): string {
  const content = extractMessage(json).content;
  return typeof content === "string" ? content : "";
}

function attachResponseMetadata(receipt: Json, json: Json | undefined) {
  if (!json) return;
  receipt.response_id = json.id;
  receipt.response_model = json.model;
  if (Array.isArray(json.choices)) {
    receipt.choice_count = json.choices.length;
    receipt.finish_reason = json.choices[0]?.finish_reason;
  }
  if (json.usage) receipt.usage = json.usage;
  if (json.error) {
    receipt.error_class = json.error.type || json.error.code || "provider_error";
    receipt.error_message = redact(json.error.message || JSON.stringify(json.error));
  }
}

async function probeCompletion(row: Json, options: Options): Promise<ProbeResult> {
  const startedAt = new Date().toISOString();
  const routeModel = targetModel(row, options);
  const receipt = makeReceiptBase("completion", row, routeModel, startedAt);
  const { response, json } = await requestJson(options, baseBody(routeModel), receipt);
  attachResponseMetadata(receipt, json);
  const content = contentFrom(json);
  const pass = Boolean(response?.ok && content.trim().length > 0);
  finishReceipt(receipt, startedAt, pass, {
    text_output: pass,
    content_chars: content.length,
  });
  return { capability: "completion", receipt, pass };
}

async function probeHeaders(row: Json, options: Options): Promise<ProbeResult> {
  const startedAt = new Date().toISOString();
  const routeModel = targetModel(row, options);
  const receipt = makeReceiptBase("headers", row, routeModel, startedAt);
  const { response, json } = await requestJson(options, { ...baseBody(routeModel), max_tokens: 4 }, receipt);
  attachResponseMetadata(receipt, json);
  const headers = receipt.headers || {};
  const hasUsefulHeaders = Object.keys(headers).length > 0;
  const pass = Boolean(response?.ok);
  finishReceipt(receipt, startedAt, pass, {
    metadata_headers_seen: hasUsefulHeaders,
    rate_limit_headers_seen: Object.keys(headers).some((k) => k.startsWith("x-ratelimit-") || k === "retry-after"),
  });
  return { capability: "headers", receipt, pass };
}

async function probeTools(row: Json, options: Options): Promise<ProbeResult> {
  const startedAt = new Date().toISOString();
  const routeModel = targetModel(row, options);
  const receipt = makeReceiptBase("tools", row, routeModel, startedAt);
  const body = {
    ...baseBody(routeModel),
    messages: [{ role: "user", content: "Call the sb_probe_echo tool with ok=true." }],
    tools: [{
      type: "function",
      function: {
        name: "sb_probe_echo",
        description: "Synthetic Switchback capability probe.",
        parameters: {
          type: "object",
          properties: { ok: { type: "boolean" } },
          required: ["ok"],
          additionalProperties: false,
        },
      },
    }],
    tool_choice: { type: "function", function: { name: "sb_probe_echo" } },
  };
  const { response, json } = await requestJson(options, body, receipt);
  attachResponseMetadata(receipt, json);
  const toolCalls = extractMessage(json).tool_calls || [];
  const pass = Boolean(response?.ok && Array.isArray(toolCalls) && toolCalls.length > 0);
  finishReceipt(receipt, startedAt, pass, {
    tool_calling: pass,
    tool_call_count: Array.isArray(toolCalls) ? toolCalls.length : 0,
  });
  return { capability: "tools", receipt, pass };
}

async function probeJsonSchema(row: Json, options: Options): Promise<ProbeResult> {
  const startedAt = new Date().toISOString();
  const routeModel = targetModel(row, options);
  const receipt = makeReceiptBase("json_schema", row, routeModel, startedAt);
  const body = {
    ...baseBody(routeModel),
    messages: [{ role: "user", content: "Return JSON with ok=true and no extra keys." }],
    response_format: {
      type: "json_schema",
      json_schema: {
        name: "switchback_probe",
        strict: true,
        schema: {
          type: "object",
          properties: { ok: { type: "boolean" } },
          required: ["ok"],
          additionalProperties: false,
        },
      },
    },
  };
  const { response, json } = await requestJson(options, body, receipt);
  attachResponseMetadata(receipt, json);
  const content = contentFrom(json);
  let parsed: Json | null = null;
  try {
    parsed = JSON.parse(content);
  } catch {
    parsed = null;
  }
  const pass = Boolean(response?.ok && parsed && parsed.ok === true);
  finishReceipt(receipt, startedAt, pass, {
    json_schema: pass ? "native" : false,
    parseable_json: Boolean(parsed),
    content_chars: content.length,
  });
  return { capability: "json_schema", receipt, pass };
}

async function probeVision(row: Json, options: Options): Promise<ProbeResult> {
  const startedAt = new Date().toISOString();
  const routeModel = targetModel(row, options);
  const receipt = makeReceiptBase("vision", row, routeModel, startedAt);
  const onePixelPng = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=";
  const body = {
    ...baseBody(routeModel),
    messages: [{
      role: "user",
      content: [
        { type: "text", text: "This is a synthetic 1x1 image probe. Reply with exactly: IMAGE_OK" },
        { type: "image_url", image_url: { url: `data:image/png;base64,${onePixelPng}` } },
      ],
    }],
  };
  const { response, json } = await requestJson(options, body, receipt);
  attachResponseMetadata(receipt, json);
  const content = contentFrom(json);
  const pass = Boolean(response?.ok && content.trim().length > 0);
  finishReceipt(receipt, startedAt, pass, {
    image_input: pass,
    text_output: pass,
    content_chars: content.length,
  });
  return { capability: "vision", receipt, pass };
}

async function probeSeed(row: Json, options: Options): Promise<ProbeResult> {
  const startedAt = new Date().toISOString();
  const routeModel = targetModel(row, options);
  const receipt = makeReceiptBase("seed", row, routeModel, startedAt);
  const body = {
    ...baseBody(routeModel),
    messages: [{ role: "user", content: "Choose one token from: ALPHA BETA GAMMA DELTA. Return only the token." }],
    seed: 12345,
    temperature: 0.7,
    max_tokens: 8,
  };
  const first = await requestJson(options, body, receipt);
  const firstContent = contentFrom(first.json);
  const secondReceipt: Json = {};
  const second = await requestJson(options, body, secondReceipt);
  attachResponseMetadata(receipt, first.json);
  receipt.second_http_status = second.response?.status;
  receipt.second_elapsed_ms = secondReceipt.elapsed_ms;
  const secondContent = contentFrom(second.json);
  const same = firstContent.length > 0 && firstContent === secondContent;
  const pass = Boolean(first.response?.ok && second.response?.ok && same);
  finishReceipt(receipt, startedAt, pass, {
    seed_supported: Boolean(first.response?.ok && second.response?.ok),
    deterministic_same_output: same,
    first_hash: firstContent ? contentHash(firstContent) : null,
    second_hash: secondContent ? contentHash(secondContent) : null,
    first_chars: firstContent.length,
    second_chars: secondContent.length,
  });
  return { capability: "seed", receipt, pass };
}

async function probeStream(row: Json, options: Options): Promise<ProbeResult> {
  const startedAt = new Date().toISOString();
  const routeModel = targetModel(row, options);
  const receipt = makeReceiptBase("stream", row, routeModel, startedAt);
  const started = performance.now();
  let eventCount = 0;
  let contentDeltas = 0;
  let toolDeltas = 0;
  let done = false;
  let firstEventMs: number | null = null;

  try {
    const response = await fetchWithTimeout(`${options.gateway.replace(/\/$/, "")}/v1/chat/completions`, {
      method: "POST",
      headers: baseHeaders(options),
      body: JSON.stringify({ ...baseBody(routeModel), stream: true }),
    }, options.timeoutMs);
    receipt.http_status = response.status;
    receipt.headers = safeHeaders(response.headers);
    receipt.switchback_route = response.headers.get("x-switchback-route") || undefined;

    if (!response.body) throw new Error("missing response body");
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    while (true) {
      const { done: readerDone, value } = await reader.read();
      if (readerDone) break;
      buffer += decoder.decode(value, { stream: true });
      const lines = buffer.split(/\r?\n/);
      buffer = lines.pop() || "";
      for (const line of lines) {
        if (!line.startsWith("data:")) continue;
        const data = line.slice("data:".length).trim();
        if (!data) continue;
        if (firstEventMs == null) firstEventMs = Math.round(performance.now() - started);
        eventCount += 1;
        if (data === "[DONE]") {
          done = true;
          continue;
        }
        try {
          const parsed = JSON.parse(data);
          const delta = parsed.choices?.[0]?.delta || {};
          if (typeof delta.content === "string" && delta.content.length > 0) contentDeltas += 1;
          if (Array.isArray(delta.tool_calls) && delta.tool_calls.length > 0) toolDeltas += delta.tool_calls.length;
        } catch {
          receipt.error_class = "stream_parse_error";
        }
      }
    }
    receipt.elapsed_ms = Math.round(performance.now() - started);
  } catch (error) {
    receipt.elapsed_ms = Math.round(performance.now() - started);
    receipt.error_class = error instanceof Error && error.name === "AbortError" ? "timeout" : "fetch_error";
    receipt.error_message = redact(error instanceof Error ? error.message : String(error));
  }

  const pass = Boolean(receipt.http_status >= 200 && receipt.http_status < 300 && eventCount > 0 && (done || contentDeltas > 0 || toolDeltas > 0));
  finishReceipt(receipt, startedAt, pass, {
    streaming: pass,
    event_count: eventCount,
    first_event_ms: firstEventMs,
    done_seen: done,
    content_delta_count: contentDeltas,
    tool_delta_count: toolDeltas,
  });
  return { capability: "stream", receipt, pass };
}

async function runCapability(row: Json, options: Options, capability: Capability): Promise<ProbeResult> {
  if (capability === "completion") return probeCompletion(row, options);
  if (capability === "stream") return probeStream(row, options);
  if (capability === "tools") return probeTools(row, options);
  if (capability === "json_schema") return probeJsonSchema(row, options);
  if (capability === "vision") return probeVision(row, options);
  if (capability === "seed") return probeSeed(row, options);
  return probeHeaders(row, options);
}

function mergeReceipt(row: Json, result: ProbeResult) {
  const verification = row.verification || {};
  const probes = verification.probes || {};
  const existing = probes[result.capability] || {};
  const history = Array.isArray(existing.history) ? existing.history : [];
  const nextHistory = existing.latest ? [...history, existing.latest].slice(-MAX_HISTORY) : history.slice(-MAX_HISTORY);

  probes[result.capability] = {
    latest: result.receipt,
    history: nextHistory,
  };

  const observed = verification.observed_capabilities || {};
  if (result.capability === "completion") observed.text_output = result.pass;
  if (result.capability === "stream") {
    observed.streaming = result.pass;
    if ((result.receipt.observed?.content_delta_count || 0) > 0) observed.text_output = true;
  }
  if (result.capability === "tools") observed.tool_calling = result.pass;
  if (result.capability === "json_schema") observed.json_schema = result.pass ? "native" : false;
  if (result.capability === "vision") observed.image_input = result.pass;
  if (result.capability === "seed") {
    observed.seed_supported = Boolean(result.receipt.observed?.seed_supported);
    observed.seed_deterministic = Boolean(result.receipt.observed?.deterministic_same_output);
  }
  if (result.capability === "headers") {
    observed.rate_limit_headers_seen = Boolean(result.receipt.observed?.rate_limit_headers_seen);
    observed.metadata_headers_seen = Boolean(result.receipt.observed?.metadata_headers_seen);
  }

  row.verification = {
    ...verification,
    probed: true,
    last_probe_at: result.receipt.finished_at,
    last_probe_status: result.receipt.status,
    probes,
    observed_capabilities: observed,
  };
}

function updateCounts(registry: Json) {
  const models = registry.models || [];
  registry.counts = {
    ...(registry.counts || {}),
    probed_models: models.filter((row: Json) => row.verification?.probed).length,
    probe_receipts: models.reduce((sum: number, row: Json) => {
      const probes = row.verification?.probes || {};
      return sum + Object.values(probes).filter((slot: any) => slot?.latest).length;
    }, 0),
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  const registry = await readJson(options.registry);
  const rows = selectRows(registry, options);
  if (rows.length === 0) {
    throw new Error("no registry rows matched; use --model or --filter");
  }
  if (options.route && rows.length !== 1) {
    throw new Error("--route requires exactly one selected registry row");
  }

  const plan = rows.map((row) => ({
    row: rowKey(row),
    request_model: targetModel(row, options),
    capabilities: declaredCapabilities(row, options.capabilities),
  }));

  if (options.dryRun) {
    console.log(JSON.stringify({ registry: options.registry, gateway: options.gateway, plan }, null, 2));
    return;
  }

  let failed = 0;
  for (const item of plan) {
    const row = rows.find((candidate) => rowKey(candidate) === item.row)!;
    for (const capability of item.capabilities) {
      const result = await runCapability(row, options, capability);
      mergeReceipt(row, result);
      const marker = result.pass ? "PASS" : "FAIL";
      console.log(`${marker} ${item.row} ${capability} ${result.receipt.elapsed_ms ?? "?"}ms`);
      if (!result.pass) failed += 1;
    }
  }

  updateCounts(registry);
  if (options.apply) {
    await writeFile(options.out, JSON.stringify(registry, null, 2) + "\n");
    console.log(`wrote ${options.out}`);
  } else {
    console.log("probe receipts not written; rerun with --apply");
  }

  if (failed > 0 && !options.allowFailures) process.exit(1);
}

main().catch((error) => {
  console.error(redact(error instanceof Error ? error.message : String(error)));
  process.exit(1);
});
