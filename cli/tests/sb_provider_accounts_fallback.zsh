#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

fail() { print -ru2 -- "FAIL: $*"; exit 1; }

prepare() {
  local root="$1" home="${1}/home" reg="${1}/authreg"
  mkdir -p "${home}/.codex" "${home}/.local/bin" "${reg}/backups"
  print -r -- '{"account":"default","tokens":{"account_id":"01234567-89ab-cdef-8123-456789abcdef"}}' > "${home}/.codex/auth.json"
  cp "${home}/.codex/auth.json" "${reg}/default.json"
  print -r -- '{"account":"work","tokens":{"account_id":"fedcba98-7654-4321-8123-fedcba987654"}}' > "${reg}/work.json"
  print -r -- default > "${reg}/.active"
  : > "${reg}/.runs"
  cat > "${home}/.local/bin/codex-switchback-tap" <<'FAKE'
#!/bin/zsh
exit 0
FAKE
  chmod +x "${home}/.local/bin/codex-switchback-tap"
}

prepare "${TMPDIR}/legacy"
prepare "${TMPDIR}/authority-absent"

cat > "${TMPDIR}/legacy.zsh" <<'LEGACY'
#!/bin/zsh
set -uo pipefail
SB_MAIN_HOME="${HOME}/.codex"
SB_AUTHREG="${SB_AUTHREG}"
SB_ACTIVE_FILE="${SB_AUTHREG}/.active"
SB_RUNS_FILE="${SB_AUTHREG}/.runs"
SB_LOCK_DIR="${SB_AUTHREG}/.lock"
_authreg_path() { echo "${SB_AUTHREG}/$1.json"; }
_shared_active() { cat "$SB_ACTIVE_FILE" 2>/dev/null || echo default; }
_shared_lock_acquire() { mkdir -p "$SB_AUTHREG" || return 1; local i=0; while ! mkdir "$SB_LOCK_DIR" 2>/dev/null; do (( i >= 200 )) && return 1; (( i++ )); sleep 0.05; done; }
_shared_lock_release() { rmdir "$SB_LOCK_DIR" 2>/dev/null || true; }
_shared_prune_runs_locked() {
  [[ -f "$SB_RUNS_FILE" ]] || return 0
  local tmp; tmp="$(mktemp)" || return 1
  local pid acct started
  while IFS=$'\t' read -r pid acct started; do
    [[ "$pid" == <-> ]] || continue
    kill -0 "$pid" 2>/dev/null && printf '%s\t%s\t%s\n' "$pid" "$acct" "$started" >> "$tmp"
  done < "$SB_RUNS_FILE"
  mv "$tmp" "$SB_RUNS_FILE"
}
_shared_conflicts_locked() { _shared_prune_runs_locked; return 0; }
_shared_register_run_locked() { printf '%s\t%s\t%s\n' "$2" "$1" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$SB_RUNS_FILE"; }
_shared_unregister_run_locked() { local tmp; tmp="$(mktemp)" || return 0; awk -F '\t' -v pid="$1" '$1 != pid' "$SB_RUNS_FILE" > "$tmp" 2>/dev/null; mv "$tmp" "$SB_RUNS_FILE"; }
_seed_authreg() { mkdir -p "${SB_AUTHREG}/backups" || return 1; [[ -f "$(_authreg_path default)" ]] || { [[ -f "${SB_MAIN_HOME}/auth.json" ]] && cp "${SB_MAIN_HOME}/auth.json" "$(_authreg_path default)"; }; [[ -f "$SB_ACTIVE_FILE" ]] || echo default > "$SB_ACTIVE_FILE"; }
_backup_main_auth() { [[ -f "${SB_MAIN_HOME}/auth.json" ]] || return 0; local ts; ts="$(date +%Y%m%d-%H%M%S)"; cp "${SB_MAIN_HOME}/auth.json" "${SB_AUTHREG}/backups/auth.$(_shared_active).${ts}.json" 2>/dev/null; }
_activate_shared() {
  local want="$1" run_pid="${2:-}"
  _shared_lock_acquire || return 1
  _seed_authreg || { _shared_lock_release; return 1; }
  local reg; reg="$(_authreg_path "$want")"
  [[ -f "$reg" ]] || { _shared_lock_release; return 1; }
  local cur; cur="$(_shared_active)"
  [[ -f "${SB_MAIN_HOME}/auth.json" ]] && cp -f "${SB_MAIN_HOME}/auth.json" "$(_authreg_path "$cur")" 2>/dev/null
  if [[ "$want" != "$cur" || ! -f "${SB_MAIN_HOME}/auth.json" ]]; then _backup_main_auth; cp -f "$reg" "${SB_MAIN_HOME}/auth.json" || { _shared_lock_release; return 1; }; fi
  echo "$want" > "$SB_ACTIVE_FILE"
  [[ -n "$run_pid" ]] && _shared_register_run_locked "$want" "$run_pid"
  _shared_lock_release
}
_finish_shared_run() { local acct="$1" pid="${2:-$$}"; _shared_lock_acquire || return 0; [[ -f "${SB_MAIN_HOME}/auth.json" ]] && cp -f "${SB_MAIN_HOME}/auth.json" "$(_authreg_path "$acct")" 2>/dev/null; _shared_unregister_run_locked "$pid"; _shared_lock_release; }
# The relay is pinned down via SB_GATEWAY in this test, so pre-v0 sb printed
# this warning too; emitting it keeps stderr parity host-independent.
print -P "%F{yellow}warning:%f relay :18765 down — 'sb reload'" >&2
_activate_shared work "$$" || exit 1
print -P "%F{8}(shared sessions · ~/.codex pool · account: work)%f" >&2
CODEX_HOME="$SB_MAIN_HOME" "${HOME}/.local/bin/codex-switchback-tap" once
rc=$?
_finish_shared_run work "$$"
exit $rc
LEGACY
chmod +x "${TMPDIR}/legacy.zsh"

set +e
HOME="${TMPDIR}/legacy/home" SB_AUTHREG="${TMPDIR}/legacy/authreg" zsh "${TMPDIR}/legacy.zsh" >"${TMPDIR}/legacy.out" 2>"${TMPDIR}/legacy.err"
legacy_status=$?
HOME="${TMPDIR}/authority-absent/home" SB_AUTHREG="${TMPDIR}/authority-absent/authreg" SWITCHBACK_STATE_DIR="${TMPDIR}/authority-absent/missing-state" SB_GATEWAY="http://127.0.0.1:0" zsh "$SB" codex --mode tap --account work --sessions shared once >"${TMPDIR}/new.out" 2>"${TMPDIR}/new.err"
new_status=$?
set -e

[[ "$legacy_status" == "$new_status" ]] || fail "exit status differs: legacy=${legacy_status} new=${new_status}"
cmp -s "${TMPDIR}/legacy.out" "${TMPDIR}/new.out" || {
  diff "${TMPDIR}/legacy.out" "${TMPDIR}/new.out" >&2 || true
  fail "stdout differs"
}
cmp -s "${TMPDIR}/legacy.err" "${TMPDIR}/new.err" || {
  diff "${TMPDIR}/legacy.err" "${TMPDIR}/new.err" >&2 || true
  fail "stderr differs"
}
for file in home/.codex/auth.json authreg/default.json authreg/work.json authreg/.active authreg/.runs; do
  cmp -s "${TMPDIR}/legacy/${file}" "${TMPDIR}/authority-absent/${file}" || fail "${file} differs"
done
legacy_backups=("${TMPDIR}/legacy/authreg/backups/"*.json(N))
new_backups=("${TMPDIR}/authority-absent/authreg/backups/"*.json(N))
[[ ${#legacy_backups} == ${#new_backups} ]] || fail "backup inventory count differs"
for i in {1..${#legacy_backups}}; do cmp -s "${legacy_backups[$i]}" "${new_backups[$i]}" || fail "backup bytes differ"; done

print "ok - absent provider authority is byte-identical to pre-v0 activation"
