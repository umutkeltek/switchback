#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export SWITCHBACK_RUNTIME_ROOT="${HOME}/.config/switchback"
export SB_CONFIG="${HOME}/.config/switchback/switchback.yaml"
export SB_ENV="${HOME}/.config/switchback/sb.env"
export SB_STATE="${HOME}/.config/switchback/state"
export CODEX_PROFILES="${HOME}/.config/switchback/codex"
export CLAUDE_PROFILES="${HOME}/.config/switchback/claude"
export SB_AUTHREG="${HOME}/.config/switchback/codex-auth"
export SB_LAUNCH_PROFILES="${HOME}/.config/switchback/launch-profiles.json"
export SB_PROFILE_PROJECTION_ROOT="${HOME}/.config/switchback/state/profile-conformance"
export SB_LANES="${HOME}/.config/switchback/lanes"
export SB_TEST_ROUTES="${TMPDIR}/routes.json"
export SB_TEST_SET_LOG="${TMPDIR}/config-set.log"
export SB_TEST_RELOAD_LOG="${TMPDIR}/reload.log"
export PATH="${TMPDIR}/bin:${PATH}"

mkdir -p "${HOME}/.config/switchback" "${TMPDIR}/bin"
: > "$SB_TEST_SET_LOG"
: > "$SB_TEST_RELOAD_LOG"

jq -n '[
  range(0; 4000) as $index |
  if $index == 21 then
    {name: "local-mac-code", match: {model: "local/mac-code"}, targets: ["mac/old-code"], sibling: {kept: true}}
  elif $index == 22 then
    {name: "local-mac-fast", match: {model: "local/mac-fast"}, targets: ["mac/old-fast"], sibling: {kept: true}}
  else
    {name: ("route-" + ($index | tostring)), match: {model: ("route/" + ($index | tostring))}, targets: ["mac/model"]}
  end
]' > "$SB_TEST_ROUTES"

cat > "${TMPDIR}/bin/switchback" <<'FAKE'
#!/bin/zsh
set -euo pipefail

[[ "${1:-}" == "config" ]] || exit 2
case "${2:-}" in
get)
  pointer="${3:-}"
  case "$pointer" in
  providers)
    print -r -- '[{"id":"mac","type":"openai_compatible","base_url":"http://127.0.0.1:1234/v1"}]'
    ;;
  routes)
    cat "$SB_TEST_ROUTES"
    ;;
  routes.*.targets)
    index="${pointer#routes.}"
    index="${index%.targets}"
    jq ".[$index].targets" "$SB_TEST_ROUTES"
    ;;
  *) exit 2 ;;
  esac
  ;;
set)
  pointer="${3:-}"
  value="${4:-}"
  [[ "$pointer" == routes.*.targets ]] || exit 3
  print -r -- "${pointer}"$'\t'"${value}" >> "$SB_TEST_SET_LOG"
  index="${pointer#routes.}"
  index="${index%.targets}"
  jq --argjson targets "$value" ".[$index].targets = \$targets" "$SB_TEST_ROUTES" > "${SB_TEST_ROUTES}.new"
  mv "${SB_TEST_ROUTES}.new" "$SB_TEST_ROUTES"
  print -r -- '{"ok":true}'
  ;;
validate)
  print -r -- "unexpected config validate" >> "$SB_TEST_SET_LOG"
  exit 4
  ;;
*) exit 2 ;;
esac
FAKE
chmod +x "${TMPDIR}/bin/switchback"

cat > "${TMPDIR}/bin/lms" <<'FAKE'
#!/bin/zsh
set -euo pipefail
[[ "${1:-}" == "ps" ]] || exit 2
cat <<'TABLE'
IDENTIFIER MODEL STATUS SIZE CONTEXT PARALLEL DEVICE TTL
served-new served-new IDLE 10 GB 131072 1 Local -
served-fast served-fast IDLE 4 GB 32768 1 Local -
TABLE
FAKE
chmod +x "${TMPDIR}/bin/lms"

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle\nactual:\n$haystack"
}

SB_SOURCE_ONLY=1 source "$SB"
sb_reload() {
  print -r -- reload >> "$SB_TEST_RELOAD_LOG"
}

before_siblings="$(jq -c 'del(.[21])' "$SB_TEST_ROUTES")"
out="$(_local_use code served-new)"
assert_contains "$out" "local/mac-code"
assert_contains "$out" "previous target for rollback: mac/old-code"
[[ "$(jq -r '.[21].targets == ["mac/served-new"]' "$SB_TEST_ROUTES")" == true ]] || fail "selected target was not updated"
[[ "$(jq -c 'del(.[21])' "$SB_TEST_ROUTES")" == "$before_siblings" ]] || fail "sibling routes changed"
[[ "$(jq -c '.[21] | del(.targets)' "$SB_TEST_ROUTES")" == '{"name":"local-mac-code","match":{"model":"local/mac-code"},"sibling":{"kept":true}}' ]] || fail "selected route fields other than targets changed"

[[ "$(wc -l < "$SB_TEST_SET_LOG" | tr -d ' ')" == 1 ]] || fail "expected one targeted config set"
set_call="$(cat "$SB_TEST_SET_LOG")"
[[ "$set_call" == $'routes.21.targets\t["mac/served-new"]' ]] || fail "unexpected config set argv: $set_call"
(( ${#set_call} < 100 )) || fail "config set argv contains the route registry"
[[ "$set_call" != *"route-3999"* ]] || fail "whole route array leaked into argv"

idempotent="$(_local_use code served-new)"
assert_contains "$idempotent" "already -> mac/served-new"
[[ "$(wc -l < "$SB_TEST_SET_LOG" | tr -d ' ')" == 1 ]] || fail "idempotent use wrote config"

if _local_use code not-loaded >/dev/null 2>&1; then
  fail "unloaded identifier was accepted"
fi
if _local_use unknown served-new >/dev/null 2>&1; then
  fail "unknown slot was accepted"
fi
[[ "$(wc -l < "$SB_TEST_SET_LOG" | tr -d ' ')" == 1 ]] || fail "refused inputs wrote config"

reload_out="$(_local_use fast served-fast --reload)"
assert_contains "$reload_out" "previous target for rollback: mac/old-fast"
[[ "$(cat "$SB_TEST_RELOAD_LOG")" == reload ]] || fail "--reload did not reload"
[[ "$(wc -l < "$SB_TEST_SET_LOG" | tr -d ' ')" == 2 ]] || fail "reload path did not perform one targeted set"
[[ "$(jq -r '.[22].targets == ["mac/served-fast"]' "$SB_TEST_ROUTES")" == true ]] || fail "fast target was not updated"
[[ "$(cat "$SB_TEST_SET_LOG")" != *"unexpected config validate"* ]] || fail "local use ran a redundant validation command"

print "ok - sb local use"
