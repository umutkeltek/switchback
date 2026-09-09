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

# Engine-only repair updates exactly the CLI engine/provenance pair. It must
# not touch config, launchers, wrappers, or independently deployed services.
repair_runtime="${TMPDIR}/repair-runtime"
repair_prefix="${TMPDIR}/repair-prefix"
mkdir -p "$repair_runtime/bin" "$repair_runtime/config" "$repair_prefix"
print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$repair_runtime/manifest.json"
print -r -- 'old-engine' > "$repair_runtime/bin/switchback-bin"
print -r -- 'old-provenance' > "$repair_runtime/bin/install-provenance.json"
print -r -- 'service-sentinel' > "$repair_runtime/bin/switchback-mode-d-bin"
print -r -- 'config-sentinel' > "$repair_runtime/config/switchback.yaml"
print -r -- 'launcher-sentinel' > "$repair_runtime/bin/switchback"
print -r -- 'prefix-sentinel' > "$repair_prefix/sb"

service_before="$(shasum -a 256 "$repair_runtime/bin/switchback-mode-d-bin")"
config_before="$(shasum -a 256 "$repair_runtime/config/switchback.yaml")"
launcher_before="$(shasum -a 256 "$repair_runtime/bin/switchback")"
prefix_before="$(shasum -a 256 "$repair_prefix/sb")"

if ! SWITCHBACK_RUNTIME_ROOT="$repair_runtime" \
  PREFIX="$repair_prefix" \
  SB_INSTALL_ENGINE_ONLY=1 \
  "$INSTALLER" >"${TMPDIR}/engine-only.out" 2>"${TMPDIR}/engine-only.err"; then
  print -ru2 -- "$(cat "${TMPDIR}/engine-only.err")"
  fail "engine-only repair failed"
fi

repair_provenance="$repair_runtime/bin/install-provenance.json"
repair_sha="$(shasum -a 256 "$repair_runtime/bin/switchback-bin" | awk '{print $1}')"
[[ "$(jq -r '.sha256' "$repair_provenance")" == "$repair_sha" ]] || fail "engine-only provenance checksum mismatch"
[[ "$(jq -r '.install_scope' "$repair_provenance")" == "engine_and_provenance_only" ]] || fail "engine-only scope missing"
[[ "$(jq -r '.services_refreshed | length' "$repair_provenance")" == "0" ]] || fail "engine-only claimed service refresh"
[[ "$(shasum -a 256 "$repair_runtime/bin/switchback-mode-d-bin")" == "$service_before" ]] || fail "engine-only changed service"
[[ "$(shasum -a 256 "$repair_runtime/config/switchback.yaml")" == "$config_before" ]] || fail "engine-only changed config"
[[ "$(shasum -a 256 "$repair_runtime/bin/switchback")" == "$launcher_before" ]] || fail "engine-only changed launcher"
[[ "$(shasum -a 256 "$repair_prefix/sb")" == "$prefix_before" ]] || fail "engine-only changed prefix"
assert_file "$repair_runtime/bin/switchback-bin.bak-engine-only"
assert_file "$repair_runtime/bin/install-provenance.json.bak-engine-only"

# A hard crash between the two renames leaves a durable transaction marker.
# The next invocation must restore the exact pair before taking a new backup,
# then complete normally without touching any other runtime surface.
crash_runtime="${TMPDIR}/crash-runtime"
mkdir -p "$crash_runtime/bin"
print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$crash_runtime/manifest.json"
print -r -- 'crash-old-engine' > "$crash_runtime/bin/switchback-bin"
print -r -- 'crash-old-provenance' > "$crash_runtime/bin/install-provenance.json"
crash_bin="${TMPDIR}/crash-bin"
mkdir -p "$crash_bin"
{
  print -r -- '#!/bin/sh'
  print -r -- '/bin/mv "$@" || exit $?'
  print -r -- 'case "$1" in'
  print -r -- '  */.engine-only-install.journal/switchback-bin.new) /bin/kill -KILL "$PPID" ;;'
  print -r -- 'esac'
} > "$crash_bin/mv"
chmod 755 "$crash_bin/mv"
if PATH="$crash_bin:$PATH" \
  SWITCHBACK_RUNTIME_ROOT="$crash_runtime" \
  SB_INSTALL_ENGINE_ONLY=1 \
  "$INSTALLER" >"${TMPDIR}/engine-only-crash.out" 2>"${TMPDIR}/engine-only-crash.err"; then
  fail "engine-only crash injection unexpectedly succeeded"
fi
[[ -f "$crash_runtime/bin/.engine-only-install.lock" ]] || fail "engine-only crash left no kernel lock inode"
[[ -d "$crash_runtime/bin/.engine-only-install.journal" ]] || fail "engine-only crash left no recovery journal"
[[ "$(cat "$crash_runtime/bin/install-provenance.json")" == "crash-old-provenance" ]] || fail "crash moved provenance before injected boundary"

