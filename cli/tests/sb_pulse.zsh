#!/bin/zsh
set -euo pipefail
unsetopt bg_nice

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

if [[ -n "${SB_UNDER_TEST:-}" ]]; then
  SB="$SB_UNDER_TEST"
else
  CLI_ROOT="${0:A:h:h}"
  SB="${CLI_ROOT}/sb"
fi
export HOME="${TMPDIR}/home"
export SWITCHBACK_ROOT="${TMPDIR}/repo"
export SB_RUNTIME_ROOT="${SWITCHBACK_ROOT}/.switchback"
export SB_STATE="${SB_RUNTIME_ROOT}/state"
export SB_PROVIDER_REGISTRY="${SWITCHBACK_ROOT}/config/provider-registry.json"
export SB_LANES="${HOME}/.config/switchback/lanes"
export SB_MODE_D_CONFIG="${HOME}/.config/switchback/mode-d.yaml"
export SB_GATEWAY="http://127.0.0.1:18765"
export SB_BIN="${TMPDIR}/bin/switchback"
export SB_LSOF_BIN="${TMPDIR}/bin/lsof"
export PATH="${TMPDIR}/bin:${PATH}"
export TEST_ALPHA_KEY="alpha-secret-value-must-not-leak"
export TEST_BETA_KEY="beta-secret-value-must-not-leak"
export SWITCHBACK_SCOUT_API_KEY="scout-secret-value-must-not-leak"
export FAKE_LOG="${TMPDIR}/calls.log"
export FAKE_LISTEN_PORTS="19000 19001 19002 19080 19865"

mkdir -p \
  "${TMPDIR}/bin" \
  "${HOME}/.config/switchback/lanes" \
  "${HOME}/.local/bin" \
  "${SWITCHBACK_ROOT}/config/generated" \
  "${SB_STATE}"

print -r -- "same generated config" > "${HOME}/.config/switchback/switchback.yaml"
cp "${HOME}/.config/switchback/switchback.yaml" "${SWITCHBACK_ROOT}/config/generated/switchback.yaml"
print -r -- "mode d fixture" > "$SB_MODE_D_CONFIG"
print -r -- '{"generated":"2998-01-01","models":[{"verification":{"probes":{"completion":{"latest":{"finished_at":"2999-01-01T00:00:00Z"}}}}}]}' > "$SB_PROVIDER_REGISTRY"

cat > "${SB_LANES}/alpha.env" <<'EOF'
SB_LANE_NAME="alpha"
SB_LANE_KEY_ENV="TEST_ALPHA_KEY"
SB_LANE_MODEL="model"
SB_LANE_ANTHROPIC_TAP="19001"
SB_LANE_ROUTE="alpha/model"
EOF

cat > "${SB_LANES}/beta.env" <<'EOF'
SB_LANE_NAME="beta"
SB_LANE_KEY_ENV="TEST_BETA_KEY"
SB_LANE_MODEL="model"
SB_LANE_ANTHROPIC_TAP="19002"
SB_LANE_ROUTE="beta/model"
EOF

cat > "$SB_BIN" <<'FAKE'
#!/bin/zsh
set -eu
print -r -- "switchback:${1:-}:${2:-}:${3:-}" >> "$FAKE_LOG"
if [[ "${1:-}" == reload || "${1:-}" == restart ]]; then
  print -r -- mutation > "${TMPDIR}/pulse-switchback-mutation"
  exit 9
fi

if [[ "${1:-}" == config && "${2:-}" == get ]]; then
  case "${3:-}" in
    server.taps) print -r -- '[{"id":"test-tap","bind":"127.0.0.1:19000"}]' ;;
    server.bind) print -r -- '"127.0.0.1:19865"' ;;
    server.forward_proxies) print -r -- '[{"id":"mode-d-proxy","bind":"127.0.0.1:19080"}]' ;;
    routes) print -r -- '[{"match":{"model":"alpha/model"},"targets":["alpha-coding/model"]},{"match":{"model":"beta/model"},"targets":["beta-coding/model"]}]' ;;
    providers) print -r -- '[]' ;;
    *) exit 1 ;;
  esac
  exit 0
fi

