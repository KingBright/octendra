#!/usr/bin/env python3
"""Add verified parallel-install entry files to the existing fixed current group.

Packaging-only: no Cargo/toolchain/SDK invocation and no deployment. Preserve the
original binary-source proof; separately disclose/validate packaging-only inputs.
"""
import argparse
import contextlib
import copy
import datetime
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile

import build_slot
import release_current
import source_snapshot

ROOT=Path(__file__).resolve().parents[1]
ALLOWED={
    'scripts/parallel-install.py','scripts/repackage-code-current.py',
    'scripts/check-parallel-delivery.py','scripts/package-code-release.py',
    'scripts/check-branding.py',
    'scripts/tests/test_parallel_install.py','scripts/tests/test_repackage_code_current.py',
    'scripts/tests/test_release_current.py',
}
DOCS={'docs/parallel-octendra/README.md','docs/parallel-octendra/gateway.json.in','docs/parallel-octendra/new-host.caddy.in'}
BINARY_NAMES={
    'octendra-code-macos-arm64','octendra-code-linux-amd64','octendra-code-windows-amd64.exe',
    'octendra-macos-arm64','octendra-linux-amd64','octendra-windows-amd64.exe',
}


def digest(path):
    return release_current.digest(path)


def packaging_inputs(root):
    return {name:digest(root/name) for name in sorted(ALLOWED|DOCS) if (root/name).is_file()}


def current_inputs(root):
    return source_snapshot.verifier(root).inputs(root)


def source_delta(base,current):
    changed={name for name in base.keys()|current.keys() if base.get(name)!=current.get(name)}
    forbidden=changed-ALLOWED
    if forbidden:
        raise ValueError('packaging-only mode refuses non-packaging source changes: '+', '.join(sorted(forbidden)))
    return sorted(changed)


def verify_base(root, expected):
    release,bundle=release_current.files(root)
    if release.is_symlink() or bundle.is_symlink() or (release/'manifest.json').is_symlink():
        raise ValueError('base release may not traverse symlinks')
    if not expected or digest(release/'manifest.json')!=expected:
        raise ValueError('base manifest differs from the explicit verified delivery hash')
    manifest=json.loads((release/'manifest.json').read_text())
    if manifest.get('instance_upgrade_protocol')!=1:
        raise ValueError('base binaries lack verified instance-bound upgrades; binary rebuild required')
    expected_members=set(manifest['artifacts'])|{'manifest.json','README.md','SHA256SUMS','source-verification.json'}
    if manifest.get('packaging'):expected_members.add('parallel-install-verification.json')
    if {p.name for p in release.iterdir()} != expected_members:
        raise ValueError('base contains unlisted files; do not copy unknown data or credentials')
    if set(manifest['included_platforms'])!={'macos','linux','windows'} or manifest['pending_platforms']:
        raise ValueError('base is not a complete three-platform release')
    proof=json.loads((release/'source-verification.json').read_text())
    if (proof.get('state')!='passed' or not proof.get('source_inputs_unchanged')
            or not proof.get('functional_tests',{}).get('evidence_complete')
            or proof['functional_tests']['failed']!=0
            or digest(release/'source-verification.json')!=manifest['source_verification_sha256']
            or proof['source_inputs']!=manifest['source_inputs']):
        raise ValueError('original compiled-source verification is incomplete or changed')
    if not BINARY_NAMES.issubset(manifest['artifacts']):
        raise ValueError('base binary set is incomplete')
    for name,info in manifest['artifacts'].items():
        if (Path(name).name!=name or (release/name).is_symlink() or digest(release/name)!=info['sha256']
                or (release/name).stat().st_size!=info['size']):
            raise ValueError('base artifact changed: '+name)
    # The existing archive is also checked against the exact current directory.
    with tarfile.open(bundle,'r:gz') as archive:
        entries=archive.getmembers();names=[p.name for p in entries]
        if len(names)!=len(set(names)) or set(names)!={p.name for p in release.iterdir()}:
            raise ValueError('base archive membership differs from current')
        for entry in entries:
            if not entry.isfile() or Path(entry.name).name!=entry.name:
                raise ValueError('base archive contains unsafe members')
            with archive.extractfile(entry) as stream:
                h=hashlib.sha256()
                for chunk in iter(lambda:stream.read(1048576),b''):h.update(chunk)
            if h.hexdigest()!=digest(release/entry.name):
                raise ValueError('base archive bytes differ from current')
    delta=source_delta(manifest['source_inputs'],current_inputs(root))
    return manifest,delta


def check_validation(root, path, base_hash):
    receipt=json.loads(path.read_text())
    if (receipt.get('kind')!='parallel-packaging-only' or receipt.get('state')!='passed'
            or receipt.get('base_manifest_sha256')!=base_hash
            or receipt.get('validation_inputs')!=packaging_inputs(root)
            or receipt.get('failed')!=0 or receipt.get('passed',0)<=0
            or set(receipt.get('checks',{}))!={'installer_tests','packaging_tests','release_entry_tests','branding'}
            or any(v.get('exit_code')!=0 for v in receipt['checks'].values())):
        raise ValueError('packaging-only targeted verification is missing, failed or stale')
    for info in receipt['checks'].values():
        log=Path(info['log'])
        if not log.is_file() or digest(log)!=info['sha256']:
            raise ValueError('targeted verification log changed or disappeared')
    return receipt


def packager(root):
    spec=importlib.util.spec_from_file_location('parallel_release_packager',root/'scripts/package-code-release.py')
    module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    return module


