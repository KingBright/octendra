#!/usr/bin/env python3
from pathlib import Path
import os,sys,json,shutil,threading,signal,resource,re,argparse,subprocess,tomllib
def updater_helpers(platform):
 helpers=['upgrade-code-agent.py' if platform=='macos' else 'upgrade-code-agent-linux.py','agent_upgrade_support.py','maintenance_client.py','macos_code_identity.py']
 if platform=='macos':helpers+=['launch-code-upgrade.py','code_upgrade_runner.py']
 return helpers

parser=argparse.ArgumentParser()
parser.add_argument('--source-root','--snapshot',dest='source_root',default=str(Path(__file__).resolve().parents[1]))
parser.add_argument('--snapshot-id',help='expected in-place source fingerprint; required for --apply')
parser.add_argument('--report',type=Path,help='durable report; an existing attempt is observation-only')
parser.add_argument('--version',required=True)
parser.add_argument('--apply',action='store_true')
parser.add_argument('--platform',choices=['linux','macos'],default='linux')
parser.add_argument('--verify-only',action='store_true',help='run fixed checks without packaging or deployment')
parser.add_argument('--verification-scope',choices=['all','capability','input'],default='all')
args=parser.parse_args()
if args.verification_scope!='all' and not args.verify_only:parser.error('focused verification cannot package a release')
if args.verification_scope=='input' and args.platform!='macos':parser.error('input verification requires the native macOS target')
assert re.fullmatch(r'[0-9]+[.][0-9]+[.][0-9]+',args.version)
source=Path(args.source_root).resolve();assert source==Path(__file__).resolve().parents[1], 'build in the actual primary checkout'
assert subprocess.check_output(['git','branch','--show-current'],cwd=source,text=True).strip()=='main'
assert Path(subprocess.check_output(['git','rev-parse','--show-toplevel'],cwd=source,text=True).strip()).resolve()==source
assert tomllib.loads((source/'crates/remote-hosts-code/Cargo.toml').read_text())['package']['version']==args.version, 'source version mismatch'
sys.path.insert(0,str(source/'scripts'));import source_snapshot,build_slot,build_process
provider=source_snapshot.verifier(source).inputs
inputs=provider(source);m={'source_inputs':inputs,'executable_files':source_snapshot.executable_files(source,inputs)}
m['snapshot_id']=source_snapshot.identity(inputs,m['executable_files'])
if args.apply:assert args.snapshot_id==m['snapshot_id'], 'source fingerprint required or changed; preflight again'
elif args.snapshot_id:assert args.snapshot_id==m['snapshot_id'], 'source fingerprint changed'
def check_source():
 current=provider(source)
 assert source_snapshot.identity(current,source_snapshot.executable_files(source,current))==m['snapshot_id'], 'main inputs changed; retain the original attempt'
report=(args.report or source/'.integration-artifacts'/('scoped-release-'+args.version+'-'+args.platform+'.json')).absolute()
base=report.parent;target=Path('/Users/jinliang/rust-target');assert target.is_dir() and shutil.disk_usage(target).free>8*2**30
out=build_slot.no_links(source/'dist/current'/args.platform)
if (out/'manifest.json').exists():
 previous=json.loads((out/'manifest.json').read_text())
 assert build_slot.digest(Path(previous['artifact']))==previous['sha256'], 'current artifact changed'
 assert previous['version']!=args.version or previous['snapshot_id']==m['snapshot_id'], 'published version cannot be replaced with different inputs'
if report.exists():
 old=json.loads(report.read_text())
 assert old.get('snapshot_id')==m['snapshot_id'], 'existing release identity conflict'
 if old.get('state')=='passed':
  assert build_slot.digest(Path(old['artifact']))==old['sha256']
  print(json.dumps({'state':'already_built','snapshot_id':m['snapshot_id'],'artifact':old['artifact'],'sha256':old['sha256'],'replayed':False}))
  sys.exit(0)
 print(json.dumps({'state':'observe_original_attempt','report':str(report),'original_pid':old.get('pid'),'original_state':old.get('state'),'replayed':False}))
 sys.exit(2)
