#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export PATH="${TMPDIR}/bin:${PATH}"
export FAKE_LOG="${TMPDIR}/shortcuts.log"
export ZAI_API_KEY="fake-zai-key"
export NEURALWATT_API_KEY="fake-neuralwatt-key"
export OPENCODE_GO_API_KEY="fake-opencode-go-key"
export CONTEXT7_API_KEY="fake-context7-key"
mkdir -p "${HOME}/.config/switchback/lanes" "${HOME}/.claude/skills/sb-smoke" "${TMPDIR}/bin"
print -r -- "{}" > "${HOME}/.claude/.mcp.json"
cat > "${HOME}/.claude/mcp-on-demand.json" <<'EOF'
{
  "claude": {
    "gbrain": {
      "type": "stdio",
      "command": "/tmp/fake-gbrain",
      "args": ["serve"],
      "env": {"HOME": "/tmp/test-home"}
    },
    "context7": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@upstash/context7-mcp@latest"],
      "env": {"CONTEXT7_API_KEY": {"$secret": {"provider": "env", "id": "CONTEXT7_API_KEY"}}}
    }
  },
  "codex": {}
}
EOF
cat > "${HOME}/.claude/settings.json" <<'EOF'
{
  "permissions": {
    "allow": ["Read", "Bash(git status)"],
    "deny": [],
    "ask": [],
    "defaultMode": "auto"
  },
  "autoMode": {
    "allow": ["Running safe commands is auto-approved in tests"]
  },
  "skipAutoPermissionPrompt": true,
  "skipDangerousModePermissionPrompt": true,
  "skipWorkflowUsageWarning": true,
  "env": {
    "ANTHROPIC_BASE_URL": "https://should-not-copy.example",
    "ANTHROPIC_AUTH_TOKEN": "should-not-copy"
  }
}
EOF
cat > "${HOME}/.claude/skills/sb-smoke/SKILL.md" <<'EOF'
---
name: sb-smoke
description: Switchback provider-mode test skill
---

Reply exactly: SB_SMOKE_SKILL
EOF

cat > "${HOME}/.config/switchback/lanes/zai.env" <<'EOF'
SB_LANE_NAME="zai"
SB_LANE_ANTHROPIC_URL="https://api.z.ai/api/anthropic"
SB_LANE_OPENAI_URL="https://api.z.ai/api/coding/paas/v4"
SB_LANE_KEY_ENV="ZAI_API_KEY"
SB_LANE_MODEL="glm-5.2"
SB_LANE_FAST_MODEL="glm-4.5-air"
SB_LANE_WIRE_API="chat"
SB_LANE_ANTHROPIC_TAP="18772"
SB_LANE_ROUTE="zai/glm-5.2"
SB_LANE_CODEX_ROUTE="zai/glm-5.2-direct"
SB_LANE_HEADROOM="1"
SB_LANE_CLAUDE_HEADROOM_BYPASS="1"
SB_LANE_DIRECT_ROUTE="zai/glm-5.2-direct"
EOF

cat > "${HOME}/.config/switchback/lanes/neuralwatt.env" <<'EOF'
SB_LANE_NAME="neuralwatt"
SB_LANE_ANTHROPIC_URL=""
SB_LANE_OPENAI_URL="https://api.neuralwatt.com/v1"
SB_LANE_KEY_ENV="NEURALWATT_API_KEY"
SB_LANE_MODEL="glm-5.2"
SB_LANE_FAST_MODEL="glm-5.2-fast"
SB_LANE_WIRE_API="chat"
SB_LANE_ANTHROPIC_TAP=""
SB_LANE_ROUTE="neuralwatt/glm-5.2"
EOF

cat > "${HOME}/.config/switchback/lanes/opencode-go.env" <<'EOF'
SB_LANE_NAME="opencode-go"
SB_LANE_ANTHROPIC_URL=""
SB_LANE_OPENAI_URL="https://opencode.ai/zen/go/v1"
SB_LANE_KEY_ENV="OPENCODE_GO_API_KEY"
SB_LANE_MODEL="glm-5.2"
SB_LANE_FAST_MODEL="deepseek-v4-flash"
SB_LANE_WIRE_API="chat"
SB_LANE_ANTHROPIC_TAP=""
SB_LANE_ROUTE="opencode-go/glm-5.2"
EOF

cat > "${TMPDIR}/bin/switchback" <<'FAKE'
#!/bin/zsh
if [[ "${1:-}" == "config" && "${2:-}" == "get" && "${3:-}" == "providers" ]]; then
  print -r -- '[{"id":"opencode-go-coding","accounts":[{"auth":{"env":"OPENCODE_GO_API_KEY"}}]}]'
