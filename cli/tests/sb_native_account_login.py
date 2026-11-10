#!/usr/bin/env python3
"""Synthetic native login lifecycle tests; no credentials or browser needed."""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import native_account_login as login


class LoginTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sb-login-test-")
        self.runtime = Path(self.temp.name).resolve()
        self.flows = []

    def tearDown(self):
        for flow in self.flows:
            try:
                login.cancel_login(self.runtime, flow["flow_id"])
            except (OSError, login.Refusal):
                pass
        # Wait for workers to observe cancellation before removing their state.
        deadline = time.monotonic() + 3
        for flow in self.flows:
            while time.monotonic() < deadline:
                path = self.runtime / "state/native-account-logins" / flow["flow_id"]
                try:
                    if not login._live(path):
                        break
                except (OSError, login.Refusal):
                    break
                time.sleep(0.02)
        self.temp.cleanup()

    def start(self, code, provider="claude", slot="test", timeout=3):
        result = login.start_login(self.runtime, provider, slot,
                                   [sys.executable, "-B", "-c", code], timeout=timeout)
        self.flows.append(result)
        return result

    def until(self, flow, states, timeout=4):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            result = login.poll_login(self.runtime, flow["flow_id"])
            if result["state"] in states:
                return result
            time.sleep(0.02)
        self.fail("flow did not reach expected state")

    def test_claude_url_and_completed_replay(self):
        flow = self.start("import time; print('private native output', flush=True); "
                          "print('https://claude.com/cai/oauth/authorize?state=synthetic', flush=True); "
                          "time.sleep(.4)")
        waiting = self.until(flow, {"waiting"})
        self.assertEqual(waiting["authorization_url"],
                         "https://claude.com/cai/oauth/authorize?state=synthetic")
        done = self.until(flow, {"completed"})
        self.assertFalse(done["relogin_required"])
        self.assertNotIn("authorization_url", done)
        self.assertEqual(done, login.poll_login(self.runtime, flow["flow_id"]))
        self.assertEqual(done, login.cancel_login(self.runtime, flow["flow_id"]))
        root = self.runtime / "state/native-account-logins"
        for file in root.rglob("*"):
            if file.is_file():
                self.assertNotIn("private native output", file.read_text())
                self.assertEqual(file.stat().st_mode & 0o077, 0)
        self.assertNotIn("pid", json.dumps(done).lower())

    def test_codex_native_url(self):
        flow = self.start("import time; "
                          "print('https://auth.openai.com/oauth/authorize?state=synthetic', flush=True); "
                          "time.sleep(.4)", provider="codex")
        self.assertIn("auth.openai.com/oauth/authorize", self.until(flow, {"waiting"})["authorization_url"])
        self.until(flow, {"completed"})

    def test_url_validator(self):
        for provider, url in (
                ("claude", "https://claude.ai/oauth/authorize?code=true&state=synthetic"),
                ("claude", "https://platform.claude.com/oauth/authorize"),
                ("claude", "https://console.anthropic.com/oauth/authorize"),
                ("codex", "https://auth.openai.com/codex/device")):
            self.assertEqual(url, login.authorization_url(provider, url))
        for url in ("https://claude.com.evil.invalid/cai/oauth/authorize",
                    "https://claude.com@evil.invalid/cai/oauth/authorize",
                    "https://claude.com:443/cai/oauth/authorize",
                    "https://claude.com/cai/oauth/authorize#token",
                    "https://claude.com/cai/oauth/authorize?access_token=secret",
                    "https://claude.com/cai/oauth/authorize?%63ode=secret",
                    "https://claude.com/cai/oauth/authorize?state=%GG",
                    "https://claude.com/cai/oauth/authorize\n",
                    "http://claude.com/cai/oauth/authorize",
                    "https://claude.com/cai/oauth/authorize/../other",
                    "https://claude.com/other", "https://[malformed"):
            self.assertIsNone(login.authorization_url("claude", url), url)
        self.assertIsNone(login.authorization_url("claude", "https://auth.openai.com/codex/device"))

    def test_unknown_and_malformed_url_not_persisted(self):
        flow = self.start("import time; "
                          "print('https://evil.invalid/login?secret=hidden', flush=True); "
                          "print('https://claude.com/cai/oauth/authorize?access_token=hidden', flush=True); "
                          "time.sleep(.3); raise SystemExit(1)")
        done = self.until(flow, {"failed"})
        self.assertTrue(done["relogin_required"])
        for file in (self.runtime / "state/native-account-logins").rglob("*.json"):
            self.assertNotIn("hidden", file.read_text())
            self.assertNotIn("evil.invalid", file.read_text())

    def test_split_url(self):
        flow = self.start("import sys,time; "
                          "sys.stdout.write('https://claude.com/cai/oauth/authorize?state='); sys.stdout.flush(); "
                          "time.sleep(.15); print('synthetic', flush=True); time.sleep(.3)")
        result = self.until(flow, {"waiting"})
        self.assertEqual(result["authorization_url"],
                         "https://claude.com/cai/oauth/authorize?state=synthetic")
        self.until(flow, {"completed"})

    def test_duplicate_start_and_cancel(self):
        flow = self.start("import time; time.sleep(20)")
        with self.assertRaises(login.Refusal):
            self.start("pass")
        cancelled = login.cancel_login(self.runtime, flow["flow_id"])
        self.assertEqual(cancelled["state"], "cancelled")
        self.assertEqual(cancelled, login.cancel_login(self.runtime, flow["flow_id"]))
        self.assertEqual(cancelled, login.poll_login(self.runtime, flow["flow_id"]))
        deadline = time.monotonic() + 3
        path = login._flow(self.runtime, flow["flow_id"])
        while login._live(path) and time.monotonic() < deadline:
            time.sleep(.02)
        replacement = self.start("pass")
        self.until(replacement, {"completed"})

    def test_timeout(self):
        flow = self.start("import time; time.sleep(20)", timeout=1)
        expired = self.until(flow, {"expired"})
        self.assertTrue(expired["relogin_required"])
        self.assertEqual(expired, login.cancel_login(self.runtime, flow["flow_id"]))

    def test_native_error_is_failed(self):
        flow = self.start("raise SystemExit(19)")
        self.until(flow, {"failed"})

    def test_cancelled_before_native_start(self):
        root = login._root(self.runtime, create=True)
        path = root / ("login_" + "a" * 32)
        path.mkdir(mode=0o700)
        login._write(path / "status.json", {
            "flow_id": path.name, "provider": "claude", "slot": "test",
            "state": "cancelled", "expires_at": time.time() + 10,
            "relogin_required": True})
        marker = self.runtime / "should-not-exist"
        result = login._run(path, [sys.executable, "-c",
                                  "from pathlib import Path; Path(%r).touch()" % str(marker)])
        self.assertEqual(result["state"], "cancelled")
        self.assertFalse(marker.exists())

    def test_dead_worker_is_failed_without_signalling_metadata_pid(self):
        root = login._root(self.runtime, create=True)
        path = root / ("login_" + "b" * 32)
        path.mkdir(mode=0o700)
        fd = login._open(path / ".alive", os.O_RDWR | os.O_CREAT)
        os.close(fd)
        login._write(path / "status.json", {
            "flow_id": path.name, "provider": "codex", "slot": "test",
            "state": "pending", "expires_at": time.time() + 10,
            "relogin_required": True, "pid": os.getpid()})
        with mock.patch.object(os, "kill", side_effect=AssertionError("unsafe signal")), \
                mock.patch.object(os, "killpg", side_effect=AssertionError("unsafe signal")):
            result = login.poll_login(self.runtime, path.name)
            self.assertEqual(result["state"], "failed")
            self.assertEqual(result, login.cancel_login(self.runtime, path.name))

    def test_descendants_cannot_hold_output_open(self):
        marker = self.runtime / "orphan-marker"
        descendant = "import time; from pathlib import Path; time.sleep(.8); Path(%r).touch()" % str(marker)
        flow = self.start("import subprocess,sys; subprocess.Popen([sys.executable,'-c',%r])" % descendant)
        self.until(flow, {"completed"}, timeout=1.5)
        time.sleep(1)
        self.assertFalse(marker.exists())

    def test_lost_supervisor_cleans_native_group(self):
        marker = self.runtime / "orphan-marker"
        ready = self.runtime / "native-ready"
        command = ("import time; from pathlib import Path; Path(%r).touch(); "
                   "time.sleep(.8); Path(%r).touch(); time.sleep(20)") % (str(ready), str(marker))
        real_popen = subprocess.Popen
        workers = []
        def capture(*args, **kwargs):
            worker = real_popen(*args, **kwargs)
            workers.append(worker)
            return worker
        with mock.patch.object(login.subprocess, "Popen", side_effect=capture):
            flow = self.start(command)
        deadline = time.monotonic() + 2
        while not ready.exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertTrue(ready.exists())
        workers[0].kill()  # A live child handle, never a persisted PID.
        workers[0].wait(timeout=2)
        self.until(flow, {"failed"})
        time.sleep(1)
        self.assertFalse(marker.exists())

    def test_state_symlink_refused(self):
        outside = self.runtime / "outside"
        outside.mkdir()
        (self.runtime / "state").symlink_to(outside, target_is_directory=True)
        with self.assertRaises(login.Refusal):
            self.start("pass")

    def test_file_symlink_refused(self):
        flow = self.start("import time; time.sleep(20)")
        login.cancel_login(self.runtime, flow["flow_id"])
        path = login._flow(self.runtime, flow["flow_id"])
        deadline = time.monotonic() + 2
        while login._live(path) and time.monotonic() < deadline:
            time.sleep(.02)
        original = (path / "status.json").read_text()
        target = self.runtime / "outside.json"
        target.write_text(original)
        (path / "status.json").unlink()
        (path / "status.json").symlink_to(target)
        with self.assertRaises((login.Refusal, OSError)):
            login.poll_login(self.runtime, flow["flow_id"])
        self.assertEqual(original, target.read_text())

    def test_invalid_inputs(self):
        for timeout in (0, -1, 2701, float("nan"), True):
            with self.assertRaises(login.Refusal):
                login.start_login(self.runtime, "claude", "test", ["true"], timeout=timeout)
        for provider, slot in (("unknown", "test"), ("claude", "../bad")):
            with self.assertRaises(login.Refusal):
                login.start_login(self.runtime, provider, slot, ["true"])
        with self.assertRaises(login.Refusal):
            login.poll_login(self.runtime, "../../outside")

    def test_output_is_bounded(self):
        flow = self.start("import os,time; os.write(1, b'x'*1100000); time.sleep(20)")
        self.until(flow, {"failed"})
        root = self.runtime / "state/native-account-logins"
        self.assertLess(sum(f.stat().st_size for f in root.rglob("*") if f.is_file()), 2000)


if __name__ == "__main__":
    unittest.main()
