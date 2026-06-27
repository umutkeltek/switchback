#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
REGISTRY="${ROOT:h}/config/provider-registry.json"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export SB_PROVIDER_REGISTRY="$REGISTRY"
mkdir -p "$HOME"

jq -e '
  (.providers | any(.id == "neuralwatt"
    and .base_url == "https://api.neuralwatt.com/v1"
    and .provider_research.energy_pricing_url == "https://portal.neuralwatt.com/energy-pricing"))
  and ([.models[] | select(.provider_id == "neuralwatt")] | length == 11)
  and (.models | any(.provider_id == "neuralwatt"
    and .model_id == "glm-5.2"
    and .input_micros_per_mtok == 1450000
    and .cached_input_micros_per_mtok == 360000
    and .output_micros_per_mtok == 4500000
    and .capabilities.tool_calling == true
    and .capabilities.reasoning == true
    and .energy.pricing_basis.energy_usd_per_kwh == 5
    and .energy.observed_prompt_band_wh."64k_256k" == 2.34))
  and (.models | any(.provider_id == "neuralwatt"
    and .model_id == "qwen3.6-35b"
    and .capabilities.image_input == true
    and .capabilities.json_schema == "native"
    and .architecture.mixture_of_experts == true
    and .architecture.parameters_active_b == 3))
' "$REGISTRY" >/dev/null

detail="$(zsh "$SB" registry model neuralwatt/glm-5.2)"
print -r -- "$detail" | jq -s -e '
  length == 1
  and .[0].provider_id == "neuralwatt"
  and .[0].model_id == "glm-5.2"
  and .[0].energy.observed_average_request_wh == 2.34
' >/dev/null

print "ok - sb registry neuralwatt"
