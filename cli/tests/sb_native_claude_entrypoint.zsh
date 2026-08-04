#!/bin/zsh
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
INSTALLER="${CLI_ROOT}/install.sh"
export HOME="${TMPDIR}/home"
export SWITCHBACK_RUNTIME_ROOT="${HOME}/.switchback"
export PREFIX="${HOME}/.local/bin"
export SB_BIN="${TMPDIR}/fake-switchback"
export SB_BUILD_COMMIT="0123456789abcdef0123456789abcdef01234567"
export FAKE_CLAUDE_LOG="${TMPDIR}/claude.log"
export FAKE_LAUNCHCTL_LOG="${TMPDIR}/launchctl.log"
export MODE_D_READY_FLAG="${TMPDIR}/mode-d-ready"
export MODE_D_CA="${SWITCHBACK_RUNTIME_ROOT}/state/mode-d/ca.pem"
fake_bin="${TMPDIR}/fake-bin"
vendor_dir="${HOME}/.local/share/claude/versions"
vendor_binary="${vendor_dir}/1.2.3"

mkdir -p "$PREFIX" "$vendor_dir" "$fake_bin"

cat > "$SB_BIN" <<'FAKE_SWITCHBACK'
#!/bin/zsh
set -euo pipefail
if [[ "$*" == "--version" ]]; then
  print -r -- "switchback 0.1.0-test"
  exit 0
fi
if [[ "$*" == *"setup --root"* ]]; then
  root="${@: -1}"
  mkdir -p "$root"/{config,state/body,eval,receipts,bin,backups}
  print -r -- '{"schema":"switchback/runtime-manifest@1","owner":"switchback"}' > "$root/manifest.json"
  exit 0
fi
if [[ "${1:-}" == "--json" && "${2:-}" == "body" && "${3:-}" == "status" ]]; then
  [[ "$*" == *"--state-dir ${SWITCHBACK_RUNTIME_ROOT}/state/mode-d"* ]] || exit 78
  [[ "$*" == *"--legacy-jsonl ${SWITCHBACK_RUNTIME_ROOT}/state/mode-d/tap-bodies.jsonl"* ]] || exit 79
  case "${FAKE_BODY_MODE:-healthy}" in
    healthy)
      print -r -- '{"schema":"switchback/body-status@2","status":"ok","archive_available":true,"spool_backlog":0,"capture_queue_depth":0,"capture_queue_drops":0,"pressure":{"mode":"segmented_full_wire","reasons":[],"warnings":[]}}'
      ;;
    metadata-only)
      print -r -- '{"schema":"switchback/body-status@2","status":"ok","archive_available":true,"spool_backlog":0,"capture_queue_depth":0,"capture_queue_drops":0,"pressure":{"mode":"metadata_only","reasons":["database_locked"],"warnings":[]}}'
      ;;
    archive-unavailable)
      print -r -- '{"schema":"switchback/body-status@2","status":"archive_unavailable","archive_available":false,"spool_backlog":2,"capture_queue_depth":0,"capture_queue_drops":0,"pressure":{"mode":"segmented_full_wire","reasons":["archive_unavailable"],"warnings":[]}}'
      ;;
    queue-drops)
      print -r -- '{"schema":"switchback/body-status@2","status":"ok","archive_available":true,"spool_backlog":0,"capture_queue_depth":0,"capture_queue_drops":3,"pressure":{"mode":"segmented_full_wire","reasons":[],"warnings":[]}}'
      ;;
    absent) exit 1 ;;
    *) exit 2 ;;
  esac
  exit 0
fi
FAKE_SWITCHBACK
chmod +x "$SB_BIN"

cat > "$vendor_binary" <<'FAKE_CLAUDE'
#!/bin/zsh
set -euo pipefail
{
  print -r -- "args=$*"
  print -r -- "HTTPS_PROXY=${HTTPS_PROXY:-}"
  print -r -- "https_proxy=${https_proxy:-}"
  print -r -- "NO_PROXY=${NO_PROXY:-}"
  print -r -- "no_proxy=${no_proxy:-}"
  print -r -- "NODE_EXTRA_CA_CERTS=${NODE_EXTRA_CA_CERTS:-}"
} >> "${FAKE_CLAUDE_LOG:?FAKE_CLAUDE_LOG is required}"
FAKE_CLAUDE
chmod +x "$vendor_binary"
ln -s "$vendor_binary" "$PREFIX/claude"