if not args.apply:
 print(json.dumps({'state':'preflight_passed','version':args.version,'snapshot_id':m['snapshot_id'],'build_host':'Mac Studio','target':('aarch64-apple-darwin' if args.platform=='macos' else 'x86_64-unknown-linux-musl'),'native_cross_compile':args.platform=='linux','source_root':str(source),'source_copied':False,'cargo_target_dir':str(target),'output':str(out),'report':str(report),'docker':False,'free_gib':round(shutil.disk_usage(target).free/2**30,2)}))
 sys.exit(0)
soft,hard=resource.getrlimit(resource.RLIMIT_NOFILE);resource.setrlimit(resource.RLIMIT_NOFILE,(8192,hard))
state={'state':'preparing','version':args.version,'snapshot_id':m['snapshot_id'],'source_inputs':m['source_inputs'],'source_root':str(source),'source_commit':subprocess.check_output(['git','rev-parse','HEAD'],cwd=source,text=True).strip(),'source_copied':False,'cargo_target_dir':str(target),'deployed':False,'pid':os.getpid(),'checks':[]}
report.parent.mkdir(parents=True,exist_ok=True)
with report.open('x') as stream:json.dump(state,stream,indent=2);stream.flush();os.fsync(stream.fileno())
def update(v):
 state['build']=v;state['child_pid']=v.get('process_id');build_slot.atomic_json(report,state)
def interrupt(s,f):raise KeyboardInterrupt
signal.signal(signal.SIGTERM,interrupt);done=threading.Event()
def guard():
 while not done.wait(3):
  if shutil.disk_usage(target).free<5*2**30:os.kill(os.getpid(),signal.SIGTERM);return
