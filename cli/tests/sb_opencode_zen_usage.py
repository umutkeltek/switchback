#!/usr/bin/env python3
"""Synthetic Go quota / Zen balance projection and shared-collector tests."""
import copy
import importlib.util
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
import native_account_usage as usage
spec = importlib.util.spec_from_file_location("zen_native_fixture", CLI / "native-accounts.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class GoZenTests(unittest.TestCase):
    now = 1_800_000_000

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sb-go-zen-fixture-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.fs = SimpleNamespace(read_json=native.read_json, write_json=native.write_json,
            safe_dir=native.safe_dir, lock=native.lock,
            valid_name=lambda x: isinstance(x, str) and bool(native.NAME.fullmatch(x)))
        self.calls = []
        self.error = None
        self.payload = self.wire()
        self.now_value = self.now
        self.context = [
            patch.object(usage, "query", side_effect=self.query),
            patch.object(usage.shutil, "which", return_value="/fixture/codexbar"),
            patch.object(Path, "home", return_value=self.root),
            patch.dict(os.environ, {"CODEX_HOME": str(self.root / "missing-codex")}),
            patch.object(usage.time, "time", side_effect=lambda: self.now_value)]
        for context in self.context:
            context.start()
            self.addCleanup(context.stop)

    def wire(self, balance=12.5, sampled=None):
        return [{"provider": "opencodego", "account": "configured-seat", "source": "web",
                 "usage": {"updatedAt": usage.iso(self.now),
                     "primary": {"usedPercent": 25, "windowMinutes": 300,
                                 "resetsAt": usage.iso(self.now + 600)},
                     "providerCost": {"used": balance, "limit": 0, "currencyCode": "USD",
                         "period": "Zen balance", "updatedAt": usage.iso(self.now if sampled is None else sampled)}}}]

    def query(self, command, timeout):
        if command[1:] == ["config", "providers"]:
            return {"providers": [{"id": "opencode-go-coding"}]}, None
        self.assertEqual(command[0], "codexbar", repr(command))
        self.assertEqual(command[command.index("--provider") + 1], "opencodego")
        self.assertEqual(command[command.index("--source") + 1], "web")
        self.assertNotIn("--no-credits", command)
        self.calls.append(command)
        return (None, self.error) if self.error else (copy.deepcopy(self.payload), None)

    def run_report(self, provider="all", refresh=False, scheduled=False):
        args = SimpleNamespace(provider=provider, slot=None, timeout=30, refresh=refresh,
                               refresh_if_due=scheduled)
        return usage.report(args, self.root, "fixture-sb", self.fs)

    def services(self, result):
        return {row["provider"]: row for row in result["accounts"]}

    def test_go_quota_and_zen_balance_are_distinct_pure_views_of_one_saved_row(self):
        result = self.run_report(refresh=True)
        services = self.services(result)
        self.assertEqual(set(services), {"opencodego", "opencodezen"})
        self.assertEqual(services["opencodego"]["windows"][0]["used_percent"], 25)
        self.assertEqual(services["opencodego"]["balances"], [])
        zen = services["opencodezen"]
        self.assertEqual(zen["windows"], [])
        self.assertEqual(zen["slot"], services["opencodego"]["slot"])
        self.assertEqual(zen["balances"][0]["kind"], "prepaid")
        self.assertEqual(zen["balances"][0]["balance"], 12.5)
        self.assertEqual(zen["balances"][0]["currencyCode"], "USD")
        self.assertEqual(zen["observed_at"], usage.iso(self.now))
        self.assertNotIn("account_id", zen)
        self.assertIn("unverified", " ".join(zen["notes"]))
        cached = native.read_json(self.root / "state/account-usage.json")
        self.assertEqual([row["provider"] for row in cached["accounts"]], ["opencodego"])
        raw = cached["accounts"][0]
        self.assertEqual(raw["balances"][0]["sampled_at"], usage.iso(self.now))
        self.assertNotIn("collector_observation", raw)
        self.assertNotIn("balances", zen["collector_observation"])
        self.assertEqual(len(self.calls), 1)

    def test_zero_balance_is_measured_and_never_becomes_a_percentage(self):
        self.payload = self.wire(balance=0)
        zen = self.services(self.run_report(refresh=True))["opencodezen"]
        self.assertEqual(zen["state"], "fresh")
        self.assertEqual(zen["balances"][0]["balance"], 0)
        self.assertEqual(zen["windows"], [])
        self.assertNotIn("used", zen["balances"][0])
        self.assertNotIn("used_percent", zen["balances"][0])

    def test_strict_wire_contract_rejects_near_matches_and_nonfinite_values(self):
        for change in ({"period": "Monthly"}, {"currencyCode": "EUR"}, {"limit": 1},
                       {"limit": False}, {"used": True}, {"used": "12.5"},
                       {"used": float("nan")}, {"used": float("inf")}):
            with self.subTest(change=change):
                wire = self.wire()
                wire[0]["usage"]["providerCost"].update(change)
                row = usage.codexbar_rows(wire, self.now)[0]
                self.assertFalse(any(usage.is_zen_prepaid(b) for b in row["balances"]))
                projected = usage.service_projection({"accounts": [row], "coverage": []}, "all", self.now)
                zen = self.services(projected)["opencodezen"]
                self.assertEqual(zen["balances"], [])
                self.assertEqual(zen["state"], "unavailable")
        wire = self.wire()
        wire[0]["provider"] = "opencode"
        row = usage.codexbar_rows(wire, self.now)[0]
        self.assertFalse(any(usage.is_zen_prepaid(b) for b in row["balances"]))
        self.assertNotIn("opencodezen", self.services(
            usage.service_projection({"accounts": [row], "coverage": []}, "all", self.now)))

    def test_absent_optional_balance_is_unavailable_without_zero_or_go_timestamp(self):
        del self.payload[0]["usage"]["providerCost"]
        services = self.services(self.run_report(refresh=True))
        self.assertEqual(services["opencodego"]["state"], "fresh")
        zen = services["opencodezen"]
        self.assertEqual(zen["balances"], [])
        self.assertIsNone(zen["observed_at"])
        self.assertEqual(zen["refresh_error"], "optional_enrichment_unavailable")
        self.assertEqual(zen["state"], "unavailable")

    def test_missing_balance_timestamp_never_borrows_fresh_go_sample(self):
        del self.payload[0]["usage"]["providerCost"]["updatedAt"]
        services = self.services(self.run_report(refresh=True))
        self.assertEqual(services["opencodego"]["state"], "fresh")
        zen = services["opencodezen"]
        self.assertIsNone(zen["observed_at"])
        self.assertIsNone(zen["balances"][0]["sampled_at"])
        self.assertEqual(zen["refresh_error"], "balance_timestamp_unavailable")
        self.assertEqual(zen["state"], "unavailable")

    def test_balance_timestamp_ages_independently_from_fresh_go_quota(self):
        sampled = self.now - 4_000
        self.payload = self.wire(sampled=sampled)
        services = self.services(self.run_report(refresh=True))
        self.assertEqual(services["opencodego"]["state"], "fresh")
        zen = services["opencodezen"]
        self.assertEqual(zen["observed_at"], usage.iso(sampled))
        self.assertEqual(zen["state"], "stale")
        self.assertNotIn("refresh_error", zen)

    def test_missing_optional_enrichment_preserves_previous_dated_balance_as_unavailable(self):
        self.payload = self.wire(sampled=self.now - 4_000)
        self.run_report(refresh=True)
        self.now_value += 30
        self.payload = self.wire()
        self.payload[0]["usage"]["updatedAt"] = usage.iso(self.now_value)
        del self.payload[0]["usage"]["providerCost"]
        services = self.services(self.run_report(refresh=True))
        self.assertEqual(services["opencodego"]["observed_at"], usage.iso(self.now_value))
        zen = services["opencodezen"]
        self.assertEqual(zen["balances"][0]["balance"], 12.5)
        self.assertEqual(zen["observed_at"], usage.iso(self.now - 4_000))
        self.assertEqual(zen["state"], "unavailable")
        self.assertEqual(zen["refresh_error"], "optional_enrichment_unavailable")

    def test_go_transport_error_overrides_zen_availability_without_signin_claim(self):
        self.run_report(refresh=True)
        self.now_value += 60
        self.error = "source_error"
        zen = self.services(self.run_report(refresh=True))["opencodezen"]
        self.assertEqual(zen["balances"][0]["balance"], 12.5)
        self.assertEqual(zen["observed_at"], usage.iso(self.now))
        self.assertEqual(zen["state"], "unavailable")
        self.assertEqual(zen["refresh_error"], "source_error")
        self.assertNotIn("sign in", " ".join(zen["notes"]).lower())

    def test_known_workspace_is_a_reference_and_changed_reference_drops_old_balance(self):
        config = self.root / ".codexbar"
        config.mkdir()
        native.write_json(config / "config.json", {"providers": [
            {"id": "opencodego", "enabled": True, "workspaceID": "wrk_fixture_one"}]})
        zen = self.services(self.run_report(refresh=True))["opencodezen"]
        self.assertEqual(zen["workspace_ref"], "wrk_fixture_one")
        self.assertNotIn("account_id", zen)
        native.write_json(config / "config.json", {"providers": [
            {"id": "opencodego", "enabled": True, "workspaceID": "wrk_fixture_two"}]})
        del self.payload[0]["usage"]["providerCost"]
        zen = self.services(self.run_report(refresh=True))["opencodezen"]
        self.assertEqual(zen["workspace_ref"], "wrk_fixture_two")
        self.assertEqual(zen["balances"], [])

    def test_alias_filters_share_one_collector_binding_and_no_second_cache_row(self):
        self.run_report(refresh=True)
        all_result = self.run_report()
        go = self.run_report(provider="opencodego")
        zen = self.run_report(provider="opencodezen")
        alias = self.run_report(provider="opencode-zen")
        self.assertEqual(set(self.services(go)), {"opencodego"})
        self.assertEqual(set(self.services(zen)), {"opencodezen"})
        self.assertEqual(zen["accounts"], alias["accounts"])
        groups = [usage.refresh_groups(result) for result in (all_result, go, zen, alias)]
        self.assertTrue(all(set(group) == {"opencodego/*"} for group in groups))
        self.assertTrue(all(len(group["opencodego/*"]) == 1 for group in groups))
        bindings = [usage.refresh_binding(group["opencodego/*"]) for group in groups]
        self.assertEqual(len(set(bindings)), 1)
        self.assertEqual(len(self.calls), 1)
        # An explicit Zen refresh queries and saves only its backing Go collector.
        self.run_report(provider="opencode-zen", refresh=True)
        self.assertEqual(len(self.calls), 2)
        cached = native.read_json(self.root / "state/account-usage.json")
        self.assertEqual([row["provider"] for row in cached["accounts"]], ["opencodego"])

    def test_zen_coverage_maps_to_the_existing_go_collector(self):
        snapshot = {"accounts": [], "coverage": [
            {"provider": "opencodego", "state": "unavailable", "reason": "source_error"}]}
        all_result = usage.service_projection(snapshot, "all", self.now)
        zen = usage.service_projection(snapshot, "opencodezen", self.now)
        self.assertEqual(set(usage.refresh_groups(all_result)), {"opencodego/*"})
        self.assertEqual(set(usage.refresh_groups(zen)), {"opencodego/*"})
        self.assertEqual([x["provider"] for x in zen["coverage"]], ["opencodezen"])


if __name__ == "__main__":
    unittest.main()
