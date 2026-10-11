#!/usr/bin/env python3
"""Repair enrolled capture entrypoints and profiles through their owners."""
import argparse, datetime, hashlib, json, os, re, socket, sqlite3, subprocess, sys
from pathlib import Path

SCHEMA = "switchback/capture-repair@1"

def read_json(path):
    return json.loads(Path(path).read_text())

def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def command_json(command, allowed=(0,)):
    result = subprocess.run(command, capture_output=True, text=True, timeout=30)
    if result.returncode not in allowed:
        raise ValueError("owner command refused: " + result.stderr[-1200:])
    return json.loads(result.stdout)

def plan(runtime):
    path = runtime / "config/capture-repair.json"
    if path.is_symlink():
        raise ValueError("repair authority must not be a symlink")
    data = read_json(path)
    if data.get("schema") != SCHEMA or data.get("owner") != "switchback":
        raise ValueError("unknown repair authority")
    for field in ("engine", "native_owner", "primary_config", "capture_config", "profile_config"):
        candidate = Path(data[field])
        if not candidate.is_absolute() or candidate.is_symlink():
            raise ValueError("repair inputs must be absolute regular owner paths")
        if digest(candidate) != data[field + "_sha256"]:
            raise ValueError("repair input changed: " + field)
    if Path(data["native_owner"]).parent != runtime / "bin":
        raise ValueError("native repair owner must be installed in the runtime")
    if Path(data["profile_authority"]) != runtime / "config/launch-profiles.json":
        raise ValueError("profile authority must be Switchback-owned")
    if not isinstance(data.get("required_ports"), list) or not data["required_ports"]:
        raise ValueError("repair requires an enrolled listener inventory")
    for port in data["required_ports"]:
        if not isinstance(port, int) or not 1024 <= port <= 65535:
            raise ValueError("invalid enrolled listener")
    if data.get("body_store"):
        body = data["body_store"]
        generation = body["generation"]
        if not isinstance(generation, str) or not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]{0,63}", generation):
            raise ValueError("body drain generation identity is invalid")
        registry = runtime / "config/capture-backup-generations.json"
        if registry.is_symlink():
            raise ValueError("body drain registry must not be a symlink")
        enrollment = read_json(registry)
        if enrollment.get("schema") != "switchback/capture-backup-generations@1" or enrollment.get("owner") != "switchback":
            raise ValueError("body drain registry authority is invalid")
        matches = [g for g in enrollment["generations"] if g["id"] == generation]
        record = matches[0] if len(matches) == 1 else None
        expected_root = runtime / "generations" / generation
        if expected_root.is_symlink():
            raise ValueError("body drain generation must not be a symlink")
        if not record or Path(record["runtime_root"]) != expected_root:
            raise ValueError("body drain generation is not enrolled")
        if Path(data["engine"]) != expected_root / "bin/switchback":
            raise ValueError("body drain engine is not the enrolled owner")
        for field in ("state_dir", "archive_root"):
            path = Path(body[field])
            if not path.is_absolute() or path.is_symlink() or body[field] != record[field]:
                raise ValueError("body drain location differs from enrollment")
        if Path(body["state_dir"]) != expected_root / "state":
            raise ValueError("body drain state is not the enrolled owner")
        if digest(Path(data["engine"])) != record["binary_sha256"] or digest(expected_root / "manifest.json") != record["manifest_sha256"]:
            raise ValueError("body drain enrollment changed")
        manifest = read_json(expected_root / "manifest.json")
        if manifest.get("schema") != "switchback/capture-generation@1" or manifest.get("owner") != "switchback" or manifest.get("generation") != generation:
            raise ValueError("body drain manifest authority is invalid")
    return data


