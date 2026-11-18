#!/bin/zsh
# A model lane's tool servers come from one owner: the user-scope "mcpServers"
# in the default ~/.claude.json.
# An explicit spec keeps reading mcp-on-demand.json; a missing or unreadable
# source falls back to the owner list with one warning, never to an empty
# strict list.
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
SB="${SB_UNDER_TEST:-${CLI_ROOT}/sb}"
export HOME="${TMPDIR}/home"
export SWITCHBACK_RUNTIME_ROOT="${HOME}/.switchback"
unset SB_RUNTIME_ROOT SB_CLAUDE_USER_CONFIG_DIR SB_CLAUDE_DEFAULT_GLOBAL_CONFIG SB_LAUNCH_PROFILE_ID \
  SB_LAUNCH_PROFILE_CONFIG SB_CLAUDE_PROVIDER_MCP SB_CLAUDE_PROVIDER_MCP_SERVERS 2>/dev/null || true
export SB_SOURCE_ONLY=1
mkdir -p "${SWITCHBACK_RUNTIME_ROOT}/config" "${HOME}/.claude" "${TMPDIR}/bin"
print -r -- '{}' > "${SWITCHBACK_RUNTIME_ROOT}/manifest.json"
source "$SB"

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
names() { jq -r '.mcpServers | keys | join(",")' "$1"; }
lines() { [[ -s "$1" ]] && wc -l < "$1" | tr -d ' ' || print 0; }

owner="${HOME}/.claude.json"
cat > "$owner" <<'EOF'
{
  "numStartups": 3,
  "oauthAccount": {"emailAddress": "owner@example.invalid"},
  "projects": {"/repo": {"mcpServers": {"project-only": {"command": "project-mcp"}}}},
  "mcpServers": {
    "search": {"type": "stdio", "command": "search-mcp", "env": {"SEARCH_TOKEN": "${SEARCH_TOKEN}"}},
    "browse": {"type": "stdio", "command": "browse-mcp", "args": ["--port", "${BROWSE_PORT:-9}"]},
    "compound": {"type": "http", "url": "http://127.0.0.1:1/mcp", "headers": {"Authorization": "Bearer ${COMPOUND_TOKEN}"}}
  }
}
EOF
export SEARCH_TOKEN="expanded-search-value" COMPOUND_TOKEN="expanded-compound-value" BROWSE_PORT=4444

lane="${CLAUDE_PROFILES}/_providers/minimax-lane"
plain="${CLAUDE_PROFILES}/_providers/plain-lane"
generated="${lane}/switchback-mcp.generated.json"
err="${TMPDIR}/err"

# 1. No spec: the owner list, values passed through as they are.
out="$(_claude_on_demand_mcp_config "$lane" "" 2>"$err")"
[[ "$out" == "$generated" ]] || fail "default spec should write ${generated}, got '${out}'"
[[ "$(names "$out")" == "browse,compound,search" ]] || fail "default spec should carry the owner list, got $(names "$out")"
jq -e 'keys == ["mcpServers"]' "$out" >/dev/null || fail "generated file must hold only mcpServers"
jq -e '.mcpServers.search.env.SEARCH_TOKEN == "${SEARCH_TOKEN}"' "$out" >/dev/null || fail "env reference must stay a reference"
jq -e '.mcpServers.compound.headers.Authorization == "Bearer ${COMPOUND_TOKEN}"' "$out" >/dev/null || fail "header reference must stay a reference"
jq -e '.mcpServers.browse.args == ["--port", "${BROWSE_PORT:-9}"]' "$out" >/dev/null || fail "arg reference must stay a reference"
! grep -q 'expanded-' "$out" || fail "an env value was expanded into the generated file"
[[ "$(ls -l "$out" | cut -c1-10)" == "-rw-------" ]] || fail "generated file must be private"
[[ "$(lines "$err")" == 0 ]] || fail "default spec should not warn: $(cat "$err")"

