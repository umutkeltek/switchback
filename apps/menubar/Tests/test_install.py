import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shutil
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("menubar_install", Path(__file__).parents[1] / "install.py")
installer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(installer)


class InstallationTests(unittest.TestCase):
    def setUp(self):
        scratch = os.environ.get("SWITCHBACK_MENUBAR_TEST_SCRATCH")
        self.root = Path(tempfile.mkdtemp(prefix="menubar-install-test-", dir=scratch)).resolve()
        self.runtime = self.root / "runtime"
        (self.runtime / "bin").mkdir(parents=True)
        (self.runtime / "manifest.json").write_text(json.dumps({
            "schema": "switchback/runtime-manifest@1", "owner": "switchback"}))
        self.source = self.bundle(self.root / "source.app", b"new-binary")
        self.destination = self.runtime / "apps/SwitchbackBar.app"
        self.verify = patch.object(installer, "verify")
        self.verify.start()

    def tearDown(self):
        self.verify.stop()
        shutil.rmtree(self.root)

    def bundle(self, path, body):
        (path / "Contents/Resources").mkdir(parents=True)
        (path / "Contents/MacOS").mkdir()
        (path / "Contents/Resources/switchback-owner").write_text(installer.OWNER)
        (path / "Contents/Info.plist").write_bytes(plistlib.dumps({
            "CFBundleIdentifier": "dev.switchback.menubar"}))
        (path / "Contents/MacOS/SwitchbackBar").write_bytes(body)
        return path

    def test_retains_previous_app_and_binds_login_runtime(self):
        self.bundle(self.destination, b"old-binary")
        agent = self.root / "agents/dev.switchback.menubar.plist"
        destination, previous = installer.install_bundle(self.source, self.runtime, agent)
        self.assertEqual((destination / "Contents/MacOS/SwitchbackBar").read_bytes(), b"new-binary")
        self.assertEqual((previous / "Contents/MacOS/SwitchbackBar").read_bytes(), b"old-binary")
        config = plistlib.loads(agent.read_bytes())
        self.assertEqual(config["EnvironmentVariables"]["SWITCHBACK_RUNTIME_ROOT"], str(self.runtime))
        self.assertEqual(config["EnvironmentVariables"]["SB_RUNTIME_ROOT"], str(self.runtime))
        # A crash relaunches the app; Quit (exit 0) leaves it quit.
        self.assertEqual(config["KeepAlive"], {"SuccessfulExit": False})
        self.assertTrue(config["RunAtLoad"])
        self.assertGreaterEqual(config["ThrottleInterval"], 10)
        self.assertEqual(config["LimitLoadToSessionType"], "Aqua")

    def test_refuses_unowned_existing_app_without_touching_it(self):
        self.bundle(self.destination, b"foreign")
        (self.destination / "Contents/Resources/switchback-owner").write_text("foreign")
        with self.assertRaises(installer.Refusal):
            installer.install_bundle(self.source, self.runtime)
        self.assertEqual((self.destination / "Contents/MacOS/SwitchbackBar").read_bytes(), b"foreign")

    def test_refuses_symlink_parent_and_wrong_manifest(self):
        (self.runtime / "apps").symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(installer.Refusal):
            installer.install_bundle(self.source, self.runtime)
        (self.runtime / "apps").unlink()
        (self.runtime / "manifest.json").write_text('{"schema":"foreign","owner":"switchback"}')
        with self.assertRaises(installer.Refusal):
            installer.install_bundle(self.source, self.runtime)
        self.assertFalse(self.destination.exists())

    def test_publication_failure_restores_prior_bytes(self):
        self.bundle(self.destination, b"old-binary")
        replace = os.replace
        failed_once = False

        def refuse_new_publication(source, destination):
            nonlocal failed_once
            if Path(source).parent.name.startswith(".menubar-stage-") and not failed_once:
                failed_once = True
                raise OSError("synthetic publication refusal")
            return replace(source, destination)

        with patch.object(installer.os, "replace", side_effect=refuse_new_publication):
            with self.assertRaisesRegex(OSError, "synthetic publication refusal"):
                installer.install_bundle(self.source, self.runtime)
        self.assertEqual((self.destination / "Contents/MacOS/SwitchbackBar").read_bytes(), b"old-binary")

    def test_post_publication_signature_failure_rolls_back(self):
        self.bundle(self.destination, b"old-binary")

        def verify(path):
            if path == self.destination:
                raise RuntimeError("synthetic signature refusal")

        with patch.object(installer, "verify", side_effect=verify):
            with self.assertRaisesRegex(RuntimeError, "synthetic signature refusal"):
                installer.install_bundle(self.source, self.runtime)
        self.assertEqual((self.destination / "Contents/MacOS/SwitchbackBar").read_bytes(), b"old-binary")
        self.assertEqual(len(list(self.destination.parent.glob("*.failed.*"))), 1)


if __name__ == "__main__":
    unittest.main()
