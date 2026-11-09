"""Bounded native Codex app-server account pool.

The caller provides the existing slot/receipt filesystem operations. App-server
owns authentication and rollout files; no credential bytes are read here.
"""
import contextlib
import hashlib
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
import time
import uuid

from native_account_process import Refusal, admission_check

LIMIT = 1024 * 1024
ANCHOR = r"""
import os, signal, subprocess, sys
fd = int(sys.argv[1])
os.set_inheritable(fd, False)
try:
    child = subprocess.Popen(sys.argv[2:], stdin=sys.stdin)
    null = os.open(os.devnull, os.O_RDWR)
    for dest in (0, 1, 2):
        os.dup2(null, dest)
    os.close(null)
    code = child.wait()
except BaseException:
    code = 127
os.write(fd, (str(code) + "\n").encode())
os.close(fd)
while True:
    signal.pause()
"""

class Cancelled(Refusal):
    pass

class App:
    def __init__(self, cmd, env, deadline):
        self.deadline = deadline
        read_fd, write_fd = os.pipe()
        try:
            self.proc = subprocess.Popen([sys.executable, "-c", ANCHOR, str(write_fd)] + cmd,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                env=env, start_new_session=True, pass_fds=(write_fd,))
        except BaseException:
            os.close(read_fd)
            os.close(write_fd)
            raise
        os.close(write_fd)
        self.read_fd = read_fd
        self.sel = selectors.DefaultSelector()
        self.sel.register(self.proc.stdout, selectors.EVENT_READ, "stdout")
        self.sel.register(self.proc.stderr, selectors.EVENT_READ, "stderr")
        self.sel.register(read_fd, selectors.EVENT_READ, "status")
        os.set_blocking(self.proc.stdin.fileno(), False)
        self.buf = b""
        self.next_id = 0
        self.pending_turn = None
        self.active_thread = None
        self.forward = False
        self.cancelled = None
        self.handlers = {}
        for sig in (signal.SIGINT, signal.SIGTERM):
            self.handlers[sig] = signal.signal(sig, self._cancel)

    def _cancel(self, signum, frame):
        self.cancelled = signum

    def remaining(self):
        if self.cancelled:
            raise Cancelled("Codex execution cancelled")
        left = self.deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError("Codex deadline expired")
        return left

    def send(self, value):
        payload = (json.dumps(value, separators=(",", ":")) + "\n").encode()
        if len(payload) > LIMIT:
            raise Refusal("Codex request exceeds limit")
        import select
        pos = 0
        while pos < len(payload):
            _, ready, _ = select.select([], [self.proc.stdin], [], min(.2, self.remaining()))
            if ready:
                try:
                    pos += os.write(self.proc.stdin.fileno(), payload[pos:])
                except BlockingIOError:
                    continue

    def event(self):
        while True:
            if b"\n" in self.buf:
                line, self.buf = self.buf.split(b"\n", 1)
                if len(line) > LIMIT:
                    raise Refusal("Codex event exceeds limit")
                try:
                    value = json.loads(line)
                except (ValueError, UnicodeDecodeError) as e:
                    raise Refusal("malformed Codex event") from e
                if not isinstance(value, dict):
                    raise Refusal("malformed Codex event")
                return value
            for key, _ in self.sel.select(min(.2, self.remaining())):
                chunk = os.read(key.fd, 16384)
                if not chunk:
                    self.sel.unregister(key.fileobj)
                    if key.data in ("stdout", "status"):
                        raise Refusal("Codex app-server exited before completion")
                    continue
                if key.data == "stdout":
                    self.buf += chunk
                    if len(self.buf) > LIMIT and b"\n" not in self.buf:
                        raise Refusal("Codex event exceeds limit")
                elif key.data == "status":
                    raise Refusal("Codex app-server exited before completion")

    def notification(self, value):
        method = value.get("method")
        if method is not None and "id" in value:
            raise Refusal("Codex requires an interactive approval or unsupported server request")
        params = value.get("params")
        if self.active_thread and isinstance(params, dict):
            thread = params.get("threadId")
            if thread is not None and thread != self.active_thread:
                raise Refusal("Codex event belongs to a different thread")
        if self.forward and method:
            print(json.dumps(value, separators=(",", ":")), flush=True)
        if method == "turn/completed":
            self.pending_turn = params

    def call(self, method, params):
        self.next_id += 1
        ident = self.next_id
        self.send({"id": ident, "method": method, "params": params})
        while True:
            value = self.event()
            if value.get("id") == ident and "method" not in value:
                if "error" in value or not isinstance(value.get("result"), dict):
                    raise Refusal("Codex app-server request failed: " + method)
                return value["result"]
            self.notification(value)

    def wait_turn(self, thread_id, turn_id):
        while self.pending_turn is None:
            self.notification(self.event())
        result = self.pending_turn
        if not isinstance(result, dict) or result.get("threadId") != thread_id:
            raise Refusal("Codex completed a different thread")
        turn = result.get("turn")
        if not isinstance(turn, dict) or turn.get("id") != turn_id:
            raise Refusal("Codex completed a different turn")
        return turn

    def close(self, require_grace=True):
        # EOF asks app-server to flush and close its rollout. The anchor keeps
        # the process group ID reserved until all remaining processes are killed.
        try:
            self.proc.stdin.close()
            status = b""
            clean = False
            end = min(self.deadline, time.monotonic() + 5)
            while time.monotonic() < end and self.sel.get_map():
                for key, _ in self.sel.select(min(.1, end - time.monotonic())):
                    data = os.read(key.fd, 16384)
                    if not data:
                        self.sel.unregister(key.fileobj)
                    elif key.data == "status":
                        status += data
                        if len(status) > 32:
                            break
                if not self.sel.get_map():
                    clean = status.strip() == b"0"
                    break
            if require_grace and not clean:
                raise Refusal("Codex rollout shutdown was not proven")
        finally:
            self.sel.close()
            os.close(self.read_fd)
            try:
                os.killpg(self.proc.pid, signal.SIGKILL)
            except OSError as e:
                raise Refusal("Codex process group could not be terminated") from e
            finally:
                self.proc.wait(timeout=2)
                for sig, handler in self.handlers.items():
                    signal.signal(sig, handler)

