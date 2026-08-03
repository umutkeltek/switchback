#!/bin/zsh
set -euo pipefail

export TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

CLI_ROOT="${0:A:h:h}"
SB="${SB_UNDER_TEST:-${CLI_ROOT}/sb}"

export HOME="${TMPDIR}/home"
export SWITCHBACK_RUNTIME_ROOT="${HOME}/.config/switchback"
export SB_LANES="${SWITCHBACK_RUNTIME_ROOT}/config/lanes"
export SB_PROVIDER_REGISTRY="${TMPDIR}/provider-registry.json"
mkdir -p "$SB_LANES" "${HOME}/.headroom/logs" "${TMPDIR}/bin"
print '{"schema":"switchback/runtime-manifest@1","test_fixture":true}' > \
  "${SWITCHBACK_RUNTIME_ROOT}/manifest.json"

cat > "${SB_LANES}/minimax.env" <<'EOF'
SB_LANE_NAME='minimax'
SB_LANE_MODEL='MiniMax-M3'
SB_LANE_TRANSPORT='headroom'
SB_LANE_ANTHROPIC_TAP='18775'
SB_LANE_HEADROOM_PORT='8789'
EOF

# The registry seed deliberately says two. The telemetry below has enough
# evidence to change the recommendation to four, proving this is an observed
# opinion rather than a hard-coded width.
cat > "$SB_PROVIDER_REGISTRY" <<'JSON'
{
  "schema": "switchback/provider-registry@3",
  "providers": [
    {
      "id": "minimax",
      "capacity_calibration": {
        "recommended_concurrency": 2,
        "confidence": "low",
        "observed_at": "2026-08-01T00:00:00Z",
        "sample_count": 4,
        "weekly_quota": {"status": "unknown"}
      }
    }
  ]
}
JSON

