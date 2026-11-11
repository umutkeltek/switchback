#!/usr/bin/env python3
"""A saved Claude account starts with the default profile's setup."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
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
