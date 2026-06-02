# Switchback — 5-minute quickstart

Stand up the gateway and exercise routing, fallback, a tenant API key, a spend
cap, traces, and the usage ledger **with zero real API keys**. Everything below
runs against the credential-free `mock` provider wired in
[`config/demo.yaml`](config/demo.yaml).

> The only secret you set is a *placeholder* demo key — it authenticates the
> client to the gateway; it is never sent upstream.

---

## 1. Build (or use Docker)

```bash
# Native (single binary at ./target/release/switchback)
cargo build --release -p sb-server

# …or Docker (the image bakes in ./config, so demo.yaml is already inside)
docker build -t switchback .
```

The examples below call the binary as `switchback`. If you didn't install it on
`PATH`, use `./target/release/switchback` (or `./target/debug/switchback`).

## 2. Set the placeholder demo key and serve

`config/demo.yaml` reads its one API key from `SWITCHBACK_DEMO_KEY`. Pick any
value — it's the bearer token clients use to reach the gateway:

```bash
export SWITCHBACK_DEMO_KEY="sk-demo-placeholder"

# Sanity-check the config first (loads + validates, exits non-zero on problems):
switchback config validate --config config/demo.yaml
# {
#   "ok": true
# }

# Serve on a free port (8791 here — 8765 is the default).
mkdir -p /tmp/switchback-demo            # demo.yaml writes usage/trace JSONL here
switchback serve --bind 127.0.0.1:8791 --config config/demo.yaml
```

Leave that running and open a second terminal. Set the same key there too:

```bash
export SWITCHBACK_DEMO_KEY="sk-demo-placeholder"
export SB=http://127.0.0.1:8791
```

> **Docker variant:** `docker run --rm -p 8791:8791 -e SWITCHBACK_DEMO_KEY \
> switchback serve --config /app/config/demo.yaml --bind 0.0.0.0:8791`

## 3. Health check (no auth)

`/health` (and `/`) are the only endpoints that don't require the key:

```bash
curl -s $SB/health
# {"ok":true}
```

## 4. The auth header

Because `demo.yaml` defines `api_keys:`, **every** `/v1/*` request must carry the
tenant key. Without it you get a `401`:

```bash
curl -s -o /dev/null -w "%{http_code}\n" -X POST $SB/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"hi"}]}'
# 401
```

Add `Authorization: Bearer $SWITCHBACK_DEMO_KEY` to authenticate. The demo key is
attributed to tenant `demo` (project `quickstart`) for usage + quotas.

## 5. Chat completion (non-streaming)

```bash
curl -s -D - $SB/v1/chat/completions \
  -H "Authorization: Bearer $SWITCHBACK_DEMO_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"mock/echo","messages":[{"role":"user","content":"Hello from the demo"}]}'
```

```http
HTTP/1.1 200 OK
content-type: application/json
x-switchback-route: strategy=ordered_fallback selected=mock/echo fallbacks=[] rejected=0
x-switchback-request-id: req_05e318d899e0423894f004f619c1208e
x-switchback-revision: 1

{"choices":[{"finish_reason":"stop","index":0,"message":{"content":"echo: Hello from the demo","role":"assistant"}}],"created":1780363156,"id":"req_05e318d899e0423894f004f619c1208e","model":"mock/echo","object":"chat.completion","usage":{"completion_tokens":8,"prompt_tokens":8,"total_tokens":16}}
```

`mock/echo` echoes your last user message. The **`x-switchback-route`** response
header is the explainable route decision; **`x-switchback-request-id`** is the
trace id you'll look up in step 9.

## 6. Streaming (`curl -N`)

Same endpoint with `"stream": true` — SSE chunks in OpenAI's wire format:

```bash
curl -N $SB/v1/chat/completions \
  -H "Authorization: Bearer $SWITCHBACK_DEMO_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"mock/echo","stream":true,"messages":[{"role":"user","content":"stream me"}]}'
```

```
data: {"choices":[{"delta":{"role":"assistant"},"finish_reason":null,"index":0}],"model":"mock/echo","object":"chat.completion.chunk", ...}

data: {"choices":[{"delta":{"content":"echo:"},"finish_reason":null,"index":0}], ...}

data: {"choices":[{"delta":{"content":" stre"},"finish_reason":null,"index":0}], ...}

data: {"choices":[{"delta":{"content":"am me"},"finish_reason":null,"index":0}], ...}

data: {"choices":[],"usage":{"completion_tokens":8,"prompt_tokens":8,"total_tokens":16}, ...}

data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}], ...}

data: [DONE]
```

## 7. Routing + fallback — the `coding` combo

`demo.yaml` defines a `coding` combo with a `fallback` strategy across two
targets: `mock/echo` first, then `ollama/qwen2.5-coder`. Request `model: "coding"`
and the gateway picks the first runnable target and records the rest as the
fallback chain. See it **without running the server**:

```bash
switchback route-preview --config config/demo.yaml --model coding
```

…or live — the chain shows up right in the route header:

```bash
curl -s -D - -o /dev/null $SB/v1/chat/completions \
  -H "Authorization: Bearer $SWITCHBACK_DEMO_KEY" \
  -H 'content-type: application/json' \
  -d '{"model":"coding","messages":[{"role":"user","content":"write a function"}]}' \
  | grep -i x-switchback
```

```http
x-switchback-route: strategy=combo_fallback selected=mock/echo fallbacks=[ollama/qwen2.5-coder] rejected=0
x-switchback-request-id: req_b05c77bb969e4a6e9f8a56462d51d9ff
x-switchback-revision: 1
```