cat > "$fake_bin/launchctl" <<'FAKE_LAUNCHCTL'
#!/bin/zsh
set -euo pipefail
print -r -- "$*" >> "${FAKE_LAUNCHCTL_LOG:?FAKE_LAUNCHCTL_LOG is required}"
if [[ "${FAKE_MODE_D_ACTIVATE:-0}" == "1" ]]; then
  mkdir -p "${MODE_D_CA:h}"
  print -r -- "synthetic Mode D CA" > "$MODE_D_CA"
  : > "${MODE_D_READY_FLAG:?MODE_D_READY_FLAG is required}"
fi
exit "${FAKE_LAUNCHCTL_STATUS:-0}"
FAKE_LAUNCHCTL
chmod +x "$fake_bin/launchctl"

cat > "$fake_bin/mode-d-probe" <<'FAKE_PROBE'
#!/bin/zsh
set -euo pipefail
[[ -f "${MODE_D_READY_FLAG:?MODE_D_READY_FLAG is required}" ]]
FAKE_PROBE
chmod +x "$fake_bin/mode-d-probe"

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected '$needle' in:\n$haystack"
}

"$INSTALLER" >"${TMPDIR}/install.out" 2>"${TMPDIR}/install.err"

# This is the regression seam: an unavailable Mode D must never degrade into a
# successful, uncaptured invocation of the real Claude binary.
set +e
(
  unset CLAUDE_NATIVE_DIRECT HTTPS_PROXY https_proxy HTTP_PROXY http_proxy ALL_PROXY all_proxy
  PATH="${fake_bin}:$PATH" \
    SB_MODE_D_LAUNCHCTL_BIN="${fake_bin}/launchctl" \
    SB_MODE_D_PROBE_BIN="${fake_bin}/mode-d-probe" \
    SB_MODE_D_READY_TIMEOUT_SECONDS=0 \
    SB_MODE_D_CA_CERT="$MODE_D_CA" \
    "$PREFIX/claude" --resume unavailable-mode-d
) >"${TMPDIR}/unavailable.out" 2>"${TMPDIR}/unavailable.err"
unavailable_status=$?
set -e
[[ "$unavailable_status" != 0 ]] || fail "unavailable Mode D launched real Claude uncaptured"
[[ ! -e "$FAKE_CLAUDE_LOG" ]] || fail "real Claude ran while Mode D was unavailable"
unavailable_err="$(cat "${TMPDIR}/unavailable.err")"
assert_contains "$unavailable_err" "ai.switchback.mode-d"
assert_contains "$unavailable_err" "not ready"

# A kick that makes the listener and CA ready must continue through Mode D.
rm -f "$FAKE_LAUNCHCTL_LOG"
FAKE_MODE_D_ACTIVATE=1 \
PATH="${fake_bin}:$PATH" \
SB_MODE_D_LAUNCHCTL_BIN="${fake_bin}/launchctl" \
SB_MODE_D_PROBE_BIN="${fake_bin}/mode-d-probe" \
SB_MODE_D_READY_TIMEOUT_SECONDS=2 \
SB_MODE_D_CA_CERT="$MODE_D_CA" \
  "$PREFIX/claude" --resume captured-mode-d
claude_log="$(cat "$FAKE_CLAUDE_LOG")"
assert_contains "$claude_log" "args=--resume captured-mode-d"
assert_contains "$claude_log" "HTTPS_PROXY=http://127.0.0.1:18780"
assert_contains "$claude_log" "NODE_EXTRA_CA_CERTS=$MODE_D_CA"
launchctl_log="$(cat "$FAKE_LAUNCHCTL_LOG")"
assert_contains "$launchctl_log" "ai.switchback.mode-d"
[[ "$launchctl_log" != *"kickstart -k"* ]] || fail "automatic startup reset an in-progress Mode D cold start"

