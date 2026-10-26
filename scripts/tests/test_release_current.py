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
        (self.root/'crates/octendra-code').mkdir(parents=True)
        (self.root/'crates/octendra-code/Cargo.toml').write_text('[package]\nversion="1.2.3"\n')
        (self.root/'Cargo.toml').write_text('[workspace]\n')
        (self.root/'crates/octendra-cli').mkdir()
        (self.root/'crates/octendra-cli/Cargo.toml').write_text('[package]\nversion="0.1.0"\n')
        shutil.copyfile(SCRIPTS/'check-code-source.py', self.root/'scripts/check-code-source.py')
        self.target = self.root/'shared-target'
        self.target.mkdir()
        self.patch = mock.patch.object(lifecycle, 'primary', side_effect=lambda path: path)
        self.real_primary = lifecycle.primary
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.addCleanup(self.tmp.cleanup)
        self.manifest = lifecycle.snapshot_evidence(None, self.root)

    def test_non_json_helper_failure_retains_exit_and_bounded_evidence(self):
        failure = subprocess.CompletedProcess([], 1, '', 'compiler active traceback')
        with mock.patch.object(subprocess, 'run', return_value=failure):
            with self.assertRaisesRegex(ValueError, 'exit=1; evidence=.*compiler active'):
                lifecycle.invoke(self.root, 'release-admit')
        with mock.patch.object(subprocess, 'run', return_value=failure):
            result = lifecycle.invoke(self.root, 'release-admit', required=False)
        self.assertEqual(result['status'], 'deferred')
        self.assertTrue(result['artifacts_preserved'])

    def test_main_fingerprint_does_not_create_snapshot_copy(self):
        self.assertEqual(self.manifest['source_inputs'], source_snapshot.verifier(self.root).inputs(self.root))
        self.assertFalse((self.root/'source-snapshot.json').exists())
        self.assertNotIn('checkout', [p.name for p in self.root.rglob('*')])

    def test_long_helper_trace_saves_receipt_and_keeps_actual_cause_in_bounded_summary(self):
        cause = 'release_cleanup_backend.Refusal: process references release'
        trace = 'Traceback (most recent call last):\n' + 'frame\n' * 2000 + cause
        failure = subprocess.CompletedProcess([], 1, '', trace)
        with mock.patch.object(subprocess, 'run', return_value=failure):
            with self.assertRaises(ValueError) as observed:
                lifecycle.invoke(self.root, 'release-admit')
        self.assertIn(cause, str(observed.exception)[:400])
        receipts = list((self.root/'target/release-helper-failures').glob('release-admit-*.json'))
        self.assertEqual(len(receipts), 1)
        saved = json.loads(receipts[0].read_text())
        self.assertEqual(saved['action'], 'release-admit')
        self.assertEqual(saved['exit_code'], 1)
        self.assertEqual(saved['stderr'], trace)
        self.assertFalse(saved['truncated'])
        self.assertIn(str(receipts[0].relative_to(self.root)), str(observed.exception)[:400])

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

    def test_previous_current_identity_cannot_create_second_component(self):
        old = {'current_components': {'previous-product-three-platform': {
            'files': [str(self.root/'dist/previous-product-current')],
        }}}
        with mock.patch.object(lifecycle, 'invoke', return_value=old) as invoke:
            with self.assertRaisesRegex(ValueError, 'current identity migration required'):
                lifecycle.admit(self.root)
        self.assertEqual(invoke.call_args_list, [mock.call(self.root, 'release-preflight')])
        self.assertFalse((self.root/'dist').exists())

    def test_current_identity_requires_both_component_and_output_names(self):
        with self.assertRaisesRegex(ValueError, 'current identity migration required'):
            lifecycle.require_current_names({'current_components': {lifecycle.COMPONENT: {
                'files': [str(self.root/'dist/previous-product-current')],
            }}})
        lifecycle.require_current_names({'current_components': {lifecycle.COMPONENT: {
            'files': list(map(str, lifecycle.files(self.root))),
        }}})

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
            self.assertGreaterEqual(int(env['CARGO_BUILD_JOBS']), 1)
            self.assertLessEqual(int(env['CARGO_BUILD_JOBS']), 4)
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
            'launch-code-upgrade-windows.ps1', 'fleet-upgrade.py', 'gateway_self_upgrade_runner.py',
            'parallel-install.py', 'upgrade_instance.py']
        for name in helpers: (self.root/'scripts'/name).write_text('# fixture helper\n')
        docs=self.root/'docs/parallel-octendra';docs.mkdir(parents=True)
        (docs/'README.md').write_text('fresh parallel installation fixture\n')
        (docs/'gateway.json.in').write_text('{}\n')
        (docs/'new-host.caddy.in').write_text('# proxy fixture\n')
        binaries = ['release/octendra-code', 'x86_64-unknown-linux-musl/release/octendra-code',
                    'x86_64-pc-windows-msvc/release/octendra-code.exe',
                    'release/octendra', 'x86_64-unknown-linux-musl/release/octendra',
                    'x86_64-pc-windows-msvc/release/octendra.exe',
                    'aarch64-unknown-linux-musl/release/octendra-code',
                    'aarch64-unknown-linux-musl/release/octendra']
        for name in binaries:
            path = self.target/name; path.parent.mkdir(parents=True, exist_ok=True)
            header = bytearray(64)
            header[:6] = b'\x7fELF\x02\x01'
            header[18:20] = (183 if name.startswith('aarch64-') else 62).to_bytes(2, 'little')
            path.write_bytes(header if 'linux' in name else b'MZ fixture')
        verifier = source_snapshot.verifier(self.root)
        proof = {'version': '1.2.3', 'state': 'passed', 'source_inputs_unchanged': True,
                 'source_inputs': verifier.inputs(self.root), 'functional_tests': {'failed': 0},
                 'checks': {name: {'exit_code': 0} for name in ('fmt', 'clippy', 'rust_tests', 'python_tests', 'workspace')}}
        proof_path = self.root/'proof.json'; proof_path.write_text(json.dumps(proof))
        (self.root/'dist').mkdir(exist_ok=True)
        current, current_bundle = lifecycle.files(self.root)
        current.mkdir(); (current/'old').write_bytes(b'old current'); current_bundle.write_bytes(b'old bundle')
        return proof_path, current, current_bundle

    def invoke_package(self, proof, mutate=False, platforms=None):
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
            if mutate and Path(src).name == 'octendra-code': Path(src).write_bytes(b'changed binary')
            return value
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.object(package, 'ROOT', self.root))
            argv = ['package', '--version', '1.2.3', '--verification', str(proof), '--producer-pid', str(os.getpid())]
            if platforms is not None:
                argv.extend(['--platforms', *platforms])
            stack.enter_context(mock.patch.object(sys, 'argv', argv))
            stack.enter_context(mock.patch.object(lifecycle, 'validate_producer', return_value={}))
            stack.enter_context(mock.patch.object(lifecycle, 'established_target', return_value=self.target))
            stack.enter_context(mock.patch.object(package.subprocess, 'check_output', return_value='octendra-code 1.2.3'))
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
        self.assertIn('octendra-code-macos-arm64', names)
        self.assertIn('octendra-code-linux-amd64', names)
        self.assertIn('octendra-code-windows-amd64.exe', names)
        self.assertIn('octendra-macos-arm64', names)
        self.assertIn('octendra-linux-amd64', names)
        self.assertIn('octendra-windows-amd64.exe', names)
        self.assertIn('source-verification.json', names)
        self.assertIn('manifest.json', names)
        self.assertIn('parallel-install.py',names)
        self.assertIn('PARALLEL-INSTALL.md',names)
        self.assertIn('parallel-gateway.json.in',names)
        self.assertIn('parallel-new-host.caddy.in',names)
        self.assertIn('read-only plan',(stage/'README.md').read_text())
        self.assertEqual((current/'old').read_bytes(), b'old current')
        self.assertEqual(current_bundle.read_bytes(), b'old bundle')
        manifest = json.loads((stage/'manifest.json').read_text())
        self.assertEqual(manifest['build_execution_root'], str(self.root))
        self.assertFalse(manifest['deployed'])

    def test_selected_platforms_preserve_missing_windows_as_pending(self):
        proof, current, current_bundle = self.package_fixture()
        (self.target/'x86_64-pc-windows-msvc/release/octendra-code.exe').unlink()
        self.invoke_package(proof, platforms=['linux', 'macos'])
        stage, bundle = lifecycle.stages(self.root)
        manifest = json.loads((stage/'manifest.json').read_text())
        self.assertEqual(manifest['included_platforms'], ['macos', 'linux'])
        self.assertEqual(manifest['pending_platforms'], ['windows', 'linux-arm64'])
        with tarfile.open(bundle) as archive:
            names = archive.getnames()
        self.assertNotIn('octendra-code-windows-amd64.exe', names)
        self.assertIn('octendra-code-linux-amd64', names)
        self.assertIn('octendra-linux-amd64', names)
        self.assertNotIn('octendra-windows-amd64.exe', names)
        self.assertEqual(manifest['components']['native_cli']['source_version'], '0.1.0')
        self.assertEqual((current/'old').read_bytes(), b'old current')
        self.assertEqual(current_bundle.read_bytes(), b'old bundle')

    def test_windows_can_be_packaged_independently(self):
        proof, current, _ = self.package_fixture()
        (self.target/'x86_64-unknown-linux-musl/release/octendra-code').unlink()
        self.invoke_package(proof, platforms=['windows'])
        stage, bundle = lifecycle.stages(self.root)
        manifest = json.loads((stage/'manifest.json').read_text())
        self.assertEqual(manifest['included_platforms'], ['windows'])
        self.assertEqual(manifest['pending_platforms'], ['macos', 'linux', 'linux-arm64'])
        with tarfile.open(bundle) as archive:
            names = archive.getnames()
        self.assertIn('octendra-code-windows-amd64.exe', names)
        self.assertNotIn('octendra-code-macos-arm64', names)
        self.assertNotIn('octendra-code-linux-amd64', names)
        self.assertIn('octendra-windows-amd64.exe', names)
        self.assertNotIn('octendra-macos-arm64', names)
        self.assertNotIn('octendra-linux-amd64', names)
        self.assertEqual((current/'old').read_bytes(), b'old current')

    def test_default_package_still_requires_windows(self):
        proof, current, _ = self.package_fixture()
        (self.target/'x86_64-pc-windows-msvc/release/octendra-code.exe').unlink()
        with self.assertRaises(SystemExit):
            self.invoke_package(proof)
        self.assertEqual((current/'old').read_bytes(), b'old current')
        self.assertFalse(lifecycle.stages(self.root)[1].exists())

    def test_arm64_can_be_packaged_without_amd64_or_windows(self):
        proof, current, current_bundle = self.package_fixture()
        for name in ('x86_64-unknown-linux-musl/release/octendra-code',
                     'x86_64-pc-windows-msvc/release/octendra-code.exe'):
            (self.target/name).unlink()
        self.invoke_package(proof, platforms=['linux-arm64'])
        stage, bundle = lifecycle.stages(self.root)
        manifest = json.loads((stage/'manifest.json').read_text())
        self.assertEqual(manifest['included_platforms'], ['linux-arm64'])
        self.assertEqual(manifest['pending_platforms'], ['macos', 'linux', 'windows'])
        with tarfile.open(bundle) as archive:
            names = archive.getnames()
        self.assertIn('octendra-code-linux-arm64', names)
        self.assertIn('octendra-linux-arm64', names)
        self.assertNotIn('octendra-code-linux-amd64', names)
        self.assertNotIn('octendra-code-windows-amd64.exe', names)
        self.assertEqual((current/'old').read_bytes(), b'old current')
        self.assertEqual(current_bundle.read_bytes(), b'old bundle')

    def test_renamed_amd64_is_not_packaged_as_arm64(self):
        proof, current, _ = self.package_fixture()
        (self.target/'aarch64-unknown-linux-musl/release/octendra-code').write_bytes(
            (self.target/'x86_64-unknown-linux-musl/release/octendra-code').read_bytes())
        with self.assertRaisesRegex(ValueError, 'ELF architecture'):
            self.invoke_package(proof, platforms=['linux-arm64'])
        self.assertEqual((current/'old').read_bytes(), b'old current')
        self.assertFalse(lifecycle.stages(self.root)[1].exists())

    def test_platform_selection_rejects_empty_duplicate_and_unknown(self):
        for values in ([], ['macos', 'macos'], ['macos', 'other']):
            with self.subTest(values=values), self.assertRaises(ValueError):
                lifecycle.selected_platforms(values)
        self.assertEqual(lifecycle.selected_platforms(), lifecycle.DEFAULT_PLATFORMS)
        self.assertEqual(lifecycle.selected_platforms(['linux', 'macos']), ('macos', 'linux'))
        self.assertEqual(lifecycle.selected_platforms(['windows']), ('windows',))
        self.assertEqual(lifecycle.selected_platforms(['linux']), ('linux',))

    def test_detached_platform_selection_is_bound_to_original_report(self):
        report = self.root/'selected.json'
        with mock.patch.object(runner, 'ROOT', self.root), mock.patch.object(os, 'posix_spawn', return_value=12345) as spawn, contextlib.redirect_stdout(io.StringIO()):
            runner.start_pipeline(None, report, platforms=['macos', 'linux'])
        argv = spawn.call_args.args[1]
        self.assertEqual(argv[argv.index('--platforms')+1:], ['macos', 'linux'])
        report.write_text(json.dumps({'snapshot_id': self.manifest['snapshot_id'],
                                     'verify_only': False, 'platforms': ['macos', 'linux'],
                                     'state': 'failed'}))
        with mock.patch.object(runner, 'ROOT', self.root), mock.patch.object(lifecycle, 'admit') as admit:
            with self.assertRaisesRegex(ValueError, 'different inputs or mode'):
                runner.run_pipeline(None, report, self.root/'target/release-slot')
        admit.assert_not_called()

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
