"""Read-only legacy identity and retired creation; tiny synthetic files only."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
def module(name, file):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS/file)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value
snapshot = module('snapshot', 'source_snapshot.py')
checker = module('snapshot_checker', 'check-code-source.py')

class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)/'legacy-evidence'
        self.dest = pathlib.Path(self.tmp.name)/'never-created'
        (self.root/'crates/remote-hosts-code/src').mkdir(parents=True)
        (self.root/'scripts').mkdir()
        (self.root/'Cargo.toml').write_text('[workspace]\n')
        (self.root/'Cargo.lock').write_text('# fixture\n')
        (self.root/'crates/remote-hosts-code/Cargo.toml').write_text('[package]\nversion="0.3.2"\n')
        self.source = self.root/'crates/remote-hosts-code/src/lib.rs'
        self.source.write_text('// synthetic legacy input\n')
        self.script = self.root/'scripts/check-code-source.py'
        self.script.write_text('raise AssertionError("never execute legacy source")\n')

    def legacy_manifest(self):
        # Construct only a tiny synthetic old receipt, never call a retired copier.
        files = checker.inputs(self.root)
        executable = snapshot.executable_files(self.root, files)
        proof = dict(schema_version=1, state='captured', source_inputs=files,
                     executable_files=executable, snapshot_id=snapshot.identity(files, executable),
                     file_count=len(files), bytes=sum((self.root/name).stat().st_size for name in files))
        (self.root/snapshot.MANIFEST).write_text(json.dumps(proof))
        return proof

    def test_cli_check_uses_current_verifier_not_archived_executable(self):
        proof = self.legacy_manifest()
        with mock.patch.object(sys, 'argv', ['source_snapshot.py', '--check', str(self.root)]), \
             mock.patch.object(snapshot, 'verifier', return_value=checker) as verifier, \
             contextlib.redirect_stdout(io.StringIO()) as output:
            snapshot.main()
        verifier.assert_called_once_with(snapshot.ROOT)
        self.assertEqual(json.loads(output.getvalue())['snapshot_id'], proof['snapshot_id'])

    def test_new_deleted_and_changed_inputs_break_identity(self):
        self.legacy_manifest()
        added = self.root/'scripts/new.py'; added.write_text('# unverified\n')
        with self.assertRaises(ValueError): snapshot.check(self.root, checker.inputs)
        added.unlink(); snapshot.check(self.root, checker.inputs)
        self.source.write_text('// changed\n')
        with self.assertRaises(ValueError): snapshot.check(self.root, checker.inputs)
        self.legacy_manifest(); (self.root/'Cargo.lock').unlink()
        with self.assertRaises(ValueError): snapshot.check(self.root, checker.inputs)

    @unittest.skipUnless(os.name=='posix', 'POSIX file identity')
    def test_executable_mode_is_bound_and_rechecked(self):
        self.script.chmod(0o755); proof=self.legacy_manifest()
        self.assertIn('scripts/check-code-source.py', proof['executable_files'])
        self.script.chmod(0o644)
        with self.assertRaises(ValueError): snapshot.check(self.root, checker.inputs)

    @unittest.skipUnless(os.name=='posix', 'symlink fixture')
    def test_linked_directory_is_not_silently_omitted(self):
        self.legacy_manifest()
        elsewhere=pathlib.Path(self.tmp.name)/'external'; elsewhere.mkdir()
        (self.root/'crates/external').symlink_to(elsewhere, target_is_directory=True)
        with self.assertRaises(ValueError): snapshot.check(self.root, checker.inputs)

    def test_root_migrations_are_bound_and_new_migration_invalidates_identity(self):
        migration=self.root/'migrations/0001.sql'; migration.parent.mkdir()
        migration.write_text('CREATE TABLE test(id INTEGER);')
        proof=self.legacy_manifest(); self.assertIn('migrations/0001.sql',proof['source_inputs'])
        (self.root/'migrations/0002.sql').write_text('ALTER TABLE test ADD name TEXT;')
        with self.assertRaises(ValueError): snapshot.check(self.root,checker.inputs)

    def test_build_outputs_credentials_and_reports_are_not_inputs(self):
        for file in ('target/debug/private.bin','dist/old.bin','.git/config','agent.json'):
            p=self.root/file;p.parent.mkdir(parents=True,exist_ok=True);p.write_text('not a source input')
        proof=self.legacy_manifest()
        for file in ('target/debug/private.bin','dist/old.bin','.git/config','agent.json'):
            self.assertNotIn(file,proof['source_inputs'])

    def test_android_sources_are_bound_but_private_generated_outputs_are_not(self):
        source=self.root/'android/app/src/main/java/Agent.kt'
        source.parent.mkdir(parents=True);source.write_text('// synthetic Android')
        private=('android/local.properties','android/build/private.json','android/.gradle/cache.bin',
                 'android/release.jks','android/app/src/debug/res/raw/fixture_ca.pem','android/release.apk')
        for name in private:
            path=self.root/name;path.parent.mkdir(parents=True,exist_ok=True);path.write_text('excluded')
        proof=self.legacy_manifest()
        self.assertIn('android/app/src/main/java/Agent.kt',proof['source_inputs'])
        for name in private:self.assertNotIn(name,proof['source_inputs'])
        source.write_text('// changed')
        with self.assertRaises(ValueError):snapshot.check(self.root,checker.inputs)

if __name__=='__main__': unittest.main()
