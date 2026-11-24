#!/usr/bin/env python3
"""Credit grant meter: fixture grants file + fixture usage store + fixture registry, all made-up numbers."""
from copy import deepcopy
from datetime import datetime, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import sqlite3
import stat
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

CLI = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(CLI))
import credit_grants as credit
import native_account_usage as usage

spec = importlib.util.spec_from_file_location("native_accounts_credit", CLI / "native-accounts.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


def at(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


NOW = at("2026-01-11T00:00:00Z")          # day 10 of a 30-day grant
READING = "2026-01-10T00:00:00Z"
READING_MS = int(at(READING) * 1000)
GRANT = {"provider": "openai", "credential_ref": "OPENAI_API_KEY", "amount_usd": 100.0,
         "received_at": "2026-01-01", "expires_at": "2026-01-31", "observed_balance_usd": 90.0,
         "observed_at": READING, "observed_from": "fixture page", "reserve_usd": 10.0, "renews": "monthly"}
# USD per 1M tokens: fixture-mini $1 in / $4 out / $0.10 cached; fixture-nocache has no cached rate.
REGISTRY = {"models": [
    {"provider_id": "openai", "model_id": "fixture-mini", "input_micros_per_mtok": 1_000_000,
     "output_micros_per_mtok": 4_000_000, "cached_input_micros_per_mtok": 100_000},
    {"provider_id": "openai", "model_id": "fixture-nocache", "input_micros_per_mtok": 2_000_000,
     "output_micros_per_mtok": 8_000_000, "cached_input_micros_per_mtok": None},
    {"provider_id": "openai", "model_id": "fixture-noprice", "input_micros_per_mtok": None,
     "output_micros_per_mtok": None, "cached_input_micros_per_mtok": None},
    {"provider_id": "deepseek", "model_id": "fixture-ds", "input_micros_per_mtok": 1_000_000,
     "output_micros_per_mtok": 1_000_000, "cached_input_micros_per_mtok": None}]}
SNAPSHOT = {"schema": usage.SCHEMA, "generated_at": usage.iso(NOW), "purpose": "usage_observation_not_admission_authority",
            "accounts": [{"provider": "deepseek", "slot": "deepseek--configured", "source": "native/deepseek",
                          "windows": [], "notes": [], "observed_at": usage.iso(NOW), "state": "fresh",
                          "balances": [{"kind": "prepaid", "balance": 7.5, "currencyCode": "USD",
                                        "period": "DeepSeek balance"}]},
                         {"provider": "codex", "slot": "fixture", "source": "native/codex", "windows": [],
                          "observed_at": None, "state": "unknown"}],
            "coverage": [{"provider": "openai", "state": "unknown", "reason": "no_usage_collector"}]}


class Fixture:
    def __init__(self, tmp, grants=(GRANT,), rows=(), store=True):
        self.runtime = Path(tmp) / "runtime"
        (self.runtime / "config").mkdir(parents=True)
        (self.runtime / "state").mkdir()
        self.registry = Path(tmp) / "registry.json"
        self.registry.write_text(json.dumps(REGISTRY))
        if grants is not None:
            self.write({"schema": credit.SCHEMA, "grants": list(grants)})
        if store:
            db = sqlite3.connect(self.runtime / "state/scout.sqlite")
            db.execute("CREATE TABLE usage (id INTEGER PRIMARY KEY, request_id TEXT, provider_id TEXT, model TEXT,"
                       " cost_micros INTEGER, cost_known INTEGER, input_tokens INTEGER, output_tokens INTEGER,"
                       " cached_input_tokens INTEGER, created_at INTEGER)")
            for provider, model, inp, out, cached, created in rows:
                # cost_micros is deliberately wrong: the meter must never sum it.
                db.execute("INSERT INTO usage (request_id, provider_id, model, cost_micros, cost_known, input_tokens,"
                           " output_tokens, cached_input_tokens, created_at) VALUES (?, ?, ?, 999999999, 1, ?, ?, ?, ?)",
                           (f"r{created}{model}", provider, model, inp, out, cached, created))
            db.commit()
            db.close()

    @property
    def grants_path(self):
        return self.runtime / "config/credit-grants.json"

    def write(self, data, raw=None):
        self.grants_path.write_text(raw if raw is not None else json.dumps(data))
        os.chmod(self.grants_path, 0o600)

    def one(self, now=NOW):
        rows, problem = credit.view(self.runtime, now, self.registry)
        assert problem is None and len(rows) == 1, (problem, rows)
        return rows[0]


class CreditGrantTests(unittest.TestCase):
    def test_other_providers_and_rows_before_the_reading_are_excluded(self):
        with tempfile.TemporaryDirectory() as tmp:
            row = Fixture(tmp, rows=[
                # another provider, even with a model name openai prices: $500+ if wrongly counted
                ("deepseek", "fixture-mini", 500_000_000, 0, 0, READING_MS + 1000),
                ("deepseek", "fixture-ds", 500_000_000, 500_000_000, 0, READING_MS + 1000),
                ("openai", "fixture-mini", 7_000_000, 0, 0, READING_MS - 1),                 # 1 ms before
                ("openai", "fixture-mini", 1_000_000, 0, 0, READING_MS)]).one()               # exactly at: $1
            balance = row["balances"][0]
            self.assertAlmostEqual(balance["spentSinceObserved"], 1.0, places=9)
            self.assertAlmostEqual(balance["balance"], 89.0, places=9)
            self.assertEqual(balance["unpricedRequests"], 0)
            self.assertEqual((balance["kind"], balance["currencyCode"], balance["estimated"]), ("prepaid", "USD", True))
            self.assertEqual((balance["observedBalance"], balance["observedAt"]), (90.0, READING))
            self.assertIn("2026-01-31", balance["period"])
            self.assertEqual((row["provider"], row["source"], row["state"], row["estimated"]),
                             ("openai-api", "credit-grant/openai", "fresh", True))

    def test_created_at_is_milliseconds_not_seconds(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, rows=[
                ("openai", "fixture-mini", 5_000_000, 0, 0, READING_MS - 3_600_000),  # an hour before: $5, excluded
                ("openai", "fixture-mini", 2_000_000, 0, 0, READING_MS + 3_600_000)])  # an hour after: $2
            row = fixture.one()
            self.assertAlmostEqual(row["balances"][0]["spentSinceObserved"], 2.0, places=9)
            store = fixture.runtime / "state/scout.sqlite"
            as_ms = credit.spend(store, "openai", READING_MS)
            as_seconds = credit.spend(store, "openai", int(at(READING)))
            # A seconds cutoff lets the earlier millisecond row in: 7M tokens ($7), the wrong answer.
            self.assertEqual(as_ms, [("fixture-mini", 1, 2_000_000, 0, 0)])
            self.assertEqual(as_seconds, [("fixture-mini", 2, 7_000_000, 0, 0)])

    def test_cached_tokens_use_the_cached_rate_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            row = Fixture(tmp, rows=[
                # input_tokens excludes cached (stored convention): 1M in + 9M cached + 0.5M out
                ("openai", "fixture-mini", 1_000_000, 500_000, 9_000_000, READING_MS + 1),
                # no cached rate in the registry: cached falls back to the input rate
                ("openai", "fixture-nocache", 1_000_000, 0, 1_000_000, READING_MS + 2)]).one()
            spent = row["balances"][0]["spentSinceObserved"]
            # $1 + 9M x $0.10 + 0.5M x $4 = $3.90; fixture-nocache $2 + $2 = $4. Total $7.90.
            # Cached at the input rate would be $16.00; cached counted twice $16.90; cost_micros ~$2000.
            self.assertAlmostEqual(spent, 7.9, places=9)
            self.assertEqual(row["balances"][0]["unpricedRequests"], 0)

    def test_unpriced_models_are_counted_and_named_in_the_note(self):
        with tempfile.TemporaryDirectory() as tmp:
            row = Fixture(tmp, rows=[
                ("openai", "fixture-unknown", 9_000_000, 9_000_000, 0, READING_MS + 1),
                ("openai", "fixture-unknown", 9_000_000, 9_000_000, 0, READING_MS + 2),
                ("openai", "fixture-noprice", 9_000_000, 9_000_000, 0, READING_MS + 3),
                ("openai", "fixture-mini", 1_000_000, 0, 0, READING_MS + 4)]).one()
            balance = row["balances"][0]
            self.assertEqual(balance["unpricedRequests"], 3)
            self.assertAlmostEqual(balance["spentSinceObserved"], 1.0, places=9)
            self.assertIn("3 requests had no known price and are not in the estimate.", row["notes"])
            one = Fixture(Path(tmp) / "one", rows=[("openai", "fixture-unknown", 1, 1, 0, READING_MS)]).one()
            self.assertIn("1 request had no known price and is not in the estimate.", one["notes"])

    def test_note_window_and_pace(self):
        with tempfile.TemporaryDirectory() as tmp:
            row = Fixture(tmp, rows=[("openai", "fixture-mini", 1_000_000, 0, 0, READING_MS)]).one()
            self.assertEqual(row["notes"][0], "Estimated $89.00 of $100.00 left, expires 2026-01-31 (20.0 days). "
                             "May spend about $3.95 a day, keeping $10.00 back. Spending is under pace.")
            pace = row["pace"]
            self.assertEqual(pace["state"], "under")
            self.assertAlmostEqual(pace["allowed_per_day_usd"], (89 - 10) / 20, places=4)
            self.assertAlmostEqual(pace["days_left"], 20.0, places=3)
            self.assertAlmostEqual(pace["used_fraction"], 0.11, places=6)
            self.assertAlmostEqual(pace["elapsed_fraction"], 1 / 3, places=6)
            self.assertEqual(row["windows"], [])  # a quota window would make the menu bar draw "no reading"
            w = row["grant_window"]
            self.assertEqual(row["pace"]["expires_at"], "2026-01-31T00:00:00Z")
            self.assertEqual((w["name"], w["state"], w["resets_at"], w["estimated"]),
                             ("credit_grant", "fresh", "2026-01-31T00:00:00Z", True))
            self.assertAlmostEqual(w["used_percent"], 11.0, places=6)
            over = Fixture(Path(tmp) / "over", rows=[("openai", "fixture-mini", 60_000_000, 0, 0, READING_MS)]).one()
            self.assertEqual(over["pace"]["state"], "over")  # used 0.70 > elapsed 0.33 + 0.15
            even = Fixture(Path(tmp) / "even", rows=[("openai", "fixture-mini", 20_000_000, 0, 0, READING_MS)]).one()
            self.assertEqual(even["pace"]["state"], "on_pace")  # used 0.30
            self.assertTrue(even["notes"][0].endswith("Spending is on pace."))

    def test_absent_grants_file_changes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=None, rows=[("openai", "fixture-mini", 1, 1, 0, READING_MS)])
            result = credit.attach(deepcopy(SNAPSHOT), fixture.runtime, now=NOW, registry=fixture.registry)
            self.assertEqual(result, SNAPSHOT)
            self.assertEqual(self.limits_json(fixture.runtime), SNAPSHOT)
            out = io.StringIO()
            with patch.dict(os.environ, {"SWITCHBACK_RUNTIME_ROOT": str(fixture.runtime)}), redirect_stdout(out):
                self.assertEqual(credit.main([]), 0)
            self.assertIn("No credit grants recorded", out.getvalue())

    def test_grant_row_is_added_after_unchanged_rows(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, rows=[("openai", "fixture-mini", 1, 1, 0, READING_MS)])
            with patch.dict(os.environ, {"SB_PROVIDER_REGISTRY": str(fixture.registry)}), \
                 patch.object(credit.time, "time", return_value=NOW):
                result = self.limits_json(fixture.runtime)
                filtered = self.limits_json(fixture.runtime, ["--provider", "deepseek"])
            self.assertEqual(result["accounts"][:2], SNAPSHOT["accounts"])
            self.assertEqual(result["coverage"], SNAPSHOT["coverage"])
            self.assertEqual([r["slot"] for r in result["accounts"][2:]], ["openai-api--credit-grant"])
            self.assertEqual(filtered, SNAPSHOT)
            text = io.StringIO()
            with redirect_stdout(text):
                usage.render(result)
            self.assertIn("Estimated $", text.getvalue())
            self.assertIn("expires 2026-01-31", text.getvalue())  # the note carries the expiry; the row has no quota window

    def test_missing_or_unreadable_store_is_unknown_not_unspent(self):
        with tempfile.TemporaryDirectory() as tmp:
            for name, damage in (("missing", lambda s: s.unlink()),
                                 ("garbage", lambda s: s.write_bytes(b"not a database" * 100)),
                                 ("no-table", lambda s: (s.unlink(), sqlite3.connect(s).close()))):
                fixture = Fixture(Path(tmp) / name)
                damage(fixture.runtime / "state/scout.sqlite")
                row = fixture.one()
                balance = row["balances"][0]
                self.assertEqual((row["state"], row["refresh_error"]), ("unknown", "usage_store_unavailable"), name)
                self.assertIsNone(balance["balance"], name)
                self.assertIsNone(balance["spentSinceObserved"], name)
                self.assertIsNone(row["grant_window"]["used_percent"], name)
                self.assertEqual((row["pace"]["state"], row["pace"]["allowed_per_day_usd"]), ("unknown", None), name)
                self.assertTrue(row["notes"][0].startswith("Balance unknown"), name)
                self.assertIn("not known", row["notes"][0], name)
                self.assertNotIn("Estimated $90.00", " ".join(row["notes"]), name)

    def test_malformed_grants_file_is_one_fixed_diagnostic(self):
        bad = dict(GRANT, amount_usd=-5)
        backwards = dict(GRANT, expires_at="2025-12-01")
        cases = {"bad json": "{not json", "deep": "[" * 50_000,
                 "schema": json.dumps({"schema": "switchback/credit-grants@2", "grants": [GRANT]}),
                 "negative": json.dumps({"schema": credit.SCHEMA, "grants": [bad]}),
                 "backwards": json.dumps({"schema": credit.SCHEMA, "grants": [GRANT, backwards]}),
                 "nan": json.dumps({"schema": credit.SCHEMA, "grants": [dict(GRANT, observed_balance_usd=float("nan"))]}),
                 "ref not an env name": json.dumps({"schema": credit.SCHEMA,
                                                    "grants": [dict(GRANT, credential_ref="not a name")]})}
        with tempfile.TemporaryDirectory() as tmp:
            for name, raw in cases.items():
                fixture = Fixture(Path(tmp) / name.replace(" ", "-"))
                fixture.write(None, raw)
                result = credit.attach(deepcopy(SNAPSHOT), fixture.runtime, now=NOW, registry=fixture.registry)
                self.assertEqual(result["accounts"], SNAPSHOT["accounts"], name)
                self.assertEqual(result["coverage"], SNAPSHOT["coverage"] + [
                    {"provider": "credit-grants", "state": "unavailable", "reason": "credit_grants_invalid"}], name)
                self.assertEqual(self.limits_json(fixture.runtime)["accounts"], SNAPSHOT["accounts"], name)
            fixture = Fixture(Path(tmp) / "link")
            target = Path(tmp) / "elsewhere.json"
            target.write_text(fixture.grants_path.read_text())
            fixture.grants_path.unlink()
            fixture.grants_path.symlink_to(target)
            rows, problem = credit.view(fixture.runtime, NOW, fixture.registry)
            self.assertEqual((rows, problem), ([], "credit_grants_unreadable"))
            os.chmod(target, 0o666)
            fixture.grants_path.unlink()
            target.rename(fixture.grants_path)
            self.assertEqual(credit.view(fixture.runtime, NOW, fixture.registry), ([], "credit_grants_unreadable"))

    def test_expired_grant_has_nothing_left_and_no_pace(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, rows=[("openai", "fixture-mini", 1_000_000, 0, 0, READING_MS)])
            for row in (fixture.one(at("2026-02-01T00:00:00Z")), fixture.one(at("2026-01-31T00:00:00Z"))):
                self.assertEqual(row["pace"]["state"], "expired")
                self.assertEqual(row["pace"]["allowed_per_day_usd"], 0)
                self.assertEqual(row["balances"][0]["balance"], 0)
                self.assertEqual(row["grant_window"]["remaining_percent"], 0)
                self.assertIn("expired on 2026-01-31", row["notes"][0])
            (fixture.runtime / "state/scout.sqlite").unlink()
            self.assertEqual(fixture.one(at("2026-02-01T00:00:00Z"))["balances"][0]["balance"], 0)

    def test_observe_round_trip_keeps_other_grants_and_rejects_bad_input(self):
        other = {"provider": "fixturecloud", "credential_ref": "FIXTURECLOUD_API_KEY", "amount_usd": 5,
                 "received_at": "2026-01-01T00:00:00Z", "expires_at": "2026-02-01T00:00:00+00:00",
                 "observed_balance_usd": 5, "observed_at": "2026-01-02", "operator_field": {"keep": True}}
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=[dict(GRANT, operator_field="keep me"), other])
            (fixture.runtime / "manifest.json").write_text("{}")
            ok = self.observe(fixture, "openai", "--balance", "61.25", "--expires", "2026-01-31", "--reserve", "5")
            self.assertEqual((ok.returncode, ok.stderr), (0, ""))
            self.assertEqual(stat.S_IMODE(fixture.grants_path.stat().st_mode), 0o600)
            saved = json.loads(fixture.grants_path.read_text())
            self.assertEqual(saved["grants"][1], other)
            first = saved["grants"][0]
            self.assertEqual((first["observed_balance_usd"], first["expires_at"], first["reserve_usd"],
                              first["received_at"], first["amount_usd"], first["operator_field"]),
                             (61.25, "2026-01-31", 5.0, "2026-01-01", 100.0, "keep me"))
            self.assertLess(abs(at(first["observed_at"]) - datetime.now(timezone.utc).timestamp()), 120)
            before = (fixture.grants_path.read_bytes(), fixture.grants_path.stat().st_mtime_ns)
            for argv in (["OpenAI!", "--balance", "1", "--expires", "2026-02-28"],
                         ["openai", "--balance", "-1", "--expires", "2026-02-28"],
                         ["openai", "--balance", "nan", "--expires", "2026-02-28"],
                         ["openai", "--balance", "inf", "--expires", "2026-02-28"],
                         ["openai", "--balance", "1", "--expires", "2026-13-45"],
                         ["openai", "--balance", "1", "--expires", "2025-12-01"],
                         ["openai", "--balance", "1", "--expires", "2026-02-28", "--amount", "0"],
                         ["openai", "--balance", "1"],
                         ["openai", "--balance", "1", "--expires", "2026-02-28", "--bogus", "1"]):
                bad = self.observe(fixture, *argv)
                self.assertNotEqual(bad.returncode, 0, argv)
                self.assertEqual(len(bad.stderr.splitlines()), 1, argv)
                self.assertTrue(bad.stderr.startswith("sb credits: "), argv)
                self.assertNotIn("Traceback", bad.stderr + bad.stdout, argv)
                self.assertEqual((fixture.grants_path.read_bytes(), fixture.grants_path.stat().st_mtime_ns), before, argv)
            fresh = Fixture(Path(tmp) / "new", grants=None)
            (fresh.runtime / "manifest.json").write_text("{}")
            created = self.observe(fresh, "openai", "--balance", "40", "--expires", "2099-01-01", "--received", "2026-01-01")
            self.assertEqual(created.returncode, 0, created.stderr)
            self.assertEqual(stat.S_IMODE(fresh.grants_path.stat().st_mode), 0o600)
            self.assertEqual(len(credit.parse(json.loads(fresh.grants_path.read_text()))), 1)
            self.assertEqual(sorted(p.name for p in fresh.grants_path.parent.iterdir()), ["credit-grants.json"])

    def observe(self, fixture, *argv):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("SWITCHBACK_", "SB_"))}
        env.update(HOME=str(fixture.runtime.parent), SWITCHBACK_RUNTIME_ROOT=str(fixture.runtime))
        return subprocess.run([str(CLI / "sb"), "credits", "observe", *argv], env=env, capture_output=True,
                              text=True, timeout=60)

    def limits_json(self, runtime, extra=()):
        """`sb limits --json` through native-accounts.py, with the cached observations replaced by SNAPSHOT."""
        out = io.StringIO()
        env = {"SWITCHBACK_RUNTIME_ROOT": str(runtime), "SB_USAGE_COSTS": "0"}
        with patch.object(usage, "report", return_value=deepcopy(SNAPSHOT)), \
             patch.object(sys, "argv", ["native-accounts.py", "usage", "--json", *extra]), \
             patch.dict(os.environ, env), redirect_stdout(out):
            self.assertEqual(native.main(), 0)
        return json.loads(out.getvalue())


