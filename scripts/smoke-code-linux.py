#!/usr/bin/env python3
"""Run finished Linux Gateways with authenticated registration fixtures; never compile."""
import argparse, hashlib, json, pathlib, secrets, shutil, socket, sqlite3, subprocess, time
import urllib.request, urllib.error
p=argparse.ArgumentParser()
for name in ['candidate','legacy-candidate','root']:p.add_argument('--'+name,required=True,type=pathlib.Path)
for name in ['candidate-sha256','legacy-sha256','version','legacy-version']:p.add_argument('--'+name,required=True)
p.add_argument('--apply',action='store_true');a=p.parse_args()
sha=lambda p:hashlib.sha256(p.read_bytes()).hexdigest()
assert sha(a.candidate)==a.candidate_sha256 and sha(a.legacy_candidate)==a.legacy_sha256
assert not a.root.exists(), 'existing fixture must be observed, never replayed'
if not a.apply:
 print(json.dumps({'state':'preflight_passed','compiler_started':False,'production_changes':False,
  'candidate_sha256':a.candidate_sha256,'legacy_sha256':a.legacy_sha256}));raise SystemExit(0)
a.root.mkdir(parents=True);old=a.root/'legacy-binary';shutil.copy2(a.legacy_candidate,old);old.chmod(0o755)
assert sha(old)==a.legacy_sha256
report={'state':'running','compiler_started':False,'production_changes':False,
 'agent_process_started':False,'scope':'real_gateway_authenticated_registration_fixture','cases':[]}
def publish():(a.root/'smoke-result.json').write_text(json.dumps(report))
publish()
def run_case(label,gateway,expected_gateway,fixture_version,legacy):
 root=a.root/label;root.mkdir();code_root=root/'empty-root';code_root.mkdir()
 state=root/'gateway-state';cfg=root/'gateway.json';agent_cfg=root/'fixture-device.json'
 sock=socket.socket();sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
 origin='https://fixture.invalid:'+str(port);backend='http://127.0.0.1:'+str(port)
 def setup(argv):
  q=subprocess.run(argv,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15)
  assert q.returncode==0, 'fixture_setup_failed'
 setup([str(gateway),'init-gateway','--config',str(cfg),'--public-url',origin,'--bind','127.0.0.1:'+str(port),
  '--state-dir',str(state),'--owner','isolated-verification','--password-file',str(root/'owner-password')])
 setup([str(gateway),'enroll','--gateway-config',str(cfg),'--agent-config',str(agent_cfg),'--name',label,
  '--state-dir',str(root/'unused-agent-state'),'--root',str(code_root)])
 device=json.loads(agent_cfg.read_text());contract=json.loads(subprocess.check_output([str(gateway),'release-manifest']))
 headers={'Host':'fixture.invalid:'+str(port),'Content-Type':'application/json','Authorization':'Bearer '+device['device_token']}
 gw=None;result={'label':label,'compiler_started':False,'production_changes':False,'agent_process_started':False}
 def request(path,value=None):
  req=urllib.request.Request(backend+path,data=json.dumps(value).encode() if value is not None else None,headers=headers)
  try:
   with urllib.request.urlopen(req,timeout=3) as response:return response.status,json.load(response)
  except urllib.error.HTTPError as error:return error.code,None
 try:
  gw=subprocess.Popen([str(gateway),'gateway','--config',str(cfg)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  deadline=time.monotonic()+15;health=None
  while time.monotonic()<deadline:
   assert gw.poll() is None, 'fixture_gateway_exited'
   try:
    status,health=request('/healthz')
    if status==200:break
   except (OSError,ValueError):pass
   time.sleep(0.2)
  assert health and health['version']==expected_gateway, 'fixture_gateway_not_ready'
  hello={'version':fixture_version,'wire_protocol':contract['wire_protocol'],'session':secrets.token_hex(32),
   'platform':'linux','arch':'x86_64','roots':[str(code_root)],'allow_write':False,'allow_exec':False,
   'runtime_features':{'protocol':1,'names':['future_feature_'+str(i) for i in range(33)]},'poll_wait_ms':100}
  statuses=[]
  if legacy:
   status,_=request('/device/poll',hello);statuses.append(status);assert status==400
   with sqlite3.connect('file:'+str(state/'state.sqlite')+'?mode=ro',uri=True) as c:
    assert c.execute("SELECT COUNT(*) FROM kv WHERE kind='online' AND key=?",(device['device_id'],)).fetchone()[0]==0
   hello.pop('runtime_features')
  for lane in ['read','write','transfer','terminal','control']:
   hello['lanes']=[lane];status,_=request('/device/poll',hello);statuses.append(status);assert status==200
  with sqlite3.connect('file:'+str(state/'state.sqlite')+'?mode=ro',uri=True) as c:
   row=c.execute("SELECT value FROM kv WHERE kind='online' AND key=?",(device['device_id'],)).fetchone()
  assert row;online=json.loads(row[0]);names=(online.get('runtime_features') or {}).get('names')
  assert (names is None)==legacy
  if names is not None:assert len(names)==33
  assert online['hello']['allow_write'] is False and online['hello']['allow_exec'] is False
  assert json.loads(cfg.read_text())['devices'][0]['scopes']==['code:read']
  result.update(state='passed',gateway_version=health['version'],registration_fixture_version=fixture_version,
   http_statuses=statuses,optional_features_reported=names is not None,
   feature_count=len(names) if names is not None else None,fixture_scope=['code:read'],gateway_pid=gw.pid)
 finally:
  if gw is not None:
   if gw.poll() is None:gw.terminate()
   try:gw.wait(timeout=5)
   except subprocess.TimeoutExpired:gw.kill();gw.wait(timeout=5)
  result['owned_processes_reaped']=gw is None or gw.poll() is not None
 return result
try:
 for args in [('new-gateway-current-registration',a.candidate,a.version,a.legacy_version,False),
  ('old-gateway-optional-metadata-omitted',old,a.legacy_version,a.version,True)]:
  report['cases'].append(run_case(*args));publish()
 report['state']='passed';publish()
except Exception as error:
 report.update(state='failed',error_type=type(error).__name__);publish();raise
print(json.dumps(report))