# The explicit direct escape remains a pass-through and must not touch launchctl.
rm -f "$FAKE_CLAUDE_LOG" "$FAKE_LAUNCHCTL_LOG" "$MODE_D_READY_FLAG" "$MODE_D_CA"
CLAUDE_NATIVE_DIRECT=1 \
PATH="${fake_bin}:$PATH" \
SB_MODE_D_LAUNCHCTL_BIN="${fake_bin}/launchctl" \
SB_MODE_D_PROBE_BIN="${fake_bin}/mode-d-probe" \
SB_MODE_D_READY_TIMEOUT_SECONDS=0 \
SB_MODE_D_CA_CERT="$MODE_D_CA" \
  "$PREFIX/claude" direct
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "args=direct"
[[ ! -e "$FAKE_LAUNCHCTL_LOG" ]] || fail "CLAUDE_NATIVE_DIRECT=1 touched Mode D service"

# An already-valid Mode D proxy is owner-routed and must be normalized without
# a redundant service kick. Inherited bypass hosts (including `*`) must never
# route Claude around Mode D.
rm -f "$FAKE_CLAUDE_LOG"
mkdir -p "${MODE_D_CA:h}"
print -r -- "synthetic Mode D CA" > "$MODE_D_CA"
: > "$MODE_D_READY_FLAG"
(
  unset https_proxy
  HTTPS_PROXY="http://127.0.0.1:18780" \
  NO_PROXY="*" \
  no_proxy="api.anthropic.com" \
  NODE_EXTRA_CA_CERTS="$MODE_D_CA" \
  PATH="${fake_bin}:$PATH" \
  SB_MODE_D_LAUNCHCTL_BIN="${fake_bin}/launchctl" \
  SB_MODE_D_PROBE_BIN="${fake_bin}/mode-d-probe" \
  SB_MODE_D_READY_TIMEOUT_SECONDS=0 \
  SB_MODE_D_CA_CERT="$MODE_D_CA" \
    "$PREFIX/claude" pre-routed
)
pre_routed_log="$(cat "$FAKE_CLAUDE_LOG")"
assert_contains "$pre_routed_log" "args=pre-routed"
assert_contains "$pre_routed_log" "HTTPS_PROXY=http://127.0.0.1:18780"
assert_contains "$pre_routed_log" "https_proxy=http://127.0.0.1:18780"
assert_contains "$pre_routed_log" "NO_PROXY=localhost,127.0.0.1,::1"
assert_contains "$pre_routed_log" "no_proxy=localhost,127.0.0.1,::1"
[[ "$pre_routed_log" != *"NO_PROXY=*"* ]] || fail "inherited NO_PROXY wildcard bypassed Mode D"
[[ "$pre_routed_log" != *"api.anthropic.com"* ]] || fail "inherited Anthropic NO_PROXY bypass survived normalization"
[[ ! -e "$FAKE_LAUNCHCTL_LOG" ]] || fail "pre-routed Claude touched Mode D service"

# The endpoint without its CA env is incomplete owner routing. Normalize it
# before launch even though the already-ready service does not need a kick.
rm -f "$FAKE_CLAUDE_LOG"
(
  unset NODE_EXTRA_CA_CERTS
  HTTPS_PROXY="http://127.0.0.1:18780" \
  PATH="${fake_bin}:$PATH" \
  SB_MODE_D_LAUNCHCTL_BIN="${fake_bin}/launchctl" \
  SB_MODE_D_PROBE_BIN="${fake_bin}/mode-d-probe" \
  SB_MODE_D_READY_TIMEOUT_SECONDS=0 \
  SB_MODE_D_CA_CERT="$MODE_D_CA" \
    "$PREFIX/claude" normalize-ca
)
normalized_ca_log="$(cat "$FAKE_CLAUDE_LOG")"
assert_contains "$normalized_ca_log" "args=normalize-ca"
assert_contains "$normalized_ca_log" "NODE_EXTRA_CA_CERTS=$MODE_D_CA"
[[ ! -e "$FAKE_LAUNCHCTL_LOG" ]] || fail "ready Mode D was restarted while normalizing CA env"

# An arbitrary proxy is not an ownership signal and cannot become an implicit
# uncaptured bypass. The launcher must replace it with a ready Mode D route.
rm -f "$FAKE_CLAUDE_LOG" "$FAKE_LAUNCHCTL_LOG" "$MODE_D_READY_FLAG" "$MODE_D_CA"
FAKE_MODE_D_ACTIVATE=1 \
HTTPS_PROXY="http://127.0.0.1:19999" \
PATH="${fake_bin}:$PATH" \
SB_MODE_D_LAUNCHCTL_BIN="${fake_bin}/launchctl" \
SB_MODE_D_PROBE_BIN="${fake_bin}/mode-d-probe" \
SB_MODE_D_READY_TIMEOUT_SECONDS=2 \
SB_MODE_D_CA_CERT="$MODE_D_CA" \
  "$PREFIX/claude" arbitrary-proxy