SWITCHBACK_RUNTIME_ROOT="$crash_runtime" \
SB_INSTALL_ENGINE_ONLY=1 \
"$INSTALLER" >"${TMPDIR}/engine-only-recover.out" 2>"${TMPDIR}/engine-only-recover.err"
[[ -f "$crash_runtime/bin/.engine-only-install.lock" ]] || fail "engine-only recovery removed stable lock inode"
[[ ! -e "$crash_runtime/bin/.engine-only-install.journal" ]] || fail "engine-only recovery left transaction journal"
[[ "$(cat "$crash_runtime/bin/switchback-bin.bak-engine-only")" == "crash-old-engine" ]] || fail "engine-only recovery did not restore engine preimage"
[[ "$(cat "$crash_runtime/bin/install-provenance.json.bak-engine-only")" == "crash-old-provenance" ]] || fail "engine-only recovery did not restore provenance preimage"

# A staging failure before the transaction is prepared must still release the
# lock so a retry is not permanently denied.
copyfail_runtime="${TMPDIR}/copyfail-runtime"
copyfail_bin="${TMPDIR}/copyfail-bin"
mkdir -p "$copyfail_runtime/bin" "$copyfail_bin"
print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$copyfail_runtime/manifest.json"
print -r -- 'copyfail-old-engine' > "$copyfail_runtime/bin/switchback-bin"
print -r -- 'copyfail-old-provenance' > "$copyfail_runtime/bin/install-provenance.json"
{
  print -r -- '#!/bin/sh'
  print -r -- 'case "$2" in'
  print -r -- '  */switchback-bin.bak-engine-only) exit 91 ;;'
  print -r -- 'esac'
  print -r -- 'exec /bin/cp "$@"'
} > "$copyfail_bin/cp"
chmod 755 "$copyfail_bin/cp"
if SWITCHBACK_RUNTIME_ROOT="$copyfail_runtime" \
  PATH="$copyfail_bin:$PATH" \
  SB_INSTALL_ENGINE_ONLY=1 \
  "$INSTALLER" >"${TMPDIR}/engine-only-copyfail.out" 2>"${TMPDIR}/engine-only-copyfail.err"; then
  fail "engine-only copy failure injection unexpectedly succeeded"
fi
[[ -f "$copyfail_runtime/bin/.engine-only-install.lock" ]] || fail "engine-only staging failure removed stable lock inode"
[[ ! -e "$copyfail_runtime/bin/.engine-only-install.journal" ]] || fail "engine-only staging failure stranded journal"
[[ "$(cat "$copyfail_runtime/bin/switchback-bin")" == "copyfail-old-engine" ]] || fail "engine-only staging failure changed engine"
[[ "$(cat "$copyfail_runtime/bin/install-provenance.json")" == "copyfail-old-provenance" ]] || fail "engine-only staging failure changed provenance"

make_engine_only_runtime() {
  local target="$1"
  mkdir -p "$target/bin"
  print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$target/manifest.json"
  print -r -- 'guard-old-engine' > "$target/bin/switchback-bin"
  print -r -- 'guard-old-provenance' > "$target/bin/install-provenance.json"
}

engine_only_must_refuse() {
  local target="$1" label="$2"
  if SWITCHBACK_RUNTIME_ROOT="$target" SB_INSTALL_ENGINE_ONLY=1 \
    "$INSTALLER" >"${TMPDIR}/$label.out" 2>"${TMPDIR}/$label.err"; then
    fail "engine-only accepted unsafe fixture: $label"
  fi
}

# Runtime-owned targets and their immediate parents are never followed through
# symlinks or accepted as non-regular/world-writable surfaces.
symlink_runtime="$TMPDIR/symlink-runtime"
make_engine_only_runtime "$symlink_runtime"
symlink_sentinel="$TMPDIR/symlink-sentinel"
print -r -- 'do-not-touch' > "$symlink_sentinel"
rm "$symlink_runtime/bin/switchback-bin"
ln -s "$symlink_sentinel" "$symlink_runtime/bin/switchback-bin"
engine_only_must_refuse "$symlink_runtime" "engine-only-symlink-engine"
[[ "$(cat "$symlink_sentinel")" == "do-not-touch" ]] || fail "engine-only followed installed-engine symlink"
[[ ! -e "$symlink_runtime/bin/.engine-only-install.lock" ]] || fail "symlink refusal acquired lock"

backup_symlink_runtime="$TMPDIR/backup-symlink-runtime"
make_engine_only_runtime "$backup_symlink_runtime"
ln -s "$symlink_sentinel" "$backup_symlink_runtime/bin/switchback-bin.bak-engine-only"
engine_only_must_refuse "$backup_symlink_runtime" "engine-only-symlink-backup"
[[ "$(cat "$symlink_sentinel")" == "do-not-touch" ]] || fail "engine-only followed backup symlink"

