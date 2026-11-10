"""Bounded process-group supervision for isolated native account launches."""
import json
import os
import selectors
import signal
import subprocess
import sys
import time

LIMIT = 1024 * 1024

class Refusal(Exception):
    pass

# The live group leader prevents PID/PGID reuse while we terminate descendants.
# It reports the native exit over a private pipe, closes its copies of the
# output pipes, and remains alive until the parent signals the entire group.
ANCHOR = r"""
import os, signal, subprocess, sys
fd = int(sys.argv[1])
os.set_inheritable(fd, False)
try:
    child = subprocess.Popen(sys.argv[2:], stdin=subprocess.DEVNULL)
    null = os.open(os.devnull, os.O_WRONLY)
    os.dup2(null, 1)
    os.dup2(null, 2)
    os.close(null)
    code = child.wait()
except BaseException:
    code = 127
signal.signal(signal.SIGTERM, lambda *_: None)
os.write(fd, (str(code) + "\n").encode())
os.close(fd)
while True:
    signal.pause()
"""

def supervised_group(cmd, env, timeout, session=None, forward=False, discard_output=False):
    read_fd, write_fd = os.pipe()
    try:
        anchor = subprocess.Popen([sys.executable, "-c", ANCHOR, str(write_fd)] + cmd,
                                  env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, start_new_session=True,
                                  pass_fds=(write_fd,))
    except BaseException:
        os.close(read_fd)
        os.close(write_fd)
        raise
    os.close(write_fd)
    old = {}
    cancelled = []
    def stop(signum, frame):
        cancelled.append(signum)
    for sig in (signal.SIGTERM, signal.SIGINT):
        old[sig] = signal.signal(sig, stop)
    sel = selectors.DefaultSelector()
    sel.register(anchor.stdout, selectors.EVENT_READ, "stdout")
    sel.register(anchor.stderr, selectors.EVENT_READ, "stderr")
    sel.register(read_fd, selectors.EVENT_READ, "status")
    pending = b""
    callback_output = b""
    status = b""
    typed = None
    success = False
    result_state = None
    malformed = False
    deadline = time.monotonic() + timeout
    expired = False
    signalled = False
    try:
        while sel.get_map():
            if cancelled or time.monotonic() >= deadline:
                expired = True
                break
            for key, _ in sel.select(min(0.2, max(0, deadline - time.monotonic()))):
                chunk = os.read(key.fd, 16384)
                if not chunk:
                    sel.unregister(key.fileobj)
                    continue
                if key.data == "status":
                    status += chunk
                    if len(status) > 32:
                        malformed = True
                    continue
                if forward:
                    output = sys.stdout.buffer if key.data == "stdout" else sys.stderr.buffer
                    output.write(chunk)
                    output.flush()
                if key.data != "stdout":
                    continue
                if not forward:
                    if discard_output:
                        continue
                    callback_output += chunk
                    if len(callback_output) > 8192:
                        malformed = True
                    continue
                pending += chunk
                if len(pending) > LIMIT and b"\n" not in pending:
                    malformed = True
                    break
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    if len(line) > LIMIT:
                        malformed = True
                        break
                    try:
                        obj = json.loads(line)
                    except (ValueError, UnicodeDecodeError):
                        malformed = True
                        continue
                    if not isinstance(obj, dict):
                        malformed = True
                        continue
                    emitted_session = obj.get("session_id")
                    if emitted_session is not None and emitted_session != session:
                        malformed = True
                    if obj.get("type") == "assistant" and obj.get("error") in ("authentication_failed", "rate_limit"):
                        typed = (obj["error"], None)
                    if obj.get("type") == "rate_limit_event":
                        info = obj.get("rate_limit_info")
                        if isinstance(info, dict) and info.get("status") == "rejected":
                            reset = info.get("resetsAt")
                            typed = ("rate_limit", reset if isinstance(reset, (int, float)) and not isinstance(reset, bool) and reset > time.time() else None)
                    if obj.get("type") == "result":
                        result_success = obj.get("subtype") == "success" and obj.get("is_error") is not True
                        if result_state is not None and result_state != result_success:
                            malformed = True
                        result_state = result_success
                        success = result_success
            if malformed:
                break
        # The anchor PID remains reserved until wait(); signal its group once.
        # If the leader has already exited, EPERM/ESRCH is a refusal, never retry.
        signalled = True
        try:
            os.killpg(anchor.pid, signal.SIGKILL)
        except OSError as e:
            raise Refusal("process group cannot be terminated safely") from e
        anchor.wait(timeout=2)
        try:
            rc = int(status.strip())
        except ValueError:
            rc = 1
            malformed = True
        if forward and pending.strip():
            malformed = True
        if malformed or success:
            # A successful terminal task is never replayed because of transient
            # errors in the same stream, even if cleanup later exits nonzero.
            typed = None
        return (1 if malformed else rc), typed, success and not malformed, bool(cancelled), expired, callback_output
    finally:
        sel.close()
        os.close(read_fd)
        if anchor.returncode is None:
            if not signalled:
                try:
                    os.killpg(anchor.pid, signal.SIGKILL)
                except OSError:
                    pass
            anchor.wait(timeout=2)
        for sig, handler in old.items():
            signal.signal(sig, handler)

def supervise(cmd, env, timeout, session):
    rc, rejection, success, cancelled, timed_out, _ = supervised_group(
        cmd, env, timeout, session=session, forward=True)
    return rc, rejection, success, cancelled, timed_out

def admission_check(command, timeout):
    if command is None:
        return
    try:
        rc, _, _, cancelled, expired, output = supervised_group(command, os.environ.copy(), timeout)
        answer = json.loads(output)
    except (OSError, ValueError) as e:
        raise Refusal("admission check did not return a valid decision") from e
    if cancelled or expired or rc != 0 or not isinstance(answer, dict) or answer.get("allow") is not True:
        raise Refusal("admission check denied or timed out")
