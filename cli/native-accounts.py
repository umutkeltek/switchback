#!/usr/bin/env python3
"""Isolated native launch slots. Credentials remain owned by the vendor CLI."""
import argparse
import contextlib
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import re
import stat
import subprocess
import selectors
import signal
import time
import uuid
import sys
import tempfile

NAME = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$")
LIMIT = 1024 * 1024

from native_account_process import Refusal, supervise, admission_check, supervised_group

def safe_dir(path, create=False):
    path = Path(os.path.abspath(path))
    # Inspect every component; even a symlink to a seemingly safe location may
    # be swapped after validation. Never follow one as a slot directory.
    cur = Path(path.anchor)
    for part in path.parts[1:]:
        cur = cur / part
        try:
            st = cur.lstat()
        except FileNotFoundError:
            if not create:
                raise Refusal("missing directory: " + str(cur))
            cur.mkdir(mode=0o700)
            st = cur.lstat()
        if stat.S_ISLNK(st.st_mode) or not stat.S_ISDIR(st.st_mode):
            raise Refusal("unsafe directory: " + str(cur))
        # System ancestors can be owned by root; owned state itself cannot.
        if st.st_uid not in (0, os.getuid()):
            raise Refusal("foreign-owned directory: " + str(cur))
        if st.st_uid == os.getuid() and st.st_mode & 0o022:
            raise Refusal("writable-by-others directory: " + str(cur))
    if path.stat().st_uid != os.getuid():
        raise Refusal("directory is not owned by current user: " + str(path))
    return path

def read_json(path):
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o022 or st.st_size > LIMIT:
            raise Refusal("unsafe metadata file")
        with os.fdopen(fd, "r", encoding="utf-8") as f:
            fd = -1
            return json.load(f)
    except (ValueError, UnicodeDecodeError) as e:
        raise Refusal("invalid metadata file") from e
    finally:
        if fd >= 0:
            os.close(fd)

def write_json(path, data):
    if path.exists() or path.is_symlink():
        read_json(path)
    fd, name = tempfile.mkstemp(prefix=".account-", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(data, f, separators=(",", ":"))
            f.write("\n")
            f.flush()
            os.fsync(f.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)

@contextlib.contextmanager
def lock(path):
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o022:
            raise Refusal("unsafe lock file")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as e:
            raise Refusal("slot is busy") from e
        yield
    finally:
        os.close(fd)

def clean_env(provider, home):
    env = os.environ.copy()
    blocked = ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL",
               "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK",
               "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY",
               "CLAUDE_CONFIG_DIR", "CODEX_HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL",
               "HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "NODE_EXTRA_CA_CERTS")
    for key in list(env):
        if key.upper() in blocked:
            env.pop(key, None)
    env["CLAUDE_CONFIG_DIR" if provider == "claude" else "CODEX_HOME"] = str(home)
    if provider == "claude":
        env["CLAUDE_NATIVE_DIRECT"] = "1"
    return env

def inference_env(home):
    env = os.environ.copy()
    for key in ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL",
                "CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY",
                "CLAUDE_CONFIG_DIR", "CODEX_HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL",
                "CLAUDE_NATIVE_DIRECT"):
        env.pop(key, None)
    env["CLAUDE_CONFIG_DIR"] = str(home)
    return env

def identity(home):
    data = read_json(home / ".claude.json")
    obj = data.get("oauthAccount") if isinstance(data, dict) else None
    if not isinstance(obj, dict):
        raise Refusal("Claude identity unavailable; login may be incomplete")
    account, org = obj.get("accountUuid"), obj.get("organizationUuid")
    if not all(isinstance(x, str) and x and len(x) <= 256 for x in (account, org)):
        raise Refusal("Claude identity is incomplete")
    return {"account_uuid": account, "organization_uuid": org}


