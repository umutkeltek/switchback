#!/usr/bin/env python3
"""Estimated token cost from local logs: pricing, dedupe across homes, attribution,
incremental reads, local day buckets, and the off switch. Synthetic fixtures only."""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

CLI = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(CLI))
import native_account_cost as cost
import native_account_prices as prices


def _never_spawn(argv, env):
    raise AssertionError("a test tried to start a real background pass")


# Tests count starts through this seam; no real process is ever started.
cost.spawn = _never_spawn


def stamp(ts):
    return datetime.fromtimestamp(ts, timezone.utc).isoformat().replace("+00:00", "Z")


def claude(mid, ts, model="claude-opus-5-5", inp=1000, w5=0, w1h=0, read=0, out=100, rid=None):
    usage = {"input_tokens": inp, "cache_creation_input_tokens": w5 + w1h,
             "cache_creation": {"ephemeral_5m_input_tokens": w5, "ephemeral_1h_input_tokens": w1h},
             "cache_read_input_tokens": read, "output_tokens": out, "speed": "standard"}
    line = {"parentUuid": None, "isSidechain": False, "sessionId": "session-a",
            "message": {"id": mid, "model": model, "role": "assistant",
                        "content": [{"type": "text", "text": "fixture"}], "usage": usage},
            "requestId": rid or "req-" + mid, "type": "assistant", "uuid": "u-" + mid, "timestamp": stamp(ts)}
    return json.dumps(line, separators=(",", ":")) + "\n"


def user(text, ts):
    return json.dumps({"type": "user", "message": {"role": "user", "content": text}, "timestamp": stamp(ts)},
                      separators=(",", ":")) + "\n"


class Fixture:
    """A runtime with saved Claude homes and their transcripts, in a temp folder."""

    def __init__(self, case):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-cost-")
        case.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.state = self.root / "state/usage-cost.sqlite"
        self.now = time.time()

    def home(self, label):
        path = self.root / "homes" / label / "projects"
        path.mkdir(parents=True, exist_ok=True)
        return path

    def homes(self, *labels):
        return [("claude", label, self.root / "homes" / label, label, [self.home(label)]) for label in labels]

    def write(self, label, rel, text, mode="w"):
        path = self.home(label) / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        with open(path, mode) as stream:
            stream.write(text)
        return path

    def collect(self, *labels, now=None, **kw):
        return cost.collect(self.state, self.homes(*labels), now=now or self.now, **kw)


def provider(summary, name="claude"):
    return next(p for p in summary["providers"] if p["provider"] == name)


def account(summary, owner, name="claude"):
    return next(a for a in provider(summary, name)["accounts"] if a["account"] == owner)


class PricingTests(unittest.TestCase):
    def test_each_token_kind_has_its_own_price(self):
        # Opus 5.5: input 4, output 20, 5m write 5, 1h write 8, read 0.20 USD per million.
        self.assertEqual(prices.claude_cost_nanos("claude-opus-5-5", 1_000_000, 0, 0, 0, 0), 4_000_000_000)
        self.assertEqual(prices.claude_cost_nanos("claude-opus-5-5", 0, 0, 0, 0, 1_000_000), 20_000_000_000)
        self.assertEqual(prices.claude_cost_nanos("claude-opus-5-5", 0, 1_000_000, 0, 0, 0), 5_000_000_000)
        self.assertEqual(prices.claude_cost_nanos("claude-opus-5-5", 0, 0, 1_000_000, 0, 0), 8_000_000_000)
        self.assertEqual(prices.claude_cost_nanos("claude-opus-5-5", 0, 0, 0, 1_000_000, 0), 200_000_000)
        # Codex: input_tokens include cached ones; cached and written tokens get their own rate.
        self.assertEqual(prices.codex_cost_nanos("gpt-6.1-sol", 1_000, 0, 0, 0), 2_000_000)
        self.assertEqual(prices.codex_cost_nanos("gpt-6.1-sol", 0, 0, 1_000, 0), 100_000)
        self.assertEqual(prices.codex_cost_nanos("gpt-6.1-sol", 0, 1_000, 0, 0), 2_500_000)
        self.assertEqual(prices.codex_cost_nanos("gpt-6.1-sol", 0, 0, 0, 1_000), 10_000_000)

    def test_prompt_size_tiers_and_modifiers(self):
        small = prices.claude_cost_nanos("claude-haiku-5-5", 100_000, 0, 0, 0, 0)
        large = prices.claude_cost_nanos("claude-haiku-5-5", 100_001, 0, 0, 0, 0)
        self.assertEqual(small, 10_000_000)
        self.assertEqual(large, 50_000_500)
        self.assertEqual(prices.codex_cost_nanos("gpt-6-astra", 272_001, 0, 0, 1),
                         round((272_001 * 10.0 * 2 + 1 * 50.0 * 1.5) * 1000))
        self.assertEqual(prices.claude_cost_nanos("claude-fable-5-2", 1_000_000, 0, 0, 0, 0, speed="fast"),
                         16_000_000_000)
        self.assertEqual(prices.claude_model("claude-sonnet-4-5-20250929"), "claude-sonnet-4-5")
        self.assertIsNone(prices.claude_cost_nanos("claude-unknown-9", 1, 0, 0, 0, 0))

    def test_collector_prices_all_four_kinds_from_a_transcript(self):
        f = Fixture(self)
        f.write("a", "p/s.jsonl", claude("m1", f.now - 60, inp=1000, w5=2000, w1h=3000, read=4000, out=500))
        summary = f.collect("a")
        week = provider(summary)["last_7_days"]
        self.assertEqual(week["tokens"], 10_500)
        expected = (1000 * 4 + 500 * 20 + 2000 * 5 + 3000 * 8 + 4000 * 0.2) / 1e6
        self.assertAlmostEqual(week["amount"], round(expected, 4))


