#!/usr/bin/env python3
"""Exercise repair after vendor updates and refusal before unowned changes."""
import hashlib, importlib.util, json, os, pathlib, shutil, socket, subprocess, sys, tempfile, unittest

CLI = pathlib.Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("capture_repair", CLI / "capture-repair.py")
owner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(owner)

def write(path, value, executable=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value)
    path.chmod(0o700 if executable else 0o600)

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

class RepairTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="capture-repair-owner-")
        self.home = pathlib.Path(self.temp.name).resolve()
        self.previous_home = os.environ.get("HOME")
        os.environ["HOME"] = str(self.home)
        self.runtime = self.home / ".switchback"
        self.bin = self.runtime / "bin"
        self.bin.mkdir(parents=True)
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(128)
        self.port = self.listener.getsockname()[1]
        self.native = self.bin / "native-clients.py"
        shutil.copy2(CLI / "native-clients.py", self.native)
        shutil.copytree(CLI / "entrypoints", self.bin / "entrypoints")
        self.vendor = self.home / "vendor"
        write(self.vendor, "#!/usr/bin/env python3\nprint('fixture')\n", True)
        self.public = self.home / ".local/bin/hermes"
        self.public.parent.mkdir(parents=True)
        self.public.symlink_to(self.vendor)
        self.ca, self.bundle = self.runtime / "ca.pem", self.runtime / "bundle.pem"
        write(self.ca, "fixture")
        write(self.bundle, "fixture")
        self.capture = self.runtime / "config/capture.json"
        write(self.capture, json.dumps({"server": {"taps": []}}))
        self.primary = self.runtime / "config/primary.json"
        write(self.primary, '{"provider":"fixture","model":"fixture/model","account":"fixture-account"}')
        self.authority = self.runtime / "config/launch-profiles.json"
        write(self.authority, '{"launch_profiles":{"fixture-profile":{"model":"fixture/model"}}}')
        self.profile_config = self.runtime / "config/profile-audit.json"
        write(self.profile_config, '{"provider":"fixture"}')
        self.profile_state = self.runtime / "profile-current"
        write(self.profile_state, "false")
        self.engine = self.bin / "switchback"
        engine = """#!PYTHON
import json,sys
from pathlib import Path
p=Path(STATE)
current=p.read_text()=="true"
if sys.argv[3]=="apply":
 p.write_text("true")
 print(json.dumps({"changed":not current,"doctor":{"ok":True}}))
else:print(json.dumps({"changed":not current}))
"""
        write(self.engine, engine.replace("PYTHON", sys.executable).replace("STATE", repr(str(self.profile_state))), True)
        self.calls = self.runtime / "compound-calls.jsonl"
        self.compound = self.bin / "compound"
        bridge = """#!PYTHON
import json,sys,subprocess
from pathlib import Path
a=sys.argv[1:]
with Path(CALLS).open("a") as f:f.write(json.dumps(a)+"\\n")
if a[:2]==["work","advance"]:
 subprocess.run(a[a.index("--")+1:],check=True)
"""
        write(self.compound, bridge.replace("PYTHON", sys.executable).replace("CALLS", repr(str(self.calls))), True)
        install = {"clients": {"hermes": {"entrypoint": str(self.public), "proxy_port": self.port,
                   "ca_cert": str(self.ca), "ca_bundle": str(self.bundle)}}}
        install_path = self.runtime / "native-plan.json"
        write(install_path, json.dumps(install))
        subprocess.run([sys.executable, str(self.native), "install", "--runtime", str(self.runtime),
                        "--plan", str(install_path)], check=True, capture_output=True)
        self.data = {"schema": owner.SCHEMA, "owner": "switchback",
                     "profile_authority": str(self.authority), "required_ports": [self.port],
                     "compound_cli": str(self.compound)}
        for field, path in [("engine", self.engine), ("native_owner", self.native),
                            ("primary_config", self.primary), ("capture_config", self.capture),
                            ("profile_config", self.profile_config)]:
            self.data[field] = str(path)
            self.data[field+"_sha256"] = sha(path)
        self.plan_path = self.runtime / "config/capture-repair.json"
        write(self.plan_path, json.dumps(self.data))

    def tearDown(self):
        self.listener.close()
        if self.previous_home is None:os.environ.pop("HOME", None)
        else:os.environ["HOME"] = self.previous_home
        self.temp.cleanup()

    def test_vendor_update_uses_governed_execution_and_preserves_models(self):
        authority_before, primary_before = self.authority.read_bytes(), self.primary.read_bytes()
        updated = self.home / "new-vendor"
        write(updated, "#!/usr/bin/env python3\nprint('updated fixture')\n", True)
        self.public.unlink()
        self.public.symlink_to(updated)
        result = owner.scheduled(self.runtime)
        self.assertTrue(result["ok"])
        self.assertTrue(result["changed"])
        commands = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.assertEqual([command[:2] for command in commands],
                         [["work","create"],["work","claim"],["work","advance"],["work","close"]])
        execute = commands[2]
        self.assertIn("--execute", execute)
        self.assertEqual(execute[execute.index("--harness")+1], "os-harness-entrypoints")
        self.assertEqual(execute[execute.index("--workspace")+1], str(self.runtime))
        manifest = json.loads((self.runtime/"config/native-client-routes.json").read_text())
        self.assertEqual(manifest["clients"]["hermes"]["executable"], str(updated))
        self.assertEqual(self.authority.read_bytes(), authority_before)
        self.assertEqual(self.primary.read_bytes(), primary_before)
        count = len(commands)
        skipped = owner.scheduled(self.runtime)
        self.assertFalse(skipped["changed"])
        self.assertFalse(skipped["governed_work_required"])
        self.assertEqual(len(self.calls.read_text().splitlines()), count)

    def test_changed_capture_input_refuses_before_repair(self):
        before = self.public.read_bytes()
        write(self.capture, '{"server":{"taps":[{"unreviewed":true}]}}')
        with self.assertRaisesRegex(ValueError, "repair input changed: capture_config"):
            owner.scheduled(self.runtime)
        self.assertEqual(self.public.read_bytes(), before)
        self.assertFalse(self.calls.exists())

    def test_missing_listener_defers_without_restarting_sessions(self):
        self.listener.close()
        before = self.public.read_bytes()
        with self.assertRaisesRegex(ValueError, "listeners unavailable"):
            owner.scheduled(self.runtime)
        self.assertEqual(self.public.read_bytes(), before)
        self.assertFalse(self.calls.exists())

    def test_codex_route_preserves_model_and_websocket_support(self):
        public = self.home / ".local/bin/codex-switchback-tap"
        shutil.copy2(CLI / "wrappers/codex-switchback-tap", public)
        native = self.home / ".local/bin/codex"
        write(native, "#!"+sys.executable+"\nimport sys,json;print(json.dumps(sys.argv[1:]))\n", True)
        write(self.runtime/"config/sb.env", "SB_CODEX_MODEL='fixture/model'\nSB_CODEX_EFFORT='high'\n")
        write(self.capture, json.dumps({"server":{"taps":[{"bind":"127.0.0.1:"+str(self.port),
                   "capture_bodies":True,"headers":{"x-switchback-harness":"codex"}}]}}))
        route = {"schema":"switchback/native-codex-capture@1","tap_port":self.port,
                 "service_label":"fixture.capture","capture_config":str(self.capture),
                 "capture_config_sha256":sha(self.capture)}
        route_path = self.runtime/"config/native-codex-capture.json"
        write(route_path, json.dumps(route))
        env = dict(os.environ, SWITCHBACK_RUNTIME_ROOT=str(self.runtime),
                   PATH=str(native.parent)+os.pathsep+os.environ["PATH"])
        result = subprocess.run([str(public),"--version"],env=env,capture_output=True,text=True,timeout=20)
        self.assertEqual(result.returncode,0,result.stderr)
        args = json.loads(result.stdout)
        provider = next(arg for arg in args if arg.startswith("model_providers.switchback-main="))
        self.assertIn("http://127.0.0.1:"+str(self.port)+"/v1",provider)
        self.assertIn("supports_websockets=true",provider)
        self.assertIn('model="fixture/model"',args)
        route["capture_config_sha256"]="0"*64
        write(route_path,json.dumps(route))
        refused = subprocess.run([str(public),"--version"],env=env,capture_output=True,text=True,timeout=20)
        self.assertNotEqual(refused.returncode,0)
        self.assertNotIn("--version",refused.stdout)

if __name__=="__main__":
    unittest.main()
