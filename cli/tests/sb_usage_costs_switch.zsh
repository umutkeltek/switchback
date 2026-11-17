#!/bin/zsh
# sb costs on|off|status keeps one switch in sb.env; off reads no log at all.
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export SWITCHBACK_RUNTIME_ROOT="${TMPDIR}/runtime"
unset SB_RUNTIME_ROOT SB_USAGE_COSTS SB_USAGE_COSTS_STATE CLAUDE_CONFIG_DIR CODEX_HOME SB_CLAUDE_DEFAULT_HOME
mkdir -p "$HOME" "${SWITCHBACK_RUNTIME_ROOT}/config"
print -r -- '{}' > "${SWITCHBACK_RUNTIME_ROOT}/manifest.json"
ENV_FILE="${SWITCHBACK_RUNTIME_ROOT}/config/sb.env"
STATE="${SWITCHBACK_RUNTIME_ROOT}/state/usage-cost.sqlite"

fail() { print -ru2 -- "FAIL: $1"; exit 1; }

out="$("$SB" costs status)"
[[ "$out" == *"Token cost estimates: off"* ]] || fail "default is off"
off_json="$("$SB" costs --json)"
[[ "$off_json" == *'"enabled": false'* ]] || fail "off json: $off_json"
[[ ! -e "$STATE" ]] || fail "off created state"

out="$("$SB" costs on)"
[[ "$out" == *"Token cost estimates: on"* ]] || fail "on message"
grep -qx 'export SB_USAGE_COSTS=1' "$ENV_FILE" || fail "on not saved"
out="$("$SB" costs status)"
[[ "$out" == *"Token cost estimates: on"* ]] || fail "status after on"

on_json="$("$SB" costs --json)"
[[ "$on_json" == *'"enabled": true'* && "$on_json" == *'"basis": "estimated_from_local_logs"'* ]] || fail "on json: $on_json"
[[ -f "$STATE" ]] || fail "on did not create state"

"$SB" costs off >/dev/null
grep -qx 'export SB_USAGE_COSTS=0' "$ENV_FILE" || fail "off not saved"
[[ "$(grep -c '^export SB_USAGE_COSTS=' "$ENV_FILE")" == 1 ]] || fail "switch saved twice"

set +e
"$SB" costs sideways >/dev/null 2>&1; code=$?
"$SB" costs on --json >/dev/null 2>&1; both=$?
set -e
(( code == 2 )) || fail "bad argument exit $code"
(( both == 2 )) || fail "switch with --json exit $both"

print -r -- "ok: sb costs switch"
