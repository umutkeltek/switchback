#!/usr/bin/env python3
"""Exercise collector timing through the real Rust CLI and the actual file lock."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

CLI = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(CLI))
import native_account_usage as usage
spec = importlib.util.spec_from_file_location("refresh_native", CLI / "native-accounts.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)

class RefreshTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-refresh-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.state = self.root / "state"
        self.state.mkdir()
        self.now = time.time()
        self.calls = []
        self.error = "timeout"
        self.fs = SimpleNamespace(read_json=native.read_json,write_json=native.write_json,
            safe_dir=native.safe_dir,lock=native.lock,
            valid_name=lambda x: isinstance(x,str) and native.NAME.fullmatch(x) is not None)
        self.args = SimpleNamespace(provider="fixture",slot=None,timeout=30,refresh=False,refresh_if_due=True)
        self.row = {"provider":"fixture","slot":"one","source":"codexbar/api",
            "observed_at":usage.iso(self.now-3600),
            "windows":[usage.window("primary",70,self.now+3600,self.now-3600,self.now,300)]}
        self.save()

    def save(self):
        native.write_json(self.state/"account-usage.json",{"schema":usage.SCHEMA,"accounts":[self.row]})

    def query(self, command, timeout):
        if command[1:3] == ["config","providers"]:
            return {"providers":[{"id":"fixture"}]}, None
        if command[1:4] == ["profile","accounts","refresh-plan"]:
            p=subprocess.run([os.environ["SB_TEST_ENGINE"],*command[1:]],capture_output=True,text=True,timeout=15)
            self.assertEqual(p.returncode,0,p.stderr)
            return json.loads(p.stdout),None
        self.calls.append(command)
        if self.error:
            return None,self.error
        return [{"provider":"fixture","account":"one","usage":{"updatedAt":usage.iso(self.now),
            "primary":{"usedPercent":70,"windowMinutes":300,"resetsAt":usage.iso(self.now+3600)}}}],None

    def run_report(self):
        with patch.object(usage,"query",side_effect=self.query), \
                patch.object(usage.shutil,"which",return_value="/fixture/bin"), \
                patch.object(Path,"home",return_value=self.root):
            return usage.report(self.args,self.root,"fixture-sb",self.fs)

    @unittest.skipUnless(os.environ.get("SB_TEST_ENGINE"),"requires actual Rust engine")
    def test_failed_refresh_backs_off_without_relabeling_old_sample(self):
        first=self.run_report()
        self.assertEqual(first["refresh"]["state"],"completed")
        self.assertEqual(first["accounts"][0]["observed_at"],self.row["observed_at"])
        self.assertEqual(first["accounts"][0]["refresh_error"],"timeout")
        self.assertEqual(len(self.calls),1)
        second=self.run_report()
        self.assertEqual(second["refresh"]["state"],"not_due")
        self.assertEqual(len(self.calls),1)
        self.assertEqual(second["accounts"][0]["windows"][0]["state"],"stale")

    @unittest.skipUnless(os.environ.get("SB_TEST_ENGINE"),"requires actual Rust engine")
    def test_changed_identity_invalidates_old_backoff(self):
        self.run_report()
        self.row["slot"]="second"
        self.save()
        self.run_report()
        self.assertEqual(len(self.calls),2)

    @unittest.skipUnless(os.environ.get("SB_TEST_ENGINE"),"requires actual Rust engine")
    def test_recent_success_skips_duplicate_scheduled_refresh(self):
        self.error=None
        self.run_report()
        result=self.run_report()
        self.assertEqual(result["refresh"]["state"],"not_due")
        self.assertEqual(len(self.calls),1)
        saved=native.read_json(self.state/"account-usage-refresh.json")
        self.assertEqual(saved["collectors"]["fixture/*"]["consecutive_failures"],0)

    def test_concurrent_request_returns_saved_observations_without_probe(self):
        self.args.refresh=True
        self.args.refresh_if_due=False
        with native.lock(self.state/".usage-refresh.lock"):
            result=self.run_report()
        self.assertEqual(result["refresh"]["state"],"in_progress")
        self.assertEqual(result["accounts"][0]["observed_at"],self.row["observed_at"])
        self.assertEqual(self.calls,[])
        self.assertFalse((self.state/"account-usage-refresh.json").exists())

    def test_unsafe_lock_is_not_reported_as_an_in_progress_refresh(self):
        target=self.root/"target"
        target.write_text("untouched")
        (self.state/".usage-refresh.lock").symlink_to(target)
        with self.assertRaises(OSError):
            self.run_report()
        self.assertEqual(target.read_text(),"untouched")
        self.assertEqual(self.calls,[])

    def test_cancelled_probe_preserves_backoff_checkpoint_and_releases_lock(self):
        self.args.refresh=True
        self.args.refresh_if_due=False
        def fail(command,timeout):
            if command[1:3] == ["config","providers"]:
                return {"providers":[{"id":"fixture"}]},None
            raise usage.Refusal("usage refresh cancelled")
        with patch.object(usage,"query",side_effect=fail), \
                patch.object(usage.shutil,"which",return_value="/fixture/bin"), \
                patch.object(Path,"home",return_value=self.root):
            with self.assertRaises(usage.Refusal):
                usage.report(self.args,self.root,"fixture-sb",self.fs)
        checkpoint=native.read_json(self.state/"account-usage-refresh.json")
        self.assertEqual(checkpoint["collectors"]["fixture/*"]["consecutive_failures"],1)
        self.assertEqual(native.read_json(self.state/"account-usage.json")["accounts"][0]["observed_at"],
                         self.row["observed_at"])
        with native.lock(self.state/".usage-refresh.lock") as acquired:
            self.assertTrue(acquired)


    @unittest.skipUnless(os.environ.get("SB_TEST_ENGINE"),"requires actual Rust engine")
    def test_rust_cli_refuses_duplicate_collectors_and_unrecognized_data(self):
        for observations in ([{"key":"one"},{"key":"one"}], [{"key":"one","credential":"synthetic"}]):
            result=subprocess.run([os.environ["SB_TEST_ENGINE"],"profile","accounts","refresh-plan",
                "--observations-json",json.dumps(observations),"--json"],capture_output=True,text=True,timeout=10)
            self.assertNotEqual(result.returncode,0)
            self.assertNotIn("synthetic",result.stderr)

    def test_corrupt_retry_state_refuses_before_network(self):
        native.write_json(self.state/"account-usage-refresh.json",
                         {"schema":"switchback/usage-refresh-state@1","collectors":[]})
        with self.assertRaises(usage.Refusal):
            self.run_report()
        self.assertEqual(self.calls,[])

    def test_native_read_preserves_transient_error_until_sample_changes(self):
        self.args.provider="claude"
        self.args.refresh_if_due=False
        slot=self.root/"config/native-account-slots/claude/one"
        slot.mkdir(parents=True)
        home=self.root/"config/claude/one"
        home.mkdir(parents=True)
        identity={"account_uuid":"fixture-account","organization_uuid":"fixture-org"}
        native.write_json(slot/"slot.json",{"provider":"claude","slot":"one","identity":identity,"enabled":True})
        sample=int((self.now-3600)*1000)
        data={"oauthAccount":{"accountUuid":"fixture-account","organizationUuid":"fixture-org"},
              "cachedUsageUtilization":{"accountUuid":"fixture-account","fetchedAtMs":sample,"utilization":{
                  "five_hour":{"utilization":20},"seven_day":{"utilization":30}}}}
        native.write_json(home/".claude.json",data)
        self.row={"provider":"claude","slot":"one","source":"native/claude","account_id":identity,
                  "observed_at":usage.iso(sample/1000),"refresh_error":"timeout","windows":[]}
        self.save()
        self.assertEqual(self.run_report()["accounts"][0]["refresh_error"],"timeout")
        data["cachedUsageUtilization"]["fetchedAtMs"]=sample+60_000
        native.write_json(home/".claude.json",data)
        self.assertNotIn("refresh_error",self.run_report()["accounts"][0])

if __name__ == "__main__":
    unittest.main()