BILLED = {**GRANT, "usage_credential_ref": "FIXTURE_USAGE_KEY"}
SECRET = "fixture-usage-key-value"


def costs(*values, more=False):
    return {"object": "page", "has_more": more, "data": [
        {"object": "bucket", "results": [{"amount": {"value": v, "currency": "usd"}}] if v is not None else []}
        for v in values]}


class BillingReadingTests(unittest.TestCase):
    """The provider's own billing, when a usage-read key is named, replaces the gateway estimate."""

    def view(self, fixture, fetch, now=NOW, key=SECRET):
        rows, problem = credit.view(fixture.runtime, now, fixture.registry,
                                    {"FIXTURE_USAGE_KEY": key} if key is not None else {}, fetch)
        assert problem is None and len(rows) == 1, (problem, rows)
        return rows[0]

    def test_billing_replaces_the_estimate_and_counts_from_the_grant_amount(self):
        with tempfile.TemporaryDirectory() as tmp:
            # The gateway saw $2 since the reading; the provider billed $37.25 in all (calls outside the gateway).
            fixture = Fixture(tmp, grants=(BILLED,), rows=[("openai", "fixture-mini", 2_000_000, 0, 0, READING_MS + 1)])
            seen = []
            row = self.view(fixture, lambda g, token: seen.append((g["received"], token)) or 37.25)
            self.assertEqual(seen, [(at("2026-01-01T00:00:00Z"), SECRET)])
            balance = row["balances"][0]
            self.assertAlmostEqual(balance["balance"], 62.75)  # 100 - 37.25, not 90 - 2
            self.assertEqual((row["estimated"], balance["estimated"], row["grant_window"]["estimated"]), (False, False, False))
            self.assertEqual(balance["billedSinceGrant"], 37.25)
            self.assertAlmostEqual(balance["spentSinceObserved"], 2.0)  # the estimate stays as detail
            self.assertAlmostEqual(row["grant_window"]["used_percent"], 37.25)
            self.assertAlmostEqual(row["pace"]["allowed_per_day_usd"], (62.75 - 10) / 20, places=3)
            self.assertIn("by the provider's billing", row["notes"][0])
            self.assertNotIn("Estimated", row["notes"][0])

    def test_no_key_or_no_named_key_makes_no_call_and_stays_an_estimate(self):
        def never(g, token):
            raise AssertionError("no usage-read key: the provider must not be called")
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=(BILLED,), rows=[("openai", "fixture-mini", 2_000_000, 0, 0, READING_MS + 1)])
            for key in (None, "", "   "):
                row = self.view(fixture, never, key=key)
                self.assertTrue(row["estimated"])
                self.assertAlmostEqual(row["balances"][0]["balance"], 88.0)
            self.assertFalse((fixture.runtime / "state/credit-grant-billing.json").exists())
        with tempfile.TemporaryDirectory() as tmp:
            row = self.view(Fixture(tmp), never)  # the grant names no usage key at all
            self.assertTrue(row["estimated"])

    def test_failed_billing_read_falls_back_to_the_estimate_and_says_so(self):
        def refused(g, token):
            raise credit.GrantError("billing_authentication_required")
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=(BILLED,), rows=[("openai", "fixture-mini", 2_000_000, 0, 0, READING_MS + 1)])
            row = self.view(fixture, refused)
            self.assertTrue(row["estimated"])
            self.assertAlmostEqual(row["balances"][0]["balance"], 88.0)
            self.assertNotIn("billedSinceGrant", row["balances"][0])
            self.assertTrue(any("billing_authentication_required" in n and "gateway estimate" in n for n in row["notes"]))

    def test_readings_are_cached_ten_minutes_and_failures_two(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture, calls = Fixture(tmp, grants=(BILLED,)), []
            def fetch(g, token):
                calls.append(1)
                return float(len(calls))
            self.assertEqual(self.view(fixture, fetch)["balances"][0]["billedSinceGrant"], 1.0)
            later = self.view(fixture, fetch, now=NOW + 599)
            self.assertEqual((later["balances"][0]["billedSinceGrant"], len(calls)), (1.0, 1))
            self.assertEqual(later["observed_at"], usage.iso(NOW))  # the row shows when the billing was read
            self.assertEqual(self.view(fixture, fetch, now=NOW + 601)["balances"][0]["billedSinceGrant"], 2.0)
        with tempfile.TemporaryDirectory() as tmp:
            fixture, calls = Fixture(tmp, grants=(BILLED,)), []
            def down(g, token):
                calls.append(1)
                raise credit.GrantError("billing_source_unavailable")
            self.view(fixture, down)
            self.view(fixture, down, now=NOW + 119)
            self.assertEqual(len(calls), 1)
            self.view(fixture, down, now=NOW + 121)
            self.assertEqual(len(calls), 2)

    def test_the_key_never_reaches_the_row_or_the_cache_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=(BILLED,))
            row = self.view(fixture, lambda g, token: 5.0)
            cache = fixture.runtime / "state/credit-grant-billing.json"
            self.assertNotIn(SECRET, json.dumps(row))
            self.assertNotIn(SECRET, cache.read_text())
            self.assertEqual(stat.S_IMODE(cache.stat().st_mode), 0o600)

    def test_billing_payload_is_summed_strictly(self):
        g = credit.grant(BILLED)
        def fetch_with(payload):
            with patch("provider_account_usage.https_json", lambda host, path, token, timeout: payload):
                return credit.billed_fetch(g, SECRET)
        self.assertAlmostEqual(fetch_with(costs(1.5, None, 0.25)), 1.75)
        for bad in (costs(1.5, more=True),                      # a second page would be an undercount
                    {"data": [{"results": [{"amount": {"value": "1.5", "currency": "usd"}}]}], "has_more": False},
                    {"data": [{"results": [{"amount": {"value": 1.5, "currency": "eur"}}]}], "has_more": False},
                    {"data": [{"results": None}], "has_more": False}, {"data": None, "has_more": False}, {}):
            with self.assertRaises(credit.GrantError):
                fetch_with(bad)

    def test_billing_request_is_one_fixed_host_read_from_the_grant_date(self):
        seen = []
        def transport(host, path, token, timeout):
            seen.append((host, path, token))
            return costs(0.5)
        with patch("provider_account_usage.https_json", transport):
            credit.billed_fetch(credit.grant(BILLED), SECRET)
        start = int(at("2026-01-01T00:00:00Z"))
        self.assertEqual(seen, [("api.openai.com", f"/v1/organization/costs?start_time={start}&bucket_width=1d&limit=180", SECRET)])

    def test_expired_grant_makes_no_billing_call(self):
        def never(g, token):
            raise AssertionError("an expired grant needs no reading")
        with tempfile.TemporaryDirectory() as tmp:
            row = self.view(Fixture(tmp, grants=(BILLED,)), never, now=at("2026-02-01T00:00:00Z"))
            self.assertEqual((row["pace"]["state"], row["balances"][0]["balance"]), ("expired", 0.0))

    def test_a_bad_usage_key_name_is_an_invalid_grants_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            for bad in ("not a name", 7, "sk-looks-like-a-key"):
                fixture = Fixture(tmp + f"/{abs(hash(str(bad)))}", grants=({**GRANT, "usage_credential_ref": bad},))
                self.assertEqual(credit.view(fixture.runtime, NOW, fixture.registry), ([], "credit_grants_invalid"))


