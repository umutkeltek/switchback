#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export PATH="${TMPDIR}/bin:${PATH}"
export FAKE_CODEX_LOG="${TMPDIR}/codex.log"
export FAKE_CODEX_STARTED="${TMPDIR}/codex.started"
export FAKE_CODEX_RELEASE="${TMPDIR}/codex.release"

mkdir -p \
  "${HOME}/.codex" \
  "${HOME}/.config/switchback/codex/work" \
  "${HOME}/.local/bin" \
  "${TMPDIR}/bin"

print -r -- '{"account":"default"}' > "${HOME}/.codex/auth.json"
print -r -- '{"account":"work"}' > "${HOME}/.config/switchback/codex/work/auth.json"

cat > "${HOME}/.local/bin/codex-switchback-tap" <<'FAKE'
#!/bin/zsh
set -euo pipefail
print -r -- "CODEX_HOME=${CODEX_HOME:-} ARGS=$*" >> "$FAKE_CODEX_LOG"
if [[ "${1:-}" == "hold" ]]; then
  touch "$FAKE_CODEX_STARTED"
  while [[ ! -f "$FAKE_CODEX_RELEASE" ]]; do sleep 0.05; done
fi
FAKE
chmod +x "${HOME}/.local/bin/codex-switchback-tap"

cat > "${TMPDIR}/bin/codex" <<'FAKE'
#!/bin/zsh
set -euo pipefail
print -r -- "codex $*" >> "$FAKE_CODEX_LOG"
FAKE
chmod +x "${TMPDIR}/bin/codex"

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle\nactual:\n$haystack"
}

zsh "$SB" codex --mode tap --account work --sessions shared hold \
  >"${TMPDIR}/first.out" 2>"${TMPDIR}/first.err" &
first_pid=$!

for _ in {1..100}; do
  [[ -f "$FAKE_CODEX_STARTED" ]] && break
  sleep 0.05
done
[[ -f "$FAKE_CODEX_STARTED" ]] || fail "first shared Codex run did not start"

zsh "$SB" sessions status >"${TMPDIR}/status.out" 2>"${TMPDIR}/status.err"
assert_contains "$(cat "${TMPDIR}/status.out")" "active shared runs: 1"
assert_contains "$(cat "${TMPDIR}/status.out")" "work (pid"

conflict_status=0
zsh "$SB" codex --mode tap --account default --sessions shared once \
  >"${TMPDIR}/conflict.out" 2>"${TMPDIR}/conflict.err" || conflict_status=$?
[[ "$conflict_status" -ne 0 ]] || fail "different-account shared launch should fail while work is active"
assert_contains "$(cat "${TMPDIR}/conflict.err")" "Shared Codex pool is already active as 'work'"
assert_contains "$(cat "${TMPDIR}/conflict.err")" "--sessions separated"

login_status=0
zsh "$SB" login codex --account default \
  >"${TMPDIR}/login-conflict.out" 2>"${TMPDIR}/login-conflict.err" || login_status=$?
[[ "$login_status" -ne 0 ]] || fail "default login should fail while a shared run is active"
assert_contains "$(cat "${TMPDIR}/login-conflict.err")" "Cannot login:default while shared Codex pool is active as 'work'"

zsh "$SB" codex --mode tap --account work --sessions shared once \
  >"${TMPDIR}/same.out" 2>"${TMPDIR}/same.err"
assert_contains "$(cat "$FAKE_CODEX_LOG")" "ARGS=once"

zsh "$SB" codex --mode tap --account work implied \
  >"${TMPDIR}/implied.out" 2>"${TMPDIR}/implied.err"
assert_contains "$(cat "${TMPDIR}/implied.err")" "auto-separated sessions"
assert_contains "$(cat "$FAKE_CODEX_LOG")" "CODEX_HOME=${HOME}/.config/switchback/codex/work ARGS=implied"

SB_DEFAULT_ACCOUNT=work zsh "$SB" codex --mode tap implied-default \
  >"${TMPDIR}/implied-default.out" 2>"${TMPDIR}/implied-default.err"
assert_contains "$(cat "${TMPDIR}/implied-default.err")" "auto-separated sessions"
assert_contains "$(cat "$FAKE_CODEX_LOG")" "CODEX_HOME=${HOME}/.config/switchback/codex/work ARGS=implied-default"

invalid_status=0
zsh "$SB" codex --mode tap --account "../bad" --sessions separated once \
  >"${TMPDIR}/invalid.out" 2>"${TMPDIR}/invalid.err" || invalid_status=$?
[[ "$invalid_status" -ne 0 ]] || fail "path-like account name should fail"
assert_contains "$(cat "${TMPDIR}/invalid.err")" "invalid Codex account name"
[[ ! -e "${HOME}/.config/switchback/codex/../bad" ]] || fail "invalid account path should not be created"

touch "$FAKE_CODEX_RELEASE"
wait "$first_pid"

zsh "$SB" sessions reset >"${TMPDIR}/reset.out" 2>"${TMPDIR}/reset.err"
assert_contains "$(cat "${TMPDIR}/reset.out")" "restored to the default account"

print "ok - sb Codex shared session account guard"