exit 0
fi
if [[ "${1:-}" == "config" && "${2:-}" == "get" && "${3:-}" == "routes" ]]; then
print -r -- '[{"name":"local-mac-code","match":{"model":"local/mac-code"},"targets":["mac/qwen/qwen3.6-27b"]},{"name":"local-mac-fast","match":{"model":"local/mac-fast"},"targets":["mac/qwen/qwen3.6-35b-a3b"]}]'
exit 0
fi
if [[ "${1:-}" == "config" && "${2:-}" == "set" && "${3:-}" == "routes" ]]; then
print -r -- "SWITCHBACK_SET_ROUTES=$4" >> "$FAKE_LOG"
print -r -- '{"ok":true}'
exit 0
fi
if [[ "${1:-}" == "config" && "${2:-}" == "validate" ]]; then
print -r -- '{"ok":true}'
exit 0
fi
exit 1
FAKE
chmod +x "${TMPDIR}/bin/switchback"

cat > "${TMPDIR}/bin/curl" <<'FAKE'
#!/bin/zsh
if [[ "$*" == *"/cp/v1/route-preview"* ]]; then
  if [[ "${FAKE_ROUTE_PREVIEW_MISS:-0}" == "1" ]]; then
    print -r -- '{"decision":{"selected":{"target_id":"zai/glm-5.2"}}}'
    exit 0
  fi
  payload="$*"
  model="${payload#*\"model\":\"}"
  model="${model%%\"*}"
  print -r -- "{\"decision\":{\"selected\":{\"target_id\":\"${model}\"}}}"
fi
exit 0
FAKE
chmod +x "${TMPDIR}/bin/curl"

cat > "${TMPDIR}/bin/lms" <<'FAKE'
#!/bin/zsh
if [[ "${1:-}" == ps ]]; then
if [[ "${FAKE_LMS_LOADED:-0}" == "1" ]]; then
print "IDENTIFIER                                    MODEL                                         STATUS    SIZE        CONTEXT    PARALLEL    DEVICE    TTL"
print "qwen3.6-27b-uncensored-hauhaucs-aggressive    qwen3.6-27b-uncensored-hauhaucs-aggressive    IDLE      18.46 GB    262144     4           Local"
else
print "No models are currently loaded."
fi
exit 0
fi
exit 0
FAKE
chmod +x "${TMPDIR}/bin/lms"

cat > "${TMPDIR}/bin/codex" <<'FAKE'
#!/bin/zsh
print -r -- "CODEX_HOME=${CODEX_HOME:-}" >> "$FAKE_LOG"
print -r -- "CODEX_API_KEY=${OPENAI_API_KEY:+set}" >> "$FAKE_LOG"
print -r -- "CODEX_ARGS=$*" >> "$FAKE_LOG"
FAKE
chmod +x "${TMPDIR}/bin/codex"

cat > "${TMPDIR}/bin/claude" <<'FAKE'
#!/bin/zsh
print -r -- "CLAUDE_BASE=${ANTHROPIC_BASE_URL:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_OPUS=${ANTHROPIC_DEFAULT_OPUS_MODEL:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_SONNET=${ANTHROPIC_DEFAULT_SONNET_MODEL:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_HAIKU=${ANTHROPIC_DEFAULT_HAIKU_MODEL:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_MODEL=${ANTHROPIC_MODEL:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_CUSTOM=${ANTHROPIC_CUSTOM_MODEL_OPTION:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_CUSTOM_NAME=${ANTHROPIC_CUSTOM_MODEL_OPTION_NAME:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_GATEWAY_DISCOVERY=${CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_CUSTOM_HEADERS=${ANTHROPIC_CUSTOM_HEADERS:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_EFFORT_ENV=${CLAUDE_CODE_EFFORT_LEVEL:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR:-}" >> "$FAKE_LOG"
print -r -- "CLAUDE_API_KEY=${ANTHROPIC_API_KEY:+set}" >> "$FAKE_LOG"
print -r -- "CLAUDE_AUTH_TOKEN=${ANTHROPIC_AUTH_TOKEN:+set}" >> "$FAKE_LOG"
print -r -- "CLAUDE_ARGS=$*" >> "$FAKE_LOG"
FAKE
chmod +x "${TMPDIR}/bin/claude"

cat > "${TMPDIR}/bin/opencode" <<'FAKE'
#!/bin/zsh
print -r -- "OPENCODE_ARGS=$*" >> "$FAKE_LOG"
FAKE
chmod +x "${TMPDIR}/bin/opencode"

