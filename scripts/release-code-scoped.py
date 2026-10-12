#!/usr/bin/env python3
from pathlib import Path
import os,sys,json,shutil,threading,signal,resource,re,argparse
parser=argparse.ArgumentParser()
parser.add_argument('--snapshot',required=True)
parser.add_argument('--snapshot-id',required=True)
parser.add_argument('--version',required=True)
parser.add_argument('--apply',action='store_true')
args=parser.parse_args()
assert re.fullmatch(r'[0-9]+[.][0-9]+[.][0-9]+',args.version)
source=Path(args.snapshot).resolve();base=source.parent;sys.path.insert(0,str(source/'scripts'));import source_snapshot,build_slot,build_process
m=source_snapshot.check(source);assert m['snapshot_id']==args.snapshot_id
report=base/('scoped-release-'+args.version+'.json');target=Path('/Users/jinliang/rust-target');assert shutil.disk_usage(target).free>8*2**30
if report.exists():
 old=json.loads(report.read_text())
 assert old.get('snapshot_id')==args.snapshot_id, 'existing release identity conflict'
 if old.get('state')=='passed':
  assert build_slot.digest(Path(old['artifact']))==old['sha256']
  print(json.dumps({'state':'already_built','snapshot_id':args.snapshot_id,'artifact':old['artifact'],'sha256':old['sha256'],'replayed':False}))
  sys.exit(0)
 print(json.dumps({'state':'observe_original_attempt','report':str(report),'original_pid':old.get('pid'),'original_state':old.get('state'),'replayed':False}))
 sys.exit(2)
if not args.apply:
 print(json.dumps({'state':'preflight_passed','version':args.version,'snapshot_id':m['snapshot_id'],'build_host':'Mac Studio','target':'x86_64-unknown-linux-musl','native_cross_compile':True,'docker':False,'free_gib':round(shutil.disk_usage(target).free/2**30,2)}))
 sys.exit(0)
soft,hard=resource.getrlimit(resource.RLIMIT_NOFILE);resource.setrlimit(resource.RLIMIT_NOFILE,(8192,hard))
state={'state':'preparing','version':args.version,'snapshot_id':m['snapshot_id'],'component_scope':'gateway_compatibility_and_official_unknown_recovery','agent_candidate_reused':'0.10.32','deployed':False,'pid':os.getpid(),'checks':[]};build_slot.atomic_json(report,state)
def update(v):state['build']=v;build_slot.atomic_json(report,state)
def interrupt(s,f):raise KeyboardInterrupt
signal.signal(signal.SIGTERM,interrupt);done=threading.Event()
def guard():
 while not done.wait(3):
  if shutil.disk_usage(target).free<5*2**30:os.kill(os.getpid(),signal.SIGTERM);return
threading.Thread(target=guard,daemon=True).start()
try:
 with build_slot.lease(target/'.remote-hosts-code-release-slot',report,m['snapshot_id']) as checkout:
  state['sync']=build_slot.synchronize(source,checkout)
  env=dict(os.environ,PATH='/Users/jinliang/.cargo/bin:'+os.environ.get('PATH',''),CARGO_TARGET_DIR=str(target),RUSTUP_TOOLCHAIN='1.94.1',CARGO_BUILD_JOBS='2',CARGO_TERM_COLOR='never',RUSTC='/Users/jinliang/.rustup/toolchains/1.94.1-aarch64-apple-darwin/bin/rustc')
  prefix=['/Users/jinliang/.cargo/bin/rustup','run','1.94.1','cargo']
  for label,selection,minimum in [('payload_regressions','payload',23),('submission_fence','submission_fence',2),('job_receipts','job_receipts',8),('job_dispatch','job_dispatch',9),('clippy',None,0)]:
   state.update(state='verifying',stage=label);build_slot.atomic_json(report,state)
   argv=prefix+(['test','-p','remote-hosts-code','--lib','--locked','--offline',selection,'--','--nocapture'] if selection else ['clippy','-p','remote-hosts-code','--all-targets','--locked','--offline','--','-D','warnings'])
   log=base/(label+'-'+args.version+'.log');res=build_process.run(argv,checkout,log,env,idle_timeout=300,on_update=update);text=log.read_text();counts=[int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',text)];count=sum(counts);passed=res['state']=='finished' and res['exit_code']==0 and (not selection or count>=minimum)
   state['checks'].append({'stage':label,'passed':passed,'tests':count,'minimum':minimum,'exit_code':res.get('exit_code'),'log':str(log)});build_slot.atomic_json(report,state)
   print(json.dumps(state['checks'][-1]),flush=True)
   if not passed:state['state']='verification_failed';build_slot.atomic_json(report,state);print(text[-10000:],flush=True);sys.exit(1)
  source_snapshot.check(source);source_snapshot.check(checkout);state.update(state='verification_passed',test_count=sum(x['tests'] for x in state['checks']));build_slot.atomic_json(base/('verified-regression-'+args.version+'.json'),state)
  state.update(state='packaging',stage='linux_release');build_slot.atomic_json(report,state)
  log=base/('linux-release-'+args.version+'.log');res=build_process.run(prefix+['zigbuild','-p','remote-hosts-code','--release','--locked','--offline','--target','x86_64-unknown-linux-musl'],checkout,log,env,idle_timeout=300,on_update=update)
  if res['state']!='finished' or res['exit_code']!=0:state['state']='package_failed';build_slot.atomic_json(report,state);print(log.read_text()[-10000:],flush=True);sys.exit(1)
  binary=target/'x86_64-unknown-linux-musl/release/remote-hosts-code';data=binary.read_bytes();assert data[:4]==b'\x7fELF' and data[4]==2 and int.from_bytes(data[18:20],'little')==62
  offset=int.from_bytes(data[32:40],'little');size=int.from_bytes(data[54:56],'little');count=int.from_bytes(data[56:58],'little');assert all(int.from_bytes(data[offset+n*size:offset+n*size+4],'little')!=3 for n in range(count))
  out=base/('artifacts-'+args.version);out.mkdir(exist_ok=False);candidate=out/('remote-hosts-code-'+args.version+'-linux-x86_64');shutil.copy2(binary,candidate);candidate.chmod(0o755)
  shutil.copy2(source/'scripts/upgrade-code-gateway.py',out/'upgrade-code-gateway.py')
  source_snapshot.check(source);source_snapshot.check(checkout)
  state.update(state='passed',artifact=str(candidate),sha256=build_slot.digest(candidate),bytes=candidate.stat().st_size,elf_machine=62,has_elf_interpreter=False,compiler='rustc 1.94.1',free_gib_after=round(shutil.disk_usage(target).free/2**30,2));build_slot.atomic_json(report,state);build_slot.atomic_json(out/'manifest.json',state);print(json.dumps({k:v for k,v in state.items() if k not in ['build','sync']}),flush=True)
finally:done.set()