def rollout(path, home, thread_id, cwd, fs):
    if not isinstance(path, str) or not os.path.isabs(path):
        return None
    target = Path(path)
    if ".." in target.parts:
        return None
    try:
        root = fs.safe_dir(home / "sessions")
        target.relative_to(root)
        fs.safe_dir(target.parent)
        fd = os.open(target, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, "rb") as stream:
            st = os.fstat(stream.fileno())
            if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o022:
                return None
            line = stream.readline(65537)
        if len(line) > 65536:
            return None
        event = json.loads(line)
        payload = event.get("payload", {})
        if event.get("type") != "session_meta" or payload.get("id") != thread_id or payload.get("cwd") != cwd:
            return None
        return str(target)
    except (OSError, ValueError, TypeError, Refusal):
        return None

def limits(result):
    if not isinstance(result, dict):
        return None, None
    snapshot = result.get("rateLimits")
    if not isinstance(snapshot, dict):
        return None, None
    exhausted = result.get("ordinaryUsageAllowed") is False
    resets = []
    for key in ("primary", "secondary"):
        window = snapshot.get(key)
        if window is None:
            continue
        if not isinstance(window, dict):
            return None, None
        used = window.get("usedPercent")
        if isinstance(used, bool) or not isinstance(used, (int, float)) or not math.isfinite(used) or not 0 <= used <= 100:
            return None, None
        if used >= 100:
            exhausted = True
            reset = window.get("resetsAt")
            if isinstance(reset, (int, float)) and not isinstance(reset, bool) and math.isfinite(reset) and reset > time.time():
                resets.append(reset)
            else:
                return True, None
    if not exhausted and result.get("ordinaryUsageAllowed") is not True:
        return None, None
    return exhausted, max(resets) if resets else None

def account_identity(result):
    routing = result.get("workspaceRouting") if isinstance(result, dict) else None
    identity = routing.get("chatgptAccountId") if isinstance(routing, dict) else None
    if isinstance(identity, str) and 0 < len(identity) <= 256:
        return {"source": "vendor_account_id", "value": identity}
    account = result.get("account") if isinstance(result, dict) else None
    email = account.get("email") if isinstance(account, dict) else None
    if isinstance(email, str) and 0 < len(email) <= 320:
        return {"source": "vendor_email_sha256",
                "value": hashlib.sha256(email.strip().lower().encode()).hexdigest()}
    return None

def inspect_account(home, env, binary, timeout=45):
    app = App([binary, "app-server"], env, time.monotonic() + timeout)
    try:
        app.call("initialize", {"clientInfo": {"name": "switchback", "version": "1"},
                                "capabilities": {"experimentalApi": True}})
        app.send({"method": "initialized", "params": {}})
        account = app.call("account/read", {"refreshToken": True})
        value = account.get("account")
        if not isinstance(value, dict) or value.get("type") != "chatgpt":
            return {"authenticated": False, "state": "relogin_required", "quota": "unknown"}
        try:
            exhausted, reset = limits(app.call("account/rateLimits/read", {}))
        except Refusal:
            exhausted, reset = None, None
        quota = "unknown" if exhausted is None else {"exhausted": exhausted, "reset": reset}
        identity = account_identity(account)
        return {"authenticated": True, "state": "ready" if identity else "identity_unverified",
                "quota": quota, "_identity": identity}
    finally:
        app.close()

