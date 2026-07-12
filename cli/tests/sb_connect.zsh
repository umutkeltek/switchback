#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export PATH="${TMPDIR}/bin:${PATH}"
export SB_BIN="${TMPDIR}/bin/switchback"
export SB_LANES="${HOME}/.config/switchback/lanes"
export FAKE_STATE="${TMPDIR}/state"
export FAKE_LOG="${TMPDIR}/operations.log"
export FAKE_RELOAD_MARKER="${TMPDIR}/reload-called"
export FAKE_RELOAD_AUTH_MARKER="${TMPDIR}/reload-auth-seen"
export FAKE_COMPLETION_MARKER="${TMPDIR}/completion-called"
export FAKE_GATEWAY_COMPLETION_MARKER="${TMPDIR}/gateway-completion-called"
export FAKE_ROUTE_AUTH_MARKER="${TMPDIR}/route-auth-seen"
export ZAI_API_KEY="test-only"

mkdir -p "${TMPDIR}/bin" "$SB_LANES" "$FAKE_STATE"
: > "$FAKE_LOG"

cat > "$SB_BIN" <<'FAKE'
#!/bin/zsh
set -euo pipefail

if [[ "${1:-}" == "--json" && "${2:-}" == "native" && "${3:-}" == "status" ]]; then
  print -r -- '{"clients":[],"warnings":[]}'
  exit 0
fi

if [[ "${1:-}" == "config" && "${2:-}" == "get" ]]; then
  case "${3:-}" in
    server.taps)
      [[ -f "${FAKE_STATE}/taps.json" ]] && cat "${FAKE_STATE}/taps.json" || print -r -- '[]'
      ;;
    providers)
      if [[ -f "${FAKE_STATE}/provider-added" ]]; then
        print -r -- '[{"id":"zai-coding","base_url":"https://api.z.ai/api/coding/paas/v4"}]'
      else
        print -r -- '[]'
      fi
      ;;
    routes)
      if [[ -f "${FAKE_STATE}/provider-added" ]]; then
        print -r -- '[{"match":{"model":"zai/glm-5.2"},"targets":["zai-coding/glm-5.2"]}]'
      else
        print -r -- '[]'
      fi
      ;;
    *) print -r -- '[]' ;;
  esac
  exit 0
fi

if [[ "${1:-}" == "config" && "${2:-}" == "set" && "${3:-}" == "server.taps" ]]; then
  print -r -- "${4:-[]}" > "${FAKE_STATE}/taps.json"
  print -r -- "config-set-taps" >> "$FAKE_LOG"
  exit 0
fi

if [[ "${1:-}" == "provider" && "${2:-}" == "add" ]]; then
  touch "${FAKE_STATE}/provider-added"
  print -r -- "provider-add" >> "$FAKE_LOG"
  exit 0
fi

exit 0
FAKE
chmod +x "$SB_BIN"

cat > "${TMPDIR}/bin/curl" <<'FAKE'
#!/bin/zsh
set -euo pipefail