class DedupeAndAttributionTests(unittest.TestCase):
    def test_same_message_in_two_homes_counts_once(self):
        # Same relative path: the copy is skipped as a shared prefix. Other
        # path and other bytes before it: the copy is read and deduped by id.
        for rel, lead in (("p/s.jsonl", False), ("q/s.jsonl", True)):
            with self.subTest(rel=rel):
                f = Fixture(self)
                text = claude("m1", f.now - 120) + claude("m2", f.now - 60)
                copy = (user("opened", f.now - 300) if lead else "") + text
                f.write("a", "p/s.jsonl", text)
                f.write("b", rel, copy)
                summary = f.collect("a", "b")
                self.assertEqual(provider(summary)["last_7_days"]["tokens"], 2 * 1100)
                read = len(text) + len(copy) if lead else len(text)
                self.assertEqual(summary["pass"]["bytes_read"], read)

    def test_copy_found_in_the_same_catch_up_is_unattributed_not_guessed(self):
        for rel in ("p/s.jsonl", "q/s.jsonl"):
            with self.subTest(rel=rel):
                f = Fixture(self)
                text = claude("m1", f.now - 120)
                f.write("a", "p/s.jsonl", text)
                f.write("b", rel, text)
                summary = f.collect("a", "b")
                claude_total = provider(summary)
                self.assertEqual(claude_total["unattributed"]["last_7_days"]["tokens"], 1100)
                self.assertEqual(claude_total["last_7_days"]["tokens"], 1100)
                self.assertEqual(account(summary, "a")["last_7_days"]["tokens"], 0)
                self.assertEqual(account(summary, "b")["last_7_days"]["tokens"], 0)

    def test_continued_session_is_charged_where_each_line_was_first_seen(self):
        f = Fixture(self)
        first = claude("m1", f.now - 300) + claude("m2", f.now - 200)
        f.write("a", "p/s.jsonl", first)
        self.assertEqual(account(f.collect("a", "b"), "a")["last_7_days"]["tokens"], 2200)
        # Chats follow: b gets a copy later, then the chat continues under b.
        f.write("b", "p/s.jsonl", first + user("go on", f.now - 100) + claude("m3", f.now - 90, out=900))
        summary = f.collect("a", "b")
        self.assertEqual(account(summary, "a")["last_7_days"]["tokens"], 2200)
        self.assertEqual(account(summary, "b")["last_7_days"]["tokens"], 1900)
        self.assertEqual(provider(summary)["unattributed"]["last_7_days"]["tokens"], 0)
        # The copy's shared prefix was not read again.
        self.assertEqual(summary["pass"]["copies_skipped"], 1)
        self.assertEqual(summary["pass"]["bytes_read"], len(user("go on", f.now - 100) +
                                                           claude("m3", f.now - 90, out=900)))

    def test_replaced_copy_with_an_old_mtime_stays_a_copy(self):
        f = Fixture(self)
        a = f.write("a", "p/s.jsonl", claude("m0", f.now - 500))
        f.collect("a", "b")
        old = f.now - 86400
        b = f.write("b", "p/s.jsonl", claude("m0", f.now - 500))
        os.utime(b, (old, old))
        f.collect("a", "b")
        f.write("a", "p/s.jsonl", claude("m1", f.now - 100), mode="a")
        f.collect("a", "b")
        # Chats follow replaces b's copy with a's longer one, keeping an old mtime.
        temporary = f.home("b") / "p" / ".s.tmp"
        temporary.write_bytes(a.read_bytes())
        os.utime(temporary, (old, old))
        os.replace(temporary, b)
        summary = f.collect("a", "b")
        self.assertEqual(summary["pass"]["files_seen"], 2)
        self.assertEqual(account(summary, "a")["last_7_days"]["tokens"], 2200)
        self.assertEqual(provider(summary)["unattributed"]["last_7_days"]["tokens"], 0)

    def test_homes_of_one_account_never_make_a_message_unattributed(self):
        f = Fixture(self)
        text = claude("m1", f.now - 120)
        f.write("a", "p/s.jsonl", text)
        f.write("b", "p/s.jsonl", text)
        same = [("claude", label, f.root / "homes" / label, "seat", [f.home(label)]) for label in ("a", "b")]
        summary = cost.collect(f.state, same, now=f.now)
        self.assertEqual(account(summary, "seat")["last_7_days"]["tokens"], 1100)
        self.assertEqual(provider(summary)["unattributed"]["last_7_days"]["tokens"], 0)

    def test_symlinked_project_folders_are_not_followed(self):
        f = Fixture(self)
        f.write("a", "real/s.jsonl", claude("m1", f.now - 60))
        # A subagent's own subagents sit in nested folders and still count.
        f.write("a", "real/s/subagents/workflows/w1/agent-1.jsonl", claude("m2", f.now - 50))
        os.symlink(f.home("a") / "real", f.home("a") / "alias")
        other = f.root / "outside"
        other.mkdir()
        (other / "x.jsonl").write_text(claude("m9", f.now - 60))
        os.symlink(other, f.home("a") / "linked")
        os.symlink(other, f.home("a") / "real" / "s" / "linked")
        summary = f.collect("a")
        self.assertEqual(summary["pass"]["files_seen"], 2)
        self.assertEqual(provider(summary)["last_7_days"]["tokens"], 2200)