# Completion timestamps + total latency let Switchback recover request start
# times and overlap without logging prompt/response bodies. Aggregate output is
# best at four streams: 1*100 tok/s, 4*50 tok/s, 8*5 tok/s.
cat > "${HOME}/.headroom/logs/minimax.jsonl" <<'JSONL'
{"request_id":"one-1","timestamp":"2026-08-02T00:00:01Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":1000,"error":null}
{"request_id":"one-2","timestamp":"2026-08-02T00:00:03Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":1000,"error":null}
{"request_id":"one-3","timestamp":"2026-08-02T00:00:05Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":1000,"error":null}
{"request_id":"one-4","timestamp":"2026-08-02T00:00:07Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":1000,"error":null}
{"request_id":"four-1","timestamp":"2026-08-02T00:00:14Z","model":"MiniMax-M3","output_tokens":200,"total_latency_ms":4000,"error":null}
{"request_id":"four-2","timestamp":"2026-08-02T00:00:14Z","model":"MiniMax-M3","output_tokens":200,"total_latency_ms":4000,"error":null}
{"request_id":"four-3","timestamp":"2026-08-02T00:00:14Z","model":"MiniMax-M3","output_tokens":200,"total_latency_ms":4000,"error":null}
{"request_id":"four-4","timestamp":"2026-08-02T00:00:14Z","model":"MiniMax-M3","output_tokens":200,"total_latency_ms":4000,"error":null}
{"request_id":"eight-1","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
{"request_id":"eight-2","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
{"request_id":"eight-3","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
{"request_id":"eight-4","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
{"request_id":"eight-5","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
{"request_id":"eight-6","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
{"request_id":"eight-7","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
{"request_id":"eight-8","timestamp":"2026-08-02T00:00:40Z","model":"MiniMax-M3","output_tokens":100,"total_latency_ms":20000,"error":null}
JSONL

# An open-ended burst is diagnostic, not a recommendation candidate. Otherwise
# one transient maximum (20 here) multiplied by the bucket median can look like
# healthy capacity even though no bounded width was repeated.
for burst_index in {1..20}; do
  print -r -- "{\"request_id\":\"burst-${burst_index}\",\"timestamp\":\"2026-08-02T00:02:00Z\",\"model\":\"MiniMax-M3\",\"output_tokens\":6000,\"total_latency_ms\":60000,\"error\":null}" >> \
    "${HOME}/.headroom/logs/minimax.jsonl"
done

# Six established requests on the transparent tap. Compound claims two, so the
# projection must treat the remaining four as operator traffic.
cat > "${TMPDIR}/bin/lsof" <<'EOF'
#!/bin/zsh
print 'switchback 100 test 10u IPv4 0x1 0t0 TCP 127.0.0.1:18775->127.0.0.1:50001 (ESTABLISHED)'
print 'switchback 100 test 11u IPv4 0x2 0t0 TCP 127.0.0.1:18775->127.0.0.1:50002 (ESTABLISHED)'
print 'switchback 100 test 12u IPv4 0x3 0t0 TCP 127.0.0.1:18775->127.0.0.1:50003 (ESTABLISHED)'
print 'switchback 100 test 13u IPv4 0x4 0t0 TCP 127.0.0.1:18775->127.0.0.1:50004 (ESTABLISHED)'
print 'switchback 100 test 14u IPv4 0x5 0t0 TCP 127.0.0.1:18775->127.0.0.1:50005 (ESTABLISHED)'
print 'switchback 100 test 15u IPv4 0x6 0t0 TCP 127.0.0.1:18775->127.0.0.1:50006 (ESTABLISHED)'
EOF
chmod +x "${TMPDIR}/bin/lsof"
export PATH="${TMPDIR}/bin:${PATH}"

output="$(zsh "$SB" lane capacity minimax --json --compound-active 2)"
print -r -- "$output" | jq -e '
  .schema == "switchback/lane-capacity@1"
  and .lane_id == "minimax"
  and .recommendation.concurrency == 4
  and .recommendation.basis == "observed_headroom_telemetry"
  and .recommendation.confidence == "medium"
  and .activity.observed_total == 6
  and .activity.compound == 2
  and .activity.operator == 4
  and .activity.available_compound_slots == 0
  and .manual_traffic.gated == false
  and .manual_traffic.priority == "operator_first"
  and .weekly_quota.status == "unknown"
  and .weekly_quota.used_percent == null
  and .weekly_quota.remaining_percent == null
  and .hard_rate_limit.observed == false
  and .source.kind == "headroom_jsonl"
  and .source.sample_count == 36
  and (.source.revision | startswith("sha256:"))
  and ([.cohorts[].range] | index("2-4") != null)
  and ([.cohorts[].range] | index("5-8") != null)
  and (.cohorts[] | select(.range == "17+") | .eligible_for_recommendation == false)
' >/dev/null || {
  print -ru2 -- "FAIL: invalid lane-capacity projection:\n${output}"
  exit 1
}

# Sparse evidence falls back to the registry calibration, while a real 429 is
# still surfaced separately from the throughput recommendation.
cat > "${HOME}/.headroom/logs/minimax.jsonl" <<'JSONL'
{"request_id":"limited-1","timestamp":"2026-08-02T00:01:00Z","model":"MiniMax-M3","output_tokens":0,"total_latency_ms":1000,"error":"HTTP 429 rate limit"}
JSONL
limited="$(zsh "$SB" lane capacity minimax --json --compound-active 2)"
print -r -- "$limited" | jq -e '
  .recommendation.concurrency == 2
  and .recommendation.basis == "registry_calibration"
  and .hard_rate_limit.observed == true
  and .hard_rate_limit.events_window == 1
  and .weekly_quota.status == "unknown"
' >/dev/null || {
  print -ru2 -- "FAIL: sparse/rate-limit projection was invalid:\n${limited}"
  exit 1
}

# Compound's count is explicit evidence; malformed or missing lane inputs fail
# loudly instead of silently creating fake headroom.
if zsh "$SB" lane capacity minimax --json --compound-active nope >/dev/null 2>&1; then
  print -ru2 -- "FAIL: malformed --compound-active was accepted"
  exit 1
fi
if zsh "$SB" lane capacity absent --json >/dev/null 2>&1; then
  print -ru2 -- "FAIL: missing lane was accepted"
  exit 1
fi

print "ok: lane capacity projection is observed, operator-first, and quota-honest"