hardlink_runtime="$TMPDIR/hardlink-runtime"
make_engine_only_runtime "$hardlink_runtime"
hardlink_sentinel="$TMPDIR/hardlink-sentinel"
print -r -- 'hardlink-do-not-touch' > "$hardlink_sentinel"
rm "$hardlink_runtime/bin/switchback-bin"
ln "$hardlink_sentinel" "$hardlink_runtime/bin/switchback-bin"
engine_only_must_refuse "$hardlink_runtime" "engine-only-hardlink-engine"
[[ "$(cat "$hardlink_sentinel")" == "hardlink-do-not-touch" ]] || fail "engine-only changed hard-linked engine"

hardlink_lock_runtime="$TMPDIR/hardlink-lock-runtime"
make_engine_only_runtime "$hardlink_lock_runtime"
ln "$hardlink_sentinel" "$hardlink_lock_runtime/bin/.engine-only-install.lock"
engine_only_must_refuse "$hardlink_lock_runtime" "engine-only-hardlink-lock"
[[ "$(cat "$hardlink_sentinel")" == "hardlink-do-not-touch" ]] || fail "engine-only changed hard-linked lock file"

nonregular_runtime="$TMPDIR/nonregular-runtime"
make_engine_only_runtime "$nonregular_runtime"
rm "$nonregular_runtime/bin/install-provenance.json"
mkdir "$nonregular_runtime/bin/install-provenance.json"
engine_only_must_refuse "$nonregular_runtime" "engine-only-nonregular-provenance"

unsafe_parent_runtime="$TMPDIR/unsafe-parent-runtime"
make_engine_only_runtime "$unsafe_parent_runtime"
chmod 777 "$unsafe_parent_runtime/bin"
engine_only_must_refuse "$unsafe_parent_runtime" "engine-only-unsafe-parent"
chmod 755 "$unsafe_parent_runtime/bin"

# A real kernel-held lock serializes installers without PID publication or
# stale-reclaimer authority.
concurrent_runtime="$TMPDIR/concurrent-lock-runtime"
make_engine_only_runtime "$concurrent_runtime"
concurrent_lock="$concurrent_runtime/bin/.engine-only-install.lock"
concurrent_ready="$TMPDIR/concurrent-lock.ready"
concurrent_release="$TMPDIR/concurrent-lock.release"
: > "$concurrent_lock"
(
  zmodload zsh/system
  zsystem flock -t 1 -f held_fd "$concurrent_lock"
  : > "$concurrent_ready"
  while [[ ! -f "$concurrent_release" ]]; do sleep 0.01; done
  zsystem flock -u "$held_fd"
) &
concurrent_holder=$!
for _ in {1..100}; do
  [[ -f "$concurrent_ready" ]] && break
  sleep 0.01
done
[[ -f "$concurrent_ready" ]] || fail "kernel-lock fixture did not acquire lock"
engine_only_must_refuse "$concurrent_runtime" "engine-only-concurrent-lock"
: > "$concurrent_release"
wait "$concurrent_holder"
[[ "$(cat "$concurrent_runtime/bin/switchback-bin")" == "guard-old-engine" ]] || fail "concurrent installer changed engine"
[[ "$(cat "$concurrent_runtime/bin/install-provenance.json")" == "guard-old-provenance" ]] || fail "concurrent installer changed provenance"

# Recovery preimages and markers receive the same lstat treatment before a
# stale transaction can copy anything back onto canonical targets.
recovery_symlink_runtime="$TMPDIR/recovery-symlink-runtime"
make_engine_only_runtime "$recovery_symlink_runtime"
recovery_lock="$recovery_symlink_runtime/bin/.engine-only-install.journal"
mkdir "$recovery_lock"
: > "$recovery_lock/prepared"
: > "$recovery_lock/had-engine"
ln -s "$symlink_sentinel" "$recovery_lock/switchback-bin.old"
engine_only_must_refuse "$recovery_symlink_runtime" "engine-only-symlink-recovery"
[[ "$(cat "$recovery_symlink_runtime/bin/switchback-bin")" == "guard-old-engine" ]] || fail "unsafe recovery changed engine"

missing_preimage_runtime="$TMPDIR/missing-preimage-runtime"
make_engine_only_runtime "$missing_preimage_runtime"
missing_journal="$missing_preimage_runtime/bin/.engine-only-install.journal"
mkdir "$missing_journal"
: > "$missing_journal/prepared"
: > "$missing_journal/had-engine"
engine_only_must_refuse "$missing_preimage_runtime" "engine-only-missing-preimage"
[[ -d "$missing_journal" ]] || fail "missing-preimage recovery discarded journal"
[[ "$(cat "$missing_preimage_runtime/bin/switchback-bin")" == "guard-old-engine" ]] || fail "missing-preimage recovery changed engine"

