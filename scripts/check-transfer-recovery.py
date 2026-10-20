#!/usr/bin/env python3
"""Fixed isolated transfer acceptance: synthetic grants, temporary loopback servers.

No owner login, device enrollment, service manager, deployment or Git action.
Only subprocesses created by these fixtures are restarted/killed. Primary main input
fingerprints and the existing metadata lease bind receipts and serialize Cargo.
"""
import argparse
import hashlib
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import time

import build_slot
import release_current

ROOT=pathlib.Path(__file__).resolve().parents[1]
CASES=[
    ('lib','transfers::tests::source_diagnostics_distinguish_dns_permission_and_address_policy_without_urls','DNS/permission/address policy classification and URL redaction'),
    ('transfers_040','source_diagnostics_address_rejection_preserves_original_operation_and_target','real DNS address policy rejection before source HTTP; original operation retained'),
    ('transfers_040','source_diagnostics_paused_observation_tracks_source_expiry_without_replay','paused source availability/expiry/missing state and owner/scope rejection'),
    ('lib','durable_transfer::tests::expired_source_keeps_checkpoint_for_explicit_authorization_refresh','real HTTP403 retains checkpoint'),
    ('lib','durable_transfer::tests::expired_source_refresh_resumes_same_checkpoint_in_a_new_process','real HTTP403 then refreshed same-source tail in a new OS process'),
    ('lib','durable_transfer::tests::inbound_checkpoint_survives_a_real_process_kill_and_resumes_only_the_tail','owned transfer process killed after durable checkpoint; same prefix and tail'),
    ('transfers_040','paused_source_is_not_cached_as_permanent_done','real Agent/Gateway suspended state remains resumable'),
    ('transfers_040','resume_keeps_job_identity_and_rejects_wrong_owner_or_changed_file','real Gateway original identity and source/owner rejection'),
    ('transfers_040','r041_exact_resume_retry_refreshes_url_without_new_generation','real Gateway same-generation explicit source authorization refresh'),
    ('transfers_040','r041_old_resume_retry_never_overwrites_newer_authorization','real Gateway stale resume cannot overwrite newer source'),
    ('transfers_040','r041_concurrent_duplicate_controls_have_one_durable_effect','real Gateway concurrent duplicate resume has one durable effect'),
    ('transfers_040','gateway_durable_offset_survives_actual_server_process_kill','owned loopback Gateway process kill/restart; same offset and final bytes'),
    ('transfers_040','interrupted_export_resumes_saved_snapshot_not_the_changed_source','real network checkpoint and reopened Agent; immutable snapshot SHA'),
]