def body_store_status(data):
    body = data.get("body_store")
    if not body:
        return None
    index = Path(body["state_dir"]) / "body/index-v2.sqlite"
    if index.is_symlink() or not index.is_file():
        raise ValueError("body drain requires the enrolled regular current index")
    with sqlite3.connect(index.as_uri() + "?mode=rw", uri=True, timeout=2) as conn:
        conn.execute("PRAGMA query_only=ON")
        sealed = conn.execute("SELECT COUNT(*) FROM body_segments WHERE storage='spool_segment' AND sealed=1").fetchone()[0]
    return {"sealed_spool_segments": sealed, "drain_needed": sealed > 0,
            "archive_available": Path(body["archive_root"]).is_dir()}


def drain_body_store(data):
    body = data["body_store"]
    env = dict(os.environ, SWITCHBACK_BODY_FORCE_SPOOL="0")
    result = subprocess.run([data["engine"], "--json", "body", "gc",
                             "--state-dir", body["state_dir"], "--archive-root", body["archive_root"],
                             "--confirm", "--drain-only"], env=env, capture_output=True, text=True, timeout=300)
    if result.returncode:
        raise ValueError("native spool drain refused: " + result.stderr[-1200:])
    report = json.loads(result.stdout)["gc"]
    if report.get("refused") or report.get("events_deleted", 0) or report.get("blobs_deleted", 0):
        raise ValueError("spool drain must preserve all retained bodies")
    return report


def entrypoint_status(runtime, data):
    statuses = {}
    for item in data.get("entrypoint_artifacts", []):
        target, source = Path(item["target"]), Path(item["source"])
        if target.parent != Path.home() / ".local/bin" or target.name != "codex-switchback-tap":
            raise ValueError("entrypoint repair target is outside its enrolled owner")
        if source.parent != runtime / "bin/entrypoints" or source.is_symlink():
            raise ValueError("entrypoint template is outside its enrolled owner")
        if digest(source) != item["sha256"] or b"switchback-owned: native-codex-capture-entrypoint@1" not in source.read_bytes():
            raise ValueError("entrypoint template changed")
        statuses[target.name] = target.exists() and digest(target) == item["sha256"]
    native = data.get("native_claude")
    if native:
        installer = Path(native["installer"])
        template = runtime / "bin/entrypoints/claude"
        if installer.parent != runtime / "bin" or installer.is_symlink() or digest(installer) != native["installer_sha256"]:
            raise ValueError("native Claude owner changed")
        if digest(template) != native["template_sha256"]:
            raise ValueError("native Claude template changed")
        public = Path.home() / ".local/bin/claude"
        pin = Path.home() / ".local/share/claude/.switchback-real"
        vendor = Path(pin.read_text().strip()) if pin.is_file() else None
        statuses["claude"] = bool(public.is_file() and digest(public) == native["template_sha256"]
                                 and vendor and vendor.is_file() and os.access(vendor, os.X_OK))
    return statuses

def repair_entrypoints(runtime, data):
    before = entrypoint_status(runtime, data)
    for item in data.get("entrypoint_artifacts", []):
        target, source = Path(item["target"]), Path(item["source"])
        if before[target.name]:
            continue
        existed, was_link = target.exists(), target.is_symlink()
        link_target = os.readlink(target) if was_link else None
        preimage = target.read_bytes() if existed else None
        saved = preimage if preimage is not None else json.dumps({"existed":existed,"symlink_target":link_target}).encode()
        backup = runtime / "backups/capture-entrypoints" / (target.name + "." + hashlib.sha256(saved).hexdigest())
        backup.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        if not backup.exists():
            backup.write_bytes(saved)
            backup.chmod(0o600)
        temporary = target.with_name("." + target.name + ".capture-repair.tmp")
        if temporary.is_symlink():
            raise ValueError("entrypoint repair temporary must not be a symlink")
        temporary.write_bytes(source.read_bytes())
        temporary.chmod(0o755)
        current_link = os.readlink(target) if target.is_symlink() else None
        current_bytes = target.read_bytes() if target.exists() else None
        if (target.exists(),target.is_symlink(),current_link,current_bytes) != (existed,was_link,link_target,preimage):
            temporary.unlink()
            raise ValueError("entrypoint changed before repair")
        os.replace(temporary, target)
    if data.get("native_claude") and not before["claude"]:
        env = dict(os.environ, SWITCHBACK_RUNTIME_ROOT=str(runtime), SB_RUNTIME_ROOT=str(runtime),
                   SB_INSTALL_NATIVE_CLAUDE_ONLY="1", SB_INSTALL_ENGINE_ONLY="0",
                   SB_INSTALL_CAPTURE_OWNERS_ONLY="0", PREFIX=str(Path.home()/".local/bin"))
        subprocess.run(["zsh", data["native_claude"]["installer"]], env=env, check=True, timeout=30)
    after = entrypoint_status(runtime, data)
    if not all(after.values()):
        raise ValueError("owned entrypoint repair did not converge")
    return before != after


