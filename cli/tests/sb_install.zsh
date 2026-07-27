#!/bin/zsh
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
INSTALLER="${CLI_ROOT}/install.sh"
export HOME="${TMPDIR}/home"
export SWITCHBACK_ROOT="${TMPDIR}/checkout"
export SWITCHBACK_RUNTIME_ROOT="${SWITCHBACK_ROOT}/.switchback"
export PREFIX="${HOME}/bin"
export SB_BIN="${TMPDIR}/fake-switchback"
mkdir -p "$HOME" "$SWITCHBACK_ROOT/config"

cat > "$SB_BIN" <<'FAKE'
#!/bin/zsh
set -eu
[[ "$*" == *"setup --root"* ]] || { print -u2 "unexpected setup invocation: $*"; exit 2; }
root="${@: -1}"
mkdir -p "$root"/{config,state/body,eval,receipts,bin,backups}
print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$root/manifest.json"
FAKE
chmod +x "$SB_BIN"

"$INSTALLER" >"${TMPDIR}/install.out" 2>"${TMPDIR}/install.err"

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
assert_file() { [[ -f "$1" ]] || fail "missing file: $1"; }
assert_link() { [[ -L "$1" ]] || fail "missing link: $1"; }
assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected '$needle' in:\n$haystack"
}

assert_file "$SWITCHBACK_RUNTIME_ROOT/manifest.json"
assert_file "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml"
assert_file "$SWITCHBACK_RUNTIME_ROOT/config/sb.env"
assert_file "$SWITCHBACK_RUNTIME_ROOT/bin/switchback"
assert_link "$PREFIX/switchback"
assert_link "$PREFIX/sb"
assert_link "$HOME/.config/switchback"
assert_contains "$(cat "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")" "$SWITCHBACK_RUNTIME_ROOT/state/scout.sqlite"
assert_contains "$(cat "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")" "$SWITCHBACK_RUNTIME_ROOT/state/traces.jsonl"
[[ "$(stat -f '%Lp' "$SWITCHBACK_RUNTIME_ROOT/config/sb.env")" == "600" ]] || fail "sb.env is not 0600"
[[ "$(stat -f '%Lp' "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")" == "600" ]] || fail "config is not 0600"

before="$(shasum -a 256 "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")"
"$INSTALLER" >"${TMPDIR}/install-second.out" 2>"${TMPDIR}/install-second.err"
after="$(shasum -a 256 "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")"
[[ "$before" == "$after" ]] || fail "idempotent install rewrote config"
assert_contains "$(cat "${TMPDIR}/install-second.out")" "kept existing $SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml"

print "ok - install owns one runtime root"
