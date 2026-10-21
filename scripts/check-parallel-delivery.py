#!/usr/bin/env python3
"""Fixed targeted checks for entrypoint-only packaging; never runs Cargo."""
import argparse
import datetime
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import sys

import build_slot
ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('parallel_repack_verify',ROOT/'scripts/repackage-code-current.py')
repack=importlib.util.module_from_spec(spec);spec.loader.exec_module(repack)


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--report',type=Path,required=True);p.add_argument('--base-manifest-sha256',required=True)
    a=p.parse_args();report=a.report.absolute();report.parent.mkdir(parents=True,exist_ok=True)
    with report.open('x') as stream:json.dump({'state':'preparing','kind':'parallel-packaging-only'},stream)
    result={'state':'running','kind':'parallel-packaging-only','base_manifest_sha256':a.base_manifest_sha256,
            'validation_inputs':repack.packaging_inputs(ROOT),'checks':{},'passed':0,'failed':0,'binaries_rebuilt':False}
    logs=report.parent/(report.stem+'-logs');logs.mkdir()
    try:
        repack.verify_base(ROOT,a.base_manifest_sha256)
        for name,pattern in (('installer_tests','test_parallel_install.py'),
                             ('packaging_tests','test_repackage_code_current.py'),
                             ('release_entry_tests','test_release_current.py'),('branding',None)):
            command=([sys.executable,'-W','error::ResourceWarning','-m','unittest','discover','-s','scripts/tests','-p',pattern,'-v']
                     if pattern else [sys.executable,'scripts/check-branding.py'])
            path=logs/(name+'.log')
            with path.open('xb') as stream:r=subprocess.run(command,cwd=ROOT,stdout=stream,stderr=subprocess.STDOUT)
            text=path.read_text(errors='replace');count=re.search(r'Ran (\d+) tests?',text)
            result['checks'][name]={'exit_code':r.returncode,'log':str(path),'sha256':repack.digest(path),'tests':int(count[1]) if count else 0}
            if r.returncode:result['failed']+=1;raise ValueError('targeted check failed: '+name)
            if pattern:
                if not count or int(count[1])==0 or not re.search(r'^OK$',text,re.MULTILINE):raise ValueError('incomplete targeted test evidence')
                result['passed']+=int(count[1])
            build_slot.atomic_json(report,result)
        if result['validation_inputs']!=repack.packaging_inputs(ROOT):raise ValueError('targeted inputs changed during checks')
        result['state']='passed'
    except Exception as error:
        result.update(state='failed',failure_type=type(error).__name__,failure_detail=str(error)[:500])
    result['finished_at']=datetime.datetime.now(datetime.timezone.utc).isoformat();build_slot.atomic_json(report,result)
    print(json.dumps({k:result.get(k) for k in ('state','passed','failed','failure_detail','binaries_rebuilt')}))
    return 0 if result['state']=='passed' else 1


if __name__=='__main__':sys.exit(main())