def listener_ready(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=0.5):
            return True
    except OSError:
        return False

def profile_command(data, action, name):
    return [data["engine"], "--json", "profile", action, name,
            "--authority", data["profile_authority"], "--config", data["profile_config"]]

def status(runtime, data=None):
    data = data or plan(runtime)
    native = command_json([sys.executable, data["native_owner"], "status", "--runtime", str(runtime)], (0, 1))
    profiles = {}
    for name in sorted(read_json(data["profile_authority"])["launch_profiles"]):
        result = command_json(profile_command(data, "diff", name))
        profiles[name] = {"current": not result["changed"]}
    missing = [p for p in data["required_ports"] if not listener_ready(p)]
    entrypoints = entrypoint_status(runtime, data)
    current = bool(native["clients"]) and native["ok"] and all(p["current"] for p in profiles.values()) and all(entrypoints.values())
    body = body_store_status(data)
    return {"schema": "switchback/capture-repair-status@1", "current": current,
            "ready": not missing, "ok": current and not missing and not (body and body["drain_needed"]), "body_store": body, "missing_ports": missing,
            "native_clients": native["clients"], "profiles": profiles, "entrypoints": entrypoints}

def run(runtime, result_path):
    data = plan(runtime)
    before = status(runtime, data)
    if not before["ready"]:
        raise ValueError("capture listeners unavailable; active sessions are preserved")
    repaired_entrypoints = repair_entrypoints(runtime, data)
    native = command_json([sys.executable, data["native_owner"], "repair", "--runtime", str(runtime)])
    if not native["ok"]:
        raise ValueError("native entrypoint repair did not converge")
    authority_bytes = Path(data["profile_authority"]).read_bytes()
    changed_profiles = []
    for name in sorted(read_json(data["profile_authority"])["launch_profiles"]):
        applied = command_json(profile_command(data, "apply", name))
        if not applied["doctor"]["ok"]:
            raise ValueError("profile conformance did not converge: " + name)
        if applied["changed"]:
            changed_profiles.append(name)
    if Path(data["profile_authority"]).read_bytes() != authority_bytes:
        raise ValueError("profile authority changed during repair")
    drain = None
    if before["body_store"] and before["body_store"]["drain_needed"]:
        if not before["body_store"]["archive_available"]:
            raise ValueError("capture archive unavailable; local spool preserved")
        drain = drain_body_store(data)
    after = status(runtime, data)
    if not after["ok"]:
        raise ValueError("capture repair remains unhealthy")
    result = {"schema": "switchback/capture-repair-result@1", "ok": True,
              "changed": not before["current"] or bool(changed_profiles) or repaired_entrypoints or bool(drain and (drain.get("spool_segments_drained") or drain.get("spool_blobs_drained"))), "body_drain": drain,
              "changed_profiles": changed_profiles, "models_accounts_authority_unchanged": True,
              "capture_generation_restarted": False, "after": after}
    result_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    temporary = result_path.with_name("." + result_path.name + ".tmp")
    if result_path.is_symlink() or temporary.is_symlink():
        raise ValueError("repair result must not follow symlinks")
    temporary.write_text(json.dumps(result, sort_keys=True) + "\n")
    temporary.chmod(0o600)
    os.replace(temporary, result_path)
    return result