# 2. A launch profile has no per-lane exclusion list: a stray `mcp_exclude` in an
#    old authority file trims nothing, found by id or by the home's label.
cat > "${SB_LAUNCH_PROFILES}" <<'EOF'
{
  "schema": "switchback/launch-profiles@1",
  "harness_presets": {
    "claude-rich-minimax": {"harness": "claude-code", "mcp_mode": "all", "mcp_exclude": ["browse", "not-configured"]},
    "claude-plain": {"harness": "claude-code", "mcp_mode": "all"}
  },
  "launch_profiles": {
    "claude-minimax": {"harness_preset": "claude-rich-minimax", "profile_label": "minimax-lane"},
    "claude-plain": {"harness_preset": "claude-plain", "profile_label": "plain-lane"}
  }
}
EOF
out="$(SB_LAUNCH_PROFILE_ID=claude-minimax _claude_on_demand_mcp_config "$lane" "" 2>"$err")"
[[ "$(names "$out")" == "browse,compound,search" ]] || fail "mcp_exclude must not trim the owner list by profile id: $(names "$out")"
out="$(_claude_on_demand_mcp_config "$lane" "" 2>"$err")"
[[ "$(names "$out")" == "browse,compound,search" ]] || fail "mcp_exclude must not trim the owner list by label: $(names "$out")"
(( ! $+functions[_claude_provider_mcp_exclude] )) || fail "the mcp_exclude reader must be gone"

# 3. An explicit spec keeps today's source and behaviour (secret placeholders resolved).
cat > "${HOME}/.claude/mcp-on-demand.json" <<'EOF'
{
  "claude": {
    "context7": {"command": "npx", "env": {"CONTEXT7_API_KEY": {"$secret": {"provider": "env", "id": "CONTEXT7_API_KEY"}}}},
    "gbrain": {"command": "gbrain-mcp"},
    "browse": {"command": "browse-mcp"}
  },
  "codex": {}
}
EOF
export CONTEXT7_API_KEY="fake-context7-key"
out="$(_claude_on_demand_mcp_config "$plain" all 2>"$err")"
[[ "$(names "$out")" == "browse,context7,gbrain" ]] || fail "--mcp-all should read mcp-on-demand.json: $(names "$out")"
jq -e '.mcpServers.context7.env.CONTEXT7_API_KEY == "fake-context7-key"' "$out" >/dev/null || fail "explicit spec behaviour changed"
# Nothing trims `all`; an explicit list is taken as written.
out="$(_claude_on_demand_mcp_config "$lane" all 2>"$err")"
[[ "$(names "$out")" == "browse,context7,gbrain" ]] || fail "mcp_exclude must not trim --mcp-all: $(names "$out")"
out="$(_claude_on_demand_mcp_config "$lane" "gbrain,browse" 2>"$err")"
[[ "$(names "$out")" == "browse,gbrain" ]] || fail "--mcp=gbrain,browse should select both: $(names "$out")"
[[ "$(lines "$err")" == 0 ]] || fail "explicit spec with a good source should not warn: $(cat "$err")"
print -r -- '{"mcpServers": {}}' > "${HOME}/.claude/.mcp.json"
out="$(_claude_on_demand_mcp_config "$lane" legacy 2>"$err")"
[[ "$out" == "${HOME}/.claude/.mcp.json" ]] || fail "an existing legacy file is passed as before"

# 4. A missing source falls back to the owner list with exactly one warning naming the path.
mv "${HOME}/.claude/mcp-on-demand.json" "${TMPDIR}/mcp-on-demand.json.away"
out="$(_claude_on_demand_mcp_config "$lane" all 2>"$err")"
[[ "$(names "$out")" == "browse,compound,search" ]] || fail "missing source must fall back to the owner list: '$(names "$out" 2>/dev/null)'"
[[ "$(lines "$err")" == 1 ]] || fail "missing source must warn exactly once: $(cat "$err")"
grep -qF "${HOME}/.claude/mcp-on-demand.json" "$err" || fail "warning must name the missing path: $(cat "$err")"
out="$(_claude_on_demand_mcp_config "$lane" "context7,gbrain" 2>"$err")"
[[ "$(names "$out")" == "browse,compound,search" ]] || fail "missing source with a list must fall back too"