if [[ "${1:-}" == --json && "${2:-}" == body && "${3:-}" == status ]]; then
  case "${FAKE_BODY_MODE:-ok}" in
    ok) print -r -- '{"status":"ok","archive_available":true,"spool_backlog":0,"spool_backlog_exact":true}' ;;
    backlog) print -r -- '{"status":"spooling","archive_available":true,"spool_backlog":3,"spool_backlog_exact":true}' ;;
    unavailable) print -r -- '{"status":"archive_unavailable","archive_available":false,"spool_backlog":0,"spool_backlog_exact":true}' ;;
    *) exit 1 ;;
  esac
  exit 0
fi

if [[ "${1:-}" == --json && "${2:-}" == native && "${3:-}" == verify ]]; then
  print -r -- "native_args:$*" >> "$FAKE_LOG"
  [[ "${FAKE_NATIVE_FAIL:-0}" == 0 ]] || exit 1
  print -r -- '{"schema":"switchback/native-verify@1","ok":true,"exercises":[{"name":"large-payload","ok":true},{"name":"stream","ok":true}]}'
  exit 0
fi

exit 1
FAKE
chmod +x "$SB_BIN"

cat > "$SB_LSOF_BIN" <<'FAKE'
#!/bin/zsh
set -eu
port=""
for arg in "$@"; do
  [[ "$arg" == -iTCP:* ]] && port="${arg#-iTCP:}"
done
[[ -n "$port" && " ${FAKE_LISTEN_PORTS:-} " == *" ${port} "* ]]
FAKE
chmod +x "$SB_LSOF_BIN"

cat > "${TMPDIR}/bin/launchctl" <<'FAKE'
#!/bin/zsh
set -eu
print -r -- "launchctl:${1:-}:${2:-}" >> "$FAKE_LOG"
[[ "${1:-}" == print ]] || { print -r -- mutation > "${TMPDIR}/pulse-launchctl-mutation"; exit 9; }
label="${2##*/}"
[[ "$label" != ai.switchback.scout || "${FAKE_SCOUT_DOWN:-0}" == 0 ]] || exit 1
[[ "$label" != ai.switchback.mode-d || "${FAKE_MODE_D_AGENT_DOWN:-0}" == 0 ]] || exit 1
print -r -- $'service = {\n\tstate = running\n\tpid = 123\n}'
FAKE
chmod +x "${TMPDIR}/bin/launchctl"

cat > "${TMPDIR}/bin/curl" <<'FAKE'
#!/bin/zsh
set -eu
url="" output="" payload="" write_code=0 auth_present=0
while (( $# )); do
  case "$1" in
    -o) output="${2:-}"; shift 2 ;;
    -w) write_code=1; shift 2 ;;
    -d|--data|--data-raw) payload="${2:-}"; shift 2 ;;
    -H)
      [[ "${2:-}" == [Aa]uthorization:* ]] && auth_present=1
      shift 2 ;;
    http://*|https://*) url="$1"; shift ;;
    *) shift ;;
  esac
done

