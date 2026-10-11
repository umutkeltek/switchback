#!/usr/bin/env python3
"""Own native capture entrypoints without changing provider or account settings."""
import argparse, fcntl, hashlib, json, os, stat, tempfile
from pathlib import Path
SCHEMA="switchback/native-client-routes@1"
MARKER=b"# switchback-owned: native-client-capture-entrypoint@1"
CLIENTS={"opencode","hermes","goose","prime","omp","qwen","dsh","pi"}
HERE=Path(__file__).resolve().parent
def digest(path):return hashlib.sha256(path.read_bytes()).hexdigest()
def atomic(path,data,mode=0o600):
    path.parent.mkdir(parents=True,exist_ok=True)
    fd,tmp=tempfile.mkstemp(prefix="."+path.name+".",dir=path.parent)
    try:
        with os.fdopen(fd,"wb") as stream:
            stream.write(data);stream.flush();os.fsync(stream.fileno())
        os.chmod(tmp,mode);os.replace(tmp,path)
    finally:
        if os.path.exists(tmp):os.unlink(tmp)
def json_write(path,value):atomic(path,(json.dumps(value,indent=2,sort_keys=True)+"\n").encode())
def owner_dir(path):
    path.mkdir(parents=True,exist_ok=True,mode=0o700)
    info=path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid!=os.getuid() or info.st_mode&0o022:
        raise ValueError("runtime directory must be owned and private: "+str(path))
def safe_path(value):
    if not isinstance(value,str) or not value.startswith("/") or any(c in value for c in "\n\r\t"):
        raise ValueError("paths must be absolute and single-line")
    return Path(value)
def install(runtime,plan):
    wrapper=(HERE/"entrypoints/native-client").read_bytes()
    if MARKER not in wrapper:raise ValueError("owned wrapper marker missing")
    config=runtime/"config";bins=runtime/"bin"
    for path in (runtime,config,bins,bins/"native-client-vendor",runtime/"backups/native-client-entrypoints"):owner_dir(path)
    routes_path=config/"native-client-routes.json"
    if routes_path.is_symlink():raise ValueError("client routes must not be symlinked")
    previous=json.loads(routes_path.read_text()) if routes_path.exists() else {"schema":SCHEMA,"clients":{}}
    if previous.get("schema")!=SCHEMA:raise ValueError("unknown native client routes schema")
    result={"schema":SCHEMA,"clients":dict(previous["clients"])}
    prepared=[]
    for name,request in plan["clients"].items():
        if name not in CLIENTS:raise ValueError("unsupported native client: "+name)
        entrypoint=safe_path(request["entrypoint"])
        if not entrypoint.exists() or not os.access(entrypoint,os.X_OK):raise ValueError("native entrypoint unavailable: "+str(entrypoint))
        original=entrypoint.read_bytes();prior=previous["clients"].get(name)
        if MARKER in original:
            if not prior or prior["entrypoint"]!=str(entrypoint):raise ValueError("wrapper lacks matching vendor provenance")
            vendor=safe_path(prior["executable"])
            if not vendor.exists() or not os.access(vendor,os.X_OK):raise ValueError("recorded vendor unavailable")
        elif entrypoint.is_symlink():vendor=entrypoint.resolve(strict=True)
        else:
            if original.startswith(b"#!"):raise ValueError("regular vendor scripts need a stable vendor target")
            vendor=bins/"native-client-vendor"/(name+"."+hashlib.sha256(original).hexdigest())
        port=request["proxy_port"]
        if not isinstance(port,int) or not 1024<=port<=65535:raise ValueError("invalid proxy port")
        ca=safe_path(request["ca_cert"]);bundle=safe_path(request["ca_bundle"])
        if not ca.is_file() or not bundle.is_file():raise ValueError("capture trust files must exist before enrollment")
        label=request.get("service_label","ai.switchback.native-capture")
        if not label or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789.-" for c in label):raise ValueError("invalid service label")
        record=dict(prior or {},**request)
        record.update(entrypoint=str(entrypoint),executable=str(vendor),service_label=label,
            proxy_url="http://127.0.0.1:"+str(port),wrapper_sha256=hashlib.sha256(wrapper).hexdigest())
        prepared.append((name,entrypoint,original,entrypoint.is_symlink(),vendor,record))
    lock_path=config/".native-client-install.lock"
    if lock_path.is_symlink():raise ValueError("installer lock must not be symlinked")
    with open(lock_path,"a") as lock:
        fcntl.flock(lock,fcntl.LOCK_EX)
        current=json.loads(routes_path.read_text()) if routes_path.exists() else {"schema":SCHEMA,"clients":{}}
        if current!=previous:raise ValueError("manifest changed during preparation")
        for name,entrypoint,original,was_link,vendor,record in prepared:
            if entrypoint.read_bytes()!=original or entrypoint.is_symlink()!=was_link:raise ValueError("entrypoint changed during preparation")
            if MARKER not in original:
                backup=runtime/"backups/native-client-entrypoints"/(name+"."+hashlib.sha256(original).hexdigest())
                if not backup.exists():atomic(backup,original,0o700)
                record["preimage_sha256"]=hashlib.sha256(original).hexdigest()
                record["preimage_symlink_target"]=str(entrypoint.resolve()) if was_link else None
                if not was_link:
                    if vendor.exists() and vendor.read_bytes()!=original:raise ValueError("vendor preimage mismatch")
                    if not vendor.exists():atomic(vendor,original,0o700)
            record["vendor_sha256"]=digest(vendor);result["clients"][name]=record
        # Vendor pins precede wrapper cutover, so a crash cannot orphan a wrapper.
        json_write(routes_path,result)
        for name,entrypoint,original,was_link,vendor,record in prepared:
            if entrypoint.read_bytes()!=original or entrypoint.is_symlink()!=was_link:raise ValueError("entrypoint changed before cutover")
            if original!=wrapper or was_link:atomic(entrypoint,wrapper,0o755)
    return status(runtime)
def status(runtime):
    path=runtime/"config/native-client-routes.json"
    data=json.loads(path.read_text()) if path.exists() else {"schema":SCHEMA,"clients":{}}
    if data.get("schema")!=SCHEMA:raise ValueError("unknown routes schema")
    clients={}
    for name,record in data["clients"].items():
        entry=safe_path(record["entrypoint"]);native=safe_path(record["executable"])
        clients[name]={"entrypoint":str(entry),"executable":str(native),"proxy_url":record["proxy_url"],
            "wrapper_current":entry.exists() and not entry.is_symlink() and digest(entry)==record["wrapper_sha256"],
            "vendor_available":native.exists() and os.access(native,os.X_OK),
            "ca_available":Path(record["ca_cert"]).is_file() and Path(record["ca_bundle"]).is_file()}
    return {"schema":"switchback/native-client-status@1","clients":clients,
        "ok":bool(clients) and all(all(r[k] for k in ("wrapper_current","vendor_available","ca_available")) for r in clients.values())}
def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action",choices=["install","repair","status"])
    parser.add_argument("--runtime",type=Path,required=True);parser.add_argument("--plan",type=Path)
    args=parser.parse_args()
    if args.action=="status":result=status(args.runtime)
    elif args.action=="install":
        if not args.plan:parser.error("install requires --plan")
        result=install(args.runtime,json.loads(args.plan.read_text()))
    else:result=install(args.runtime,json.loads((args.runtime/"config/native-client-routes.json").read_text()))
    print(json.dumps(result,indent=2,sort_keys=True));return 0 if result["ok"] else 1
if __name__=="__main__":raise SystemExit(main())
