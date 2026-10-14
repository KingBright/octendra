#!/usr/bin/env python3
"""Read-only preflight or idempotent Linux Gateway deployment using the existing updater.
No compiler, credentials, network configuration changes, or user-source rollback.
"""
import argparse, hashlib, importlib.util, json, pathlib, platform, subprocess, sys, socket
def sha(p):
 h=hashlib.sha256()
 with pathlib.Path(p).open('rb') as f:
  for b in iter(lambda:f.read(1024*1024),b''):h.update(b)
 return h.hexdigest()
def static_linux_x64(p):
 with pathlib.Path(p).open('rb') as f:
  data=f.read(64)
  if len(data)!=64 or data[:4]!=b'\x7fELF' or data[4]!=2 or data[5]!=1 or int.from_bytes(data[18:20],'little')!=62:raise ValueError('wrong_candidate_platform')
  off=int.from_bytes(data[32:40],'little');size=int.from_bytes(data[54:56],'little');count=int.from_bytes(data[56:58],'little')
  if size<4 or count>128:raise ValueError('invalid_elf_program_headers')
  for n in range(count):
   f.seek(off+n*size)
   if int.from_bytes(f.read(4),'little')==3:raise ValueError('candidate_not_static')
def checked_plan(a):
 if platform.system()!='Linux' or platform.machine() not in ('x86_64','amd64'):raise ValueError('wrong_deployment_machine')
 for value in [a.sha256,a.source_snapshot_id,a.expected_config_sha256,a.expected_current_sha256]:
  if len(value)!=64 or any(c not in '0123456789abcdef' for c in value):raise ValueError('invalid_identity')
 if sha(a.candidate)!=a.sha256:raise ValueError('candidate_hash_mismatch')
 static_linux_x64(a.candidate)
 if subprocess.check_output([str(a.candidate),'--version'],text=True,timeout=10).strip()!='remote-hosts-code '+a.version:raise ValueError('candidate_version_mismatch')
 if sha(a.config_path)!=a.expected_config_sha256:raise ValueError('configuration_changed_stop')
 current_sha=sha(a.binary_path);current_version=subprocess.check_output([str(a.binary_path),'--version'],text=True,timeout=10).strip().replace('remote-hosts-code ', '', 1)
 same=current_sha==a.sha256 and current_version==a.version
 if current_version==a.version and not same:raise ValueError('same_version_different_hash_refused')
 if not same and current_sha!=a.expected_current_sha256:raise ValueError('running_baseline_changed_stop')
 spec=importlib.util.spec_from_file_location('existing_gateway_updater',a.updater);mod=importlib.util.module_from_spec(spec);spec.loader.exec_module(mod)
 subprocess.run([str(a.candidate),'check','--config',str(a.config_path)],check=True,stdout=subprocess.DEVNULL,timeout=15)
 cfg=json.loads(a.config_path.read_text());mod.require_task_authorization_support(pathlib.Path(cfg['state_dir'])/'state.sqlite',a.candidate,'preflight')
 return {'state':'already_installed' if same else 'preflight_passed','version':a.version,'candidate_sha256':a.sha256,'source_snapshot_id':a.source_snapshot_id,'source_commit':a.source_commit,'source_commit_status':'explicit_snapshot_identity' if a.source_commit=='snapshot-no-git-metadata' else 'caller_declared_base_commit','build_host':a.build_host,'deployment_host':socket.gethostname(),'target':'linux-x86_64-static-musl','current_version':current_version,'current_sha256':current_sha,'same_version_same_hash':same,'compiler_started':False}
def main():
 p=argparse.ArgumentParser()
 for n in ['candidate','binary-path','config-path','backup-root','result','updater']:p.add_argument('--'+n,required=True,type=pathlib.Path)
 for n in ['sha256','version','source-snapshot-id','source-commit','build-host','expected-config-sha256','expected-current-sha256']:p.add_argument('--'+n,required=True)
 p.add_argument('--apply',action='store_true');a=p.parse_args();plan=checked_plan(a)
 print(json.dumps(plan),flush=True)
 if not a.apply or plan['same_version_same_hash']:return
 if a.result.exists():raise ValueError('original_result_exists_observe_it_never_replay')
 argv=[sys.executable,str(a.updater),'--candidate',str(a.candidate),'--sha256',a.sha256,'--version',a.version,'--result',str(a.result),'--binary-path',str(a.binary_path),'--config-path',str(a.config_path),'--backup-root',str(a.backup_root)]
 subprocess.run(argv,check=True,timeout=180)
 record=json.loads(a.result.read_text())
 if record.get('state')!='upgraded' or record.get('installed_sha256')!=a.sha256:raise ValueError('deployment_not_confirmed_stop')
 print(json.dumps({'state':'verified_upgraded','version':a.version,'sha256':a.sha256,'result':str(a.result),'backup':record.get('backup'),'pid':record.get('pid')}),flush=True)
if __name__=='__main__':main()