def run(args, runtime, root, fs):
    with contextlib.ExitStack() as session_locks:
        return _run(args, runtime, root, fs, session_locks)

def _run(args, runtime, root, fs, session_locks):
    names = args.pool.split(",") if args.pool else []
    if not names or len(names) > 16 or len(set(names)) != len(names) or any(not fs.valid_name(x) or x.lower() == "default" for x in names):
        raise Refusal("Codex run needs an explicit pool of distinct non-default slots")
    if args.max_attempts is None:
        args.max_attempts = len(names)
    if not args.prompt or not 1 <= args.timeout <= 2700 or not 1 <= args.max_attempts <= len(names):
        raise Refusal("Codex run needs a prompt and bounded attempts and timeout")
    if args.approval_policy not in ("never", "on-request", "untrusted") or args.sandbox not in ("read-only", "workspace-write", "danger-full-access"):
        raise Refusal("Codex run requires explicit --approval-policy and --sandbox")
    if args.expected_settings_sha256:
        raise Refusal("Claude settings digest does not attest Codex permissions")
    if args.max_budget_usd is not None or args.permission_mode or args.dangerously_skip_permissions or args.session_id:
        raise Refusal("unsupported Codex run flag")
    check = None
    if args.admission_check_command:
        try:
            check = json.loads(args.admission_check_command)
        except ValueError as e:
            raise Refusal("admission check must be a JSON array") from e
        if not isinstance(check, list) or not 1 <= len(check) <= 64 or not all(isinstance(x, str) and x and len(x) <= 4096 for x in check) or not os.path.isabs(check[0]):
            raise Refusal("invalid admission check command")
    deadline = time.monotonic() + args.timeout
    cwd = os.getcwd()
    previous = None
    earliest = []
    attempted = 0
    slots = []
    wrapper = Path(runtime) / "bin/entrypoints/codex-switchback-tap"
    if not wrapper.exists():
        wrapper = Path(__file__).absolute().parent / "wrappers/codex-switchback-tap"
    if not wrapper.is_file():
        raise Refusal("permission-preserving Codex capture wrapper is unavailable")
    for name in names:
        if attempted >= args.max_attempts:
            break
        if time.monotonic() >= deadline:
            return 124
        slot = fs.safe_dir(root / name)
        home = fs.safe_dir(Path(runtime) / "config/codex" / name)
        try:
            with fs.lock(slot / ".lock"):
                data = fs.read_json(slot / "slot.json")
                if not isinstance(data, dict) or data.get("slot") != name or data.get("provider") != "codex" or not data.get("enabled"):
                    slots.append({"slot": name, "state": "disabled_or_invalid"})
                    continue
                cooldown = data.get("cooldown")
                if cooldown:
                    reset = cooldown.get("reset") if isinstance(cooldown, dict) else None
                    if not isinstance(reset, (int, float)) or reset > time.time():
                        if isinstance(reset, (int, float)):
                            earliest.append(reset)
                        slots.append({"slot": name, "state": "quota_blocked"})
                        continue
                remaining = deadline - time.monotonic()
                admission_check(check, min(10, max(.01, remaining)))
                env = fs.clean_env("codex", home)
                attempted += 1
                receipts = fs.safe_dir(slot / "receipts", create=True)
                receipt_path = receipts / (str(uuid.uuid4()) + ".json")
                receipt = {"schema": "switchback/native-account-attempt@1", "provider": "codex",
                           "slot": name, "started_at": time.time(), "disposition": "running"}
                fs.write_json(receipt_path, receipt)
                app = None
                rejection = None
                reset = None
                success = False
                thread_id = None
                thread_path = None
                try:
                    app = App([str(wrapper), "--sb-preserve-permissions", "app-server"], env, deadline)
                    app.call("initialize", {"clientInfo": {"name": "switchback", "version": "1"},
                                            "capabilities": {"experimentalApi": True}})
                    app.send({"method": "initialized", "params": {}})
                    account = app.call("account/read", {"refreshToken": True})
                    if not isinstance(account.get("account"), dict) or account["account"].get("type") != "chatgpt":
                        slots.append({"slot": name, "state": "relogin_required"})
                        receipt["disposition"] = "relogin_required"
                        continue
                    observed_identity = account_identity(account)
                    if not observed_identity or data.get("identity") != observed_identity:
                        slots.append({"slot": name, "state": "identity_unverified_or_mismatch"})
                        receipt["disposition"] = "identity_unverified_or_mismatch"
                        continue
                    try:
                        quota = app.call("account/rateLimits/read", {})
                    except Refusal:
                        quota = None
                    exhausted, reset = limits(quota)
                    if exhausted:
                        data["cooldown"] = {"reason": "rate_limit", "reset": reset}
                        fs.write_json(slot / "slot.json", data)
                        if reset:
                            earliest.append(reset)
                        slots.append({"slot": name, "state": "quota_blocked"})
                        receipt["disposition"] = "quota_blocked"
                        continue
                    params = {"cwd": cwd, "approvalPolicy": args.approval_policy, "sandbox": args.sandbox}
                    if args.model:
                        params["model"] = args.model
                    if previous:
                        if rollout(previous["path"], Path(previous["home"]), previous["id"], cwd, fs) is None:
                            raise Refusal("Codex continuation rollout changed or is unsafe")
                        params.update({"threadId": previous["id"], "path": previous["path"]})
                        started = app.call("thread/resume", params)
                    else:
                        started = app.call("thread/start", params)
                    thread = started.get("thread")
                    if not isinstance(thread, dict) or not isinstance(thread.get("id"), str) or started.get("cwd") != cwd:
                        raise Refusal("Codex did not prove the requested thread and cwd")
                    thread_id = thread["id"]
                    try:
                        uuid.UUID(thread_id)
                    except ValueError as e:
                        raise Refusal("Codex returned an invalid thread ID") from e
                    if previous and thread_id != previous["id"]:
                        raise Refusal("Codex resumed a different thread")
                    expected_sandbox = {"read-only": "readOnly", "workspace-write": "workspaceWrite", "danger-full-access": "dangerFullAccess"}[args.sandbox]
                    if started.get("approvalPolicy") != args.approval_policy or not isinstance(started.get("sandbox"), dict) or started["sandbox"].get("type") != expected_sandbox:
                        raise Refusal("Codex did not preserve the requested permission posture")
                    session_locks.enter_context(fs.lock(slot / (".session-" + thread_id + ".lock")))
                    thread_path = thread.get("path")
                    app.active_thread = thread_id
                    data["last_session"] = {"id": thread_id, "cwd": cwd, "state": "running"}
                    fs.write_json(slot / "slot.json", data)
                    prompt = "Continue the previous task from the recorded rollout." if previous else args.prompt
                    app.forward = True
                    turn_response = app.call("turn/start", {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]})
                    turn = turn_response.get("turn")
                    if not isinstance(turn, dict) or not isinstance(turn.get("id"), str):
                        raise Refusal("Codex turn ID missing")
                    done = app.wait_turn(thread_id, turn["id"])
                    success = done.get("status") == "completed"
                    error = done.get("error")
                    info = error.get("codexErrorInfo") if isinstance(error, dict) else None
                    if not success and info in ("usageLimitExceeded", "rateLimitExceeded", "unauthorized"):
                        rejection = info
                        if info != "unauthorized":
                            app.forward = False
                            try:
                                _, reset = limits(app.call("account/rateLimits/read", {}))
                            except Refusal:
                                reset = None
                    receipt.update(thread_id=thread_id, disposition="succeeded" if success else rejection or "failed")
                except TimeoutError:
                    receipt["disposition"] = "timeout"
                    return 124
                except Cancelled:
                    receipt["disposition"] = "cancelled"
                    return 130
                finally:
                    try:
                        if app is not None:
                            app.close(require_grace=receipt["disposition"] not in ("timeout", "cancelled"))
                    except Refusal:
                        receipt["disposition"] = "shutdown_unproven"
                        raise
                    finally:
                        if receipt["disposition"] == "running":
                            receipt["disposition"] = "failed"
                        if not success and thread_id and isinstance(data.get("last_session"), dict):
                            data["last_session"]["state"] = "interrupted"
                            fs.write_json(slot / "slot.json", data)
                        receipt["finished_at"] = time.time()
                        fs.write_json(receipt_path, receipt)
                        print(json.dumps({"account_attempt_receipt": str(receipt_path)}), file=sys.stderr)
                if success:
                    data["last_session"]["state"] = "finished"
                    fs.write_json(slot / "slot.json", data)
                    return 0
                if not rejection:
                    return 1
                path_home = Path(previous["home"]) if previous and thread_path == previous["path"] else home
                path = rollout(thread_path, path_home, thread_id, cwd, fs)
                data["last_session"]["state"] = "interrupted"
                if path:
                    data["last_session"]["transcript"] = path
                data["cooldown"] = {"reason": rejection, "reset": reset}
                fs.write_json(slot / "slot.json", data)
                if reset:
                    earliest.append(reset)
                if not path:
                    raise Refusal("Codex continuation blocked; exact rollout not proven")
                previous = {"id": thread_id, "path": path, "home": str(path_home)}
        except Refusal as e:
            if str(e) == "slot is busy":
                slots.append({"slot": name, "state": "busy"})
                continue
            raise
    print(json.dumps({"error": "all_unavailable", "earliest_reset": min(earliest) if earliest else None,
                      "continuation": previous, "slots": slots}), file=sys.stderr)
    return 75
