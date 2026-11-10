#!/usr/bin/env python3
"""Exercise terminal selection and unattended launches without vendor processes."""
import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import tempfile
import time
import unittest


SB = Path(__file__).resolve().parents[1] / "sb"


class AccountPickerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sb-account-picker-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.runtime = self.home / ".switchback"
        self.profiles = self.runtime / "config/claude"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.profiles.mkdir(parents=True)
        (self.home / ".claude").mkdir()
        (self.runtime / "manifest.json").write_text("{}")
        self.log = self.root / "launched.json"
        self.menu = self.root / "choices.txt"
        self.identity("default", "default@example.invalid")
        self.identity("seat1", "first@example.invalid")
        self.identity("seat2", "second@example.invalid")
        (self.profiles / "not-logged-in").mkdir()
        self.program("claude", """
import json, os, sys
from pathlib import Path
Path(os.environ['PICKER_LAUNCH_LOG']).write_text(json.dumps({
    'config': os.environ.get('CLAUDE_CONFIG_DIR'),
    'proxy': os.environ.get('HTTPS_PROXY'), 'args': sys.argv[1:],
    'oauth_override': os.environ.get('CLAUDE_CODE_OAUTH_TOKEN'),
    'secure_storage_override': os.environ.get('CLAUDE_SECURESTORAGE_CONFIG_DIR')}))
""")
        self.program("fzf", """
import os, sys
from pathlib import Path
choices = sys.stdin.read()
Path(os.environ['PICKER_MENU_LOG']).write_text(choices)
choice = os.environ.get('PICKER_CHOICE', 'seat2')
if choice == 'cancel':
    sys.exit(130)
if choice == 'first':
    print(choices.splitlines()[0])
else:
    print(next(row for row in choices.splitlines() if row.split('\\t')[0] == choice))
""")
        (self.bin / "python3").symlink_to(sys.executable)
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("SB_", "SWITCHBACK_", "CLAUDE_", "ANTHROPIC_"))}
        self.env.update(HOME=str(self.home), PATH=f"{self.bin}:/usr/bin:/bin",
                        SWITCHBACK_RUNTIME_ROOT=str(self.runtime),
                        SB_NATIVE_CLAUDE=str(self.bin / "claude"),
                        SB_DEFAULT_CLAUDE_MODE="native", SB_DEFAULT_CLAUDE_ACCOUNT="default",
                        PICKER_LAUNCH_LOG=str(self.log), PICKER_MENU_LOG=str(self.menu))
        self.command = ["/bin/zsh", str(SB), "claude"]

    def identity(self, slot, email):
        path = self.home / ".claude.json" if slot == "default" else self.profiles / slot / ".claude.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({"oauthAccount": {"emailAddress": email}}))

    def program(self, name, body):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + body)
        path.chmod(0o700)

    def launch(self, args=(), terminal=True, answer=None):
        cmd = [*self.command, *args]
        if not terminal:
            result = subprocess.run(cmd, env=self.env, capture_output=True, timeout=8)
            return result.returncode, result.stdout + result.stderr
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(cmd[0], cmd, self.env)
        output = b""
        deadline = time.monotonic() + 8
        try:
            while time.monotonic() < deadline:
                if select.select([fd], [], [], 0.1)[0]:
                    try:
                        chunk = os.read(fd, 65536)
                    except OSError as exc:
                        if exc.errno == errno.EIO:
                            break
                        raise
                    if not chunk:
                        break
                    output += chunk
                    if answer is not None and 'account › '.encode() in output:
                        os.write(fd, answer)
                        answer = None
                done, status = os.waitpid(pid, os.WNOHANG)
                if done:
                    return os.waitstatus_to_exitcode(status), output
            else:
                self.fail(f"CLI hung: {output.decode(errors='replace')}")
            _, status = os.waitpid(pid, 0)
            return os.waitstatus_to_exitcode(status), output
        finally:
            os.close(fd)
            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except ProcessLookupError:
                pass
            except ChildProcessError:
                pass

    def test_terminal_selection_uses_isolated_profile_and_preserves_capture_mode(self):
        before = {p: p.read_bytes() for p in self.home.rglob("*.json")}
        rc, output = self.launch(["--mode", "remote"])
        self.assertEqual(rc, 0, output)
        self.assertEqual(json.loads(self.log.read_text())["config"], str(self.profiles / "seat2"))
        self.assertTrue(json.loads(self.log.read_text())["proxy"].endswith(":18780"))
        self.assertIn("second@example.invalid", self.menu.read_text())
        self.assertNotIn("not-logged-in", self.menu.read_text())
        self.assertEqual(before, {p: p.read_bytes() for p in self.home.rglob("*.json")})

    def test_explicit_account_and_vendor_arguments_never_prompt(self):
        for args in (["--account", "seat1"], ["--account=default"], ["--print", "task"],
                     ["auth", "status"], ["--version"], ["--resume"], ["--", "task"]):
            with self.subTest(args=args):
                rc, output = self.launch(args)
                self.assertEqual(rc, 0, output)
                self.assertFalse(self.menu.exists())

    def test_noninteractive_launch_never_prompts(self):
        rc, output = self.launch(terminal=False)
        self.assertEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())
        self.assertIsNone(json.loads(self.log.read_text())["config"])

    def test_cancellation_never_launches_or_changes_credentials(self):
        self.env["PICKER_CHOICE"] = "cancel"
        rc, _ = self.launch()
        self.assertEqual(rc, 130)
        self.assertFalse(self.log.exists())

    def test_numbered_fallback_uses_same_account_selection(self):
        (self.bin / "fzf").unlink()
        rc, output = self.launch(answer=b"3\n")
        self.assertEqual(rc, 0, output)
        self.assertIn(b"second@example.invalid", output)
        self.assertEqual(json.loads(self.log.read_text())["config"], str(self.profiles / "seat2"))

    def test_numbered_fallback_out_of_range_does_not_launch(self):
        (self.bin / "fzf").unlink()
        rc, output = self.launch(answer=b"999\n")
        self.assertEqual(rc, 130, output)
        self.assertFalse(self.log.exists())
        self.assertNotIn(b"parameter not set", output)

    def test_numbered_fallback_cancel_does_not_launch(self):
        (self.bin / "fzf").unlink()
        rc, _ = self.launch(answer=b"\n")
        self.assertEqual(rc, 130)
        self.assertFalse(self.log.exists())

    def test_opt_out_uses_default(self):
        self.env["SB_CLAUDE_ACCOUNT_PICKER"] = "0"
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())

    def test_invalid_mode_is_rejected_without_prompting(self):
        rc, output = self.launch(["--mode", "invalid"])
        self.assertNotEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())
        self.assertFalse(self.log.exists())

    def test_configured_default_is_first(self):
        self.env.update(SB_DEFAULT_CLAUDE_ACCOUNT="seat1", PICKER_CHOICE="first")
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertTrue(self.menu.read_text().startswith("seat1\t"))
        self.assertEqual(json.loads(self.log.read_text())["config"], str(self.profiles / "seat1"))

    def test_choose_account_works_with_continue_and_picker_opt_out(self):
        self.env.update(SB_CLAUDE_ACCOUNT_PICKER="0", SB_DEFAULT_CLAUDE_PROFILE="other-route")
        rc, output = self.launch(["--choose-account", "--continue"])
        self.assertEqual(rc, 0, output)
        launched = json.loads(self.log.read_text())
        self.assertEqual(launched["config"], str(self.profiles / "seat2"))
        self.assertIn("--continue", launched["args"])
        self.assertNotIn("--choose-account", launched["args"])

    def test_choose_account_refuses_without_terminal_or_with_conflicting_account(self):
        for args, terminal in ((["--choose-account"], False),
                               (["--choose-account", "--account", "seat1"], True)):
            with self.subTest(args=args, terminal=terminal):
                rc, output = self.launch(args, terminal=terminal)
                self.assertNotEqual(rc, 0, output)
                self.assertFalse(self.log.exists())
                self.assertFalse(self.menu.exists())

    def test_selected_profile_clears_inherited_native_login_overrides(self):
        self.env.update(CLAUDE_CONFIG_DIR=str(self.profiles / "seat2"),
                        CLAUDE_CODE_OAUTH_TOKEN="synthetic-unrelated-login",
                        CLAUDE_SECURESTORAGE_CONFIG_DIR=str(self.profiles / "seat2"))
        rc, output = self.launch(["--account", "default"])
        self.assertEqual(rc, 0, output)
        launched = json.loads(self.log.read_text())
        self.assertIsNone(launched["config"])
        self.assertIsNone(launched["oauth_override"])
        self.assertIsNone(launched["secure_storage_override"])

    def test_explicit_account_bypasses_default_launch_profile(self):
        self.env["SB_DEFAULT_CLAUDE_PROFILE"] = "other-route"
        self.env["SB_DEFAULT_CLAUDE_ACCOUNT"] = "seat1"
        rc, output = self.launch(["--account", "seat1"])
        self.assertEqual(rc, 0, output)
        self.assertEqual(json.loads(self.log.read_text())["config"], str(self.profiles / "seat1"))

    def changed_login(self):
        binding = self.runtime / "config/native-account-slots/claude/seat1/slot.json"
        binding.parent.mkdir(parents=True)
        binding.write_text(json.dumps({"provider": "claude", "slot": "seat1", "enabled": True,
            "identity": {"account_uuid": "original-account", "organization_uuid": "original-org"}}))
        # Simulate Claude's in-session /login writing its native metadata.
        for target in (self.profiles / "seat1/.claude.json", self.profiles / "seat2/.claude.json"):
            target.write_text(json.dumps({"oauthAccount": {"emailAddress": "new@example.invalid",
                "accountUuid": "new-account", "organizationUuid": "new-org",
                "organizationName": "Example Organization"}, "unrelatedSecret": "must-not-escape"}))
        return binding

    def test_native_login_change_is_observed_without_rebinding_or_hiding_profiles(self):
        binding = self.changed_login()
        before = {p: p.read_bytes() for p in self.home.rglob("*.json")}
        command = [str(SB), "claude", "accounts", "--json"]
        result = subprocess.run(command, env=self.env, capture_output=True, text=True, timeout=8)
        self.assertEqual(result.returncode, 0, result.stderr)
        rows = {row["slot"]: row for row in json.loads(result.stdout)["accounts"]}
        row = rows["seat1"]
        self.assertEqual(row["identity_state"], "changed")
        self.assertEqual(row["email"], "new@example.invalid")
        self.assertEqual(row["identity"]["organization_uuid"], "new-org")
        self.assertEqual(row["bound_identity"]["account_uuid"], "original-account")
        self.assertIn("seat2", row["same_identity_profiles"])
        self.assertEqual(rows["default"]["profile_kind"], "native_default")
        self.assertNotIn("must-not-escape", result.stdout)
        self.assertEqual(before, {p: p.read_bytes() for p in self.home.rglob("*.json")})
        status = subprocess.run([str(SB), "account", "status", "--provider", "claude",
            "--slot", "seat1", "--json"], env=self.env, capture_output=True, text=True, timeout=8)
        self.assertEqual(status.returncode, 0, status.stderr)
        self.assertEqual(json.loads(status.stdout)["identity_state"], "changed")
        usage = subprocess.run([str(SB), "limits", "--provider", "claude", "--slot", "seat1", "--json"],
            env=self.env, capture_output=True, text=True, timeout=8)
        self.assertEqual(usage.returncode, 0, usage.stderr)
        current = json.loads(usage.stdout)["accounts"][0]
        self.assertEqual(current["identity_state"], "changed")
        self.assertEqual(current["observed_identity"]["account_uuid"], "new-account")
        self.assertEqual(current["refresh_error"], "identity_mismatch")
        self.assertEqual(current["windows"], [])
        rc, output = self.launch(["--account", "seat1"])
        self.assertEqual(rc, 0, output)
        self.assertIn(b"new@example.invalid", output)
        self.assertIn(b"login changed", output)
        self.assertEqual(json.loads(binding.read_text())["identity"]["account_uuid"], "original-account")

    def test_one_saved_identity_skips_menu(self):
        for slot in ("seat1", "seat2"):
            (self.profiles / slot / ".claude.json").unlink()
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())

    def test_duplicate_identity_keeps_each_native_home_selectable(self):
        for path in (self.home / '.claude.json', self.profiles / 'seat2/.claude.json'):
            path.write_text(json.dumps({'oauthAccount': {'emailAddress': 'same@example.invalid',
                'accountUuid': 'person-one', 'organizationUuid': 'organization-one'}}))
        self.env['SB_DEFAULT_CLAUDE_ACCOUNT'] = 'seat1'
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        choices = self.menu.read_text()
        self.assertIn('default\t', choices)
        self.assertIn('same login as', choices)
        self.assertIn('seat2\t', choices)
        self.env['SB_DEFAULT_CLAUDE_ACCOUNT'] = 'default'
        self.env['PICKER_CHOICE'] = 'first'
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertTrue(self.menu.read_text().startswith('default\t'))
        self.assertIn('seat2\t', self.menu.read_text())

    def test_same_person_in_different_organizations_remains_selectable(self):
        for slot, org in (('seat1', 'organization-one'), ('seat2', 'organization-two')):
            (self.profiles / slot / '.claude.json').write_text(json.dumps({'oauthAccount': {
                'emailAddress': 'same@example.invalid', 'accountUuid': 'person-one', 'organizationUuid': org}}))
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertIn('seat1\t', self.menu.read_text())
        self.assertIn('seat2\t', self.menu.read_text())

    def test_default_launch_profile_keeps_its_route(self):
        self.env.update(SB_DEFAULT_CLAUDE_PROFILE="claude", SB_PROFILE_WRAPPER_ROOT=str(self.bin))
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())
        self.assertIn(b"launch-profile=claude", output)

    def test_identity_metadata_cannot_inject_menu_rows_or_terminal_controls(self):
        self.identity("seat1", "first@example.invalid\nforged\t\x1b[31m")
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        rows = self.menu.read_text().splitlines()
        self.assertEqual(len(rows), 3)
        self.assertTrue(all(row.count("\t") == 1 and "\x1b" not in row for row in rows))

    def test_unreadable_metadata_is_not_an_authentication_failure(self):
        (self.profiles / "seat1/.claude.json").write_text("invalid-json")
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertNotIn("seat1\t", self.menu.read_text())


