# Eval Kill Test

This pack keeps the first evidence experiment CLI-only before any
`/cp/v1/eval` API or routing behavior is added:

```text
5 cases x 3 harnesses x 2 runs = 30 runs
```

`pack.json` contains sanitized `switchback.eval.case/v1` case manifests and
`switchback.eval.run/v1` run manifests for `codex-cli`, `claude-code`, and
`aider`. It stores labels, metrics, human outcome signals, and stable evidence
references only. It does not store prompts, responses, diffs, stdout, stderr,
logs, or artifact bodies.

Use the pack to check whether generic ingestion/reporting is useful before
freezing a control-plane contract:

```bash
switchback --json eval --store .switchback/eval.sqlite report \
  --by harness \
  --task-type coding \
  --tag kill_test \
  --min-runs 1

switchback --json eval --store .switchback/eval.sqlite snapshot build \
  --by harness,harness_version \
  --task-type coding \
  --tag kill_test \
  --min-runs 1 \
  --generated-at-ms 70000 \
  --output .switchback/eval-snapshot.json

switchback --json eval --store .switchback/eval.sqlite snapshot publish \
  --snapshot .switchback/eval-snapshot.json \
  --name current

switchback --json eval --store .switchback/eval.sqlite snapshot current \
  --name current
```

Expected report signal from this fixture:

```text
harness       runs  cases  pass_rate  median_cost  human_acceptance
claude-code   10    5      0.90       420000       0.90
codex-cli     10    5      0.80       180000       0.80
aider         10    5      0.60        90000       0.60
```

Snapshot rows should be preview-eligible but not routing-eligible:

```text
preview_eligible: true
routing_eligible: false
ineligible_reasons: routing_min_runs_not_met, routing_min_distinct_cases_not_met
```

That is intentional. It proves report/snapshot mechanics without implying route
authority.
