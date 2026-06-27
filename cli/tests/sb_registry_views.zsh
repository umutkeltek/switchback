#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export SB_PROVIDER_REGISTRY="${TMPDIR}/provider-registry.json"
mkdir -p "$HOME" "${TMPDIR}/bin"

cat > "$SB_PROVIDER_REGISTRY" <<'JSON'
{
  "schema": "switchback/provider-registry@2",
  "money": "integer micro-USD per 1M tokens",
  "counts": {"providers": 2, "models": 2, "free_models": 2, "enriched_models": 2, "benchmarked_models": 1},
  "provider_catalogs": {
    "openrouter_free": {"free_models": 1, "benchmarked_free_models": 1},
    "nvidia_build": {"total_models": 1}
  },
  "providers": [
    {"id": "openrouter", "name": "OpenRouter", "base_url": "https://openrouter.ai/api/v1", "free_tier": true, "aggregator": true},
    {"id": "nvidia", "name": "NVIDIA Build", "base_url": "https://integrate.api.nvidia.com/v1", "free_tier": true, "aggregator": false}
  ],
  "models": [
    {
      "provider_id": "openrouter",
      "model_id": "qwen/qwen3-coder:free",
      "display_name": "Qwen3 Coder Free",
      "tier": "F",
      "context_window": 1048576,
      "vision": false,
      "tool_calling": true,
      "json_schema": "none",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "flags": ["OpenRouter :free; non-SLA tripwire/execution lane only"],
      "capabilities": {
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "supported_parameters": ["tools", "tool_choice", "temperature"],
        "tool_calling": true,
        "tool_choice": true,
        "json_schema": "none"
      },
      "limits": {"context_window": 1048576, "provider_context_window": 262000, "max_completion_tokens": 262000},
      "architecture": {"source": "openrouter_models_api", "mixture_of_experts": true, "tokenizer": "Qwen3"},
      "benchmarks": {
        "openrouter": {
          "source": "openrouter_models_api",
          "values": {
            "design_arena": [{"arena": "models", "category": "codecategories", "elo": 1193, "rank": 54, "win_rate": 61.2}]
          }
        }
      },
      "provenance": [{"kind": "api", "source_url": "https://openrouter.ai/api/v1/models?output_modalities=all"}]
    },
    {
      "provider_id": "nvidia",
      "model_id": "minimaxai/minimax-m3",
      "display_name": "MiniMax M3",
      "tier": "F",
      "context_window": 1000000,
      "vision": true,
      "tool_calling": true,
      "json_schema": "unknown",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "flags": ["NVIDIA Build free endpoint"],
      "capabilities": {
        "input_modalities": ["text", "image"],
        "output_modalities": ["text"],
        "tool_calling": true,
        "reasoning": true,
        "json_schema": "unknown"
      },
      "benchmarks": {"vendor": {"source": "minimax_model_blog", "values": {"SWE-Bench Pro": 59.0}}},
      "provenance": [{"kind": "model_card", "source_url": "https://www.minimax.io/blog/minimax-m3"}]
    }
  ]
}
JSON

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle\nactual:\n$haystack"
}

summary="$(zsh "$SB" registry)"
caps="$(zsh "$SB" registry capabilities qwen)"
bench="$(zsh "$SB" registry benchmarks minimax)"
detail="$(zsh "$SB" registry model qwen/qwen3-coder:free)"

assert_contains "$summary" "schema: switchback/provider-registry@2"
assert_contains "$summary" "openrouter_free=1"
assert_contains "$caps" "qwen/qwen3-coder:free"
assert_contains "$caps" "262000"
assert_contains "$bench" "SWE-Bench Pro=59"
assert_contains "$detail" "\"provider_context_window\": 262000"
assert_contains "$detail" "\"pair\": \"\$0/\$0 per 1M tokens\""
assert_contains "$detail" "\"usd_per_mtok\""

print "ok - sb registry views"