`selected=mock/echo fallbacks=[ollama/qwen2.5-coder]` — if `mock/echo` couldn't
run (e.g. you reorder so a real provider is first and it's down), Switchback
fails over to the next target *before the first streamed byte*. Start
`ollama serve` (and `ollama pull qwen2.5-coder`) to make the fallback target real.

## 8. List models

```bash
curl -s $SB/v1/models -H "Authorization: Bearer $SWITCHBACK_DEMO_KEY"
```

```
auto, auto/cheap, auto/fast, auto/coding, auto/private, auto/large-context, mock/echo, coding, mock, ollama
```

## 9. Usage ledger + spend cap

`demo.yaml` sets a `$5.00` global budget and a `$5.00` per-tenant cap (over either
→ HTTP `402`). `mock` is free, so the demo stays at `$0` — proving the meter
without spending a cent. The demo key has the `operator` role, so it can read its
own tenant's usage:

```bash
curl -s $SB/v1/usage -H "Authorization: Bearer $SWITCHBACK_DEMO_KEY"
```

```json
{"requests":3,"total_cost_micros":0,"total_cost_usd":0.0,
 "by_model":{},"by_provider":{},"by_tenant":{"demo":[3,0]},
 "scope":{"tenant":"demo"},
 "durability":{"status":"memory_only","memory_writes":3, ...}}
```

`by_tenant: {"demo": [requests, cost_micros]}`. Usage is also appended to
`/tmp/switchback-demo/usage.jsonl` (configured in `demo.yaml`).

## 10. Traces — every request, end to end

```bash
curl -s "$SB/v1/traces?limit=1" -H "Authorization: Bearer $SWITCHBACK_DEMO_KEY"
```

One metadata-only record per request: the route decision, every account/egress
attempt, tenant attribution, status, latency, and token usage — **never** prompt
or response content. The `coding`-combo request traces its fallback chain:

```json
{"inbound_model":"coding","route":"combo/coding",
 "decision":{"strategy":"combo_fallback",
   "selected":{"target_id":"mock/echo"},
   "fallbacks":[{"target_id":"ollama/qwen2.5-coder"}],
   "reason":["route=combo/coding","combo=coding","combo_strategy=fallback","tenant=demo", ...]},
 "attempts":[{"provider_id":"mock","target_id":"mock/echo","outcome":"success","latency_ms":0}],
 "final_status":200,"tenant":"demo","project":"quickstart",
 "usage":{"input_tokens":8,"output_tokens":8}}
```

Look up a single trace by the id from any `x-switchback-request-id` header:

```bash
curl -s $SB/v1/traces/req_b05c77bb969e4a6e9f8a56462d51d9ff \
  -H "Authorization: Bearer $SWITCHBACK_DEMO_KEY"
```

## 11. Point an OpenAI SDK at it

Switchback speaks the OpenAI wire format, so any OpenAI client works — just swap
`base_url` and `api_key`:

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8791/v1",   # the gateway, not api.openai.com
    api_key="sk-demo-placeholder",          # your SWITCHBACK_DEMO_KEY
)

# Hit the credential-free mock target…
resp = client.chat.completions.create(
    model="mock/echo",
    messages=[{"role": "user", "content": "Hello from the SDK"}],
)
print(resp.choices[0].message.content)   # -> "echo: Hello from the SDK"

# …or the routed combo with fallback:
resp = client.chat.completions.create(
    model="coding",
    messages=[{"role": "user", "content": "write a function"}],
)
```

```typescript
import OpenAI from "openai";

const client = new OpenAI({
  baseURL: "http://127.0.0.1:8791/v1",
  apiKey: "sk-demo-placeholder",
});

const resp = await client.chat.completions.create({
  model: "mock/echo",
  messages: [{ role: "user", content: "Hello from the SDK" }],
  stream: true, // streaming works through the SDK too
});
for await (const chunk of resp) process.stdout.write(chunk.choices[0]?.delta?.content ?? "");
```

---

## What you just exercised

| Concept | Where it showed up |
|---|---|
| Credential-free run | `mock` provider — no upstream keys needed |
| Explainable routing | `x-switchback-route` header + `route-preview` |
| Two-target fallback | `coding` combo (`strategy: fallback`) → `mock/echo`, then `ollama/qwen2.5-coder` |
| Tenant API key | `Authorization: Bearer $SWITCHBACK_DEMO_KEY` → tenant `demo` |
| Spend cap | `server.budget` ($5) + per-tenant `budget_usd` ($5) → `402` over limit |
| Usage ledger | `GET /v1/usage` (`by_tenant`) + `usage.jsonl` |
| Traces | `GET /v1/traces`, `GET /v1/traces/{id}` + `x-switchback-request-id` |

## Going real

Open [`config/switchback.example.yaml`](config/switchback.example.yaml) — it
documents every option. To add a real provider, it's mostly config:

```bash
# OpenRouter, Groq, Ollama, vLLM… are all "openai_compatible" + a base_url:
switchback provider add openrouter --config config/demo.yaml --model anthropic/claude-3.5-sonnet
export OPENROUTER_API_KEY=sk-or-...
switchback provider doctor openrouter --config config/demo.yaml   # discover + chat + stream check
```

Then add that target to the `coding` combo (or a `routes:` entry) and the
fallback becomes a real cross-provider failover — no rebuild required.