case "$*" in
  *"/v1/reload"*)
    [[ "$*" == *"authorization: Bearer "* ]] && touch "$FAKE_RELOAD_AUTH_MARKER"
    touch "$FAKE_RELOAD_MARKER"
    print -r -- '{"ok":true,"revision":2}'
    ;;
  *"/cp/v1/route-preview"*)
    [[ "$*" == *"authorization: Bearer "* ]] && touch "$FAKE_ROUTE_AUTH_MARKER"
    print -r -- '{"decision":{"selected":{"target_id":"'"${FAKE_ROUTE_TARGET:-other/model}"'"},"reason":[]}}'
    ;;
  *"/v1/chat/completions"*|*"/v1/responses"*|*"/v1/messages"*)
    touch "$FAKE_COMPLETION_MARKER"
    [[ "$*" == *"http://127.0.0.1:18765/v1/"* ]] && touch "$FAKE_GATEWAY_COMPLETION_MARKER"
    args=("$@")
    output_file=""
    for (( i = 1; i <= ${#args[@]}; i++ )); do
      [[ "${args[i]}" == "-o" ]] && output_file="${args[i + 1]}"
    done
    if [[ -n "$output_file" ]]; then
      print -r -- '{"choices":[{"message":{"content":"ok"}}]}' > "$output_file"
      print -r -- "${FAKE_COMPLETION_CODE:-200}"
    else
      print -r -- '{"choices":[{"message":{"content":"ok"}}]}'
    fi
    ;;
  *"/v1/models"*) print -r -- '{"data":[{"id":"scout/code"}]}' ;;
  *"/health"*) print -r -- '{"ok":true}' ;;
esac
exit 0
FAKE
chmod +x "${TMPDIR}/bin/curl"

cat > "${TMPDIR}/bin/sb" <<'FAKE'
#!/bin/zsh
touch "$FAKE_RELOAD_MARKER"
exit 0
FAKE
chmod +x "${TMPDIR}/bin/sb"

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle\nactual:\n$haystack"
}

assert_not_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" != *"$needle"* ]] || fail "expected output not to contain: $needle\nactual:\n$haystack"
}

assert_file() { [[ -f "$1" ]] || fail "expected file: $1"; }
assert_executable() { [[ -x "$1" ]] || fail "expected executable: $1"; }

file_mode() {
  if stat -f '%Lp' "$1" >/dev/null 2>&1; then
    stat -f '%Lp' "$1"
  else
    stat -c '%a' "$1"
  fi
}

mtime() {
  if stat -f '%m' "$1" >/dev/null 2>&1; then
    stat -f '%m' "$1"
  else
    stat -c '%Y' "$1"
  fi
}

operation_count() {
  awk -v wanted="$1" '$0 == wanted { count++ } END { print count + 0 }' "$FAKE_LOG"
}

run_connect() {
  zsh "$SB" connect zai --alias claudex --no-verify --yes </dev/null 2>&1
}

first_status=0
first_output="$(run_connect)" || first_status=$?
(( first_status == 0 )) || fail "initial connect failed:\n${first_output}"
lane_file="${SB_LANES}/zai.env"
bin_dir="${HOME}/.local/bin"

assert_file "$lane_file"
[[ "$(file_mode "$lane_file")" == "600" ]] || fail "lane file mode must be 600"
assert_contains "$(cat "$lane_file")" 'SB_LANE_ANTHROPIC_URL="https://api.z.ai/api/anthropic"'
assert_contains "$(cat "$lane_file")" 'SB_LANE_OPENAI_URL="https://api.z.ai/api/coding/paas/v4"'
assert_contains "$(cat "$lane_file")" 'SB_LANE_KEY_ENV="ZAI_API_KEY"'
assert_contains "$(cat "$lane_file")" 'SB_LANE_MODEL="glm-5.2"'
assert_contains "$(cat "$lane_file")" 'SB_LANE_ANTHROPIC_TAP="18772"'
assert_contains "$(cat "$lane_file")" 'SB_LANE_ROUTE="zai/glm-5.2"'

assert_contains "$first_output" 'key already set via $ZAI_API_KEY — keeping it'
assert_contains "$first_output" "lane: zai"
assert_contains "$first_output" "https://api.z.ai/api/anthropic"
assert_contains "$first_output" "https://api.z.ai/api/coding/paas/v4"
assert_contains "$first_output" "model: glm-5.2"
assert_contains "$first_output" 'key: $ZAI_API_KEY · environment only'
assert_contains "$first_output" "verbatim Anthropic tap"
assert_contains "$first_output" "(Responses→Chat)"
assert_contains "$first_output" "verification: skipped"
assert_contains "$first_output" "claudex"
assert_contains "$first_output" "sb lane list"
assert_contains "$first_output" "sb usage"
assert_contains "$first_output" "sb registry costs zai"
assert_contains "$first_output" "sb reload"
assert_not_contains "$first_output" "[y/N]"
assert_not_contains "$first_output" "$ZAI_API_KEY"
[[ ! -e "$FAKE_RELOAD_MARKER" ]] || fail "non-TTY connect must never reload"
[[ ! -e "$FAKE_COMPLETION_MARKER" ]] || fail "--no-verify must skip completion probes"
[[ -e "$FAKE_ROUTE_AUTH_MARKER" ]] || fail "route preview must carry gateway authorization"

