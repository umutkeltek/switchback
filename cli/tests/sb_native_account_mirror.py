#!/usr/bin/env python3
"""A saved Claude account starts with the default profile's setup."""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
MIRROR = CLI / "native_account_mirror.py"


class MirrorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sb-mirror-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.runtime = self.home / ".switchback"
        self.default = self.home / ".claude"
        self.seat = self.runtime / "config/claude/seat1"
        self.lane = self.runtime / "config/claude/qwen-tap"
        for path in (self.default / "skills/real-skill", self.default / "agents", self.seat, self.lane):
            path.mkdir(parents=True, exist_ok=True)
        (self.runtime / "manifest.json").write_text("{}")
        self.settings = {"model": "opus", "theme": "dark", "env": {"FOO": "1"},
                         "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "true"}]}]}}
        self.write(self.default / "settings.json", self.settings)
        (self.default / "keybindings.json").write_text('{"bindings": []}\n')
        (self.default / "skills/real-skill/SKILL.md").write_text("skill")
        (self.default / "agents/reviewer.md").write_text("agent")
        (self.default / "skills/.DS_Store").write_text("x")
        self.write(self.home / ".claude.json", {"mcpServers": {"search": {"command": "search-mcp"}}, "numStartups": 9})
        self.write(self.seat / "settings.json", {"model": "fable", "theme": "dark", "seatOnly": True})
        self.write(self.seat / ".claude.json", {"oauthAccount": {"emailAddress": "seat@example.invalid"},
                                               "projects": {"/repo": {"x": 1}}})
        self.write(self.lane / "settings.json", {"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:1"}})
        self.slot("seat1")
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("SB_", "SWITCHBACK_", "CLAUDE_", "ANTHROPIC_"))}
        self.env.update(HOME=str(self.home), SWITCHBACK_RUNTIME_ROOT=str(self.runtime))

    def write(self, path, value):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value, indent=2) + "\n")
        path.chmod(0o600)

    def slot(self, name, **extra):
        record = {"slot": name, "provider": "claude", "enabled": True,
                  "identity": {"account_uuid": name + "-uuid"}, **extra}
        self.write(self.runtime / "config/native-account-slots/claude" / name / "slot.json", record)

    def mirror(self, slot="seat1"):
        result = subprocess.run([sys.executable, "-B", str(MIRROR), "--slot", slot, "--json"],
                                env=self.env, capture_output=True, text=True, timeout=20)
        return result.returncode, (json.loads(result.stdout) if result.stdout.strip() else None), result.stderr

    def test_account_gets_default_setup_and_local_settings_are_kept_in_a_backup(self):
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertEqual((self.seat / "settings.json").read_bytes(), (self.default / "settings.json").read_bytes())
        self.assertEqual(report["settings_reset"], ["model", "seatOnly"])
        saved = json.loads(Path(report["backup"]).read_text())
        self.assertEqual(saved["model"], "fable")
        self.assertEqual((self.seat / "keybindings.json").read_text(), '{"bindings": []}\n')
        self.assertEqual(os.readlink(self.seat / "skills/real-skill"), str(self.default / "skills/real-skill"))
        self.assertEqual((self.seat / "agents/reviewer.md").read_text(), "agent")
        self.assertFalse(os.path.lexists(self.seat / "skills/.DS_Store"))
        config = json.loads((self.seat / ".claude.json").read_text())
        self.assertEqual(config["mcpServers"], {"search": {"command": "search-mcp"}})
        self.assertEqual(config["oauthAccount"]["emailAddress"], "seat@example.invalid")
        self.assertEqual(config["projects"], {"/repo": {"x": 1}})
        self.assertNotIn("numStartups", config)
        self.assertEqual((self.seat / "settings.json").stat().st_mode & 0o777, 0o600)
        rc, again, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertEqual((again["copied"], again["linked"]), ([], {}))
        relaid = json.dumps(json.loads((self.seat / "settings.json").read_text()), sort_keys=True)
        (self.seat / "settings.json").write_text(relaid)
        rc, again, err = self.mirror()
        self.assertEqual((rc, again["copied"]), (0, []), err)
        self.assertEqual((self.seat / "settings.json").read_text(), relaid)

    def test_default_changes_follow_and_removed_entries_go(self):
        self.mirror()
        self.settings["model"] = "fable"
        self.write(self.default / "settings.json", self.settings)
        (self.default / "agents/reviewer.md").unlink()
        (self.default / "agents/planner.md").write_text("plan")
        (self.seat / "agents/mine.md").write_text("account only")
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertEqual(json.loads((self.seat / "settings.json").read_text())["model"], "fable")
        self.assertNotIn("settings_reset", report)
        self.assertFalse(os.path.lexists(self.seat / "agents/reviewer.md"))
        self.assertEqual((self.seat / "agents/planner.md").read_text(), "plan")
        self.assertEqual((self.seat / "agents/mine.md").read_text(), "account only")
        edited = json.loads((self.seat / "settings.json").read_text())
        edited["model"] = "sonnet"
        self.write(self.seat / "settings.json", edited)
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertEqual(report["settings_reset"], ["model"])
        self.assertEqual(json.loads(Path(report["backup"]).read_text())["model"], "sonnet")
        self.assertEqual(json.loads((self.seat / "settings.json").read_text())["model"], "fable")

    def test_default_with_a_credential_or_route_override_is_not_copied(self):
        for bad in ({"apiKeyHelper": "/bin/echo"}, {"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:1"}},
                    {"env": {"CLAUDE_CONFIG_DIR": "/tmp/x"}}):
            with self.subTest(bad=bad):
                before = (self.seat / "settings.json").read_bytes()
                self.write(self.default / "settings.json", {**self.settings, **bad})
                rc, _, err = self.mirror()
                self.assertEqual(rc, 1)
                self.assertIn("not copied", err)
                self.assertEqual((self.seat / "settings.json").read_bytes(), before)

    def test_only_saved_subscription_accounts_are_touched(self):
        before = (self.lane / "settings.json").read_bytes()
        rc, report, err = self.mirror("qwen-tap")
        self.assertEqual(rc, 0, err)
        self.assertIn("skipped", report)
        self.assertEqual((self.lane / "settings.json").read_bytes(), before)
        self.slot("seat1", enabled=False)
        rc, report, _ = self.mirror()
        self.assertIn("skipped", report)
        self.assertEqual(json.loads((self.seat / "settings.json").read_text())["model"], "fable")

    def test_links_and_held_config_lock_are_respected(self):
        (self.seat / "settings.json").unlink()
        (self.seat / "settings.json").symlink_to(self.root / "elsewhere.json")
        rc, _, err = self.mirror()
        self.assertEqual(rc, 1)
        self.assertTrue((self.seat / "settings.json").is_symlink())
        (self.seat / "settings.json").unlink()
        (self.seat / ".claude.json.lock").mkdir()
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertNotIn("mcpServers", report["copied"])
        self.assertTrue(any("MCP servers not copied" in note for note in report["notes"]))
        self.assertNotIn("mcpServers", json.loads((self.seat / ".claude.json").read_text()))

    def test_a_held_mirror_lock_is_waited_on_briefly_then_skipped(self):
        before = (self.seat / "settings.json").read_bytes()
        with open(self.seat / ".switchback-default-mirror.lock", "a") as guard:
            fcntl.flock(guard, fcntl.LOCK_EX)
            started = time.monotonic()
            result = subprocess.run([sys.executable, "-B", str(MIRROR), "--slot", "seat1"],
                                    env=self.env, capture_output=True, text=True, timeout=15)
            waited = time.monotonic() - started
            rc, report, _ = self.mirror()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(waited, 12)
        lines = result.stderr.splitlines()
        self.assertEqual(len(lines), 1, result.stderr)
        self.assertIn("default setup not copied to seat1", lines[0])
        self.assertEqual(rc, 0)
        self.assertTrue(report["busy"])
        self.assertEqual(report["copied"], [])
        self.assertEqual((self.seat / "settings.json").read_bytes(), before)
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertNotIn("busy", report)
        self.assertIn("settings.json", report["copied"])

    def test_default_env_is_linked_only_where_the_account_has_nothing(self):
        source = self.default / ".env"
        source.write_text("FIXTURE_NAME=fixture-value\n")
        source.chmod(0o600)
        target = self.seat / ".env"
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertTrue(target.is_symlink())
        self.assertEqual(os.readlink(target), str(source))
        self.assertIn(".env (link)", report["copied"])
        # A second run finds the link it made and says nothing about it.
        rc, again, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertNotIn(".env (link)", again["copied"])
        self.assertFalse(any(".env" in note for note in again["notes"]))

    def test_account_own_env_file_is_left_alone(self):
        (self.default / ".env").write_text("FIXTURE_NAME=default\n")
        target = self.seat / ".env"
        target.write_text("FIXTURE_NAME=account\n")
        target.chmod(0o600)
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertFalse(target.is_symlink())
        self.assertEqual(target.read_text(), "FIXTURE_NAME=account\n")
        self.assertNotIn(".env (link)", report["copied"])
        self.assertIn("account has its own .env; left alone", report["notes"])

    def test_account_env_link_to_elsewhere_is_left_alone_and_noted(self):
        (self.default / ".env").write_text("FIXTURE_NAME=default\n")
        elsewhere = self.root / "other.env"
        elsewhere.write_text("FIXTURE_NAME=other\n")
        target = self.seat / ".env"
        target.symlink_to(elsewhere)
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertEqual(os.readlink(target), str(elsewhere))
        self.assertEqual(elsewhere.read_text(), "FIXTURE_NAME=other\n")
        self.assertNotIn(".env (link)", report["copied"])
        self.assertIn(".env in the account links elsewhere; left alone", report["notes"])

    def test_no_default_env_means_no_account_env(self):
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertFalse(os.path.lexists(self.seat / ".env"))
        self.assertNotIn(".env (link)", report["copied"])

    def test_lsp_config_is_copied_and_a_local_edit_is_saved_first(self):
        self.write(self.default / ".lsp.json", {"servers": {"go": {"command": "gopls"}}})
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertIn(".lsp.json", report["copied"])
        self.assertFalse((self.seat / ".lsp.json").is_symlink())
        self.assertEqual((self.seat / ".lsp.json").read_bytes(), (self.default / ".lsp.json").read_bytes())
        self.write(self.seat / ".lsp.json", {"servers": {"py": {"command": "pyright"}}})
        rc, report, err = self.mirror()
        self.assertEqual(rc, 0, err)
        self.assertIn(".lsp.json", report["copied"])
        self.assertEqual((self.seat / ".lsp.json").read_bytes(), (self.default / ".lsp.json").read_bytes())
        saved = sorted((self.seat / "backups").glob(".lsp.json.*.before-default"))
        self.assertEqual(len(saved), 1)
        self.assertEqual(json.loads(saved[0].read_text()), {"servers": {"py": {"command": "pyright"}}})

    def sb_mirror(self, *args, env=None):
        return subprocess.run(["/bin/zsh", str(SB), "claude", "mirror", *args], env=env or self.env,
                              capture_output=True, text=True, timeout=30)

    def test_sb_claude_mirror_copies_one_account_without_a_launch(self):
        result = self.sb_mirror("--account", "seat1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("default setup copied to seat1", result.stderr)
        self.assertEqual((self.seat / "settings.json").read_bytes(), (self.default / "settings.json").read_bytes())
        self.assertEqual(json.loads((self.seat / ".claude.json").read_text())["mcpServers"],
                         {"search": {"command": "search-mcp"}})
        # A server added to the default later reaches the account on the next call.
        self.write(self.home / ".claude.json", {"mcpServers": {"search": {"command": "search-mcp"},
                                                               "fresh": {"command": "fresh-mcp"}}})
        result = self.sb_mirror("--account=seat1", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["copied"], ["mcpServers"])
        self.assertEqual(sorted(json.loads((self.seat / ".claude.json").read_text())["mcpServers"]), ["fresh", "search"])

    def test_sb_claude_mirror_all_covers_every_saved_account(self):
        seat2 = self.runtime / "config/claude/seat2"
        seat2.mkdir(parents=True)
        self.write(seat2 / ".claude.json", {"oauthAccount": {"emailAddress": "two@example.invalid"}})
        self.slot("seat2")
        off = self.runtime / "config/claude/seat3"
        off.mkdir(parents=True)
        self.slot("seat3", enabled=False)
        result = self.sb_mirror("--all", "--json")
        self.assertEqual(result.returncode, 0, result.stderr)
        reports = {r["slot"]: r for r in json.loads(result.stdout)}
        self.assertEqual(sorted(reports), ["seat1", "seat2", "seat3"])
        self.assertIn("settings.json", reports["seat1"]["copied"])
        self.assertIn("mcpServers", reports["seat2"]["copied"])
        self.assertIn("skipped", reports["seat3"])
        self.assertFalse((off / "settings.json").exists())
        for home in (self.seat, seat2):
            self.assertEqual((home / "settings.json").read_bytes(), (self.default / "settings.json").read_bytes())
            self.assertIn("search", json.loads((home / ".claude.json").read_text())["mcpServers"])

    def test_sb_claude_mirror_all_reports_a_refused_account_and_still_does_the_rest(self):
        bad = self.runtime / "config/claude/seat0"
        bad.mkdir(parents=True)
        (bad / "settings.json").symlink_to(self.root / "elsewhere.json")
        self.slot("seat0")
        result = self.sb_mirror("--all", "--json")
        self.assertEqual(result.returncode, 1)
        reports = {r["slot"]: r for r in json.loads(result.stdout)}
        self.assertIn("error", reports["seat0"])
        self.assertIn("default setup not copied to seat0", result.stderr)
        self.assertEqual((self.seat / "settings.json").read_bytes(), (self.default / "settings.json").read_bytes())

    def test_sb_claude_mirror_respects_the_off_switch_and_bad_usage(self):
        before = (self.seat / "settings.json").read_bytes()
        result = self.sb_mirror("--account", "seat1", env=dict(self.env, SB_CLAUDE_MIRROR_DEFAULT="0"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("SB_CLAUDE_MIRROR_DEFAULT=0", result.stderr)
        self.assertEqual((self.seat / "settings.json").read_bytes(), before)
        for bad in ((), ("--all", "--account", "seat1"), ("--account",), ("--bogus",)):
            with self.subTest(bad=bad):
                result = self.sb_mirror(*bad)
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertIn("usage: sb claude mirror", result.stderr)
        self.assertEqual((self.seat / "settings.json").read_bytes(), before)
        result = self.sb_mirror("--account", "../seat1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("seat1", result.stderr)

    def test_account_launch_copies_setup_and_pool_runs_opt_out(self):
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        log = self.root / "launch.json"
        fake = bin_dir / "claude"
        fake.write_text(f"#!{sys.executable}\nimport json, os, sys\nfrom pathlib import Path\n"
                        "home = Path(os.environ['CLAUDE_CONFIG_DIR'])\n"
                        f"Path({str(log)!r}).write_text(json.dumps({{'model': json.loads((home / 'settings.json').read_text()).get('model')}}))\n")
        fake.chmod(0o700)
        (bin_dir / "python3").symlink_to(sys.executable)
        env = dict(self.env, PATH=f"{bin_dir}:/usr/bin:/bin", SB_NATIVE_CLAUDE=str(fake),
                   SB_DEFAULT_CLAUDE_MODE="native", SB_DEFAULT_CLAUDE_ACCOUNT="default")
        command = ["/bin/zsh", str(SB), "claude", "--mode", "native", "--account", "seat1", "--print", "hi"]
        opted_out = subprocess.run(command, env=dict(env, SB_CLAUDE_MIRROR_DEFAULT="0"),
                                   capture_output=True, text=True, timeout=30)
        self.assertEqual(opted_out.returncode, 0, opted_out.stderr)
        self.assertEqual(json.loads(log.read_text())["model"], "fable")
        result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(log.read_text())["model"], "opus")
        self.assertIn("default setup copied to seat1", result.stderr)


if __name__ == "__main__":
    unittest.main()