case "$url" in
  http://127.0.0.1:18765/health)
    [[ "${FAKE_ENGINE_DOWN:-0}" == 0 ]] || exit 7
    [[ "${FAKE_ENGINE_BAD_JSON:-0}" == 0 ]] && print -r -- '{"ok":true}' || print -r -- '{"ok":false}' ;;
  http://127.0.0.1:8787/health)
    [[ "${FAKE_HEADROOM_DOWN:-0}" == 0 ]] || exit 7
    print -r -- '{"status":"healthy","ready":true}' ;;
  http://127.0.0.1:19865/health)
    [[ "${FAKE_MODE_D_HEALTH_DOWN:-0}" == 0 ]] || exit 7
    print -r -- '{"ok":true}' ;;
  http://127.0.0.1:18765/cp/v1/route-preview)
    [[ "${FAKE_ENGINE_DOWN:-0}" == 0 ]] || exit 7
    model="${payload#*\"model\":\"}"; model="${model%%\"*}"
    if [[ " ${FAKE_ROUTE_DOWN:-} " == *" ${model} "* ]]; then
      print -r -- '{"decision":{"selected":{"target_id":"fallback/other"}}}'
    else
      print -r -- "{\"decision\":{\"selected\":{\"target_id\":\"${model}\"},\"reason\":[\"route=${model}\"]}}"
    fi ;;
  http://127.0.0.1:18765/v1/usage/events\?limit=1)
    [[ "${FAKE_ENGINE_DOWN:-0}" == 0 ]] || exit 7
    [[ "${FAKE_USAGE_DOWN:-0}" == 0 ]] || exit 7
    now_s="$(date +%s)"; age_s="$(( ${FAKE_USAGE_AGE_HOURS:-0} * 3600 ))"
    print -r -- "{\"events\":[{\"created_at_ms\":$(( (now_s - age_s) * 1000 ))}]}" ;;
  http://127.0.0.1:18765/v1/chat/completions)
    print -r -- "completion:auth=${auth_present}:${payload}" >> "$FAKE_LOG"
    code="${FAKE_COMPLETION_CODE:-200}"
    if [[ "$code" == 200 && "${FAKE_COMPLETION_EMPTY:-0}" == 1 ]]; then
      print -r -- '{"choices":[{}]}' > "$output"
    elif [[ "$code" == 200 && "${FAKE_COMPLETION_NULL_CONTENT:-0}" == 1 ]]; then
      print -r -- '{"choices":[{"message":{"role":"assistant","content":null},"finish_reason":"length"}],"usage":{"completion_tokens":1}}' > "$output"
    elif [[ "$code" == 200 ]]; then
      print -r -- '{"choices":[{"message":{"content":"x"}}]}' > "$output"
    else
      print -r -- '{"error":{"message":"synthetic"}}' > "$output"
    fi
    (( write_code )) && print -n -- "$code" ;;
  *) print -r -- "unexpected-url:${url}" >> "$FAKE_LOG"; exit 22 ;;
esac
FAKE
chmod +x "${TMPDIR}/bin/curl"

cat > "${HOME}/.local/bin/switchback-codex-stress" <<'FAKE'
#!/bin/zsh
set -eu
print -r -- "stress:$*" >> "$FAKE_LOG"
if [[ "${FAKE_STRESS_FAIL:-0}" == 1 ]]; then
  print "PASS config"; print "FAIL codex"; print "FAILED: 1 failure(s), 0 warning(s), 2 checks."; exit 1
fi
print "PASS config"; print "PASS route"; print "OK: 2 checks, 0 warning(s)."
FAKE
chmod +x "${HOME}/.local/bin/switchback-codex-stress"

cat > "${TMPDIR}/bin/fzf" <<'FAKE'
#!/bin/zsh
print -r -- used > "${TMPDIR}/pulse-fzf-used"
exit 9
FAKE
chmod +x "${TMPDIR}/bin/fzf"

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: ${needle}\nactual:\n${haystack}"
}
assert_eq() { [[ "$1" == "$2" ]] || fail "expected '$2', got '$1'"; }

typeset RUN_STATUS RUN_OUT RUN_ERR
run_pulse() {
  local tag="$1"; shift
  RUN_OUT="${TMPDIR}/${tag}.out"; RUN_ERR="${TMPDIR}/${tag}.err"; RUN_STATUS=0
  zsh "$SB" pulse "$@" </dev/null >"$RUN_OUT" 2>"$RUN_ERR" || RUN_STATUS=$?
}

receipt="${SB_STATE}/pulse/last.json"
history_file="${SB_STATE}/pulse/history.jsonl"

validate_receipt() {
  local file="$1"
  jq -e '
    .schema == "switchback/pulse@1"
    and (.ts | type == "string" and length > 0)
    and (.mode == "fast" or .mode == "live")
    and (.overall == "ok" or .overall == "warn" or .overall == "fail")
    and (.duration_ms | type == "number" and . >= 0)
    and (.checks | type == "array")
    and all(.checks[];
      (.name | type == "string" and length > 0)
      and (.area | type == "string" and length > 0)
      and (.level == "ok" or .level == "warn" or .level == "fail")
      and (.detail | type == "string"))
    and .summary.ok == ([.checks[] | select(.level == "ok")] | length)
    and .summary.warn == ([.checks[] | select(.level == "warn")] | length)
    and .summary.fail == ([.checks[] | select(.level == "fail")] | length)
  ' "$file" >/dev/null || fail "invalid pulse receipt: $file"
}

