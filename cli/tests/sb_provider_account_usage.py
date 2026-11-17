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
    now = 1_800_000_000

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
        self.assertIsNone(windows[1]["resets_at"])
        self.assertIsNone(windows[1]["reset_in_seconds"])
        self.assertEqual(windows[1]["used_percent"], 10)
        self.assertEqual(windows[1]["reset_source"], "unavailable")

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

    def test_opencode_bridge_uses_web_and_never_exposes_local_quota_estimates(self):
        with tempfile.TemporaryDirectory(prefix="sb-go-bridge-fixture-") as tmp:
            root = Path(tmp).resolve()
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock, valid_name=lambda x: bool(native.NAME.fullmatch(x)))
            args = SimpleNamespace(provider="opencodego", slot=None, timeout=30, refresh=True)
            def query(command, timeout):
                if command[1:] == ["config", "providers"]:
                    return {"providers": [{"id": "opencode-go-coding"}]}, None
                self.assertEqual(command[0], "codexbar")
                self.assertIn("web", command)
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
            with patch.object(usage, "query", side_effect=query), patch.object(provider, "collect", side_effect=collect) as collector, \
                 patch.object(Path, "home", return_value=root), patch.object(usage.shutil, "which", return_value=None), \
                 patch.dict(os.environ, {"ZAI_API_KEY": "SYNTHETIC"}):
                result = usage.report(args, root, "fixture-sb", fs)
            self.assertEqual([(w["scope"], w["used_percent"]) for w in result["accounts"][0]["windows"]],
                             [("generation", 20), ("tools", 100)])
            self.assertEqual(collector.call_count, 1)



    def test_long_dotted_account_ids_round_trip_through_report_slot_filter(self):
        with tempfile.TemporaryDirectory(prefix="sb-provider-slot-fixture-") as tmp:
            root = Path(tmp).resolve()
            ids = ["team.alpha." * 12 + suffix for suffix in ("one", "two")]
            definitions = [{"id": "zai-coding", "accounts": [
                {"id": name, "auth": {"kind": "api_key", "env": ref}}
                for name, ref in zip(ids, ("FIXTURE_ONE", "FIXTURE_TWO"))]}]
            def query(command, timeout):
                return ({"providers": [{"id": "zai-coding"}]}, None) if command[1:] == ["config", "providers"] else (definitions, None)
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock, valid_name=lambda x: bool(native.NAME.fullmatch(x)))
            args = SimpleNamespace(provider="zai", slot=None, timeout=30, refresh=False)
            calls = []
            def collect(account, timeout, now, make_window):
                calls.append(account["account_label"])
                return [make_window("5h", 25, now + 60, now, now, 300)], []
            with patch.object(usage, "query", side_effect=query), patch.object(provider, "collect", side_effect=collect), \
                 patch.object(Path, "home", return_value=root), patch.object(usage.shutil, "which", return_value=None), \
                 patch.dict(os.environ, {"FIXTURE_ONE": "SYNTHETIC_ONE", "FIXTURE_TWO": "SYNTHETIC_TWO"}):
                initial = usage.report(args, root, "fixture-sb", fs)
                slots = [row["slot"] for row in initial["accounts"]]
                self.assertEqual(len(set(slots)), 2)
                self.assertTrue(all(native.NAME.fullmatch(slot) and len(slot) <= 64 for slot in slots))
                args.slot, args.refresh = slots[1], True
                result = usage.report(args, root, "fixture-sb", fs)
            self.assertEqual(calls, [ids[1]])
            self.assertEqual(result["accounts"][0]["account_label"], ids[1])
            self.assertEqual(result["accounts"][0]["slot"], slots[1])
            self.assertEqual(len(result["accounts"]), 1)


    def test_minimax_untyped_relative_durations_never_invent_reset_times(self):
        for relative in (240_000, 600_000, 1_000_001):
            with self.subTest(relative=relative):
                raw = {"model_name": "general", "current_interval_remaining_percent": 25,
                       "remains_time": relative, "current_weekly_remaining_percent": 90,
                       "weekly_remains_time": relative}
                windows, _ = provider.parse_minimax({"model_remains": [raw]}, self.now, usage.window)
                self.assertEqual([w["used_percent"] for w in windows], [75, 10])
                self.assertTrue(all(w["resets_at"] is None and w["reset_in_seconds"] is None for w in windows))
                self.assertTrue(all(w["reset_source"] == "unavailable" for w in windows))

    def test_minimax_explicit_epoch_end_times_keep_their_units_and_expiry(self):
        raw = {"model_name": "general", "current_interval_remaining_percent": 25,
               "end_time": self.now + 60, "remains_time": 600_000,
               "current_weekly_remaining_percent": 90,
               "weekly_end_time": (self.now + 120) * 1_000, "weekly_remains_time": 600_000}
        windows, _ = provider.parse_minimax({"model_remains": [raw]}, self.now, usage.window)
        self.assertEqual([w["reset_in_seconds"] for w in windows], [60, 120])
        raw["end_time"] = self.now - 60
        raw["weekly_end_time"] = (self.now - 120) * 1_000
        windows, _ = provider.parse_minimax({"model_remains": [raw]}, self.now, usage.window)
        self.assertTrue(all(w["state"] == "reset_passed" and w["reset_in_seconds"] == 0 for w in windows))


