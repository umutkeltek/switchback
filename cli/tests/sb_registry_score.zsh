#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export SB_PROVIDER_REGISTRY="${TMPDIR}/provider-registry.json"
mkdir -p "$HOME"

cat > "$SB_PROVIDER_REGISTRY" <<'JSON'
{
  "schema": "switchback/provider-registry@2",
  "money": "integer micro-USD per 1M tokens",
  "providers": [
    {"id": "deepseek", "name": "DeepSeek", "free_tier": false},
    {"id": "nvidia", "name": "NVIDIA Build", "free_tier": true},
    {"id": "openrouter", "name": "OpenRouter", "free_tier": true}
  ],
  "models": [
    {
      "provider_id": "deepseek",
      "model_id": "deepseek-v4-pro",
      "tier": "R/G",
      "context_window": 1000000,
      "tool_calling": true,
      "json_schema": "native",
      "input_micros_per_mtok": 1740000,
      "output_micros_per_mtok": 3480000,
      "verification": {"declared": true, "probed": false, "probes": {}}
    },
    {
      "provider_id": "nvidia",
      "model_id": "nvidia/nemotron-3-ultra-550b-a55b",
      "context_window": 1000000,
      "tool_calling": true,
      "json_schema": "native",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "capabilities": {"reasoning": true, "tool_calling": true, "json_schema": "native", "text_output": true},
      "benchmarks": {"vendor": {"values": {"SWE-Bench Verified": 69.7, "GPQA (no tools)": 87.9}}},
      "verification": {"declared": true, "probed": false, "probes": {}}
    },
    {
      "provider_id": "openrouter",
      "model_id": "openrouter/free",
      "context_window": 200000,
      "tool_calling": true,
      "json_schema": "native",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "capabilities": {"reasoning": true, "tool_calling": true, "json_schema": "native", "text_output": true},
      "verification": {
        "declared": true,
        "probed": true,
        "last_probe_at": "2026-06-27T00:00:00.000Z",
        "observed_capabilities": {"text_output": false, "streaming": true},
        "probes": {
          "completion": {"latest": {"status": "fail"}},
          "stream": {"latest": {"status": "pass"}}
        }
      }
    }
  ]
}
JSON

judge_top="$("$SB" registry score judge --limit 1 --json | jq -r '.rows[0].offering_id')"
long_top="$("$SB" registry score long_context nvidia --limit 1 --json | jq -r '.rows[0].offering_id')"
tripwire_top="$("$SB" registry score cheap_tripwire --require-probed --limit 1 --json | jq -r '.rows[0].offering_id')"
judge_table="$("$SB" registry score judge --limit 3)"

[[ "$judge_top" == "deepseek/deepseek-v4-pro" ]] || {
  print "expected judge top deepseek/deepseek-v4-pro, got ${judge_top}" >&2
  exit 1
}
[[ "$long_top" == "nvidia/nvidia/nemotron-3-ultra-550b-a55b" ]] || {
  print "expected long_context top nvidia/nvidia/nemotron-3-ultra-550b-a55b, got ${long_top}" >&2
  exit 1
}
[[ "$tripwire_top" == "openrouter/openrouter/free" ]] || {
  print "expected probed tripwire openrouter/openrouter/free, got ${tripwire_top}" >&2
  exit 1
}
print -r -- "$judge_table" | grep -q "free_not_certifier"

print "ok - sb registry score"