class IncrementalTests(unittest.TestCase):
    def test_second_pass_reads_only_appended_bytes(self):
        f = Fixture(self)
        head = claude("m1", f.now - 120) + user("x" * 5000, f.now - 110)
        path = f.write("a", "p/s.jsonl", head)
        self.assertEqual(f.collect("a")["pass"]["bytes_read"], len(head))
        self.assertEqual(f.collect("a")["pass"]["bytes_read"], 0)
        more = claude("m2", f.now - 60)
        f.write("a", "p/s.jsonl", more, mode="a")
        summary = f.collect("a")
        self.assertEqual(summary["pass"]["bytes_read"], len(more))
        self.assertEqual(provider(summary)["last_7_days"]["tokens"], 2200)
        self.assertEqual(path.stat().st_size, len(head) + len(more))

    def test_unfinished_last_line_is_counted_when_completed(self):
        f = Fixture(self)
        line = claude("m2", f.now - 60)
        f.write("a", "p/s.jsonl", claude("m1", f.now - 120) + line[:40])
        self.assertEqual(provider(f.collect("a"))["last_7_days"]["tokens"], 1100)
        f.write("a", "p/s.jsonl", line[40:], mode="a")
        summary = f.collect("a")
        self.assertEqual(provider(summary)["last_7_days"]["tokens"], 2200)
        self.assertEqual(summary["pass"]["bytes_read"], len(line))

    def test_rotated_or_shrunk_file_is_read_again_without_double_counting(self):
        f = Fixture(self)
        path = f.write("a", "p/s.jsonl", claude("m1", f.now - 300) + claude("m2", f.now - 200))
        self.assertEqual(provider(f.collect("a"))["last_7_days"]["tokens"], 2200)
        # Replaced by a shorter file with one old and one new message (new inode).
        replacement = path.with_name(".tmp")
        replacement.write_text(claude("m1", f.now - 300) + claude("m4", f.now - 30, out=400))
        os.replace(replacement, path)
        summary = f.collect("a")
        self.assertEqual(provider(summary)["last_7_days"]["tokens"], 2200 + 1400)
        # Truncated in place, then rewritten.
        with open(path, "w") as stream:
            stream.write(claude("m5", f.now - 10, out=0, inp=7))
        summary = f.collect("a")
        self.assertEqual(provider(summary)["last_7_days"]["tokens"], 2200 + 1400 + 7)

    def test_files_untouched_for_eight_days_are_not_read_on_first_pass(self):
        f = Fixture(self)
        path = f.write("a", "p/old.jsonl", claude("m1", f.now - 9 * 86400))
        old = f.now - 9 * 86400
        os.utime(path, (old, old))
        summary = f.collect("a")
        self.assertEqual(summary["pass"]["bytes_read"], 0)
        self.assertEqual(summary["coverage_since"], cost.iso(f.now - 8 * 86400))