arbitrary_proxy_log="$(cat "$FAKE_CLAUDE_LOG")"
assert_contains "$arbitrary_proxy_log" "args=arbitrary-proxy"
assert_contains "$arbitrary_proxy_log" "HTTPS_PROXY=http://127.0.0.1:18780"
[[ "$arbitrary_proxy_log" != *"HTTPS_PROXY=http://127.0.0.1:19999"* ]] || fail "arbitrary HTTPS_PROXY bypassed Mode D"
assert_contains "$(cat "$FAKE_LAUNCHCTL_LOG")" "ai.switchback.mode-d"

# Installation ownership is explicit and independently verifiable. The pin
# keeps the vendor executable reachable after its public symlink is replaced.
entrypoint_provenance="${SWITCHBACK_RUNTIME_ROOT}/bin/native-claude-entrypoint-provenance.json"
pin_file="${HOME}/.local/share/claude/.switchback-real"
[[ -f "$entrypoint_provenance" ]] || fail "missing native Claude entrypoint provenance"
[[ "$(<$pin_file)" == "${vendor_binary:A}" ]] || fail "installer did not pin the vendor Claude binary"
grep -Fqx '# switchback-owned: native-claude-mode-d-entrypoint@1' "$PREFIX/claude" || fail "installed entrypoint has no ownership marker"
installed_sha="$(shasum -a 256 "$PREFIX/claude" | awk '{print $1}')"
source_sha="$(shasum -a 256 "$CLI_ROOT/entrypoints/claude" | awk '{print $1}')"
[[ "$installed_sha" == "$source_sha" ]] || fail "installed entrypoint differs from tracked source"
jq -e \
  --arg installed "${PREFIX:A}/claude" \
  --arg vendor "${vendor_binary:A}" \
  --arg sha "$installed_sha" \
  '.schema == "switchback/native-claude-entrypoint-provenance@1"
   and .artifact == "native-claude-mode-d-entrypoint@1"
   and .installed_path == $installed
   and .vendor_binary == $vendor
   and .sha256 == $sha
   and .source_sha256 == $sha' \
  "$entrypoint_provenance" >/dev/null || fail "entrypoint provenance is incomplete"

# An owned reinstall updates via the same installer without leaving temp files.
"$INSTALLER" >"${TMPDIR}/install-second.out" 2>"${TMPDIR}/install-second.err"
[[ "$(shasum -a 256 "$PREFIX/claude" | awk '{print $1}')" == "$installed_sha" ]] || fail "owned reinstall changed entrypoint bytes"
[[ -z "$(find "$PREFIX" -maxdepth 1 -name '.claude.*.tmp' -print -quit)" ]] || fail "installer left a partial entrypoint artifact"

# The doctor must use provenance + hashes. Keeping the marker while changing
# the bytes is deliberately insufficient for conformance.
mode_d_index="${SWITCHBACK_RUNTIME_ROOT}/state/mode-d/body/index.sqlite"
mkdir -p "${mode_d_index:h}"
MODE_D_INDEX="$mode_d_index" python3 - <<'PY'
import json
import os
import sqlite3

conn = sqlite3.connect(os.environ["MODE_D_INDEX"])
conn.execute(
    """CREATE TABLE body_events (
        event_id TEXT PRIMARY KEY,
        request_id TEXT NOT NULL,
        observed_at_unix_ms INTEGER NOT NULL,
        capture_stage TEXT NOT NULL,
        metadata_json TEXT NOT NULL
    )"""
)
rows = [
    ("evt-old", "req-mode-d-old", 101, "client_inbound", "/v1/messages"),
    ("evt-new", "req-mode-d-new", 202, "upstream_response", "/v1/messages"),
    ("evt-other", "req-mode-d-other", 303, "client_inbound", "/v1/models"),
]
for event_id, request_id, observed_at, stage, path in rows:
    metadata = json.dumps({"path": path, "proxy_id": "mode-d-test"})
    conn.execute(
        "INSERT INTO body_events VALUES (?, ?, ?, ?, ?)",
        (event_id, request_id, observed_at, stage, metadata),
    )
