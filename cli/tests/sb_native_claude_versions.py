#!/usr/bin/env python3
"""Native version updates must preserve capture ownership and deliberate pins."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

CLI = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("capture_repair_versions", CLI / "capture-repair.py")
repair = importlib.util.module_from_spec(spec)
spec.loader.exec_module(repair)


class NativeVersions(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sb-native-versions-")
        self.home = Path(self.temp.name).resolve()
        self.runtime = self.home / ".switchback"
        self.prefix = self.home / ".local/bin"
        self.versions = self.home / ".local/share/claude/versions"
        for p in (self.runtime / "bin/entrypoints", self.runtime / "config", self.prefix, self.versions):
            p.mkdir(parents=True, exist_ok=True)
        (self.runtime / "manifest.json").write_text('{"schema":"switchback/runtime-manifest@1","owner":"switchback"}')
        self.public = self.prefix / "claude"
        shutil.copy2(CLI / "entrypoints/claude", self.public)
        shutil.copy2(CLI / "entrypoints/claude", self.runtime / "bin/entrypoints/claude")
        shutil.copy2(CLI / "install.sh", self.runtime / "bin/native-claude-install.sh")
        self.old = self.vendor("1.2.9")
        self.new = self.vendor("1.2.10")
        self.pin = self.versions.parent / ".switchback-real"
        self.pin.write_text(str(self.old) + "\n")
        provenance = self.runtime / "bin/native-claude-entrypoint-provenance.json"
        provenance.write_text(json.dumps({"schema": "switchback/native-claude-entrypoint-provenance@1",
            "sha256": repair.digest(self.public), "vendor_binary": str(self.old)}))
        provenance.chmod(0o600)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("SB_", "SWITCHBACK_", "CLAUDE_"))}
        self.env.update(HOME=str(self.home), PREFIX=str(self.prefix), SWITCHBACK_RUNTIME_ROOT=str(self.runtime), SB_INSTALL_NATIVE_CLAUDE_ONLY="1")

    def tearDown(self):
        self.temp.cleanup()

    def vendor(self, version):
        p = self.versions / version
        p.write_text("#!/bin/sh\nprintf '%s\\n' '" + version + "'\n")
        p.chmod(0o755)
        return p

    def install(self, **overrides):
        return subprocess.run([str(CLI / "install.sh")], env=dict(self.env, **overrides), capture_output=True, text=True, timeout=15)

    def resolve(self):
        result = subprocess.run([str(self.public)], env=dict(self.env, SB_NATIVE_CLAUDE_RESOLVE_ONLY="1"), capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def policy(self):
        return {"native_claude": {"installer": str(self.runtime / "bin/native-claude-install.sh"),
                "installer_sha256": repair.digest(self.runtime / "bin/native-claude-install.sh"),
                "template_sha256": repair.digest(self.runtime / "bin/entrypoints/claude")}}

    def test_install_updates_surviving_old_pin_and_records_selection(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.pin.read_text().strip(), str(self.new))
        provenance = json.loads((self.runtime / "bin/native-claude-entrypoint-provenance.json").read_text())
        self.assertEqual(provenance["vendor_selection"], "latest")
        self.assertTrue(self.old.exists())

    def test_launch_and_scheduled_repair_follow_new_install(self):
        self.assertEqual(self.install().returncode, 0)
        newer = self.vendor("1.3.0")
        self.assertEqual(self.resolve()["path"], str(newer))
        with patch.dict(os.environ, self.env, clear=True):
            self.assertFalse(repair.entrypoint_status(self.runtime, self.policy())["claude"])
            self.assertTrue(repair.repair_entrypoints(self.runtime, self.policy()))
            self.assertTrue(repair.entrypoint_status(self.runtime, self.policy())["claude"])
        self.assertEqual(self.pin.read_text().strip(), str(newer))

    def test_explicit_pin_is_preserved_and_missing_pin_refuses(self):
        result = self.install(SB_NATIVE_CLAUDE_BIN=str(self.old))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.resolve(), {"path": str(self.old), "selection": "explicit"})
        self.assertEqual(self.install().returncode, 0)
        self.assertEqual(self.pin.read_text().strip(), str(self.old))
        self.old.unlink()
        result = subprocess.run([str(self.public)], env=dict(self.env, SB_NATIVE_CLAUDE_RESOLVE_ONLY="1"), capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("explicit", result.stderr)

    def test_non_versions_and_unsafe_files_are_not_updates(self):
        self.vendor("999.0.0.partial")
        unsafe = self.vendor("900.0.0")
        unsafe.chmod(0o777)
        self.assertEqual(self.resolve()["path"], str(self.new))

    def test_vendor_self_update_symlink_is_repaired_by_owner(self):
        self.assertEqual(self.install().returncode, 0)
        newer = self.vendor("1.3.0")
        self.public.unlink()
        self.public.symlink_to(newer)
        with patch.dict(os.environ, self.env, clear=True):
            self.assertFalse(repair.entrypoint_status(self.runtime, self.policy())["claude"])
            repair.repair_entrypoints(self.runtime, self.policy())
            self.assertTrue(repair.entrypoint_status(self.runtime, self.policy())["claude"])
        self.assertFalse(self.public.is_symlink())
        self.assertEqual(self.pin.read_text().strip(), str(newer))

    def test_unowned_symlink_refuses_without_changing_pin(self):
        self.assertEqual(self.install().returncode, 0)
        foreign = self.home / "foreign"
        foreign.write_text("#!/bin/sh\nexit 0\n")
        foreign.chmod(0o755)
        self.public.unlink()
        self.public.symlink_to(foreign)
        before = self.pin.read_bytes()
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.pin.read_bytes(), before)
        self.assertEqual(self.public.readlink(), foreign)


if __name__ == "__main__":
    unittest.main()