assert_executable "${bin_dir}/codex-zai"
assert_executable "${bin_dir}/claude-zai"
assert_executable "${bin_dir}/claudex"
assert_contains "$(cat "${bin_dir}/claudex")" 'exec sb run claude --with zai "$@"'

first_mtime="$(mtime "$lane_file")"
tap_writes="$(operation_count config-set-taps)"
provider_writes="$(operation_count provider-add)"
sleep 1
second_output="$(run_connect)"
assert_contains "$second_output" "lane already configured, unchanged"
[[ "$(mtime "$lane_file")" == "$first_mtime" ]] || fail "idempotent connect rewrote the lane file"
[[ "$(operation_count config-set-taps)" == "$tap_writes" ]] || fail "idempotent connect rewrote taps"
[[ "$(operation_count provider-add)" == "$provider_writes" ]] || fail "idempotent connect rewrote provider config"

enhanced_tmp="${lane_file}.enhanced"
awk '
  /^SB_LANE_ANTHROPIC_URL=/ { print "SB_LANE_ANTHROPIC_URL=\"http://127.0.0.1:8787 -> https://api.z.ai/api/anthropic\""; next }
  /^SB_LANE_OPENAI_URL=/ { print "SB_LANE_OPENAI_URL=\"http://127.0.0.1:8787/v1 -> https://api.z.ai/api/coding/paas/v4\""; next }
  { print }
' "$lane_file" > "$enhanced_tmp"
print -r -- 'SB_LANE_HEADROOM="1"' >> "$enhanced_tmp"
mv "$enhanced_tmp" "$lane_file"
chmod 600 "$lane_file"
enhanced_mtime="$(mtime "$lane_file")"
sleep 1
enhanced_output="$(run_connect)"
assert_contains "$enhanced_output" "lane already configured, unchanged"
assert_contains "$enhanced_output" "http://127.0.0.1:8787 -> https://api.z.ai/api/anthropic"
assert_contains "$(cat "$lane_file")" 'SB_LANE_HEADROOM="1"'
[[ "$(mtime "$lane_file")" == "$enhanced_mtime" ]] || fail "preset reconnect rewrote an enhanced lane"
[[ "$(operation_count config-set-taps)" == "$tap_writes" ]] || fail "preset reconnect rewrote enhanced-lane taps"
[[ "$(operation_count provider-add)" == "$provider_writes" ]] || fail "preset reconnect rewrote enhanced-lane provider config"

tampered_tmp="${lane_file}.tampered"
awk '
  /^SB_LANE_OPENAI_URL=/ { print "SB_LANE_OPENAI_URL=\"http://127.0.0.1:8787/v1 -> https://wrong.invalid/v1\""; next }
  { print }
' "$lane_file" > "$tampered_tmp"
mv "$tampered_tmp" "$lane_file"
tampered_output="$(run_connect)"
assert_not_contains "$tampered_output" "lane already configured, unchanged"
assert_contains "$(cat "$lane_file")" 'SB_LANE_OPENAI_URL="https://api.z.ai/api/coding/paas/v4"'

missing_status=0
missing_output="$(zsh "$SB" connect zai --alias </dev/null 2>&1)" || missing_status=$?
(( missing_status != 0 )) || fail "missing --alias value must fail"
assert_contains "$missing_output" "missing value for --alias"

picker_status=0
picker_output="$(
  (
    export SB_SOURCE_ONLY=1
    source "$SB"
    FZF=""
    _connect_interactive() { return 0; }
    sb_connect picker --no-verify <<< $'\nhttps://api.picker.invalid/v1\nZAI_API_KEY\n1\nn'
  )
)" || picker_status=$?
(( picker_status == 0 )) || fail "plain model picker connect failed:\n${picker_output}"
assert_contains "$(cat "${SB_LANES}/picker.env")" 'SB_LANE_MODEL="scout/code"'