conn.commit()
conn.close()
PY
[[ ! -e "${SWITCHBACK_RUNTIME_ROOT}/state/tap-bodies.jsonl" ]] || fail "test requires the global legacy body log to be absent"
[[ ! -e "${SWITCHBACK_RUNTIME_ROOT}/state/mode-d/tap-bodies.jsonl" ]] || fail "test requires the Mode D legacy body log to be absent"

doctor_json="$(PATH="${PREFIX}:$PATH" "$CLI_ROOT/sb" capture doctor --json)"
print -r -- "$doctor_json" | jq -e '
  .native_claude_entrypoint.status == "current"
  and .body_capture_health.status == "healthy"
  and .body_status.pressure.mode == "segmented_full_wire"
' >/dev/null || fail "doctor did not accept healthy full-wire capture"
print -r -- "$doctor_json" | jq -e '
  .latest_messages == [
    {
      "request_id": "req-mode-d-old",
      "stage": "client_inbound",
      "lane": "mode-d-test",
      "path": "/v1/messages",
      "observed_at_unix_ms": 101
    },
    {
      "request_id": "req-mode-d-new",
      "stage": "upstream_response",
      "lane": "mode-d-test",
      "path": "/v1/messages",
      "observed_at_unix_ms": 202
    }
  ]
' >/dev/null || fail "doctor did not report latest captures from the Mode D body index"
doctor_text="$(PATH="${PREFIX}:$PATH" "$CLI_ROOT/sb" capture doctor)"
assert_contains "$doctor_text" "req-mode-d-old /v1/messages"
assert_contains "$doctor_text" "req-mode-d-new /v1/messages"

# Capture conformance is more than entrypoint bytes: absent health, degraded
# modes, an unavailable archive, or known queue loss must all return useful JSON
# and a failing process status.
for body_mode in absent metadata-only archive-unavailable queue-drops; do
  set +e
  unhealthy_json="$(FAKE_BODY_MODE="$body_mode" PATH="${PREFIX}:$PATH" "$CLI_ROOT/sb" capture doctor --json)"
  unhealthy_status=$?
  set -e
  [[ "$unhealthy_status" != 0 ]] || fail "capture doctor accepted unhealthy body mode: $body_mode"
  print -r -- "$unhealthy_json" | jq -e --arg mode "$body_mode" '
    .native_claude_entrypoint.status == "current"
    and .body_capture_health.status == "unhealthy"
    and (.body_capture_health.reason | length > 0)
    and (if $mode == "absent" then .body_status == {} else (.body_status | type == "object") end)
  ' >/dev/null || fail "capture doctor did not preserve useful JSON for unhealthy body mode: $body_mode"
done

print -r -- '# switchback-owned: native-claude-mode-d-entrypoint@1' > "$PREFIX/claude"
print -r -- 'exit 0' >> "$PREFIX/claude"
chmod +x "$PREFIX/claude"
set +e
drift_json="$(PATH="${PREFIX}:$PATH" "$CLI_ROOT/sb" capture doctor --json)"
drift_status=$?
set -e
[[ "$drift_status" != 0 ]] || fail "doctor accepted a hash-drifted entrypoint"
print -r -- "$drift_json" | jq -e '.native_claude_entrypoint.status == "hash_mismatch"' >/dev/null || fail "doctor did not report entrypoint hash drift"

# A marker alone does not grant ownership: the installer must refuse the
# drifted/unowned bytes rather than clobbering them.
unowned_before="$(shasum -a 256 "$PREFIX/claude" | awk '{print $1}')"
set +e
"$INSTALLER" >"${TMPDIR}/install-unowned.out" 2>"${TMPDIR}/install-unowned.err"
unowned_status=$?
set -e
[[ "$unowned_status" != 0 ]] || fail "installer overwrote an unowned native Claude entrypoint"
[[ "$(shasum -a 256 "$PREFIX/claude" | awk '{print $1}')" == "$unowned_before" ]] || fail "installer changed the unowned entrypoint"
assert_contains "$(cat "${TMPDIR}/install-unowned.err")" "refusing to overwrite unowned native Claude entrypoint"

print "ok - native Claude entrypoint fails closed and starts Mode D"
