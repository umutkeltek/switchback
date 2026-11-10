#!/usr/bin/env python3
import json
import os
import hashlib
import fcntl
import sys
import socket
import signal
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

SB = Path(__file__).resolve().parents[1] / "sb"

FAKE = r"""#!/usr/bin/env python3
import json, os, pathlib, sys, time, subprocess
from datetime import datetime, timedelta, timezone
home = pathlib.Path(os.environ.get("CLAUDE_CONFIG_DIR", os.environ.get("CODEX_HOME", "")))
args = sys.argv[1:]
if args[:2] == ["auth", "login"]:
    (home / ".claude.json").write_text(json.dumps({"oauthAccount": {"accountUuid": os.environ.get("FAKE_ACCOUNT", home.name), "organizationUuid": "org"}}))
    sys.exit(0)
if args[:2] == ["auth", "status"]:
    print(json.dumps({"loggedIn": os.environ.get("FAKE_AUTH_REVOKED") != "1", "authMethod": "claude.ai", "configDirectory": str(home)}))
    sys.exit(0)
if args[:2] == ["login", "status"]:
    sys.exit(0)
if args[:1] == ["login"]:
    sys.exit(0)
if args[:2] == ["-p", "/usage"]:
    if os.environ.get("FAKE_QUOTA_LOG"):
        with open(os.environ["FAKE_QUOTA_LOG"],"a") as log:
            log.write("refresh\n")
    mode = os.environ.get("FAKE_QUOTA")
    if mode == "delay":
        time.sleep(3)
    if mode == "unchanged":
        sys.exit(0)
    if mode not in ("1", "ready", "partial"):
        sys.exit(1)
    path = home / ".claude.json"
    value = json.loads(path.read_text())
    used = 100 if mode == "1" else 10
    value["cachedUsageUtilization"] = {"accountUuid": value["oauthAccount"]["accountUuid"],
        "fetchedAtMs": int(time.time() * 1000),
        "utilization": {"five_hour": {"utilization": used, "resets_at": (datetime.now(timezone.utc) + timedelta(hours=5)).isoformat()},
                        "seven_day": {"utilization": used, "resets_at": (datetime.now(timezone.utc) + timedelta(days=7)).isoformat()}}}
    if mode == "partial":
        value["cachedUsageUtilization"]["utilization"].pop("seven_day")
    path.write_text(json.dumps(value))
    sys.exit(0)
if "--session-id" in args:
    session = args[args.index("--session-id") + 1]
else:
    session = pathlib.Path(args[args.index("--resume") + 1]).stem
if os.environ.get("FAKE_HANG") == "1":
    if os.environ.get("FAKE_CHILD") == "1":
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        pathlib.Path(os.environ["FAKE_LOG"] + ".pid").write_text(str(child.pid))
    if os.environ.get("FAKE_CLOSED_PIPES") == "1":
        os.close(1)
        os.close(2)
    time.sleep(60)
if os.environ.get("FAKE_SESSION_MISMATCH") == "1":
    print(json.dumps({"type":"system", "subtype":"init", "session_id":"wrong"}), flush=True)
    sys.exit(1)
with open(os.environ["FAKE_LOG"], "a") as out:
    out.write(json.dumps({"slot":home.name,"args":args,"api_key":os.environ.get("ANTHROPIC_API_KEY")}) + "\n")
project = home / "projects" / "project"
project.mkdir(parents=True, exist_ok=True)
if os.environ.get("FAKE_NO_TRANSCRIPT") != "1":
    target = pathlib.Path(args[args.index("--resume") + 1]) if "--resume" in args and os.environ.get("FAKE_KEEP_RESUME_PATH") == "1" else project / (session + ".jsonl")
    target.write_text(json.dumps({"sessionId":session,"cwd":os.getcwd()}) + "\n")
if home.name in os.environ.get("FAKE_REJECT_SLOTS", "one").split(","):
    if os.environ.get("FAKE_GENERIC") == "1":
        print(json.dumps({"type":"result","subtype":"error_during_execution","is_error":True}))
        sys.exit(1)
    print(json.dumps({"type":"assistant","error":"rate_limit"}))
    sys.exit(1)
print(json.dumps({"type":"result","subtype":"success","is_error":False}))
"""

