"""Verification evidence tests run tiny disposable child processes, never Cargo."""
import contextlib
import importlib.util
import io
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
SPEC = importlib.util.spec_from_file_location('source_check', pathlib.Path(__file__).resolve().parents[1]/'check-code-source.py')
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


class SourceEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name)
        for directory in ('crates/octendra-code/src', 'crates/dependency/src', 'scripts/tests'):
            (self.root/directory).mkdir(parents=True)
        (self.root/'Cargo.toml').write_text('[workspace]\n')
        (self.root/'Cargo.lock').write_text('# fixture\n')
        (self.root/'crates/octendra-code/Cargo.toml').write_text('[package]\nversion="0.3.1"\n')
        (self.root/'crates/octendra-code/src/lib.rs').write_text('// fixture\n')
        self.dependency = self.root/'crates/dependency/src/lib.rs'
        self.dependency.write_text('// dependency\n')
        self.report = self.root/'docs/proof.json'

    def tearDown(self):
        self.tmp.cleanup()

    def gates(self):
        return [(name, [sys.executable, '-c', 'print('+repr(
            'test result: ok. 2 passed; 0 failed; 0 ignored;' if name == 'rust_tests' else
            'Ran 2 tests in 0.001s\n\nOK' if name == 'python_tests' else 'ok')+')'])
                for name in ('fmt', 'clippy', 'rust_tests', 'python_tests', 'workspace')]

    def run_check(self, gates=None, timeout=5):
        with contextlib.redirect_stdout(io.StringIO()):
            return checker.run_verification(self.root, self.report, gates if gates is not None else self.gates(), timeout)

    def test_unchanged_inputs_have_complete_machine_receipt(self):
        result = self.run_check()
        self.assertEqual(result['state'], 'passed')
        self.assertEqual(result['functional_tests']['passed'], 4)
        self.assertEqual(result['functional_tests']['rust_executed'], 2)
        self.assertEqual(result['functional_tests']['python_executed'], 2)
        self.assertTrue(result['functional_tests']['evidence_complete'])
        self.assertTrue(checker.receipt_current(result, self.root))
        self.assertEqual(json.loads(self.report.read_text()), result)
        for entry in result['checks'].values():
            self.assertIsInstance(entry['command'], list)
            self.assertEqual(entry['working_directory'], str(self.root.resolve()))
            self.assertEqual(checker.digest(self.root/entry['log']), entry['sha256'])

    def test_primary_main_receipt_binds_identity_and_modes_without_source_snapshot(self):
        import source_snapshot
        files = checker.inputs(self.root)
        expected = source_snapshot.identity(files, source_snapshot.executable_files(self.root, files))
        result = self.run_check()
        self.assertEqual(result['snapshot_id'], expected)
        self.assertTrue(checker.receipt_current(result, self.root))
        self.assertFalse((self.root/'source-snapshot.json').exists())
        mode = self.dependency.stat().st_mode & 0o7777
        self.dependency.chmod(mode | 0o100)
        self.assertFalse(checker.receipt_current(result, self.root))

    def test_checkout_rules_and_page_fixtures_invalidate_receipts(self):
        paths = ('.gitattributes', 'scripts/tests/page.rs', 'scripts/tests/page.mjs')
        for name in paths:
            (self.root/name).write_text('original fixture\n')
        proof = self.run_check()
        self.assertTrue(checker.receipt_current(proof, self.root))
        for name in paths:
            with self.subTest(path=name):
                path = self.root/name
                path.write_text('changed fixture\n')
                self.assertFalse(checker.receipt_current(proof, self.root))
                path.write_text('original fixture\n')
                self.assertTrue(checker.receipt_current(proof, self.root))

    def test_native_gates_have_inactivity_budgets_without_wall_limits(self):
        observed = {}
        commands = {}
        def fake_run(argv, root, path, env, timeout, on_update=None):
            observed[path.stem] = timeout
            commands[path.stem] = list(argv)
            path.write_text('test result: ok. 2 passed; 0 failed; 0 ignored;' if path.stem == 'rust_tests' else
                            'Ran 2 tests in 0.001s\n\nOK' if path.stem == 'python_tests' else 'ok')
            return {'state':'finished', 'exit_code':0}
        with mock.patch.object(checker, 'run_command', side_effect=fake_run), contextlib.redirect_stdout(io.StringIO()):
            proof = checker.run_verification(self.root, self.report)
        self.assertEqual(observed, {'fmt':900, 'clippy':900, 'rust_tests':2700, 'python_tests':900, 'workspace':1800})
        self.assertTrue(checker.receipt_current(proof, self.root))
        self.assertEqual(proof['checks']['rust_tests']['idle_timeout_seconds'], 2700)
        self.assertTrue(all(x['wall_timeout_seconds'] is None for x in proof['checks'].values()))
        # Serialize independent cases, not the actors inside each concurrency test.
        # Preserve the full workspace and collection of every failure.
        self.assertEqual(commands['rust_tests'], [
            'bash', 'scripts/test.sh', '--release', '--locked', '--no-fail-fast',
            '--', '--test-threads=1', '--color', 'never'])
        self.assertIn('--all-targets', commands['clippy'])
        self.assertIn('--release', commands['clippy'])
        self.assertEqual(commands['workspace'], ['cargo', 'check', '--workspace', '--release', '--locked'])
        self.assertEqual(proof['rust_test_plan']['mode'], 'full')
        marker = json.loads((self.root/'target/source-verification/latest-full.json').read_text())
        self.assertEqual(marker['sha256'], checker.digest(self.report))

    def full_workspace_fixture(self):
        # This is a receipt/selection fixture, not a real Cargo execution claim.
        tests = set(checker.CORE_TESTS) | {'maintenance_mcp', 'poll_write_budget'}
        for _, targets in checker.TEST_DOMAINS:
            tests.update(targets)
        for name in tests:
            path = self.root/'crates/octendra-code/tests'/f'{name}.rs'
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('// fixture\n')
        for name in ('auth', 'files', 'terminal', 'token_output', 'gateway'):
            (self.root/'crates/octendra-code/src'/f'{name}.rs').write_text('// fixture\n')
        proof = self.run_check()
        proof['checks']['rust_tests']['command'] = list(checker.FULL_RUST_COMMAND)
        baseline = self.root/'target/previous-logs/verification.json'
        baseline.parent.mkdir(parents=True)
        baseline.write_text(json.dumps(proof))
        self.report = self.root/'docs/next.json'
        self.assertIsNotNone(checker.full_baseline(baseline, self.root))
        return baseline

    def plan(self, baseline=None, full=False):
        return checker.rust_test_plan(self.root, checker.inputs(self.root), full, baseline)

    def test_unchanged_rust_runs_core_release_targets_and_reports_omissions(self):
        baseline = self.full_workspace_fixture()
        plan = self.plan(baseline)
        self.assertEqual(plan['mode'], 'affected')
        self.assertEqual(set(plan['selected_targets']), {'octendra-code::lib', *checker.CORE_TESTS})
        self.assertIn('maintenance_mcp', plan['omitted_integration_targets'])
        self.assertIn('--release', plan['command'])
        self.assertIn('--lib', plan['command'])
        self.assertNotIn('--workspace', plan['command'])
        self.assertNotIn('octendra-mcp', plan['command'])

    def test_changed_domains_add_their_contracts_to_the_core(self):
        baseline = self.full_workspace_fixture()
        for module, required in [('auth', 'owner_password_cli'), ('files', 'transfers_040'),
                                 ('terminal', 'interaction_031'), ('token_output', 'experience_contract')]:
            with self.subTest(module=module):
                path = self.root/'crates/octendra-code/src'/f'{module}.rs'
                path.write_text('// changed\n')
                plan = self.plan(baseline)
                self.assertEqual(plan['mode'], 'affected')
                self.assertTrue(set(checker.CORE_TESTS).issubset(plan['selected_targets']))
                self.assertIn(required, plan['selected_targets'])
                self.assertIn('maintenance_mcp', plan['omitted_integration_targets'])
                path.write_text('// fixture\n')

    def test_shared_dependencies_and_unknown_source_changes_require_full_run(self):
        baseline = self.full_workspace_fixture()
        for name in ('Cargo.toml', 'Cargo.lock', 'crates/dependency/src/lib.rs',
                     'crates/octendra-code/src/gateway.rs'):
            with self.subTest(name=name):
                path = self.root/name; original = path.read_text()
                path.write_text(original+'// changed\n')
                plan = self.plan(baseline)
                self.assertEqual(plan['mode'], 'full')
                self.assertEqual(plan['command'], checker.FULL_RUST_COMMAND)
                path.write_text(original)

    def test_source_verifier_edits_do_not_compile_unrelated_rust_suites(self):
        baseline = self.full_workspace_fixture()
        (self.root/'scripts/check-code-source.py').write_text('# changed verifier\n')
        plan = self.plan(baseline)
        self.assertEqual(plan['mode'], 'affected')
        self.assertEqual(plan['changed_inputs'], ['scripts/check-code-source.py'])

    def test_changed_and_new_integration_tests_are_always_selected(self):
        baseline = self.full_workspace_fixture()
        for name in ('maintenance_mcp', 'new_contract'):
            path = self.root/'crates/octendra-code/tests'/f'{name}.rs'
            path.write_text('// changed test\n')
        plan = self.plan(baseline)
        self.assertEqual(plan['mode'], 'affected')
        self.assertTrue({'maintenance_mcp', 'new_contract'}.issubset(plan['selected_targets']))

    def test_removed_targets_shared_fixtures_and_wrapper_edits_require_full_run(self):
        baseline = self.full_workspace_fixture()
        path = self.root/'crates/octendra-code/tests/maintenance_mcp.rs'; path.unlink()
        self.assertEqual(self.plan(baseline)['mode'], 'full')
        path.write_text('// fixture\n')
        path = self.root/'crates/octendra-code/tests/support/common.rs'
        path.parent.mkdir(); path.write_text('// changed fixture\n')
        self.assertEqual(self.plan(baseline)['mode'], 'full')
        path.unlink()
        (self.root/'scripts/test.sh').write_text('cargo test --workspace\n')
        baseline.write_text(json.dumps(dict(json.loads(baseline.read_text()), source_inputs=checker.inputs(self.root))))
        (self.root/'scripts/test.sh').write_text('cargo test --lib\n')
        self.assertEqual(self.plan(baseline)['mode'], 'full')

    def test_failed_incomplete_zero_or_partial_baselines_never_omit_targets(self):
        baseline = self.full_workspace_fixture(); original = json.loads(baseline.read_text())
        mutations = [('state', lambda p: p.update(state='failed')),
                     ('truncated', lambda p: p['checks']['rust_tests'].update(output_truncated=True)),
                     ('incomplete', lambda p: p['checks']['rust_tests'].update(output_complete=False)),
                     ('zero', lambda p: p['functional_tests'].update(rust_executed=0)),
                     ('partial', lambda p: p.update(rust_test_plan={'mode':'affected'})),
                     ('narrowed command', lambda p: p['checks']['rust_tests'].update(command=['cargo','test','--release','--lib']))]
        for name, mutate in mutations:
            with self.subTest(name=name):
                proof = json.loads(json.dumps(original)); mutate(proof)
                baseline.write_text(json.dumps(proof))
                self.assertEqual(self.plan(baseline)['mode'], 'full')
        baseline.write_text(json.dumps(original))
        (self.root/original['checks']['rust_tests']['log']).write_text('corrupted\n')
        self.assertEqual(self.plan(baseline)['mode'], 'full')

    def test_explicit_full_mode_preserves_the_original_workspace_recipe(self):
        baseline = self.full_workspace_fixture()
        plan = self.plan(baseline, full=True)
        self.assertEqual(plan['mode'], 'full')
        self.assertEqual(plan['command'], checker.FULL_RUST_COMMAND)
        self.assertFalse(plan['omitted_integration_targets'])

    def test_affected_receipt_binds_the_plan_baseline_and_executed_command(self):
        baseline = self.full_workspace_fixture()
        def fake_run(argv, root, path, env, timeout, **kwargs):
            path.write_text('test result: ok. 2 passed; 0 failed; 0 ignored;' if path.stem == 'rust_tests' else
                            'Ran 2 tests in 0.001s\n\nOK' if path.stem == 'python_tests' else 'ok')
            return {'state':'finished', 'exit_code':0, 'output_complete':True}
        with mock.patch.object(checker, 'run_command', side_effect=fake_run), contextlib.redirect_stdout(io.StringIO()):
            proof = checker.run_verification(self.root, self.report)
        self.assertEqual(proof['rust_test_plan']['mode'], 'affected')
        self.assertTrue(checker.receipt_current(proof, self.root))
        self.assertEqual(proof['functional_tests']['rust_passed'], 2)  # No baseline counts added.
        changed = json.loads(json.dumps(proof)); changed['checks']['rust_tests']['command'] = ['cargo','test','--release','--lib']
        self.assertFalse(checker.receipt_current(changed, self.root))
        changed = json.loads(json.dumps(proof)); changed.pop('rust_test_plan')
        self.assertFalse(checker.receipt_current(changed, self.root))
        baseline.write_text(baseline.read_text()+'\n')
        self.assertFalse(checker.receipt_current(proof, self.root))

    def test_existing_receipt_is_rejected_before_executing_commands(self):
        self.report.parent.mkdir()
        self.report.write_text('original receipt')
        with mock.patch.object(checker.subprocess, 'Popen') as process:
            with self.assertRaises(FileExistsError):
                self.run_check()
        process.assert_not_called()
        self.assertEqual(self.report.read_text(), 'original receipt')

    def test_dependency_edits_and_new_files_invalidate_previous_success(self):
        result = self.run_check()
        self.dependency.write_text('// changed dependency')
        self.assertFalse(checker.receipt_current(result, self.root))
        self.dependency.write_text('// dependency\n')
        self.assertTrue(checker.receipt_current(result, self.root))
        added = self.dependency.with_name('added.rs')
        added.write_text('// newly added unverified file')
        self.assertFalse(checker.receipt_current(result, self.root))
        added.unlink()
        self.dependency.unlink()
        self.assertFalse(checker.receipt_current(result, self.root))

    def test_source_change_during_gate_produces_stale_not_passed(self):
        code = 'import pathlib; pathlib.Path('+repr(str(self.dependency))+').write_text("changed")'
        result = self.run_check([('fmt', [sys.executable, '-c', code])])
        self.assertEqual(result['state'], 'stale')
        self.assertIn('crates/dependency/src/lib.rs', result['changed_inputs'])
        self.assertFalse(result['source_inputs_unchanged'])

    def test_python_failures_and_errors_are_counted(self):
        logs = self.root/'logs'
        logs.mkdir()
        (logs/'rust_tests.log').write_text('test result: FAILED. 2 passed; 1 failed; 0 ignored;\n')
        (logs/'python_tests.log').write_text('Ran 5 tests in 0.02s\n\nFAILED (failures=2, errors=1)\n')
        count = checker.summarize({'rust_tests': {'exit_code': 1}, 'python_tests': {'exit_code': 1}}, logs)
        self.assertEqual(count['failed'], 4)
        self.assertEqual(count['python_failed'], 3)
        self.assertEqual(count['passed'], 4)
        self.assertFalse(count['test_gates_completed_successfully'])

    def test_zero_selected_tests_cannot_be_reported_as_verified(self):
        gates = [
            ('rust_tests', [sys.executable, '-c', 'print("test result: ok. 0 passed; 0 failed; 0 ignored;")']),
            ('python_tests', [sys.executable, '-c', 'print("Ran 0 tests in 0.001s\\n\\nOK")']),
        ]
        result = self.run_check(gates)
        self.assertEqual(result['state'], 'failed')
        self.assertEqual(result['failure_type'], 'IncompleteTestEvidence')
        self.assertFalse(result['functional_tests']['evidence_complete'])
        self.assertEqual(result['functional_tests']['rust_executed'], 0)
        self.assertEqual(result['functional_tests']['python_executed'], 0)
        self.assertFalse(checker.receipt_current(result, self.root))

    def test_all_ignored_or_skipped_tests_never_count_as_executed(self):
        gates = [
            ('rust_tests', [sys.executable, '-c', 'print("test result: ok. 0 passed; 0 failed; 3 ignored;")']),
            ('python_tests', [sys.executable, '-c', 'print("Ran 2 tests in 0.001s\\n\\nOK (skipped=2)")']),
        ]
        result = self.run_check(gates)
        count = result['functional_tests']
        self.assertEqual(result['state'], 'failed')
        self.assertEqual((count['rust_selected'], count['python_selected']), (3, 2))
        self.assertEqual((count['rust_executed'], count['python_executed']), (0, 0))
        self.assertFalse(count['evidence_complete'])

    def test_exit_zero_does_not_hide_failed_or_truncated_test_evidence(self):
        logs = self.root/'logs'; logs.mkdir()
        (logs/'rust_tests.log').write_text('test result: FAILED. 1 passed; 1 failed; 0 ignored;\n')
        (logs/'python_tests.log').write_text('Ran 1 test in 0.01s\n\nOK\n')
        checks = {name: {'state':'finished', 'exit_code':0, 'output_complete':True}
                  for name in ('rust_tests', 'python_tests')}
        self.assertFalse(checker.summarize(checks, logs)['evidence_complete'])
        (logs/'rust_tests.log').write_text('test result: ok. 1 passed; 0 failed; 0 ignored;\n')
        checks['rust_tests']['output_truncated'] = True
        self.assertFalse(checker.summarize(checks, logs)['evidence_complete'])
        checks['rust_tests']['output_truncated'] = False
        checks['python_tests']['output_complete'] = False
        self.assertFalse(checker.summarize(checks, logs)['evidence_complete'])

    def test_failed_gate_is_saved_and_stops_following_commands(self):
        sentinel = self.root/'should-not-exist'
        gates = [('fmt', [sys.executable, '-c', 'raise SystemExit(7)']),
                 ('clippy', [sys.executable, '-c', 'open('+repr(str(sentinel))+',"w").close()'])]
        result = self.run_check(gates)
        self.assertEqual(result['state'], 'failed')
        self.assertEqual(result['checks']['fmt']['exit_code'], 7)
        self.assertNotIn('clippy', result['checks'])
        self.assertFalse(sentinel.exists())

    def test_missing_interpreter_has_a_durable_failure_receipt(self):
        result = self.run_check([('fmt', [str(self.root/'missing-command')])])
        self.assertEqual(result['state'], 'failed')
        self.assertEqual(result['checks']['fmt']['state'], 'start_or_collection_failed')
        self.assertEqual(result['checks']['fmt']['failure_type'], 'FileNotFoundError')

    @unittest.skipUnless(os.name == 'posix', 'process group cleanup is a POSIX guarantee')
    def test_inactivity_stops_owned_child_group_and_preserves_log(self):
        sentinel = self.root/'orphan-wrote-file'
        child = 'import time,pathlib; time.sleep(2.0); pathlib.Path('+repr(str(sentinel))+').touch()'
        ready = self.root/'child-group-created'
        parent = 'import subprocess,sys,time,pathlib; subprocess.Popen([sys.executable,"-c",'+repr(child)+']); print("started",flush=True); pathlib.Path('+repr(str(ready))+').touch(); time.sleep(30)'
        # Synchronize process startup before measuring the cancellation window.
        # Under compiler load, interpreter startup can exceed 200ms; an empty
        # startup log is not evidence that process-group cancellation is broken.
        original_popen = checker.subprocess.Popen
        def started_process(*args, **kwargs):
            process = original_popen(*args, **kwargs)
            deadline = time.monotonic()+5
            while not ready.exists() and time.monotonic()<deadline:
                time.sleep(0.01)
            if not ready.exists():
                import signal
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
                raise RuntimeError('fixture child did not start')
            return process
        with mock.patch.object(checker.subprocess, 'Popen', side_effect=started_process):
            result = self.run_check([('fmt', [sys.executable, '-c', parent])], timeout=0.2)
        self.assertEqual(result['state'], 'failed')
        self.assertEqual(result['checks']['fmt']['state'], 'stalled')
        self.assertIn('started', (self.root/result['checks']['fmt']['log']).read_text())
        time.sleep(2.1)
        self.assertFalse(sentinel.exists())


if __name__ == '__main__':
    unittest.main()
