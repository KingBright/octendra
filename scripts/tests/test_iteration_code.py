"""Pure policy tests for the iteration driver. No Git push or deployment."""
import importlib.util
import pathlib
import sys
import unittest

SCRIPTS = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SCRIPT = SCRIPTS/'iteration-code.py'
spec = importlib.util.spec_from_file_location('iteration_code', SCRIPT)
iteration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(iteration)


class IterationPolicyTests(unittest.TestCase):
    def test_docs_only_uses_lightest_gate(self):
        self.assertEqual(iteration.classify(['README.md','docs/product/NEXT.md']), 'docs')

    def test_skill_only_uses_lightest_gate(self):
        self.assertEqual(iteration.classify([
            'skills/octendra-agent/SKILL.md',
            'skills/octendra-agent/references/setup-and-runtime.md',
        ]), 'skill')

    def test_docs_plus_skill_uses_skill_gate(self):
        self.assertEqual(iteration.classify(['docs/product/NEXT.md','skills/octendra-agent/SKILL.md']), 'skill')

    def test_release_python_does_not_force_rust_rebuild(self):
        self.assertEqual(iteration.classify(['scripts/check-code-gateway.py',
                                             'scripts/tests/test_acceptance_scope_032.py',
                                             'docs/releases/0.7.1/RELEASE.md',
                                             'skills/octendra-agent/SKILL.md']), 'release_python')

    def test_runtime_change_requires_full_release_pipeline(self):
        for paths in (['crates/octendra-code/src/agent.rs'], ['Cargo.lock'],
                      ['migrations/0001_initial.sql'], ['scripts/a.py','crates/x/src/lib.rs']):
            self.assertEqual(iteration.classify(paths), 'runtime', paths)

    def test_empty_tree_is_not_an_iteration(self):
        self.assertEqual(iteration.classify([]), 'clean')

    def test_fingerprint_is_stable_for_same_files(self):
        import tempfile
        old_root=iteration.ROOT
        try:
            with tempfile.TemporaryDirectory() as tmp:
                iteration.ROOT=pathlib.Path(tmp)
                (iteration.ROOT/'a').write_text('one')
                first=iteration.fingerprint(['a','missing'])
                self.assertEqual(first, iteration.fingerprint(['a','missing']))
                (iteration.ROOT/'a').write_text('two')
                self.assertNotEqual(first, iteration.fingerprint(['a','missing']))
        finally:
            iteration.ROOT=old_root

    def test_validated_is_distinct_from_fully_passed_policy(self):
        self.assertNotEqual('validated', 'passed')

    def test_commit_audit_rejects_release_blobs_and_bearer_urls(self):
        import tempfile
        old_root=iteration.ROOT
        try:
            with tempfile.TemporaryDirectory() as tmp:
                iteration.ROOT=pathlib.Path(tmp)
                (iteration.ROOT/'artifact.tgz').write_bytes(b'x')
                with self.assertRaisesRegex(ValueError, 'artifact'):
                    iteration.audit_commit_scope(['artifact.tgz'])
                (iteration.ROOT/'Cargo.lock').write_text('version = 4')
                iteration.audit_commit_scope(['Cargo.lock'])
                (iteration.ROOT/'publish.lock').write_text('temporary')
                with self.assertRaisesRegex(ValueError, 'artifact'):
                    iteration.audit_commit_scope(['publish.lock'])
                (iteration.ROOT/'evidence.json').write_text(
                    '{"download_url":"https://example.test/files/'+'a'*64+'/x"}')
                with self.assertRaisesRegex(ValueError, 'bearer'):
                    iteration.audit_commit_scope(['evidence.json'])
                (iteration.ROOT/'safe.md').write_text('no private capability here')
                iteration.audit_commit_scope(['safe.md'])
        finally:
            iteration.ROOT=old_root

    def test_publish_status_is_compact_and_does_not_require_secrets(self):
        import json, tempfile
        with tempfile.TemporaryDirectory() as tmp:
            root=pathlib.Path(tmp)
            (root/'deployment.json').write_text(json.dumps({
                'version':'1.2.3','state':'partial','phase':'finished','all_targets_accepted':False,
                'gateway':{'state':'upgraded','version':'1.2.3','private':'discard'},
                'agents':{'d':{'name':'Mac','state':'needs_recovery','phase':'acceptance','secret':'discard'}}}))
            value=iteration.compact_publish_status(root)
            self.assertEqual(value['gateway'], {'state':'upgraded','version':'1.2.3'})
            self.assertEqual(value['agents']['d'], {'name':'Mac','state':'needs_recovery','phase':'acceptance'})
            self.assertNotIn('private', json.dumps(value))
            self.assertNotIn('secret', json.dumps(value))