class BalanceCollectorTests(unittest.TestCase):
    now = 1_800_000_000
    DEEPSEEK = {"is_available": True, "balance_infos": [
        {"currency": "USD", "total_balance": "19.39", "granted_balance": "0.00", "topped_up_balance": "19.39"},
        {"currency": "CNY", "total_balance": "0.00", "granted_balance": "0.00", "topped_up_balance": "0.00"}]}
    OPENROUTER = {"data": {"total_credits": 25, "total_usage": 10.5}}
    NEURALWATT = {"snapshot_at": "2027-01-15T08:00:00Z", "balance": {
        "credits_remaining_usd": 4.5, "total_credits_usd": 10.0, "credits_used_usd": 5.5,
        "accounting_method": "energy"}, "subscription": None, "key": {"name": "fixture", "allowance": None}}

    def test_deepseek_normal_reply_keeps_funded_wallets_only(self):
        windows, notes, balances = provider.parse_deepseek(self.DEEPSEEK, self.now, usage.window)
        self.assertEqual((windows, notes), ([], []))
        self.assertEqual(balances, [{"kind": "prepaid", "balance": 19.39, "currencyCode": "USD",
                                     "period": "DeepSeek balance", "grantedBalance": 0.0,
                                     "toppedUpBalance": 19.39}])
        funded = {"is_available": True, "balance_infos": [
            {**row, "total_balance": "3.50"} for row in self.DEEPSEEK["balance_infos"]]}
        _, _, balances = provider.parse_deepseek(funded, self.now, usage.window)
        self.assertEqual([(b["currencyCode"], b["balance"]) for b in balances], [("USD", 3.5), ("CNY", 3.5)])

    def test_deepseek_zero_balance_is_a_measured_zero(self):
        empty = {"is_available": False, "balance_infos": [
            {"currency": "CNY", "total_balance": "0.00", "granted_balance": "0.00", "topped_up_balance": "0.00"},
            {"currency": "USD", "total_balance": "0", "granted_balance": "0", "topped_up_balance": "0"}]}
        _, notes, balances = provider.parse_deepseek(empty, self.now, usage.window)
        self.assertEqual([(b["currencyCode"], b["balance"]) for b in balances], [("USD", 0.0)])
        self.assertEqual(notes, ["DeepSeek reports the balance cannot cover API calls."])

    def test_deepseek_malformed_reply_is_refused_not_zeroed(self):
        good = self.DEEPSEEK["balance_infos"][0]
        for payload, error in (({"balance_infos": [good]}, "invalid_response"),
                               ({"is_available": True, "balance_infos": {}}, "invalid_response"),
                               ({"is_available": True, "balance_infos": [{**good, "total_balance": "DO_NOT_ECHO"}]}, "invalid_response"),
                               ({"is_available": True, "balance_infos": [{**good, "total_balance": "nan"}]}, "invalid_response"),
                               ({"is_available": True, "balance_infos": [{**good, "granted_balance": True}]}, "invalid_response"),
                               ({"is_available": True, "balance_infos": [{**good, "currency": "usd"}]}, "invalid_response"),
                               ({"is_available": True, "balance_infos": ["x"]}, "invalid_response"),
                               ({"is_available": True, "balance_infos": []}, "no_usage_source")):
            with self.subTest(payload=payload), self.assertRaisesRegex(provider.UsageError, "^" + error + "$"):
                provider.parse_deepseek(payload, self.now, usage.window)

    def test_openrouter_normal_and_zero_balances(self):
        windows, notes, balances = provider.parse_openrouter(self.OPENROUTER, self.now, usage.window)
        self.assertEqual((windows, notes), ([], []))
        self.assertEqual(balances, [{"kind": "prepaid", "balance": 14.5, "currencyCode": "USD",
                                     "period": "OpenRouter credits", "totalCredits": 25, "totalUsage": 10.5}])
        for data in ({"total_credits": 10, "total_usage": 10.0086954}, {"total_credits": 0, "total_usage": 0}):
            with self.subTest(data=data):
                _, _, balances = provider.parse_openrouter({"data": data}, self.now, usage.window)
                self.assertEqual(balances[0]["balance"], 0)

    def test_openrouter_malformed_reply_is_refused(self):
        for payload in ({}, {"data": []}, {"data": {"total_credits": "10", "total_usage": 1}},
                        {"data": {"total_credits": 10}}, {"data": {"total_credits": True, "total_usage": 0}},
                        {"data": {"total_credits": -1, "total_usage": 0}},
                        {"data": {"total_credits": float("inf"), "total_usage": 0}}):
            with self.subTest(payload=payload), self.assertRaisesRegex(provider.UsageError, "^invalid_response$"):
                provider.parse_openrouter(payload, self.now, usage.window)

    def test_neuralwatt_normal_derived_and_zero_balances(self):
        windows, notes, balances = provider.parse_neuralwatt(self.NEURALWATT, self.now, usage.window)
        self.assertEqual((windows, notes), ([], []))
        self.assertEqual(balances, [{"kind": "prepaid", "balance": 4.5, "currencyCode": "USD",
                                     "period": "Neuralwatt prepaid balance"}])
        derived = {"balance": {"total_credits_usd": 10.0, "credits_used_usd": 7.25}}
        self.assertEqual(provider.parse_neuralwatt(derived, self.now, usage.window)[2][0]["balance"], 2.75)
        zero = {**self.NEURALWATT, "balance": {"credits_remaining_usd": 0.0, "total_credits_usd": 1.0,
                                               "credits_used_usd": 1.0}}
        self.assertEqual(provider.parse_neuralwatt(zero, self.now, usage.window)[2][0]["balance"], 0)

    def test_neuralwatt_subscription_kwh_window_keeps_its_period(self):
        start, end = usage.iso(self.now - 86400), usage.iso(self.now + 86400)
        payload = {**self.NEURALWATT, "subscription": {"kwh_included": 40, "kwh_used": 10,
                   "current_period_start": start, "current_period_end": end}}
        windows, notes, balances = provider.parse_neuralwatt(payload, self.now, usage.window)
        self.assertEqual([(w["name"], w["used_percent"], w["window_minutes"], w["reset_in_seconds"], w["scope"])
                          for w in windows], [("subscription_kwh", 25, 2880, 86400, "generation")])
        self.assertEqual(notes, ["10 / 40 kWh this period"])
        self.assertEqual(balances[0]["balance"], 4.5)

    def test_neuralwatt_malformed_reply_is_refused(self):
        for payload in ({}, {"balance": None}, {"balance": []},
                        {"balance": {"credits_remaining_usd": "4.5"}},
                        {"balance": {"credits_remaining_usd": 4.5, "total_credits_usd": "10"}},
                        {"balance": {"accounting_method": "energy"}},
                        {"balance": {"credits_remaining_usd": -1}}):
            with self.subTest(payload=payload), self.assertRaisesRegex(provider.UsageError, "^invalid_response$"):
                provider.parse_neuralwatt(payload, self.now, usage.window)

    def test_balance_families_discover_default_refs_on_fixed_hosts(self):
        environment = {"DEEPSEEK_API_KEY": "SYNTHETIC_DS", "OPENROUTER_API_KEY": "SYNTHETIC_OR",
                       "NEURALWATT_API_KEY": "SYNTHETIC_NW"}
        accounts = provider.discover([], {"deepseek", "openrouter", "neuralwatt"}, environment)
        self.assertEqual([(a["provider"], a["source"], a["credential_ref"], a["region"], a["_error"]) for a in accounts],
                         [("deepseek", "native/deepseek", "DEEPSEEK_API_KEY", "global", None),
                          ("neuralwatt", "native/neuralwatt", "NEURALWATT_API_KEY", "global", None),
                          ("openrouter", "native/openrouter", "OPENROUTER_API_KEY", "global", None)])
        configured = provider.discover([{"id": "neuralwatt-coding", "api_key_env": "NEURALWATT_API_KEY",
                                         "base_url": "https://api.neuralwatt.com/v1"}], {"neuralwatt"}, environment)
        self.assertEqual([(a["slot"], a["region"], a["_error"]) for a in configured],
                         [("neuralwatt-coding--configured", "global", None)])
        self.assertTrue(provider.DIRECT_FAMILIES >= {"deepseek", "openrouter", "neuralwatt"})

    def test_balance_collectors_map_http_errors_and_oversize_without_echo(self):
        class Response:
            def __init__(self, status, body):
                self.status, self.body = status, io.BytesIO(body)
            def getheader(self, name):
                return None
            def read1(self, count):
                return self.body.read(count)
        replies = {"deepseek": ("api.deepseek.com", "/user/balance", self.DEEPSEEK, 19.39),
                   "openrouter": ("openrouter.ai", "/api/v1/credits", self.OPENROUTER, 14.5),
                   "neuralwatt": ("api.neuralwatt.com", "/v1/quota", self.NEURALWATT, 4.5)}
        for family, (host, path, payload, amount) in replies.items():
            account = provider.discover([], {family}, {provider.DEFAULT_ENV[family]: "SYNTHETIC_SECRET"})[0]
            cases = ((401, b"DO_NOT_ECHO", "authentication_required"), (403, b"DO_NOT_ECHO", "authentication_required"),
                     (429, b"DO_NOT_ECHO", "rate_limited"), (503, b"DO_NOT_ECHO", "source_error"),
                     (200, b"x" * (provider.MAX_BODY + 1), "response_too_large"),
                     (200, b"<html>DO_NOT_ECHO", "invalid_response"))
            for status, body, error in cases:
                with self.subTest(family=family, status=status, error=error), \
                     patch.object(provider.http.client, "HTTPSConnection") as conn:
                    conn.return_value.getresponse.return_value = Response(status, body)
                    with self.assertRaisesRegex(provider.UsageError, "^" + error + "$"):
                        provider.collect(account, 2, self.now, usage.window)
                    self.assertEqual(conn.call_args.args[0], host)
                    conn.return_value.close.assert_called_once()
            with self.subTest(family=family, status=200), patch.object(provider.http.client, "HTTPSConnection") as conn:
                conn.return_value.getresponse.return_value = Response(200, json.dumps(payload).encode())
                _, _, balances = provider.collect(account, 2, self.now, usage.window)
                method, requested = conn.return_value.request.call_args.args[:2]
                self.assertEqual((conn.call_args.args[0], method, requested), (host, "GET", path))
                self.assertEqual(conn.return_value.request.call_args.kwargs["headers"]["Authorization"],
                                 "Bearer SYNTHETIC_SECRET")
                self.assertEqual(balances[0]["balance"], amount)

    def test_report_collects_balances_natively_and_never_bridges_them(self):
        with tempfile.TemporaryDirectory(prefix="sb-balance-fixture-") as tmp:
            root = Path(tmp).resolve()
            (root / "state").mkdir()
            # A CodexBar row saved before the cutover must not survive beside the native one.
            native.write_json(root / "state/account-usage.json", {"schema": usage.SCHEMA, "accounts": [
                {"provider": "deepseek", "slot": "fixture", "source": "codexbar/api", "windows": [],
                 "balances": [], "notes": ["old bridge"], "observed_at": usage.iso(self.now - 60)}], "coverage": []})
            definitions = [{"id": "deepseek", "api_key_env": "FIXTURE_DS", "base_url": "https://api.deepseek.com"},
                           {"id": "openrouter", "api_key_env": "FIXTURE_OR", "base_url": "https://openrouter.ai/api/v1"},
                           {"id": "neuralwatt-coding", "api_key_env": "FIXTURE_NW", "base_url": "https://api.neuralwatt.com/v1"}]
            def query(command, timeout):
                if command[1:] == ["config", "providers"]:
                    return {"providers": [{"id": d["id"]} for d in definitions]}, None
                if command[1:] == ["config", "get", "providers", "--json"]:
                    return definitions, None
                self.fail("Unexpected process invocation: " + repr(command[:3]))
            replies = {"/user/balance": self.DEEPSEEK, "/api/v1/credits": self.OPENROUTER, "/v1/quota": self.NEURALWATT}
            calls = []
            def transport(host, path, token, timeout):
                calls.append((host, path, token))
                return replies[path]
            real_collect = provider.collect
            def collect(account, timeout, now, make_window):
                return real_collect(account, timeout, now, make_window, transport)
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock, valid_name=lambda x: bool(native.NAME.fullmatch(x)))
            args = SimpleNamespace(provider=None, slot=None, timeout=30, refresh=True)
            with patch.object(usage, "query", side_effect=query), patch.object(provider, "collect", side_effect=collect), \
                 patch.object(Path, "home", return_value=root), \
                 patch.object(usage.shutil, "which", return_value="/fixture/codexbar"), \
                 patch.dict(os.environ, {"FIXTURE_DS": "SYNTHETIC_DS", "FIXTURE_OR": "SYNTHETIC_OR",
                                         "FIXTURE_NW": "SYNTHETIC_NW", "CODEX_HOME": str(root / "no-codex")}), \
                 patch.object(usage.time, "time", return_value=self.now):
                result = usage.report(args, root, "fixture-sb", fs)
            rows = {r["provider"]: r for r in result["accounts"]}
            self.assertEqual(sorted(rows), ["deepseek", "neuralwatt", "openrouter"])
            self.assertEqual({p: (r["source"], r["state"], r["balances"][0]["balance"]) for p, r in rows.items()},
                             {"deepseek": ("native/deepseek", "fresh", 19.39),
                              "openrouter": ("native/openrouter", "fresh", 14.5),
                              "neuralwatt": ("native/neuralwatt", "fresh", 4.5)})
            self.assertTrue(all(r["balances"][0]["currencyCode"] == "USD" and "refresh_error" not in r
                                for r in rows.values()))
            self.assertEqual(sorted(token for _, _, token in calls), ["SYNTHETIC_DS", "SYNTHETIC_NW", "SYNTHETIC_OR"])
            self.assertEqual(result["coverage"], [])
            self.assertNotIn("SYNTHETIC_", json.dumps(result))
            saved = native.read_json(root / "state/account-usage.json")
            self.assertFalse([r for r in saved["accounts"] if str(r.get("source")).startswith("codexbar/")])

    def test_sb_exports_balance_key_names_for_limits(self):
        import subprocess
        with tempfile.TemporaryDirectory(prefix="sb-balance-env-fixture-") as tmp:
            root = Path(tmp).resolve()
            (root / "manifest.json").write_text("{}")
            (root / "config").mkdir()
            (root / "config/sb.env").write_text("DEEPSEEK_API_KEY=synthetic\nOPENROUTER_API_KEY=synthetic\n"
                                                 "NEURALWATT_API_KEY=synthetic\n")
            binary = root / "bin"
            binary.mkdir()
            python = binary / "python3"
            python.write_text('#!/bin/zsh\nfor n in DEEPSEEK_API_KEY OPENROUTER_API_KEY NEURALWATT_API_KEY; do\n'
                              '  [[ -n "${(P)n:-}" ]] && print -n yes, || print -n no,\ndone\nprint\n')
            python.chmod(0o755)
            env = dict(os.environ, HOME=str(root), SWITCHBACK_RUNTIME_ROOT=str(root),
                       PATH=str(binary) + ":/usr/bin:/bin", SB_BIN="/usr/bin/true")
            for name in ("DEEPSEEK_API_KEY", "OPENROUTER_API_KEY", "NEURALWATT_API_KEY"):
                env.pop(name, None)
            for args, expected in ((["limits", "--json"], "yes,yes,yes,"), (["account", "inspect"], "no,no,no,")):
                result = subprocess.run(["/bin/zsh", str(CLI / "sb"), *args],
                                        env=env, capture_output=True, text=True, timeout=20)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), expected)


if __name__ == "__main__":
    unittest.main()
