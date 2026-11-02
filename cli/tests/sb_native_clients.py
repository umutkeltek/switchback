#!/usr/bin/env python3
import json,os,pathlib,shutil,socket,subprocess,sys,tempfile
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
# A Homebrew upgrade deletes the old Cellar folder, and the route must survive that.
with tempfile.TemporaryDirectory(prefix="native-capture-brew-") as directory:
    root=pathlib.Path(directory).resolve();runtime=root/"runtime";runtime.mkdir(mode=0o700)
    routes=runtime/"config/native-client-routes.json";opt=str(root/"opt/opencode/bin/opencode")
    ca=root/"ca.pem";ca.write_text("fixture");bundle=root/"bundle.pem";bundle.write_text("fixture")
    listener=socket.socket();listener.bind(("127.0.0.1",0));listener.listen()
    (root/"bin").mkdir();(root/"opt").mkdir()
    def script(path,text):
        path.parent.mkdir(parents=True,exist_ok=True);path.write_text("#!/bin/sh\necho "+text+"\n");path.chmod(0o755);return path
    def brew(version):
        exe=script(root/"Cellar/opencode"/version/"bin/opencode",version);link=root/"opt/opencode"
        if link.is_symlink():link.unlink()
        link.symlink_to(root/"Cellar/opencode"/version);return exe
    def call(*args):
        return subprocess.run([sys.executable,str(installer),*args,"--runtime",str(runtime)],text=True,capture_output=True)
    def clients():
        return json.loads(routes.read_text())["clients"]
    def edit_route(executable):
        data=json.loads(routes.read_text());data["clients"]["opencode"]["executable"]=executable;routes.write_text(json.dumps(data))
    public=root/"bin/opencode";public.symlink_to(brew("1.0"))
    goose=root/"bin/goose";goose.symlink_to(script(root/"goose-vendor","goose"))
    def request(path):
        return {"entrypoint":str(path),"proxy_port":listener.getsockname()[1],"ca_cert":str(ca),"ca_bundle":str(bundle)}
    def plan(**entries):
        path=root/"plan.json";path.write_text(json.dumps({"clients":entries}));return str(path)
    def direct():
        env={**os.environ,"SB_NATIVE_ROUTES_FILE":str(routes),"SB_NATIVE_DIRECT":"1"}
        return subprocess.run([str(public)],env=env,text=True,capture_output=True).stdout.strip()
    result=call("install","--plan",plan(opencode=request(public),goose=request(goose)));assert result.returncode==0,(result.stdout,result.stderr)
    assert clients()["opencode"]["executable"]==opt,clients()["opencode"]["executable"]
    brew("2.0");shutil.rmtree(root/"Cellar/opencode/1.0")
    assert direct()=="2.0"
    assert call("status").returncode==0
    edit_route(str(root/"Cellar/opencode/1.0/bin/opencode"))
    assert call("status").returncode==1
    assert call("repair").returncode==0 and clients()["opencode"]["executable"]==opt
    edit_route(str(root/"Cellar/ghost/1.0/bin/ghost"));stale=routes.read_bytes()
    assert call("repair").returncode!=0 and routes.read_bytes()==stale
    edit_route(opt)
    wrapper,neighbor=public.read_bytes(),clients()["goose"]
    other=script(root/"other-copy","other")
    result=call("install","--plan",plan(opencode={**request(public),"executable":str(other)}));assert result.returncode==0,(result.stdout,result.stderr)
    assert clients()["opencode"]["executable"]==str(other) and direct()=="other"
    assert public.read_bytes()==wrapper and clients()["goose"]==neighbor
    named=routes.read_bytes()
    assert call("install","--plan",plan(opencode={**request(public),"executable":str(public)})).returncode!=0
    assert call("install","--plan",plan(opencode={**request(public),"executable":str(root/"missing")})).returncode!=0
    assert routes.read_bytes()==named
print("native client owner: vendor preservation, repeat install, update repair, upgrade survival, one-client retarget, and refusal controls pass")
