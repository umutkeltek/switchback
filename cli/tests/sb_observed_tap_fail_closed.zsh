#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT

export HOME="${TEST_ROOT}/home"
export SB_LANES="${HOME}/.config/switchback/lanes"
export PATH="${TEST_ROOT}/bin:${PATH}"
export ZAI_API_KEY="test-only"
mkdir -p "$SB_LANES" "${TEST_ROOT}/bin"

cat > "${SB_LANES}/zai.env" <<'EOF'
SB_LANE_NAME="zai"
SB_LANE_ANTHROPIC_URL="https://api.z.ai/api/anthropic"
SB_LANE_KEY_ENV="ZAI_API_KEY"
SB_LANE_MODEL="glm-5.2"
SB_LANE_FAST_MODEL="glm-4.5-air"
SB_LANE_ANTHROPIC_TAP="18772"
SB_LANE_HEADROOM="1"
SB_LANE_HEADROOM_PORT="8790"
SB_LANE_CLAUDE_VIA_TAP="1"
EOF

cat > "${TEST_ROOT}/bin/lsof" <<'EOF'
#!/bin/zsh
exit 1
EOF
chmod +x "${TEST_ROOT}/bin/lsof"
export SB_LSOF_BIN="${TEST_ROOT}/bin/lsof"

cat > "${TEST_ROOT}/bin/claude" <<'EOF'
#!/bin/zsh
print -r -- "unexpected claude execution" >&2
exit 0
EOF
chmod +x "${TEST_ROOT}/bin/claude"

set +e
output="$(zsh "$SB" claude-zai --version 2>&1)"
exit_code=$?
set -e

if (( exit_code == 0 )); then
  print -ru2 -- "FAIL: observed profile bypassed its unavailable tap"
  print -ru2 -- "$output"
  exit 1
fi
if [[ "$output" != *"requires capture tap :18772"* ]]; then
  print -ru2 -- "FAIL: missing fail-closed tap diagnosis"
  print -ru2 -- "$output"
  exit 1
fi
if [[ "$output" == *"unexpected claude execution"* ]]; then
  print -ru2 -- "FAIL: Claude executed after the capture topology failed"
  exit 1
fi

print "ok - observed Claude profiles fail closed when their required tap is unavailable"