class ReviewFixTests(unittest.TestCase):
    """Defects a second reviewer found on 2026-11-24."""
    observe = CreditGrantTests.observe

    def test_a_later_expiry_is_a_new_grant_not_the_old_one_extended(self):
        today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=[dict(BILLED, operator_field="keep me")])
            (fixture.runtime / "manifest.json").write_text("{}")
            ok = self.observe(fixture, "openai", "--balance", "150", "--expires", "2099-01-01")
            self.assertEqual((ok.returncode, ok.stderr), (0, ""))
            saved = json.loads(fixture.grants_path.read_text())["grants"][0]
            # The old grant (100, received 2026-01-01) must not survive: its whole bill would be subtracted again.
            self.assertEqual((saved["received_at"], saved["amount_usd"], saved["observed_balance_usd"], saved["operator_field"]),
                             (today, 150.0, 150.0, "keep me"))
            self.assertIn("New grant", ok.stdout)
            # With the provider's billing since the NEW date (2 spent), 148 is left; the old rule gave 150 - 99 = 51.
            g = credit.grant(saved)
            since = []
            rows, problem = credit.view(fixture.runtime, at("2098-06-01T00:00:00Z"), fixture.registry,
                                        {"FIXTURE_USAGE_KEY": "k"}, lambda grant, token: since.append(grant["received"]) or 2.0)
            self.assertIsNone(problem)
            self.assertEqual(since, [g["received"]])
            self.assertAlmostEqual(rows[0]["balances"][0]["balance"], 148.0)
            # Explicit values win over the defaults.
            ok = self.observe(fixture, "openai", "--balance", "250", "--expires", "2199-01-01",
                              "--received", "2198-12-01", "--amount", "300")
            self.assertEqual(ok.returncode, 0, ok.stderr)
            saved = json.loads(fixture.grants_path.read_text())["grants"][0]
            self.assertEqual((saved["received_at"], saved["amount_usd"]), ("2198-12-01", 300.0))

    def test_a_balance_above_the_grant_amount_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp)
            (fixture.runtime / "manifest.json").write_text("{}")
            before = fixture.grants_path.read_bytes()
            bad = self.observe(fixture, "openai", "--balance", "250", "--expires", "2026-01-31")  # grant is 100
            self.assertNotEqual(bad.returncode, 0)
            self.assertEqual(len(bad.stderr.splitlines()), 1)
            self.assertEqual(fixture.grants_path.read_bytes(), before)

    def test_the_expired_note_names_the_received_date(self):
        with tempfile.TemporaryDirectory() as tmp:
            row = Fixture(tmp).one(at("2026-02-01T00:00:00Z"))
            self.assertIn("--expires YYYY-MM-DD --received YYYY-MM-DD", row["notes"][0])

    def test_a_huge_number_is_an_invalid_file_not_a_crash(self):
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp)
            fixture.write(None, raw=json.dumps({"schema": credit.SCHEMA, "grants": [GRANT]}).replace("100.0", "1" + "0" * 400))
            self.assertEqual(credit.view(fixture.runtime, NOW, fixture.registry), ([], "credit_grants_invalid"))
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=(BILLED,))
            (fixture.runtime / "state/credit-grant-billing.json").write_text(
                '{"openai-api--credit-grant@%d": {"billed_usd": 1, "read_at": 1%s}}' % (at("2026-01-01T00:00:00Z"), "0" * 400))
            rows, problem = credit.view(fixture.runtime, NOW, fixture.registry, {"FIXTURE_USAGE_KEY": "k"}, lambda g, token: 3.0)
            self.assertIsNone(problem)
            self.assertEqual(rows[0]["balances"][0]["billedSinceGrant"], 3.0)  # the bad cache entry is dropped, a fresh read is used

    def test_a_dated_model_behind_a_provider_prefix_is_priced(self):
        with tempfile.TemporaryDirectory() as tmp:
            row = Fixture(tmp, rows=[("openai", "openai/fixture-mini-2026-01-05", 1_000_000, 0, 0, READING_MS + 1)]).one()
            self.assertEqual(row["balances"][0]["unpricedRequests"], 0)
            self.assertAlmostEqual(row["balances"][0]["spentSinceObserved"], 1.0)

    def test_a_failed_billing_read_is_named_in_the_row(self):
        def refused(g, token):
            raise credit.GrantError("billing_authentication_required")
        with tempfile.TemporaryDirectory() as tmp:
            fixture = Fixture(tmp, grants=(BILLED,))
            rows, _ = credit.view(fixture.runtime, NOW, fixture.registry, {"FIXTURE_USAGE_KEY": "k"}, refused)
            self.assertEqual((rows[0]["estimated"], rows[0]["billing_error"]), (True, "billing_authentication_required"))
            rows, _ = credit.view(fixture.runtime, NOW + 1000, fixture.registry, {"FIXTURE_USAGE_KEY": "k"}, lambda g, token: 1.0)
            self.assertNotIn("billing_error", rows[0])


if __name__ == "__main__":
    unittest.main()