run_pulse green-json --json
assert_eq "$RUN_STATUS" 0
validate_receipt "$RUN_OUT"; validate_receipt "$receipt"
cmp -s "$RUN_OUT" "$receipt" || fail "--json stdout must equal last.json"
jq -e '.mode == "fast" and .overall == "ok" and .summary.warn == 0 and .summary.fail == 0' "$receipt" >/dev/null || fail "green fast pulse was not ok"
jq -e '.duration_ms < 5000' "$receipt" >/dev/null || fail "fast pulse exceeded the five-second target"
for name in engine tap:test-tap mode-d headroom launchagent:scout launchagent:mode-d body lane:alpha lane:beta registry usage-flow config-drift; do
  jq -e --arg name "$name" 'any(.checks[]; .name == $name)' "$receipt" >/dev/null || fail "missing fast check: $name"
done
jq -e 'all(.checks[]; .name != "native-verify" and .name != "gateway-completion" and .name != "stress-fast")' "$receipt" >/dev/null || fail "fast pulse ran live checks"
fast_log="$(cat "$FAKE_LOG")"
[[ "$fast_log" != *"native_args:"* && "$fast_log" != *"completion:"* && "$fast_log" != *"stress:"* && "$fast_log" != *"unexpected-url:"* ]] || fail "fast pulse invoked a live or unexpected surface"

run_pulse green-human
assert_eq "$RUN_STATUS" 0
assert_eq "$(wc -l < "$RUN_OUT" | tr -d ' ')" 1
assert_contains "$(cat "$RUN_OUT")" "pulse: ok"
assert_contains "$(cat "$RUN_OUT")" ".switchback/state/pulse/last.json"

run_pulse green-live --live --json
assert_eq "$RUN_STATUS" 0
validate_receipt "$RUN_OUT"
jq -e '.mode == "live" and .overall == "ok"' "$receipt" >/dev/null || fail "green live pulse was not ok"
for name in native-verify gateway-completion stress-fast; do
  jq -e --arg name "$name" 'any(.checks[]; .name == $name and .level == "ok")' "$receipt" >/dev/null || fail "missing live check: $name"
done
native_line="$(grep '^native_args:' "$FAKE_LOG" | tail -1)"
assert_contains "$native_line" "--exercise large-payload"
assert_contains "$native_line" "--exercise stream"
completion_line="$(grep '^completion:' "$FAKE_LOG" | tail -1)"
assert_contains "$completion_line" "completion:auth=1:"
assert_contains "$completion_line" '"model":"scout/code"'
assert_contains "$completion_line" '"max_tokens":1'
[[ "$(grep '^stress:' "$FAKE_LOG" | tail -1)" == "stress:" ]] || fail "stress-fast received live/model-call flags"

export FAKE_COMPLETION_CODE=429
run_pulse live-rate-limit --live --json
assert_eq "$RUN_STATUS" 0
jq -e '.overall == "warn" and any(.checks[]; .name == "gateway-completion" and .level == "warn")' "$receipt" >/dev/null || fail "rate limit was not a live warning"
unset FAKE_COMPLETION_CODE

export FAKE_COMPLETION_EMPTY=1
run_pulse live-empty-completion --live --json
assert_eq "$RUN_STATUS" 1
jq -e 'any(.checks[]; .name == "gateway-completion" and .level == "fail")' "$receipt" >/dev/null || fail "empty 2xx completion must fail"
unset FAKE_COMPLETION_EMPTY

export FAKE_COMPLETION_NULL_CONTENT=1
run_pulse live-null-content-completion --live --json
assert_eq "$RUN_STATUS" 0
jq -e 'any(.checks[]; .name == "gateway-completion" and .level == "ok")' "$receipt" >/dev/null || fail "null content with billed completion tokens must pass"
unset FAKE_COMPLETION_NULL_CONTENT

export FAKE_NATIVE_FAIL=1 FAKE_COMPLETION_CODE=500 FAKE_STRESS_FAIL=1
run_pulse live-failures --live --json
assert_eq "$RUN_STATUS" 1
for name in native-verify gateway-completion stress-fast; do
  jq -e --arg name "$name" 'any(.checks[]; .name == $name and .level == "fail")' "$receipt" >/dev/null || fail "live failure was not folded into: $name"
done
unset FAKE_NATIVE_FAIL FAKE_COMPLETION_CODE FAKE_STRESS_FAIL