class InstalledEntrypointTests(unittest.TestCase):
    identity = AccountPickerTests.identity
    program = AccountPickerTests.program
    launch = AccountPickerTests.launch

    def setUp(self):
        AccountPickerTests.setUp(self)
        self.command = ["/bin/zsh", str(SB.parent / "entrypoints/claude")]
        pin = self.root / "vendor-pin"
        pin.write_text(str(self.bin / "claude"))
        ca = self.runtime / "state/mode-d/ca.pem"
        ca.parent.mkdir(parents=True)
        ca.touch()
        self.env.update(SB_NATIVE_CLAUDE_PIN_FILE=str(pin),
                        SB_CLAUDE_CONTROL_BIN=str(SB),
                        SB_NATIVE_CLAUDE=str(SB.parent / "entrypoints/claude"),
                        SB_MODE_D_CA_CERT=str(ca), SB_MODE_D_PROBE_BIN="/usr/bin/true")

    def test_executable_choose_account_passes_vendor_flags(self):
        rc, output = self.launch(["--choose-account", "--continue"])
        self.assertEqual(rc, 0, output)
        self.assertTrue(self.menu.exists())
        launched = json.loads(self.log.read_text())
        self.assertEqual(launched["config"], str(self.profiles / "seat2"))
        self.assertIn("--continue", launched["args"])
        self.assertNotIn("--choose-account", launched["args"])

    def test_plain_executable_offers_picker_without_shell_function(self):
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertTrue(self.menu.exists(), "installed executable bypassed account selection")
        self.assertEqual(json.loads(self.log.read_text())["config"], str(self.profiles / "seat2"))

    def test_executable_explicit_account_skips_menu_and_preserves_capture(self):
        rc, output = self.launch(["--account", "seat1"])
        self.assertEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())
        launched = json.loads(self.log.read_text())
        self.assertEqual(launched["config"], str(self.profiles / "seat1"))
        self.assertTrue(launched["proxy"])

    def test_executable_noninteractive_default_does_not_prompt(self):
        self.env["SB_DEFAULT_CLAUDE_ACCOUNT"] = "seat1"
        rc, output = self.launch(terminal=False)
        self.assertEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())
        self.assertEqual(json.loads(self.log.read_text())["config"], str(self.profiles / "seat1"))

    def test_executable_cancel_never_launches(self):
        self.env["PICKER_CHOICE"] = "cancel"
        rc, _ = self.launch()
        self.assertEqual(rc, 130)
        self.assertFalse(self.log.exists())

    def test_executable_preserves_explicit_api_context(self):
        self.env["ANTHROPIC_API_KEY"] = "synthetic-fixture"
        rc, output = self.launch()
        self.assertEqual(rc, 0, output)
        self.assertFalse(self.menu.exists())
        self.assertIsNone(json.loads(self.log.read_text())["config"])


if __name__ == "__main__":
    unittest.main()
