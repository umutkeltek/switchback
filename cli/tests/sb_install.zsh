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
export SB_BUILD_COMMIT="0123456789abcdef0123456789abcdef01234567"
export FAKE_SETUP_LOG="${TMPDIR}/setup-calls.log"
mkdir -p "$HOME" "$SWITCHBACK_ROOT/config"

cat > "$SB_BIN" <<'FAKE'
#!/bin/zsh
set -euo pipefail
if [[ "$*" == "--version" ]]; then
  print -r -- "switchback 0.1.0-test"
  exit 0
fi
if [[ "$*" == *"setup --root"* ]]; then
  print -r -- "$*" >> "${FAKE_SETUP_LOG:?FAKE_SETUP_LOG is required}"
  root="${@: -1}"
  mkdir -p "$root"/{config,state/body,eval,receipts,bin,backups}
  print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$root/manifest.json"
  exit 0
fi
{
  print -r -- "runtime=${SWITCHBACK_RUNTIME_ROOT:-}"
  print -r -- "runtime_alias=${SB_RUNTIME_ROOT:-}"
  print -r -- "runtime_env=${SB_LAUNCHER_SENTINEL:-}"
  print -r -- "legacy_env=${SB_LEGACY_SENTINEL:-}"
  print -r -- "cwd=$PWD"
  print -r -- "args=$*"
} > "${FAKE_LOG:?FAKE_LOG is required}"
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
file_mode() {
  if stat -c '%a' "$1" >/dev/null 2>&1; then
    stat -c '%a' "$1"
  else
    stat -f '%Lp' "$1"
  fi
}

assert_file "$SWITCHBACK_RUNTIME_ROOT/manifest.json"
assert_file "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml"
assert_file "$SWITCHBACK_RUNTIME_ROOT/config/sb.env"
assert_file "$SWITCHBACK_RUNTIME_ROOT/bin/switchback"
assert_file "$SWITCHBACK_RUNTIME_ROOT/bin/switchback-bin"
assert_file "$SWITCHBACK_RUNTIME_ROOT/bin/install-provenance.json"
assert_link "$PREFIX/switchback"
assert_link "$PREFIX/sb"
assert_link "$HOME/.config/switchback"
assert_contains "$(cat "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")" "$SWITCHBACK_RUNTIME_ROOT/state/scout.sqlite"
assert_contains "$(cat "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")" "$SWITCHBACK_RUNTIME_ROOT/state/traces.jsonl"
[[ "$(file_mode "$SWITCHBACK_RUNTIME_ROOT/config/sb.env")" == "600" ]] || fail "sb.env is not 0600"
[[ "$(file_mode "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")" == "600" ]] || fail "config is not 0600"
[[ "$(file_mode "$SWITCHBACK_RUNTIME_ROOT/bin/install-provenance.json")" == "600" ]] || fail "install provenance is not 0600"

provenance="$SWITCHBACK_RUNTIME_ROOT/bin/install-provenance.json"
installed_sha="$(shasum -a 256 "$SWITCHBACK_RUNTIME_ROOT/bin/switchback-bin" | awk '{print $1}')"
[[ "$(jq -r '.schema' "$provenance")" == "switchback/install-provenance@1" ]] || fail "unexpected provenance schema"
[[ "$(jq -r '.version' "$provenance")" == "switchback 0.1.0-test" ]] || fail "missing installed version"
[[ "$(jq -r '.git_commit' "$provenance")" == "$SB_BUILD_COMMIT" ]] || fail "missing source commit"
[[ "$(jq -r '.source_engine' "$provenance")" == "${SB_BIN:A}" ]] || fail "wrong source engine path"
[[ "$(jq -r '.installed_engine' "$provenance")" == "${SWITCHBACK_RUNTIME_ROOT:A}/bin/switchback-bin" ]] || fail "wrong installed engine path"
[[ "$(jq -r '.sha256' "$provenance")" == "$installed_sha" ]] || fail "installed engine checksum mismatch"