export FAKE_ENGINE_DOWN=1
run_pulse engine-down
assert_eq "$RUN_STATUS" 1
validate_receipt "$receipt"
jq -e '.overall == "fail" and any(.checks[]; .name == "engine" and .level == "fail")' "$receipt" >/dev/null || fail "engine-down did not produce a failing receipt"
assert_contains "$(cat "$RUN_OUT")" "fail engine"
non_ok_count="$(jq '[.checks[] | select(.level != "ok")] | length' "$receipt")"
assert_eq "$(wc -l < "$RUN_OUT" | tr -d ' ')" "$(( non_ok_count + 1 ))"
unset FAKE_ENGINE_DOWN

export FAKE_LISTEN_PORTS=""
export FAKE_MODE_D_HEALTH_DOWN=1 FAKE_HEADROOM_DOWN=1 FAKE_SCOUT_DOWN=1 FAKE_MODE_D_AGENT_DOWN=1
export FAKE_BODY_MODE=unavailable FAKE_ROUTE_DOWN="alpha/model beta/model"
run_pulse fast-failures --json
assert_eq "$RUN_STATUS" 1
for name in tap:test-tap mode-d headroom launchagent:scout launchagent:mode-d body lanes:tap lanes:route; do
  jq -e --arg name "$name" 'any(.checks[]; .name == $name and .level == "fail")' "$receipt" >/dev/null || fail "fast failure was not reported by: $name"
done
export FAKE_LISTEN_PORTS="19000 19001 19002 19080 19865"
unset FAKE_MODE_D_HEALTH_DOWN FAKE_HEADROOM_DOWN FAKE_SCOUT_DOWN FAKE_MODE_D_AGENT_DOWN FAKE_BODY_MODE FAKE_ROUTE_DOWN

export FAKE_USAGE_DOWN=1
run_pulse usage-unavailable --json
assert_eq "$RUN_STATUS" 0
jq -e '.overall == "warn" and .summary.fail == 0 and any(.checks[]; .name == "usage-flow" and .level == "warn")' "$receipt" >/dev/null || fail "usage unavailability must warn, never fail"
unset FAKE_USAGE_DOWN

cat > "${SB_LANES}/gamma.env" <<'EOF'
SB_LANE_NAME="gamma"
SB_LANE_KEY_ENV=""
SB_LANE_MODEL="model"
SB_LANE_ANTHROPIC_TAP=""
SB_LANE_ROUTE="gamma/model"
EOF
run_pulse empty-key-env --json
assert_eq "$RUN_STATUS" 0
jq -e 'any(.checks[]; .name == "lane:gamma" and .level == "warn" and (.detail | contains("key[?]=unset, tap=n/a, route=loaded")))' "$receipt" >/dev/null || fail "empty lane key-env shifted health fields"
rm -f "${SB_LANES}/gamma.env"

export SB_GATEWAY="https://not-loopback.invalid"
export HTTP_PROXY="http://not-loopback.invalid:9999"
run_pulse loopback-boundary --json
assert_eq "$RUN_STATUS" 0
grep -Fq "not-loopback.invalid" "$receipt" && fail "pulse persisted or used inherited non-loopback gateway/proxy"
unset SB_GATEWAY HTTP_PROXY

rm -f "$SB_MODE_D_CONFIG"
export FAKE_BODY_MODE=backlog FAKE_USAGE_AGE_HOURS=25
unset TEST_BETA_KEY
print -r -- '{"generated":"2001-01-01","models":[{"verification":{"probes":{"completion":{"latest":{"finished_at":"2000-01-01T00:00:00Z"}}}}}]}' > "$SB_PROVIDER_REGISTRY"
print -r -- "intentional live config difference" > "${HOME}/.config/switchback/switchback.yaml"

run_pulse warn-only --json
assert_eq "$RUN_STATUS" 0
validate_receipt "$receipt"
jq -e '.overall == "warn" and .summary.warn > 0 and .summary.fail == 0' "$receipt" >/dev/null || fail "warn-only pulse had wrong overall/exit"
for name in mode-d body lane:beta registry usage-flow config-drift; do
  jq -e --arg name "$name" 'any(.checks[]; .name == $name and .level == "warn")' "$receipt" >/dev/null || fail "missing warning check: $name"
done
jq -e 'any(.checks[]; .name == "config-drift" and (.detail | contains("intentional runtime drift is possible")))' "$receipt" >/dev/null || fail "config drift warning lacks required wording"
jq -e 'all(.checks[]; .name != "launchagent:mode-d")' "$receipt" >/dev/null || fail "absent Mode D must not check its LaunchAgent"

