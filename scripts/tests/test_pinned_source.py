"""Tiny local Git fixtures; no Cargo, real package authority or deployment."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import pinned_source
import release_current
import source_snapshot
import build_slot

spec = importlib.util.spec_from_file_location('pinned_runner_fixture', SCRIPTS/'release-code.py')
runner = importlib.util.module_from_spec(spec); spec.loader.exec_module(runner)


class PinnedSourceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.name', 'Fixture'); self.git('config', 'user.email', 'fixture@example.invalid')
        self.git('config', 'commit.gpgsign', 'false'); self.git('config', 'core.hooksPath', '/dev/null')
        (self.root/'crates/octendra-code').mkdir(parents=True)
        (self.root/'crates/octendra-code/Cargo.toml').write_text('[package]\nname="fixture"\nversion="0.12.1"\n')
        (self.root/'crates/example.rs').write_text('selected old source\n')
        (self.root/'scripts').mkdir()
        shutil.copyfile(SCRIPTS/'check-code-source.py', self.root/'scripts/check-code-source.py')
        (self.root/'scripts/tool.sh').write_text('#!/bin/sh\nexit 0\n'); (self.root/'scripts/tool.sh').chmod(0o755)
        self.git('add', '.'); self.git('commit', '-qm', 'selected')
        self.commit = self.git('rev-parse', 'HEAD').strip()
        (self.root/'crates/example.rs').write_text('later security change\n')
        self.git('commit', '-qam', 'later main')
        self.head = self.git('rev-parse', 'HEAD').strip()
        # Both tracked and untracked WIP must survive the entire operation.
        (self.root/'crates/example.rs').write_text('user WIP must remain\n')
        (self.root/'untracked.txt').write_text('user untracked WIP\n')
        self.before = self.git('diff', '--binary')
        self.report = self.root/'receipts/build.json'; self.report.parent.mkdir()
        self.slot = self.root/'target/release-slot'

    def git(self, *args):
        return subprocess.check_output(['git', *args], cwd=self.root, text=True)

    @contextlib.contextmanager
    def source(self):
        with build_slot.lease(self.slot, self.report, self.commit, execution_root=self.root):
            receipt = pinned_source.prepare(self.root, self.commit, self.report)
            try: yield receipt, pinned_source.source_path(self.root)
            finally:
                if json.loads(receipt.read_text())['state'] == 'ready':
                    pinned_source.cleanup(receipt, self.root)

    def test_exact_commit_excludes_later_main_and_preserves_all_wip(self):
        with self.source() as (receipt, source):
            self.assertEqual((source/'crates/example.rs').read_text(), 'selected old source\n')
            self.assertFalse((source/'untracked.txt').exists())
            self.assertTrue((source/'scripts/tool.sh').stat().st_mode & 0o111)
            value = pinned_source.read(receipt, self.root)
            self.assertEqual(value['source_commit'], self.commit)
            self.assertEqual(value['producer_commit'], self.head)
            with self.assertRaisesRegex(ValueError, 'occupied'):
                pinned_source.prepare(self.root, self.commit, self.report)
        self.assertFalse(pinned_source.source_path(self.root).exists())
        self.assertEqual(self.git('rev-parse', 'HEAD').strip(), self.head)
        self.assertEqual(self.git('diff', '--binary'), self.before)
        self.assertEqual((self.root/'untracked.txt').read_text(), 'user untracked WIP\n')

    def test_branch_abbreviation_and_option_are_not_commits(self):
        for value in ['main', self.commit[:12], '--help', 'HEAD~1', '../source']:
            with self.subTest(value=value), self.assertRaises(ValueError):
                pinned_source.resolve(self.root, value)
        self.assertFalse(pinned_source.source_path(self.root).exists())

    def test_source_binding_rejects_other_root_and_modified_bytes(self):
        with build_slot.lease(self.slot, self.report, self.commit, execution_root=self.root):
            receipt = pinned_source.prepare(self.root, self.commit, self.report)
            with self.assertRaisesRegex(ValueError, 'primary producer'):
                pinned_source.read(self.report, self.root)
            source = pinned_source.source_path(self.root)
            (source/'crates/example.rs').write_text('changed input for inspection')
            with self.assertRaisesRegex(ValueError, 'source changed'):
                pinned_source.read(receipt, self.root)
            with self.assertRaisesRegex(ValueError, 'source changed'):
                pinned_source.cleanup(receipt, self.root)
            self.assertEqual((source/'crates/example.rs').read_text(), 'changed input for inspection')

    def test_unknown_or_symlinked_source_area_is_preserved(self):
        self.slot.mkdir(parents=True)
        source = pinned_source.source_path(self.root); source.mkdir(); (source/'unknown').write_text('retain')
        (self.slot/'owner.json').write_text(json.dumps({'pid': os.getpid(), 'state': 'running'}))
        with self.assertRaisesRegex(ValueError, 'occupied'):
            pinned_source.prepare(self.root, self.commit, self.report)
        self.assertEqual((source/'unknown').read_text(), 'retain')

    def test_input_fingerprint_uses_bound_source_without_weakening_primary(self):
        with self.source() as (receipt, source):
            manifest = release_current.snapshot_evidence(None, source, receipt, self.root)
            self.assertEqual(manifest['source_commit'], self.commit)
            release_current.check_inputs(source, manifest)
            with self.assertRaises(ValueError):
                release_current.primary(source)
        self.assertEqual(self.git('diff', '--binary'), self.before)

    def test_pinned_pipeline_binds_all_stages_to_one_input_and_main_publication(self):
        stages = []; target = self.root/'existing-cache'; target.mkdir()
        def stage(name, argv, checkout, directory, report, state, env, **kwargs):
            self.assertEqual(checkout, pinned_source.source_path(self.root))
            self.assertEqual((checkout/'crates/example.rs').read_text(), 'selected old source\n')
            self.assertEqual(env['PYTHONDONTWRITEBYTECODE'], '1')
            self.assertTrue(env['CARGO_TARGET_DIR'].endswith('/rust-target'))
            stages.append(name)
            state['stages'][name] = {'cleanup': {'state': 'not_required'}}
            if name == 'verification':
                self.assertEqual(argv[1], str(self.root/'scripts/check-code-source.py'))
                self.assertIn('--source-binding', argv)
                verifier = source_snapshot.verifier(checkout)
                proof = {'state': 'passed', 'version': '0.12.1', 'source_commit': self.commit,
                         'source_inputs': verifier.inputs(checkout), 'source_inputs_unchanged': True,
                         'snapshot_id': verifier.snapshot_identity(checkout),
                         'checks': {n: {'exit_code': 0} for n in verifier.REQUIRED},
                         'functional_tests': {'passed': 1, 'failed': 0, 'evidence_complete': True,
                                              'test_gates_completed_successfully': True}}
                (directory/'verification.json').write_text(json.dumps(proof))
            if name == 'package':
                self.assertEqual(argv[1], str(self.root/'scripts/package-code-release.py'))
                self.assertIn('--source-binding', argv)
                dest, bundle = release_current.stages(self.root); dest.parent.mkdir(exist_ok=True); dest.mkdir()
                (dest/'binary').write_bytes(b'fixture candidate')
                (dest/'manifest.json').write_text(json.dumps({'source_inputs': kwargs['manifest']['source_inputs'],
                    'source_commit': self.commit, 'artifacts': {'binary': {'sha256': pinned_source.digest(dest/'binary')}}}))
                bundle.write_bytes(b'fixture archive')
        def publish(root):
            self.assertEqual(root, self.root)
            for a,b in zip(release_current.stages(root), release_current.files(root)): a.rename(b)
            return {'current_committed': True}
        real = subprocess.check_output
        with mock.patch.object(runner, 'ROOT', self.root), \
             mock.patch.object(runner, 'shared_build_lock', return_value=contextlib.nullcontext()), \
             mock.patch.object(runner, 'run_stage', side_effect=stage), \
             mock.patch.object(runner, 'build_commands', return_value=[('macos_release', ['cargo', 'build'])]), \
             mock.patch.object(release_current, 'preflight', return_value={}), \
             mock.patch.object(release_current, 'admit', return_value={}), \
             mock.patch.object(release_current, 'publish', side_effect=publish), \
             mock.patch.object(release_current, 'established_target', return_value=target), \
             mock.patch.object(runner.subprocess, 'check_output', wraps=subprocess.check_output) as command, \
             contextlib.redirect_stdout(io.StringIO()):
            # Probe is the only non-Git subprocess; no compiler is run.
            command.side_effect = lambda argv, **kw: 'fixture rustc' if argv[0]=='rustc' else real(argv, **kw)
            result = runner.run_pipeline(None, self.report, self.slot, platforms=['macos'], source_commit=self.commit)
        self.assertEqual(result['state'], 'passed', result)
        self.assertEqual(stages, ['verification', 'macos_release', 'package'])
        self.assertEqual(result['source_cleanup']['state'], 'removed')
        self.assertFalse(pinned_source.source_path(self.root).exists())
        self.assertEqual(self.git('diff', '--binary'), self.before)
        self.assertEqual(self.git('rev-parse', 'HEAD').strip(), self.head)

    def test_verification_logs_survive_source_cleanup_and_keep_candidate_identity(self):
        spec = importlib.util.spec_from_file_location('pinned_verifier_fixture', SCRIPTS/'check-code-source.py')
        verifier = importlib.util.module_from_spec(spec); spec.loader.exec_module(verifier)
        def command(argv, root, path, env, timeout, **kwargs):
            path.write_text('Ran 1 test in 0.01s\n\nOK\n' if argv==['python_tests'] else
                            'test result: ok. 1 passed; 0 failed; 0 ignored;\n')
            return {'state': 'finished', 'exit_code': 0, 'output_complete': True}
        with self.source() as (receipt, source):
            evidence = self.report.parent/'verification.json'
            binding = pinned_source.read(receipt, self.root)
            with mock.patch.object(verifier, 'run_command', side_effect=command), contextlib.redirect_stdout(io.StringIO()):
                result = verifier.run_verification(source, evidence,
                    gates=[(name,[name]) for name in verifier.REQUIRED],
                    logs_root=self.report.parent/'durable-logs', source_binding=binding)
            self.assertEqual(result['state'], 'passed')
            self.assertEqual(result['source_commit'], self.commit)
            for check in result['checks'].values():
                self.assertTrue(Path(check['log']).is_absolute())
                self.assertFalse(Path(check['log']).is_relative_to(source))
            logs = [Path(row['log']) for row in result['checks'].values()]
        self.assertTrue(all(path.is_file() for path in logs))

    def test_detached_launcher_pins_sha_and_never_materializes_or_replays(self):
        with mock.patch.object(runner, 'ROOT', self.root), \
             mock.patch.object(runner.os, 'posix_spawn', return_value=123456789) as spawn, \
             mock.patch.object(pinned_source, 'prepare') as prepare, contextlib.redirect_stdout(io.StringIO()):
            runner.start_pipeline(None, self.report, platforms=['macos'], source_commit=self.commit)
            runner.start_pipeline(None, self.report, platforms=['macos'], source_commit=self.commit)
        spawn.assert_called_once(); prepare.assert_not_called()
        argv = spawn.call_args.args[1]
        self.assertEqual(argv[argv.index('--source-commit')+1], self.commit)
        self.assertNotIn('--snapshot', argv)
        self.assertNotIn('--expected-source-id', argv)
        self.assertFalse(pinned_source.source_path(self.root).exists())


if __name__ == '__main__':
    unittest.main()
