#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

CLI = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(CLI))
import native_account_usage as usage

spec = importlib.util.spec_from_file_location("native_accounts", CLI / "native-accounts.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class UsageTests(unittest.TestCase):
    def setUp(self):
        self.now = 1_800_000_000
        self.cache = {"cachedUsageUtilization": {"fetchedAtMs": self.now * 1000,
            "accountUuid": "account-one", "utilization": {
                "five_hour": {"utilization": 20, "resets_at": usage.iso(self.now + 3600)},
                "seven_day": {"utilization": 100, "resets_at": usage.iso(self.now + 86400)},
                "seven_day_breakdown": {"rows": []}}}}

    def test_identity_mismatch_never_borrows_another_accounts_quota(self):
        self.assertEqual(usage.claude_windows(self.cache, "account-two", self.now), [])

    def test_resets_usage_and_countdown_remain_distinct_per_window(self):
        windows = usage.claude_windows(self.cache, "account-one", self.now)
        self.assertEqual(len(windows), 2)
        self.assertEqual(windows[0]["remaining_percent"], 80)
        self.assertEqual(windows[1]["remaining_percent"], 0)
        self.assertEqual(windows[0]["reset_in_seconds"], 3600)
        self.assertEqual(windows[1]["reset_in_seconds"], 86400)

    def test_stale_and_expired_windows_are_not_current_capacity(self):
        windows = usage.claude_windows(self.cache, "account-one", self.now + 4000)
        self.assertEqual(windows[0]["state"], "reset_passed")
        self.assertEqual(windows[1]["state"], "stale")
        self.assertIn("stale", usage.summary(windows))

    def test_missing_malformed_and_future_samples_do_not_become_zero_usage(self):
        for value in (None, True, "20", float('nan'), float('inf'), -1, 101):
            row = usage.window("primary", value, None, self.now, self.now)
            self.assertIsNone(row["remaining_percent"])
            self.assertEqual(row["state"], "unknown")
        row = usage.window("primary", 10, None, self.now + 3600, self.now)
        self.assertEqual(row["state"], "unknown")

    def test_codexbar_balance_placeholder_is_not_unlimited_capacity(self):
        payload = [{"provider": "fixture", "usage": {"updatedAt": usage.iso(self.now),
            "primary": {"usedPercent": 0, "resetDescription": "$12.34 balance"},
            "providerCost": {"used": 2, "limit": 14, "currencyCode": "USD", "token": "never-output"},
            "accessToken": "never-output"}}]
        rows = usage.codexbar_rows(payload, self.now)
        self.assertEqual(rows[0]["windows"], [])
        self.assertEqual(rows[0]["notes"], ["$12.34 balance"])
        self.assertNotIn("never-output", json.dumps(rows))

    def test_codexbar_keeps_each_token_account_separate(self):
        payload = [{"provider": "fixture", "account": name, "usage": {
            "updatedAt": usage.iso(self.now), "primary": {"usedPercent": percent,
            "windowMinutes": 300, "resetsAt": usage.iso(self.now + 3600)}}}
            for name, percent in (("first", 15), ("second", 90))]
        rows = usage.codexbar_rows(payload, self.now)
        self.assertEqual([r["slot"] for r in rows], ["first", "second"])
        self.assertEqual([r["windows"][0]["remaining_percent"] for r in rows], [85, 10])

    def test_refresh_failure_preserves_last_sample_without_freshening_it(self):
        with tempfile.TemporaryDirectory(prefix="sb-usage-fixture-") as tmp:
            root = Path(tmp).resolve()
            state = root / "state"
            state.mkdir()
            sampled = self.now - 3600
            previous = {"provider": "fixture", "slot": "one", "source": "codexbar/api",
                        "observed_at": usage.iso(sampled),
                        "windows": [usage.window("primary", 70, self.now + 3600, sampled, self.now, 300)]}
            native.write_json(state / "account-usage.json", {"schema": usage.SCHEMA, "accounts": [previous]})
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock,
                valid_name=lambda x: isinstance(x, str) and native.NAME.fullmatch(x) is not None)
            args = SimpleNamespace(provider="fixture", slot=None, timeout=30, refresh=True)
            def query(command, timeout):
                if command[1:3] == ["config", "providers"]:
                    return {"providers": [{"id": "fixture"}]}, None
                return None, "timeout"
            with patch.object(usage, "query", side_effect=query), patch.object(usage.time, "time", return_value=self.now), \
                    patch.object(usage.shutil, "which", return_value="/fixture/bin"), patch.object(Path, "home", return_value=root):
                result = usage.report(args, root, "fixture-sb", fs)
            row = result["accounts"][0]
            self.assertEqual(row["refresh_error"], "timeout")
            self.assertEqual(row["observed_at"], usage.iso(sampled))
            self.assertEqual(row["windows"][0]["state"], "stale")
            self.assertEqual(row["windows"][0]["remaining_percent"], 30)

    def test_default_codex_cache_does_not_follow_a_changed_login_or_home(self):
        with tempfile.TemporaryDirectory(prefix="sb-usage-fixture-") as tmp:
            root = Path(tmp).resolve()
            home = root / ".codex"
            home.mkdir()
            auth = home / "auth.json"
            auth.write_text("synthetic login one")
            state = root / "state"
            state.mkdir()
            previous = {"provider": "codex", "slot": "default", "source": "native/codex",
                        "login_revision": usage.login_revision(home), "account_id": {"value": "one"},
                        "observed_at": usage.iso(self.now),
                        "windows": [usage.window("primary", 70, self.now + 3600, self.now, self.now, 300)]}
            native.write_json(state / "account-usage.json", {"schema": usage.SCHEMA, "accounts": [previous]})
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                                 safe_dir=native.safe_dir, lock=native.lock,
                                 valid_name=lambda x: isinstance(x, str) and native.NAME.fullmatch(x) is not None)
            args = SimpleNamespace(provider="codex", slot="default", timeout=30, refresh=False)
            with patch.object(usage, "query", return_value=({"providers": []}, None)), \
                 patch.object(Path, "home", return_value=root), \
                 patch.dict(usage.os.environ, {"CODEX_HOME": str(home)}):
                unchanged = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                self.assertEqual(unchanged["windows"][0]["used_percent"], 70)
                auth.write_text("different synthetic login")
                changed = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                self.assertEqual(changed["windows"], [])
                self.assertNotIn("account_id", changed)
                other = root / "other-codex"
                other.mkdir()
                (other / "auth.json").write_text("synthetic login one")
                with patch.dict(usage.os.environ, {"CODEX_HOME": str(other)}):
                    changed = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                    self.assertEqual(changed["windows"], [])

    def test_codex_history_uses_latest_per_account_window_and_ignores_future(self):
        with tempfile.TemporaryDirectory(prefix="sb-usage-fixture-") as tmp:
            path = Path(tmp) / "history.jsonl"
            rows = [{"provider": "codex", "accountKey": account, "windowKind": "primary",
                     "sampledAt": usage.iso(at), "usedPercent": percent, "windowMinutes": 300}
                    for account, at, percent in (("one", self.now - 60, 20), ("one", self.now, 30),
                                                 ("one", self.now + 3600, 90), ("two", self.now, 75))]
            path.write_text(''.join(json.dumps(x) + '\n' for x in rows))
            result = usage.history_rows(path, self.now)
            self.assertEqual([(r["slot"], r["windows"][0]["used_percent"]) for r in result],
                             [("one", 30), ("two", 75)])

    def test_malformed_collector_metadata_is_not_a_traceback(self):
        self.assertEqual(usage.account_rows({"accounts": None}), [])
        self.assertEqual(usage.account_rows({"accounts": [None, "invalid", {"id": "ok"}]}), [{"id": "ok"}])
        self.assertFalse(usage.number(10 ** 1000))

    def test_codex_login_change_during_refresh_discards_usage(self):
        import native_codex_account as codex
        with patch.object(codex, "App") as app_type:
            app = app_type.return_value
            app.call.side_effect = [{},
                {"account": {"type": "chatgpt"}, "workspaceRouting": {"chatgptAccountId": "one"}},
                {"rateLimits": {"primary": {"usedPercent": 20}}, "ordinaryUsageAllowed": True},
                {"account": {"type": "chatgpt"}, "workspaceRouting": {"chatgptAccountId": "two"}}]
            with self.assertRaisesRegex(usage.Refusal, "account changed"):
                codex.inspect_account(Path("/fixture"), {}, "fixture-codex")
            app.close.assert_called_once()

    def test_enrolled_default_claude_reads_its_native_metadata_home(self):
        with tempfile.TemporaryDirectory(prefix="sb-usage-fixture-") as tmp:
            root = Path(tmp).resolve()
            (root / ".claude").mkdir()
            slot = root / "config/native-account-slots/claude/personal"
            slot.mkdir(parents=True)
            native.write_json(slot / "slot.json", {"provider": "claude", "slot": "personal",
                "native_home": {"kind": "existing_default"},
                "identity": {"account_uuid": "account-one", "organization_uuid": "organization-one"}})
            native.write_json(root / ".claude.json", {**self.cache, "oauthAccount": {
                "accountUuid": "account-one", "organizationUuid": "organization-one"}})
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                                 safe_dir=native.safe_dir, lock=native.lock,
                                 valid_name=lambda x: isinstance(x, str) and native.NAME.fullmatch(x) is not None)
            args = SimpleNamespace(provider="claude", slot="personal", timeout=30, refresh=True)
            with patch.object(usage, "query", return_value=({"providers": []}, None)), \
                 patch.object(Path, "home", return_value=root):
                row = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                self.assertEqual(row["slot"], "personal")
                self.assertEqual(row["windows"][0]["used_percent"], 20)
                self.assertNotIn("refresh_error", row)

    def test_rejected_login_and_unsuccessful_refresh_are_visible_with_saved_usage(self):
        for rejected, login in ((False, False), (True, False), (False, True), (True, True)):
            with self.subTest(rejected=rejected, login=login), tempfile.TemporaryDirectory(prefix="sb-usage-fixture-") as tmp:
                root = Path(tmp).resolve()
                slot = root / "config/native-account-slots/claude/one"
                slot.mkdir(parents=True)
                meta = {"provider": "claude", "slot": "one", "enabled": True,
                        "identity": {"account_uuid": "account-one", "organization_uuid": "org"}}
                if rejected:
                    meta["cooldown"] = {"reason": "authentication_failed", "reset": None}
                native.write_json(slot / "slot.json", meta)
                home = root / "config/claude/one"
                home.mkdir(parents=True)
                native.write_json(home / ".claude.json", {**self.cache, "oauthAccount": {
                    "accountUuid": "account-one", "organizationUuid": "org"}})
                fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                    safe_dir=native.safe_dir, lock=native.lock,
                    valid_name=lambda x: isinstance(x, str) and native.NAME.fullmatch(x) is not None)
                args = SimpleNamespace(provider="claude", slot="one", timeout=30, refresh=True)
                def query(command, timeout):
                    return ({"quota_error": "refresh_failed_or_invalid_cache"}, None) if command[1] == "account" else ({"providers": []}, None)
                other = {"provider": "codex", "slot": "other", "windows": [], "observed_at": None}
                fs.safe_dir(root / "state", create=True)
                native.write_json(root / "state/account-usage.json", {"schema": usage.SCHEMA, "accounts": [other]})
                with patch.object(usage, "query", side_effect=query) as probe, patch.object(Path, "home", return_value=root), \
                     patch.object(usage.time, "time", return_value=self.now):
                    if login:
                        result = usage.publish_claude_login(root, "fixture-sb", fs, "one",
                            {"quota_error": "refresh_failed_or_invalid_cache"})
                        self.assertFalse(any(call.args[0][1] == "account" for call in probe.call_args_list))
                    else:
                        result = usage.report(args, root, "fixture-sb", fs)
                    row = result["accounts"][0]
                saved = native.read_json(root / "state/account-usage.json")
                self.assertIn(other, saved["accounts"])
                self.assertIn(row, saved["accounts"])
                self.assertEqual(row["relogin_required"], rejected)
                self.assertEqual(row["refresh_error"], "authentication_required" if rejected else "native_usage_unavailable")
                self.assertEqual(row["observed_at"], usage.iso(self.now))

    def test_saved_claude_usage_comes_from_published_snapshot(self):
        with tempfile.TemporaryDirectory(prefix="sb-usage-fixture-") as tmp:
            root = Path(tmp).resolve()
            slot = root / "config/native-account-slots/claude/one"
            slot.mkdir(parents=True)
            identity = {"account_uuid": "account-one", "organization_uuid": "org"}
            native.write_json(slot / "slot.json", {"provider": "claude", "slot": "one",
                "enabled": True, "identity": identity})
            home = root / "config/claude/one"
            home.mkdir(parents=True)
            native.write_json(home / ".claude.json", {**self.cache, "oauthAccount": {
                "accountUuid": "account-one", "organizationUuid": "org"}})
            (root / "state").mkdir()
            previous = {"provider": "claude", "slot": "one", "source": "native/claude",
                "account_id": identity, "observed_at": usage.iso(self.now - 60),
                "windows": [usage.window("five_hour", 70, self.now + 3600, self.now - 60, self.now, 300)]}
            cache_path = root / "state/account-usage.json"
            native.write_json(cache_path, {"schema": usage.SCHEMA, "accounts": [previous]})
            before = cache_path.read_bytes()
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock,
                valid_name=lambda x: isinstance(x, str) and native.NAME.fullmatch(x) is not None)
            args = SimpleNamespace(provider="claude", slot="one", timeout=30, refresh=False)
            with patch.object(usage, "query", return_value=({"providers": []}, None)), \
                 patch.object(Path, "home", return_value=root), \
                 patch.object(usage.time, "time", return_value=self.now):
                row = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                self.assertEqual(row["windows"][0]["used_percent"], 70)
                self.assertEqual(row["observed_at"], usage.iso(self.now - 60))
                self.assertEqual(cache_path.read_bytes(), before)
                native.write_json(home / ".claude.json", {**self.cache, "oauthAccount": {
                    "accountUuid": "different-account", "organizationUuid": "org"}})
                row = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                self.assertEqual(row["windows"], [])
                self.assertEqual(row["refresh_error"], "identity_mismatch")

    def test_scope_and_estimate_metadata_survive_window_aging(self):
        observed = usage.window("monthly_mcp", 100, self.now + 86400, self.now, self.now)
        observed.update(scope="tools", estimated=True, usage_kind="estimated", model="fixture-model")
        row = usage.age_rows([{"provider": "fixture", "slot": "one",
            "observed_at": usage.iso(self.now), "windows": [observed]}], self.now)[0]
        self.assertEqual(row["windows"][0]["scope"], "tools")
        self.assertEqual(row["windows"][0]["usage_kind"], "estimated")
        self.assertTrue(row["windows"][0]["estimated"])
        self.assertEqual(row["windows"][0]["model"], "fixture-model")

    def recorded(self, utilization):
        """A cachedUsageUtilization block in the shape Claude Code records, with synthetic values."""
        empty = {key: None for key in ("seven_day_oauth_apps", "seven_day_opus", "seven_day_sonnet",
                                       "seven_day_cowork", "pace", "indicator")}
        return {"cachedUsageUtilization": {"fetchedAtMs": self.now * 1000, "accountUuid": "account-one",
            "utilization": {**empty, "extra_usage": {"is_enabled": False, "utilization": None},
                            "seven_day_breakdown": {"rows": [{"key": "chat", "percent": 0}]}, **utilization}}}

    def limit(self, kind, percent, reset, model=None):
        scope = {"model": {"id": None, "display_name": model}, "surface": None} if model else None
        return {"kind": kind, "group": "session" if kind == "session" else "weekly", "percent": percent,
                "severity": "normal", "resets_at": usage.iso(self.now + reset), "scope": scope, "is_active": False}

    def test_model_scoped_weekly_limit_becomes_its_own_named_window(self):
        data = self.recorded({
            "five_hour": {"utilization": 20, "resets_at": usage.iso(self.now + 3600)},
            "seven_day": {"utilization": 59, "resets_at": usage.iso(self.now + 86400)},
            "limits": [self.limit("session", 20, 3600), self.limit("weekly_all", 59, 86400),
                       self.limit("weekly_scoped", 73, 86399, "Fable")],
            "weekly_scoped_shares": [{"limit_index": 2, "used_percent_of_weekly": 37}]})
        windows = usage.claude_windows(data, "account-one", self.now)
        self.assertEqual([w["name"] for w in windows], ["five_hour", "seven_day", "seven_day_fable"])
        fable = windows[2]
        self.assertEqual((fable["model"], fable["limit_kind"]), ("Fable", "weekly_scoped"))
        self.assertEqual((fable["used_percent"], fable["remaining_percent"]), (73, 27))
        self.assertEqual((fable["window_minutes"], fable["state"]), (10080, "fresh"))
        # Account-wide rows are not counted twice: the limits copies of 5-hour and weekly add nothing.
        self.assertEqual(sum(1 for w in windows if w["name"] == "seven_day"), 1)
        # Model and kind survive the aging pass that every published row goes through.
        row = usage.age_rows([{"provider": "claude", "slot": "one", "observed_at": usage.iso(self.now),
                               "windows": windows}], self.now)[0]
        self.assertEqual((row["windows"][2]["model"], row["windows"][2]["limit_kind"]), ("Fable", "weekly_scoped"))

    def test_used_up_model_limit_without_a_session_keeps_five_hour_unknown(self):
        data = self.recorded({"five_hour": None,
            "seven_day": {"utilization": 89, "resets_at": usage.iso(self.now + 86400)},
            "limits": [self.limit("weekly_all", 89, 86400), self.limit("weekly_scoped", 100, 86400, "Fable")]})
        windows = {w["name"]: w for w in usage.claude_windows(data, "account-one", self.now)}
        self.assertIsNone(windows["five_hour"]["used_percent"])
        self.assertEqual(windows["five_hour"]["state"], "unknown")
        self.assertEqual(windows["seven_day_fable"]["remaining_percent"], 0)

    def test_limits_fill_missing_named_windows_and_win_over_legacy_model_keys(self):
        data = self.recorded({"five_hour": None, "seven_day": None,
            "seven_day_opus": {"utilization": 10, "resets_at": usage.iso(self.now + 86400)},
            "limits": [self.limit("session", 30, 3600), self.limit("weekly_all", 40, 86400),
                       self.limit("weekly_scoped", 50, 86400, "Opus"),
                       self.limit("weekly_scoped", 60, 86400, "Claude Next 2.1"),
                       self.limit("weekly_scoped", 70, 86400, "")]})
        windows = {w["name"]: w for w in usage.claude_windows(data, "account-one", self.now)}
        self.assertEqual(set(windows), {"five_hour", "seven_day", "seven_day_opus", "seven_day_claude_next_2_1"})
        self.assertEqual((windows["five_hour"]["used_percent"], windows["seven_day"]["used_percent"]), (30, 40))
        self.assertEqual((windows["seven_day_opus"]["used_percent"], windows["seven_day_opus"]["model"]), (50, "Opus"))
        self.assertEqual(windows["seven_day_claude_next_2_1"]["model"], "Claude Next 2.1")

    def test_malformed_limits_never_invent_a_window(self):
        for limits in ("nope", [None, 3, {"kind": "weekly_scoped", "percent": 120,
                       "scope": {"model": {"display_name": "Fable"}}}]):
            data = self.recorded({"five_hour": {"utilization": 5, "resets_at": usage.iso(self.now + 60)},
                                  "seven_day": {"utilization": 6, "resets_at": usage.iso(self.now + 60)},
                                  "limits": limits})
            windows = usage.claude_windows(data, "account-one", self.now)
            extra = [w for w in windows if w["name"] not in ("five_hour", "seven_day")]
            self.assertTrue(all(w["used_percent"] is None and w["state"] == "unknown" for w in extra), extra)

    def test_family_without_a_usage_source_is_reported_once_and_never_scheduled(self):
        with tempfile.TemporaryDirectory(prefix="sb-usage-source-fixture-") as tmp:
            root = Path(tmp).resolve()
            (root / ".codexbar").mkdir()
            native.write_json(root / ".codexbar/config.json", {"providers": [
                {"id": "fixturebridge", "enabled": True}, {"id": "fixtureoff", "enabled": False}]})
            fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
                safe_dir=native.safe_dir, lock=native.lock, valid_name=lambda x: bool(native.NAME.fullmatch(x)))
            args = SimpleNamespace(provider="all", slot=None, timeout=30, refresh=True)
            bridged = []

            def query(command, timeout):
                if command[1:] == ["config", "providers"]:
                    return {"providers": [{"id": "gatewayonly"}, {"id": "fixtureoff"}, {"id": "fixturebridge"}]}, None
                self.assertEqual(command[0], "codexbar")
                bridged.append(command[command.index("--provider") + 1])
                return [{"provider": "fixturebridge", "usage": {"updatedAt": usage.iso(self.now),
                         "primary": {"usedPercent": 10, "windowMinutes": 300,
                                     "resetsAt": usage.iso(self.now + 300)}}}], None
            with patch.object(usage, "query", side_effect=query), patch.object(Path, "home", return_value=root), \
                 patch.object(usage.shutil, "which", return_value="/fixture/codexbar"), \
                 patch.object(usage.time, "time", return_value=self.now):
                result = usage.report(args, root, "fixture-sb", fs)
            self.assertEqual(bridged, ["fixturebridge"])
            coverage = {row["provider"]: row["reason"] for row in result["coverage"]}
            self.assertEqual(coverage, {"fixtureoff": usage.NO_COLLECTOR, "gatewayonly": usage.NO_COLLECTOR})
            self.assertEqual(set(usage.refresh_groups(result)), {"fixturebridge/*"})


if __name__ == "__main__":
    unittest.main()