class BucketTests(unittest.TestCase):
    def test_today_is_the_local_calendar_day(self):
        with patch.dict(os.environ, {"TZ": "XST-5"}):  # UTC+5, no daylight saving
            time.tzset()
            self.addCleanup(time.tzset)
            f = Fixture(self)
            today = datetime.fromtimestamp(f.now).date()
            midnight = time.mktime((today.year, today.month, today.day, 0, 0, 0, 0, 0, -1))
            now = midnight + 1800  # 00:30 local; 19:30 UTC the day before
            f.write("a", "p/s.jsonl", claude("m1", midnight - 600, out=1) + claude("m2", midnight + 600, out=2)
                    + claude("m3", midnight - 7 * 86400 + 60, out=3))
            summary = f.collect("a", now=now)
            claude_total = provider(summary)
            self.assertEqual(claude_total["today"]["tokens"], 1002)
            self.assertEqual(claude_total["last_7_days"]["tokens"], 1001 + 1002)
        time.tzset()


class UnpricedTests(unittest.TestCase):
    def test_unknown_model_is_counted_and_reported_never_priced_as_zero(self):
        f = Fixture(self)
        f.write("a", "p/s.jsonl", claude("m1", f.now - 60, model="claude-future-9", inp=10, out=5)
                + claude("m2", f.now - 30))
        summary = f.collect("a")
        claude_total = provider(summary)
        self.assertEqual(claude_total["unpriced_models"], [{"model": "claude-future-9", "tokens_last_7_days": 15}])
        self.assertEqual(claude_total["last_7_days"]["tokens"], 1115)
        self.assertEqual(account(summary, "a")["unpriced_tokens"], 15)


class CodexTests(unittest.TestCase):
    def test_response_records_win_over_repeated_token_counts(self):
        f = Fixture(self)
        root = f.root / "codex/sessions/2026/11/17"
        root.mkdir(parents=True)
        def line(kind, payload, ts):
            return json.dumps({"timestamp": stamp(ts), "type": kind, "payload": payload}, separators=(",", ":")) + "\n"
        usage = {"input_tokens": 300_000, "cached_input_tokens": 200_000, "cache_write_input_tokens": 0,
                 "output_tokens": 1000, "reasoning_output_tokens": 10, "total_tokens": 301_000}
        count = {"type": "token_count", "info": {"total_token_usage": usage, "last_token_usage": usage}}
        text = (line("session_meta", {"id": "thread-1"}, f.now - 100)
                + line("turn_context", {"model": "gpt-6.1-sol"}, f.now - 90)
                + line("token_usage_record", {"response_id": "resp-1", "usage": usage}, f.now - 80)
                + line("event_msg", count, f.now - 79) + line("event_msg", count, f.now - 78))
        (root / "rollout.jsonl").write_text(text)
        home = [("codex", "default", f.root / "codex", "default", [f.root / "codex/sessions"])]
        summary = cost.collect(f.state, home, now=f.now)
        codex = provider(summary, "codex")
        self.assertEqual(codex["last_7_days"]["tokens"], 301_000)
        expected = (100_000 * 2.0 * 2 + 200_000 * 0.10 * 2 + 1000 * 10.0 * 1.5) / 1e6  # over 272K
        self.assertAlmostEqual(codex["last_7_days"]["amount"], round(expected, 4))