def run_stage(name, argv, checkout, env, directory, state, timeout=1200):
    release_current.check_inputs(checkout, state['source_evidence'])
    log=directory/(name+'.log')
    entry={'state':'running','command':argv,'log':str(log),'started_at':int(time.time())}
    state['stages'][name]=entry
    build_slot.atomic_json(directory/'verification.json',state)
    with log.open('wb') as output:
        process=subprocess.Popen(argv,cwd=checkout,env=env,stdout=output,stderr=subprocess.STDOUT,start_new_session=True)
        state['child_pid']=process.pid
        build_slot.atomic_json(directory/'verification.json',state)
        try:
            code=process.wait(timeout=timeout)
        except BaseException:
            # Only this script's own process group; never a global process search.
            try: os.killpg(process.pid,signal.SIGTERM)
            except ProcessLookupError: pass
            try: process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid,signal.SIGKILL)
                process.wait()
            raise
        finally:
            state.pop('child_pid',None)
    entry.update(exit_code=code,finished_at=int(time.time()),log_sha256=build_slot.digest(log))
    if code != 0:
        entry['state']='failed'
        raise RuntimeError('isolated acceptance stage failed: '+name)
    text=log.read_text(errors='replace')
    if re.fullmatch(r'rust_[0-9]+',name):
        if not re.search(r'test result: ok\. 1 passed; 0 failed; 0 ignored;',text):
            raise RuntimeError('exact Rust case not executed: '+name)
        entry.update(executed=1,passed=1,failed=0,skipped=0)
    elif name=='python_coordinator':
        count=re.search(r'Ran (\d+) tests?',text)
        if not count or not re.search(r'^OK$',text,re.M):
            raise RuntimeError('coordinator acceptance incomplete')
        entry.update(executed=int(count[1]),passed=int(count[1]),failed=0,skipped=0)
    release_current.check_inputs(checkout, state['source_evidence'])
    entry['state']='passed'
    build_slot.atomic_json(directory/'verification.json',state)
    print(json.dumps({'stage':name,'state':'passed','executed':entry.get('executed',0)}),flush=True)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report-dir',type=pathlib.Path,required=True)
    parser.add_argument('--build-slot',type=pathlib.Path,default=ROOT/'target/release-slot')
    args=parser.parse_args()
    release_current.primary(ROOT)
    slot=build_slot.no_links(args.build_slot)
    if slot.resolve() != (ROOT/'target/release-slot').resolve():
        raise ValueError('acceptance must reuse the existing target/release-slot metadata lease')
    directory=args.report_dir.absolute()
    if directory.exists():
        raise FileExistsError('acceptance report already exists; observe its original evidence before another isolated run')
    directory.mkdir(parents=True)
    state={'schema_version':1,'state':'preparing','scope':'isolated_fixture_protocol_and_process_recovery',
           'production_touched':False,'new_persistent_authorization':False,'deployment_requested':False,
           'cases':[{'target':t,'test':n,'boundary':b} for t,n,b in CASES],'stages':{}}
    build_slot.atomic_json(directory/'verification.json',state)
    try:
        captured=release_current.snapshot_evidence(None, ROOT)
        state.update(snapshot_id=captured['snapshot_id'],source_inputs=captured['source_inputs'],source_evidence=captured)
        with build_slot.lease(slot,directory/'verification.json',captured['snapshot_id'],execution_root=ROOT) as checkout:
            release_current.check_inputs(checkout,captured)
            env=dict(os.environ,CARGO_TARGET_DIR=str(pathlib.Path.home()/'rust-target'))
            state['target']=str(release_current.established_target(checkout,env))
            state['execution_root']=str(checkout)
            for key in ('RH040_CHECKPOINT_FIXTURE','RH040_GATEWAY_FIXTURE'):
                env.pop(key,None)
            run_stage('python_coordinator',[sys.executable,'-B','-W','error::ResourceWarning','-m','unittest',
                       'discover','-s','scripts/tests','-p','test_fleet_transfer*.py','-v'],checkout,env,directory,state,120)
            run_stage('rust_format',['cargo','fmt','--all','--','--check'],checkout,env,directory,state,120)
            for index,(target,name,_boundary) in enumerate(CASES,1):
                selection=['--lib'] if target=='lib' else ['--test',target]
                run_stage('rust_'+str(index),['cargo','test','--offline','--locked','-p','octendra-code',
                          *selection,name,'--','--exact','--nocapture'],checkout,env,directory,state)
            release_current.check_inputs(checkout,captured)
            state.update(state='passed',python_passed=state['stages']['python_coordinator']['passed'],
                         rust_passed=len(CASES),failed=0,skipped=0,
                         accepted_scope='fixture coordinator plus actual Rust protocol/owned process recovery; not deployed fleet',
                         formal_runtime_acceptance='not_run')
    except BaseException as error:
        state.update(state='failed',error_type=type(error).__name__,
                     next_action='inspect the original stage log and owned child; no production operation may be replayed')
        raise
    finally:
        build_slot.atomic_json(directory/'verification.json',state)
    print(json.dumps({key:state[key] for key in ('state','snapshot_id','python_passed','rust_passed','failed','skipped','production_touched')}))


if __name__=='__main__':
    main()
