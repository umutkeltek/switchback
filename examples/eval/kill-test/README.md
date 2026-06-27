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
  --by harness \
  --task-type coding \
  --tag kill_test \
  --min-runs 1 \
  --output .switchback/eval-snapshot.json

switchback --json eval --store .switchback/eval.sqlite snapshot publish \
  --snapshot .switchback/eval-snapshot.json \
  --name current

switchback --json eval --store .switchback/eval.sqlite snapshot current \
  --name current
```

Snapshot rows include:

```text
preview_eligible: enough evidence to show in preview
routing_eligible: enough evidence for a future explicit eval-aware route policy
ineligible_reasons: why the row is weak, stale, or unsafe for routing
```

Default gates:

```text
preview: >= 5 runs and >= 3 distinct cases
routing: >= 20 runs and >= 8 distinct cases
blocks: stale evidence, missing harness version, high inconclusive rate,
        high rolled-back rate, missing task/tag scope
```

This fixture pack should be preview-eligible but not routing-eligible. That is
intentional: it proves report/snapshot mechanics without implying route
authority.