def stage(root, base, validation_path, delta, source_commit=None):
    current,_=release_current.files(root);dest,bundle=release_current.stages(root)
    if dest.exists() or bundle.exists():raise ValueError('observe/recover original fixed stage before packaging')
    dest.mkdir()
    excluded={'README.md','SHA256SUMS','manifest.json','parallel-install-verification.json'}
    for source in current.iterdir():
        if source.is_symlink() or not source.is_file():raise ValueError('current member is not a regular file')
        if source.name not in excluded:
            shutil.copyfile(source,dest/source.name);shutil.copymode(source,dest/source.name)
    module=packager(root);extras=module.parallel_files(root)
    for name,source in extras.items():
        if source.suffix=='.py':compile(source.read_text(),str(source),'exec')
        shutil.copyfile(source,dest/name)
    shutil.copyfile(validation_path,dest/'parallel-install-verification.json')
    manifest=copy.deepcopy(base)
    for name in extras:
        manifest['artifacts'][name]={'sha256':digest(dest/name),'size':(dest/name).stat().st_size}
    for name,info in base['artifacts'].items():
        if name not in extras and digest(dest/name)!=info['sha256']:
            raise ValueError('verified artifact changed during packaging-only copy')
    if digest(dest/'source-verification.json')!=base['source_verification_sha256']:
        raise ValueError('original binary verification receipt changed during copy')
    receipt=json.loads(validation_path.read_text())
    manifest['packaged_at']=datetime.datetime.now(datetime.timezone.utc).isoformat()
    manifest['default_install_strategy']='parallel-fresh-only'
    manifest.pop('parallel_limitations',None)
    manifest['packaging']={
        'mode':'entrypoints-only; no binaries rebuilt','binary_source_snapshot_id':base['snapshot_id'],
        'binary_source_manifest_sha256':receipt['base_manifest_sha256'],'source_commit':source_commit,
        'packaging_snapshot_id':source_snapshot.identity(current_inputs(root),source_snapshot.executable_files(root,current_inputs(root))),
        'changed_packaging_inputs':delta,'validation_inputs':receipt['validation_inputs'],
        'verification_sha256':digest(validation_path),'tests':{'passed':receipt['passed'],'failed':0},
        'compiled_source_receipt_preserved':True,'binary_hashes_unchanged':True,
    }
    (dest/'README.md').write_text(module.release_readme(base['version']))
    (dest/'manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
    (dest/'SHA256SUMS').write_text(''.join(digest(p)+'  '+p.name+'\n' for p in sorted(dest.iterdir()) if p.name!='SHA256SUMS'))
    with tarfile.open(bundle,'x:gz') as archive:
        for p in sorted(dest.iterdir()):archive.add(p,arcname=p.name)
    return manifest


def run(root, report, validation, base_hash):
    root=release_current.primary(root)
    if report.exists():raise ValueError('report already exists; observe the original packaging operation')
    report.parent.mkdir(parents=True,exist_ok=True)
    state={'state':'preparing','kind':'packaging-only','pid':os.getpid(),'binary_builds_started':0,'deployed':False,'stages':{}}
    with report.open('x') as stream:json.dump(state,stream)
    admitted=False;published=False
    with contextlib.ExitStack() as scope:
        try:
            base,delta=verify_base(root,base_hash)
            proof=check_validation(root,validation,base_hash)
            before=current_inputs(root);inputs=packaging_inputs(root)
            identity=source_snapshot.identity(before,source_snapshot.executable_files(root,before))
            scope.enter_context(build_slot.lease(root/'target/release-slot',report,identity,execution_root=root))
            state['current_preflight']=release_current.invoke(root,'release-preflight')
            state['current_admission']=release_current.admit(root);admitted=True
            state['state']='packaging';build_slot.atomic_json(report,state)
            commit=subprocess.check_output(['git','rev-parse','HEAD'],cwd=root,text=True).strip()
            manifest=stage(root,base,validation,delta,source_commit=commit)
            if before!=current_inputs(root) or inputs!=packaging_inputs(root):
                raise ValueError('packaging inputs changed during copy; current was not published')
            state['publication']=release_current.publish(root);published=True
            output,bundle=release_current.files(root)
            state.update(state='passed',source_snapshot_id=base['snapshot_id'],
                packaging_snapshot_id=identity,source_commit=commit,
                binary_hashes_unchanged=True,package={'path':str(output),'bundle':str(bundle),
                'manifest_sha256':digest(output/'manifest.json'),'bundle_sha256':digest(bundle),'bundle_size':bundle.stat().st_size},
                targeted_tests={'passed':proof['passed'],'failed':0},original_tests=manifest['tests'])
        except Exception as error:
            state.update(state='failed',failure_type=type(error).__name__,failure_detail=str(error)[:700])
            if admitted and not published:state['recovery']=release_current.abort(root)
        finally:
            state['finished_at']=datetime.datetime.now(datetime.timezone.utc).isoformat()
            build_slot.atomic_json(report,state)
    return state


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--report',type=Path,required=True)
    p.add_argument('--validation',type=Path,required=True)
    p.add_argument('--base-manifest-sha256',required=True)
    a=p.parse_args();r=run(ROOT,a.report.absolute(),a.validation.resolve(),a.base_manifest_sha256)
    print(json.dumps({k:r.get(k) for k in ('state','kind','pid','binary_builds_started','binary_hashes_unchanged','package','targeted_tests','failure_detail')}))
    return 0 if r['state']=='passed' else 1


if __name__=='__main__':sys.exit(main())
