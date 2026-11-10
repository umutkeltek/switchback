#!/usr/bin/env python3
"""Account platform integration at the native CLI/authority/capture boundaries."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import native_account_catalog as catalog
import native_account_capture as capture
from native_account_process import Refusal
import sb_native_accounts as fixtures

class Platform(unittest.TestCase):
    def setUp(self):
        self.f = fixtures.NativeAccounts("test_setup_login_cached_status_and_idempotence")
        self.f.setUp()
    def tearDown(self):
        self.f.tearDown()

    def test_background_login_keeps_foreground_identity_binding(self):
        result = self.f.call("setup", "--provider", "claude", "--slot", "team")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.f.call("login", "--provider", "claude", "--slot", "team", "--background", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        flow = json.loads(result.stdout)["flow_id"]
        deadline = time.monotonic() + 8
        while True:
            result = self.f.call("login-status", "--flow", flow, "--json")
            self.assertEqual(result.returncode, 0, result.stderr)
            state = json.loads(result.stdout)
            if state["state"] not in ("pending", "waiting") or time.monotonic() > deadline:
                break
            time.sleep(.05)
        self.assertEqual(state["state"], "completed", state)
        slot = self.f.runtime / "config/native-account-slots/claude/team/slot.json"
        self.assertEqual(json.loads(slot.read_text())["identity"]["account_uuid"], "team")

    def test_default_enrollment_preserves_credentials_and_settings(self):
        home = self.f.home / ".claude"
        home.mkdir()
        original = {"settings.json": '{"theme":"dark"}', ".credentials.json": '{"synthetic":"do-not-copy"}'}
        for name, value in original.items():
            (home / name).write_text(value)
        meta = self.f.home / ".claude.json"
        meta.write_text(json.dumps({"oauthAccount":{"accountUuid":"personal","organizationUuid":"personal-org"}}))
        result = self.f.call("setup", "--provider", "claude", "--slot", "personal",
                             "--existing-default", "--plan", "max", "--label", "Personal")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.f.runtime / "config/claude/personal").exists())
        for name, value in original.items():
            self.assertEqual((home / name).read_text(), value)
        data = json.loads((self.f.runtime / "config/native-account-slots/claude/personal/slot.json").read_text())
        self.assertEqual(data["native_home"], {"kind":"existing_default"})
        self.assertEqual(data["declared"], {"plan":"max"})
        self.assertIsNone(data["identity"])

    def test_capture_required_refuses_before_model_launch(self):
        self.f.setup_slot("two")
        result = self.f.call("run", "--provider", "claude", "--pool", "two", "--require-capture",
                             "--prompt", "synthetic task", "--permission-mode", "plan")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("capture preflight", result.stderr)
        self.assertFalse(Path(self.f.env["FAKE_LOG"]).exists())

    def test_success_receipt_distinguishes_missing_capture(self):
        self.f.setup_slot("two")
        result = self.f.call("run", "--provider", "claude", "--pool", "two",
                             "--prompt", "synthetic task", "--permission-mode", "plan")
        self.assertEqual(result.returncode, 0, result.stderr)
        receipts = list((self.f.runtime / "config/native-account-slots/claude/two/receipts").glob("*.json"))
        self.assertEqual(len(receipts), 1)
        receipt = json.loads(receipts[0].read_text())
        self.assertEqual(receipt["provider_outcome"], "succeeded")
        self.assertTrue(receipt["attempt_terminal"])
        self.assertFalse(receipt["capture"]["complete"])
        self.assertEqual(receipt["attempt_id"], receipts[0].stem)
        self.assertNotIn("synthetic task", receipts[0].read_text())

    def test_background_device_code_requires_foreground_handoff(self):
        result = self.f.call("login", "--provider", "codex", "--slot", "team", "--background", "--device-auth")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("foreground", result.stderr)


    def test_capture_codex_ignores_sourced_shell_overrides(self):
        import subprocess
        env = self.f.make_codex_pool(("one",))
        self.addCleanup(self.f.doCleanups)
        marker = self.f.home / "must-not-run"
        malicious = "touch " + str(marker) + "\nexport CODEX_HOME=/wrong\nexport SB_NATIVE_CODEX=/wrong\n"
        (self.f.runtime / "config/sb.env").write_text(malicious)
        legacy = self.f.home / "legacy.env"
        legacy.write_text(malicious)
        fake = self.f.home / "native-inspect"
        fake.write_text("#!/usr/bin/python3\nimport json,os,sys\nprint(json.dumps({'home':os.environ.get('CODEX_HOME'),'args':sys.argv[1:]}))\n")
        fake.chmod(0o755)
        home = self.f.runtime / "config/codex/one"
        env.update(SB_NATIVE_CODEX=str(fake), CODEX_HOME=str(home), SWITCHBACK_LEGACY_ENV=str(legacy),
                   SB_NATIVE_ACCOUNT_ATTEMPT="attempt", SB_NATIVE_ACCOUNT_SESSION="session", SB_NATIVE_ACCOUNT_SLOT="one")
        wrapper = Path(__file__).resolve().parents[1] / "wrappers/codex-switchback-tap"
        result = subprocess.run([str(wrapper),"--sb-preserve-permissions","app-server"],env=env,
                                text=True,capture_output=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertFalse(marker.exists())
        report=json.loads(result.stdout)
        self.assertEqual(report["home"],str(home))
        provider=next(a for a in report["args"] if a.startswith("model_providers."))
        self.assertIn('supports_websockets=false',provider)
        self.assertIn('"x-switchback-account-attempt"="attempt"',provider)
        self.assertNotIn(",}",provider)

    def test_terminal_success_does_not_replay_transient_rejection(self):
        source=fixtures.FAKE.replace('print(json.dumps({"type":"result","subtype":"success","is_error":False}))',
            'if os.environ.get("FAKE_RECOVERY_ORDER")=="before": print(json.dumps({"type":"assistant","error":"rate_limit"}))\n'
            'print(json.dumps({"type":"result","subtype":"success","is_error":False}))\n'
            'if os.environ.get("FAKE_RECOVERY_ORDER")=="after": print(json.dumps({"type":"assistant","error":"rate_limit"}))')
        self.f.fake.write_text(source)
        self.f.setup_slot("two")
        self.f.setup_slot("three")
        for order in ("before","after"):
            result=self.f.call("run","--provider","claude","--pool","two,three","--prompt","synthetic",
                               "--permission-mode","plan",env=dict(self.f.env,FAKE_RECOVERY_ORDER=order))
            self.assertEqual(result.returncode,0,result.stderr)
        rows=[json.loads(line) for line in Path(self.f.env["FAKE_LOG"]).read_text().splitlines()]
        self.assertEqual([row["slot"] for row in rows],["two","two"])

class AuthorityAndCapture(unittest.TestCase):
    def test_composite_identity_prevents_organization_drift(self):
        args = SimpleNamespace(account_specs={"team":{"enabled":True,"native_home":{"kind":"managed","label":"team"},
                                                     "observed":{"account_id":"same-person","organization_id":"team-org"}}})
        with self.assertRaises(Refusal):
            catalog.check_account(args, "team", {"identity":{"account_uuid":"same-person","organization_uuid":"personal-org"}})

    def test_authority_revision_rechecked(self):
        args = SimpleNamespace(authority_revision="sha256:before")
        with patch.object(catalog, "call", return_value={"authority_revision":"sha256:after"}):
            with self.assertRaises(Refusal):
                catalog.check_revision("sb", args)

    def test_default_settings_require_explicit_apply(self):
        args = SimpleNamespace(profile="personal", profile_settings={"policy_ref":"shared"}, profile_revision="sha256:profile", authority_revision="sha256:authority")
        with patch.object(catalog, "call", return_value={"changed":True,"conflicts":[],"profile_revision":"sha256:profile","authority_revision":"sha256:authority","baseline_changed":False,"pending_recovery":False}):
            with self.assertRaises(Refusal):
                catalog.sync_settings("sb", args, "personal", {"native_home":{"kind":"existing_default"}})

    def test_managed_sync_uses_owner_revisions(self):
        args = SimpleNamespace(profile="team", profile_settings={"policy_ref":"shared"}, profile_revision="sha256:profile", authority_revision="sha256:authority")
        plan={"changed":True,"conflicts":[],"profile_revision":"sha256:profile","authority_revision":"sha256:authority","baseline_changed":False,"pending_recovery":False,"settings_revision":"sha256:before","effective_revision":"sha256:after"}
        with patch.object(catalog, "call", side_effect=[plan,{"applied":True,"plan":plan}]) as call:
            self.assertEqual(catalog.sync_settings("sb", args, "team", {}), "after")
            command = call.call_args_list[-1][0][1]
            self.assertIn("sha256:profile", command)
            self.assertIn("sha256:before", command)

    def test_capture_verification_failure_never_changes_provider_success(self):
        receipt={"attempt_id":"exact","disposition":"succeeded"}
        with patch.object(capture, "verify", return_value={"complete":False,"state":"unverified"}):
            capture.finish(receipt, {})
        self.assertEqual(receipt["disposition"], "succeeded")
        self.assertEqual(receipt["provider_outcome"], "succeeded")
        self.assertEqual(capture.capture_exit(receipt, True), 74)

    def test_verifier_must_match_exact_attempt(self):
        result=SimpleNamespace(returncode=0,stdout=json.dumps({"schema":"switchback/body-attempt-verification@1",
                                                             "attempt_id":"another","complete":True}).encode())
        with patch.object(capture.subprocess, "run", return_value=result):
            proof=capture.verify({"state":"ready","binary":"engine","state_dir":"/synthetic"}, "exact", wait=0)
        self.assertFalse(proof["complete"])

    def test_listener_capability_must_match_storage_and_protocol(self):
        report={"schema":"switchback/native-account-capture@1","capture_available":True,
                "observed_capture_mode":"segmented_full_wire","verifier_schema":"switchback/body-attempt-verification@1",
                "listener_kind":"forward_proxy","body_state_dir":"/wrong",
                "correlation_headers":["x-switchback-account-attempt","x-switchback-account-session","x-switchback-account-slot"]}
        with patch.object(capture.http.client, "HTTPConnection") as conn:
            conn.return_value.getresponse.return_value.status=200
            conn.return_value.getresponse.return_value.read.return_value=json.dumps(report).encode()
            with self.assertRaises(Refusal):
                capture.probe({"proxy_port":12345},"claude",Path("/expected"))
            conn.return_value.request.assert_called_once_with("GET","/.well-known/switchback/native-account-capture")


    def test_capture_codex_normalizes_competing_proxy(self):
        env=capture.configure_env({"state":"ready","provider":"codex"},{
            "HTTP_PROXY":"http://competing.invalid","http_proxy":"http://other.invalid",
            "ALL_PROXY":"socks5://competing.invalid","no_proxy":"*","SWITCHBACK_LEGACY_ENV":"/untrusted"})
        self.assertNotIn("HTTP_PROXY",env)
        self.assertNotIn("http_proxy",env)
        self.assertNotIn("ALL_PROXY",env)
        self.assertNotIn("SWITCHBACK_LEGACY_ENV",env)
        self.assertEqual(env["NO_PROXY"],"localhost,127.0.0.1,::1")
        self.assertEqual(env["no_proxy"],env["NO_PROXY"])

    def test_refresh_cannot_reenable_or_relabel_authority(self):
        prior={"provider":"claude","label":"Canonical","enabled":False,"native_home":{"kind":"managed","label":"team"},
               "declared":{"plan":"canonical-plan"}}
        data={"label":"stale","enabled":True,"declared":{"plan":"old"}}
        with patch.object(catalog,"authority_exists",return_value=True), patch.object(catalog,"call",return_value={
                "authority_revision":"sha256:revision","accounts":{"team":prior}}) as call:
            result=catalog.publish("sb","/runtime","claude","team",data)
        self.assertFalse(result["account"]["enabled"])
        self.assertEqual(result["account"]["label"],"Canonical")
        self.assertEqual(result["account"]["declared"],{"plan":"canonical-plan"})
        self.assertEqual(call.call_count,1)

    def test_capture_receipt_rejects_other_slot_same_attempt(self):
        result=SimpleNamespace(returncode=0,stdout=json.dumps({"schema":"switchback/body-attempt-verification@1",
            "attempt_id":"exact","complete":True,"scope":"observed_inference_requests","attempt_completion":"not_attested",
            "state_dir":"/synthetic","requests":[{"complete":True,"account_slot":"other","account_session_id":"session"}]}).encode())
        with patch.object(capture.subprocess,"run",return_value=result):
            proof=capture.verify({"state":"ready","binary":"engine","state_dir":"/synthetic"},"exact",wait=0,slot="expected",session="session")
        self.assertFalse(proof["complete"])

    def test_capture_required_project_settings_cannot_bypass_proxy(self):
        import tempfile
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            home=root/"native"; home.mkdir()
            project=root/"project"; (project/".claude").mkdir(parents=True)
            (project/".git").mkdir()
            for key in ("NO_PROXY","https_proxy","NODE_EXTRA_CA_CERTS"):
                (project/".claude/settings.json").write_text(json.dumps({"env":{key:"unsafe"}}))
                with self.assertRaises(Refusal):
                    catalog.validate_claude_settings(home,project,lambda p:p.read_bytes(),capture_required=True)

    def test_attested_native_executable_rejects_script(self):
        import tempfile, hashlib
        with tempfile.TemporaryDirectory() as temporary:
            path=Path(temporary)/"wrapper"
            path.write_text("#!/bin/sh\nexit 0\n");path.chmod(0o755)
            args=SimpleNamespace(expected_native_executable=str(path),expected_native_sha256=hashlib.sha256(path.read_bytes()).hexdigest())
            with self.assertRaises(Refusal):
                catalog.verify_native(args,{})

    def test_admission_settings_detects_change(self):
        import tempfile
        with tempfile.TemporaryDirectory() as temporary:
            home=Path(temporary);(home/"config.toml").write_text('model="changed"\n')
            args=SimpleNamespace(provider="codex",account_settings={"one":"missing"})
            with self.assertRaises(Refusal):
                catalog.check_settings(args,"one",home)

    def test_same_listener_id_with_stale_live_interception_refuses(self):
        report={"schema":"switchback/native-account-capture@1","listener_id":"native",
                "capture_available":True,"observed_capture_mode":"segmented_full_wire",
                "verifier_schema":"switchback/body-attempt-verification@1","listener_kind":"forward_proxy",
                "body_state_dir":"/expected","capture_harness":"claude","native_claude_intercepted":False,
                "correlation_headers":["x-switchback-account-attempt","x-switchback-account-session","x-switchback-account-slot"]}
        with patch.object(capture.http.client,"HTTPConnection") as conn:
            conn.return_value.getresponse.return_value.status=200
            conn.return_value.getresponse.return_value.read.return_value=json.dumps(report).encode()
            with self.assertRaises(Refusal):
                capture.probe({"proxy_port":12345},"claude",Path("/expected"),"native",{"native_claude_intercepted":True})


    def test_equal_settings_still_adopts_baseline(self):
        args=SimpleNamespace(profile="team",profile_settings={"policy_ref":"shared"},
                             profile_revision="sha256:profile",authority_revision="sha256:authority")
        plan={"changed":False,"baseline_changed":True,"pending_recovery":False,"conflicts":[],
              "profile_revision":args.profile_revision,"authority_revision":args.authority_revision,
              "settings_revision":"sha256:before","effective_revision":"sha256:after"}
        with patch.object(catalog,"call",side_effect=[plan,{"applied":True,"plan":plan}]) as call:
            self.assertEqual(catalog.sync_settings("sb",args,"team",{}),"after")
        self.assertEqual(call.call_args_list[-1][0][1][:2],["settings","apply"])

    def test_managed_recovery_replans_before_apply(self):
        args=SimpleNamespace(profile="team",profile_settings={"policy_ref":"shared"},
                             profile_revision="sha256:profile",authority_revision="sha256:authority")
        plan={"changed":True,"baseline_changed":False,"pending_recovery":False,"conflicts":[],
              "profile_revision":args.profile_revision,"authority_revision":args.authority_revision,
              "settings_revision":"sha256:before","effective_revision":"sha256:after"}
        pending=dict(plan,pending_recovery=True)
        with patch.object(catalog,"call",side_effect=[pending,{"recovered":False,"conflicts":[]},plan,{"applied":True,"plan":plan}]) as call:
            self.assertEqual(catalog.sync_settings("sb",args,"team",{}),"after")
        self.assertEqual([item[0][1][1] for item in call.call_args_list],["plan","recover","plan","apply"])

    def test_unchanged_settings_cannot_skip_revision_guard(self):
        args=SimpleNamespace(profile="team",profile_settings={"policy_ref":"shared"},
                             profile_revision="sha256:profile",authority_revision="sha256:authority")
        plan={"changed":False,"baseline_changed":False,"pending_recovery":False,"conflicts":[],
              "profile_revision":"sha256:other","authority_revision":args.authority_revision,
              "settings_revision":"sha256:before","effective_revision":"sha256:after"}
        with patch.object(catalog,"call",return_value=plan) as call:
            with self.assertRaises(Refusal):
                catalog.sync_settings("sb",args,"team",{})
        self.assertEqual(call.call_count,1)

    def test_default_recovery_requires_explicit_owner_action(self):
        args=SimpleNamespace(profile="personal",profile_settings={"policy_ref":"shared"},
                             profile_revision="sha256:profile",authority_revision="sha256:authority")
        plan={"changed":False,"baseline_changed":False,"pending_recovery":True,"conflicts":[],
              "profile_revision":args.profile_revision,"authority_revision":args.authority_revision}
        with patch.object(catalog,"call",return_value=plan) as call:
            with self.assertRaises(Refusal):
                catalog.sync_settings("sb",args,"personal",{"native_home":{"kind":"existing_default"}})
        self.assertEqual(call.call_count,1)

    def test_native_profile_cannot_ignore_off_capture_policy(self):
        args=SimpleNamespace(profile="private",pool=None,pool_name=None)
        report={"profile":{"execution_backend":"native_account_pool","capture_policy":{"mode":"off"}}}
        with patch.object(catalog,"call",return_value=report):
            with self.assertRaises(Refusal):
                catalog.resolve("sb",args)

if __name__ == "__main__":
    unittest.main()
