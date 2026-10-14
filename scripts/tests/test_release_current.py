"""Fixed-current entry fixtures: temporary files/mocks, no build or authority access."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import release_current as lifecycle
import source_snapshot
import build_slot


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS/filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


runner = module('current_runner_fixture', 'release-code.py')
package = module('current_package_fixture', 'package-code-release.py')


class CurrentTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name).resolve()
        (self.root/'scripts').mkdir()
        (self.root/'.git').mkdir()
        (self.root/'crates/remote-hosts-code').mkdir(parents=True)
        (self.root/'crates/remote-hosts-code/Cargo.toml').write_text('[package]\nversion="1.2.3"\n')
        (self.root/'Cargo.toml').write_text('[workspace]\n')
        shutil.copyfile(SCRIPTS/'check-code-source.py', self.root/'scripts/check-code-source.py')
        self.target = self.root/'shared-target'
        self.target.mkdir()
        self.patch = mock.patch.object(lifecycle, 'primary', side_effect=lambda path: path)
        self.real_primary = lifecycle.primary
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.addCleanup(self.tmp.cleanup)
        self.manifest = lifecycle.snapshot_evidence(None, self.root)

    def test_main_fingerprint_does_not_create_snapshot_copy(self):
        self.assertEqual(self.manifest['source_inputs'], source_snapshot.verifier(self.root).inputs(self.root))
        self.assertFalse((self.root/'source-snapshot.json').exists())
        self.assertNotIn('checkout', [p.name for p in self.root.rglob('*')])

    def test_detached_worker_is_bound_to_launch_inputs(self):
        report = self.root/'detached.json'
        with mock.patch.object(runner, 'ROOT', self.root), mock.patch.object(os, 'posix_spawn', return_value=12345) as spawn, contextlib.redirect_stdout(io.StringIO()):
            runner.start_pipeline(None, report)
        args = spawn.call_args.args[1]
        self.assertNotIn('--snapshot', args)
        self.assertEqual(args[args.index('--expected-source-id')+1], self.manifest['snapshot_id'])
        (self.root/'Cargo.toml').write_text('changed after submission')
        with mock.patch.object(runner, 'ROOT', self.root), mock.patch.object(lifecycle, 'admit') as admit:
            with self.assertRaisesRegex(ValueError, 'changed after launch'):
                runner.run_pipeline(None, report, self.root/'target/release-slot', expected_source_id=self.manifest['snapshot_id'])
        admit.assert_not_called()

    def test_source_mutation_is_rejected_without_restoring_edits(self):
        (self.root/'Cargo.toml').write_text('user change')
        with self.assertRaisesRegex(ValueError, 'main source differs'):
            lifecycle.check_inputs(self.root, self.manifest)
        self.assertEqual((self.root/'Cargo.toml').read_text(), 'user change')

    def test_snapshot_check_never_imports_snapshot_code(self):
        snapshot = self.root/'old-evidence'; snapshot.mkdir()
        # Synthetic old bytes/receipt; the retired creation API is never used.
        (snapshot/'Cargo.toml').write_text('[workspace]\n')
        import hashlib
        from types import SimpleNamespace
        provider = SimpleNamespace(inputs=lambda root: {
            'Cargo.toml': hashlib.sha256((root/'Cargo.toml').read_bytes()).hexdigest()})
        files=provider.inputs(snapshot); executable=[]
        manifest=dict(schema_version=1,state='captured',source_inputs=files,
                      executable_files=executable,snapshot_id=source_snapshot.identity(files,executable))
        (snapshot/source_snapshot.MANIFEST).write_text(json.dumps(manifest))
        with mock.patch.object(source_snapshot, 'verifier', return_value=provider) as verifier:
            lifecycle.snapshot_evidence(snapshot, self.root)
        self.assertEqual({call.args[0] for call in verifier.call_args_list}, {self.root})

    def test_primary_rejects_linked_worktree_before_git(self):
        (self.root/'.git').rmdir()
        (self.root/'.git').write_text('gitdir: /elsewhere')
        with mock.patch.object(subprocess, 'check_output') as git:
            with self.assertRaisesRegex(ValueError, 'primary'):
                self.real_primary(self.root)
        git.assert_not_called()

    def test_primary_rejects_branch_and_accepts_main(self):
        with mock.patch.object(subprocess, 'check_output', side_effect=['feature\n', str(self.root)]):
            with self.assertRaisesRegex(ValueError, 'main'):
                self.real_primary(self.root)
        with mock.patch.object(subprocess, 'check_output', side_effect=['main\n', str(self.root)]):
            self.assertEqual(self.real_primary(self.root), self.root)

    def test_target_environment_is_observed_without_rewriting(self):
        home = self.root/'home'
        (home/'rust-target').mkdir(parents=True)
        env = {'CARGO_TARGET_DIR': str(home/'rust-target'), 'CUSTOM': 'preserve'}
        with mock.patch.object(Path, 'home', return_value=home), mock.patch.object(subprocess, 'check_output', return_value=json.dumps({'target_directory': str(home/'rust-target')})) as metadata:
            self.assertEqual(lifecycle.established_target(self.root, env), home/'rust-target')
        self.assertIs(metadata.call_args.kwargs['env'], env)
        self.assertEqual(env['CUSTOM'], 'preserve')
        self.assertIn('--offline', metadata.call_args.args[0])

    def test_alternate_target_refused(self):
        with mock.patch.object(Path, 'home', return_value=self.root), mock.patch.object(subprocess, 'check_output', return_value=json.dumps({'target_directory': str(self.target)})):
            (self.root/'rust-target').mkdir()
            with self.assertRaisesRegex(ValueError, 'established'):
                lifecycle.established_target(self.root)

    def test_admission_fixed_files_and_long_lived_pid(self):
        response = {'admission': 'reserved', 'staged_files': list(map(str, lifecycle.stages(self.root)))}
        with mock.patch.object(lifecycle, 'invoke', return_value=response) as invoke:
            lifecycle.admit(self.root)
        self.assertEqual(invoke.call_args.args[:2], (self.root, 'release-admit'))
        args = invoke.call_args.args[2:]
        self.assertEqual(args[args.index('--producer-pid')+1], str(os.getpid()))
        self.assertNotIn('--generation', args)
        self.assertEqual(args.count('--file'), 2)
        self.assertTrue((self.root/'dist').is_dir())
        self.assertFalse(any(path.exists() for path in lifecycle.stages(self.root)))

    def test_admission_rejects_unexpected_stage(self):
        with mock.patch.object(lifecycle, 'invoke', return_value={'admission': 'reserved', 'staged_files': ['/other']}):
            with self.assertRaisesRegex(ValueError, 'fixed current'):
                lifecycle.admit(self.root)

    def test_shared_stage_signatures_receipt_and_commit_required(self):
        stage, bundle = lifecycle.stages(self.root)
        stage.mkdir(parents=True); (stage/'manifest.json').write_text('{}')
        bundle.write_bytes(b'tgz')
        helper = mock.Mock(); helper.release_signature.return_value = 'a'*64
        with mock.patch.object(lifecycle, 'public_api', return_value=helper), mock.patch.object(lifecycle, 'invoke', return_value={'current_committed': True}) as invoke:
            lifecycle.publish(self.root)
        args = invoke.call_args.args
        self.assertEqual(args.count('--expected-sha256'), 2)
        self.assertEqual(args[args.index('--validation-file')+1], str(stage/'manifest.json'))
        self.assertEqual(args[args.index('--validation-kind')+1], 'unsigned-dev')

    def test_metadata_lease_executes_main_without_checkout(self):
        slot = self.root/'target/release-slot'
        with build_slot.lease(slot, self.root/'report.json', 'fingerprint', execution_root=self.root) as execution:
            self.assertEqual(execution, self.root)
        self.assertFalse((slot/'checkout').exists())
        self.assertFalse((slot/'cargo-target').exists())

    def pipeline(self, verify_only=False, failed=False, unsettled=False):
        def stage(name, argv, checkout, directory, report, state, env, **kwargs):
            self.assertEqual(checkout, self.root)
            self.assertNotIn('CARGO_TARGET_DIR', env)
            self.assertEqual(kwargs['manifest'], self.manifest)
            if failed:
                state['stages'][name] = {'cleanup': {'state': 'unconfirmed' if unsettled else 'complete'}}
                raise RuntimeError('fixture failed')
            proof = {'functional_tests': {'failed': 0}, 'source_inputs': self.manifest['source_inputs']}
            (directory/'verification.json').write_text(json.dumps(proof))
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(runner, 'ROOT', self.root))
            stack.enter_context(mock.patch.dict(os.environ, {}, clear=True))
            stack.enter_context(mock.patch.object(lifecycle, 'preflight', return_value={}))
            admit = stack.enter_context(mock.patch.object(lifecycle, 'admit', return_value={}))
            abort = stack.enter_context(mock.patch.object(lifecycle, 'abort', return_value={}))
            stack.enter_context(mock.patch.object(lifecycle, 'established_target', return_value=self.target))
            stack.enter_context(mock.patch.object(runner.subprocess, 'check_output', return_value='rust fixture'))
            verifier = source_snapshot.verifier(self.root)
            verifier.receipt_current = lambda proof, root: True
            stack.enter_context(mock.patch.object(source_snapshot, 'verifier', return_value=verifier))
            stack.enter_context(mock.patch.object(runner, 'run_stage', side_effect=stage))
            with contextlib.redirect_stdout(io.StringIO()):
                result = runner.run_pipeline(None, self.root/'report.json', self.root/'target/release-slot', verify_only)
        return result, admit, abort

    def test_verify_only_no_admission_or_complete_package(self):
        result, admit, abort = self.pipeline(verify_only=True)
        self.assertEqual(result['state'], 'passed')
        admit.assert_not_called(); abort.assert_not_called()
        self.assertFalse((self.root/'dist').exists())

    def test_failed_stage_recovers_owned_temp(self):
        result, admit, abort = self.pipeline(failed=True)
        self.assertEqual(result['state'], 'failed')
        admit.assert_called_once(); abort.assert_called_once_with(self.root)

    def test_unsettled_child_defers_abort(self):
        result, _, abort = self.pipeline(failed=True, unsettled=True)
        self.assertEqual(result['current_recovery']['status'], 'deferred')
        abort.assert_not_called()

    def test_unconfirmed_lease_preserves_remaining_group_guard(self):
        report = self.root/'report.json'
        slot = self.root/'target/release-slot'
        with build_slot.lease(slot, report, 'source', execution_root=self.root):
            report.write_text(json.dumps({'stages': {'build': {'cleanup': {'state': 'unconfirmed', 'remaining_groups': [123456789]}}}}))
        self.assertEqual(json.loads((slot/'owner.json').read_text())['state'], 'recovery_required')
        with mock.patch.object(build_slot, 'process_alive', side_effect=lambda pid, **kwargs: pid == 123456789):
            with self.assertRaisesRegex(ValueError, 'recovery_required'):
                with build_slot.lease(slot, self.root/'new.json', 'new', execution_root=self.root):
                    self.fail('reused while original descendant group survived')

    def package_fixture(self):
        helpers = ['upgrade-code-agent.py', 'agent_upgrade_support.py', 'launch-code-upgrade.py',
            'code_upgrade_runner.py', 'macos_code_identity.py', 'upgrade-code-gateway.py',
            'check-code-gateway.py', 'check-terminal-delivery.py', 'release_receipts.py',
            'maintenance_client.py', 'release_targets.py', 'sync_bundle.py', 'release_client.py',
            'native_release_client.py', 'publish-code.py', 'check-collaboration.py',
            'install-code-agent.py', 'upgrade-code-agent-linux.py', 'upgrade-code-agent-windows.ps1',
            'launch-code-upgrade-windows.ps1', 'fleet-upgrade.py', 'gateway_self_upgrade_runner.py']
        for name in helpers: (self.root/'scripts'/name).write_text('# fixture helper\n')
        binaries = ['release/remote-hosts-code', 'x86_64-unknown-linux-musl/release/remote-hosts-code',
                    'x86_64-pc-windows-msvc/release/remote-hosts-code.exe']
        for name in binaries:
            path = self.target/name; path.parent.mkdir(parents=True, exist_ok=True); path.write_bytes(b'MZ fixture')
        verifier = source_snapshot.verifier(self.root)
        proof = {'version': '1.2.3', 'state': 'passed', 'source_inputs_unchanged': True,
                 'source_inputs': verifier.inputs(self.root), 'functional_tests': {'failed': 0},
                 'checks': {name: {'exit_code': 0} for name in ('fmt', 'clippy', 'rust_tests', 'python_tests', 'workspace')}}
        proof_path = self.root/'proof.json'; proof_path.write_text(json.dumps(proof))
        (self.root/'dist').mkdir(exist_ok=True)
        current, current_bundle = lifecycle.files(self.root)
        current.mkdir(); (current/'old').write_bytes(b'old current'); current_bundle.write_bytes(b'old bundle')
        return proof_path, current, current_bundle

    def invoke_package(self, proof, mutate=False):
        original_loader = package.importlib.util.module_from_spec
        def loader(spec):
            module = original_loader(spec)
            original_exec = spec.loader.exec_module
            def execute(value):
                original_exec(value)
                value.receipt_current = lambda evidence, root: True
            spec.loader.exec_module = execute
            return module
        original_copy = package.shutil.copyfile
        def copy(src, dst):
            value = original_copy(src, dst)
            if mutate and Path(src).name == 'remote-hosts-code': Path(src).write_bytes(b'changed binary')
            return value
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(package, 'ROOT', self.root))
            stack.enter_context(mock.patch.object(sys, 'argv', ['package', '--version', '1.2.3', '--verification', str(proof), '--producer-pid', str(os.getpid())]))
            stack.enter_context(mock.patch.object(lifecycle, 'validate_producer', return_value={}))
            stack.enter_context(mock.patch.object(lifecycle, 'established_target', return_value=self.target))
            stack.enter_context(mock.patch.object(package.subprocess, 'check_output', return_value='remote-hosts-code 1.2.3'))
            stack.enter_context(mock.patch.object(package, 'machine_contract', return_value={'version': '1.2.3'}))
            stack.enter_context(mock.patch.object(package.importlib.util, 'module_from_spec', side_effect=loader))
            stack.enter_context(mock.patch.object(package.shutil, 'copyfile', side_effect=copy))
            with contextlib.redirect_stdout(io.StringIO()): package.main()

    def test_package_writes_exact_stage_group_and_preserves_current(self):
        proof, current, current_bundle = self.package_fixture()
        self.invoke_package(proof)
        stage, bundle = lifecycle.stages(self.root)
        with tarfile.open(bundle) as archive:
            names = archive.getnames()
        self.assertIn('remote-hosts-code-macos-arm64', names)
        self.assertIn('remote-hosts-code-linux-amd64', names)
        self.assertIn('remote-hosts-code-windows-amd64.exe', names)
        self.assertIn('source-verification.json', names)
        self.assertIn('manifest.json', names)
        self.assertEqual((current/'old').read_bytes(), b'old current')
        self.assertEqual(current_bundle.read_bytes(), b'old bundle')
        manifest = json.loads((stage/'manifest.json').read_text())
        self.assertEqual(manifest['build_execution_root'], str(self.root))
        self.assertFalse(manifest['deployed'])

    def test_package_rejects_binary_change_during_copy(self):
        proof, current, _ = self.package_fixture()
        with self.assertRaisesRegex(ValueError, 'artifact changed'):
            self.invoke_package(proof, mutate=True)
        self.assertEqual((current/'old').read_bytes(), b'old current')
        self.assertFalse(lifecycle.stages(self.root)[1].exists())

    def test_package_preserves_failed_stage_for_parent_recovery(self):
        proof, current, _ = self.package_fixture()
        stage = lifecycle.stages(self.root)[0]; stage.mkdir(); (stage/'partial').write_text('evidence')
        with self.assertRaisesRegex(SystemExit, 'stage already exists'):
            self.invoke_package(proof)
        self.assertEqual((stage/'partial').read_text(), 'evidence')
        self.assertEqual((current/'old').read_bytes(), b'old current')

    def test_incomplete_source_receipt_refuses_package(self):
        proof, current, _ = self.package_fixture()
        data = json.loads(proof.read_text()); data['checks'].pop('python_tests'); proof.write_text(json.dumps(data))
        with self.assertRaisesRegex(SystemExit, 'verification is incomplete'):
            self.invoke_package(proof)
        self.assertFalse(lifecycle.stages(self.root)[0].exists())
        self.assertEqual((current/'old').read_bytes(), b'old current')

    def test_bare_package_requires_active_producer(self):
        with mock.patch.object(package, 'ROOT', self.root), mock.patch.object(sys, 'argv', ['package', '--version', '1.2.3', '--verification', '/unused', '--producer-pid', str(os.getpid())]), mock.patch.object(lifecycle, 'validate_producer', side_effect=ValueError('no active reservation')):
            with self.assertRaisesRegex(ValueError, 'reservation'):
                package.main()
        self.assertFalse((self.root/'dist').exists())


if __name__ == '__main__':
    unittest.main()