rm -f "$FAKE_RELOAD_MARKER"
interactive_status=0
interactive_output="$(
  (
    export SB_SOURCE_ONLY=1
    source "$SB"
    _connect_interactive() { return 0; }
    sb_reload() { touch "$FAKE_RELOAD_MARKER"; return 0; }
    sb_connect zai --alias claudex --no-verify <<< "n"
  )
)" || interactive_status=$?
(( interactive_status == 0 )) || fail "interactive connect with reload declined failed"
assert_contains "$interactive_output" 'run `sb reload` now? [y/N]'
[[ ! -e "$FAKE_RELOAD_MARKER" ]] || fail "interactive connect reloaded before or after a no answer"

rm -f "$FAKE_RELOAD_MARKER" "$FAKE_RELOAD_AUTH_MARKER"
(
  export SB_SOURCE_ONLY=1
  source "$SB"
  sb_reload >/dev/null
)
[[ -e "$FAKE_RELOAD_MARKER" ]] || fail "sandboxed sb_reload did not reach the reload endpoint"
[[ -e "$FAKE_RELOAD_AUTH_MARKER" ]] || fail "sb_reload must carry gateway authorization"

unsafe_status=0
unsafe_output="$(zsh "$SB" connect unsafe --openai-url https://user@example.invalid/v1 --key-env ZAI_API_KEY --model safe-model --no-verify </dev/null 2>&1)" || unsafe_status=$?
(( unsafe_status != 0 )) || fail "credential-bearing provider URL must be rejected"
assert_contains "$unsafe_output" "invalid provider endpoint"
assert_not_contains "$unsafe_output" "user@example.invalid"

custom_output="$(zsh "$SB" connect custom --openai-url https://api.custom.invalid/v1 --key-env ZAI_API_KEY --model custom-model --alias customx --agent both --no-verify </dev/null 2>&1)"
assert_contains "$custom_output" "provider 'custom' resolved from custom flags"
assert_contains "$custom_output" "customx-claude, customx-codex"
assert_contains "$(cat "${SB_LANES}/custom.env")" 'SB_LANE_OPENAI_URL="https://api.custom.invalid/v1"'
assert_contains "$(cat "${SB_LANES}/custom.env")" 'SB_LANE_MODEL="custom-model"'
assert_executable "${bin_dir}/customx-claude"
assert_executable "${bin_dir}/customx-codex"
assert_contains "$(cat "${bin_dir}/customx-claude")" 'exec sb run claude --with custom "$@"'
assert_contains "$(cat "${bin_dir}/customx-codex")" 'exec sb run codex --with custom "$@"'

rm -f "$FAKE_COMPLETION_MARKER" "$FAKE_GATEWAY_COMPLETION_MARKER"
doctor_ok_status=0
doctor_ok_output="$(FAKE_ROUTE_TARGET="custom/custom-model" FAKE_COMPLETION_CODE=200 zsh "$SB" doctor custom 2>&1)" || doctor_ok_status=$?
(( doctor_ok_status == 0 )) || fail "loaded lane doctor failed:\n${doctor_ok_output}"
assert_contains "$doctor_ok_output" "route: loaded"
assert_contains "$doctor_ok_output" "1-token completion: ok"
[[ -e "$FAKE_COMPLETION_MARKER" ]] || fail "loaded route doctor did not run the completion probe"
[[ -e "$FAKE_GATEWAY_COMPLETION_MARKER" ]] || fail "loaded route doctor bypassed the gateway"

doctor_auth_status=0
doctor_auth_output="$(FAKE_ROUTE_TARGET="custom/custom-model" FAKE_COMPLETION_CODE=401 zsh "$SB" doctor custom 2>&1)" || doctor_auth_status=$?
(( doctor_auth_status != 0 )) || fail "HTTP 401 lane doctor must fail"
assert_contains "$doctor_auth_output" "completion: auth error (HTTP 401)"

