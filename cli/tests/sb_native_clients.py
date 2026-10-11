#!/usr/bin/env python3
import json,os,pathlib,socket,subprocess,sys,tempfile
cli=pathlib.Path(__file__).resolve().parent.parent;installer=cli/"native-clients.py"
with tempfile.TemporaryDirectory(prefix="native-capture-owner-") as directory:
    root=pathlib.Path(directory);runtime=root/"runtime";runtime.mkdir(mode=0o700)
    vendor=root/"vendor";vendor.write_text("#!/usr/bin/env python3\nimport os,json; print(json.dumps({k:os.environ.get(k) for k in ['HTTP_PROXY','NO_PROXY','OPENAI_API_KEY']}))\n");vendor.chmod(0o700)
    public=root/"hermes";public.symlink_to(vendor)
    ca=root/"ca.pem";ca.write_text("fixture");bundle=root/"bundle.pem";bundle.write_text("fixture")
    plan=root/"plan.json"
    plan.write_text(json.dumps({"clients":{"hermes":{"entrypoint":str(public),"proxy_port":18882,"ca_cert":str(ca),"ca_bundle":str(bundle)}}}))
    listener=socket.socket();listener.bind(("127.0.0.1",0));listener.listen()
    tap=socket.socket();tap.bind(("127.0.0.1",0));tap.listen()
    origin="http://127.0.0.1:"+str(tap.getsockname()[1])
    capture=root/"capture.json";capture.write_text(json.dumps({"server":{"taps":[{"bind":"127.0.0.1:"+str(tap.getsockname()[1]),"capture_bodies":True,"headers":{"x-switchback-harness":"hermes"}}]}}))
    settings=json.loads(plan.read_text());settings["clients"]["hermes"].update(proxy_port=listener.getsockname()[1],capture_tap_origins=[origin],capture_config=str(capture));plan.write_text(json.dumps(settings))
    before=vendor.read_bytes()
    def call(*args):
        return subprocess.run([sys.executable,str(installer),*args,"--runtime",str(runtime)],text=True,capture_output=True)
    result=call("install","--plan",str(plan));assert result.returncode==0,(result.stdout,result.stderr)
    assert vendor.read_bytes()==before
    assert not public.is_symlink() and b"switchback-owned: native-client-capture-entrypoint@1" in public.read_bytes()
    manifest=json.loads((runtime/"config/native-client-routes.json").read_text())
    assert manifest["clients"]["hermes"]["executable"]==str(vendor.resolve())
    env={**os.environ,"SB_NATIVE_ROUTES_FILE":str(runtime/"config/native-client-routes.json"),"SB_NATIVE_CAPTURE_READY_TIMEOUT_SECONDS":"0","NO_PROXY":"*","OPENAI_API_KEY":"fixture"}
    observed=subprocess.run([str(public)],env=env,text=True,capture_output=True)
    assert observed.returncode==0,(observed.stdout,observed.stderr)
    observed=json.loads(observed.stdout)
    assert observed["HTTP_PROXY"]=="http://127.0.0.1:"+str(listener.getsockname()[1])
    assert observed["NO_PROXY"]=="127.0.0.1:"+str(tap.getsockname()[1]),observed
    assert observed["OPENAI_API_KEY"]=="fixture"
    first=public.read_bytes()
    assert call("repair").returncode==0 and public.read_bytes()==first
    public.unlink();public.symlink_to(vendor)
    assert call("status").returncode==1
    assert call("repair").returncode==0 and public.read_bytes()==first
    invalid=json.loads(plan.read_text());invalid["clients"]["hermes"]["capture_tap_origins"]=["http://127.0.0.1:9"];plan.write_text(json.dumps(invalid))
    assert call("install","--plan",str(plan)).returncode!=0
    assert public.read_bytes()==first
    public.unlink();public.symlink_to(vendor);ca.unlink()
    assert call("repair").returncode!=0 and public.is_symlink()
print("native client owner: vendor preservation, repeat install, update repair, and refusal controls pass")