# Recovery is repeatable after a crash between restoring the first and second
# canonical files. Both durable preimages remain until the pair is complete.
partial_runtime="$TMPDIR/partial-recovery-runtime"
make_engine_only_runtime "$partial_runtime"
print -r -- 'partial-new-provenance' > "$partial_runtime/bin/install-provenance.json"
partial_journal="$partial_runtime/bin/.engine-only-install.journal"
mkdir "$partial_journal"
: > "$partial_journal/prepared"
: > "$partial_journal/had-engine"
: > "$partial_journal/had-provenance"
print -r -- 'guard-old-engine' > "$partial_journal/switchback-bin.old"
print -r -- 'guard-old-provenance' > "$partial_journal/install-provenance.json.old"
SWITCHBACK_RUNTIME_ROOT="$partial_runtime" SB_INSTALL_ENGINE_ONLY=1 \
  "$INSTALLER" >"$TMPDIR/engine-only-partial.out" 2>"$TMPDIR/engine-only-partial.err"
[[ "$(cat "$partial_runtime/bin/switchback-bin.bak-engine-only")" == "guard-old-engine" ]] || fail "partial recovery lost engine preimage"
[[ "$(cat "$partial_runtime/bin/install-provenance.json.bak-engine-only")" == "guard-old-provenance" ]] || fail "partial recovery lost provenance preimage"
[[ ! -e "$partial_journal" ]] || fail "partial recovery left journal"

# A failed first canonical restore must stop immediately: a later provenance
# restore may not hide it and cause recovery to discard durable preimages.
restore_fail_runtime="$TMPDIR/restore-fail-runtime"
make_engine_only_runtime "$restore_fail_runtime"
print -r -- 'restore-fail-new-provenance' > "$restore_fail_runtime/bin/install-provenance.json"
restore_fail_journal="$restore_fail_runtime/bin/.engine-only-install.journal"
mkdir "$restore_fail_journal"
: > "$restore_fail_journal/prepared"
: > "$restore_fail_journal/had-engine"
: > "$restore_fail_journal/had-provenance"
print -r -- 'guard-old-engine' > "$restore_fail_journal/switchback-bin.old"
print -r -- 'guard-old-provenance' > "$restore_fail_journal/install-provenance.json.old"
restore_fail_bin="$TMPDIR/restore-fail-bin"
mkdir "$restore_fail_bin"
cat > "$restore_fail_bin/mv" <<'EOF'
#!/bin/sh
if [ "$1" = "$SB_TEST_FAIL_MV_SOURCE" ] && [ "$2" = "$SB_TEST_FAIL_MV_TARGET" ]; then
  exit 86
fi
exec /bin/mv "$@"
EOF
chmod 755 "$restore_fail_bin/mv"
if PATH="$restore_fail_bin:$PATH" SB_TEST_FAIL_MV_SOURCE="$restore_fail_journal/switchback-bin.restore" SB_TEST_FAIL_MV_TARGET="$restore_fail_runtime/bin/switchback-bin" SWITCHBACK_RUNTIME_ROOT="$restore_fail_runtime" SB_INSTALL_ENGINE_ONLY=1 "$INSTALLER" >"$TMPDIR/engine-only-restore-fail.out" 2>"$TMPDIR/engine-only-restore-fail.err"; then
  fail "engine-only first restore mv failure unexpectedly succeeded"
fi
[[ -d "$restore_fail_journal" ]] || fail "restore failure discarded journal"
[[ -f "$restore_fail_journal/switchback-bin.old" && -f "$restore_fail_journal/install-provenance.json.old" ]] || fail "restore failure discarded preimages"
SWITCHBACK_RUNTIME_ROOT="$restore_fail_runtime" SB_INSTALL_ENGINE_ONLY=1 "$INSTALLER" >"$TMPDIR/engine-only-restore-retry.out" 2>"$TMPDIR/engine-only-restore-retry.err"
[[ "$(cat "$restore_fail_runtime/bin/switchback-bin.bak-engine-only")" == "guard-old-engine" ]] || fail "restore retry lost engine preimage"
[[ "$(cat "$restore_fail_runtime/bin/install-provenance.json.bak-engine-only")" == "guard-old-provenance" ]] || fail "restore retry lost provenance preimage"
[[ ! -e "$restore_fail_journal" ]] || fail "restore retry left journal"

if env -u SWITCHBACK_RUNTIME_ROOT -u SB_RUNTIME_ROOT \
  SB_INSTALL_ENGINE_ONLY=1 SB_BIN="$SB_BIN" "$INSTALLER" >/dev/null 2>&1; then
  fail "engine-only accepted implicit runtime root"
fi

print "ok - install owns one runtime root"
