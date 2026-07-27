#!/bin/zsh
set -euo pipefail
unsetopt bg_nice

# Effort and model must be settable per invocation, not baked into a lane's identity.
#
# Before this, effort existed only as SB_LANE_CLAUDE_EFFORT in the lane record, so two effort
# presets over one model needed two lane files writing the same shared key -- they clobbered
# each other, and "any model at any effort" was unreachable. These tests pin the behaviour
# that makes one lane serve every effort.
#
# The sharpest one is `invalid effort is fatal`: an unrecognised effort does NOT error at the
# far end, it silently degrades to `high`. A typo would spend a whole session at the wrong
# reasoning level with nothing to show for it, so rejection has to be unrecoverable rather
# than a return code a caller can forget to check.

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

if [[ -n "${SB_UNDER_TEST:-}" ]]; then
  SB="$SB_UNDER_TEST"
else
  CLI_ROOT="${0:A:h:h}"
  SB="${CLI_ROOT}/sb"
fi

pass=0
fail=0
check() {
  if [[ "$2" == "$3" ]]; then
    (( ++pass ))
  else
    (( ++fail ))
    print -u2 "FAIL ${1}: got '${2}' want '${3}'"
  fi
}

# Exercise the shipped functions rather than a copy of them: extract the override block
# straight out of the real sb. If these get renamed or removed, this test stops finding them
# and fails, which is the point.
helpers="${TMPDIR}/helpers.zsh"
awk '/^SB_EFFORT_LEVELS=\(/,/^_take_opts\(\) \{/{print} /^_take_opts\(\) \{/{f=1} f&&/^\}$/{print "}"; exit}' "$SB" > "$helpers"
awk '/^_take_opts\(\) \{/,/^\}$/' "$SB" >> "$helpers"

if ! grep -q '_take_invocation_overrides' "$helpers"; then
  print -u2 "FAIL: could not extract the invocation-override helpers from ${SB}"
  exit 1
fi

SB_DEFAULT_ACCOUNT=default
SB_OPT_DEFAULT_ACCOUNT=""
source "$helpers"

# --- parsing -----------------------------------------------------------------------------
_take_invocation_overrides --effort max -p "hello"
check "--effort captured" "$SB_INVOKE_EFFORT" "max"
check "--effort stripped from forwarded args" "${SB_INVOKE_REST[*]}" "-p hello"

_take_invocation_overrides --effort=low
check "--effort= form" "$SB_INVOKE_EFFORT" "low"

_take_invocation_overrides --lane-model gpt-5.6-terra --effort high extra
check "--lane-model captured" "$SB_INVOKE_MODEL" "gpt-5.6-terra"
check "both stripped, remainder preserved" "${SB_INVOKE_REST[*]}" "extra"

# --- the silent-degrade guard ------------------------------------------------------------
# Probed in a subshell because rejection is a hard `exit`, deliberately not a return code.
if ( _take_invocation_overrides --effort ludicrous ) 2>/dev/null; then
  (( ++fail ))
  print -u2 "FAIL: an invalid effort was accepted; it would degrade to 'high' unnoticed"
else
  (( ++pass ))
fi

# --- precedence --------------------------------------------------------------------------
SB_LANE_CLAUDE_EFFORT=xhigh
SB_CLAUDE_EFFORT=medium
SB_INVOKE_EFFORT=""
check "lane default outranks ambient env" "$(_resolve_claude_effort)" "xhigh"

SB_INVOKE_EFFORT=max
check "invocation flag outranks the lane" "$(_resolve_claude_effort)" "max"

SB_INVOKE_EFFORT=""
SB_LANE_CLAUDE_EFFORT=""
SB_CLAUDE_EFFORT=""
check "fallback when nothing is set" "$(_resolve_claude_effort)" "xhigh"

SB_INVOKE_MODEL=""
check "lane model kept when unset" "$(_resolve_claude_model wpcom/gpt-5.6-sol)" "wpcom/gpt-5.6-sol"
SB_INVOKE_MODEL=gpt-5.6-luna
check "lane model overridden" "$(_resolve_claude_model wpcom/gpt-5.6-sol)" "gpt-5.6-luna"

# --- _take_opts still owns its own flags --------------------------------------------------
SB_INVOKE_EFFORT=""
_take_opts --mode tap --account raylucian --effort max -p hi
check "_take_opts mode" "$mode" "tap"
check "_take_opts account" "$account" "raylucian"
check "_take_opts effort" "$SB_INVOKE_EFFORT" "max"
check "_take_opts REST excludes overrides" "${REST[*]}" "-p hi"

# --- both real entry points reject invalid effort -----------------------------------------
for entry in "claude --mode tap --effort nope -p x" "run claude --with zai --effort nope -p x"; do
  out="$(zsh "$SB" ${=entry} 2>&1 || true)"
  if [[ "$out" == *"invalid effort"* ]]; then
    (( ++pass ))
  else
    (( ++fail ))
    print -u2 "FAIL: 'sb ${entry}' did not reject the effort; got: ${out%%$'\n'*}"
  fi
done

if (( fail )); then
  print -u2 "sb_claude_effort: ${pass} passed, ${fail} FAILED"
  exit 1
fi
print "sb_claude_effort: ${pass} passed"
