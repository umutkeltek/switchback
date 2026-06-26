#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export PATH="${TMPDIR}/bin:${PATH}"
export FAKE_SET_LOG="${TMPDIR}/capture-set.json"
export FAKE_TAPS='[{"id":"claude-tap","capture_bodies":true},{"id":"codex-tap","capture_bodies":false}]'
mkdir -p "${HOME}/.config/switchback" "${TMPDIR}/bin"

cat > "${TMPDIR}/bin/switchback" <<'FAKE'
#!/bin/zsh
set -euo pipefail

case "${1:-}" in
  config)
    case "${2:-}" in
      get)
        [[ "${3:-}" == "server.taps" ]] || exit 2
        print -r -- "${FAKE_TAPS}"
        ;;
      set)
        [[ "${3:-}" == "server.taps" ]] || exit 2
        print -r -- "${4:-}" > "${FAKE_SET_LOG}"
        print -r -- "${4:-}" | jq -e 'type == "array" and all(.[]; (.capture_bodies | type) == "boolean")' >/dev/null
        ;;
      validate)
        print -r -- '{"ok":true}'
        ;;
      *)
        exit 2
        ;;
    esac
    ;;
  *)
    exit 2
    ;;
esac
FAKE
chmod +x "${TMPDIR}/bin/switchback"

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle\nactual:\n$haystack"
}

SB_SOURCE_ONLY=1 source "$SB"

[[ "$(_tap_capture_state)" == "mixed" ]] || fail "mixed capture state not detected"

out="$(_set_tap_capture_bodies off)"
assert_contains "$out" "not reloaded"
jq -e 'all(.[]; .capture_bodies == false)' "$FAKE_SET_LOG" >/dev/null || fail "capture off did not set every tap false"

FAKE_TAPS='[{"id":"bad","capture_bodies":"truetrue"}]'
rm -f "$FAKE_SET_LOG"
[[ "$(_tap_capture_state)" == "invalid" ]] || fail "invalid capture state not detected"

rc=0
err="$(_set_tap_capture_bodies on 2>&1 >/tmp/sb-capture-invalid.out)" || rc=$?
[[ "$rc" -ne 0 ]] || fail "invalid capture edit should fail"
[[ ! -f "$FAKE_SET_LOG" ]] || fail "invalid capture edit should not call config set"
assert_contains "$err" "Refusing capture edit"
