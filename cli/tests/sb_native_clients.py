#!/usr/bin/env python3
import json,os,pathlib,subprocess,sys,tempfile
cli=pathlib.Path(__file__).resolve().parent.parent;installer=cli/"native-clients.py"
with tempfile.TemporaryDirectory(prefix="native-capture-owner-") as directory:
    root=pathlib.Path(directory);runtime=root/"runtime";runtime.mkdir(mode=0o700)
    vendor=root/"vendor";vendor.write_text("#!/bin/sh\nexit 0\n");vendor.chmod(0o700)
    public=root/"hermes";public.symlink_to(vendor)
    ca=root/"ca.pem";ca.write_text("fixture");bundle=root/"bundle.pem";bundle.write_text("fixture")
    plan=root/"plan.json"
    plan.write_text(json.dumps({"clients":{"hermes":{"entrypoint":str(public),"proxy_port":18882,"ca_cert":str(ca),"ca_bundle":str(bundle)}}}))
    before=vendor.read_bytes()
    def call(*args):
        return subprocess.run([sys.executable,str(installer),*args,"--runtime",str(runtime)],text=True,capture_output=True)
    result=call("install","--plan",str(plan));assert result.returncode==0,(result.stdout,result.stderr)
    assert vendor.read_bytes()==before
    assert not public.is_symlink() and b"switchback-owned: native-client-capture-entrypoint@1" in public.read_bytes()
    manifest=json.loads((runtime/"config/native-client-routes.json").read_text())
    assert manifest["clients"]["hermes"]["executable"]==str(vendor.resolve())
    first=public.read_bytes()
    assert call("repair").returncode==0 and public.read_bytes()==first
    public.unlink();public.symlink_to(vendor)
    assert call("status").returncode==1
    assert call("repair").returncode==0 and public.read_bytes()==first
    public.unlink();public.symlink_to(vendor);ca.unlink()
    assert call("repair").returncode!=0 and public.is_symlink()
print("native client owner: vendor preservation, repeat install, update repair, and refusal controls pass")
