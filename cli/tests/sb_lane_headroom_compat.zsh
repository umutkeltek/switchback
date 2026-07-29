#!/bin/zsh
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
SB="${SB_UNDER_TEST:-${CLI_ROOT}/sb}"
export HOME="${TMPDIR}/home"
export PATH="${TMPDIR}/bin:${PATH}"
export SWITCHBACK_RUNTIME_ROOT="${HOME}/.config/switchback"
export SB_CONFIG="${SWITCHBACK_RUNTIME_ROOT}/switchback.yaml"
export SB_ENV="${SWITCHBACK_RUNTIME_ROOT}/sb.env"
export SB_STATE="${SWITCHBACK_RUNTIME_ROOT}/state"
export SB_LANES="${SWITCHBACK_RUNTIME_ROOT}/lanes"
export SB_LSOF_BIN="${TMPDIR}/bin/lsof"
export SB_SOURCE_ONLY=1

mkdir -p "${TMPDIR}/bin" "${HOME}/.local/bin" "$SB_LANES" \
  "${HOME}/Library/LaunchAgents"

cat > "${HOME}/.local/bin/headroom" <<'FAKE'
#!/bin/zsh
exit 0
FAKE
cat > "${TMPDIR}/bin/launchctl" <<'FAKE'
#!/bin/zsh
exit 0
FAKE
cat > "$SB_LSOF_BIN" <<'FAKE'
#!/bin/zsh
exit 0
FAKE
chmod +x "${HOME}/.local/bin/headroom" "${TMPDIR}/bin/launchctl" "$SB_LSOF_BIN"

cat > "${SB_LANES}/minimax.env" <<'LANE'
SB_LANE_ANTHROPIC_URL="https://api.minimax.invalid/anthropic"
LANE

default_plist="${HOME}/Library/LaunchAgents/com.headroom.default.plist"
cat > "$default_plist" <<'PLIST'
<plist>
<dict>
  <key>HEADROOM_TOOL_SEARCH</key><string>true</string>
</dict>
</plist>
PLIST
default_before="$(shasum -a 256 "$default_plist")"

# Reproduce the launchd leak: a global opt-in must not activate Anthropic
# server-side tool search in a third-party provider's isolated Headroom proxy.
export HEADROOM_TOOL_SEARCH=true
source "$SB"
sleep() { :; }
_lane_headroom_up minimax --port 8789 >/dev/null

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
lane_plist="${HOME}/Library/LaunchAgents/com.switchback.headroom-minimax.plist"
[[ -f "$lane_plist" ]] || fail "lane Headroom plist was not generated"

tool_search_guard='<key>HEADROOM_TOOL_SEARCH</key><string>0</string>'
guard_count="$(grep -Fc "$tool_search_guard" "$lane_plist" || true)"
[[ "$guard_count" == "1" ]] || \
  fail "expected exactly one provider-scoped tool-search guard; got ${guard_count}"
grep -Fq '<key>ANTHROPIC_TARGET_API_URL</key><string>https://api.minimax.invalid/anthropic</string>' \
  "$lane_plist" || fail "MiniMax upstream was not preserved"
grep -Fq '<string>--no-memory-tools</string>' "$lane_plist" || \
  fail "ordinary Headroom arguments were not preserved"
grep -Fq '<string>--no-ccr</string>' "$lane_plist" || \
  fail "CCR disablement was not preserved"

default_after="$(shasum -a 256 "$default_plist")"
[[ "$default_after" == "$default_before" ]] || \
  fail "lane setup modified the default Headroom profile"

print "ok - lane Headroom applies provider compatibility guard"
