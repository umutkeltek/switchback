#!/bin/zsh
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
SB="${SB_UNDER_TEST:-${CLI_ROOT}/sb}"
SB_BIN="${SB_BIN:-${CLI_ROOT:h}/target/debug/switchback}"
[[ -x "$SB_BIN" ]] || { print -ru2 -- "FAIL: missing typed binary: $SB_BIN"; exit 1; }

export HOME="${TMPDIR}/home"
export SB_BIN
export SB_LANES="${HOME}/.config/switchback/lanes"
export CLAUDE_PROFILES="${HOME}/.config/switchback/claude"
config="${HOME}/.config/switchback/switchback.yaml"
mkdir -p "${config:h}"

cat > "$config" <<'YAML'
server:
  bind: "127.0.0.1:0"
  retry:
    max_retries: 2
  circuit_breaker:
    enabled: true
    failure_threshold: 3
    open_secs: 30
providers:
  - id: primary
    type: mock
  - id: fallback
    type: mock
routes:
  - name: sol-canonical
    match:
      model: "openai/gpt-5.6-sol"
    targets:
      - "primary/gpt-5.6-sol"
      - "fallback/gpt-5.5"
  - name: sol-alias
    match:
      model: "gpt-5.6-sol"
    targets:
      - "primary/gpt-5.6-sol"
      - "fallback/gpt-5.5"
YAML

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: ${needle}\nactual:\n${haystack}"
}

typeset RUN_STATUS RUN_OUTPUT
run_sb() {
  RUN_STATUS=0
  RUN_OUTPUT="$(zsh "$SB" "$@" 2>&1)" || RUN_STATUS=$?
}

define_args=(
  lane define --json typed-ultra
  --model openai/gpt-5.6-sol
  --route openai/gpt-5.6-sol
  --alias gpt-5.6-sol
  --transport headroom
  --effort ultra
  --anthropic-port 8788
  --profile-label typed-ultra
  --display-name "GPT-5.6 Sol Ultra"
  --min-fallbacks 1
)

# The shell is a thin adapter: dry-run stays read-only, and typed Rust owns the report.
run_sb "${define_args[@]}"
(( RUN_STATUS == 0 )) || fail "typed dry-run failed: ${RUN_OUTPUT}"
print -r -- "$RUN_OUTPUT" | jq -e '
  .schema == "switchback/claude-lane-define@1"
  and .dry_run == true
  and .definition.requested_effort == "ultra"
  and .definition.claude_effort == "max"
  and .definition.profile_label == "typed-ultra"
  and .audit.ok == true
' >/dev/null || fail "typed dry-run report was invalid: ${RUN_OUTPUT}"
[[ ! -e "${SB_LANES}/typed-ultra.env" ]] || fail "dry-run wrote a lane record"
[[ ! -e "${CLAUDE_PROFILES}/_providers/typed-ultra/settings.json" ]] || fail "dry-run wrote settings"

# Apply creates the existing lane record + Claude profile surfaces through the typed owner.
run_sb "${define_args[@]}" --apply
(( RUN_STATUS == 0 )) || fail "typed apply failed: ${RUN_OUTPUT}"
print -r -- "$RUN_OUTPUT" | jq -e '
  .schema == "switchback/claude-lane-define@1"
  and .applied == true
  and .definition.transport == "headroom"
  and .definition.requested_effort == "ultra"
  and .definition.claude_effort == "max"
  and .audit.ok == true
' >/dev/null || fail "typed apply report was invalid: ${RUN_OUTPUT}"
[[ -f "${SB_LANES}/typed-ultra.env" ]] || fail "apply did not create lane record"
[[ -f "${CLAUDE_PROFILES}/_providers/typed-ultra/settings.json" ]] || fail "apply did not create settings"
grep -F "SB_LANE_REQUESTED_EFFORT='ultra'" "${SB_LANES}/typed-ultra.env" >/dev/null || \
  fail "lane record did not preserve requested Ultra"
grep -F "SB_LANE_CLAUDE_EFFORT='max'" "${SB_LANES}/typed-ultra.env" >/dev/null || \
  fail "lane record did not materialize Claude max"
jq -e '.effortLevel == "max"' "${CLAUDE_PROFILES}/_providers/typed-ultra/settings.json" >/dev/null || \
  fail "settings did not materialize Claude max"

# Named doctor delegates to typed Claude conformance; unnamed doctor keeps runtime-route authority.
run_sb lane doctor typed-ultra --json
(( RUN_STATUS == 0 )) || fail "named typed audit failed: ${RUN_OUTPUT}"
print -r -- "$RUN_OUTPUT" | jq -e '
  .schema == "switchback/claude-lane-audit@1"
  and .ok == true
  and .definition.profile_label == "typed-ultra"
' >/dev/null || fail "named typed audit was invalid: ${RUN_OUTPUT}"

run_sb lane doctor --json
(( RUN_STATUS == 0 )) || fail "runtime lane doctor failed: ${RUN_OUTPUT}"
print -r -- "$RUN_OUTPUT" | jq -e '.schema == "switchback/lane-doctor@1"' >/dev/null || \
  fail "unnamed doctor did not retain runtime authority: ${RUN_OUTPUT}"

# Pulse discovers typed lane records, not arbitrary provider directories.
pulse_checks="${TMPDIR}/pulse-checks.jsonl"
: > "$pulse_checks"
(
  export SB_SOURCE_ONLY=1
  export PULSE_CHECKS_FILE="$pulse_checks"
  source "$SB"
  _pulse_lane_doctor
)
jq -e '
  .name == "lane-doctor"
  and .area == "lanes"
  and .level == "ok"
  and (.detail | contains("all 1 typed Claude lane"))
' "$pulse_checks" >/dev/null || fail "pulse did not consume typed lane records"

# Launch consumes the materialized profile label and native effort without inventing `-lane`.
launch_result="$(
  export SB_SOURCE_ONLY=1
  source "$SB"
  _lane_keyval() { print -r -- "test-key"; }
  _listening() { return 0; }
  _exec_claude_provider_mode() { print -r -- "profile=$1 effort=$8 headers=${ANTHROPIC_CUSTOM_HEADERS:-}"; }
  _run_claude_lane typed-ultra
)"
assert_contains "$launch_result" "profile=typed-ultra"
assert_contains "$launch_result" "effort=max"
assert_contains "$launch_result" "x-switchback-lane-id: typed-ultra"
assert_contains "$launch_result" "x-switchback-lane-revision: sha256:"
assert_contains "$launch_result" "x-switchback-requested-effort: ultra"

codex_headers="$(
  export SB_SOURCE_ONLY=1
  source "$SB"
  SB_EXECUTION_LANE_ID=typed-ultra \
  SB_EXECUTION_LANE_REVISION="$(sed -n "s/^SB_LANE_REVISION='\(.*\)'$/\1/p" "${SB_LANES}/typed-ultra.env")" \
  SB_EXECUTION_REQUESTED_EFFORT=ultra \
  _codex_execution_headers_toml
)"
assert_contains "$codex_headers" '"x-switchback-lane-id"="typed-ultra"'
assert_contains "$codex_headers" '"x-switchback-lane-revision"="sha256:'
assert_contains "$codex_headers" '"x-switchback-requested-effort"="ultra"'

! rg -q '_lane_doctor_report|PyYAML' "$SB" || fail "duplicate Python lane doctor still exists"

print "ok - sb typed lane define/doctor adapter"
