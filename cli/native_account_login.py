#!/usr/bin/env python3
"""Private, bounded background supervision of the existing native login command.

Call start_login with the foreground account command, then poll_login/cancel_login.
Native vendor CLIs alone implement OAuth and credential storage. No output is logged.
Browser OAuth handoff is supported; use foreground device-auth for user codes.
"""
import contextlib
import fcntl
import json
import math
import os
from pathlib import Path
import re
import selectors
import signal
import stat
import subprocess
import sys
import tempfile
import time
from urllib.parse import parse_qsl, urlsplit
import uuid

from native_account_process import Refusal

FINAL = frozenset(("completed", "failed", "cancelled", "expired"))
ACTIVE = frozenset(("pending", "waiting"))
NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
FLOW = re.compile(r"^login_[a-f0-9]{32}$")
URL = re.compile(rb"""https://[^\s<>\x00-\x20\x7f"']{1,8192}(?=[\s<>\x00-\x20\x7f"'])""")
LIMIT = 1024 * 1024

# A live session leader reserves the PGID. The supervisor keeps a private pipe
# open; EOF makes the leader clean up its own group, including orphan descendants.
# The native command cannot inherit the status or lifeline descriptors.
ANCHOR = r"""
import os, selectors, signal, subprocess, sys
status, life = int(sys.argv[1]), int(sys.argv[2])
os.set_inheritable(status, False)
os.set_inheritable(life, False)
child = None
try:
    child = subprocess.Popen(sys.argv[3:], stdin=subprocess.DEVNULL, close_fds=True)
    watch = selectors.DefaultSelector()
    watch.register(life, selectors.EVENT_READ)
    while True:
        if watch.select(0.1):
            if not os.read(life, 1):
                os.killpg(os.getpid(), signal.SIGKILL)
        if child is not None and child.poll() is not None:
            os.write(status, (str(child.returncode) + "\n").encode())
            os.close(status)
            child = None
except BaseException:
    try:
        os.write(status, b"127\n")
        os.close(status)
    except OSError:
        pass
    os.killpg(os.getpid(), signal.SIGKILL)
"""


def _safe_dir(path, create=False):
    path = Path(os.path.abspath(str(path)))
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current = current / part
        try:
            info = current.lstat()
        except FileNotFoundError:
            if not create:
                raise Refusal("missing login state directory")
            try:
                current.mkdir(mode=0o700)
            except FileExistsError:
                pass
            info = current.lstat()
        if not stat.S_ISDIR(info.st_mode) or stat.S_ISLNK(info.st_mode):
            raise Refusal("unsafe login state directory")
        if info.st_uid not in (0, os.getuid()):
            raise Refusal("foreign login state directory")
        if info.st_uid == os.getuid() and info.st_mode & 0o022:
            raise Refusal("writable login state directory")
    info = path.stat()
    if info.st_uid != os.getuid():
        raise Refusal("login state must be owned by current user")
    return path


def _root(runtime, create=False):
    if not os.path.isabs(str(runtime)):
        raise Refusal("an absolute owned runtime is required")
    runtime = _safe_dir(runtime)
    root = _safe_dir(runtime / "state" / "native-account-logins", create=create)
    if root.stat().st_mode & 0o077:
        raise Refusal("login flow directory must be private")
    return root


def _open(path, flags):
    fd = os.open(str(path), flags | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    info = os.fstat(fd)
    if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
            or info.st_mode & 0o077 or info.st_nlink != 1 or info.st_size > 16384):
        os.close(fd)
        raise Refusal("unsafe login flow file")
    return fd


@contextlib.contextmanager
def _lock(path):
    fd = _open(path, os.O_RDWR | os.O_CREAT)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield fd
    finally:
        os.close(fd)


def _read(path):
    fd = _open(path, os.O_RDONLY)
    try:
        with os.fdopen(fd, "r", encoding="utf-8") as stream:
            data = json.load(stream)
    except (ValueError, UnicodeDecodeError):
        raise Refusal("invalid login flow metadata")
    if not isinstance(data, dict):
        raise Refusal("invalid login flow metadata")
    return data


def _write(path, data):
    if path.exists() or path.is_symlink():
        fd = _open(path, os.O_RDONLY)
        os.close(fd)
    fd, name = tempfile.mkstemp(prefix=".login-", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(data, stream, separators=(",", ":"))
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, str(path))
    finally:
        if os.path.exists(name):
            os.unlink(name)


def _flow(runtime, flow_id):
    if not isinstance(flow_id, str) or not FLOW.fullmatch(flow_id):
        raise Refusal("invalid login flow id")
    path = _safe_dir(_root(runtime) / flow_id)
    if path.stat().st_mode & 0o077:
        raise Refusal("login flow directory must be private")
    return path