fail() { print -ru2 -- "FAIL: $*"; exit 1; }
assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output contain: $needle\nactual:\n$haystack"
}

run_sb() { zsh "$SB" "$@" >/tmp/sb-provider-shortcuts.out 2>/tmp/sb-provider-shortcuts.err; }

run_sb codex-zai --version
run_sb codex-zai-direct --version
run_sb run codex --with zai --version
run_sb run codex --with zai-direct --version
run_sb claude-zai --version
run_sb claude-zai-direct --version
run_sb run claude --with zai --version
run_sb run claude --with zai-direct --version
run_sb codex-neuralwatt --version
run_sb claude-neuralwatt --version
run_sb codex-neuralwatt --qwen-fast --version
run_sb claude-neuralwatt --kimi-code --version
run_sb run codex --with neuralwatt --version
run_sb run claude --with neuralwatt --version
run_sb codex-nvidia-build --version
run_sb codex-nvidia-build --minimax-m3-direct --version
run_sb claude-nvidia-build --long --rich --version
run_sb run codex --with nvidia-build --multimodal --version
run_sb run claude --with nvidia-build --chat --mcp=gbrain --version
run_sb codex-openrouter-free --version
run_sb codex-openrouter-free --router --version
run_sb claude-openrouter-free --multimodal --skills --version
run_sb run codex --with openrouter-free --long --version
run_sb run claude --with openrouter-free --chat --mcp=gbrain --version
run_sb codex-opencode-go --version
run_sb codex-opencode-go --fast --version
run_sb claude-opencode-go --fast --version
run_sb opencode-go --kimi-code --help
run_sb run codex --with opencode-go --version
run_sb run claude --with opencode-go --version
run_sb run opencode --with opencode-go --fast --help
run_sb codex-lmstudio --version
run_sb claude-lmstudio --fast --version
run_sb claude-lmstudio --mcp --skills --version
run_sb claude-zai --mcp=context7,gbrain --version
run_sb opencode-lmstudio --help
run_sb modes generate --dir "${TMPDIR}/generated"
FAKE_LMS_LOADED=1 zsh "$SB" local use code qwen3.6-27b-uncensored-hauhaucs-aggressive >"${TMPDIR}/local-use.out" 2>"${TMPDIR}/local-use.err"
ANTHROPIC_BASE_URL="https://bad.example" ANTHROPIC_API_KEY="bad" ANTHROPIC_AUTH_TOKEN="bad" run_sb claude --mode native --version
ANTHROPIC_API_KEY="bad" ANTHROPIC_AUTH_TOKEN="bad" run_sb claude --mode tap --version