CODEX_FAKE = r"""#!/usr/bin/env python3
import json, os, pathlib, sys, subprocess, time
home=pathlib.Path(os.environ["CODEX_HOME"])
args=sys.argv[1:]
if args[:1] == ["login"]:
    sys.exit(0)
if "app-server" not in args:
    sys.exit(2)
thread="019aabbc-4890-43cb-a0b9-02a25f2b6e73"
turn="019aabbc-4890-43cb-a0b9-02a25f2b6e74"
cwd=os.getcwd()
rollout=home/"sessions"/("rollout-"+thread+".jsonl")
for line in sys.stdin:
    req=json.loads(line)
    method=req.get("method")
    ident=req.get("id")
    if ident is None:
        continue
    params=req.get("params",{})
    if method=="initialize":
        result={}
    elif method=="account/read":
        if os.environ.get("FAKE_CODEX_AUTH_HANG") == "1":
            pathlib.Path(os.environ["FAKE_LOG"]+".probe").write_text("started")
            time.sleep(3)
        if os.environ.get("FAKE_CODEX_REVOKED") == "1":
            print(json.dumps({"id":ident,"result":{"requiresOpenaiAuth":True,"account":None}}),flush=True)
            continue
        if os.environ.get("FAKE_CODEX_AUTH_FAIL") == "1":
            print(json.dumps({"id":ident,"error":{"code":1,"message":"failure"}}),flush=True)
            continue
        result={"requiresOpenaiAuth":True,"account":{"type":"chatgpt","email":None,"planType":"pro"},
                "workspaceRouting":{"chatgptAccountId":os.environ.get("FAKE_CODEX_ID",home.name)}}
    elif method=="account/rateLimits/read":
        if os.environ.get("FAKE_QUOTA_LOG"):
            with open(os.environ["FAKE_QUOTA_LOG"],"a") as log:
                log.write("refresh\n")
        if os.environ.get("FAKE_CODEX_QUOTA_FAIL") == "1":
            print(json.dumps({"id":ident,"error":{"code":1,"message":"failure"}}),flush=True)
            continue
        window = {"usedPercent":100,"resetsAt":int(time.time())+120} if os.environ.get("FAKE_CODEX_EXHAUSTED") == "1" else {"usedPercent":0}
        result={"rateLimits":{"primary":window}}
        if os.environ.get("FAKE_CODEX_READY") == "1":
            result["ordinaryUsageAllowed"] = True
    elif method in ("thread/start","thread/resume"):
        if method=="thread/resume":
            rollout=pathlib.Path(params["path"])
        if method=="thread/start":
            rollout.parent.mkdir(parents=True,exist_ok=True)
            rollout.write_text(json.dumps({"type":"session_meta","payload":{"id":thread,"cwd":cwd}})+"\n")
        with open(os.environ["FAKE_LOG"],"a") as f:
            f.write(json.dumps({"slot":home.name,"method":method,"params":params,"args":args})+"\n")
        returned_id = "019aabbc-4890-43cb-a0b9-02a25f2b6e75" if method=="thread/resume" and os.environ.get("FAKE_WRONG_THREAD")=="1" else thread
        returned_cwd = "/not-the-cwd" if method=="thread/resume" and os.environ.get("FAKE_WRONG_CWD")=="1" else cwd
        returned_path = "/not-an-owned-rollout" if os.environ.get("FAKE_WRONG_PATH")=="1" else str(rollout)
        result={"cwd":returned_cwd,"thread":{"id":returned_id,"path":returned_path},"approvalPolicy":params["approvalPolicy"],
                "sandbox":{"type":"readOnly"}}
    elif method=="turn/start":
        if os.environ.get("FAKE_CODEX_HANG") == "1":
            child=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"])
            pathlib.Path(os.environ["FAKE_LOG"]+".pid").write_text(str(child.pid))
            time.sleep(60)
        with open(os.environ["FAKE_LOG"],"a") as f:
            f.write(json.dumps({"slot":home.name,"method":method,"params":params})+"\n")
        result={"turn":{"id":turn}}
        print(json.dumps({"id":ident,"result":result}),flush=True)
        status="failed" if home.name in os.environ.get("FAKE_REJECT_SLOTS","one").split(",") else "completed"
        error={"message":"failure","codexErrorInfo":os.environ.get("FAKE_CODEX_ERROR","usageLimitExceeded")} if status=="failed" else None
        print(json.dumps({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":status,"error":error}}}),flush=True)
        continue
    else:
        print(json.dumps({"id":ident,"error":{"code":-32601,"message":"unsupported"}}),flush=True)
        continue
    print(json.dumps({"id":ident,"result":result}),flush=True)
"""

