#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TOOL="${ROOT:h}/tools/probe-provider-registry.ts"
TMPDIR="$(mktemp -d)"
SERVER_PID=""
trap '[[ -n "$SERVER_PID" ]] && kill "$SERVER_PID" 2>/dev/null || true; rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export SB_PROVIDER_REGISTRY="${TMPDIR}/provider-registry.json"
export PORT_FILE="${TMPDIR}/port"
mkdir -p "$HOME"

cat > "$SB_PROVIDER_REGISTRY" <<'JSON'
{
  "schema": "switchback/provider-registry@2",
  "money": "integer micro-USD per 1M tokens",
  "counts": {"providers": 1, "models": 1},
  "providers": [
    {"id": "test", "name": "Test Provider", "base_url": "http://127.0.0.1", "free_tier": true, "aggregator": false}
  ],
  "models": [
    {
      "provider_id": "test",
      "model_id": "echo",
      "display_name": "Echo",
      "context_window": 8192,
      "vision": true,
      "tool_calling": true,
      "json_schema": "native",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "capabilities": {
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"],
        "tool_calling": true,
        "json_schema": "native",
        "seed": true,
        "image_input": true
      },
      "determinism": {"seed_supported": true},
      "verification": {"declared": true, "probed": false, "probes": {}}
    }
  ]
}
JSON

cat > "${TMPDIR}/server.ts" <<'TS'
const portFile = process.env.PORT_FILE!;
const server = Bun.serve({
  port: 0,
  async fetch(req) {
    if (new URL(req.url).pathname !== "/v1/chat/completions") {
      return Response.json({ error: { message: "not found" } }, { status: 404 });
    }
    const body = await req.json();
    const headers = {
      "x-switchback-route": body.model,
      "x-ratelimit-limit-requests": "40",
      "content-type": body.stream ? "text/event-stream" : "application/json",
    };
    if (body.stream) {
      return new Response(
        `data: {"choices":[{"delta":{"content":"SB"}}]}\n\n` +
        `data: {"choices":[{"delta":{"content":"_PROBE_OK"}}]}\n\n` +
        `data: [DONE]\n\n`,
        { headers },
      );
    }
    let message: any = { role: "assistant", content: "SB_PROBE_OK" };
    if (body.tools) {
      message = {
        role: "assistant",
        content: "",
        tool_calls: [{
          id: "call_probe",
          type: "function",
          function: { name: "sb_probe_echo", arguments: "{\"ok\":true}" },
        }],
      };
    } else if (body.response_format) {
      message = { role: "assistant", content: "{\"ok\":true}" };
    } else if (body.seed) {
      message = { role: "assistant", content: "ALPHA" };
    }
    return Response.json({
      id: "chatcmpl_probe",
      model: body.model,
      choices: [{ index: 0, message, finish_reason: "stop" }],
      usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
    }, { headers });
  },
});
await Bun.write(portFile, String(server.port));
setInterval(() => {}, 1000);
TS

bun "${TMPDIR}/server.ts" >/tmp/sb-registry-probe-test.log 2>&1 &
SERVER_PID="$!"
for _ in {1..50}; do
  [[ -s "$PORT_FILE" ]] && break
  sleep 0.1
done
[[ -s "$PORT_FILE" ]] || { print "server did not start" >&2; exit 1; }
export SB_GATEWAY="http://127.0.0.1:$(cat "$PORT_FILE")"

bun "$TOOL" \
  --registry "$SB_PROVIDER_REGISTRY" \
  --gateway "$SB_GATEWAY" \
  --model test/echo \
  --capability completion,stream,tools,json_schema,vision,seed,headers \
  --apply >/tmp/sb-registry-probe-tool.out

jq -e '
  .counts.probed_models == 1 and
  .counts.probe_receipts == 7 and
  .models[0].verification.probed == true and
  .models[0].verification.probes.completion.latest.status == "pass" and
  .models[0].verification.probes.stream.latest.observed.streaming == true and
  .models[0].verification.probes.tools.latest.observed.tool_calling == true and
  .models[0].verification.probes.json_schema.latest.observed.json_schema == "native" and
  .models[0].verification.probes.vision.latest.observed.image_input == true and
  .models[0].verification.probes.seed.latest.observed.deterministic_same_output == true and
  .models[0].verification.probes.headers.latest.observed.rate_limit_headers_seen == true
' "$SB_PROVIDER_REGISTRY" >/dev/null

"$SB" registry probe --dry-run --model test/echo | jq -e '.plan[0].row == "test/echo"' >/dev/null

print "ok - sb registry probe"
