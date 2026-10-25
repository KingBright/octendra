"""Consumer admission with temporary gate roots and mocked helpers/children."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import build_slot
import lifecycle_preload_gate as gate
import release_current as current


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ConsumerGuardTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.skills = self.root / 'skills'
        self.skills.mkdir(mode=0o700)
        self.guard = gate.Gate(self.skills)
        patch = mock.patch.object(gate, 'Gate', return_value=self.guard)
        patch.start()
        self.addCleanup(patch.stop)

    def hold(self):
        return self.guard.change('begin', 'consumer-fixture', 'absent', 'a' * 64)

    def assert_admitted(self):
        gate.require_admitted()
        with self.assertRaisesRegex(gate.Refusal, 'gate_busy'):
            self.hold()

    def test_body_error_identity_survives_gate_exit(self):
        error = ValueError('original producer failure')
        with self.assertRaises(ValueError) as observed:
            with current.admission():
                self.assert_admitted()
                raise error
        self.assertIs(observed.exception, error)
        with self.assertRaisesRegex(gate.Refusal, 'scoped'):
            gate.require_admitted()

    def test_raw_loader_refuses_before_import_without_scope(self):
        with mock.patch.object(current.importlib.util, 'spec_from_file_location') as loader:
            with self.assertRaisesRegex(gate.Refusal, 'scoped'):
                current.public_api()
        loader.assert_not_called()

    def test_helper_method_and_child_are_covered_until_return(self):
        helper = mock.Mock()
        helper.companion.return_value.validate_producer.side_effect = lambda *args: self.assert_admitted()
        with mock.patch.object(current, 'public_api', return_value=helper):
            current.validate_producer(self.root, 123)
        with self.assertRaisesRegex(gate.Refusal, 'scoped'):
            gate.require_admitted()
        def child(*args, **kwargs):
            self.assert_admitted()
            return subprocess.CompletedProcess(args[0], 0, '{"status":"ok"}', '')
        with mock.patch.object(current.subprocess, 'run', side_effect=child):
            self.assertEqual(current.invoke(self.root, 'release-preflight')['status'], 'ok')

    def test_held_gate_prevents_helper_and_preserves_deferred_abort(self):
        self.hold()
        with mock.patch.object(current.subprocess, 'run') as child:
            with self.assertRaisesRegex(gate.Refusal, 'gate_held'):
                current.invoke(self.root, 'release-preflight')
            result = current.abort(self.root)
        child.assert_not_called()
        self.assertEqual(result['status'], 'deferred')
        self.assertTrue(result['artifacts_preserved'])

    def test_original_build_lease_covers_its_yield_and_rejects_before_creation(self):
        slot = self.root / 'slot'
        with build_slot.lease(slot, self.root / 'report.json', 'fixture'):
            self.assert_admitted()
        self.hold()
        blocked = self.root / 'blocked-slot'
        with self.assertRaisesRegex(gate.Refusal, 'gate_held'):
            with build_slot.lease(blocked, self.root / 'blocked.json', 'fixture'):
                self.fail('held producer entered')
        self.assertFalse(blocked.exists())

    def test_producers_refuse_before_their_original_work(self):
        runner = load('guard_runner_fixture', 'release-code.py')
        repackage = load('guard_repackage_fixture', 'repackage-code-current.py')
        desktop = load('guard_desktop_fixture', 'build-linux-desktop-worker.py')
        windows = load('guard_windows_fixture', 'build_windows_current.py')
        package = load('guard_package_fixture', 'package-code-release.py')
        admin = load('guard_admin_fixture', 'prepare-admin-helper.py')
        verifier = load('guard_verifier_fixture', 'check-code-source.py')
        self.hold()
        report = self.root / 'not-created' / 'report.json'
        operations = [
            lambda: runner.run_pipeline(None, report, self.root / 'slot'),
            lambda: repackage.run(self.root, report, None, None, None),
            lambda: desktop.run(SimpleNamespace()),
            lambda: windows.build(root=self.root),
            package.main,
            lambda: admin.publish_managed(SimpleNamespace()),
            lambda: verifier.run_verification(self.root, report),
        ]
        for operation in operations:
            with self.subTest(operation=operation):
                with self.assertRaisesRegex(gate.Refusal, 'gate_held'):
                    operation()
        self.assertFalse(report.parent.exists())

    def worker(self, held):
        worker = load('guard_worker_fixture', 'dev-verify.py')
        queue = self.root / 'queue'
        requests = queue / 'requests'
        requests.mkdir(parents=True)
        request = requests / 'fixture.json'
        request.write_text(json.dumps({'id': 'fixture', 'argv': ['cargo', '--version']}))
        if held:
            self.hold()
        def child(*args, **kwargs):
            self.assert_admitted()
            kwargs['stdout'].write(b'fixture child\n')
            return subprocess.CompletedProcess(args[0], 0)
        toolchain = {'available': True, 'cargo': 'cargo', 'rustc': 'rustc',
                     'cargo_target_dir': str(self.root / 'target'), 'env': {}}
        with mock.patch.object(worker, 'ROOT', self.root), \
             mock.patch.object(worker, 'find_toolchain', return_value=toolchain), \
             mock.patch.object(worker.subprocess, 'run', side_effect=child) as run:
            worker.serve_worker(queue_dir=queue, once=True)
        return request, queue, run

    def test_held_worker_preserves_queued_request_without_starting_child(self):
        request, queue, child = self.worker(True)
        self.assertTrue(request.exists())
        self.assertFalse((queue / 'results/fixture.json').exists())
        child.assert_not_called()

    def test_worker_holds_scope_through_child_and_result_publication(self):
        request, queue, child = self.worker(False)
        self.assertFalse(request.exists())
        result = json.loads((queue / 'results/fixture.json').read_text())
        self.assertEqual(result['exit_code'], 0)
        child.assert_called_once()


if __name__ == '__main__':
    unittest.main()