log="$(cat "$FAKE_LOG")"
local_use_out="$(cat "${TMPDIR}/local-use.out")"
lmstudio_mcp="${HOME}/.config/switchback/claude/_providers/gateway-lmstudio/switchback-mcp.generated.json"
zai_mcp="${HOME}/.config/switchback/claude/_providers/zai-lane/switchback-mcp.generated.json"
assert_contains "$local_use_out" "local/mac-code"
assert_contains "$local_use_out" "mac/qwen3.6-27b-uncensored-hauhaucs-aggressive"
assert_contains "$log" "SWITCHBACK_SET_ROUTES="
assert_contains "$log" "mac/qwen3.6-27b-uncensored-hauhaucs-aggressive"
assert_contains "$log" 'model="zai/glm-5.2-direct"'
assert_contains "$log" 'model="local/mac-code"'
assert_contains "$log" 'model="neuralwatt/glm-5.2"'
assert_contains "$log" 'model="neuralwatt/qwen3.6-35b-fast"'
assert_contains "$log" 'CLAUDE_OPUS=neuralwatt/kimi-k2.7-code'
assert_contains "$log" 'model="nvidia/free-code"'
assert_contains "$log" 'model="nvidia/minimaxai/minimax-m3"'
assert_contains "$log" 'model="nvidia/free-multimodal"'
assert_contains "$log" 'CLAUDE_OPUS=nvidia/free-long-context'
assert_contains "$log" 'CLAUDE_OPUS=nvidia/free-chat'
assert_contains "$log" 'model="openrouter/free-code"'
assert_contains "$log" 'model="openrouter/openrouter/free"'
assert_contains "$log" 'model="openrouter/free-long-context"'
assert_contains "$log" 'CLAUDE_OPUS=openrouter/free-multimodal'
assert_contains "$log" 'CLAUDE_OPUS=openrouter/free-chat'
assert_contains "$log" 'model="opencode-go/glm-5.2"'
assert_contains "$log" 'model="opencode-go/deepseek-v4-flash"'
assert_contains "$log" 'model_reasoning_effort="xhigh"'
assert_contains "$log" 'CLAUDE_OPUS=opencode-go/deepseek-v4-flash'
assert_contains "$log" 'OPENCODE_ARGS=-m opencode-go/kimi-k2.7-code --help'
assert_contains "$log" 'OPENCODE_ARGS=-m opencode-go/deepseek-v4-flash --help'
assert_contains "$log" "CODEX_HOME=${HOME}/.config/switchback/codex/_providers/zai-lane"
assert_contains "$log" "CODEX_HOME=${HOME}/.config/switchback/codex/_providers/zai-direct"
assert_contains "$log" "CODEX_HOME=${HOME}/.config/switchback/codex/_providers/gateway-neuralwatt"
assert_contains "$log" "CODEX_HOME=${HOME}/.config/switchback/codex/_providers/gateway-opencode-go"
assert_contains "$log" "CODEX_HOME=${HOME}/.config/switchback/codex/_providers/gateway-lmstudio"
assert_contains "$log" "CODEX_API_KEY="
assert_contains "$log" "CLAUDE_BASE=http://127.0.0.1:18772"
assert_contains "$log" "CLAUDE_BASE=https://api.z.ai/api/anthropic"
assert_contains "$log" "CLAUDE_OPUS=glm-5.2[1m]"
assert_contains "$log" "CLAUDE_SONNET=glm-5.2[1m]"
assert_contains "$log" "CLAUDE_HAIKU=glm-4.5-air"
assert_contains "$log" "CLAUDE_CUSTOM=glm-5.2[1m]"
assert_contains "$log" "CLAUDE_CUSTOM_NAME=zai glm-5.2[1m]"
assert_contains "$log" "CLAUDE_GATEWAY_DISCOVERY=1"
assert_contains "$log" "CLAUDE_CUSTOM_HEADERS=x-headroom-bypass: true"
assert_contains "$log" "CLAUDE_MODEL="
assert_contains "$log" "CLAUDE_EFFORT_ENV="
assert_contains "$log" "CLAUDE_CONFIG_DIR=${HOME}/.config/switchback/claude/_providers/zai-lane"
assert_contains "$log" "CLAUDE_CONFIG_DIR=${HOME}/.config/switchback/claude/_providers/zai-direct"
assert_contains "$log" "CLAUDE_API_KEY="
assert_contains "$log" "CLAUDE_BASE=http://127.0.0.1:18765"
assert_contains "$log" "CLAUDE_OPUS=local/mac-fast"
assert_contains "$log" "CLAUDE_CONFIG_DIR=${HOME}/.config/switchback/claude/_providers/gateway-lmstudio"
assert_contains "$log" "CLAUDE_CONFIG_DIR=${HOME}/.config/switchback/claude/_providers/gateway-neuralwatt"
assert_contains "$log" "CLAUDE_CONFIG_DIR=${HOME}/.config/switchback/claude/_providers/gateway-opencode-go"
assert_contains "$log" "--setting-sources user"
assert_contains "$log" "--mcp-config ${lmstudio_mcp} --strict-mcp-config"
assert_contains "$log" "--mcp-config ${zai_mcp} --strict-mcp-config"
jq -e '.mcpServers | keys == ["gbrain"]' "$lmstudio_mcp" >/dev/null || fail "default --mcp should generate only gbrain"
jq -e '(.mcpServers | keys | sort) == ["context7", "gbrain"]' "$zai_mcp" >/dev/null || fail "--mcp=context7,gbrain should generate selected servers"
jq -e '.mcpServers.context7.env.CONTEXT7_API_KEY == "fake-context7-key"' "$zai_mcp" >/dev/null || fail "Claude MCP config should resolve env secret placeholders"
assert_contains "$log" "--add-dir ${HOME}/.config/switchback/claude/_providers/gateway-lmstudio/switchback-user-skills"
[[ -L "${HOME}/.config/switchback/claude/_providers/gateway-lmstudio/switchback-user-skills/.claude/skills" ]] || fail "expected provider skill mount"
[[ "$(readlink "${HOME}/.config/switchback/claude/_providers/gateway-lmstudio/switchback-user-skills/.claude/skills")" == "${HOME}/.claude/skills" ]] || fail "provider skill mount points at wrong target"
[[ -L "${HOME}/.config/switchback/claude/_providers/gateway-lmstudio/skills" ]] || fail "expected provider profile skills link"
[[ "$(readlink "${HOME}/.config/switchback/claude/_providers/gateway-lmstudio/skills")" == "${HOME}/.claude/skills" ]] || fail "provider profile skills points at wrong target"
assert_contains "$log" "CLAUDE_AUTH_TOKEN="