# 5. An unreadable source does the same.
print -r -- 'not json' > "${HOME}/.claude/mcp-on-demand.json"
out="$(_claude_on_demand_mcp_config "$lane" all 2>"$err")"
[[ "$(names "$out")" == "browse,compound,search" ]] || fail "unreadable source must fall back to the owner list"
[[ "$(lines "$err")" == 1 ]] && grep -qF "missing or unreadable" "$err" || fail "unreadable source must warn once: $(cat "$err")"

# 6. A missing legacy file falls back too.
mv "${HOME}/.claude/.mcp.json" "${TMPDIR}/legacy.away"
out="$(_claude_on_demand_mcp_config "$lane" legacy 2>"$err")"
[[ "$(names "$out")" == "browse,compound,search" ]] || fail "missing legacy file must fall back to the owner list"
grep -qF "${HOME}/.claude/.mcp.json" "$err" || fail "warning must name the missing legacy path"

# 7. No owner either: no file is offered, and the lane is told why. A stale
#    generated file from an earlier launch is never handed out.
mv "$owner" "${TMPDIR}/owner.away"
if out="$(_claude_on_demand_mcp_config "$lane" "" 2>"$err")"; then fail "no owner must not produce a config"; fi
[[ -z "$out" ]] || fail "no owner must print no path"
grep -qF "$owner" "$err" || fail "warning must name the missing owner file"
if out="$(_claude_on_demand_mcp_config "$lane" all 2>"$err")"; then fail "no source and no owner must not produce a config"; fi
[[ -z "$out" ]] || fail "no source and no owner must print no path"

# 8. An owner with no servers: no strict empty list.
cat > "$owner" <<'EOF'
{"mcpServers": {}}
EOF
if out="$(_claude_on_demand_mcp_config "$lane" "" 2>"$err")"; then
  fail "an empty owner list must not produce a config"
fi
grep -qF "no user-scope MCP servers in ${owner}" "$err" || fail "an empty owner list must warn: $(cat "$err")"

# 9. The launch: a lane passes the owner file with --strict-mcp-config, and passes
#    nothing strict when there is no list at all.
cat > "$owner" <<'EOF'
{"mcpServers": {"search": {"command": "search-mcp"}, "browse": {"command": "browse-mcp"}}}
EOF
fake="${TMPDIR}/bin/claude"
cat > "$fake" <<'EOF'
#!/bin/zsh
print -r -- "ARGS=$*" >> "$FAKE_LOG"
EOF
chmod +x "$fake"
export FAKE_LOG="${TMPDIR}/launch.log"
launch() {
  ( NATIVE_CLAUDE="$fake"
    _exec_claude_provider_mode minimax-lane http://127.0.0.1:9 lane-token m m m m high "" "lane" "lane" "$@" ) 2>"$err"
}
launch --mcp --version
log="$(cat "$FAKE_LOG")"
[[ "$log" == *"--mcp-config ${generated} --strict-mcp-config"* ]] || fail "default --mcp must pass the owner file strictly: ${log}"
[[ "$(names "$generated")" == "browse,search" ]] || fail "launch should pass the whole owner list: $(names "$generated")"
: > "$FAKE_LOG"
mv "$owner" "${TMPDIR}/owner2.away"
launch --mcp --version
log="$(cat "$FAKE_LOG")"
[[ "$log" != *"--strict-mcp-config"* ]] || fail "no owner list must not launch with a strict MCP list: ${log}"
grep -qF "this lane starts without --mcp-config" "$err" || fail "launch without a list must say so: $(cat "$err")"

print "ok - sb lane MCP list derives from the user-scope owner"