class CallerFlowTests(unittest.TestCase):
    """Existing coordinator/case-selection flows with tiny inputs and mocked tools."""
    def setUp(self):
        import contextlib, hashlib, tempfile
        from types import SimpleNamespace
        from unittest import mock
        self.mock=mock
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.root=pathlib.Path(self.tmp.name).resolve()
        (self.root/'Cargo.toml').write_text('[workspace]\n')
        self.stack=contextlib.ExitStack();self.addCleanup(self.stack.close)
        lifecycle=iteration.release_current
        self.stack.enter_context(mock.patch.object(lifecycle,'primary',side_effect=lambda root:root))
        provider=SimpleNamespace(inputs=lambda root:{'Cargo.toml':hashlib.sha256((root/'Cargo.toml').read_bytes()).hexdigest()})
        self.stack.enter_context(mock.patch.object(lifecycle.source_snapshot,'verifier',return_value=provider))
        self.stack.enter_context(mock.patch.object(lifecycle,'established_target',return_value=pathlib.Path.home()/'rust-target'))
        self.stack.enter_context(mock.patch.object(iteration,'ROOT',self.root))

    def runner(self,filename):
        spec=importlib.util.spec_from_file_location('fixture_'+filename.replace('-','_'),SCRIPTS/filename)
        value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value)
        self.stack.enter_context(self.mock.patch.object(value,'ROOT',self.root))
        return value

    def runtime_iteration(self, *, mutate=False):
        import contextlib, io, json
        report=self.root/'evidence/iteration.json'
        pipeline=report.parent/'pipeline.json'
        def run(argv,**_):
            self.assertEqual(argv,[sys.executable,'scripts/release-code.py','--report',str(pipeline)])
            pipeline.write_text(json.dumps({'state':'passed','version':'1.2.3','package':{
                'manifest_sha256':'manifest','path':'current','bundle':'current.tgz','bundle_sha256':'bundle'}}))
            if mutate:(self.root/'Cargo.toml').write_text('changed while verifying')
        with self.mock.patch.object(sys,'argv',['iteration-code.py','--report',str(report),'--no-push']), \
             self.mock.patch.object(iteration,'changed_paths',return_value=['Cargo.toml']), \
             self.mock.patch.object(iteration,'run',side_effect=run),contextlib.redirect_stdout(io.StringIO()):
            if mutate:
                with self.assertRaises(SystemExit):iteration.main()
            else:iteration.main()
        return json.loads(report.read_text())

    def test_runtime_iteration_records_original_package_result(self):
        result=self.runtime_iteration()
        self.assertEqual(result['state'],'validated')
        self.assertEqual(result['version'],'1.2.3')
        self.assertEqual(result['bundle_sha256'],'bundle')
        self.assertEqual(result['stages']['verification']['state'],'passed')

    def test_runtime_iteration_retains_changed_inputs_and_failed_receipt(self):
        result=self.runtime_iteration(mutate=True)
        self.assertEqual(result['state'],'failed')
        self.assertEqual((self.root/'Cargo.toml').read_text(),'changed while verifying')
        self.assertNotEqual(result['stages'].get('verification',{}).get('state'),'passed')

    def test_task_authorization_keeps_all_selected_checks(self):
        import contextlib, io, json
        from types import SimpleNamespace
        runner=self.runner('check-task-authorization.py')
        report=self.root/'evidence/report.json';calls=[]
        def stage(argv,checkout,target,path,name,state,limit):
            self.assertEqual(checkout,self.root)
            self.assertEqual(target,pathlib.Path.home()/'rust-target')
            calls.append((name,argv))
            state['stages'][name]={'exit_code':0,'source_unchanged':True}
        with self.mock.patch.object(sys,'argv',['check-task-authorization.py','--evidence-dir',str(report.parent),
                                               '--extended','--export-contract','--acceptance']), \
             self.mock.patch.object(runner.shutil,'disk_usage',return_value=SimpleNamespace(free=20*1024**3)), \
             self.mock.patch.object(runner,'stage',side_effect=stage),contextlib.redirect_stdout(io.StringIO()):
            runner.main()
        self.assertEqual([name for name,_ in calls],['format','task_authorization','queue_dispatch','receipts',
            'diagnostics','contract_export','upgrade_policy_guard','release_manifest','catalog_contract','experience_contract'])
        self.assertEqual(calls[1][1],['cargo','test','--offline','--locked','-p','octendra-code','--lib',
                                    'task_authorization','--','--nocapture'])
        self.assertEqual(json.loads(report.read_text())['state'],'passed')

    def test_transfer_recovery_keeps_exact_cases_and_receipt_counts(self):
        import contextlib, io, json
        runner=self.runner('check-transfer-recovery.py')
        report=self.root/'evidence/verification.json';calls=[]
        def stage(name,argv,checkout,env,directory,state,timeout=1200):
            self.assertEqual(checkout,self.root)
            self.assertEqual(env['CARGO_TARGET_DIR'],str(pathlib.Path.home()/'rust-target'))
            calls.append((name,argv))
            state['stages'][name]={'state':'passed','passed':2 if name=='python_coordinator' else 1}
        with self.mock.patch.object(sys,'argv',['check-transfer-recovery.py','--report-dir',str(report.parent)]), \
             self.mock.patch.object(runner,'run_stage',side_effect=stage),contextlib.redirect_stdout(io.StringIO()):
            runner.main()
        self.assertEqual(len(calls),len(runner.CASES)+2)
        for call,case in zip(calls[2:],runner.CASES):
            self.assertEqual(call[1][-4:],[case[1],'--','--exact','--nocapture'])
        result=json.loads(report.read_text())
        self.assertEqual((result['state'],result['python_passed'],result['rust_passed']),('passed',2,len(runner.CASES)))

    def test_acceptance_stage_keeps_input_drift_failure(self):
        import contextlib, io
        runner=self.runner('check-transfer-recovery.py')
        state={'stages':{},'source_evidence':runner.release_current.snapshot_evidence(None,self.root)}
        process=self.mock.Mock(pid=12345)
        def wait(**_):
            (self.root/'Cargo.toml').write_text('concurrent edit')
            return 0
        process.wait.side_effect=wait
        with self.mock.patch.object(runner.subprocess,'Popen',return_value=process), \
             contextlib.redirect_stdout(io.StringIO()),self.assertRaises(ValueError):
            runner.run_stage('format',['cargo','fmt'],self.root,{},self.root,state)
        self.assertNotEqual(state['stages']['format']['state'],'passed')
        self.assertEqual((self.root/'Cargo.toml').read_text(),'concurrent edit')


if __name__ == '__main__':
    unittest.main()
