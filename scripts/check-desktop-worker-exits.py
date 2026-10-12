#!/usr/bin/env python3
"""Actual disabled worker process exit checks; no Portal or approval."""
import argparse,json,pathlib,signal,struct,subprocess,time,hashlib
p=argparse.ArgumentParser();p.add_argument("--worker",required=True);p.add_argument("--report",required=True);a=p.parse_args()
binary=pathlib.Path(a.worker).resolve(strict=True);report=pathlib.Path(a.report)
if report.exists():raise SystemExit("inspect original report")
cases=[]
for name,payload,terminate in [
 ("idle_stdin_open",b"",False),
 ("partial_prefix_stdin_open",b"\x00\x00",False),
 ("partial_body_stdin_open",struct.pack(">I",64)+b"{",False),
 ("sigterm_stdin_open",b"",True),
 ("sigterm_partial_body",struct.pack(">I",64)+b"{",True)]:
 started=time.monotonic()
 child=subprocess.Popen([str(binary)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.DEVNULL,start_new_session=True)
 timed_out=False
 try:
  if payload:child.stdin.write(payload);child.stdin.flush()
  if terminate:time.sleep(.15);child.send_signal(signal.SIGTERM)
  child.wait(timeout=2 if terminate else 6.5)
 except subprocess.TimeoutExpired:
  timed_out=True;child.kill();child.wait(timeout=2)
 finally:
  # The parent's pipe remains open until AFTER the child exits.
  child.stdin.close();child.stdout.close()
 cases.append({"case":name,"exit_code":child.returncode,"elapsed_seconds":round(time.monotonic()-started,3),
  "parent_stdin_closed_before_exit":False,"timed_out":timed_out})
result={"worker_sha256":hashlib.sha256(binary.read_bytes()).hexdigest(),"cases":cases,
 "success":all(not c["timed_out"] and c["exit_code"]==0 for c in cases),"portal_used":False}
report.write_text(json.dumps(result,indent=2));print(json.dumps(result),flush=True)
raise SystemExit(0 if result["success"] else 1)