def _state(path):
    data = _read(path / "status.json")
    if (data.get("flow_id") != path.name or data.get("provider") not in ("claude", "codex")
            or not isinstance(data.get("slot"), str) or not NAME.fullmatch(data["slot"])
            or data.get("state") not in ACTIVE | FINAL
            or not isinstance(data.get("expires_at"), (int, float))
            or not math.isfinite(data["expires_at"])):
        raise Refusal("invalid login flow metadata")
    return data


def _finish(path, state):
    with _lock(path / ".guard"):
        data = _state(path)
        if data["state"] not in FINAL:
            data["state"] = state
            data["relogin_required"] = state != "completed"
            data.pop("authorization_url", None)
            _write(path / "status.json", data)
        return data


def _live(path):
    fd = _open(path / ".alive", os.O_RDWR)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False
    finally:
        os.close(fd)


def poll_login(runtime, flow_id):
    """Return private handoff metadata; callers must not log authorization_url."""
    path = _flow(runtime, flow_id)
    with _lock(path / ".guard"):
        data = _state(path)
        if data["state"] not in FINAL:
            state = None
            if time.time() >= data["expires_at"]:
                state = "expired"
            elif not _live(path):
                state = "failed"
            if state:
                data["state"] = state
                data["relogin_required"] = True
                data.pop("authorization_url", None)
                _write(path / "status.json", data)
        return data


def cancel_login(runtime, flow_id):
    """Commit cancellation under the same guard read by the live worker."""
    path = _flow(runtime, flow_id)
    # Terminal state itself is the cancellation request. Only the live worker
    # knows a child PGID; polling clients never signal anything.
    return _finish(path, "cancelled")


def authorization_url(provider, value):
    """Accept only native vendor authorization endpoints, never arbitrary URLs."""
    if (not isinstance(value, str) or len(value) > 8192
            or any(ord(c) <= 32 or ord(c) >= 127 for c in value)
            or re.search(r"%(?![A-Fa-f0-9]{2})", value) or "\\" in value):
        return None
    try:
        url = urlsplit(value)
        allowed = {
            "claude": {("claude.ai", "/oauth/authorize"), ("console.anthropic.com", "/oauth/authorize"),
                       ("claude.com", "/cai/oauth/authorize"), ("platform.claude.com", "/oauth/authorize")},
            "codex": {("auth.openai.com", "/oauth/authorize"), ("auth.openai.com", "/codex/device")},
        }
        if (url.scheme != "https" or url.username or url.password or url.port
                or url.fragment or (url.hostname, url.path) not in allowed.get(provider, set())):
            return None
        if any((key.lower() == "code" and val != "true")
               or key.lower() in ("token", "access_token", "refresh_token", "id_token", "password")
               for key, val in parse_qsl(url.query, keep_blank_values=True)):
            return None
    except ValueError:
        return None
    return value