class SwitchTests(unittest.TestCase):
    def snapshot(self):
        return {"accounts": [{"provider": "claude", "slot": "seat1", "source": "native/claude"},
                             {"provider": "zai", "slot": "z", "source": "zai/api"}]}

    def test_off_means_no_cost_key_and_no_file_read(self):
        f = Fixture(self)
        result = self.snapshot()
        before = json.dumps(result, sort_keys=True)
        with patch.object(cost, "homes", side_effect=AssertionError("read while off")), \
             patch.object(cost, "collect", side_effect=AssertionError("read while off")), \
             patch.object(cost, "saved", side_effect=AssertionError("read while off")), \
             patch.object(cost, "maybe_start", side_effect=AssertionError("start while off")), \
             patch("os.scandir", side_effect=AssertionError("read while off")):
            for value in (None, "0", "", "yes"):
                env = {} if value is None else {"SB_USAGE_COSTS": value}
                cost.attach(result, str(f.root), env=env)
        self.assertEqual(json.dumps(result, sort_keys=True), before)
        self.assertFalse((f.root / "state").exists())

    def test_on_adds_the_agreed_shape_to_native_rows_only(self):
        f = Fixture(self)
        runtime = f.root
        slot = runtime / "config/native-account-slots/claude/seat1"
        slot.mkdir(parents=True)
        (slot / "slot.json").write_text(json.dumps({"provider": "claude", "slot": "seat1",
            "identity": {"account_uuid": "acct-1", "organization_uuid": "org-1"}}))
        home = runtime / "config/claude/seat1"
        (home / "projects/p").mkdir(parents=True)
        (home / ".claude.json").write_text(json.dumps({"oauthAccount": {"accountUuid": "acct-1",
                                                                         "organizationUuid": "org-1"}}))
        (home / "projects/p/s.jsonl").write_text(claude("m1", f.now - 60))
        env = {"SB_USAGE_COSTS": "1", "HOME": str(runtime / "user"),
               "SB_CLAUDE_DEFAULT_HOME": str(runtime / "user/.claude"), "CODEX_HOME": str(runtime / "user/.codex")}
        with patch.dict(os.environ, {"HOME": env["HOME"]}):
            cost.collect(cost.state_path(str(runtime), env), cost.homes(str(runtime), env))
            result = cost.attach(self.snapshot(), str(runtime), env=env)
        row = result["accounts"][0]
        self.assertEqual(set(row["cost"]), {"currency_code", "basis", "sampled_at", "today", "last_7_days"})
        self.assertEqual(row["cost"]["basis"], "estimated_from_local_logs")
        self.assertEqual(row["cost"]["currency_code"], "USD")
        self.assertEqual(row["cost"]["today"]["tokens"], 1100)
        self.assertNotIn("cost", result["accounts"][1])
        status = json.loads((runtime / "state/usage-cost-status.json").read_text())
        self.assertEqual(status["limits_step"]["state"], "attached")

    def summary(self, unattributed_today, unattributed_week):
        def money(tokens):
            return {"amount": tokens / 1000, "tokens": tokens}
        return {"state": "complete", "sampled_at": "2026-01-02T00:00:00Z", "currency_code": "USD",
                "basis": "estimated_from_local_logs",
                "providers": [{"provider": "claude", "today": money(1000), "last_7_days": money(10000),
                               "accounts": [{"account": "seat1", "today": money(400), "last_7_days": money(900)}],
                               "unattributed": {"today": money(unattributed_today),
                                                "last_7_days": money(unattributed_week)}}]}

    def test_account_row_carries_only_windows_whose_split_is_known(self):
        # 2% of the window's tokens may be unattributed; past that the account figure is known to be low.
        both = cost.account_cost(self.summary(20, 200), "claude", "seat1")
        self.assertEqual(both["today"]["tokens"], 400)
        self.assertEqual(both["last_7_days"]["tokens"], 900)
        today_only = cost.account_cost(self.summary(20, 201), "claude", "seat1")
        self.assertEqual(today_only["today"]["tokens"], 400)
        self.assertNotIn("last_7_days", today_only)
        self.assertEqual(today_only["sampled_at"], "2026-01-02T00:00:00Z")
        week_only = cost.account_cost(self.summary(21, 200), "claude", "seat1")
        self.assertNotIn("today", week_only)
        self.assertEqual(week_only["last_7_days"]["tokens"], 900)
        self.assertIsNone(cost.account_cost(self.summary(21, 201), "claude", "seat1"))

    def test_provider_totals_are_given_even_when_no_account_row_carries_cost(self):
        f = Fixture(self)
        summary = self.summary(600, 9000)
        now = cost.parse_ts(summary["sampled_at"]) + 60  # fresh, and no start is due
        with patch.object(cost, "saved", return_value=summary):
            result = cost.attach(self.snapshot(), str(f.root), env={"SB_USAGE_COSTS": "1"}, now=now)
        self.assertNotIn("cost", result["accounts"][0])
        total = result["cost_summary"][0]
        self.assertEqual((total["provider"], total["today"]["tokens"], total["last_7_days"]["tokens"]),
                         ("claude", 1000, 10000))
        self.assertEqual(total["unattributed"]["last_7_days"]["tokens"], 9000)
        self.assertEqual(total["basis"], "estimated_from_local_logs")
        summary["state"] = "incomplete"
        with patch.object(cost, "saved", return_value=summary):
            result = cost.attach(self.snapshot(), str(f.root), env={"SB_USAGE_COSTS": "1"}, now=now)
        self.assertNotIn("cost_summary", result)

    def test_timeout_leaves_rows_without_cost_and_reports_it(self):
        f = Fixture(self)
        def slow(*args, **kwargs):
            time.sleep(0.6)
        with patch.object(cost, "saved", side_effect=slow), patch.object(cost, "maybe_start", return_value="x"):
            result = cost.attach(self.snapshot(), str(f.root), limit=0.3, env={"SB_USAGE_COSTS": "1"})
            time.sleep(0.6)  # let the guarded read end while its patches still hold
        self.assertNotIn("cost", result["accounts"][0])
        status = json.loads((f.root / "state/usage-cost-status.json").read_text())
        self.assertEqual(status["limits_step"]["state"], "timeout")