def scheduled(runtime):
    data = plan(runtime)
    observed = status(runtime, data)
    if observed["ok"]:
        return {"schema": "switchback/capture-repair-cycle@1", "ok": True,
                "changed": False, "governed_work_required": False}
    if not observed["ready"]:
        raise ValueError("capture listeners unavailable; launchd owns restart and sessions remain intact")
    if observed["body_store"] and observed["body_store"]["drain_needed"] and not observed["body_store"]["archive_available"]:
        raise ValueError("capture archive unavailable; local spool preserved")
    actor = "scheduler:switchback-capture-repair"
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    work_id = "work_switchback_capture_repair_" + stamp
    cli = data["compound_cli"]
    if not Path(cli).is_absolute() or not os.access(cli, os.X_OK):
        raise ValueError("governed execution owner unavailable")
    def invoke(*args):
        subprocess.run([cli, *args], check=True, timeout=300)
    invoke("work", "create", "--work-id", work_id, "--owner", actor, "--by", actor,
           "--state", "ready", "--creator-harness", "os-harness-entrypoints",
           "--creator-lane", "switchback-capture-repair", "--canonical-surface", str(runtime),
           "--outcome", "Restore capture wrappers, profile conformance, and archive sealed spool segments",
           "--predicate", "Capture listeners ready; enrolled wrappers and profiles current; model and account authorities unchanged")
    invoke("work", "claim", work_id, "--by", actor)
    result_path = runtime / "state/capture-repair" / (work_id + ".json")
    execution = subprocess.run(
        [cli, "work", "advance", work_id, "--to", "running", "--by", actor,
         "--execute", "--harness", "os-harness-entrypoints", "--workspace", str(runtime),
         "--timeout", "10m", "--", sys.executable, str(Path(__file__).resolve()), "run",
         "--runtime", str(runtime), "--result", str(result_path)], timeout=660)
    if execution.returncode:
        invoke("work", "close", work_id, "--outcome", "failed", "--move-type", "audit_only",
               "--by", actor, "--summary", "Capture owner repair failed; sessions preserved",
               "--failure-surface", "runtime", "--failure-mechanism", "owner")
        raise ValueError("governed repair failed: " + work_id)
    result = read_json(result_path)
    if not result.get("ok"):
        raise ValueError("repair result did not attest conformance")
    close = ["work", "close", work_id, "--outcome", "completed", "--move-type",
             "control_plane_repair" if result["changed"] else "audit_only", "--by", actor,
             "--summary", "Capture wrappers and profiles restored by their owners",
             "--proof", str(result_path), "--used", "switchback-native-capture"]
    if result["changed"]:
        close.append("--changed")
    invoke(*close)
    return {"schema": "switchback/capture-repair-cycle@1", "ok": True,
            "changed": result["changed"], "work_id": work_id, "result": str(result_path)}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("status", "run", "scheduled"))
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--result", type=Path)
    args = parser.parse_args()
    runtime = args.runtime.resolve()
    try:
        if args.action == "status":
            result = status(runtime)
        elif args.action == "run":
            if args.result is None or args.result.parent != runtime / "state/capture-repair":
                raise ValueError("governed run requires an owner result destination")
            result = run(runtime, args.result)
        else:
            result = scheduled(runtime)
        print(json.dumps(result, sort_keys=True))
        return 0 if result.get("ok") else 1
    except (OSError, ValueError, KeyError, sqlite3.Error, subprocess.SubprocessError) as error:
        print("capture repair refused: " + str(error), file=sys.stderr)
        return 1

if __name__ == "__main__":
    raise SystemExit(main())