threading.Thread(target=guard,daemon=True).start()
try:
 with build_slot.lease(target/'.remote-hosts-code-release-slot',report,m['snapshot_id']):
  checkout=source
  env=dict(os.environ,PATH='/Users/jinliang/.cargo/bin:'+os.environ.get('PATH',''),CARGO_TARGET_DIR=str(target),RUSTUP_TOOLCHAIN='1.94.1',CARGO_BUILD_JOBS='2',CARGO_INCREMENTAL='0',CARGO_TERM_COLOR='never',RUSTC='/Users/jinliang/.rustup/toolchains/1.94.1-aarch64-apple-darwin/bin/rustc')
  prefix=['/Users/jinliang/.cargo/bin/rustup','run','1.94.1','cargo']
  checks=([('clippy',None,0),('macos_worker','macos_worker',2),('desktop_protocol_and_transport','computer_use',29),('delivery','delivery',19),('job_receipts','job_receipts',8)] if args.platform=='macos' else [('clippy',None,0),('desktop_lifecycle_and_transport','computer_use',39),('payload_regressions','payload',23),('submission_fence','submission_fence',2),('job_receipts','job_receipts',8),('job_dispatch','job_dispatch',9)])
  if args.verification_scope=='capability':checks=[('clippy',None,0),('capability_current_authority','computer_use::transport::tests::capability_reports_',6)]
  if args.verification_scope=='input':checks=[('clippy',None,0),('macos_input_and_protocol_regression','computer_use',57)]
  if args.verification_scope=='all':checks += [('connection_authorization','task_authorization',8),('local_sudo','local_sudo',3),('terminal_recovery','terminal::tests',6)]
  state['verification_scope']=args.verification_scope
  for label,selection,minimum in checks:
   state.update(state='verifying',stage=label);build_slot.atomic_json(report,state)
   argv=prefix+(['test','-p','remote-hosts-code','--lib','--locked','--offline',selection,'--','--nocapture'] if selection else ['clippy','-p','remote-hosts-code','--all-targets','--locked','--offline','--','-D','warnings'])
   log=base/(label+'-'+args.version+'-'+args.platform+'.log');res=build_process.run(argv,checkout,log,env,idle_timeout=300,on_update=update);text=log.read_text();counts=[int(n) for n in re.findall(r'test result: ok\. (\d+) passed;',text)];count=sum(counts);passed=res['state']=='finished' and res['exit_code']==0 and (not selection or count>=minimum)
   state['checks'].append({'stage':label,'passed':passed,'tests':count,'minimum':minimum,'exit_code':res.get('exit_code'),'log':str(log)});build_slot.atomic_json(report,state)
   print(json.dumps(state['checks'][-1]),flush=True)
   if not passed:state['state']='verification_failed';build_slot.atomic_json(report,state);print(text[-10000:],flush=True);sys.exit(1)
   check_source()
  check_source();state.update(state='verification_passed',test_count=sum(x['tests'] for x in state['checks']),source_inputs_unchanged=True);build_slot.atomic_json(report,state)
  if args.verify_only:
   state.update(state='verified',packaged=False,deployed=False);build_slot.atomic_json(report,state);print(json.dumps({'state':'verified','snapshot_id':m['snapshot_id'],'test_count':state['test_count'],'packaged':False,'deployed':False}),flush=True);sys.exit(0)
  state.update(state='packaging',stage=args.platform+'_release');build_slot.atomic_json(report,state)
  target_triple='aarch64-apple-darwin' if args.platform=='macos' else 'x86_64-unknown-linux-musl'
  log=base/(args.platform+'-release-'+args.version+'.log');res=build_process.run(prefix+[('build' if args.platform=='macos' else 'zigbuild'),'-p','remote-hosts-code','--release','--locked','--offline','--target',target_triple],checkout,log,env,idle_timeout=300,on_update=update)
  if res['state']!='finished' or res['exit_code']!=0:state['state']='package_failed';build_slot.atomic_json(report,state);print(log.read_text()[-10000:],flush=True);sys.exit(1)
  binary=target/target_triple/'release/remote-hosts-code';data=binary.read_bytes()
  if args.platform=='macos':
   assert data[:4]==bytes.fromhex('cffaedfe') and int.from_bytes(data[4:8],'little')==0x0100000c
  else:
   assert data[:4]==b'\x7fELF' and data[4]==2 and int.from_bytes(data[18:20],'little')==62
   offset=int.from_bytes(data[32:40],'little');size=int.from_bytes(data[54:56],'little');count=int.from_bytes(data[56:58],'little');assert all(int.from_bytes(data[offset+n*size:offset+n*size+4],'little')!=3 for n in range(count))
  if args.platform=='macos':
   assert subprocess.check_output([str(binary),'--version'],text=True,timeout=15).strip()=='remote-hosts-code '+args.version
   contract=json.loads(subprocess.check_output([str(binary),'release-manifest'],text=True,timeout=15))
   assert contract['version']==args.version and contract['task_authorization_protocol']==2 and contract['tools_sha256']==contract['tool_schema_revision']
   state['machine_contract']=contract
  check_source()
  out.mkdir(parents=True,exist_ok=True)
  def publish_file(src,dst,mode):
   pending=build_slot.no_links(dst.with_name('.'+dst.name+'.pending'))
   with pending.open('xb') as writer,src.open('rb') as reader:
    shutil.copyfileobj(reader,writer);writer.flush();os.fsync(writer.fileno());os.fchmod(writer.fileno(),mode)
   assert build_slot.digest(pending)==build_slot.digest(src)
   os.replace(pending,dst)
  candidate=out/('remote-hosts-code-macos-arm64' if args.platform=='macos' else 'remote-hosts-code-linux-x86_64')
  publish_file(binary,candidate,0o755)
  helpers=updater_helpers(args.platform)+(['upgrade-code-gateway.py'] if args.platform=='linux' else [])
  for helper in helpers:publish_file(source/'scripts'/helper,out/helper,0o644)
  state['artifacts']={name:{'sha256':build_slot.digest(out/name),'bytes':(out/name).stat().st_size} for name in [candidate.name,*helpers]}
  check_source()
  state.update(state='passed',artifact=str(candidate),sha256=build_slot.digest(candidate),bytes=candidate.stat().st_size,artifact_platform=args.platform,artifact_target=target_triple,elf_machine=(None if args.platform=='macos' else 62),has_elf_interpreter=(None if args.platform=='macos' else False),compiler='rustc 1.94.1',free_gib_after=round(shutil.disk_usage(target).free/2**30,2));build_slot.atomic_json(report,state);build_slot.atomic_json(out/'manifest.json',state);print(json.dumps({k:v for k,v in state.items() if k not in ['build','source_inputs']}),flush=True)
except BaseException as error:
 if state['state'] not in ('passed','verified','verification_failed','package_failed'):state.update(state='interrupted' if isinstance(error,KeyboardInterrupt) else 'failed',failure_type=type(error).__name__);build_slot.atomic_json(report,state)
 raise
finally:done.set()
