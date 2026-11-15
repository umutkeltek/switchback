#!/usr/bin/env python3
"""Owned, staged publication for the signed Switchback menu bar bundle."""
import datetime
import json
import os
from pathlib import Path
import plistlib
import shutil
import stat
import subprocess
import sys
import tempfile
import uuid

OWNER = "switchback-menubar-owned-v1"


class Refusal(RuntimeError):
    pass


def safe_dir(path, create=False):
    path = Path(os.path.abspath(path))
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        try:
            info = current.lstat()
        except FileNotFoundError:
            if not create:
                raise Refusal("Missing directory: " + str(current))
            current.mkdir(mode=0o700)
            info = current.lstat()
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise Refusal("Unsafe directory: " + str(current))
        if info.st_uid not in (0, os.getuid()):
            raise Refusal("Foreign-owned directory: " + str(current))
        if info.st_uid == os.getuid() and info.st_mode & 0o022:
            raise Refusal("Directory is writable by others: " + str(current))
    if path.stat().st_uid != os.getuid():
        raise Refusal("Directory is not owned by current user: " + str(path))
    return path


def owned_bytes(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        info = os.fstat(stream.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o022 or info.st_size > 1_048_576):
            raise Refusal("Unsafe metadata file: " + str(path))
        return stream.read()


def attest_app(path):
    safe_dir(path)
    safe_dir(path / "Contents/Resources")
    safe_dir(path / "Contents/MacOS")
    marker = owned_bytes(path / "Contents/Resources/switchback-owner").decode().strip()
    info = plistlib.loads(owned_bytes(path / "Contents/Info.plist"))
    if (marker != OWNER or not isinstance(info, dict)
            or info.get("CFBundleIdentifier") != "dev.switchback.menubar"):
        raise Refusal("Refusing to replace an unowned app: " + str(path))


def verify(path):
    subprocess.run(["/usr/bin/codesign", "--verify", "--strict", str(path)], check=True)


def install_bundle(source, runtime, login_agent=None):
    if not Path(runtime).is_absolute():
        raise Refusal("Runtime root must be absolute")
    runtime = safe_dir(runtime)
    safe_dir(runtime / "bin")
    manifest_bytes = owned_bytes(runtime / "manifest.json")
    manifest = json.loads(manifest_bytes)
    if (not isinstance(manifest, dict)
            or manifest.get("schema") != "switchback/runtime-manifest@1"
            or manifest.get("owner") != "switchback"):
        raise Refusal("Runtime ownership is not proven")
    source = Path(source)
    attest_app(source)
    verify(source)
    apps = runtime / "apps"
    if apps.exists() or apps.is_symlink():
        safe_dir(apps)
    destination = apps / "SwitchbackBar.app"
    had_app = destination.exists() or destination.is_symlink()
    if had_app:
        attest_app(destination)
    agent = Path(login_agent) if login_agent else None
    if agent:
        if agent.parent.exists() or agent.parent.is_symlink():
            safe_dir(agent.parent)
        if agent.exists() or agent.is_symlink():
            existing = plistlib.loads(owned_bytes(agent))
            if existing.get("SwitchbackOwner") != OWNER:
                raise Refusal("Refusing to replace an unowned login registration")
    safe_dir(apps, create=True)
    suffix = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "." + uuid.uuid4().hex
    staging = Path(tempfile.mkdtemp(prefix=".menubar-stage-", dir=apps))
    candidate = staging / "SwitchbackBar.app"
    previous = destination.with_name(destination.name + ".previous." + suffix)
    failed = destination.with_name(destination.name + ".failed." + suffix)
    old_moved = published = agent_moved = agent_published = False
    agent_stage = agent_previous = None
    try:
        shutil.copytree(source, candidate, symlinks=True)
        attest_app(candidate)
        verify(candidate)
        if agent:
            safe_dir(agent.parent, create=True)
            descriptor, temporary = tempfile.mkstemp(prefix=".menubar-login-", dir=agent.parent)
            agent_stage = Path(temporary)
            with os.fdopen(descriptor, "wb") as stream:
                plistlib.dump({
                    "Label": "dev.switchback.menubar",
                    "ProgramArguments": [str(destination / "Contents/MacOS/SwitchbackBar")],
                    "RunAtLoad": True, "KeepAlive": False, "SwitchbackOwner": OWNER,
                    "EnvironmentVariables": {"SWITCHBACK_RUNTIME_ROOT": str(runtime),
                                             "SB_RUNTIME_ROOT": str(runtime)},
                }, stream)
                stream.flush()
                os.fsync(stream.fileno())
            agent_previous = agent.with_name(agent.name + ".previous." + suffix)
        # Recheck ownership after staging and immediately before publication.
        safe_dir(runtime)
        safe_dir(apps)
        if owned_bytes(runtime / "manifest.json") != manifest_bytes:
            raise Refusal("Runtime manifest changed during staging")
        if had_app:
            attest_app(destination)
            os.replace(destination, previous)
            old_moved = True
        elif destination.exists() or destination.is_symlink():
            raise Refusal("App destination appeared during staging")
        os.replace(candidate, destination)
        published = True
        verify(destination)
        if agent:
            if agent.exists() or agent.is_symlink():
                if plistlib.loads(owned_bytes(agent)).get("SwitchbackOwner") != OWNER:
                    raise Refusal("Login registration ownership changed")
                os.replace(agent, agent_previous)
                agent_moved = True
            os.replace(agent_stage, agent)
            agent_published = True
        return destination, previous if old_moved else None
    except BaseException:
        # Keep both old bytes and the failed new bundle; restore the prior published state.
        if agent_published:
            os.replace(agent, agent.with_name(agent.name + ".failed." + suffix))
        if agent_moved:
            os.replace(agent_previous, agent)
        if published:
            os.replace(destination, failed)
        if old_moved:
            os.replace(previous, destination)
        raise
    finally:
        # Only our unique temporary staging paths are eligible for cleanup.
        if agent_stage and agent_stage.exists():
            agent_stage.unlink()
        if staging.exists():
            shutil.rmtree(staging)


def main():
    runtime, source, login = sys.argv[1:]
    agent = Path.home() / "Library/LaunchAgents/dev.switchback.menubar.plist" if login == "true" else None
    try:
        destination, previous = install_bundle(Path(source), Path(runtime), agent)
    except (OSError, ValueError, Refusal, subprocess.CalledProcessError) as error:
        raise SystemExit("Menu bar installation refused or rolled back: " + str(error))
    print("Installed: " + str(destination))
    if previous:
        print("Previous app retained: " + str(previous))
    if agent:
        print("Registered for next login; not launched.")


if __name__ == "__main__":
    main()
