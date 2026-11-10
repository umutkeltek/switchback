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
            args = SimpleNamespace(provider="claude", slot="personal", timeout=30, refresh=False)
            with patch.object(usage, "query", return_value=({"providers": []}, None)), \
                 patch.object(Path, "home", return_value=root):
                row = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                self.assertEqual(row["slot"], "personal")
                self.assertEqual(row["windows"][0]["used_percent"], 20)
                self.assertNotIn("refresh_error", row)

    def test_rejected_login_and_unsuccessful_refresh_are_visible_with_saved_usage(self):
        for rejected in (False, True):
            with self.subTest(rejected=rejected), tempfile.TemporaryDirectory(prefix="sb-usage-fixture-") as tmp:
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
                with patch.object(usage, "query", side_effect=query), patch.object(Path, "home", return_value=root), \
                     patch.object(usage.time, "time", return_value=self.now):
                    row = usage.report(args, root, "fixture-sb", fs)["accounts"][0]
                self.assertEqual(row["relogin_required"], rejected)
                self.assertEqual(row["refresh_error"], "authentication_required" if rejected else "native_usage_unavailable")
                self.assertEqual(row["observed_at"], usage.iso(self.now))


if __name__ == "__main__":
    unittest.main()
