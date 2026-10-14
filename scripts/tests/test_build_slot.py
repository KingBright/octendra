"""Deterministic build-slot tests: temporary source trees, no real build/deployment."""
import importlib.util
import json
import os
import pathlib
import shutil
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import build_slot
import source_snapshot

spec = importlib.util.spec_from_file_location('release_runner035', SCRIPTS/'release-code.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class BuildSlotTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.base = pathlib.Path(self.tmp.name).resolve()
        self.root = self.base/'editor'
        (self.root/'scripts').mkdir(parents=True)
        (self.root/'crates/remote-hosts-code/src').mkdir(parents=True)
        (self.root/'Cargo.toml').write_text('[workspace]\nmembers=[]\n')
        (self.root/'crates/remote-hosts-code/Cargo.toml').write_text('[package]\nname="remote-hosts-code"\nversion="0.3.5"\n')
        (self.root/'crates/remote-hosts-code/src/lib.rs').write_text('pub fn answer() -> u8 { 42 }\n')
        shutil.copyfile(SCRIPTS/'check-code-source.py', self.root/'scripts/check-code-source.py')
        self.slot = self.base/'slot'
        self.snapshot = None
        self.patches = [mock.patch.object(runner, 'ROOT', self.root),
            mock.patch.object(runner.release_current, 'primary', side_effect=lambda root: root),
            mock.patch.object(runner.release_current, 'preflight', return_value={}),
            mock.patch.object(runner.release_current, 'admit', return_value={}),
            mock.patch.object(runner.release_current, 'abort', return_value={}),
            mock.patch.object(runner.release_current, 'established_target', return_value=self.base/'shared-target')]
        for patch in self.patches: patch.start()
        self.manifest = runner.release_current.snapshot_evidence(None, self.root)
    def tearDown(self):
        for patch in reversed(self.patches): patch.stop()
        self.tmp.cleanup()
    def prepare(self):
        with build_slot.lease(self.slot, self.base/'receipt.json', self.manifest['snapshot_id'], execution_root=self.root) as checkout:
            self.checkout = checkout
            return runner.release_current.check_inputs(checkout, self.manifest)
    def test_main_input_mutation_is_preserved_and_rejected(self):
        self.prepare()
        path=self.root/'Cargo.toml';path.write_text('new editor changes')
        with self.assertRaises(ValueError):self.prepare()
        self.assertEqual(path.read_text(),'new editor changes')
    def test_concurrent_owner_returns_original_report_without_work(self):
        report = self.base/'active.json'
        with build_slot.lease(self.slot, report, self.manifest['snapshot_id']):
            with self.assertRaises(build_slot.SlotBusy) as caught:
                with build_slot.lease(self.slot, self.base/'other.json', 'other'):
                    self.fail('second caller acquired active slot')
            self.assertEqual(caught.exception.owner['report'], str(report))
    def test_lock_released_on_failure(self):
        with self.assertRaisesRegex(RuntimeError, 'test failure'):
            with build_slot.lease(self.slot, self.base/'one.json', 'one'):
                raise RuntimeError('test failure')
        with build_slot.lease(self.slot, self.base/'two.json', 'two'):
            self.assertEqual(json.loads((self.slot/'owner.json').read_text())['snapshot_id'], 'two')
    def test_unowned_directory_and_symlink_are_rejected(self):
        self.slot.mkdir()
        (self.slot/'README').write_text('user directory')
        with self.assertRaises(ValueError):
            with build_slot.lease(self.slot, self.base/'r.json', 'x'):
                self.fail('adopted user directory')
        link = self.base/'link'
        link.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(ValueError):
            build_slot.synchronize(self.snapshot, link/'checkout')
    def test_snapshot_and_checkout_must_not_overlap(self):
        with self.assertRaises(ValueError):
            build_slot.synchronize(self.snapshot, self.root/'nested')
    def test_busy_pipeline_does_not_execute_any_command(self):
        with build_slot.lease(self.slot, self.base/'active.json', self.manifest['snapshot_id']):
            with mock.patch.object(runner.subprocess, 'Popen', side_effect=AssertionError('must not compile')):
                result = runner.run_pipeline(self.snapshot, self.base/'busy.json', self.slot)
        self.assertEqual(result['state'], 'busy')
        self.assertEqual(result['owner']['report'], str(self.base/'active.json'))
    def test_existing_report_is_observation_not_a_new_build(self):
        report = self.base/'prior.json'
        report.write_text(json.dumps({'state': 'failed', 'snapshot_id': self.manifest['snapshot_id'], 'verify_only': False}))
        with mock.patch.object(runner.subprocess, 'Popen', side_effect=AssertionError('must not replay')):
            result = runner.run_pipeline(self.snapshot, report, self.slot)
        self.assertEqual(result['state'], 'failed')
        self.assertFalse(self.slot.exists())
    def test_toolchain_probe_timeout_is_bounded_and_distinct_from_a_lease_failure(self):
        report = self.base/'toolchain-timeout.json'
        error = runner.subprocess.TimeoutExpired(['rustc', '-vV'], 60)
        with mock.patch.object(runner.subprocess, 'check_output', side_effect=error) as probe, \
             mock.patch.object(runner, 'run_stage') as stage:
            result = runner.run_pipeline(self.snapshot, report, self.slot)
        self.assertEqual(probe.call_args.args[0], ['rustc', '-vV'])
        self.assertEqual(probe.call_args.kwargs['timeout'], 60)
        self.assertEqual(result['state'], 'failed')
        self.assertEqual(result['phase'], 'toolchain_probe')
        self.assertEqual(result['failure_type'], 'TimeoutExpired')
        self.assertIn('rustc', result['failure_detail'])
        self.assertEqual(json.loads(report.read_text())['phase'], 'toolchain_probe')
        self.assertEqual(json.loads((self.slot/'owner.json').read_text())['state'], 'released')
        stage.assert_not_called()
    def test_status_query_does_not_run_a_command(self):
        report = self.base/'status.json'
        report.write_text(json.dumps({'state': 'running', 'phase': 'verification'}))
        with mock.patch.object(runner.subprocess, 'Popen', side_effect=AssertionError('must not run')):
            value = runner.current_status(report)
        self.assertFalse(value['observation']['mutates_build'])
        self.assertEqual(value['phase'], 'verification')
    def test_failed_stage_retains_exit_code_log_and_does_not_claim_success(self):
        self.prepare()
        directory = self.base/'stage-logs'
        directory.mkdir()
        state = {'stages': {}}
        with mock.patch.object(runner.build_process, 'process_table', return_value={}), \
             mock.patch.object(runner.build_process, 'live_groups', return_value=[]), \
             self.assertRaisesRegex(RuntimeError, 'stage_failed'):
            runner.run_stage('fixture', [sys.executable, '-c', 'print("fixture-output"); raise SystemExit(7)'],
                             self.checkout, directory, self.base/'stage.json', state, dict(os.environ), timeout=10, manifest=self.manifest)
        saved = json.loads((self.base/'stage.json').read_text())
        self.assertEqual(saved['stages']['fixture']['exit_code'], 7)
        self.assertIn('fixture-output', (directory/'fixture.log').read_text())
        self.assertNotEqual(saved['state'], 'passed')
    def test_stage_rejects_input_mutation_even_with_zero_exit(self):
        self.prepare()
        directory = self.base/'stage-logs'
        directory.mkdir()
        state = {'stages': {}}
        with mock.patch.object(runner.build_process, 'process_table', return_value={}), \
             mock.patch.object(runner.build_process, 'live_groups', return_value=[]), \
             self.assertRaises(ValueError):
            runner.run_stage('fixture', [sys.executable, '-c', 'from pathlib import Path; Path("Cargo.toml").write_text("changed")'],
                             self.checkout, directory, self.base/'stage.json', state, dict(os.environ), timeout=10, manifest=self.manifest)
        self.assertEqual((self.checkout/'Cargo.toml').read_text(), 'changed')
        self.assertNotEqual(state['state'], 'passed')
    def test_symlinked_metadata_does_not_redirect_status_writes(self):
        self.slot.mkdir()
        target = self.base/'private-file'
        target.write_text('unchanged')
        (self.slot/'slot.json').symlink_to(target)
        with self.assertRaises(ValueError):
            with build_slot.lease(self.slot, self.base/'r.json', 'x'):
                self.fail('followed symlink')
        self.assertEqual(target.read_text(), 'unchanged')
    def test_admission_refusal_preserves_historical_version_before_expensive_checks(self):
        self.prepare()
        (self.checkout/'dist/remote-hosts-code-0.3.5').mkdir(parents=True)
        with mock.patch.object(runner.subprocess, 'Popen', side_effect=AssertionError('must not compile')), \
             mock.patch.object(runner.release_current, 'admit', side_effect=ValueError('current activity protected')):
            result = runner.run_pipeline(self.snapshot, self.base/'collision.json', self.slot)
        self.assertEqual(result['state'], 'failed')
        self.assertEqual(result['failure_type'], 'ValueError')
        self.assertTrue((self.checkout/'dist/remote-hosts-code-0.3.5').is_dir())
    def test_progress_reads_active_cargo_log_and_rejects_escaped_path(self):
        self.prepare()
        directory = self.base/'probe-logs'
        directory.mkdir()
        nested = self.checkout/'target/check.log'
        nested.parent.mkdir()
        nested.write_text('Blocking waiting for file lock on build directory')
        fallback = directory/'verification.log'
        fallback.write_text('stage started')
        report = directory/'verification.json'
        report.write_text(json.dumps({'checks': {'clippy': {'state':'running','log':'target/check.log'}}}))
        self.assertEqual(runner.progress_log('verification', self.checkout, directory, fallback), (nested, 'clippy'))
        report.write_text(json.dumps({'checks': {'clippy': {'state':'running','log':str(fallback)}}}))
        self.assertEqual(runner.progress_log('verification', self.checkout, directory, fallback)[0], fallback)
    def test_process_sampling_excludes_other_users_jobs(self):
        table = '10 1 00:01.50\n11 10 1:02.00\n12 11 00:03\n99 1 01:00:00\ninvalid\n'
        self.assertEqual(build_slot.descendants(table, 10), {10: 1.5, 11: 62.0, 12: 3.0})
        self.assertEqual(build_slot.cpu_time('2-01:02:03'), 176523)


if __name__ == '__main__':
    unittest.main()
