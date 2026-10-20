#!/usr/bin/env python3
"""Bounded offline native adapter checks. No install, Portal call, service or deployment."""
import argparse, fcntl, hashlib, json, os, pathlib, shutil, signal, subprocess, time, re, tomllib
p=argparse.ArgumentParser()
p.add_argument("--rp-root",required=True)
p.add_argument("--report",required=True)
p.add_argument("--opus-archive",required=True)
a=p.parse_args()
rh=pathlib.Path(__file__).resolve().parents[1]; rp=pathlib.Path(a.rp_root).resolve()
target=rp/"target"; report=pathlib.Path(a.report).resolve()
if not target.is_dir(): raise SystemExit("existing RP target required")
if report.exists(): raise SystemExit("new report path required; inspect original before rerunning")
if shutil.disk_usage(target).free<5*1024**3: raise SystemExit("5 GiB preflight reserve required")
lock=open(target/".octendra-desktop-worker-check.lock","a")
try: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
except BlockingIOError: raise SystemExit("another owned native check is active")
source_files=list((rh/"desktop-worker/src").glob("*.rs"))+list((rh/"crates/octendra-desktop-protocol").rglob("*.rs"))
source_files += [rh/"crates/octendra-code/src/computer_use/transport.rs",rh/"crates/octendra-code/src/computer_use/transport_tests.rs"]
source_files += [rh/"desktop-worker/Cargo.toml",rh/"desktop-worker/Cargo.lock",rh/"crates/octendra-desktop-protocol/Cargo.toml"]
source_files += [rp/"Cargo.toml",rp/"Cargo.lock"]
for name in ["host","protocol","remote_core","remote_platform","third_party/screencapturekit"]:
    for directory, children, files in os.walk(rp/name):
        children[:] = [n for n in children if n not in ("target",".git","node_modules","__pycache__")]
        for file in files:
            if file == "Cargo.toml" or pathlib.Path(file).suffix in (".rs",".c",".h",".mm",".m",".wgsl",".glsl",".metal"):
                source_files.append(pathlib.Path(directory)/file)
source_files = sorted(set(source_files))
version = tomllib.loads((rh/"desktop-worker/Cargo.toml").read_text())["package"]["version"]
def hashes(): return {str(f):hashlib.sha256(f.read_bytes()).hexdigest() for f in source_files}
archive=pathlib.Path(a.opus_archive).resolve(strict=True)
if not archive.is_file() or archive.name!="libopus.a" or target not in archive.parents:
    raise SystemExit("explicit existing target Opus archive required")
opus_libraries=[archive]
native_library_path=os.pathsep.join(str(f.parent) for f in opus_libraries)
receipt={"started_at":int(time.time()),"target":str(target),"steps":[],"source_before":{},
    "opus_archives":{str(f):hashlib.sha256(f.read_bytes()).hexdigest() for f in opus_libraries}}
def save():
    tmp=report.with_suffix(report.suffix+".partial")
    tmp.write_text(json.dumps(receipt,indent=2));os.replace(tmp,report)
def run(name,cmd,cwd,seconds=180):
    log=report.with_name(report.name+"."+name+".log")
    started=time.monotonic()
    with open(log,"xb") as out:
        child=subprocess.Popen(cmd,cwd=cwd,stdout=out,stderr=subprocess.STDOUT,start_new_session=True,
            env={**os.environ,"CARGO_TARGET_DIR":str(target),"CARGO_BUILD_JOBS":"2","CMAKE_POLICY_VERSION_MINIMUM":"3.5","LIBRARY_PATH":native_library_path})
        boundary=None
        while child.poll() is None:
            if time.monotonic()-started>seconds: boundary="timeout"
            if shutil.disk_usage(target).free<4*1024**3: boundary="disk_reserve"
            if out.tell()>32*1024**2: boundary="output_bound"
            if boundary:
                os.killpg(child.pid,signal.SIGTERM)
                try: child.wait(timeout=5)
                except subprocess.TimeoutExpired: os.killpg(child.pid,signal.SIGKILL);child.wait()
                break
            time.sleep(.25)
    step={"name":name,"exit_code":child.returncode,"boundary":boundary,"elapsed_seconds":round(time.monotonic()-started,2),"log":str(log)}
    receipt["steps"].append(step);save()
    print(json.dumps(step),flush=True)
    tail=log.read_text(errors="replace")[-6500:]
    print(tail,flush=True)
    if child.returncode!=0 or boundary: raise RuntimeError(name+" failed")
    return log
