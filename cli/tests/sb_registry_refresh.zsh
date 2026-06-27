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
  "counts": {"providers": 2, "models": 3},
  "provider_catalogs": {
    "openrouter_free": {
      "source_url": "https://openrouter.ai/api/v1/models?output_modalities=all",
      "fetched_at": "2026-01-01T00:00:00.000Z",
      "total_models": 3,
      "free_models": 2,
      "benchmarked_free_models": 0,
      "model_ids": ["old/free:free", "removed/free:free"]
    },
    "nvidia_build": {
      "source_url": "https://integrate.api.nvidia.com/v1/models",
      "fetched_at": "2026-01-01T00:00:00.000Z",
      "total_models": 1,
      "model_ids": ["minimaxai/minimax-m3"]
    }
  },
  "providers": [
    {"id": "openrouter", "name": "OpenRouter", "free_tier": true, "aggregator": true},
    {"id": "nvidia", "name": "NVIDIA Build", "free_tier": true, "aggregator": false}
  ],
  "models": [
    {
      "provider_id": "openrouter",
      "model_id": "old/free:free",
      "display_name": "Old Free",
      "context_window": 1000,
      "vision": false,
      "tool_calling": false,
      "json_schema": "none",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "capabilities": {
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "supported_parameters": []
      },
      "verification": {
        "declared": true,
        "probed": true,
        "probes": {"completion": {"latest": {"status": "pass"}}}
      },
      "provenance": [
        {
          "kind": "api",
          "source_url": "https://openrouter.ai/api/v1/models?output_modalities=all",
          "fetched_at": "2026-01-01T00:00:00.000Z"
        }
      ]
    },
    {
      "provider_id": "openrouter",
      "model_id": "removed/free:free",
      "display_name": "Removed Free",
      "context_window": 1000,
      "vision": false,
      "tool_calling": false,
      "json_schema": "none",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "capabilities": {
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "supported_parameters": []
      },
      "verification": {"declared": true, "probed": false, "probes": {}},
      "provenance": [
        {
          "kind": "api",
          "source_url": "https://openrouter.ai/api/v1/models?output_modalities=all",
          "fetched_at": "2026-01-01T00:00:00.000Z"
        }
      ]
    },
    {
      "provider_id": "nvidia",
      "model_id": "minimaxai/minimax-m3",
      "display_name": "MiniMax M3",
      "context_window": 512000,
      "vision": false,
      "tool_calling": false,
      "json_schema": "none",
      "input_micros_per_mtok": 0,
      "output_micros_per_mtok": 0,
      "capabilities": {
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "tool_calling": false,
        "json_schema": "none"
      },
      "verification": {
        "declared": true,
        "probed": true,
        "probes": {"completion": {"latest": {"status": "pass"}}}
      },
      "provenance": [
        {
          "kind": "api",
          "source_url": "https://integrate.api.nvidia.com/v1/models",
          "fetched_at": "2026-01-01T00:00:00.000Z"
        }
      ]
    }
  ]
}
JSON

cat > "${TMPDIR}/openrouter.json" <<'JSON'
{
  "data": [
    {
      "id": "old/free:free",
      "name": "Old Free",
      "context_length": 2000,
      "pricing": {"prompt": "0", "completion": "0", "input_cache_read": "0"},
      "architecture": {
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "tokenizer": "test",
        "instruct_type": "chat",
        "modality": "text->text"
      },
      "supported_parameters": ["tools", "response_format", "seed"],
      "top_provider": {"context_length": 2000, "max_completion_tokens": 512, "is_moderated": false},
      "per_request_limits": null,
      "benchmarks": {"ToyBench": 12.3}
    },
    {
      "id": "new/free:free",
      "name": "New Free",
      "context_length": 4096,
      "pricing": {"prompt": "0", "completion": "0"},
      "architecture": {
        "input_modalities": ["text"],
        "output_modalities": ["text"],
        "tokenizer": "test",
        "instruct_type": "chat",
        "modality": "text->text"
      },
      "supported_parameters": ["tools"],
      "top_provider": {"context_length": 4096, "max_completion_tokens": 1024, "is_moderated": false}
    },
    {
      "id": "paid/model",
      "name": "Paid Model",
      "context_length": 4096,
      "pricing": {"prompt": "0.000001", "completion": "0.000001"},
      "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
      "supported_parameters": []
    }
  ]
}
JSON

cat > "${TMPDIR}/nvidia.json" <<'JSON'
{
  "data": [
    {"id": "minimaxai/minimax-m3", "object": "model", "created": 0, "owned_by": "nvidia"}
  ]
}
JSON

out="$("$SB" registry refresh \
  --source openrouter \
  --source nvidia \
  --openrouter-json "${TMPDIR}/openrouter.json" \
  --nvidia-json "${TMPDIR}/nvidia.json" \
  --json \
  --no-receipt)"

print -r -- "$out" | jq -e '
  .applied == false and
  .receipt_path == null and
  (.sources | length == 2) and
  .drift.summary.added_models == 1 and
  .drift.summary.removed_models == 1 and
  .drift.summary.changed_models == 2 and
  .drift.summary.provider_catalog_changes == 1 and
  .drift.summary.stale_probe_rows == 2 and
  (.drift.added_models | index("openrouter/new/free:free")) and
  (.drift.removed_models | index("openrouter/removed/free:free")) and
  (.drift.changed_models[] | select(.key == "openrouter/old/free:free" and (.categories | index("context")) and (.categories | index("capabilities")) and .stale_probe == true)) and
  (.drift.changed_models[] | select(.key == "nvidia/minimaxai/minimax-m3" and (.categories | index("context")) and (.categories | index("capabilities")) and (.categories | index("benchmarks")) and .stale_probe == true))
' >/dev/null

"$SB" registry refresh \
  --source openrouter \
  --openrouter-json "${TMPDIR}/openrouter.json" \
  --check-drift \
  --no-receipt \
  --limit 2 | grep -q "registry refresh drift"

print "ok - sb registry refresh"