def quota(home, account, require_complete=False):
    data = read_json(home / ".claude.json")
    cached = data.get("cachedUsageUtilization") if isinstance(data, dict) else None
    if not isinstance(cached, dict) or cached.get("accountUuid") != account:
        return None
    fetched = cached.get("fetchedAtMs")
    usage = cached.get("utilization")
    now = time.time() * 1000
    if isinstance(fetched, bool) or not isinstance(fetched, (int, float)) or not math.isfinite(fetched) or not now - 600000 <= fetched <= now + 30000 or not isinstance(usage, dict):
        return None
    exhausted = False
    windows = 0
    resets = []
    for key in ("five_hour", "seven_day"):
        window = usage.get(key)
        if window is None:
            if require_complete:
                return None
            continue
        if not isinstance(window, dict):
            return None
        windows += 1
        val = window.get("utilization")
        if isinstance(val, bool) or not isinstance(val, (int, float)) or not math.isfinite(val) or not 0 <= val <= 100:
            return None
        if val >= 100:
            exhausted = True
            reset = window.get("resets_at")
            if not isinstance(reset, str):
                return None
            try:
                from datetime import datetime
                parsed = datetime.fromisoformat(reset.replace("Z", "+00:00"))
                if parsed.tzinfo is None:
                    return None
                timestamp = parsed.timestamp()
                if not math.isfinite(timestamp) or timestamp <= time.time():
                    return None
                resets.append(timestamp)
            except ValueError:
                return None
    if not windows:
        return None
    return {"exhausted": exhausted, "reset": max(resets) if resets else None, "fetched_at_ms": fetched}

def refresh_quota(home, timeout=45, propagate_cancel=False):
    env = clean_env("claude", home)
    binary = os.environ.get("SB_NATIVE_CLAUDE", "claude")
    cmd = [binary, "-p", "/usage", "--output-format", "json", "--tools", "",
           "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
           "--setting-sources", "", "--no-session-persistence"]
    try:
        rc, _, _, cancelled, expired, _ = supervised_group(cmd, env, timeout, discard_output=True)
        if cancelled and propagate_cancel:
            raise InterruptedError("quota refresh cancelled")
        return rc == 0 and not cancelled and not expired
    except InterruptedError:
        raise
    except (OSError, Refusal):
        return False

def auth_status(home, timeout=15, propagate_cancel=False):
    env = clean_env("claude", home)
    binary = os.environ.get("SB_NATIVE_CLAUDE", "claude")
    try:
        rc, _, _, cancelled, expired, output = supervised_group([binary, "auth", "status", "--json"], env, timeout)
        if cancelled and propagate_cancel:
            raise InterruptedError("authentication check cancelled")
        if rc or cancelled or expired:
            return None
        status = json.loads(output)
        if (not isinstance(status, dict) or status.get("loggedIn") is not True
                or status.get("authMethod") not in ("claude.ai", "claudeai", "oauth")
                or status.get("configDirectory") != str(home)):
            return None
        return status
    except InterruptedError:
        raise
    except (OSError, ValueError, Refusal):
        return None

def verified_auth(home, binding, timeout=15, propagate_cancel=False):
    try:
        return auth_status(home, timeout, propagate_cancel) is not None and identity(home) == binding
    except InterruptedError:
        raise
    except (OSError, Refusal):
        return False

def transcript(home, session, cwd):
    projects = home / "projects"
    if not projects.exists() or projects.is_symlink():
        return None
    matches = []
    for root, dirs, files in os.walk(projects, followlinks=False):
        dirs[:] = [d for d in dirs if not (Path(root) / d).is_symlink()]
        if session + ".jsonl" not in files:
            continue
        path = Path(root) / (session + ".jsonl")
        if path.is_symlink() or not path.is_file() or path.stat().st_uid != os.getuid():
            continue
        # A filename is insufficient proof: the native record must name both
        # this exact session and working directory.
        try:
            with path.open("rb") as f:
                for _ in range(20):
                    line = f.readline(LIMIT + 1)
                    if not line or len(line) > LIMIT:
                        break
                    event = json.loads(line)
                    if isinstance(event, dict) and event.get("sessionId") == session and event.get("cwd") == cwd:
                        matches.append(path)
                        break
        except (OSError, ValueError):
            continue
    return str(matches[0].absolute()) if len(matches) == 1 else None