try:
    fmt=[shutil.which("rustfmt")]
    if not fmt[0]: raise RuntimeError("installed rustfmt required")
    receipt["source_before"]=hashes();save()
    # Never format the RP worktree.
    fmt_files=list((rh/"desktop-worker/src").glob("*.rs"))+list((rh/"crates/octendra-desktop-protocol").rglob("*.rs"))
    run("format",["rustup","run","1.99.1","rustfmt","--edition","2024","--config","skip_children=true","--check"]+[str(f) for f in fmt_files],rh,30)
    cargo=["cargo","+1.99.1"]
    manifest=rh/"desktop-worker/Cargo.toml"
    patch=["--config",'patch.crates-io.host.path="'+str(rp/"host")+'"',
        "--config",'patch.crates-io.protocol.path="'+str(rp/"protocol")+'"',
        "--config",'patch.crates-io.screencapturekit.path="'+str(rp/"third_party/screencapturekit")+'"']
    # Preserve locked versions; these path patches are the established RP sources.
    receipt["worker_lock_sha256"]=hashlib.sha256((rh/"desktop-worker/Cargo.lock").read_bytes()).hexdigest();save()
    tree=run("dependency_tree",cargo+["tree","--manifest-path",str(manifest),"--locked","--offline","--target","x86_64-unknown-linux-gnu","-e","normal"]+patch,rh,45)
    if "gpui" in tree.read_text().lower(): raise RuntimeError("worker has GPUI dependency")
    run("worker_clippy",cargo+["clippy","--manifest-path",str(manifest),"--all-targets","--locked","--offline"]+patch+["--","-D","warnings"],rh,600)
    test_log=run("worker_tests",cargo+["test","--manifest-path",str(manifest),"--locked","--offline"]+patch+["--","--nocapture"],rh,600)
    receipt["worker_tests"]=sum(int(n) for n in re.findall(r"test result: ok\. (\d+) passed;",test_log.read_text()));save()
    if receipt["worker_tests"] < 2: raise RuntimeError("worker tests not executed")
    run("worker_build",cargo+["build","--manifest-path",str(manifest),"--locked","--offline"]+patch,rh,600)
    binary=target/"debug/octendra-desktop-worker"
    receipt["worker_sha256"]=hashlib.sha256(binary.read_bytes()).hexdigest()
    version_log=run("worker_version",[str(binary),"--version"],rh,10)
    if version_log.read_text().strip()!="octendra-desktop-worker "+version: raise RuntimeError("worker version mismatch")
    receipt["worker_version"]=version
    run("disabled_capabilities",[str(binary),"--capabilities"],rh,10)
    run("disabled_pipe_exits",["python3","-I","-B","-u",str(rh/"scripts/check-desktop-worker-exits.py"),"--worker",str(binary),"--report",str(report)+".exits.json"],rh,30)
    run("portal_private_tests",cargo+["test","--manifest-path",str(rp/"Cargo.toml"),"-p","host","--lib","linux_portal::runtime::tests::worker_tests","--locked","--offline"],rp)
    receipt["success"]=True
except Exception as error:
    receipt["success"]=False;receipt["error"]=str(error)
finally:
    receipt["source_after"]=hashes();receipt["source_unchanged_during_checks"]=receipt["source_before"]==receipt["source_after"]
    receipt["success"] = receipt["success"] and receipt["source_unchanged_during_checks"]
    receipt["free_bytes"]=shutil.disk_usage(target).free;receipt["completed_at"]=int(time.time());save()
print(json.dumps({"report":str(report),"success":receipt["success"],"free_bytes":receipt["free_bytes"]}),flush=True)
raise SystemExit(0 if receipt["success"] else 1)