def start_login(runtime, provider, slot, command, timeout=300, env=None):
    """Start one foreground native login per provider/slot in a detached worker."""
    if provider not in ("claude", "codex") or not isinstance(slot, str) or not NAME.fullmatch(slot):
        raise Refusal("invalid native login provider or slot")
    if (isinstance(timeout, bool) or not isinstance(timeout, (int, float))
            or not math.isfinite(timeout) or not 1 <= timeout <= 2700):
        raise Refusal("login timeout must be between 1 and 2700 seconds")
    if (not isinstance(command, (list, tuple)) or not command
            or not all(isinstance(arg, str) and arg and "\0" not in arg for arg in command)):
        raise Refusal("invalid foreground login command")
    root = _root(runtime, create=True)
    slot_fd = _open(root / ("." + provider + "-" + slot + ".lock"), os.O_RDWR | os.O_CREAT)
    alive_fd = None
    path = None
    try:
        try:
            fcntl.flock(slot_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise Refusal("native login already active for this slot")
        flow_id = "login_" + uuid.uuid4().hex
        path = root / flow_id
        path.mkdir(mode=0o700)
        alive_fd = _open(path / ".alive", os.O_RDWR | os.O_CREAT)
        fcntl.flock(alive_fd, fcntl.LOCK_EX)
        now = time.time()
        data = {"flow_id": flow_id, "provider": provider, "slot": slot,
                "state": "pending", "created_at": now, "expires_at": now + timeout,
                "relogin_required": True}
        _write(path / "status.json", data)
        worker = subprocess.Popen(
            [sys.executable, "-B", str(Path(__file__).resolve()), "--worker",
             str(runtime), flow_id, str(slot_fd), str(alive_fd)],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, close_fds=True, pass_fds=(slot_fd, alive_fd),
            env=os.environ.copy() if env is None else env)
        # The command is passed through an anonymous pipe, never a metadata file
        # or process argument. Closing stdin also bounds worker initialization.
        try:
            worker.stdin.write(json.dumps(list(command)).encode("utf-8"))
            worker.stdin.close()
        except (OSError, ValueError):
            _finish(path, "failed")
        # A double fork is unnecessary: init reaps detached workers after caller
        # exit; retaining no pipes prevents them from holding callers open.
        return data
    except BaseException:
        if path is not None and (path / "status.json").exists():
            _finish(path, "failed")
        raise
    finally:
        if alive_fd is not None:
            os.close(alive_fd)
        os.close(slot_fd)


def _waiting(path, provider, chunk, pending):
    pending = (pending + chunk)[-16384:]
    for match in URL.finditer(pending):
        try:
            raw = match.group().decode("ascii").rstrip(").,;")
        except UnicodeDecodeError:
            continue
        value = authorization_url(provider, raw)
        if value:
            with _lock(path / ".guard"):
                data = _state(path)
                if data["state"] in ACTIVE:
                    data["state"] = "waiting"
                    data["authorization_url"] = value
                    _write(path / "status.json", data)
    return pending


def _run(path, command):
    data = _state(path)
    remaining = data["expires_at"] - time.time()
    if data["state"] in FINAL or remaining <= 0:
        return _finish(path, "expired" if remaining <= 0 else data["state"])
    status_r, status_w = os.pipe()
    life_r, life_w = os.pipe()
    anchor = None
    watch = selectors.DefaultSelector()
    previous = {}
    interrupted = []
    for sig in (signal.SIGTERM, signal.SIGINT):
        previous[sig] = signal.signal(sig, lambda *_: interrupted.append(True))
    try:
        # Serialize start against cancellation: a cancelled flow cannot launch.
        with _lock(path / ".guard"):
            data = _state(path)
            if data["state"] in FINAL:
                return data
            anchor = subprocess.Popen(
                [sys.executable, "-B", "-c", ANCHOR, str(status_w), str(life_r)] + command,
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                close_fds=True, pass_fds=(status_w, life_r), start_new_session=True)
        os.close(status_w)
        status_w = None
        os.close(life_r)
        life_r = None
        watch.register(status_r, selectors.EVENT_READ, "status")
        watch.register(anchor.stdout, selectors.EVENT_READ, "out")
        watch.register(anchor.stderr, selectors.EVENT_READ, "err")
        deadline = time.monotonic() + remaining
        pending = {"out": b"", "err": b""}
        status_output = b""
        total = 0
        result = "failed"
        while True:
            current = _state(path)
            if current["state"] in FINAL:
                result = current["state"]
                break
            if interrupted:
                result = "cancelled"
                break
            if time.monotonic() >= deadline:
                result = "expired"
                break
            for key, _ in watch.select(min(0.1, max(0, deadline - time.monotonic()))):
                chunk = os.read(key.fd, 16384)
                if not chunk:
                    watch.unregister(key.fileobj)
                    continue
                if key.data == "status":
                    status_output += chunk
                else:
                    total += len(chunk)
                    if total <= LIMIT:
                        pending[key.data] = _waiting(path, data["provider"], chunk, pending[key.data])
            if total > LIMIT:
                break
            if b"\n" in status_output:
                result = "completed" if status_output == b"0\n" else "failed"
                break
            if anchor.poll() is not None:
                break
        return _finish(path, result)
    finally:
        watch.close()
        # Close the lifeline first. The reserved anchor also kills its group if
        # this supervisor is terminated before reaching the explicit cleanup.
        os.close(life_w)
        if anchor is not None:
            try:
                if anchor.poll() is None:
                    os.killpg(anchor.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            finally:
                anchor.wait(timeout=2)
                anchor.stdout.close()
                anchor.stderr.close()
        for fd in (status_r, status_w, life_r):
            if fd is not None:
                os.close(fd)
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def _worker(runtime, flow_id, slot_fd, alive_fd):
    path = _flow(runtime, flow_id)
    try:
        # stdin comes only from start_login and is closed before it returns.
        raw = sys.stdin.buffer.read(65537)
        command = json.loads(raw) if len(raw) <= 65536 else None
        if not isinstance(command, list) or not command or not all(isinstance(arg, str) for arg in command):
            raise Refusal("invalid login worker command")
        _run(path, command)
    except BaseException:
        _finish(path, "failed")
    finally:
        os.close(alive_fd)
        os.close(slot_fd)


if __name__ == "__main__":
    if len(sys.argv) != 6 or sys.argv[1] != "--worker":
        raise SystemExit(2)
    _worker(sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]))
