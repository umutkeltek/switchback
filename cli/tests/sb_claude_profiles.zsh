#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export PATH="${TMPDIR}/bin:${PATH}"
export SB_DEFAULT_CLAUDE_MODE="native"
export SB_NATIVE_CLAUDE="${TMPDIR}/bin/claude"
export FAKE_CLAUDE_LOG="${TMPDIR}/claude.log"
export FAKE_TAIL_LOG="${TMPDIR}/tail.log"

mkdir -p "$HOME" "${TMPDIR}/bin" "${HOME}/.claude/agents"
print -r -- "global memory" > "${HOME}/.claude/CLAUDE.md"
print -r -- "agent spec" > "${HOME}/.claude/agents/reviewer.md"

cat > "$SB_NATIVE_CLAUDE" <<'FAKE'
#!/bin/zsh
set -euo pipefail
print -r -- "CLAUDE_CONFIG_DIR=${CLAUDE_CONFIG_DIR:-}" > "$FAKE_CLAUDE_LOG"
print -r -- "ANTHROPIC_BASE_URL=${ANTHROPIC_BASE_URL:-}" >> "$FAKE_CLAUDE_LOG"
print -r -- "HTTPS_PROXY=${HTTPS_PROXY:-}" >> "$FAKE_CLAUDE_LOG"
print -r -- "NODE_EXTRA_CA_CERTS=${NODE_EXTRA_CA_CERTS:-}" >> "$FAKE_CLAUDE_LOG"
print -r -- "ARGS=$*" >> "$FAKE_CLAUDE_LOG"
FAKE
chmod +x "$SB_NATIVE_CLAUDE"

cat > "${TMPDIR}/bin/tail" <<'FAKE'
#!/bin/zsh
set -euo pipefail
print -r -- "TAIL_ARGS=$*" > "$FAKE_TAIL_LOG"
FAKE
chmod +x "${TMPDIR}/bin/tail"

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_file() { [[ -f "$1" ]] || fail "expected file: $1"; }
assert_dir() { [[ -d "$1" ]] || fail "expected dir: $1"; }
assert_symlink() { [[ -L "$1" ]] || fail "expected symlink: $1"; }
assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle\nactual:\n$haystack"
}
assert_not_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" != *"$needle"* ]] || fail "expected output not to contain: $needle\nactual:\n$haystack"
}

run_sb() {
  zsh "$SB" "$@"
}

profile="${HOME}/.config/switchback/claude/personal"
run_sb claude init --account personal --copy-user-memory >/tmp/sb-claude-init.out
assert_dir "${profile}/projects"
assert_dir "${profile}/agents"
assert_file "${profile}/CLAUDE.md"
assert_contains "$(cat "${profile}/CLAUDE.md")" "global memory"

accounts="$(run_sb claude accounts)"
assert_contains "$accounts" "default"
assert_contains "$accounts" "personal"
assert_contains "$accounts" "$profile"

doctor="$(run_sb claude doctor --account personal)"
assert_contains "$doctor" "account: personal"
assert_contains "$doctor" "config: ${profile}"
assert_contains "$doctor" "history: separate"
assert_contains "$doctor" "native: ${SB_NATIVE_CLAUDE}"

run_sb claude --account personal --print hi >/tmp/sb-claude-run.out 2>/tmp/sb-claude-run.err
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "CLAUDE_CONFIG_DIR=${profile}"
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "ARGS=--setting-sources project,local --print hi"
assert_contains "$(cat /tmp/sb-claude-run.err)" "Claude native · account=personal"

run_sb claude --mode remote >/tmp/sb-claude-remote.out 2>/tmp/sb-claude-remote.err
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "ANTHROPIC_BASE_URL="
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "HTTPS_PROXY=http://127.0.0.1:18780"
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "NODE_EXTRA_CA_CERTS=${HOME}/.local/state/switchback/mode-d/ca.pem"
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "ARGS=--setting-sources project,local --remote-control"

run_sb claude --mode remote --print hi >/tmp/sb-claude-remote-print.out 2>/tmp/sb-claude-remote-print.err
assert_contains "$(cat "$FAKE_CLAUDE_LOG")" "ARGS=--setting-sources project,local --print hi"
assert_not_contains "$(cat "$FAKE_CLAUDE_LOG")" "--remote-control"

linked="${HOME}/.config/switchback/claude/linked"
run_sb claude init --account linked --link-user-memory --link-agents >/tmp/sb-claude-linked.out
assert_symlink "${linked}/CLAUDE.md"
assert_symlink "${linked}/agents"
[[ "$(readlink "${linked}/CLAUDE.md")" == "${HOME}/.claude/CLAUDE.md" ]] || fail "CLAUDE.md symlink target mismatch"
[[ "$(readlink "${linked}/agents")" == "${HOME}/.claude/agents" ]] || fail "agents symlink target mismatch"

missing_status=0
run_sb claude --account missing --print hi >/tmp/sb-claude-missing.out 2>/tmp/sb-claude-missing.err || missing_status=$?
[[ "$missing_status" -ne 0 ]] || fail "missing profile launch should fail"
assert_contains "$(cat /tmp/sb-claude-missing.err)" "Claude profile 'missing' does not exist"

default_transcript="${HOME}/.claude/projects/default-session.jsonl"
personal_transcript="${profile}/projects/personal-session.jsonl"
mkdir -p "${default_transcript:h}" "${personal_transcript:h}"
print -r -- "{}" > "$default_transcript"
sleep 1
print -r -- "{}" > "$personal_transcript"

watch_output="$(run_sb watch claude)"
assert_contains "$watch_output" "Tailing Claude transcript: account=personal"
assert_contains "$watch_output" "$personal_transcript"
assert_contains "$(cat "$FAKE_TAIL_LOG")" "TAIL_ARGS=-f ${personal_transcript}"

sleep 1
touch "$default_transcript"
watch_scoped_output="$(run_sb watch claude --account personal)"
assert_contains "$watch_scoped_output" "Tailing Claude transcript: account=personal"
assert_contains "$watch_scoped_output" "$personal_transcript"
assert_contains "$(cat "$FAKE_TAIL_LOG")" "TAIL_ARGS=-f ${personal_transcript}"

print "ok - sb Claude profile commands"