run_pulse warn-strict --json --strict
assert_eq "$RUN_STATUS" 1
jq -e '.overall == "warn" and .summary.fail == 0' "$receipt" >/dev/null || fail "strict changed warn receipt semantics"

print -r -- "mode d fixture" > "$SB_MODE_D_CONFIG"
unset FAKE_BODY_MODE FAKE_USAGE_AGE_HOURS
export TEST_BETA_KEY="beta-secret-value-must-not-leak"
print -r -- '{"generated":"2998-01-01","models":[{"verification":{"probes":{"completion":{"latest":{"finished_at":"2999-01-01T00:00:00Z"}}}}}]}' > "$SB_PROVIDER_REGISTRY"
cp "${SWITCHBACK_ROOT}/config/generated/switchback.yaml" "${HOME}/.config/switchback/switchback.yaml"

history_before="$(wc -l < "$history_file" | tr -d ' ')"
{
  zsh "$SB" pulse --json </dev/null >"${TMPDIR}/concurrent-a.out" 2>"${TMPDIR}/concurrent-a.err" &
  pid_a=$!
  zsh "$SB" pulse --json </dev/null >"${TMPDIR}/concurrent-b.out" 2>"${TMPDIR}/concurrent-b.err" &
  pid_b=$!
} 2>"${TMPDIR}/concurrent-launch.err"
wait "$pid_a" || fail "first concurrent pulse failed"
wait "$pid_b" || fail "second concurrent pulse failed"
validate_receipt "${TMPDIR}/concurrent-a.out"
validate_receipt "${TMPDIR}/concurrent-b.out"
assert_eq "$(wc -l < "$history_file" | tr -d ' ')" "$(( history_before + 2 ))"

mkdir -p "${SB_STATE}/pulse/.lock"
touch -t 200001010000 "${SB_STATE}/pulse/.lock"
run_pulse stale-lock --json
assert_eq "$RUN_STATUS" 0
[[ ! -d "${SB_STATE}/pulse/.lock" ]] || fail "stale pulse lock was not recovered and released"

mkdir -p "${history_file:h}"
for i in {1..1000}; do print -r -- "{\"old\":${i}}"; done >| "$history_file"
run_pulse history-cap --json
assert_eq "$RUN_STATUS" 0
assert_eq "$(wc -l < "$history_file" | tr -d ' ')" 1000
[[ "$(head -1 "$history_file")" == '{"old":2}' ]] || fail "history cap did not drop the oldest row"
tail -1 "$history_file" | jq -S . > "${TMPDIR}/history-last.json"
jq -S . "$receipt" > "${TMPDIR}/receipt-last.json"
cmp -s "${TMPDIR}/history-last.json" "${TMPDIR}/receipt-last.json" || fail "history did not append the complete latest receipt"
find "${SB_STATE}/pulse" -name 'last.json.tmp.*' -print -quit | grep -q . && fail "atomic receipt temp file was left behind"

for secret in alpha-secret-value-must-not-leak beta-secret-value-must-not-leak scout-secret-value-must-not-leak; do
  grep -R -Fq "$secret" "$TMPDIR" && fail "secret leaked into pulse output/receipt/log: $secret"
done
grep -Eq 'launchctl:(kickstart|bootstrap|bootout)' "$FAKE_LOG" && fail "pulse mutated a LaunchAgent"
grep -Eq '^switchback:(reload|restart):' "$FAKE_LOG" && fail "pulse reloaded or restarted Switchback"
grep -q '^unexpected-url:' "$FAKE_LOG" && fail "pulse called an unexpected URL"
[[ ! -e "${TMPDIR}/pulse-switchback-mutation" ]] || fail "pulse attempted a Switchback mutation"
[[ ! -e "${TMPDIR}/pulse-fzf-used" ]] || fail "pulse invoked fzf"
[[ ! -e "${TMPDIR}/pulse-launchctl-mutation" ]] || fail "pulse attempted a LaunchAgent mutation"
grep -R -Eiq 'enter to continue|press enter|select an option|fzf' "${TMPDIR}"/*.out "${TMPDIR}"/*.err && fail "pulse emitted an interactive prompt"

print "ok - sb pulse"
