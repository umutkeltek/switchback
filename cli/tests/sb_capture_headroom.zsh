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

    def headroom(pressure):
        result = subprocess.run(["zsh", sb, "body", "headroom", "--json"],
                    env={**env, "FAKE_BODY_JSON": json.dumps({"pressure": pressure})},
                    text=True, capture_output=True)
        data = json.loads(result.stdout) if result.stdout.strip() else None
        return result, data

    # Path 1: the engine publishes its resolved lines. They win over the thresholds, which
    # here still carry an older engine's numbers that would give different lines.
    old_thresholds = {"resume_free_bytes": 100_000_000_000, "resume_free_bps": 1500,
                      "degrade_free_bytes": 50_000_000_000, "degrade_free_bps": 800}
    new_thresholds = {"resume_free_bytes": 15_000_000_000, "resume_free_bps": 300,
                      "degrade_free_bytes": 10_000_000_000, "degrade_free_bps": 200}
    disk = 994_662_584_320
    warn, degrade, resume = 100_000_000_000, 19_893_251_686, 29_839_877_529
    lines = {"free_warn_line_bytes": warn, "free_degrade_line_bytes": degrade,
             "free_resume_line_bytes": resume}
    resolved_cases = [
        # free, mode, exit code, state
        (150_000_000_000, "segmented_full_wire", 0, "healthy"),
        (43_600_000_000, "segmented_full_wire", 3, "at_risk"),
        (25_000_000_000, "metadata_only", 3, "degraded_unrecoverable"),
        (31_000_000_000, "metadata_only", 3, "degraded_recoverable"),
        (150_000_000_000, "metadata_only", 0, "degraded_recoverable"),
        (31_000_000_000, "healing_probe", 3, "healing"),
    ]
    for thresholds in (old_thresholds, new_thresholds, None):
        for free, mode, code, state in resolved_cases:
            pressure = {"mode": mode, "capacity_bytes": disk, "free_bytes": free, **lines}
            if thresholds is not None:
                pressure["thresholds"] = thresholds
            result, data = headroom(pressure)
            assert result.returncode == code, (free, mode, result.returncode, result.stdout, result.stderr)
            assert data["schema"] == "switchback/capture-headroom@1", data
            assert data["required_free_bytes"] == warn, data
            assert data["degrade_floor_bytes"] == degrade, data
            assert data["resume_floor_bytes"] == resume, data
            assert data["state"] == state, data
            assert data["deficit_bytes"] == max(0, warn - free), data
            assert data["can_resume_if_degraded"] == (free >= resume), data
            assert data["required_free_bytes"] > data["resume_floor_bytes"] > data["degrade_floor_bytes"], data

    # Path 2: an older engine publishes thresholds only, or only some lines. Keep the
    # derivation it was built for: reclaim to its resume floor.
    fallback_cases = [
        (994_000_000_000, 105_000_000_000, "segmented_full_wire", 100_000_000_000, 0, "healthy"),
        (994_000_000_000, 99_000_000_000, "segmented_full_wire", 100_000_000_000, 3, "at_risk"),
        (500_000_000_000, 80_000_000_000, "segmented_full_wire", 75_000_000_000, 0, "healthy"),
        (100_000_000_000, 60_000_000_000, "segmented_full_wire", 15_000_000_000, 0, "healthy"),
        (994_000_000_000, 99_000_000_000, "metadata_only", 100_000_000_000, 3, "degraded_unrecoverable"),
        (994_000_000_000, 105_000_000_000, "metadata_only", 100_000_000_000, 0, "degraded_recoverable"),
    ]
    for partial in ({}, {"free_degrade_line_bytes": 1}):
        for capacity, free, mode, required, code, state in fallback_cases:
            pressure = {"mode": mode, "capacity_bytes": capacity, "free_bytes": free,
                        "thresholds": old_thresholds, **partial}
            result, data = headroom(pressure)
            assert result.returncode == code, (capacity, free, result.returncode, result.stdout, result.stderr)
            assert data["required_free_bytes"] == required, data
            assert data["degrade_floor_bytes"] == max(50_000_000_000, capacity * 800 // 10_000), data
            assert data["resume_floor_bytes"] == required, data
            assert data["state"] == state, data
            assert data["deficit_bytes"] == max(0, required - free), data
            assert data["can_resume_if_degraded"] == (free >= required), data

    # Neither lines nor thresholds: unreadable, nothing on stdout.
    for body in ("{}", json.dumps({"pressure": {"mode": "segmented_full_wire",
                                                "capacity_bytes": disk, "free_bytes": 1}})):
        result = subprocess.run(["zsh", sb, "body", "headroom", "--json"],
                     env={**env, "FAKE_BODY_JSON": body}, text=True, capture_output=True)
        assert result.returncode == 1 and not result.stdout.strip(), result

    # The human report names all three lines.
    result = subprocess.run(["zsh", sb, "body", "headroom"],
                 env={**env, "FAKE_BODY_JSON": json.dumps({"pressure": {
                     "mode": "segmented_full_wire", "capacity_bytes": disk,
                     "free_bytes": 43_600_000_000, **lines}})},
                 text=True, capture_output=True)
    assert result.returncode == 3, result
    for text in ("warn below", "100.00 GB", "degrade below", "19.89 GB", "resume at", "29.84 GB"):
        assert text in result.stdout, (text, result.stdout)
print("capture headroom: resolved lines, threshold fallback, state and unreadable-status controls pass")
PY
