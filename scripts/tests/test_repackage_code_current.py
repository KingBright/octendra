"""Packaging-only provenance and immutable-binary checks, no Cargo/real authority."""
import copy
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

SCRIPTS=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(SCRIPTS))
spec=importlib.util.spec_from_file_location('repackage_fixture',SCRIPTS/'repackage-code-current.py')
repack=importlib.util.module_from_spec(spec);spec.loader.exec_module(repack)


class RepackageTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.root=Path(self.tmp.name).resolve();(self.root/'scripts').mkdir();(self.root/'crates').mkdir()
        (self.root/'crates/example.rs').write_text('compiled source unchanged\n')
        self.compiled={'crates/example.rs':repack.digest(self.root/'crates/example.rs'),
                       'scripts/package-code-release.py':'old-packaging-source-hash'}
        shutil.copyfile(SCRIPTS/'package-code-release.py',self.root/'scripts/package-code-release.py')
        (self.root/'scripts/parallel-install.py').write_text('# synthetic installer fixture\n')
        d=self.root/'docs/parallel-octendra';d.mkdir(parents=True)
        (d/'README.md').write_text('fresh parallel directions\n');(d/'gateway.json.in').write_text('{}\n')
        (d/'new-host.caddy.in').write_text('# proxy fixture\n')
        self.current,self.bundle=repack.release_current.files(self.root)
        self.current.mkdir(parents=True)
        artifacts={}
        for name in sorted(repack.BINARY_NAMES):
            p=self.current/name;p.write_bytes(('verified-'+name).encode());p.chmod(0o755)
            artifacts[name]={'sha256':repack.digest(p),'size':p.stat().st_size}
        proof={'state':'passed','source_inputs_unchanged':True,'source_inputs':self.compiled,
               'functional_tests':{'passed':1708,'failed':0,'evidence_complete':True}}
        (self.current/'source-verification.json').write_text(json.dumps(proof))
        self.manifest={'instance_upgrade_protocol':1,'version':'0.12.0','snapshot_id':'original-binary-fingerprint','source_inputs':self.compiled,
            'source_verification_sha256':repack.digest(self.current/'source-verification.json'),
            'included_platforms':['macos','linux','windows'],'pending_platforms':[],
            'artifacts':artifacts,'tests':proof['functional_tests'],'deployed':False}
        (self.current/'manifest.json').write_text(json.dumps(self.manifest));(self.current/'README.md').write_text('old directions')
        (self.current/'SHA256SUMS').write_text('fixture checksum list')
        with tarfile.open(self.bundle,'w:gz') as archive:
            for p in sorted(self.current.iterdir()):archive.add(p,arcname=p.name)
        self.base_hash=repack.digest(self.current/'manifest.json')
        self.now=dict(self.compiled,**{'scripts/package-code-release.py':repack.digest(self.root/'scripts/package-code-release.py'),
                                      'scripts/parallel-install.py':repack.digest(self.root/'scripts/parallel-install.py')})
        patch=mock.patch.object(repack,'current_inputs',return_value=self.now);patch.start();self.addCleanup(patch.stop)
        self.validation=self.root/'entrypoint-checks.json'
        checks={}
        for name in ('installer_tests','packaging_tests','release_entry_tests','branding'):
            p=self.root/(name+'.log');p.write_text('fixture actual log\n')
            checks[name]={'exit_code':0,'log':str(p),'sha256':repack.digest(p)}
        self.receipt={'kind':'parallel-packaging-only','state':'passed','base_manifest_sha256':self.base_hash,
            'validation_inputs':repack.packaging_inputs(self.root),'failed':0,'passed':60,'checks':checks}
        self.validation.write_text(json.dumps(self.receipt))

    def test_binary_source_change_is_not_a_packaging_exception(self):
        changed=dict(self.now);changed['crates/example.rs']='different'
        with self.assertRaisesRegex(ValueError,'non-packaging'):repack.source_delta(self.compiled,changed)

    def test_old_binary_contract_cannot_receive_new_parallel_installer(self):
        self.manifest.pop('instance_upgrade_protocol')
        manifest=self.current/'manifest.json';manifest.write_text(json.dumps(self.manifest))
        with self.assertRaisesRegex(ValueError,'binary rebuild required'):repack.verify_base(self.root,repack.digest(manifest))

    def test_removed_and_unknown_sources_are_rejected(self):
        for changed in ({},dict(self.now,**{'Cargo.toml':'changed'})):
            with self.assertRaisesRegex(ValueError,'non-packaging'):repack.source_delta(self.compiled,changed)

    def test_packaging_changes_preserve_original_binary_proof(self):
        base,delta=repack.verify_base(self.root,self.base_hash)
        self.assertEqual(base['snapshot_id'],'original-binary-fingerprint')
        self.assertIn('scripts/parallel-install.py',delta)
        self.assertEqual(repack.check_validation(self.root,self.validation,self.base_hash)['passed'],60)

    def test_changed_base_manifest_or_binary_refuses_before_admission(self):
        with self.assertRaisesRegex(ValueError,'explicit verified'):repack.verify_base(self.root,'0'*64)
        name=next(iter(repack.BINARY_NAMES));(self.current/name).write_bytes(b'modified')
        with self.assertRaisesRegex(ValueError,'artifact changed'):repack.verify_base(self.root,self.base_hash)

    def test_unknown_package_file_is_never_read_or_copied(self):
        (self.current/'unexpected-private-data').write_bytes(b'fixture-not-a-real-credential')
        with self.assertRaisesRegex(ValueError,'unlisted files'):repack.verify_base(self.root,self.base_hash)

    def test_modified_verification_log_and_stale_entrypoint_receipt_refused(self):
        first=next(iter(self.receipt['checks'].values()));Path(first['log']).write_text('modified')
        with self.assertRaisesRegex(ValueError,'log changed'):repack.check_validation(self.root,self.validation,self.base_hash)
        self.receipt['validation_inputs']={};self.validation.write_text(json.dumps(self.receipt))
        with self.assertRaisesRegex(ValueError,'stale'):repack.check_validation(self.root,self.validation,self.base_hash)

    def test_staging_keeps_all_six_binaries_byte_identical_and_executable(self):
        before={p.name:p.read_bytes() for p in self.current.iterdir()}
        m=repack.stage(self.root,self.manifest,self.validation,['scripts/parallel-install.py'])
        stage,bundle=repack.release_current.stages(self.root)
        for name in repack.BINARY_NAMES:
            self.assertEqual(repack.digest(stage/name),self.manifest['artifacts'][name]['sha256'])
            self.assertTrue((stage/name).stat().st_mode & 0o111)
        self.assertEqual(before,{p.name:p.read_bytes() for p in self.current.iterdir()})
        self.assertEqual(m['tests']['passed'],1708)
        self.assertEqual(m['packaging']['tests']['passed'],60)
        self.assertEqual(m['snapshot_id'],'original-binary-fingerprint')
        with tarfile.open(bundle) as a:self.assertIn('parallel-install.py',a.getnames())
        self.assertIn('read-only plan',(stage/'README.md').read_text())

    def test_existing_stage_never_replaced(self):
        stage,_=repack.release_current.stages(self.root);stage.mkdir();(stage/'keep').write_bytes(b'owned')
        with self.assertRaisesRegex(ValueError,'original fixed stage'):repack.stage(self.root,self.manifest,self.validation,[])
        self.assertEqual((stage/'keep').read_bytes(),b'owned')

    def test_failed_package_aborts_while_original_producer_lease_is_held(self):
        active=[]
        @contextlib.contextmanager
        def lease(*args,**kwargs):
            active.append(True)
            try:yield self.root
            finally:active.pop()
        def abort(root):
            self.assertTrue(active);return {'status':'completed'}
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(repack.release_current,'primary',return_value=self.root))
            stack.enter_context(mock.patch.object(repack.release_current,'invoke',return_value={}))
            stack.enter_context(mock.patch.object(repack.release_current,'admit',return_value={}))
            stack.enter_context(mock.patch.object(repack.release_current,'abort',side_effect=abort))
            stack.enter_context(mock.patch.object(repack.build_slot,'lease',side_effect=lease))
            stack.enter_context(mock.patch.object(repack.subprocess,'check_output',return_value='fixture-commit\n'))
            stack.enter_context(mock.patch.object(repack,'stage',side_effect=ValueError('fixture failure')))
            r=repack.run(self.root,self.root/'operation.json',self.validation,self.base_hash)
        self.assertEqual(r['state'],'failed');self.assertEqual(r['binary_builds_started'],0)


if __name__=='__main__':unittest.main()
