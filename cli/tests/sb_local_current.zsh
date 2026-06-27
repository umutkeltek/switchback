#!/bin/zsh
set -euo pipefail

ROOT="${0:A:h:h}"
SB="${ROOT}/sb"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

export HOME="${TMPDIR}/home"
export PATH="${TMPDIR}/bin:${PATH}"

mkdir -p "${HOME}/.config/switchback" "${TMPDIR}/bin"

cat > "${TMPDIR}/bin/switchback" <<'FAKE'
#!/bin/zsh
set -euo pipefail

case "${1:-}" in
config)
  case "${2:-}" in
  get)
    case "${3:-}" in
    providers)
      cat <<'JSON'
[
  {
    "id": "mac",
    "type": "openai_compatible",
    "base_url": "http://127.0.0.1:1234/v1"
  }
]
JSON
      ;;
    routes)
      cat <<'JSON'
[
  {
    "name": "local-mac-code",
    "match": {"model": "local/mac-code"},
    "targets": ["mac/old-code"]
  },
  {
    "name": "local-mac-fast",
    "match": {"model": "local/mac-fast"},
    "targets": ["mac/served-fast"]
  }
]
JSON
      ;;
    *)
      exit 2
      ;;
    esac
    ;;
  *)
    exit 2
    ;;
  esac
  ;;
*)
  exit 2
  ;;
esac
FAKE
chmod +x "${TMPDIR}/bin/switchback"

cat > "${TMPDIR}/bin/curl" <<'FAKE'
#!/bin/zsh
set -euo pipefail

local url=""
while (( $# )); do
  case "$1" in
  http*) url="$1"; shift ;;
  -m) shift 2 ;;
  -*) shift ;;
  *) shift ;;
  esac
done

[[ "$url" == "http://127.0.0.1:1234/v1/models" ]] || exit 22
cat <<'JSON'
{
  "data": [
    {"id": "served-code", "object": "model"},
    {"id": "served-fast", "object": "model"}
  ]
}
JSON
FAKE
chmod +x "${TMPDIR}/bin/curl"

cat > "${TMPDIR}/bin/lms" <<'FAKE'
#!/bin/zsh
set -euo pipefail

[[ "${1:-}" == "ps" ]] || exit 2
cat <<'TABLE'
IDENTIFIER MODEL STATUS SIZE CONTEXT PARALLEL DEVICE TTL
served-code served-code IDLE 10 GB 131072 1 Local -
served-fast served-fast IDLE 4 GB 32768 1 Local -
TABLE
FAKE
chmod +x "${TMPDIR}/bin/lms"

fail() {
  print -ru2 -- "FAIL: $*"
  exit 1
}

assert_contains() {
  local haystack="$1" needle="$2"
  [[ "$haystack" == *"$needle"* ]] || fail "expected output to contain: $needle\nactual:\n$haystack"
}

SB_SOURCE_ONLY=1 source "$SB"

json="$(_local_current --json)"
print -r -- "$json" | jq -e '.provider == "lmstudio"' >/dev/null || fail "expected lmstudio provider"
print -r -- "$json" | jq -e '.served_count == 2' >/dev/null || fail "expected two served models"
print -r -- "$json" | jq -e '.ok == false' >/dev/null || fail "expected stale local route to make ok=false"
print -r -- "$json" | jq -e '.routes[] | select(.slot == "code" and .status == "stale" and .suggestion == "sb local use code served-code --reload")' >/dev/null || fail "expected code stale suggestion"
print -r -- "$json" | jq -e '.routes[] | select(.slot == "fast" and .status == "loaded")' >/dev/null || fail "expected fast route loaded"

served_json="$(sb_local served --json)"
print -r -- "$served_json" | jq -e '.routes | length == 2' >/dev/null || fail "served alias should expose local status JSON"

out="$(_local_current)"
assert_contains "$out" "served: 2"
assert_contains "$out" "local/mac-code -> mac/old-code [stale]"
assert_contains "$out" "fix: sb local use code served-code --reload"
assert_contains "$out" "local/mac-fast -> mac/served-fast [loaded]"
assert_contains "$out" "LM Studio served model ids"
assert_contains "$out" "LM Studio loaded processes"

print "ok - sb local current"
