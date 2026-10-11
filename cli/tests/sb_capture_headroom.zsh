#!/bin/zsh
set -euo pipefail
SB="${0:A:h:h}/sb"
python3 - "$SB" <<'PY'
import json, os, pathlib, subprocess, sys, tempfile
sb = sys.argv[1]
with tempfile.TemporaryDirectory(prefix="sb-headroom-fixture-") as directory:
    root = pathlib.Path(directory)
    binary = root / "switchback"
    binary.write_text("""#!/usr/bin/env python3
import os,sys
if sys.argv[1:4] != ["--json","body","status"]:
    sys.exit(2)
print(os.environ["FAKE_BODY_JSON"])
""")
    binary.chmod(0o700)
    env = {**os.environ, "HOME": str(root / "home"),
           "SWITCHBACK_RUNTIME_ROOT": str(root / "runtime"),
           "SB_BIN": str(binary)}
    env.pop("SB_SOURCE_ONLY", None)
    thresholds = {"resume_free_bytes": 100_000_000_000, "resume_free_bps": 1500,
                  "degrade_free_bytes": 50_000_000_000, "degrade_free_bps": 800}
    cases = [
        (994_000_000_000, 105_000_000_000, "segmented_full_wire", 100_000_000_000, 0, "healthy"),
        (994_000_000_000, 99_000_000_000, "segmented_full_wire", 100_000_000_000, 3, "at_risk"),
        (500_000_000_000, 80_000_000_000, "segmented_full_wire", 75_000_000_000, 0, "healthy"),
        (100_000_000_000, 60_000_000_000, "segmented_full_wire", 15_000_000_000, 0, "healthy"),
        (994_000_000_000, 99_000_000_000, "metadata_only", 100_000_000_000, 3, "degraded_unrecoverable"),
        (994_000_000_000, 105_000_000_000, "metadata_only", 100_000_000_000, 0, "degraded_recoverable"),
    ]
    for capacity, free, mode, required, code, state in cases:
        body = {"pressure": {"mode": mode, "capacity_bytes": capacity,
                 "free_bytes": free, "thresholds": thresholds}}
        result = subprocess.run(["zsh", sb, "body", "headroom", "--json"],
                    env={**env, "FAKE_BODY_JSON": json.dumps(body)},
                    text=True, capture_output=True)
        assert result.returncode == code, (capacity, free, result.returncode, result.stdout, result.stderr)
        data = json.loads(result.stdout)
        assert data["required_free_bytes"] == required, data
        assert data["degrade_floor_bytes"] == max(50_000_000_000, capacity * 800 // 10_000), data
        assert data["state"] == state, data
        assert data["deficit_bytes"] == max(0, required - free), data
    result = subprocess.run(["zsh", sb, "body", "headroom", "--json"],
                 env={**env, "FAKE_BODY_JSON": "{}"}, text=True, capture_output=True)
    assert result.returncode == 1 and not result.stdout.strip(), result
print("capture headroom: existing controller floors, state and unreadable-status controls pass")
PY
