#!/usr/bin/env python3
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

CLI = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(CLI))
import provider_account_usage as provider
import native_account_usage as usage

spec = importlib.util.spec_from_file_location("native_accounts_fixture", CLI / "native-accounts.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class ProviderUsageTests(unittest.TestCase):
    now = 1800000000

    def test_zai_keeps_independent_windows_and_monthly_marker(self):
        payload = {"success": True, "code": 200, "data": {"limits": [
            {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 8, "nextResetTime": (self.now + 90) * 1000},
            {"type": "TOKENS_LIMIT", "unit": 6, "number": 1, "usage": 100, "currentValue": 10,
             "remaining": 80, "percentage": 2, "nextResetTime": (self.now + 900) * 1000},
            {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 40}]}}
        windows, _ = provider.parse_zai(payload, self.now, usage.window)
        self.assertEqual([w["used_percent"] for w in windows], [8, 20, 40])
        self.assertEqual([w["window_minutes"] for w in windows], [300, 10080, None])
        self.assertEqual([w["reset_in_seconds"] for w in windows], [90, 900, None])
        self.assertEqual(windows[2]["name"], "mcp_monthly")
        self.assertEqual([w["scope"] for w in windows], ["generation", "generation", "tools"])

    def test_zai_missing_or_malformed_values_never_invent_usage(self):
        payload = {"success": True, "code": 200, "data": {"limits": [
            {"type": "TOKENS_LIMIT", "unit": {}, "number": 5, "usage": 10},
            {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": True}]}}
        windows, _ = provider.parse_zai(payload, self.now, usage.window)
        self.assertTrue(all(w["used_percent"] is None for w in windows))
        for payload in ({}, {"success": True, "code": 200, "data": {"limits": []}}):
            with self.assertRaises(provider.UsageError):
                provider.parse_zai(payload, self.now, usage.window)

    def test_minimax_nested_remaining_fields_and_percent_only_weekly(self):
        raw = {"model_name": "general", "current_interval_total_count": "1000",
               "current_interval_usage_count": "250",
               "start_time": self.now * 1000, "end_time": (self.now + 18000) * 1000,
               "current_weekly_total_count": 0, "current_weekly_usage_count": 0,
               "current_weekly_remaining_percent": "70", "current_weekly_status": 1,
               "weekly_start_time": self.now, "weekly_end_time": self.now + 604800,
               "weekly_boost_permille": 1500}
        windows, _ = provider.parse_minimax({"data": {"base_resp": {"status_code": 0}, "model_remains": [raw]}},
                                            self.now, usage.window)
        self.assertEqual([w["used_percent"] for w in windows], [75, 30])
        self.assertEqual([w["window_minutes"] for w in windows], [300, 10080])
        self.assertEqual([w["reset_in_seconds"] for w in windows], [18000, 604800])
        self.assertTrue(all(w["scope"] == "generation" and w["model"] == "general" for w in windows))

    def test_minimax_unavailable_and_unlimited_are_distinct(self):
        raw = [{"model_name": name, "current_weekly_total_count": 0, "current_weekly_usage_count": 0,
                "current_weekly_remaining_percent": 100, "current_weekly_status": 3}
               for name in ("general", "video")]
        windows, notes = provider.parse_minimax({"model_remains": raw}, self.now, usage.window)
        self.assertEqual(windows, [])
        self.assertEqual(notes, ["general: weekly unlimited"])

    def test_minimax_reset_is_per_window_and_does_not_freshen_past_reset(self):
        raw = {"model_name": "general", "current_interval_remaining_percent": 25,
               "end_time": self.now - 60, "remains_time": 7200000,
               "current_weekly_remaining_percent": 90, "weekly_remains_time": 7200000}
        windows, _ = provider.parse_minimax({"model_remains": [raw]}, self.now, usage.window)
        self.assertEqual(windows[0]["state"], "reset_passed")
        self.assertEqual(windows[1]["reset_in_seconds"], 7200)
        self.assertEqual(windows[1]["reset_source"], "relative_duration_heuristic")

    def test_minimax_missing_reset_stays_unknown_and_auth_body_is_not_echoed(self):
        windows, _ = provider.parse_minimax({"model_remains": [{"model_name": "general",
                                            "current_interval_remaining_percent": 20}]}, self.now, usage.window)
        self.assertIsNone(windows[0]["resets_at"])
        with self.assertRaisesRegex(provider.UsageError, "^authentication_required$"):
            provider.parse_minimax({"data": {"base_resp": {"status_code": 1004, "status_msg": "DO_NOT_ECHO"}}},
                                   self.now, usage.window)

    def test_account_refs_do_not_collapse_or_fall_through_from_vault(self):
        definitions = [{"id": "zai-coding", "accounts": [
            {"id": "one", "auth": {"kind": "api_key", "env": "FIXTURE_ONE"}},
            {"id": "two", "auth": {"kind": "api_key", "env": "FIXTURE_TWO"}},
            {"id": "vault", "auth": {"kind": "api_key", "env": "FIXTURE_ONE", "vault": "fixture"}}]}]
        accounts = provider.discover(definitions, {"zai"}, {"FIXTURE_ONE": "SYNTHETIC_ONE", "FIXTURE_TWO": "SYNTHETIC_TWO"})
        self.assertEqual([a["slot"] for a in accounts], ["zai-coding--one", "zai-coding--two", "zai-coding--vault"])
        self.assertEqual(accounts[2]["_error"], "credential_source_unsupported")
        self.assertIsNone(accounts[2]["_token"])
        rotated = provider.discover(definitions, {"zai"}, {"FIXTURE_ONE": "ROTATED", "FIXTURE_TWO": "SYNTHETIC_TWO"})
        self.assertNotEqual(accounts[0]["credential_revision"], rotated[0]["credential_revision"])
        self.assertEqual(accounts[1]["credential_revision"], rotated[1]["credential_revision"])

    def test_minimax_legacy_fallback_keeps_account_region_and_deadline(self):
        account = provider.discover([], {"minimax"}, {"MINIMAX_API_KEY": "SYNTHETIC"})[0]
        calls = []
        def transport(host, path, token, timeout):
            calls.append((host, path, timeout))
            if path == "/v1/token_plan/remains":
                raise provider.UsageError("endpoint_unavailable")
            return {"model_remains": [{"current_interval_remaining_percent": 20}]}
        windows, _ = provider.collect(account, 4, self.now, usage.window, transport)
        self.assertEqual(windows[0]["used_percent"], 80)
        self.assertEqual([c[0] for c in calls], ["api.minimax.io", "api.minimax.io"])
        self.assertLessEqual(calls[1][2], 4)
        for code in ("rate_limited", "timeout"):
            calls.clear()
            def refuse(*args):
                calls.append(args)
                raise provider.UsageError(code)
            with self.assertRaisesRegex(provider.UsageError, "^" + code + "$"):
                provider.collect(account, 4, self.now, usage.window, refuse)
            self.assertEqual(len(calls), 1)

    def test_minimax_legacy_endpoint_accepts_older_key_without_switching_account(self):
        account = provider.discover([], {"minimax"}, {"MINIMAX_API_KEY": "SYNTHETIC"})[0]
        calls = []
        def transport(host, path, token, timeout):
            calls.append((host, path, token))
            if path == "/v1/token_plan/remains":
                return {"base_resp": {"status_code": 1004, "status_msg": "DO_NOT_ECHO"}}
            return {"model_remains": [{"current_interval_remaining_percent": 50}]}
        windows, _ = provider.collect(account, 4, self.now, usage.window, transport)
        self.assertEqual(windows[0]["used_percent"], 50)
        self.assertEqual([c[2] for c in calls], ["SYNTHETIC", "SYNTHETIC"])
        self.assertEqual([c[0] for c in calls], ["api.minimax.io", "api.minimax.io"])
        with self.assertRaisesRegex(provider.UsageError, "^authentication_required$"):
            provider.collect(account, 4, self.now, usage.window,
                             lambda *a: (_ for _ in ()).throw(provider.UsageError("authentication_required")))

    def test_http_refuses_redirects_errors_oversize_and_invalid_json_without_echo(self):
        class Response:
            def __init__(self, status, body):
                self.status, self.body = status, io.BytesIO(body)
            def getheader(self, name):
                return None
            def read1(self, count):
                return self.body.read(count)
        for status, body, error in ((302, b"DO_NOT_ECHO", "source_error"),
                                    (401, b"DO_NOT_ECHO", "authentication_required"),
                                    (429, b"DO_NOT_ECHO", "rate_limited"),
                                    (503, b"DO_NOT_ECHO", "source_error"),
                                    (200, b"x" * (provider.MAX_BODY + 1), "response_too_large"),
                                    (200, b"DO_NOT_ECHO", "invalid_response")):
            with self.subTest(status=status, error=error), patch.object(provider.http.client, "HTTPSConnection") as conn:
                conn.return_value.getresponse.return_value = Response(status, body)
                with self.assertRaisesRegex(provider.UsageError, "^" + error + "$"):
                    provider.https_json("api.z.ai", "/fixture", "SYNTHETIC", 1)
                self.assertEqual(conn.call_count, 1)
                conn.return_value.close.assert_called_once()
        with patch.object(provider.http.client, "HTTPSConnection") as conn:
            with self.assertRaisesRegex(provider.UsageError, "^unsupported_endpoint$"):
                provider.https_json("attacker.invalid", "/fixture", "SYNTHETIC", 1)
            conn.assert_not_called()

    def test_http_timeout_is_sanitized_and_success_is_bounded(self):
        with patch.object(provider.http.client, "HTTPSConnection") as conn:
            conn.return_value.request.side_effect = TimeoutError("SYNTHETIC_SECRET")
            with self.assertRaisesRegex(provider.UsageError, "^timeout$"):
                provider.https_json("api.z.ai", "/fixture", "SYNTHETIC_SECRET", 1)
        with patch.object(provider.http.client, "HTTPSConnection") as conn:
            response = conn.return_value.getresponse.return_value
            response.status = 200
            response.getheader.return_value = str(provider.MAX_BODY + 1)
            with self.assertRaisesRegex(provider.UsageError, "^response_too_large$"):
                provider.https_json("api.z.ai", "/fixture", "SYNTHETIC", 1)
            response.read1.assert_not_called()

    def test_report_without_codexbar_preserves_separate_accounts_and_failure_sample(self):
        with tempfile.TemporaryDirectory(prefix="sb-provider-fixture-") as tmp:
            root = Path(tmp).resolve()
            state = root / "state"
            state.mkdir()
            definitions = [{"id": "zai-coding", "accounts": [
                {"id": "one", "auth": {"kind": "api_key", "env": "FIXTURE_ONE"}},
                {"id": "two", "auth": {"kind": "api_key", "env": "FIXTURE_TWO"}}]}]
            def query(command, timeout):
                if command[1:] == ["config", "providers"]:
                    return {"providers": [{"id": "zai-coding"}]}, None
                if command[1:] == ["config", "get", "providers", "--json"]:
                    return definitions, None
                self.fail("Unexpected process invocation: " + repr(command))
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock, valid_name=lambda x: bool(native.NAME.fullmatch(x)))
            args = SimpleNamespace(provider="zai", slot=None, timeout=30, refresh=True)
            def collect(account, timeout, now, make_window):
                return [make_window("5h", 20 if account["slot"].endswith("one") else 90,
                                    now + 3600, now, now, 300)], []
            with patch.object(usage, "query", side_effect=query), patch.object(provider, "collect", side_effect=collect), \
                 patch.object(Path, "home", return_value=root), patch.object(usage.shutil, "which", return_value=None), \
                 patch.dict(os.environ, {"FIXTURE_ONE": "SYNTHETIC_ONE", "FIXTURE_TWO": "SYNTHETIC_TWO"}), \
                 patch.object(usage.time, "time", return_value=self.now):
                result = usage.report(args, root, "fixture-sb", fs)
                self.assertEqual([r["windows"][0]["used_percent"] for r in result["accounts"]], [20, 90])
                self.assertNotIn("SYNTHETIC_", json.dumps(result))
                with patch.object(provider, "collect", side_effect=provider.UsageError("timeout")), \
                     patch.object(usage.time, "time", return_value=self.now + 4000):
                    failed = usage.report(args, root, "fixture-sb", fs)
                self.assertTrue(all(r["observed_at"] == usage.iso(self.now) for r in failed["accounts"]))
                self.assertTrue(all(r["refresh_error"] == "timeout" for r in failed["accounts"]))
                with patch.dict(os.environ, {"FIXTURE_ONE": "ROTATED"}):
                    args.refresh = False
                    changed = usage.report(args, root, "fixture-sb", fs)
                self.assertEqual(changed["accounts"][0]["windows"], [])
                self.assertEqual(changed["accounts"][1]["windows"][0]["used_percent"], 90)

    def test_sb_exports_fixed_keys_only_for_usage_aliases(self):
        import subprocess
        with tempfile.TemporaryDirectory(prefix="sb-usage-env-fixture-") as tmp:
            root = Path(tmp).resolve()
            (root / "manifest.json").write_text("{}")
            (root / "config").mkdir()
            (root / "config/sb.env").write_text("ZAI_API_KEY=synthetic\nMINIMAX_API_KEY=synthetic\nCUSTOM_USAGE_KEY=synthetic\n")
            binary = root / "bin"
            binary.mkdir()
            python = binary / "python3"
            python.write_text('#!/bin/zsh\n[[ -n "${ZAI_API_KEY:-}" ]] && z=yes || z=no\n[[ -n "${MINIMAX_API_KEY:-}" ]] && m=yes || m=no\n[[ -n "${CUSTOM_USAGE_KEY:-}" ]] && c=yes || c=no\nprint -r -- "$z/$m/$c"\n')
            python.chmod(0o755)
            env = dict(os.environ, HOME=str(root), SWITCHBACK_RUNTIME_ROOT=str(root),
                       PATH=str(binary) + ":/usr/bin:/bin", SB_BIN="/usr/bin/true")
            for name in ("ZAI_API_KEY", "MINIMAX_API_KEY", "CUSTOM_USAGE_KEY"):
                env.pop(name, None)
            for args, expected in ((["limits", "--json"], "yes/yes/no"),
                                   (["account", "usage", "--json"], "yes/yes/no"),
                                   (["account", "inspect"], "no/no/no")):
                result = subprocess.run(["/bin/zsh", str(CLI / "sb"), *args],
                                        env=env, capture_output=True, text=True, timeout=20)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), expected)

    def test_opencode_bridge_uses_auto_and_never_exposes_local_quota_estimates(self):
        with tempfile.TemporaryDirectory(prefix="sb-go-bridge-fixture-") as tmp:
            root = Path(tmp).resolve()
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock, valid_name=lambda x: bool(native.NAME.fullmatch(x)))
            args = SimpleNamespace(provider="opencodego", slot=None, timeout=30, refresh=True)
            def query(command, timeout):
                if command[1:] == ["config", "providers"]:
                    return {"providers": [{"id": "opencode-go-coding"}]}, None
                self.assertEqual(command[0], "codexbar")
                self.assertIn("auto", command)
                return [{"provider": "opencodego", "source": "local", "usage": {
                    "updatedAt": usage.iso(self.now), "primary": {"usedPercent": 50,
                    "windowMinutes": 300, "resetsAt": usage.iso(self.now + 300)}}}], None
            with patch.object(usage, "query", side_effect=query), patch.object(Path, "home", return_value=root), \
                 patch.object(usage.shutil, "which", return_value="/fixture/codexbar"), \
                 patch.object(usage.time, "time", return_value=self.now):
                result = usage.report(args, root, "fixture-sb", fs)
                self.assertEqual(result["accounts"][0]["windows"], [])
                self.assertIn("real quota unavailable", " ".join(result["accounts"][0]["notes"]))
                # An older cached local result must also lose its invented quota on read.
                cached = native.read_json(root / "state/account-usage.json")
                cached["accounts"][0]["windows"] = [usage.window("primary", 50, self.now + 300, self.now, self.now, 300)]
                native.write_json(root / "state/account-usage.json", cached)
                args.refresh = False
                self.assertEqual(usage.report(args, root, "fixture-sb", fs)["accounts"][0]["windows"], [])


    def test_alias_routes_with_same_credential_value_produce_one_subscription(self):
        definitions = [{"id": "zai-coding-headroom", "api_key_env": "ALIAS_KEY"},
                       {"id": "zai", "api_key_env": "Z_AI_API_KEY"},
                       {"id": "zai-coding", "accounts": [
                           {"id": "main", "auth": {"kind": "api_key", "env": "ZAI_API_KEY"}}]}]
        environment = {"ALIAS_KEY": "SAME_SYNTHETIC", "Z_AI_API_KEY": "SAME_SYNTHETIC", "ZAI_API_KEY": "SAME_SYNTHETIC"}
        accounts = provider.discover(definitions, {"zai"}, environment)
        self.assertEqual(len(accounts), 1)
        self.assertEqual(accounts[0]["slot"], "zai-coding--main")
        environment["ALIAS_KEY"] = "DISTINCT_SYNTHETIC"
        accounts = provider.discover(definitions, {"zai"}, environment)
        self.assertEqual(len(accounts), 2)
        self.assertEqual(len({a["credential_revision"] for a in accounts}), 2)

    def test_collector_to_report_hands_scope_to_aging_owner(self):
        with tempfile.TemporaryDirectory(prefix="sb-provider-scope-fixture-") as tmp:
            root = Path(tmp).resolve()
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock, valid_name=lambda x: bool(native.NAME.fullmatch(x)))
            args = SimpleNamespace(provider="zai", slot=None, timeout=30, refresh=True)
            def query(command, timeout):
                if command[1:] == ["config", "providers"]:
                    return {"providers": [{"id": "zai-coding"}]}, None
                if command[1:] == ["config", "get", "providers", "--json"]:
                    return [{"id": "zai-coding", "api_key_env": "ZAI_API_KEY"}], None
                self.fail("Unexpected process: " + repr(command))
            payload = {"success": True, "code": 200, "data": {"limits": [
                {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": 20},
                {"type": "TIME_LIMIT", "unit": 5, "number": 1, "percentage": 100}]}}
            def collect(account, timeout, now, make_window):
                return provider.parse_zai(payload, now, make_window)
            handed_to_aging = []
            original_age = usage.age_rows
            def age(rows, now):
                handed_to_aging.extend([dict(w) for row in rows for w in row.get("windows", [])])
                return original_age(rows, now)
            with patch.object(usage, "query", side_effect=query), patch.object(provider, "collect", side_effect=collect), \
                 patch.object(usage, "age_rows", side_effect=age), patch.object(Path, "home", return_value=root), \
                 patch.object(usage.shutil, "which", return_value=None), \
                 patch.dict(os.environ, {"ZAI_API_KEY": "SYNTHETIC"}):
                usage.report(args, root, "fixture-sb", fs)
            self.assertEqual([(w["scope"], w["used_percent"]) for w in handed_to_aging],
                             [("generation", 20), ("tools", 100)])



if __name__ == "__main__":
    unittest.main()
