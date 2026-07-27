#!/bin/zsh
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
SB="${SB_UNDER_TEST:-${CLI_ROOT}/sb}"
export HOME="${TMPDIR}/home"
export SWITCHBACK_ROOT="${TMPDIR}/checkout"
export SB_SOURCE_ONLY=1
mkdir -p "$HOME" "$SWITCHBACK_ROOT/config"

source "$SB"

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
assert_eq() { [[ "$1" == "$2" ]] || fail "expected '$2', got '$1'"; }

assert_eq "$SB_RUNTIME_ROOT" "${SWITCHBACK_ROOT}/.switchback"
assert_eq "$SWITCHBACK_RUNTIME_ROOT" "$SB_RUNTIME_ROOT"
assert_eq "$SB_CONFIG" "$SB_RUNTIME_ROOT/config/switchback.yaml"
assert_eq "$SB_ENV" "$SB_RUNTIME_ROOT/config/sb.env"
assert_eq "$SB_STATE" "$SB_RUNTIME_ROOT/state"
assert_eq "$SB_MODE_D_CONFIG" "$SB_RUNTIME_ROOT/config/mode-d.yaml"
assert_eq "$SB_LANES" "$SB_RUNTIME_ROOT/config/lanes"
assert_eq "$SB_LAUNCH_PROFILES" "$SB_RUNTIME_ROOT/config/launch-profiles.json"
assert_eq "$SB_PROFILE_PROJECTION_ROOT" "$SB_RUNTIME_ROOT/state/profile-conformance"
assert_eq "$SB_AUTHREG" "$SB_RUNTIME_ROOT/config/codex-auth"
assert_eq "$SB_PROVIDER_REGISTRY" "$SWITCHBACK_ROOT/config/provider-registry.json"

print "ok - sb runtime path ownership"
