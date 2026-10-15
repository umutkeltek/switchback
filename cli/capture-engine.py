#!/usr/bin/env python3
"""Upgrade one enrolled capture engine during an observed quiet window."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import time


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for data in iter(lambda: f.read(8 * 1024 * 1024), b''):
            h.update(data)
    return h.hexdigest()


def regular(path):
    if path.is_symlink() or not path.is_file() or path.stat().st_uid != os.getuid():
        raise ValueError('capture owner requires an owned regular file: ' + str(path))
    return path


def read(path):
    return json.loads(regular(path).read_text())


def atomic(path, data, mode):
    temporary = path.with_name('.' + path.name + '.capture-engine-new')
    if temporary.exists() or temporary.is_symlink():
        raise ValueError('unfinished owner write requires recovery: ' + str(temporary))
    with temporary.open('xb') as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    temporary.chmod(mode)
    os.replace(temporary, path)


def encode(data):
    return (json.dumps(data, indent=2, sort_keys=True) + '\n').encode()


def plan(runtime, generation, binary, commit, launch_agents=None):
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_-]{0,63}', generation):
        raise ValueError('invalid generation identity')
    if not re.fullmatch(r'[0-9a-f]{40}', commit):
        raise ValueError('source commit must be a full Git SHA')
    runtime = runtime.absolute()
    root = runtime / 'generations' / generation
    for path in (runtime, runtime / 'config', runtime / 'generations', root, root / 'bin', root / 'config'):
        if path.is_symlink() or not path.is_dir() or path.stat().st_uid != os.getuid():
            raise ValueError('unsafe capture runtime directory')
    binary = regular(binary.absolute())
    registry_path = runtime / 'config/capture-backup-generations.json'
    registry = read(registry_path)
    if registry.get('schema') != 'switchback/capture-backup-generations@1' or registry.get('owner') != 'switchback':
        raise ValueError('unknown generation registry authority')
    rows = [g for g in registry['generations'] if g.get('id') == generation]
    if len(rows) != 1:
        raise ValueError('generation is not uniquely enrolled')
    row = rows[0]
    engine = regular(root / 'bin/switchback')
    manifest_path = root / 'manifest.json'
    manifest = read(manifest_path)
    if row['runtime_root'] != str(root) or row['cli'] != str(engine):
        raise ValueError('enrolled owner paths differ from generation')
    if row['binary_sha256'] != digest(engine) or row['manifest_sha256'] != digest(manifest_path):
        raise ValueError('enrolled owner fingerprints changed')
    if manifest.get('schema') != 'switchback/capture-generation@1' or manifest.get('owner') != 'switchback' or manifest.get('generation') != generation:
        raise ValueError('generation manifest authority mismatch')
    label = 'ai.switchback.' + generation
    launch_agents = launch_agents or Path.home() / 'Library/LaunchAgents'
    launch_path = launch_agents / (label + '.plist')
    launch = plistlib.loads(regular(launch_path).read_bytes())
    if launch.get('Label') != label or launch.get('ProgramArguments') != [str(engine), 'serve', '--config', str(root / 'config/capture.json')]:
        raise ValueError('launch owner differs from enrolled generation')
    files = [engine, manifest_path, registry_path, launch_path, regular(root / 'config/capture.json')]
    repair_path = runtime / 'config/capture-repair.json'
    repair = read(repair_path)
    repair_targeted = repair.get('body_store', {}).get('generation') == generation
    if repair_targeted:
        if repair.get('engine') != str(engine) or repair.get('engine_sha256') != digest(engine):
            raise ValueError('repair engine authority changed')
        files.append(repair_path)
    return dict(schema='switchback/capture-engine-upgrade-plan@1', runtime=str(runtime), generation=generation,
                engine=str(engine), binary=str(binary), binary_sha256=digest(binary), source_commit=commit,
                label=label, launch_path=str(launch_path), manifest_path=str(manifest_path), registry_path=str(registry_path),
                repair_path=str(repair_path), repair_targeted=repair_targeted,
                expected={str(p): digest(p) for p in files}, state_dir=row['state_dir'], archive_root=row['archive_root'])


def work_guard(work, actor, policy):
    output = subprocess.check_output(['compound', 'work', 'show', work, '--json'], text=True)
    current = json.loads(output)
    if current.get('state') != 'running' or current.get('policy_decision_id') != policy:
        raise ValueError('capture upgrade work reservation changed')
    lifecycle = current.get('lifecycle', [])
    if not lifecycle or lifecycle[-1].get('by') != actor or not any(a.startswith('act_') for a in lifecycle[-1].get('attached', [])):
        raise ValueError('capture upgrade actor/action reservation changed')


def service_pid(label):
    result = subprocess.run(['launchctl', 'print', 'gui/' + str(os.getuid()) + '/' + label], capture_output=True, text=True)
    if result.returncode:
        return None
    match = re.search(r'^\s*pid = ([0-9]+)\s*$', result.stdout, re.M)
    return int(match.group(1)) if match else None


def connections(pid):
    result = subprocess.run(['lsof', '-nP', '-a', '-p', str(pid), '-iTCP', '-FftnT'], capture_output=True, text=True)
    if result.returncode not in (0, 1):
        raise ValueError('capture liveness probe failed')
    return [line for line in result.stdout.splitlines() if line.startswith('TST=') and line != 'TST=LISTEN']


def wait_quiet(label, seconds=5):
    pid = service_pid(label)
    if not pid:
        raise ValueError('capture service is not live')
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if service_pid(label) != pid or connections(pid):
            raise ValueError('capture upgrade deferred: active connections or service identity drift')
        time.sleep(0.2)
    return pid


def stop_service(plan):
    subprocess.run(['launchctl', 'bootout', 'gui/' + str(os.getuid()) + '/' + plan['label']], check=True)
    end = time.monotonic() + 10
    while service_pid(plan['label']) and time.monotonic() < end:
        time.sleep(0.1)
    if service_pid(plan['label']):
        raise ValueError('capture service did not stop')


def start_service(plan):
    subprocess.run(['launchctl', 'bootstrap', 'gui/' + str(os.getuid()), plan['launch_path']], check=True)
    end = time.monotonic() + 10
    while not service_pid(plan['label']) and time.monotonic() < end:
        time.sleep(0.1)
    if not service_pid(plan['label']):
        raise ValueError('capture service did not become live')


def restart_options(allow_active_restart, restart_reason):
    reason = (restart_reason or '').strip()
    if allow_active_restart and not reason:
        raise ValueError('operator-approved active restart requires a restart reason')
    if reason and not allow_active_restart:
        raise ValueError('restart reason requires --allow-active-restart')
    return reason


def apply_locked(plan, work, actor, policy, allow_active_restart=False, restart_reason=None):
    reason = restart_options(allow_active_restart, restart_reason)
    work_guard(work, actor, policy)
    for name, expected in plan['expected'].items():
        if digest(regular(Path(name))) != expected:
            raise ValueError('capture upgrade input changed: ' + name)
    if digest(regular(Path(plan['binary']))) != plan['binary_sha256']:
        raise ValueError('candidate engine changed')
    pid = service_pid(plan['label']) if allow_active_restart else wait_quiet(plan['label'])
    if not pid:
        raise ValueError('capture service is not live')
    plan = dict(plan, cutover_mode='approved_restart' if allow_active_restart else 'quiet',
                restart_reason=reason or None)
    work_guard(work, actor, policy)
    root = Path(plan['runtime']) / 'generations' / plan['generation']
    journal = root / 'engine-upgrades' / (time.strftime('%Y%m%dT%H%M%SZ', time.gmtime()) + '-' + plan['binary_sha256'][:12])
    journal.mkdir(parents=True)
    targets = [Path(plan[k]) for k in ('engine', 'manifest_path', 'registry_path')]
    if plan['repair_targeted']:
        targets.append(Path(plan['repair_path']))
    originals = {}
    for n, path in enumerate(targets):
        backup = journal / (str(n) + '.original')
        shutil.copy2(path, backup)
        if digest(backup) != plan['expected'][str(path)]:
            raise ValueError('rollback backup changed')
        originals[path] = (backup, path.stat().st_mode & 0o777)
    (journal / 'plan.json').write_bytes(encode(plan))
    work_guard(work, actor, policy)
    if service_pid(plan['label']) != pid:
        raise ValueError('capture changed at cutover')
    active_connections = len(connections(pid))
    if active_connections and not allow_active_restart:
        raise ValueError('capture changed at cutover')
    stopped = False
    changed = False
    activated = False
    try:
        stop_service(plan)
        stopped = True
        work_guard(work, actor, policy)
        manifest = read(Path(plan['manifest_path']))
        manifest['source_commit'] = plan['source_commit']
        manifest_bytes = encode(manifest)
        registry = read(Path(plan['registry_path']))
        row = next(g for g in registry['generations'] if g['id'] == plan['generation'])
        row['binary_sha256'] = plan['binary_sha256']
        row['manifest_sha256'] = hashlib.sha256(manifest_bytes).hexdigest()
        atomic(Path(plan['engine']), Path(plan['binary']).read_bytes(), 0o755)
        changed = True
        atomic(Path(plan['manifest_path']), manifest_bytes, originals[Path(plan['manifest_path'])][1])
        atomic(Path(plan['registry_path']), encode(registry), originals[Path(plan['registry_path'])][1])
        if plan['repair_targeted']:
            repair = read(Path(plan['repair_path']))
            repair['engine_sha256'] = plan['binary_sha256']
            atomic(Path(plan['repair_path']), encode(repair), originals[Path(plan['repair_path'])][1])
        start_service(plan)
        stopped = False
        activated = True
        receipt = dict(schema='switchback/capture-engine-upgrade-receipt@1', work_id=work, policy_decision_id=policy,
                       generation=plan['generation'], previous_pid=pid, current_pid=service_pid(plan['label']),
                       binary_sha256=plan['binary_sha256'], source_commit=plan['source_commit'],
                       state_dir=plan['state_dir'], archive_root=plan['archive_root'], journal=str(journal), upgraded=True,
                       cutover_mode=plan['cutover_mode'], active_connections_at_cutover=active_connections,
                       restart_reason=plan['restart_reason'])
        (journal / 'receipt.json').write_bytes(encode(receipt))
        return receipt
    except BaseException:
        if activated:
            # A receipt IO failure is audit debt, not a reason to interrupt a live new engine.
            raise
        if not stopped and not changed:
            raise
        # Process-start and authority-write failures restore the exact owner generation.
        if service_pid(plan['label']):
            stop_service(plan)
        for target, (backup, mode) in originals.items():
            atomic(target, backup.read_bytes(), mode)
        start_service(plan)
        stopped = False
        raise
    finally:
        if stopped:
            start_service(plan)


def apply(plan, work, actor, policy, allow_active_restart=False, restart_reason=None):
    restart_options(allow_active_restart, restart_reason)
    work_guard(work, actor, policy)
    lock_path = Path(plan['runtime']) / 'config/.capture-engine.lock'
    if lock_path.is_symlink():
        raise ValueError('unsafe capture engine lock')
    with lock_path.open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return apply_locked(plan, work, actor, policy, allow_active_restart, restart_reason)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime', type=Path, required=True)
    parser.add_argument('--generation', required=True)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--source-commit', required=True)
    parser.add_argument('--apply', action='store_true')
    parser.add_argument('--work')
    parser.add_argument('--actor')
    parser.add_argument('--expected-policy')
    parser.add_argument('--allow-active-restart', action='store_true',
                        help='Operator-approved maintenance: active connections may be interrupted')
    parser.add_argument('--restart-reason', help='Record the operator-approved maintenance reason')
    args = parser.parse_args()
    restart_options(args.allow_active_restart, args.restart_reason)
    proposed = plan(args.runtime, args.generation, args.binary, args.source_commit)
    if args.apply:
        if not all((args.work, args.actor, args.expected_policy)):
            parser.error('apply requires current running work, actor and exact PolicyDecision')
        print(json.dumps(apply(proposed, args.work, args.actor, args.expected_policy,
                               args.allow_active_restart, args.restart_reason), sort_keys=True))
    else:
        proposed['active_connections'] = len(connections(service_pid(proposed['label']))) if service_pid(proposed['label']) else None
        proposed['cutover_mode'] = 'approved_restart' if args.allow_active_restart else 'quiet'
        proposed['restart_reason'] = (args.restart_reason or '').strip() or None
        print(json.dumps(proposed, sort_keys=True))


if __name__ == '__main__':
    main()