class BackgroundTests(unittest.TestCase):
    """`sb limits` reads stored totals only; a stale state starts one detached pass."""

    def setUp(self):
        self.f = Fixture(self)
        self.runtime = str(self.f.root)
        self.env = {"SB_USAGE_COSTS": "1", "SB_USAGE_COSTS_STATE": str(self.f.state),
                    "HOME": str(self.f.root / "user"), "PATH": "/usr/bin:/bin"}
        self.starts = []
        self.guard = threading.Lock()

    def count_start(self, argv, env):
        with self.guard:
            self.starts.append((argv, env))

    def rows(self, *slots):
        return {"accounts": [{"provider": "claude", "slot": s, "source": "native/claude"} for s in slots]}

    def attach(self, now, *slots):
        with patch.object(cost, "spawn", self.count_start):
            return cost.attach(self.rows(*(slots or ("a",))), self.runtime, env=self.env, now=now)

    def status(self):
        return json.loads((self.f.root / "state/usage-cost-status.json").read_text())["limits_step"]

    def test_attach_reads_stored_totals_and_never_runs_a_pass_here(self):
        f = self.f
        f.write("a", "p/s.jsonl", claude("m1", f.now - 120))
        f.collect("a", now=f.now)
        f.write("a", "p/s.jsonl", claude("m2", f.now - 60), mode="a")  # not yet read by any pass
        opened = []
        real_open, real_os_open = open, os.open

        def watch_open(path, *args, **kwargs):
            opened.append(str(path))
            return real_open(path, *args, **kwargs)

        def watch_os_open(path, *args, **kwargs):
            opened.append(str(path))
            return real_os_open(path, *args, **kwargs)

        with patch("builtins.open", watch_open), patch("os.open", watch_os_open), \
             patch.object(cost.Pass, "run", side_effect=AssertionError("a pass ran in sb limits")), \
             patch.object(cost, "transcripts", side_effect=AssertionError("a transcript was listed")), \
             patch.object(cost, "collect", side_effect=AssertionError("a pass ran in sb limits")):
            result = self.attach(f.now + 300)  # fresh enough to show, old enough to start a pass
        self.assertEqual(result["accounts"][0]["cost"]["today"]["tokens"], 1100)
        self.assertFalse([p for p in opened if p.endswith(".jsonl")], opened)
        self.assertEqual(len(self.starts), 1)
        argv, env = self.starts[0]
        self.assertEqual((Path(argv[2]).name, argv[3]), ("native_account_cost.py", "pass"))
        self.assertEqual(self.status()["start"], "started")

    def test_only_a_fresh_pass_from_today_attaches(self):
        f = self.f
        f.write("a", "p/s.jsonl", claude("m1", f.now - 120))
        f.collect("a", now=f.now)
        self.assertIn("cost", self.attach(f.now + 60)["accounts"][0])
        # sampled_at is kept to the second, so the limit is exact to one second.
        self.assertIn("cost", self.attach(f.now + cost.FRESH_SECONDS - 1)["accounts"][0])
        late = self.attach(f.now + cost.FRESH_SECONDS + 1)
        self.assertNotIn("cost", late["accounts"][0])
        self.assertNotIn("cost_summary", late)
        self.assertEqual(self.status()["state"], "stale")
        # Three minutes old, but from yesterday: not today's figure.
        g = Fixture(self)
        midnight = cost.bounds(time.time())[0]
        g.write("a", "p/s.jsonl", claude("m1", midnight - 200))
        g.collect("a", now=midnight - 120)
        self.env["SB_USAGE_COSTS_STATE"] = str(g.state)
        self.runtime = str(g.root)
        crossed = self.attach(midnight + 60)
        self.assertNotIn("cost", crossed["accounts"][0])
        self.assertNotIn("cost_summary", crossed)
        self.assertIn("cost", self.attach(midnight - 60)["accounts"][0])

    def test_stale_or_missing_totals_start_exactly_one_pass(self):
        f = self.f
        self.attach(f.now)  # no state at all
        self.assertEqual(len(self.starts), 1)
        self.attach(f.now + 1)
        self.assertEqual(len(self.starts), 1)
        f.write("a", "p/s.jsonl", claude("m1", f.now - 120))
        f.collect("a", now=f.now)
        self.attach(f.now + cost.START_EVERY - 1)  # not stale yet
        self.assertEqual(len(self.starts), 1)
        self.attach(f.now + cost.START_EVERY + 1)  # stale, and the last start is old
        self.assertEqual(len(self.starts), 2)
        self.attach(f.now + cost.START_EVERY + 2)
        self.assertEqual(len(self.starts), 2)
        self.assertEqual(self.status()["start"], "started_recently")
        self.assertEqual(cost.read_stamp(f.state)["starts"], 2)

    def test_thirty_callers_at_once_start_one_pass(self):
        f = self.f
        f.write("a", "p/s.jsonl", claude("m1", f.now - 120))
        f.collect("a", now=f.now)
        gate = threading.Barrier(30)
        results = []

        def slow_start(argv, env):
            time.sleep(0.05)  # the decider holds its lock a little longer
            self.count_start(argv, env)

        def caller():
            gate.wait()
            results.append(cost.attach(self.rows("a"), self.runtime, env=self.env, now=f.now + 300))

        with patch.object(cost, "spawn", slow_start):
            threads = [threading.Thread(target=caller) for _ in range(30)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join(10)
        self.assertEqual(len(results), 30)
        self.assertEqual(len(self.starts), 1)
        self.assertTrue(all("cost" in r["accounts"][0] for r in results))

    def test_a_failed_start_still_holds_its_slot(self):
        f = self.f
        calls = []

        def broken(argv, env):
            calls.append(argv)
            raise OSError("no process")

        with patch.object(cost, "spawn", broken):
            cost.attach(self.rows("a"), self.runtime, env=self.env, now=f.now)
            self.assertEqual(self.status()["start"], "start_failed")
            cost.attach(self.rows("a"), self.runtime, env=self.env, now=f.now + cost.START_EVERY - 1)
            self.assertEqual(len(calls), 1)
            cost.attach(self.rows("a"), self.runtime, env=self.env, now=f.now + cost.START_EVERY + 1)
            self.assertEqual(len(calls), 2)

    def test_started_pass_exits_at_once_when_another_pass_holds_the_lock(self):
        f = self.f
        f.write("a", "p/s.jsonl", claude("m1", f.now - 120))
        with cost.exclusive(f.state) as held, \
             patch.object(cost, "homes", return_value=f.homes("a")), \
             patch.object(cost, "connect", side_effect=AssertionError("opened the state while busy")), \
             patch.object(cost.Pass, "run", side_effect=AssertionError("ran while busy")):
            self.assertTrue(held)
            started = time.monotonic()
            result = cost.background(self.runtime, self.env)
            took = time.monotonic() - started
        self.assertEqual(result["pass_state"], "busy")
        self.assertLess(took, 0.5)

    def test_cut_off_background_pass_keeps_the_last_complete_totals_attachable(self):
        f = self.f
        f.write("a", "p/one.jsonl", claude("m1", f.now - 300))
        f.collect("a", now=f.now)
        f.write("a", "p/one.jsonl", claude("m2", f.now - 200, out=900), mode="a")
        f.write("a", "p/two.jsonl", claude("m3", f.now - 100))
        real_read = cost.Pass.read

        def read_then_stop(pass_, *args):
            finished = real_read(pass_, *args)
            pass_.deadline = float("-inf")  # the time limit hits after this file
            return finished

        with patch.object(cost, "homes", return_value=f.homes("a")), patch.object(cost.Pass, "read", read_then_stop):
            result = cost.background(self.runtime, self.env)
        self.assertEqual(result["pass_state"], "incomplete")
        self.assertGreater(result["pass"]["messages_new"], 0)  # it did read new data
        shown = self.attach(f.now + 250)["accounts"][0]["cost"]
        self.assertEqual(shown["today"]["tokens"], 1100)  # the last complete pass, unchanged
        self.assertEqual(shown["sampled_at"], cost.iso(f.now))
        self.assertEqual(len(self.starts), 1)  # and the next pass is started

    def test_idle_window_is_left_out_of_rows_but_kept_in_the_summary(self):
        f = self.f
        f.write("b", "p/s.jsonl", claude("m1", f.now - 3 * 86400))
        f.home("c")
        f.collect("b", "c", now=f.now)
        result = self.attach(f.now + 60, "b", "c")
        b, c = result["accounts"]
        self.assertEqual(set(b["cost"]) & set(cost.WINDOWS), {"last_7_days"})
        self.assertNotIn("cost", c)
        self.assertEqual(result["cost_summary"][0]["today"], {"amount": 0.0, "tokens": 0})
        self.assertEqual(result["cost_summary"][0]["last_7_days"]["tokens"], 1100)

    def test_homes_and_owners_do_not_depend_on_claude_config_dir(self):
        root = self.f.root
        user = root / "user"
        for name, uuid in (("seat1", "acct-1"), ("seat2", "acct-2")):
            slot = root / "config/native-account-slots/claude" / name
            slot.mkdir(parents=True)
            (slot / "slot.json").write_text(json.dumps({"provider": "claude", "slot": name,
                                                        "identity": {"account_uuid": uuid}}))
        codex_slot = root / "config/native-account-slots/codex/cx"
        codex_slot.mkdir(parents=True)
        (codex_slot / "slot.json").write_text(json.dumps({"provider": "codex", "slot": "cx",
                                                          "native_home": {"kind": "existing_default"}}))
        seat1 = root / "config/claude/seat1"
        for home, uuid in ((seat1, "acct-1"), (root / "config/claude/seat2", "acct-1"), (user / ".claude", "acct-2")):
            (home / "projects/p").mkdir(parents=True)
            record = user / ".claude.json" if home == user / ".claude" else home / ".claude.json"
            record.write_text(json.dumps({"oauthAccount": {"accountUuid": uuid}}))
        (user / ".claude/projects/p/s.jsonl").write_text(claude("m1", self.f.now - 60))
        (seat1 / "projects/p/t.jsonl").write_text(claude("m2", self.f.now - 60))
        (user / ".codex/sessions").mkdir(parents=True)
        other = root / "elsewhere"
        (other / "sessions").mkdir(parents=True)
        (other / "auth.json").write_text("{}")
        bar = {"HOME": str(user), "PATH": "/usr/bin:/bin"}
        shell = {**bar, "CLAUDE_CONFIG_DIR": str(seat1), "CODEX_HOME": str(other), "SB_USAGE_COSTS": "1"}
        with patch.dict(os.environ, {"HOME": str(user)}):
            from_bar, from_shell = cost.homes(self.runtime, bar), cost.homes(self.runtime, shell)
            self.assertEqual(from_bar, from_shell)
            self.assertEqual(cost.spawn_env(self.runtime, bar), cost.spawn_env(self.runtime, shell))
            self.assertNotIn("CLAUDE_CONFIG_DIR", cost.spawn_env(self.runtime, shell))
            owners = {(h[0], h[1]): h[3] for h in from_shell}
            self.assertEqual(owners, {("claude", "seat1"): "seat1", ("claude", "seat2"): "seat1",
                                      ("claude", "default"): "seat2", ("codex", "cx"): "cx"})
            totals = [cost.collect(root / name, home_list, now=self.f.now)["providers"]
                      for name, home_list in (("bar.sqlite", from_bar), ("shell.sqlite", from_shell))]
        self.assertEqual(totals[0], totals[1])


if __name__ == "__main__":
    unittest.main()
