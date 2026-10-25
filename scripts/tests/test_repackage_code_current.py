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
        for name,source_name in repack.IMMUTABLE_PARALLEL.items():
            p=self.current/name;shutil.copyfile(self.root/source_name,p)
            artifacts[name]={'sha256':repack.digest(p),'size':p.stat().st_size}
        proof={'state':'passed','source_inputs_unchanged':True,'source_inputs':self.compiled,
               'functional_tests':{'passed':1708,'failed':0,'evidence_complete':True}}
        (self.current/'source-verification.json').write_text(json.dumps(proof))
        self.manifest={'instance_upgrade_protocol':1,'version':'0.12.0','snapshot_id':'original-binary-fingerprint','source_inputs':self.compiled,
            'source_verification_sha256':repack.digest(self.current/'source-verification.json'),
            'included_platforms':['macos','linux','windows'],'pending_platforms':[],
            'artifacts':artifacts,'tests':proof['functional_tests'],'deployed':False}
        (self.current/'manifest.json').write_text(json.dumps(self.manifest));(self.current/'README.md').write_text('old read-only plan directions')
        (self.current/'SHA256SUMS').write_text('fixture checksum list')
        with tarfile.open(self.bundle,'w:gz') as archive:
            for p in sorted(self.current.iterdir()):archive.add(p,arcname=p.name)
        self.base_hash=repack.digest(self.current/'manifest.json')
        self.bundle_hash=repack.digest(self.bundle)
        self.now=dict(self.compiled,**{'scripts/package-code-release.py':repack.digest(self.root/'scripts/package-code-release.py'),
                                      'scripts/parallel-install.py':repack.digest(self.root/'scripts/parallel-install.py')})
        patch=mock.patch.object(repack,'current_inputs',return_value=self.now);patch.start();self.addCleanup(patch.stop)
        self.validation=self.root/'entrypoint-checks.json'
        checks={}
        for name in ('installer_tests','packaging_tests','release_entry_tests','branding'):
            p=self.root/(name+'.log');p.write_text('Ran 20 tests in 0.001s\n\nOK\n' if name!='branding' else 'branding passed\n')
            checks[name]={'exit_code':0,'log':str(p),'sha256':repack.digest(p)}
            if name!='branding':checks[name].update(tests=20,passed=20,skipped=0)
        self.receipt={'kind':'parallel-packaging-only','state':'passed','base_manifest_sha256':self.base_hash,
            'base_bundle_sha256':self.bundle_hash,'validation_inputs':repack.packaging_inputs(self.root),'failed':0,'passed':60,'skipped':0,'binaries_rebuilt':False,'checks':checks}
        self.validation.write_text(json.dumps(self.receipt))

    def test_binary_source_change_is_not_a_packaging_exception(self):
        changed=dict(self.now);changed['crates/example.rs']='different'
        with self.assertRaisesRegex(ValueError,'non-packaging'):repack.source_delta(self.compiled,changed)

    def test_old_binary_contract_cannot_receive_new_parallel_installer(self):
        self.manifest.pop('instance_upgrade_protocol')
        manifest=self.current/'manifest.json';manifest.write_text(json.dumps(self.manifest))
        with self.assertRaisesRegex(ValueError,'binary rebuild required'):repack.verify_base(self.root,repack.digest(manifest),self.bundle_hash)

    def test_removed_and_unknown_sources_are_rejected(self):
        for changed in ({},dict(self.now,**{'Cargo.toml':'changed'})):
            with self.assertRaisesRegex(ValueError,'non-packaging'):repack.source_delta(self.compiled,changed)

    def test_packaging_changes_preserve_original_binary_proof(self):
        base,delta=repack.verify_base(self.root,self.base_hash,self.bundle_hash)
        self.assertEqual(base['snapshot_id'],'original-binary-fingerprint')
        self.assertIn('scripts/parallel-install.py',delta)
        self.assertEqual(repack.check_validation(self.root,self.validation,self.base_hash,self.bundle_hash)['passed'],60)

    def test_changed_base_manifest_or_binary_refuses_before_admission(self):
        with self.assertRaisesRegex(ValueError,'explicit verified'):repack.verify_base(self.root,'0'*64,self.bundle_hash)
        name=next(iter(repack.BINARY_NAMES));(self.current/name).write_bytes(b'modified')
        with self.assertRaisesRegex(ValueError,'artifact changed'):repack.verify_base(self.root,self.base_hash,self.bundle_hash)

    def test_unknown_package_file_is_never_read_or_copied(self):
        (self.current/'unexpected-private-data').write_bytes(b'fixture-not-a-real-credential')
        with self.assertRaisesRegex(ValueError,'unlisted files'):repack.verify_base(self.root,self.base_hash,self.bundle_hash)

    def test_modified_verification_log_and_stale_entrypoint_receipt_refused(self):
        first=next(iter(self.receipt['checks'].values()));Path(first['log']).write_text('modified')
        with self.assertRaisesRegex(ValueError,'log changed'):repack.check_validation(self.root,self.validation,self.base_hash,self.bundle_hash)
        self.receipt['validation_inputs']={};self.validation.write_text(json.dumps(self.receipt))
        with self.assertRaisesRegex(ValueError,'stale'):repack.check_validation(self.root,self.validation,self.base_hash,self.bundle_hash)

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
        self.assertEqual((stage/'README.md').read_bytes(),before['README.md'])

    def test_existing_stage_never_replaced(self):
        stage,_=repack.release_current.stages(self.root);stage.mkdir();(stage/'keep').write_bytes(b'owned')
        with self.assertRaisesRegex(ValueError,'original fixed stage'):repack.stage(self.root,self.manifest,self.validation,[])
        self.assertEqual((stage/'keep').read_bytes(),b'owned')

    def rewrite_base(self, included, pending):
        self.manifest['included_platforms']=included;self.manifest['pending_platforms']=pending
        required={name for name,platform in repack.BINARY_PLATFORMS.items() if platform in included}
        for name in repack.BINARY_NAMES-required:
            (self.current/name).unlink(missing_ok=True);self.manifest['artifacts'].pop(name,None)
        self.reseal_base()

    def reseal_base(self):
        (self.current/'manifest.json').write_text(json.dumps(self.manifest))
        with tarfile.open(self.bundle,'w:gz') as archive:
            for p in sorted(self.current.iterdir()):archive.add(p,arcname=p.name)
        self.base_hash=repack.digest(self.current/'manifest.json');self.bundle_hash=repack.digest(self.bundle)
        self.receipt['base_manifest_sha256']=self.base_hash;self.receipt['base_bundle_sha256']=self.bundle_hash
        self.validation.write_text(json.dumps(self.receipt))

    def test_two_platform_base_and_all_its_binaries_proof_docs_are_preserved(self):
        self.rewrite_base(['linux','windows'],['macos'])
        base,delta=repack.verify_base(self.root,self.base_hash,self.bundle_hash)
        original={p.name:p.read_bytes() for p in self.current.iterdir()}
        m=repack.stage(self.root,base,self.validation,delta)
        stage,_=repack.release_current.stages(self.root)
        self.assertEqual(m['included_platforms'],['linux','windows']);self.assertEqual(m['pending_platforms'],['macos'])
        self.assertEqual(set(m['artifacts'])&repack.BINARY_NAMES,set(base['artifacts'])&repack.BINARY_NAMES)
        for name in set(base['artifacts'])|{'README.md','source-verification.json'}:
            self.assertEqual((stage/name).read_bytes(),original[name])
        self.assertEqual(m['tests'],base['tests']);self.assertEqual(m['source_inputs'],base['source_inputs'])
        self.assertEqual(m['packaging']['binary_source_bundle_sha256'],self.bundle_hash)
        self.assertFalse((stage/'octendra-code-macos-arm64').exists())

    def test_single_selected_platform_is_supported_with_exact_complement(self):
        self.rewrite_base(['linux'],['macos','windows'])
        base,_=repack.verify_base(self.root,self.base_hash,self.bundle_hash)
        self.assertEqual(repack.base_platforms(base),('linux',))

    def test_unknown_missing_empty_duplicate_overlapping_or_unpartitioned_platforms_refuse(self):
        baseline=copy.deepcopy(self.manifest)
        cases=[([],['macos','linux','windows']),(['linux','linux','windows'],['macos']),
               (['linux','windows'],['macos','macos']),(['linux','freebsd'],['macos','windows']),
               (['linux','windows'],[]),(['linux','windows'],['macos','linux']),
               (None,['macos']),(['linux','windows'],None),('linux',['macos','windows']),
               (['linux',{}],['macos','windows'])]
        for included,pending in cases:
            with self.subTest(included=included,pending=pending):
                m=copy.deepcopy(baseline);m['included_platforms']=included;m['pending_platforms']=pending
                with self.assertRaisesRegex(ValueError,'platform partition'):repack.base_platforms(m)
        for field in ('included_platforms','pending_platforms'):
            m=copy.deepcopy(baseline);m.pop(field)
            with self.assertRaisesRegex(ValueError,'platform partition'):repack.base_platforms(m)

    def test_missing_one_binary_or_unselected_platform_binary_refuses(self):
        m=copy.deepcopy(self.manifest);m['artifacts'].pop('octendra-code-linux-amd64')
        with self.assertRaisesRegex(ValueError,'binary set'):repack.base_platforms(m)
        m=copy.deepcopy(self.manifest);m.update(included_platforms=['linux','windows'],pending_platforms=['macos'])
        with self.assertRaisesRegex(ValueError,'binary set'):repack.base_platforms(m)

    def test_wrong_or_missing_bundle_digest_refuses_before_archive_use(self):
        for expected in ('0'*64,None,'bad'):
            with self.assertRaisesRegex(ValueError,'base bundle differs'):
                repack.verify_base(self.root,self.base_hash,expected)

    def test_modified_bundle_refuses_with_original_pinned_digest(self):
        with self.bundle.open('ab') as stream:stream.write(b'changed')
        with self.assertRaisesRegex(ValueError,'base bundle differs'):
            repack.verify_base(self.root,self.base_hash,self.bundle_hash)

    def test_validation_cannot_be_reused_for_a_different_base_bundle(self):
        with self.assertRaisesRegex(ValueError,'stale'):
            repack.check_validation(self.root,self.validation,self.base_hash,'0'*64)

    def test_parallel_document_or_template_change_cannot_be_a_script_update(self):
        for name in repack.DOCS:
            path=self.root/name;before=path.read_bytes();path.write_bytes(before+b'changed')
            with self.assertRaisesRegex(ValueError,'non-installer parallel artifact'):
                repack.verify_base(self.root,self.base_hash,self.bundle_hash)
            path.write_bytes(before)

    def test_original_reported_tests_must_match_immutable_compiled_receipt(self):
        self.manifest['tests']={'passed':999999,'failed':0};self.reseal_base()
        with self.assertRaisesRegex(ValueError,'verification is incomplete'):
            repack.verify_base(self.root,self.base_hash,self.bundle_hash)

    def test_duplicate_archive_member_refuses_even_when_bundle_hash_was_supplied(self):
        with tarfile.open(self.bundle,'w:gz') as archive:
            for p in sorted(self.current.iterdir()):archive.add(p,arcname=p.name)
            archive.add(self.current/'README.md',arcname='README.md')
        with self.assertRaisesRegex(ValueError,'archive membership'):
            repack.verify_base(self.root,self.base_hash,repack.digest(self.bundle))

    def test_unittest_skip_counts_are_separate_from_actual_passes(self):
        self.assertEqual(repack.unittest_counts('Ran 100 tests in 2.3s\n\nOK (skipped=1)\n'),
                         {'tests':100,'passed':99,'skipped':1})
        self.assertEqual(repack.unittest_counts('Ran 20 tests in 0.2s\n\nOK\n'),
                         {'tests':20,'passed':20,'skipped':0})
        for text in ('Ran 0 tests in 0.1s\nOK\n','Ran 2 tests in 0.1s\nOK (skipped=2)\n',
                     'Ran 1 test in 0.1s\nFAILED (failures=1)\n','Ran 1 test in 0.1s\nOK (expected failures=1)\n',
                     'Ran 1 test in 0.1s\nOK\nRan 1 test in 0.1s\nOK\n'):
            with self.assertRaises(ValueError):repack.unittest_counts(text)

    def test_validation_pass_skip_and_aggregate_counts_must_match_logs(self):
        self.receipt['passed']=61;self.validation.write_text(json.dumps(self.receipt))
        with self.assertRaisesRegex(ValueError,'aggregate counts'):
            repack.check_validation(self.root,self.validation,self.base_hash,self.bundle_hash)
        self.receipt['passed']=60;self.receipt['checks']['installer_tests']['passed']=19
        self.validation.write_text(json.dumps(self.receipt))
        with self.assertRaisesRegex(ValueError,'test counts differ'):
            repack.check_validation(self.root,self.validation,self.base_hash,self.bundle_hash)

    def test_stage_refuses_a_different_base_bundle_without_touching_current(self):
        before={p.name:p.read_bytes() for p in self.current.iterdir()}
        self.receipt['base_bundle_sha256']='0'*64;self.validation.write_text(json.dumps(self.receipt))
        with self.assertRaisesRegex(ValueError,'pinned base changed'):
            repack.stage(self.root,self.manifest,self.validation,[])
        self.assertEqual(before,{p.name:p.read_bytes() for p in self.current.iterdir()})

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
            r=repack.run(self.root,self.root/'operation.json',self.validation,self.base_hash,self.bundle_hash)
        self.assertEqual(r['state'],'failed');self.assertEqual(r['binary_builds_started'],0)


if __name__=='__main__':unittest.main()
