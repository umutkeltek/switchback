#!/usr/bin/env python3
"""Account installer preserves the capture repair owner's pinned artifacts."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

CLI=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location("capture_repair_install",CLI/"capture-repair.py")
owner=importlib.util.module_from_spec(spec);spec.loader.exec_module(owner)

class AccountInstall(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory(prefix="sb-account-install-")
        self.home=Path(self.temp.name).resolve()
        self.runtime=self.home/".switchback"
        self.prefix=self.home/".local/bin"
        for path in (self.runtime/"bin/entrypoints",self.runtime/"config",self.prefix):
            path.mkdir(parents=True,exist_ok=True)
        (self.runtime/"manifest.json").write_text(json.dumps({"schema":"switchback/runtime-manifest@1","owner":"switchback"}))
        self.sb=self.runtime/"bin/sb";self.sb.write_text("#!/bin/sh\nexit 0\n");self.sb.chmod(0o755)
        (self.prefix/"sb").symlink_to(self.sb)
        self.template=self.runtime/"bin/entrypoints/codex-switchback-tap"
        self.public=self.prefix/"codex-switchback-tap"
        self.old=b"#!/bin/sh\n# switchback-owned: native-codex-capture-entrypoint@1\nexit 0\n"
        for path in (self.template,self.public):
            path.write_bytes(self.old);path.chmod(0o755)
        self.policy=self.runtime/"config/capture-repair.json"
        self.policy.write_text(json.dumps({"schema":owner.SCHEMA,"owner":"switchback","profiles":[],"clients":[],
            "entrypoint_artifacts":[{"source":str(self.template),"target":str(self.public),
                                    "sha256":hashlib.sha256(self.old).hexdigest()}]}))
        self.policy.chmod(0o600)
        self.env=dict(os.environ,HOME=str(self.home),PREFIX=str(self.prefix),SWITCHBACK_RUNTIME_ROOT=str(self.runtime),
                      SB_INSTALL_ACCOUNTS_ONLY="1",SB_INSTALL_ENGINE_ONLY="0",SB_INSTALL_NATIVE_CLAUDE_ONLY="0",
                      SB_INSTALL_CAPTURE_OWNERS_ONLY="0")
    def tearDown(self):
        (self.runtime/"config").chmod(0o700)
        self.temp.cleanup()
    def install(self):
        return subprocess.run([str(CLI/"install.sh")],env=self.env,text=True,capture_output=True,timeout=20)
    def test_upgrade_keeps_repair_owner_and_public_template_consistent(self):
        result=self.install()
        self.assertEqual(result.returncode,0,result.stderr)
        candidate=(CLI/"wrappers/codex-switchback-tap").read_bytes()
        self.assertEqual(self.template.read_bytes(),candidate)
        self.assertEqual(self.public.read_bytes(),candidate)
        policy=json.loads(self.policy.read_text())
        self.assertEqual(policy["entrypoint_artifacts"][0]["sha256"],hashlib.sha256(candidate).hexdigest())
        self.assertEqual((self.runtime/"bin/capture-repair.py").read_bytes(),(CLI/"capture-repair.py").read_bytes())
        with unittest.mock.patch.dict(os.environ,{"HOME":str(self.home)}):
            self.assertTrue(owner.entrypoint_status(self.runtime,policy)["codex-switchback-tap"])
        self.assertEqual((self.template.with_name(self.template.name+".bak-accounts-only")).read_bytes(),self.old)
        self.assertTrue((self.prefix/"sb").is_symlink())
    def test_failed_policy_publication_restores_installed_artifacts(self):
        before={p:p.read_bytes() for p in (self.sb,self.template,self.public,self.policy)}
        (self.runtime/"config").chmod(0o500)
        result=self.install()
        self.assertNotEqual(result.returncode,0)
        for path,body in before.items():
            self.assertEqual(path.read_bytes(),body,str(path))
        self.assertFalse((self.runtime/"bin/native_account_capture.py").exists())
    def test_foreign_public_template_refuses_before_helper_install(self):
        self.public.write_text("#!/bin/sh\n# not-owned\nexit 0\n")
        before={p:p.read_bytes() for p in (self.sb,self.template,self.public,self.policy)}
        result=self.install()
        self.assertNotEqual(result.returncode,0)
        for path,body in before.items():
            self.assertEqual(path.read_bytes(),body)
        self.assertFalse((self.runtime/"bin/native_account_capture.py").exists())
    def test_idempotent_upgrade_keeps_matching_pin(self):
        self.assertEqual(self.install().returncode,0)
        before=self.policy.read_bytes()
        result=self.install()
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(self.policy.read_bytes(),before)

if __name__=="__main__":
    from unittest import mock
    unittest.main()