zai_settings="$(cat "${HOME}/.config/switchback/claude/_providers/zai-lane/settings.json")"
assert_contains "$zai_settings" '"model": "glm-5.2[1m]"'
assert_contains "$zai_settings" '"effortLevel": "xhigh"'
assert_contains "$zai_settings" '"CLAUDE_CODE_AUTO_COMPACT_WINDOW": "1000000"'
assert_contains "$zai_settings" '"ANTHROPIC_DEFAULT_OPUS_MODEL": "glm-5.2[1m]"'
assert_contains "$zai_settings" '"ANTHROPIC_DEFAULT_SONNET_MODEL": "glm-5.2[1m]"'
assert_contains "$zai_settings" '"ANTHROPIC_DEFAULT_HAIKU_MODEL": "glm-4.5-air"'
assert_contains "$zai_settings" '"defaultMode": "auto"'
assert_contains "$zai_settings" '"skipAutoPermissionPrompt": true'

unset OPENCODE_GO_API_KEY
status_out="$(zsh "$SB" status 2>&1)"
export OPENCODE_GO_API_KEY="fake-opencode-go-key"
assert_contains "$status_out" "lane opencode-go:"
assert_contains "$status_out" "key✓"

[[ -x "${TMPDIR}/generated/codex-opencode-go" ]] || fail "expected generated codex-opencode-go wrapper"
[[ -x "${TMPDIR}/generated/codex-nvidia-build" ]] || fail "expected generated codex-nvidia-build wrapper"
[[ -x "${TMPDIR}/generated/claude-openrouter-free-full" ]] || fail "expected generated claude-openrouter-free-full wrapper"
assert_contains "$(cat "${TMPDIR}/generated/codex-opencode-go")" 'exec sb run codex --with opencode-go "$@"'
assert_contains "$(cat "${TMPDIR}/generated/codex-nvidia-build")" 'exec sb run codex --with nvidia-build "$@"'
assert_contains "$(cat "${TMPDIR}/generated/claude-openrouter-free-full")" 'exec sb run claude --with openrouter-free --rich "$@"'
assert_contains "$log" "CLAUDE_ARGS=--setting-sources project,local --version"
assert_contains "$log" "OPENCODE_ARGS=-m lmstudio/qwen/qwen3-coder-30b --help"

cat > "${HOME}/.config/switchback/lanes/zai.env" <<'EOF'
SB_LANE_NAME="zai"
SB_LANE_ANTHROPIC_URL="http://127.0.0.1:8787 -> https://api.z.ai/api/anthropic"
SB_LANE_OPENAI_URL="http://127.0.0.1:8787/v1 -> https://api.z.ai/api/coding/paas/v4"
SB_LANE_KEY_ENV="ZAI_API_KEY"
SB_LANE_MODEL="glm-5.2"
SB_LANE_FAST_MODEL="glm-4.5-air"
SB_LANE_WIRE_API="chat"
SB_LANE_ANTHROPIC_TAP="18772"
SB_LANE_ROUTE="zai/glm-5.2"
SB_LANE_CODEX_ROUTE="zai/glm-5.2-direct"
SB_LANE_HEADROOM="1"
SB_LANE_DIRECT_ANTHROPIC_TAP="65432"
SB_LANE_DIRECT_ROUTE="zai/glm-5.2-direct"
EOF

if zsh "$SB" claude-zai --version >"${TMPDIR}/headroom-guard.out" 2>"${TMPDIR}/headroom-guard.err"; then
  fail "claude-zai should refuse when the Headroom-backed lane is not activated"
fi
guard_err="$(cat "${TMPDIR}/headroom-guard.err")"
assert_contains "$guard_err" "Headroom lane is configured but not activated"
assert_contains "$guard_err" "claude-zai-direct"
assert_contains "$guard_err" "Do not run /login"

if FAKE_ROUTE_PREVIEW_MISS=1 zsh "$SB" codex-zai-direct --version >"${TMPDIR}/codex-direct-guard.out" 2>"${TMPDIR}/codex-direct-guard.err"; then
  fail "codex-zai-direct should refuse when the direct route is not activated"
fi
codex_guard_err="$(cat "${TMPDIR}/codex-direct-guard.err")"
assert_contains "$codex_guard_err" "zai/glm-5.2-direct is configured on disk but not active"
assert_contains "$codex_guard_err" "sb reload"

print "ok - sb provider shortcuts"