foreign="${bin_dir}/foreign-alias"
print -r -- $'#!/bin/zsh\nprint "foreign"\n' > "$foreign"
chmod +x "$foreign"
foreign_before="$(cksum "$foreign")"
foreign_status=0
foreign_output="$(zsh "$SB" connect zai --alias foreign-alias --no-verify </dev/null 2>&1)" || foreign_status=$?
(( foreign_status != 0 )) || fail "connect overwrote a foreign alias"
assert_contains "$foreign_output" "refusing to overwrite"
[[ "$(cksum "$foreign")" == "$foreign_before" ]] || fail "foreign alias content changed"

mkdir -p "${HOME}/.config/switchback"
print -r -- 'export EXISTING_SETTING="kept"' > "${HOME}/.config/switchback/sb.env"
chmod 644 "${HOME}/.config/switchback/sb.env"
(
  export SB_SOURCE_ONLY=1
  source "$SB"
  _set_env CONNECT_TEST_VALUE "test-only-value"
)
[[ "$(file_mode "${HOME}/.config/switchback/sb.env")" == "600" ]] || fail "_set_env must enforce mode 600"
injection_marker="${FAKE_STATE}/set-env-injection"
literal_value='literal $HOME $(touch "'"${injection_marker}"'")'
(
  export SB_SOURCE_ONLY=1
  source "$SB"
  _set_env CONNECT_TEST_VALUE "$literal_value"
)
persisted_value="$(zsh -c 'source "$1"; print -r -- "$CONNECT_TEST_VALUE"' _ "${HOME}/.config/switchback/sb.env")"
[[ "$persisted_value" == "$literal_value" ]] || fail "_set_env changed a literal value while persisting it"
[[ ! -e "$injection_marker" ]] || fail "_set_env allowed shell expansion from a persisted value"

bad_env_path="${FAKE_STATE}/sb-env-directory"
mkdir -p "$bad_env_path"
bad_env_status=0
(
  export SB_SOURCE_ONLY=1
  source "$SB"
  SB_ENV="$bad_env_path"
  _set_env CONNECT_TEST_VALUE "test-placeholder-not-a-secret"
) >/dev/null 2>&1 || bad_env_status=$?
chmod 700 "$bad_env_path"
(( bad_env_status != 0 )) || fail "_set_env must report a persistence failure"

chmod_failure_env="${FAKE_STATE}/chmod-failure.env"
print -r -- 'export EXISTING_SETTING="kept"' > "$chmod_failure_env"
chmod 644 "$chmod_failure_env"
chmod_failure_status=0
(
  export SB_SOURCE_ONLY=1
  source "$SB"
  SB_ENV="$chmod_failure_env"
  chmod() { return 1; }
  _set_env CONNECT_MUST_NOT_WRITE "test-only"
) >/dev/null 2>&1 || chmod_failure_status=$?
(( chmod_failure_status != 0 )) || fail "_set_env must fail when sb.env cannot be secured"
grep -q '^export CONNECT_MUST_NOT_WRITE=' "$chmod_failure_env" && fail "_set_env wrote before securing sb.env"

store_failure_status=0
(
  export SB_SOURCE_ONLY=1
  source "$SB"
  _set_env() { return 1; }
  print -r -- "test-placeholder-not-a-secret" | _lane_store_key failed FAIL_KEY 1 0 1
) >/dev/null 2>&1 || store_failure_status=$?
(( store_failure_status != 0 )) || fail "_lane_store_key must propagate _set_env failure"

doctor_output="$(zsh "$SB" doctor 2>&1)"
assert_contains "$doctor_output" "lanes:"
assert_contains "$doctor_output" "zai: key=set"
assert_contains "$doctor_output" "tap="
assert_contains "$doctor_output" "route=not-loaded"

print "ok - sb connect"
