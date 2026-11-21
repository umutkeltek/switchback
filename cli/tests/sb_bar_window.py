#!/usr/bin/env python3
"""`sb bar window` against stand-in apps in a throwaway runtime: it never reaches an installed app."""

import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SB = Path(__file__).resolve().parents[1] / "sb"
OWNER = "switchback-menubar-owned-v1"


class SbBarWindow(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-bar-window-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        (self.root / "home").mkdir()
        runtime = self.root / "runtime"
        (runtime / "config").mkdir(parents=True)
        (runtime / "manifest.json").write_text("{}")
        self.app = runtime / "apps/SwitchbackBar.app"
        (self.app / "Contents/MacOS").mkdir(parents=True)
        (self.app / "Contents/Resources").mkdir(parents=True)
        (self.app / "Contents/Resources/switchback-owner").write_text(OWNER)
        self.calls = self.root / "calls.txt"
        # The override, set explicitly: the app `sb` looks for lives in this throwaway runtime.
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.root / "home"),
            "XDG_CONFIG_HOME": str(self.root / "xdg"),
            "SWITCHBACK_RUNTIME_ROOT": str(runtime),
        }

    def stand_in(self, body):
        """The app's executable: a script that records its arguments, then runs `body`."""
        executable = self.app / "Contents/MacOS/SwitchbackBar"
        executable.write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{self.calls}"\n{body}\n')
        executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
        return executable

    def bar(self, *args):
        return subprocess.run(["zsh", str(SB), "bar", *args], env=self.env, cwd=self.root,
                              capture_output=True, text=True, timeout=60)

    def calls_made(self):
        return self.calls.read_text().splitlines() if self.calls.exists() else []

    def test_an_older_app_gets_one_line_and_no_usage(self):
        # An app built before the window answers --open-window with its usage and code 2.
        self.stand_in('echo "Usage: SwitchbackBar [--version | --open-panel | --print-menu snapshot.json [--text]]" >&2\n'
                      "exit 2")
        run = self.bar("window")
        self.assertEqual(run.returncode, 1)
        self.assertEqual(run.stderr, "This Switchback app is older: run `sb menubar install`\n")
        self.assertEqual(run.stdout, "")
        # Asked once; never started, never asked to open the panel instead.
        self.assertEqual(self.calls_made(), ["--open-window"])

    def test_any_other_failure_shows_its_first_line_only(self):
        self.stand_in('printf "Could not open the window\\nmore detail\\n" >&2\nexit 3')
        run = self.bar("window")
        self.assertEqual(run.returncode, 3)
        self.assertEqual(run.stderr, "Could not open the window\n")
        self.assertEqual(self.calls_made(), ["--open-window"])

    def test_no_app_in_the_runtime_says_so(self):
        (self.app / "Contents/Resources/switchback-owner").unlink()
        run = self.bar("window")
        self.assertEqual(run.returncode, 1)
        self.assertIn("not installed", run.stderr)
        self.assertEqual(self.calls_made(), [])


if __name__ == "__main__":
    unittest.main()
