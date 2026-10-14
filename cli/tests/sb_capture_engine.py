import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tempfile
import unittest
from unittest.mock import patch


class CaptureEngineTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='capture-engine-fixture-')
        self.root = Path(self.tmp.name)
        source = Path(__file__).resolve().parents[1] / 'capture-engine.py'
        spec = importlib.util.spec_from_file_location('capture_engine_fixture', source)
        self.owner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.owner)
        self.runtime = self.root / 'runtime'
        self.generation = 'capture-fixture'
        self.gen = self.runtime / 'generations' / self.generation
        for path in [self.gen/'bin', self.gen/'config', self.gen/'state', self.runtime/'config', self.root/'agents']:
            path.mkdir(parents=True, exist_ok=True)
        self.engine = self.gen/'bin/switchback'
        self.engine.write_bytes(b'old engine fixture'); self.engine.chmod(0o755)
        self.new = self.root/'candidate-engine'
        self.new.write_bytes(b'new engine fixture'); self.new.chmod(0o755)
        self.body = self.gen/'state/preserved-body'
        self.body.write_bytes(b'capture history fixture')
        manifest = self.gen/'manifest.json'
        manifest.write_text(json.dumps(dict(schema='switchback/capture-generation@1',owner='switchback',generation=self.generation,source_commit='a'*40)))
        (self.gen/'config/capture.json').write_text('{}')
        registry = dict(schema='switchback/capture-backup-generations@1',owner='switchback',generations=[dict(id=self.generation,runtime_root=str(self.gen),cli=str(self.engine),binary_sha256=self.owner.digest(self.engine),manifest_sha256=self.owner.digest(manifest),state_dir=str(self.gen/'state'),archive_root=str(self.root/'archive'))])
        (self.runtime/'config/capture-backup-generations.json').write_text(json.dumps(registry))
        (self.runtime/'config/capture-repair.json').write_text(json.dumps(dict(engine=str(self.engine),engine_sha256=self.owner.digest(self.engine),body_store=dict(generation=self.generation))))
        label = 'ai.switchback.'+self.generation
        plist = dict(Label=label,ProgramArguments=[str(self.engine),'serve','--config',str(self.gen/'config/capture.json')])
        (self.root/'agents'/(''+label+'.plist')).write_bytes(plistlib.dumps(plist))
        self.plan = self.owner.plan(self.runtime,self.generation,self.new,'b'*40,self.root/'agents')

    def tearDown(self):
        self.assertEqual(self.body.read_bytes(), b'capture history fixture')
        self.tmp.cleanup()

    def test_owner_fingerprint_drift_refuses_planning(self):
        self.engine.write_bytes(b'foreign drift')
        with self.assertRaisesRegex(ValueError,'fingerprints changed'):
            self.owner.plan(self.runtime,self.generation,self.new,'b'*40,self.root/'agents')

    def test_active_clients_are_preserved_and_not_stopped(self):
        with patch.object(self.owner,'work_guard'), patch.object(self.owner,'wait_quiet',side_effect=ValueError('active connections')), patch.object(self.owner,'stop_service') as stop:
            with self.assertRaisesRegex(ValueError,'active connections'):
                self.owner.apply(self.plan,'work_fixture','actor_fixture','pol_fixture')
            stop.assert_not_called()
        self.assertEqual(self.engine.read_bytes(), b'old engine fixture')

    def test_candidate_drift_refuses_before_stopping(self):
        self.new.write_bytes(b'unreviewed candidate')
        with patch.object(self.owner,'work_guard'), patch.object(self.owner,'stop_service') as stop:
            with self.assertRaisesRegex(ValueError,'candidate engine changed'):
                self.owner.apply(self.plan,'work_fixture','actor_fixture','pol_fixture')
            stop.assert_not_called()

    def test_success_updates_coupled_authority_and_preserves_state(self):
        pid = [71]
        def stop(plan): pid[0] = None
        def start(plan): pid[0] = 72
        with patch.object(self.owner,'work_guard'), patch.object(self.owner,'wait_quiet',return_value=71), patch.object(self.owner,'service_pid',side_effect=lambda label:pid[0]), patch.object(self.owner,'connections',return_value=[]), patch.object(self.owner,'stop_service',side_effect=stop), patch.object(self.owner,'start_service',side_effect=start):
            receipt=self.owner.apply(self.plan,'work_fixture','actor_fixture','pol_fixture')
        self.assertTrue(receipt['upgraded'])
        self.assertEqual(receipt['current_pid'],72)
        self.assertEqual(self.engine.read_bytes(), self.new.read_bytes())
        registry=self.owner.read(self.runtime/'config/capture-backup-generations.json')
        self.assertEqual(registry['generations'][0]['binary_sha256'],self.owner.digest(self.new))
        self.assertEqual(registry['generations'][0]['manifest_sha256'],self.owner.digest(self.gen/'manifest.json'))
        self.assertEqual(self.owner.read(self.runtime/'config/capture-repair.json')['engine_sha256'],self.owner.digest(self.new))

    def test_failed_start_restores_exact_previous_owner_generation(self):
        before={path:Path(path).read_bytes() for path in self.plan['expected']}
        pid=[71];starts=[0]
        def stop(plan): pid[0]=None
        def start(plan):
            starts[0]+=1
            if starts[0]==1:raise ValueError('fixture failed start')
            pid[0]=73
        with patch.object(self.owner,'work_guard'), patch.object(self.owner,'wait_quiet',return_value=71), patch.object(self.owner,'service_pid',side_effect=lambda label:pid[0]), patch.object(self.owner,'connections',return_value=[]), patch.object(self.owner,'stop_service',side_effect=stop), patch.object(self.owner,'start_service',side_effect=start):
            with self.assertRaisesRegex(ValueError,'failed start'):
                self.owner.apply(self.plan,'work_fixture','actor_fixture','pol_fixture')
        for path,body in before.items():self.assertEqual(Path(path).read_bytes(),body)
        self.assertEqual(pid[0],73)


class CaptureOwnerInstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='capture-owner-install-fixture-')
        self.root = Path(self.tmp.name)
        self.runtime = self.root / 'runtime'
        (self.runtime / 'bin').mkdir(parents=True)
        (self.runtime / 'config').mkdir()
        (self.runtime / 'manifest.json').write_text('{}')
        self.installer = self.runtime / 'bin/native-claude-install.sh'
        self.original = self.root / 'enrolled-native-installer'
        self.original.write_bytes(b'original enrolled owner fixture')
        self.installer.write_bytes(self.original.read_bytes())
        self.engine = self.runtime / 'bin/switchback-bin'
        self.engine.write_bytes(b'preserved recorder fixture')
        self.policy = self.runtime / 'config/capture-repair.json'
        self.policy.write_text(json.dumps({'native_claude': {'installer': str(self.installer),
            'installer_sha256': hashlib.sha256(self.original.read_bytes()).hexdigest()}}))
        self.before_policy = self.policy.read_bytes()
        self.script = Path(__file__).resolve().parents[1] / 'install.sh'

    def tearDown(self):
        self.assertEqual(self.engine.read_bytes(), b'preserved recorder fixture')
        self.assertEqual(self.policy.read_bytes(), self.before_policy)
        self.tmp.cleanup()

    def install(self, original=None):
        env = dict(os.environ, SWITCHBACK_RUNTIME_ROOT=str(self.runtime),
                   SB_INSTALL_CAPTURE_OWNERS_ONLY='1', SB_INSTALL_NATIVE_CLAUDE_ONLY='0',
                   SB_CAPTURE_NATIVE_OWNER_RESTORE_FROM=str(original) if original else '')
        return subprocess.run(['zsh', str(self.script)], env=env, capture_output=True, text=True)

    def test_owner_install_preserves_enrolled_native_claude_helper(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installer.read_bytes(), self.original.read_bytes())
        self.assertTrue((self.runtime / 'bin/capture-engine.py').is_file())

    def test_exact_registered_native_owner_can_be_restored(self):
        self.installer.write_bytes(b'changed owner fixture')
        result = self.install(self.original)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installer.read_bytes(), self.original.read_bytes())
        self.assertEqual(self.installer.stat().st_mode & 0o777, 0o755)
        backups = list(self.installer.parent.glob('native-claude-install.sh.rollback-*'))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_bytes(), b'changed owner fixture')

    def test_native_owner_restore_refuses_unregistered_bytes_before_installing(self):
        self.installer.write_bytes(b'changed owner fixture')
        wrong = self.root / 'unregistered-owner'
        wrong.write_bytes(b'unregistered owner fixture')
        result = self.install(wrong)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('does not match enrolled owner', result.stderr)
        self.assertEqual(self.installer.read_bytes(), b'changed owner fixture')
        self.assertFalse((self.runtime / 'bin/capture-engine.py').exists())


if __name__=='__main__':
    unittest.main(verbosity=2)