print -r -- 'export SB_LAUNCHER_SENTINEL=runtime-owned' > "$SWITCHBACK_RUNTIME_ROOT/config/sb.env"
legacy_env="${TMPDIR}/legacy.env"
print -r -- 'export SB_LEGACY_SENTINEL=legacy-opt-in' > "$legacy_env"
mkdir -p "${TMPDIR}/elsewhere"
(
  cd "${TMPDIR}/elsewhere"
  unset SWITCHBACK_RUNTIME_ROOT SB_RUNTIME_ROOT
  FAKE_LOG="${TMPDIR}/launcher.log" \
    SWITCHBACK_LEGACY_ENV="$legacy_env" \
    "$PREFIX/switchback" probe --flag
)
launcher_log="$(cat "${TMPDIR}/launcher.log")"
assert_contains "$launcher_log" "runtime=${SWITCHBACK_RUNTIME_ROOT:A}"
assert_contains "$launcher_log" "runtime_alias=${SWITCHBACK_RUNTIME_ROOT:A}"
assert_contains "$launcher_log" "runtime_env=runtime-owned"
assert_contains "$launcher_log" "legacy_env=legacy-opt-in"
assert_contains "$launcher_log" "cwd=${TMPDIR}/elsewhere"
assert_contains "$launcher_log" "args=probe --flag"

before="$(shasum -a 256 "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")"

# Launch-profile materialization is the authority for wrappers it marks as
# owned. A later source install must not replace that generated wrapper with a
# legacy tracked symlink of the same name.
profile_wrapper="$PREFIX/claude-neuralwatt"
rm -f "$profile_wrapper"
cat > "$profile_wrapper" <<'PROFILE_WRAPPER'
#!/bin/zsh
# switchback-owned: launch-profile-wrapper@1
exec switchback profile-owned-wrapper "$@"
PROFILE_WRAPPER
chmod 700 "$profile_wrapper"
profile_wrapper_before="$(shasum -a 256 "$profile_wrapper")"

# A marker substring is not ownership proof. The installer must restore the
# tracked wrapper instead of preserving this lookalike.
substring_wrapper="$PREFIX/claude-neuralwatt-full"
rm -f "$substring_wrapper"
cat > "$substring_wrapper" <<'SUBSTRING_WRAPPER'
#!/bin/zsh
# switchback-owned: launch-profile-wrapper@1-not-exact
exec switchback marker-substring "$@"
SUBSTRING_WRAPPER
chmod 700 "$substring_wrapper"

# Profile-wrapper ownership never extends to Switchback's core commands. Even
# an executable file with the exact marker must not pin either core entrypoint.
for core_command in switchback sb; do
  core_path="$PREFIX/$core_command"
  rm -f "$core_path"
  cat > "$core_path" <<'CORE_WRAPPER'
#!/bin/zsh
# switchback-owned: launch-profile-wrapper@1
exec false
CORE_WRAPPER
  chmod 700 "$core_path"
done

"$INSTALLER" >"${TMPDIR}/install-second.out" 2>"${TMPDIR}/install-second.err"
after="$(shasum -a 256 "$SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml")"
[[ "$before" == "$after" ]] || fail "idempotent install rewrote config"
[[ ! -L "$profile_wrapper" ]] || fail "install replaced a profile-owned wrapper with a symlink"
[[ "$(shasum -a 256 "$profile_wrapper")" == "$profile_wrapper_before" ]] || fail "install rewrote a profile-owned wrapper"
assert_link "$substring_wrapper"
[[ "$(readlink "$substring_wrapper")" == "$CLI_ROOT/wrappers/claude-neuralwatt-full" ]] || fail "install preserved a wrapper with only a marker substring"
assert_link "$PREFIX/switchback"
[[ "$(readlink "$PREFIX/switchback")" == "$SWITCHBACK_RUNTIME_ROOT/bin/switchback" ]] || fail "install preserved a marker-bearing core switchback command"
assert_link "$PREFIX/sb"
[[ "$(readlink "$PREFIX/sb")" == "$CLI_ROOT/sb" ]] || fail "install preserved a marker-bearing core sb command"
setup_calls="$(wc -l < "$FAKE_SETUP_LOG" | tr -d ' ')"
[[ "$setup_calls" == "1" ]] || fail "idempotent install reran setup ($setup_calls calls)"
assert_contains "$(cat "${TMPDIR}/install-second.out")" "kept existing $SWITCHBACK_RUNTIME_ROOT/config/switchback.yaml"

print "ok - install owns one runtime root"