def read_owned_bytes(path):
    parent = safe_dir(Path(path).expanduser().absolute().parent.resolve(strict=True))
    target = parent / Path(path).name
    try:
        fd = os.open(target, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError as e:
        raise Refusal("settings source is unavailable or unsafe") from e
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o022 or st.st_size > LIMIT:
            raise Refusal("settings source is unsafe")
        with os.fdopen(fd, "rb") as stream:
            fd = -1
            body = stream.read(LIMIT + 1)
        if len(body) > LIMIT:
            raise Refusal("settings source exceeds limit")
        return body
    finally:
        if fd >= 0:
            os.close(fd)

def seed_instructions(source, home):
    body = read_owned_bytes(source)
    target = home / "CLAUDE.md"
    digest = hashlib.sha256(body).hexdigest()
    if target.exists() or target.is_symlink():
        if read_owned_bytes(target) != body:
            raise Refusal("existing profile instructions differ; not overwriting")
        return digest
    fd, temporary = tempfile.mkstemp(prefix=".instructions-", dir=home)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(body)
            stream.flush()
            os.fsync(stream.fileno())
        os.link(temporary, target, follow_symlinks=False)
    finally:
        os.unlink(temporary)
    return digest

def seed_settings(source, home):
    body = read_owned_bytes(source)
    try:
        settings = json.loads(body)
    except (ValueError, UnicodeDecodeError) as e:
        raise Refusal("settings source is not valid JSON") from e
    if not isinstance(settings, dict) or any(k in settings for k in
        ("apiKeyHelper", "awsAuthRefresh", "awsCredentialExport", "vertexAuthRefresh")):
        raise Refusal("settings source contains credential or provider overrides")
    env = settings.get("env", {})
    if not isinstance(env, dict):
        raise Refusal("settings env must be an object")
    blocked = ("ANTHROPIC_", "OPENAI_", "CLAUDE_CODE_OAUTH", "CLAUDE_CODE_USE_")
    if any(not isinstance(k, str) or k.upper().startswith(blocked) or
           k.upper() in ("CLAUDE_CONFIG_DIR", "CODEX_HOME") for k in env):
        raise Refusal("settings source contains credential or provider overrides")
    target = home / "settings.json"
    digest = hashlib.sha256(body).hexdigest()
    if target.exists() or target.is_symlink():
        if read_owned_bytes(target) != body:
            raise Refusal("existing profile settings differ; not overwriting")
        return digest
    fd, temporary = tempfile.mkstemp(prefix=".settings-", dir=home)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(body)
            stream.flush()
            os.fsync(stream.fileno())
        os.link(temporary, target, follow_symlinks=False)
    finally:
        os.unlink(temporary)
    return digest

def settings_digest(home):
    path = home / "settings.json"
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError as e:
        raise Refusal("settings file unavailable for digest check") from e
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or st.st_mode & 0o022 or st.st_size > LIMIT:
            raise Refusal("unsafe settings file")
        with os.fdopen(fd, "rb") as stream:
            fd = -1
            body = stream.read(LIMIT + 1)
        if len(body) > LIMIT:
            raise Refusal("settings file exceeds limit")
        return hashlib.sha256(body).hexdigest()
    finally:
        if fd >= 0:
            os.close(fd)

def run_pool(args, runtime, root):
    with contextlib.ExitStack() as session_locks:
        return _run_pool(args, runtime, root, session_locks)

def _run_pool(args, runtime, root, session_locks):
    if args.provider != "claude":
        raise Refusal("pool execution is currently supported only for Claude")
    names = args.pool.split(",") if args.pool else []
    if not names or len(set(names)) != len(names) or any(not NAME.fullmatch(n) or n.lower() == "default" for n in names):
        raise Refusal("--pool must name distinct non-default slots")
    if len(names) > 16:
        raise Refusal("pool exceeds 16 slots")
    if args.max_attempts is None:
        args.max_attempts = len(names)
    if not args.prompt or not 1 <= args.timeout <= 2700 or not 1 <= args.max_attempts <= len(names):
        raise Refusal("run requires --prompt, a bounded --timeout, and --max-attempts within the pool size")
    if (not args.permission_mode and not args.dangerously_skip_permissions) or (args.permission_mode and args.dangerously_skip_permissions):
        raise Refusal("run requires exactly one explicit permission posture")
    if args.max_budget_usd is not None and (not math.isfinite(args.max_budget_usd) or args.max_budget_usd <= 0):
        raise Refusal("max budget must be a positive finite amount")
    if args.session_id:
        try:
            if str(uuid.UUID(args.session_id)) != args.session_id.lower():
                raise ValueError()
        except ValueError as e:
            raise Refusal("session ID must be a UUID") from e
    if args.permission_mode and args.permission_mode not in ("default", "plan", "acceptEdits", "dontAsk", "bypassPermissions"):
        raise Refusal("unsupported permission mode")
    check_command = None
    if args.admission_check_command:
        try:
            check_command = json.loads(args.admission_check_command)
        except ValueError as e:
            raise Refusal("admission check command must be a JSON array") from e
        if (not isinstance(check_command, list) or not 1 <= len(check_command) <= 64
                or not all(isinstance(x, str) and x and len(x) <= 4096 for x in check_command)
                or not os.path.isabs(check_command[0])):
            raise Refusal("admission check command requires an absolute executable and string arguments")
    if args.expected_settings_sha256 and not re.fullmatch(r"[0-9a-f]{64}", args.expected_settings_sha256):
        raise Refusal("expected settings digest must be lowercase SHA-256")
    cwd = os.getcwd()
    deadline = time.monotonic() + args.timeout
    session = args.session_id or str(uuid.uuid4())
    resume = None
    resume_home = None
    attempted = 0
    earliest = []
    sb = str(Path(__file__).absolute().parent / "sb")
    for name in names:
        if attempted >= args.max_attempts:
            break
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return 124
        slot = safe_dir(root / name)
        home = safe_dir(Path(runtime) / "config" / "claude" / name)
        try:
            with lock(slot / ".lock"):
                data = read_json(slot / "slot.json")
                if not isinstance(data, dict) or data.get("slot") != name or data.get("provider") != "claude" or not data.get("enabled") or not isinstance(data.get("identity"), dict):
                    continue
                cooldown = data.get("cooldown")
                if cooldown:
                    if not isinstance(cooldown, dict) or cooldown.get("reason") not in ("rate_limit", "authentication_failed"):
                        continue
                    reason = cooldown["reason"]
                    reset = cooldown.get("reset")
                    if (reason == "rate_limit" and isinstance(reset, (int, float))
                            and not isinstance(reset, bool) and math.isfinite(reset) and reset > time.time()):
                        earliest.append(reset)
                        continue
                    # Refresh is native control traffic, but still requires the
                    # caller's fresh admission before it can contact the vendor.
                    admission_check(check_command, min(10, max(0.01, deadline - time.monotonic())))
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        return 124
                    refresh_started_ms = int(time.time() * 1000)
                    try:
                        refreshed = refresh_quota(home, min(45, remaining), propagate_cancel=True)
                    except InterruptedError:
                        return 130
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        return 124
                    current_quota = quota(home, data["identity"].get("account_uuid"), require_complete=True) if refreshed else None
                    if not current_quota or current_quota["fetched_at_ms"] < refresh_started_ms:
                        continue
                    try:
                        authenticated = verified_auth(home, data["identity"], min(15, remaining), propagate_cancel=True)
                    except InterruptedError:
                        return 130
                    if deadline - time.monotonic() <= 0:
                        return 124
                    if not authenticated:
                        continue
                    data["last_good_quota"] = current_quota
                    if current_quota["exhausted"]:
                        data["cooldown"] = {"reason": "rate_limit", "reset": current_quota["reset"]}
                        write_json(slot / "slot.json", data)
                        if current_quota["reset"]:
                            earliest.append(current_quota["reset"])
                        continue
                    data.pop("cooldown", None)
                    write_json(slot / "slot.json", data)
                else:
                    current_quota = quota(home, data["identity"].get("account_uuid"))
                    if current_quota and current_quota["exhausted"]:
                        if current_quota["reset"]:
                            earliest.append(current_quota["reset"])
                        continue
                    try:
                        authenticated = verified_auth(home, data["identity"], min(15, remaining), propagate_cancel=True)
                    except InterruptedError:
                        return 130
                    if deadline - time.monotonic() <= 0:
                        return 124
                    if not authenticated:
                        continue
                digest = None
                if args.expected_settings_sha256:
                    digest = settings_digest(home)
                    if digest != args.expected_settings_sha256:
                        raise Refusal("candidate settings digest differs from expected digest")
                session_locks.enter_context(lock(slot / (".session-" + session + ".lock")))
                if resume and transcript(resume_home, session, cwd) != resume:
                    raise Refusal("continuation transcript changed or is unsafe")
                attempted += 1
                flags = ["--print", "--verbose", "--output-format", "stream-json"]
                if args.model:
                    flags += ["--model", args.model]
                if args.permission_mode:
                    flags += ["--permission-mode", args.permission_mode]
                if args.dangerously_skip_permissions:
                    flags += ["--dangerously-skip-permissions"]
                if args.max_budget_usd is not None:
                    flags += ["--max-budget-usd", str(args.max_budget_usd)]
                if resume:
                    flags += ["--resume", resume, "Continue the previous task from the recorded transcript."]
                else:
                    flags += ["--session-id", session, args.prompt]
                env = inference_env(home)
                env["SB_CLAUDE_PERMISSION_MODE"] = ""
                cmd = [sb, "claude", "--mode", "remote", "--account", name] + flags
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return 124
                receipts = safe_dir(slot / "receipts", create=True)
                receipt_path = receipts / (str(uuid.uuid4()) + ".json")
                receipt = {"schema": "switchback/native-account-attempt@1", "slot": name,
                           "provider": "claude", "session_id": session, "started_at": time.time(),
                           "settings_sha256": digest, "disposition": "running"}
                write_json(receipt_path, receipt)
                if digest and settings_digest(home) != digest:
                    receipt.update(disposition="settings_changed", finished_at=time.time())
                    write_json(receipt_path, receipt)
                    raise Refusal("candidate settings changed before launch")
                try:
                    admission_check(check_command, min(10, max(0.01, deadline - time.monotonic())))
                except Refusal:
                    receipt.update(disposition="admission_refused", finished_at=time.time())
                    write_json(receipt_path, receipt)
                    print(json.dumps({"account_attempt_receipt": str(receipt_path)}), file=sys.stderr)
                    raise
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    receipt.update(disposition="timeout", finished_at=time.time())
                    write_json(receipt_path, receipt)
                    return 124
                data["last_session"] = {"id": session, "cwd": cwd, "state": "running"}
                write_json(slot / "slot.json", data)
                try:
                    rc, rejection, success, cancelled, timed_out = supervise(cmd, env, remaining, session)
                except BaseException:
                    data["last_session"]["state"] = "interrupted"
                    write_json(slot / "slot.json", data)
                    receipt.update(disposition="supervision_error", finished_at=time.time())
                    write_json(receipt_path, receipt)
                    raise
                receipt.update(disposition=("cancelled" if cancelled else "timeout" if timed_out
                                            else "succeeded" if success and rc == 0 and not rejection
                                            else rejection[0] if rejection else "failed"),
                               finished_at=time.time())
                write_json(receipt_path, receipt)
                print(json.dumps({"account_attempt_receipt": str(receipt_path)}), file=sys.stderr)
                path = transcript(home, session, cwd)
                transcript_home = home
                if not path and resume and transcript(resume_home, session, cwd) == resume:
                    path = resume
                    transcript_home = resume_home
                data["last_session"]["state"] = "finished" if success and rc == 0 else "interrupted"
                if path:
                    data["last_session"]["transcript"] = path
                if rejection:
                    kind, reset = rejection
                    data["cooldown"] = {"reason": kind, "reset": reset}
                    if reset:
                        earliest.append(reset)
                write_json(slot / "slot.json", data)
                if cancelled or timed_out:
                    print("account: interrupted; session metadata retained", file=sys.stderr)
                    return 130 if cancelled else 124
                if success and rc == 0 and not rejection:
                    return 0
                if not rejection:
                    return rc or 1
                if args.max_budget_usd is not None:
                    print("account: continuation blocked; remaining budget is not proven", file=sys.stderr)
                    return 75
                if not path:
                    print("account: continuation blocked; exact transcript not proven", file=sys.stderr)
                    return 75
                resume_home = transcript_home
                resume = path
        except BlockingIOError:
            continue
        except Refusal as e:
            if str(e) == "slot is busy":
                continue
            raise
    print(json.dumps({"error": "all_unavailable", "earliest_reset": min(earliest) if earliest else None,
                      "resumable_transcript": resume}), file=sys.stderr)
    return 75

class QuietParser(argparse.ArgumentParser):
    def error(self, message):
        raise Refusal("invalid or unsupported arguments")

def main():
    argv = sys.argv[1:]
    native = None
    if "--" in argv:
        index = argv.index("--")
        native, argv = argv[index + 1:], argv[:index]
    parser = QuietParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("action", choices=("providers", "setup", "login", "status", "list", "enable", "disable", "run"))
    parser.add_argument("--provider")
    parser.add_argument("--slot")
    parser.add_argument("--settings-from")
    parser.add_argument("--instructions-from")
    parser.add_argument("--preset")
    parser.add_argument("--api-key-env")
    parser.add_argument("--base-url")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--device-auth", action="store_true")
    parser.add_argument("--refresh", action="store_true")
    parser.add_argument("--quota-refresh", action="store_true")
    parser.add_argument("--pool")
    parser.add_argument("--prompt")
    parser.add_argument("--timeout", type=int, default=300)
    parser.add_argument("--max-attempts", type=int)
    parser.add_argument("--model")
    parser.add_argument("--permission-mode")
    parser.add_argument("--max-budget-usd", type=float)
    parser.add_argument("--session-id")
    parser.add_argument("--expected-settings-sha256")
    parser.add_argument("--admission-check-command")
    parser.add_argument("--approval-policy")
    parser.add_argument("--sandbox")
    parser.add_argument("--dangerously-skip-permissions", action="store_true")
    args = parser.parse_args(argv)
    if native is not None:
        if args.action != "run" or args.prompt or args.model or args.permission_mode or args.session_id or args.dangerously_skip_permissions or args.max_budget_usd:
            raise Refusal("native arguments apply only to run and cannot be combined with run flags")
        vendor = QuietParser(prog="sb account run --", allow_abbrev=False)
        vendor.add_argument("--print", required=True, action="store_true")
        vendor.add_argument("--permission-mode")
        vendor.add_argument("--dangerously-skip-permissions", action="store_true")
        vendor.add_argument("--session-id")
        vendor.add_argument("--model")
        vendor.add_argument("--max-budget-usd", type=float)
        vendor.add_argument("prompt")
        passed = vendor.parse_args(native)
        args.prompt = passed.prompt
        args.permission_mode = passed.permission_mode
        args.dangerously_skip_permissions = passed.dangerously_skip_permissions
        args.session_id = passed.session_id
        args.model = passed.model
        args.max_budget_usd = passed.max_budget_usd
    sb = str(Path(__file__).absolute().parent / "sb")
    if args.action == "providers":
        def query(argv):
            p = subprocess.run([sb] + argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=15)
            if p.returncode or len(p.stdout) > LIMIT:
                raise Refusal("provider inventory unavailable")
            try:
                return json.loads(p.stdout)
            except ValueError as e:
                raise Refusal("provider inventory malformed") from e
        result = {"configured": query(["config", "providers"]), "presets": query(["provider", "presets", "--json"])}
        print(json.dumps(result) if args.json else json.dumps(result, indent=2))
        return 0
    if not args.provider:
        raise Refusal("--provider is required")
    if args.provider not in ("claude", "codex"):
        if args.action == "setup":
            if not NAME.fullmatch(args.provider) or not args.preset or not NAME.fullmatch(args.preset):
                raise Refusal("setup requires a safe provider ID and --preset")
            if not args.api_key_env or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", args.api_key_env):
                raise Refusal("setup requires an API key environment variable name in --api-key-env")
            command = [sb, "provider", "add", args.preset, "--id", args.provider, "--api-key-env", args.api_key_env]
            if args.base_url:
                command += ["--base-url", args.base_url]
            if args.model:
                command += ["--model", args.model]
            return subprocess.call(command)
        if args.action not in ("list", "status"):
            raise Refusal("provider login is unsupported; configure a key with 'sb provider add <preset> --api-key-env NAME' or 'sb vault'")
        result = subprocess.run([sb, "config", "providers"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=15)
        if result.returncode or len(result.stdout) > LIMIT:
            raise Refusal("configured provider inventory unavailable")
        try:
            matches = [x for x in json.loads(result.stdout)["providers"] if x.get("id") == args.provider]
        except (ValueError, KeyError, TypeError) as e:
            raise Refusal("configured provider inventory malformed") from e
        if not matches:
            raise Refusal("provider is not configured")
        out = {"provider": matches[0], "capabilities": {"native_login": False, "gateway_rotation": True, "native_quota": False}}
        print(json.dumps(out) if args.json else json.dumps(out, indent=2))
        return 0
    if args.action not in ("list", "run") and (not args.slot or not NAME.fullmatch(args.slot) or args.slot.lower() == "default"):
        raise Refusal("a safe non-default --slot is required")
    if (args.settings_from or args.instructions_from) and (args.action != "setup" or args.provider != "claude"):
        raise Refusal("settings and instructions seeding is only available for Claude setup")
    if args.action == "login" and not 1 <= args.timeout <= 2700:
        raise Refusal("login timeout must be between 1 and 2700 seconds")
    if args.device_auth and (args.action != "login" or args.provider != "codex"):
        raise Refusal("--device-auth is only available for Codex login")
    if args.quota_refresh and (args.action != "status" or args.provider not in ("claude", "codex")):
        raise Refusal("--quota-refresh is only available for native account status")
    runtime = os.environ.get("SWITCHBACK_RUNTIME_ROOT", "")
    if not runtime or not os.path.isabs(runtime):
        raise Refusal("an absolute owned runtime is required")
    runtime = os.path.realpath(runtime)
    root = safe_dir(Path(runtime) / "config" / "native-account-slots", create=args.action == "setup")
    provider_dir = safe_dir(root / args.provider, create=args.action == "setup")
    if args.action == "run":
        if args.provider == "codex":
            from types import SimpleNamespace
            from native_codex_account import run as run_codex_pool
            fs = SimpleNamespace(safe_dir=safe_dir, lock=lock, read_json=read_json,
                                 write_json=write_json, clean_env=clean_env,
                                 valid_name=lambda value: NAME.fullmatch(value) is not None)
            return run_codex_pool(args, runtime, provider_dir, fs)
        return run_pool(args, runtime, provider_dir)
    if args.action == "list":
        rows = []
        for child in sorted(provider_dir.iterdir()):
            if not NAME.fullmatch(child.name):
                continue
            safe_dir(child)
            data = read_json(child / "slot.json")
            if data:
                rows.append(data)
        result = {"provider": args.provider, "slots": rows, "quota": "unknown",
                  "capabilities": {"setup": True, "login": True, "status_refresh": True,
                                   "pool_run": True, "quota": True, "continuation": "experimental" if args.provider == "codex" else True}}
        print(json.dumps(result) if args.json else json.dumps(result, indent=2))
        return 0
    slot = safe_dir(provider_dir / args.slot, create=args.action == "setup")
    profile_root = Path(runtime) / "config" / args.provider
    safe_dir(profile_root, create=args.action == "setup")
    home = profile_root / args.slot
    if args.action == "setup" and home.exists():
        safe_dir(home)
        # Explicit skeletal profiles may contain settings, never an existing auth store.
        for directory, dirs, files in os.walk(home, followlinks=False):
            for name in dirs + files:
                path = Path(directory) / name
                if path.is_symlink() or path.lstat().st_uid != os.getuid():
                    raise Refusal("unsafe existing profile")
                if name in (".claude.json", ".credentials.json", "auth.json"):
                    # An existing managed slot is idempotent; any other auth is not adopted.
                    if read_json(slot / "slot.json") is None:
                        raise Refusal("existing native credentials cannot be adopted")
    home = safe_dir(home, create=args.action == "setup")
    path = slot / "slot.json"
    with lock(slot / ".lock"):
        data = read_json(path)
        if args.action == "setup":
            settings_sha = seed_settings(args.settings_from, home) if args.settings_from else None
            instructions_sha = seed_instructions(args.instructions_from, home) if args.instructions_from else None
            if data is not None:
                if data.get("slot") != args.slot or data.get("provider") != args.provider:
                    raise Refusal("slot metadata mismatch")
                print(json.dumps({"slot": args.slot, "provider": args.provider, "existing": True, "settings_sha256": settings_sha, "instructions_sha256": instructions_sha}))
                return 0
            data = {"schema": "switchback/native-launch-slot@1", "provider": args.provider,
                    "slot": args.slot, "enabled": True, "identity": None}
            write_json(path, data)
            result = {"slot": args.slot, "provider": args.provider, "settings_sha256": settings_sha, "instructions_sha256": instructions_sha, "next": "sb account login --provider %s --slot %s" % (args.provider, args.slot)}
        else:
            if not isinstance(data, dict) or data.get("slot") != args.slot or data.get("provider") != args.provider:
                raise Refusal("slot metadata missing or invalid")
            if args.action in ("enable", "disable"):
                data["enabled"] = args.action == "enable"
                write_json(path, data)
                result = data
            elif args.action == "status" and not args.refresh and not args.quota_refresh:
                result = {"slot": args.slot, "provider": args.provider, "authenticated": "unknown", "quota": "unknown", "enabled": data["enabled"]}
            else:
                env = clean_env(args.provider, home)
                binary = os.environ.get("SB_NATIVE_CLAUDE" if args.provider == "claude" else "SB_NATIVE_CODEX", args.provider)
                if args.action == "login":
                    cmd = [binary, "auth", "login", "--claudeai"] if args.provider == "claude" else [binary, "login"] + (["--device-auth"] if args.device_auth else [])
                    try:
                        rc = subprocess.run(cmd, env=env, timeout=args.timeout).returncode
                    except subprocess.TimeoutExpired:
                        raise Refusal("native login timed out; retry login when ready")
                    if rc:
                        return rc
                if args.provider == "claude":
                    if auth_status(home) is None:
                        raise Refusal("native status does not prove this isolated Claude login")
                    observed = identity(home)
                    if data.get("identity") and data["identity"] != observed:
                        raise Refusal("login identity differs from existing slot binding")
                    # Serialize duplicate checks with a provider-wide lock.
                    with lock(provider_dir / ".identity.lock"):
                        for other in provider_dir.iterdir():
                            if other == slot or not other.is_dir() or other.is_symlink():
                                continue
                            safe_dir(other)
                            row = read_json(other / "slot.json")
                            if row and row.get("identity") == observed:
                                raise Refusal("identity is already bound to another slot")
                        data["identity"] = observed
                        if isinstance(data.get("cooldown"), dict) and data["cooldown"].get("reason") == "authentication_failed":
                            data.pop("cooldown")
                        write_json(path, data)
                    result = {"slot": args.slot, "provider": args.provider, "authenticated": True, "quota": "unknown"}
                    if args.quota_refresh:
                        ok = refresh_quota(home)
                        current = quota(home, observed["account_uuid"]) if ok else None
                        if current is None:
                            result["quota_error"] = "refresh_failed_or_invalid_cache"
                            if data.get("last_good_quota"):
                                result["previous_quota"] = data["last_good_quota"]
                                result["previous_quota_stale"] = True
                        else:
                            data["last_good_quota"] = current
                            if not current["exhausted"]:
                                data.pop("cooldown", None)
                            elif isinstance(data.get("cooldown"), dict) and data["cooldown"].get("reason") == "rate_limit":
                                data["cooldown"]["reset"] = current["reset"]
                            result["quota"] = current
                            write_json(path, data)
                else:
                    from native_codex_account import inspect_account
                    try:
                        result = inspect_account(home, env, binary)
                    except (Refusal, OSError, TimeoutError):
                        result = {"authenticated": "unknown", "state": "unknown", "quota": "unknown"}
                    observed = result.pop("_identity", None)
                    if observed:
                        binding = observed
                        if data.get("identity") and data["identity"] != binding:
                            raise Refusal("login identity differs from existing slot binding")
                        with lock(provider_dir / ".identity.lock"):
                            for other in provider_dir.iterdir():
                                if other == slot or not other.is_dir() or other.is_symlink():
                                    continue
                                safe_dir(other)
                                row = read_json(other / "slot.json")
                                if row and row.get("identity") == binding:
                                    raise Refusal("identity is already bound to another slot")
                            data["identity"] = binding
                            if isinstance(data.get("cooldown"), dict) and data["cooldown"].get("reason") == "unauthorized":
                                data.pop("cooldown")
                            write_json(path, data)
                    result.update(slot=args.slot, provider=args.provider)
                    if result.get("state") == "ready" and isinstance(result.get("quota"), dict):
                        q = result["quota"]
                        if not q["exhausted"]:
                            data.pop("cooldown", None)
                        elif isinstance(data.get("cooldown"), dict):
                            data["cooldown"]["reset"] = q["reset"]
                        fs_data = data
                        write_json(path, fs_data)
        print(json.dumps(result) if args.json else json.dumps(result, indent=2))
        return 0

if __name__ == "__main__":
    try:
        sys.exit(main())
    except (Refusal, OSError, subprocess.TimeoutExpired) as e:
        print("account: " + str(e), file=sys.stderr)
        sys.exit(1)
