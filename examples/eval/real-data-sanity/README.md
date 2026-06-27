# Real-Data Sanity Pack

This pack exercises the neutral eval ingest path with sanitized, harness-shaped
inputs for Codex CLI, Claude Code, and Aider. It does not contain raw prompts,
responses, diffs, stdout/stderr, logs, tokens, or secrets.

```bash
tmpdir=$(mktemp -d /tmp/switchback-real-data-sanity.XXXXXX)
store="$tmpdir/eval.sqlite"

switchback --json eval --store "$store" case import examples/eval/real-data-sanity/case.json

switchback --json eval convert codex-cli \
  --input examples/eval/real-data-sanity/inputs/codex-cli.json \
  --case-id real-data-sanity-001 \
  --case-revision rev-1 \
  --strategy-id default > "$tmpdir/codex-cli.json"

switchback --json eval convert claude-code \
  --input examples/eval/real-data-sanity/inputs/claude-code.json \
  --case-id real-data-sanity-001 \
  --case-revision rev-1 \
  --strategy-id review-repair > "$tmpdir/claude-code.json"

switchback --json eval convert aider \
  --input examples/eval/real-data-sanity/inputs/aider.json \
  --case-id real-data-sanity-001 \
  --case-revision rev-1 \
  --strategy-id default > "$tmpdir/aider.json"

for run in "$tmpdir"/*.json; do
  switchback --json eval --store "$store" ingest --result "$run"
done

switchback --json eval --store "$store" report \
  --by harness,harness_version \
  --task-type coding \
  --tag real_data_sanity \
  --min-runs 1
```

Expected signal:

```text
codex-cli    pass  cost_micros=14200
claude-code  pass  cost_micros=31400
aider        fail  cost_micros=4200
```

This proves the generic converter schema covers three harness families without
adding harness-specific fields to `sb-eval`.