class NativeAccounts(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name).resolve()
        self.home = root / "home"
        self.home.mkdir()
        self.runtime = root / "runtime"
        self.runtime.mkdir()
        (self.runtime / "manifest.json").write_text("{}")
        self.fake = root / "claude"
        self.fake.write_text(FAKE)
        self.fake.chmod(0o755)
        self.env = os.environ.copy()
        self.env.update(HOME=str(self.home), SWITCHBACK_RUNTIME_ROOT=str(self.runtime),
                        SB_NATIVE_CLAUDE=str(self.fake), SB_NATIVE_CODEX=str(self.fake),
                        FAKE_LOG=str(root / "calls"), ANTHROPIC_API_KEY="must-clear")
        self.env.pop("SWITCHBACK_ROOT", None)
    def tearDown(self):
        self.tmp.cleanup()
    def call(self, *args, env=None):
        return subprocess.run([str(SB), "account", *args], env=env or self.env,
                              cwd=self.home, text=True, capture_output=True, timeout=20)
    def setup_slot(self, name):
        p = self.call("setup", "--provider", "claude", "--slot", name)
        self.assertEqual(p.returncode, 0, p.stderr)
        p = self.call("login", "--provider", "claude", "--slot", name)
        self.assertEqual(p.returncode, 0, p.stderr)
    def _four_claude_accounts(self, append_original):
        names = ("one", "two", "three", "four")
        for name in names:
            self.setup_slot(name)
        env = dict(self.env, FAKE_REJECT_SLOTS="one,two,three",
                   FAKE_KEEP_RESUME_PATH="1" if append_original else "0")
        result = self.call("run", "--provider", "claude", "--pool", ",".join(names),
                           "--", "--print", "--permission-mode", "plan", "original task", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = [json.loads(line) for line in Path(self.env["FAKE_LOG"]).read_text().splitlines()]
        self.assertEqual([row["slot"] for row in rows], list(names))
        for index, row in enumerate(rows[1:], start=1):
            args = row["args"]
            path = Path(args[args.index("--resume") + 1])
            expected = "one" if append_original else names[index - 1]
            self.assertEqual(path.parents[2].name, expected)
            self.assertNotIn("original task", args)

    def test_four_claude_accounts_with_profile_local_transcripts(self):
        self._four_claude_accounts(False)

    def test_four_claude_accounts_with_original_transcript(self):
        self._four_claude_accounts(True)

    def test_setup_login_cached_status_and_idempotence(self):
        self.setup_slot("one")
        p = self.call("setup", "--provider", "claude", "--slot", "one")
        self.assertEqual(p.returncode, 0, p.stderr)
        p = self.call("status", "--provider", "claude", "--slot", "one", "--json")
        self.assertEqual(json.loads(p.stdout)["authenticated"], "unknown")
        self.assertFalse((self.home / ".claude").exists())
    def test_rotation_continues_exact_transcript(self):
        self.setup_slot("one")
        self.setup_slot("two")
        p = self.call("run", "--provider", "claude", "--pool", "one,two",
                      "--", "--print", "--permission-mode", "plan", "--session-id",
                      "b9e1fa9b-4890-43cb-a0b9-02a25f2b6e73", "original prompt")
        self.assertEqual(p.returncode, 0, p.stderr)
        rows = [json.loads(line) for line in Path(self.env["FAKE_LOG"]).read_text().splitlines()]
        self.assertEqual([r["slot"] for r in rows], ["one", "two"])
        self.assertIsNone(rows[0]["api_key"])
        self.assertIn("--resume", rows[1]["args"])
        path = rows[1]["args"][rows[1]["args"].index("--resume") + 1]
        self.assertTrue(Path(path).is_absolute())
        self.assertNotIn("original prompt", rows[1]["args"])
        self.assertIn("--permission-mode", rows[1]["args"])
        meta = (self.runtime / "config/native-account-slots/claude/one/slot.json").read_text()
        self.assertNotIn("original prompt", meta)
    def test_tool_failure_and_missing_transcript_never_replay(self):
        for no_transcript, generic in ((False, True), (True, False)):
            with self.subTest(no_transcript=no_transcript):
                self.setup_slot("one")
                self.setup_slot("two")
                env = self.env.copy()
                env["FAKE_GENERIC"] = "1" if generic else "0"
                env["FAKE_NO_TRANSCRIPT"] = "1" if no_transcript else "0"
                p = self.call("run", "--provider", "claude", "--pool", "one,two", "--max-attempts", "2",
                              "--", "--print", "--permission-mode", "plan", "prompt", env=env)
                self.assertNotEqual(p.returncode, 0, p.stderr)
                rows = Path(self.env["FAKE_LOG"]).read_text().splitlines()
                self.assertEqual(len(rows), 1)
                self.tearDown()
                self.setUp()

    def test_settings_seed_and_digest_guard(self):
        settings = self.home / "approved.json"
        settings.write_text('{"permissions":{"defaultMode":"plan"},"hooks":{}}')
        digest = hashlib.sha256(settings.read_bytes()).hexdigest()
        p = self.call("setup", "--provider", "claude", "--slot", "one", "--settings-from", str(settings), "--json")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout)["settings_sha256"], digest)
        p = self.call("login", "--provider", "claude", "--slot", "one")
        self.assertEqual(p.returncode, 0, p.stderr)
        profile = self.runtime / "config/claude/one/settings.json"
        profile.write_text("{}")
        p = self.call("run", "--provider", "claude", "--pool", "one", "--expected-settings-sha256", digest,
                      "--", "--print", "--permission-mode", "plan", "prompt")
        self.assertNotEqual(p.returncode, 0)
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())
        p = self.call("setup", "--provider", "claude", "--slot", "one", "--settings-from", str(settings))
        self.assertNotEqual(p.returncode, 0)
        settings.write_text('{"env":{"ANTHROPIC_API_KEY":"not-a-real-key"}}')
        p = self.call("setup", "--provider", "claude", "--slot", "other", "--settings-from", str(settings))
        self.assertNotEqual(p.returncode, 0)

    def test_callback_is_rechecked_and_blocks_retry(self):
        self.setup_slot("one")
        self.setup_slot("two")
        checker = self.home / "checker.py"
        marker = self.home / "check-count"
        # Admission precedes native control/settings, then repeats at spawn.
        checker.write_text("import json,pathlib,sys\np=pathlib.Path(sys.argv[1]);n=int(p.read_text()) if p.exists() else 0;p.write_text(str(n+1));print(json.dumps({'allow':n<2}))\n")
        command = json.dumps([sys.executable, str(checker), str(marker)])
        p = self.call("run", "--provider", "claude", "--pool", "one,two", "--max-attempts", "2",
                      "--admission-check-command", command,
                      "--", "--print", "--permission-mode", "plan", "prompt")
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(marker.read_text(), "3")
        self.assertEqual(len(Path(self.env["FAKE_LOG"]).read_text().splitlines()), 1)
        receipt_files = list((self.runtime / "config/native-account-slots/claude").glob("*/receipts/*.json"))
        self.assertEqual(len(receipt_files), 1)
        receipts = [json.loads(path.read_text()) for path in receipt_files]
        self.assertEqual({r["disposition"] for r in receipts}, {"rate_limit"})
        self.assertNotIn("prompt", json.dumps(receipts))

    def test_quota_refresh_uses_later_window_and_skips_exhausted(self):
        self.setup_slot("one")
        env = self.env.copy()
        env["FAKE_QUOTA"] = "1"
        p = self.call("status", "--provider", "claude", "--slot", "one", "--quota-refresh", "--json", env=env)
        self.assertEqual(p.returncode, 0, p.stderr)
        quota = json.loads(p.stdout)["quota"]
        self.assertTrue(quota["exhausted"])
        self.assertGreater(quota["reset"] - time.time(), 6 * 86400)
        self.assertLess(quota["reset"] - time.time(), 8 * 86400)
        p = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print", "--permission-mode", "plan", "prompt")
        self.assertEqual(p.returncode, 75, p.stderr)
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_wrong_identity_and_duplicate_binding(self):
        self.setup_slot("one")
        p = self.call("setup", "--provider", "claude", "--slot", "two")
        self.assertEqual(p.returncode, 0, p.stderr)
        env = self.env.copy()
        env["FAKE_ACCOUNT"] = "one"
        p = self.call("login", "--provider", "claude", "--slot", "two", env=env)
        self.assertNotEqual(p.returncode, 0)
        env["FAKE_ACCOUNT"] = "other"
        p = self.call("login", "--provider", "claude", "--slot", "one", env=env)
        self.assertNotEqual(p.returncode, 0)
        path = self.runtime / "config/native-account-slots/claude/one/slot.json"
        self.assertEqual(json.loads(path.read_text())["identity"]["account_uuid"], "one")

    def test_busy_and_session_mismatch_refuse_retry(self):
        self.setup_slot("one")
        lock_path = self.runtime / "config/native-account-slots/claude/one/.lock"
        with lock_path.open("r+") as f:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            p = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print", "--permission-mode", "plan", "prompt")
            self.assertEqual(p.returncode, 75)
        env = self.env.copy()
        env["FAKE_SESSION_MISMATCH"] = "1"
        p = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print", "--permission-mode", "plan", "prompt", env=env)
        self.assertNotEqual(p.returncode, 0)

    def test_timeout_when_child_closes_pipes(self):
        self.setup_slot("one")
        env = self.env.copy()
        env.update(FAKE_HANG="1", FAKE_CLOSED_PIPES="1")
        p = self.call("run", "--provider", "claude", "--pool", "one", "--timeout", "1",
                      "--", "--print", "--permission-mode", "plan", "prompt", env=env)
        self.assertEqual(p.returncode, 124, p.stderr)


    def test_timeout_kills_descendant_holding_output(self):
        self.setup_slot("one")
        env = self.env.copy()
        env.update(FAKE_HANG="1", FAKE_CHILD="1")
        p = self.call("run", "--provider", "claude", "--pool", "one", "--timeout", "1",
                      "--", "--print", "--permission-mode", "plan", "prompt", env=env)
        self.assertEqual(p.returncode, 124, p.stderr)
        pid = int(Path(self.env["FAKE_LOG"] + ".pid").read_text())
        state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], text=True, capture_output=True).stdout.strip()
        self.assertTrue(not state or state.startswith("Z"), state)



    def make_codex_pool(self, names=("one", "two")):
        fake = self.home / "fake-codex-more"
        fake.write_text(CODEX_FAKE)
        fake.chmod(0o755)
        env = self.env.copy()
        env["SB_NATIVE_CODEX"] = str(fake)
        for name in names:
            p = self.call("setup", "--provider", "codex", "--slot", name)
            self.assertEqual(p.returncode, 0, p.stderr)
            p = self.call("status", "--provider", "codex", "--slot", name, "--refresh", env=env)
            self.assertEqual(p.returncode, 0, p.stderr)
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(8)
        self.addCleanup(listener.close)
        port = listener.getsockname()[1]
        config = self.runtime / "config"
        taps = config / "capture.json"
        taps.write_text(json.dumps({"server":{"taps":[{"bind":"127.0.0.1:"+str(port),
            "capture_bodies":True,"headers":{"x-switchback-harness":"codex"}}]}}))
        (config / "native-codex-capture.json").write_text(json.dumps({
            "schema":"switchback/native-codex-capture@1","tap_port":port,
            "service_label":"invalid.test","capture_config":str(taps),
            "capture_config_sha256":hashlib.sha256(taps.read_bytes()).hexdigest()}))
        tools = self.home / "codex-tools"
        tools.mkdir()
        launchctl = tools / "launchctl"
        launchctl.write_text("#!/bin/sh\nexit 1\n")
        launchctl.chmod(0o755)
        env["PATH"] = str(tools)+":"+env["PATH"]
        return env

    def codex_run(self, env, pool="one,two"):
        return self.call("run", "--provider", "codex", "--pool", pool, "--timeout", "15",
                         "--prompt", "private prompt", "--approval-policy", "never",
                         "--sandbox", "read-only", env=env)


    def test_claude_cooldown_recovers_on_next_run_after_fresh_quota(self):
        self.setup_slot("one")
        first = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                          "--permission-mode", "plan", "first")
        self.assertEqual(first.returncode, 75, first.stderr)
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        self.assertIsNone(json.loads(slot.read_text())["cooldown"]["reset"])
        env = dict(self.env, FAKE_QUOTA="ready", FAKE_REJECT_SLOTS="", FAKE_QUOTA_LOG=str(self.home / "refresh.log"))
        second = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                           "--permission-mode", "plan", "next", env=env)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertNotIn("cooldown", json.loads(slot.read_text()))
        self.assertEqual((self.home / "refresh.log").read_text().splitlines(), ["refresh"])

    def test_claude_cooldown_unknown_is_retained_and_retried(self):
        self.setup_slot("one")
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"rate_limit","reset":None}
        slot.write_text(json.dumps(state))
        profile = self.runtime / "config/claude/one/.claude.json"
        native = json.loads(profile.read_text())
        native["cachedUsageUtilization"] = {"accountUuid":"one","fetchedAtMs":int(time.time()*1000)-2000,
                                             "utilization":{"five_hour":{"utilization":1}}}
        profile.write_text(json.dumps(native))
        env = dict(self.env, FAKE_QUOTA="unchanged", FAKE_QUOTA_LOG=str(self.home / "refresh.log"))
        for _ in range(2):
            result = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                               "--permission-mode", "plan", "prompt", env=env)
            self.assertEqual(result.returncode, 75, result.stderr)
        self.assertEqual((self.home / "refresh.log").read_text().splitlines(), ["refresh", "refresh"])
        self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_claude_revoked_or_changed_auth_never_recovers_cooldown(self):
        self.setup_slot("one")
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"authentication_failed","reset":None}
        slot.write_text(json.dumps(state))
        env = dict(self.env, FAKE_QUOTA="ready", FAKE_AUTH_REVOKED="1")
        result = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                           "--permission-mode", "plan", "prompt", env=env)
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertEqual(json.loads(slot.read_text())["identity"]["account_uuid"], "one")
        self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())
        profile = self.runtime / "config/claude/one/.claude.json"
        native = json.loads(profile.read_text())
        native["oauthAccount"]["accountUuid"] = "different"
        profile.write_text(json.dumps(native))
        result = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                           "--permission-mode", "plan", "prompt",
                           env=dict(self.env, FAKE_QUOTA="ready"))
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertEqual(json.loads(slot.read_text())["identity"]["account_uuid"], "one")
        self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_claude_refresh_respects_total_deadline(self):
        self.setup_slot("one")
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"rate_limit","reset":None}
        slot.write_text(json.dumps(state))
        env = dict(self.env, FAKE_QUOTA="delay")
        started = time.monotonic()
        result = self.call("run", "--provider", "claude", "--pool", "one", "--timeout", "1",
                           "--", "--print", "--permission-mode", "plan", "prompt", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertLess(time.monotonic()-started, 3)
        self.assertIn("cooldown", json.loads(slot.read_text()))


    def test_claude_auth_cooldown_recovers_after_native_refresh(self):
        self.setup_slot("one")
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"authentication_failed","reset":None}
        slot.write_text(json.dumps(state))
        env = dict(self.env, FAKE_QUOTA="ready", FAKE_REJECT_SLOTS="")
        result = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                           "--permission-mode", "plan", "prompt", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("cooldown", json.loads(slot.read_text()))

    def test_claude_partial_or_exhausted_refresh_stays_blocked(self):
        self.setup_slot("one")
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"rate_limit","reset":None}
        slot.write_text(json.dumps(state))
        for mode in ("partial", "1"):
            env = dict(self.env, FAKE_QUOTA=mode)
            result = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                               "--permission-mode", "plan", "prompt", env=env)
            self.assertEqual(result.returncode, 75, result.stderr)
            self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertGreater(json.loads(slot.read_text())["cooldown"]["reset"], time.time())
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_future_reset_skips_native_probe_and_reports_reset(self):
        for provider in ("claude", "codex"):
            with self.subTest(provider=provider):
                if provider == "claude":
                    self.setup_slot("one")
                    env = dict(self.env)
                else:
                    env = self.make_codex_pool(("one",))
                slot = self.runtime / ("config/native-account-slots/" + provider + "/one/slot.json")
                state = json.loads(slot.read_text())
                reset = int(time.time()) + 120
                state["cooldown"] = {"reason":"rate_limit","reset":reset}
                slot.write_text(json.dumps(state))
                env["FAKE_QUOTA_LOG"] = str(self.home / (provider + "-quota.log"))
                if provider == "claude":
                    result = self.call("run", "--provider", "claude", "--pool", "one", "--", "--print",
                                       "--permission-mode", "plan", "prompt", env=env)
                else:
                    result = self.codex_run(env, pool="one")
                self.assertEqual(result.returncode, 75, result.stderr)
                self.assertIn(str(reset), result.stderr)
                self.assertFalse(Path(env["FAKE_QUOTA_LOG"]).exists())

    def test_refresh_obeys_admission_before_native_probe(self):
        self.setup_slot("one")
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"rate_limit","reset":None}
        slot.write_text(json.dumps(state))
        checker = self.home / "deny.py"
        checker.write_text("import json; print(json.dumps({'allow':False}))\n")
        env = dict(self.env, FAKE_QUOTA="ready", FAKE_QUOTA_LOG=str(self.home / "refresh.log"))
        command = json.dumps([sys.executable, str(checker)])
        result = self.call("run", "--provider", "claude", "--pool", "one",
                           "--admission-check-command", command, "--", "--print",
                           "--permission-mode", "plan", "prompt", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.home / "refresh.log").exists())
        self.assertIn("cooldown", json.loads(slot.read_text()))

    def test_codex_refreshed_exhaustion_records_future_reset(self):
        env = self.make_codex_pool(("one",))
        slot = self.runtime / "config/native-account-slots/codex/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"rateLimitExceeded","reset":None}
        slot.write_text(json.dumps(state))
        env["FAKE_CODEX_EXHAUSTED"] = "1"
        result = self.codex_run(env, pool="one")
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertGreater(json.loads(slot.read_text())["cooldown"]["reset"], time.time())
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_codex_cooldown_recovers_next_run_with_fresh_bound_quota(self):
        env = self.make_codex_pool(("one",))
        env.update(FAKE_CODEX_READY="1", FAKE_REJECT_SLOTS="one")
        first = self.codex_run(env, pool="one")
        self.assertEqual(first.returncode, 75, first.stderr)
        slot = self.runtime / "config/native-account-slots/codex/one/slot.json"
        self.assertIsNone(json.loads(slot.read_text())["cooldown"]["reset"])
        env["FAKE_REJECT_SLOTS"] = ""
        second = self.codex_run(env, pool="one")
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertNotIn("cooldown", json.loads(slot.read_text()))

    def test_codex_unknown_and_revoked_cooldown_remain_retryable(self):
        env = self.make_codex_pool(("one",))
        slot = self.runtime / "config/native-account-slots/codex/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"usageLimitExceeded","reset":None}
        slot.write_text(json.dumps(state))
        env["FAKE_QUOTA_LOG"] = str(self.home / "refresh.log")
        for _ in range(2):
            result = self.codex_run(env, pool="one")
            self.assertEqual(result.returncode, 75, result.stderr)
            self.assertIn("quota_unknown", result.stderr)
        self.assertEqual((self.home / "refresh.log").read_text().splitlines(), ["refresh", "refresh"])
        revoked = dict(env, FAKE_CODEX_REVOKED="1", FAKE_CODEX_READY="1")
        result = self.codex_run(revoked, pool="one")
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertIn("relogin_required", result.stderr)
        self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())


    def test_cancellation_during_claude_refresh_preserves_cooldown(self):
        self.setup_slot("one")
        slot = self.runtime / "config/native-account-slots/claude/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"rate_limit","reset":None}
        slot.write_text(json.dumps(state))
        quota_log = self.home / "quota.log"
        env = dict(self.env, FAKE_QUOTA="delay", FAKE_QUOTA_LOG=str(quota_log))
        command = [str(SB), "account", "run", "--provider", "claude", "--pool", "one",
                   "--timeout", "15", "--", "--print", "--permission-mode", "plan", "prompt"]
        proc = subprocess.Popen(command, env=env, cwd=self.home, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        until = time.monotonic() + 8
        while not quota_log.exists() and time.monotonic() < until:
            time.sleep(.05)
        self.assertTrue(quota_log.exists())
        proc.send_signal(signal.SIGTERM)
        _, err = proc.communicate(timeout=10)
        self.assertEqual(proc.returncode, 130, err)
        self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_cancellation_during_codex_refresh_preserves_cooldown(self):
        env = self.make_codex_pool(("one",))
        slot = self.runtime / "config/native-account-slots/codex/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"unauthorized","reset":None}
        slot.write_text(json.dumps(state))
        env["FAKE_CODEX_AUTH_HANG"] = "1"
        probe = Path(self.env["FAKE_LOG"] + ".probe")
        command = [str(SB), "account", "run", "--provider", "codex", "--pool", "one",
                   "--timeout", "15", "--prompt", "prompt", "--approval-policy", "never",
                   "--sandbox", "read-only"]
        proc = subprocess.Popen(command, env=env, cwd=self.home, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        until = time.monotonic() + 8
        while not probe.exists() and time.monotonic() < until:
            time.sleep(.05)
        self.assertTrue(probe.exists())
        proc.send_signal(signal.SIGTERM)
        _, err = proc.communicate(timeout=10)
        self.assertEqual(proc.returncode, 130, err)
        self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_codex_changed_identity_does_not_clear_cooldown(self):
        env = self.make_codex_pool(("one",))
        slot = self.runtime / "config/native-account-slots/codex/one/slot.json"
        state = json.loads(slot.read_text())
        original = state["identity"]
        state["cooldown"] = {"reason":"unauthorized","reset":None}
        slot.write_text(json.dumps(state))
        env.update(FAKE_CODEX_ID="different", FAKE_CODEX_READY="1")
        result = self.codex_run(env, pool="one")
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertIn("identity_unverified_or_mismatch", result.stderr)
        self.assertEqual(json.loads(slot.read_text())["identity"], original)
        self.assertIn("cooldown", json.loads(slot.read_text()))
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_codex_refresh_respects_total_deadline(self):
        env = self.make_codex_pool(("one",))
        slot = self.runtime / "config/native-account-slots/codex/one/slot.json"
        state = json.loads(slot.read_text())
        state["cooldown"] = {"reason":"unauthorized","reset":None}
        slot.write_text(json.dumps(state))
        env["FAKE_CODEX_AUTH_HANG"] = "1"
        started = time.monotonic()
        result = self.call("run", "--provider", "codex", "--pool", "one", "--timeout", "1",
                           "--prompt", "prompt", "--approval-policy", "never", "--sandbox", "read-only", env=env)
        self.assertEqual(result.returncode, 124, result.stderr)
        self.assertLess(time.monotonic()-started, 3)
        self.assertIn("cooldown", json.loads(slot.read_text()))

    def test_codex_generic_failure_and_bad_rollout_do_not_rotate(self):
        env = self.make_codex_pool()
        for key in ("FAKE_CODEX_ERROR", "FAKE_WRONG_PATH"):
            with self.subTest(key=key):
                test_env = env.copy()
                test_env[key] = "internalServerError" if key == "FAKE_CODEX_ERROR" else "1"
                p = self.codex_run(test_env)
                self.assertNotEqual(p.returncode, 0)
                events = [json.loads(line) for line in Path(self.env["FAKE_LOG"]).read_text().splitlines()]
                self.assertFalse(any(e["slot"] == "two" for e in events))
                Path(self.env["FAKE_LOG"]).unlink()
                slot = self.runtime / "config/native-account-slots/codex/one/slot.json"
                data = json.loads(slot.read_text())
                data.pop("cooldown", None)
                slot.write_text(json.dumps(data))

    def test_codex_wrong_resume_identity_stops_before_second_turn(self):
        for key in ("FAKE_WRONG_THREAD", "FAKE_WRONG_CWD"):
            with self.subTest(key=key):
                env = self.make_codex_pool()
                env[key] = "1"
                p = self.codex_run(env)
                self.assertNotEqual(p.returncode, 0)
                events = [json.loads(line) for line in Path(self.env["FAKE_LOG"]).read_text().splitlines()]
                self.assertFalse(any(e["slot"] == "two" and e["method"] == "turn/start" for e in events))
                receipts = (self.runtime / "config/native-account-slots/codex").glob("*/receipts/*.json")
                self.assertTrue(all(json.loads(path.read_text())["disposition"] != "running" for path in receipts))
                self.tearDown()
                self.setUp()

    def test_codex_duplicate_identity_and_unknown_refresh(self):
        env = self.make_codex_pool()
        mismatch = env.copy()
        mismatch["FAKE_CODEX_ID"] = "two"
        p = self.call("status", "--provider", "codex", "--slot", "one", "--refresh", env=mismatch)
        self.assertNotEqual(p.returncode, 0)
        mismatch["FAKE_CODEX_ID"] = "one"
        p = self.call("setup", "--provider", "codex", "--slot", "third")
        self.assertEqual(p.returncode, 0, p.stderr)
        p = self.call("status", "--provider", "codex", "--slot", "third", "--refresh", env=mismatch)
        self.assertNotEqual(p.returncode, 0)
        for key in ("FAKE_CODEX_AUTH_FAIL", "FAKE_CODEX_QUOTA_FAIL"):
            bad = env.copy()
            bad[key] = "1"
            p = self.call("status", "--provider", "codex", "--slot", "one", "--refresh", "--json", env=bad)
            self.assertEqual(p.returncode, 0, p.stderr)
            value = json.loads(p.stdout)
            self.assertEqual(value["state"] if key.endswith("AUTH_FAIL") else value["quota"], "unknown")

    def test_codex_missing_capture_route_refuses(self):
        env = self.make_codex_pool()
        (self.runtime / "config/native-codex-capture.json").unlink()
        p = self.codex_run(env)
        self.assertNotEqual(p.returncode, 0)
        self.assertFalse(Path(self.env["FAKE_LOG"]).exists())

    def test_codex_cancel_kills_descendants_and_records_receipt(self):
        env = self.make_codex_pool()
        env["FAKE_CODEX_HANG"] = "1"
        command = [str(SB), "account", "run", "--provider", "codex", "--pool", "one,two",
                   "--timeout", "15", "--prompt", "private prompt", "--approval-policy", "never",
                   "--sandbox", "read-only"]
        proc = subprocess.Popen(command, env=env, cwd=self.home, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        pidfile = Path(self.env["FAKE_LOG"] + ".pid")
        until = time.monotonic()+8
        while not pidfile.exists() and time.monotonic()<until:
            time.sleep(.05)
        self.assertTrue(pidfile.exists())
        proc.send_signal(signal.SIGTERM)
        out, err = proc.communicate(timeout=12)
        self.assertEqual(proc.returncode, 130, err)
        pid = int(pidfile.read_text())
        state = subprocess.run(["ps","-o","stat=","-p",str(pid)], capture_output=True, text=True).stdout.strip()
        self.assertTrue(not state or state.startswith("Z"), state)
        receipts = list((self.runtime / "config/native-account-slots/codex/one/receipts").glob("*.json"))
        self.assertEqual(json.loads(receipts[0].read_text())["disposition"], "cancelled")
        self.assertNotIn("private prompt", receipts[0].read_text())

    def test_installer_rolls_back_late_failure(self):
        (self.runtime / "manifest.json").write_text('{"schema":"switchback/runtime-manifest@1","owner":"switchback"}')
        bindir = self.runtime / "bin"
        bindir.mkdir()
        for name in ("sb", "native-accounts.py", "native_account_process.py", "native_codex_account.py"):
            (bindir / name).write_text("old-" + name)
        prefix = self.home / "bin"
        prefix.mkdir()
        (prefix / "sb").symlink_to(SB)
        hooks = self.home / "python-hooks"
        hooks.mkdir()
        marker = self.home / "injected"
        (hooks / "sitecustomize.py").write_text(
            "import os\n_orig=os.replace\n"
            "def once(src,dst):\n"
            " if str(dst).endswith('/native_account_process.py') and not os.path.exists(os.environ['FAIL_MARKER']):\n"
            "  open(os.environ['FAIL_MARKER'],'w').close();raise OSError('injected late failure')\n"
            " return _orig(src,dst)\n"
            "os.replace=once\n")
        env = self.env.copy()
        env.update(SB_INSTALL_ACCOUNTS_ONLY="1", PREFIX=str(prefix), PYTHONPATH=str(hooks), FAIL_MARKER=str(marker))
        p = subprocess.run([str(SB.parent / "install.sh")], env=env, text=True, capture_output=True, timeout=20)
        self.assertNotEqual(p.returncode, 0)
        self.assertTrue(marker.exists(), p.stderr)
        for name in ("sb", "native-accounts.py", "native_account_process.py", "native_codex_account.py"):
            self.assertEqual((bindir / name).read_text(), "old-" + name)
        self.assertEqual((prefix / "sb").resolve(), SB.resolve())

    def test_instructions_seed_is_exact_and_non_overwriting(self):
        source = self.home / "instructions.md"
        source.write_text("Project instructions.\n")
        p = self.call("setup", "--provider", "claude", "--slot", "one",
                      "--instructions-from", str(source), "--json")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout)["instructions_sha256"], hashlib.sha256(source.read_bytes()).hexdigest())
        target = self.runtime / "config/claude/one/CLAUDE.md"
        self.assertEqual(target.read_bytes(), source.read_bytes())
        target.write_text("changed")
        p = self.call("setup", "--provider", "claude", "--slot", "one", "--instructions-from", str(source))
        self.assertNotEqual(p.returncode, 0)
        self.assertEqual(target.read_text(), "changed")

    def test_installer_narrow_preserves_engine(self):
        (self.runtime / "manifest.json").write_text('{"schema":"switchback/runtime-manifest@1","owner":"switchback"}')
        bindir = self.runtime / "bin"
        bindir.mkdir()
        engine = bindir / "switchback-bin"
        engine.write_bytes(b"existing engine")
        prefix = self.home / "bin"
        prefix.mkdir()
        (prefix / "sb").symlink_to(SB)
        env = self.env.copy()
        env.update(SB_INSTALL_ACCOUNTS_ONLY="1", PREFIX=str(prefix))
        installer = SB.parent / "install.sh"
        p = subprocess.run([str(installer)], env=env, text=True, capture_output=True, timeout=20)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(engine.read_bytes(), b"existing engine")
        self.assertEqual((prefix / "sb").resolve(), SB.resolve())
        self.assertTrue((bindir / "native_account_process.py").is_file())
        self.assertTrue((bindir / "native-accounts.py").is_file())

    def test_configured_provider_bridge(self):
        engine = self.home / "engine"
        engine.write_text("#!/usr/bin/env python3\nimport json,os,sys\na=sys.argv[1:];open(os.environ['FAKE_LOG'],'a').write(json.dumps(a)+'\\n');print(json.dumps({'providers':[{'id':'example','accounts':['one']}]} if 'providers' in a else {'presets':[]}))\n")
        engine.chmod(0o755)
        env = self.env.copy()
        env["SB_BIN"] = str(engine)
        p = self.call("providers", "--json", env=env)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout)["configured"]["providers"][0]["id"], "example")
        p = self.call("setup", "--provider", "example", "--preset", "openai", "--api-key-env", "EXAMPLE_KEY", env=env)
        self.assertEqual(p.returncode, 0, p.stderr)
        args = json.loads(Path(self.env["FAKE_LOG"]).read_text().splitlines()[-1])
        self.assertIn("EXAMPLE_KEY", args)
        self.assertNotIn("must-clear", args)
        p = self.call("status", "--provider", "example", "--json", env=env)
        self.assertEqual(json.loads(p.stdout)["provider"]["accounts"], ["one"])


    def test_codex_pool_uses_captured_app_server_and_exact_path(self):
        for name in ("one", "two", "three"):
            p = self.call("setup", "--provider", "codex", "--slot", name)
            self.assertEqual(p.returncode, 0, p.stderr)
        fake = self.home / "fake-codex"
        fake.write_text(CODEX_FAKE)
        fake.chmod(0o755)
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(8)
        self.addCleanup(listener.close)
        port = listener.getsockname()[1]
        config = self.runtime / "config"
        config.mkdir(exist_ok=True)
        taps = config / "capture.json"
        taps.write_text(json.dumps({"server":{"taps":[{"bind":"127.0.0.1:"+str(port),
            "capture_bodies":True,"headers":{"x-switchback-harness":"codex"}}]}}))
        route = {"schema":"switchback/native-codex-capture@1","tap_port":port,
                 "service_label":"invalid.test","capture_config":str(taps),
                 "capture_config_sha256":hashlib.sha256(taps.read_bytes()).hexdigest()}
        (config / "native-codex-capture.json").write_text(json.dumps(route))
        tools = self.home / "tools"
        tools.mkdir()
        launchctl = tools / "launchctl"
        launchctl.write_text("#!/bin/sh\nexit 1\n")
        launchctl.chmod(0o755)
        env = self.env.copy()
        env.update(SB_NATIVE_CODEX=str(fake), PATH=str(tools)+":"+env["PATH"], FAKE_REJECT_SLOTS="one,two")
        for name in ("one", "two", "three"):
            p = self.call("status", "--provider", "codex", "--slot", name, "--refresh", env=env)
            self.assertEqual(p.returncode, 0, p.stderr)
        p = self.call("run", "--provider", "codex", "--pool", "one,two,three", "--prompt", "original task",
                      "--approval-policy", "never", "--sandbox", "read-only", env=env)
        self.assertEqual(p.returncode, 0, p.stderr)
        events = [json.loads(line) for line in Path(self.env["FAKE_LOG"]).read_text().splitlines()]
        starts = [e for e in events if e["method"] in ("thread/start", "thread/resume")]
        self.assertEqual([e["method"] for e in starts], ["thread/start", "thread/resume", "thread/resume"])
        self.assertEqual(starts[1]["params"]["path"], starts[2]["params"]["path"])
        self.assertIn("/config/codex/one/sessions/", starts[1]["params"]["path"])
        self.assertNotIn("--dangerously-bypass-approvals-and-sandbox", starts[0]["args"])
        turns = [e for e in events if e["method"] == "turn/start"]
        self.assertNotIn("original task", json.dumps(turns[1]))
        self.assertIn("Continue", json.dumps(turns[1]))
        self.assertEqual(starts[1]["params"]["approvalPolicy"], "never")

    def test_budget_cap_blocks_automatic_resume(self):
        self.setup_slot("one")
        self.setup_slot("two")
        p = self.call("run", "--provider", "claude", "--pool", "one,two", "--max-attempts", "2",
                      "--", "--print", "--permission-mode", "plan", "--max-budget-usd", "1", "prompt")
        self.assertEqual(p.returncode, 75, p.stderr)
        self.assertIn("remaining budget", p.stderr)
        self.assertEqual(len(Path(self.env["FAKE_LOG"]).read_text().splitlines()), 1)

    def test_refuses_symlink_slot(self):
        root = self.runtime / "config/native-account-slots/claude"
        root.mkdir(parents=True)
        (root / "bad").symlink_to(self.home, target_is_directory=True)
        p = self.call("setup", "--provider", "claude", "--slot", "bad")
        self.assertNotEqual(p.returncode, 0)
    def test_codex_isolated_login_and_quota_unknown(self):
        fake = self.home / "fake-codex-status"
        fake.write_text(CODEX_FAKE)
        fake.chmod(0o755)
        env = self.env.copy()
        env["SB_NATIVE_CODEX"] = str(fake)
        for action in ("setup", "login", "status"):
            args = [action, "--provider", "codex", "--slot", "work"]
            if action == "status":
                args += ["--refresh", "--json"]
            p = self.call(*args, env=env)
            self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(json.loads(p.stdout)["quota"], "unknown")
        self.assertFalse((self.home / ".codex").exists())

if __name__ == "__main__":
    unittest.main()
