#!/bin/zsh
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
SB="${SB_UNDER_TEST:-${CLI_ROOT}/sb}"
export HOME="${TMPDIR}/home"
# Tests must not read the developer's live runtime tree. These explicit values
# preserve the historical fixture paths while exercising the new root contract.
export SWITCHBACK_RUNTIME_ROOT="${HOME}/.config/switchback"
export SB_CONFIG="${HOME}/.config/switchback/switchback.yaml"
export SB_ENV="${HOME}/.config/switchback/sb.env"
export SB_STATE="${HOME}/.config/switchback/state"
export CODEX_PROFILES="${HOME}/.config/switchback/codex"
export CLAUDE_PROFILES="${HOME}/.config/switchback/claude"
export SB_AUTHREG="${HOME}/.config/switchback/codex-auth"
export SB_LAUNCH_PROFILES="${HOME}/.config/switchback/launch-profiles.json"
export SB_PROFILE_PROJECTION_ROOT="${HOME}/.config/switchback/state/profile-conformance"

export SB_SOURCE_ONLY=1
export SB_LAUNCH_PROFILES="${HOME}/.config/switchback/launch-profiles.json"
export SB_LANES="${HOME}/.config/switchback/lanes"
export CLAUDE_PROFILES="${HOME}/.config/switchback/claude"
export SB_PROFILE_WRAPPER_ROOT="${HOME}/.local/bin"
export SB_PROFILE_PROJECTION_ROOT="${HOME}/.local/state/switchback/profile-conformance"
export SB_CONFIG="${HOME}/.config/switchback/switchback.yaml"

source "$SB"

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: ${needle}\nactual:\n${haystack}"
}
assert_not_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" != *"$needle"* ]] || fail "expected output not to contain: ${needle}\nactual:\n${haystack}"
}

_switchback() {
  print -r -- "$*"
}

profile_plan="$(sb_profile --json plan claude-zai-full)"
assert_contains "$profile_plan" "--json profile plan claude-zai-full"
assert_contains "$profile_plan" "--authority ${SB_LAUNCH_PROFILES}"
assert_contains "$profile_plan" "--lane-root ${SB_LANES}"
assert_contains "$profile_plan" "--profile-root ${CLAUDE_PROFILES}/_providers"
assert_contains "$profile_plan" "--wrapper-root ${SB_PROFILE_WRAPPER_ROOT}"
assert_contains "$profile_plan" "--projection-root ${SB_PROFILE_PROJECTION_ROOT}"
assert_contains "$profile_plan" "--config ${SB_CONFIG}"

settings_apply="$(sb_settings --json apply claude-qwen)"
assert_contains "$settings_apply" "--json profile apply claude-qwen"
assert_contains "$settings_apply" "--authority ${SB_LAUNCH_PROFILES}"

assert_not_contains "$(_mode_wrapper_spec)" "claude-zai-full"

mkdir -p "${SB_LANES}" "${HOME}/.local/bin"
print -r -- '#!/bin/sh' > "${HOME}/.local/bin/headroom"
chmod +x "${HOME}/.local/bin/headroom"
cat > "${SB_LANES}/gpt56-sol-wpcom-headroom.env" <<'EOF'
SB_LANE_ANTHROPIC_URL='http://127.0.0.1:18765'
SB_LANE_HEADROOM_TOOL_SEARCH='0'
EOF
_have() { return 0; }
_listening() { return 0; }
launchctl() { return 0; }
sleep() { return 0; }

export HEADROOM_TOOL_SEARCH=1
_lane_headroom_up gpt56-sol-wpcom-headroom --port 8792 >/dev/null
headroom_plist="$(<"$(_lane_headroom_plist gpt56-sol-wpcom-headroom)")"
assert_contains "$headroom_plist" "<key>HEADROOM_TOOL_SEARCH</key><string>0</string>"

print "ok - sb launch-profile/settings adapters"
