#!/usr/bin/env python3
"""Exercise finished Linux Gateway/Agent binaries in isolated temporary fixtures; never compile."""
import argparse, hashlib, json, pathlib, shutil, socket, sqlite3, subprocess, time, urllib.request
p=argparse.ArgumentParser()
for name in ['candidate','legacy-candidate','root']:
 p.add_argument('--'+name,required=True,type=pathlib.Path)
for name in ['candidate-sha256','legacy-sha256','version','legacy-version']:p.add_argument('--'+name,required=True)
p.add_argument('--apply',action='store_true')
a=p.parse_args()
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(a.candidate)==a.candidate_sha256 and sha(a.legacy_candidate)==a.legacy_sha256
assert not a.root.exists(), 'existing fixture must be observed, never replayed'
if not a.apply:
 print(json.dumps({'state':'preflight_passed','compiler_started':False,'production_changes':False,
  'candidate_sha256':a.candidate_sha256,'legacy_sha256':a.legacy_sha256}));raise SystemExit(0)
a.root.mkdir(parents=True)
old=a.root/'legacy-binary';shutil.copy2(a.legacy_candidate,old);old.chmod(0o755)
assert sha(old)==a.legacy_sha256
report={'state':'running','compiler_started':False,'production_changes':False,'cases':[]}
def publish():
 (a.root/'smoke-result.json').write_text(json.dumps(report))
publish()
def run_case(label,gateway,agent,expected_gateway,expected_agent,optional_features):
 root=a.root/label;root.mkdir();code_root=root/'empty-root';code_root.mkdir()
 state=root/'gateway-state';agent_state=root/'agent-state';cfg=root/'gateway.json';agent_cfg=root/'agent.json'
 sock=socket.socket();sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
 url='http://127.0.0.1:'+str(port)
 def setup(argv):
  r=subprocess.run(argv,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15)
  assert r.returncode==0, 'fixture_setup_failed'
 setup([str(gateway),'init-gateway','--config',str(cfg),'--public-url',url,'--bind','127.0.0.1:'+str(port),
  '--state-dir',str(state),'--owner','isolated-verification','--password-file',str(root/'owner-password')])
 setup([str(gateway),'enroll','--gateway-config',str(cfg),'--agent-config',str(agent_cfg),'--name',label,
  '--state-dir',str(agent_state),'--root',str(code_root)])
 gw=None;ag=None;result={'label':label,'compiler_started':False,'production_changes':False}
 try:
  gw=subprocess.Popen([str(gateway),'gateway','--config',str(cfg)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  deadline=time.monotonic()+15;health=None
  while time.monotonic()<deadline:
   assert gw.poll() is None, 'fixture_gateway_exited'
   try:
    with urllib.request.urlopen(url+'/healthz',timeout=1) as response:health=json.load(response)
    break
   except (OSError,ValueError):time.sleep(0.2)
  assert health and health['version']==expected_gateway, 'fixture_gateway_not_ready'
  ag=subprocess.Popen([str(agent),'agent','--config',str(agent_cfg),'--foreground'],
   stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  device=json.loads(agent_cfg.read_text())['device_id'];online=None;deadline=time.monotonic()+20
  while time.monotonic()<deadline:
   assert ag.poll() is None and gw.poll() is None, 'fixture_process_exited'
   try:
    with sqlite3.connect('file:'+str(state/'state.sqlite')+'?mode=ro',uri=True) as c:
     row=c.execute("SELECT value FROM kv WHERE kind='online' AND key=?",(device,)).fetchone()
    if row:
     online=json.loads(row[0])
     if online['hello']['version']==expected_agent:break
   except (sqlite3.Error,ValueError):pass
   time.sleep(0.2)
  assert online and online['hello']['version']==expected_agent, 'fixture_agent_not_registered'
  names=(online.get('runtime_features') or {}).get('names')
  assert (names is not None)==optional_features, 'unexpected_optional_metadata'
  if optional_features:assert len(names)>32
  result.update(state='passed',gateway_version=health['version'],agent_version=online['hello']['version'],
   optional_features_reported=names is not None,feature_count=len(names) if names is not None else None,
   allow_write=online['hello']['allow_write'],allow_exec=online['hello']['allow_exec'],
   gateway_pid=gw.pid,agent_pid=ag.pid)
  assert result['allow_write'] is False and result['allow_exec'] is False
 finally:
  for proc in [ag,gw]:
   if proc is not None:
    if proc.poll() is None:proc.terminate()
    try:proc.wait(timeout=5)
    except subprocess.TimeoutExpired:proc.kill();proc.wait(timeout=5)
  result['owned_processes_reaped']=all(proc is None or proc.poll() is not None for proc in [ag,gw])
 return result
try:
 for args in [
  ('new-gateway-old-agent',a.candidate,old,a.version,a.legacy_version,True),
  ('old-gateway-new-agent',old,a.candidate,a.legacy_version,a.version,False)]:
  report['cases'].append(run_case(*args));publish()
 report['state']='passed';publish()
except Exception as error:
 report.update(state='failed',error_type=type(error).__name__);publish();raise
print(json.dumps(report))
