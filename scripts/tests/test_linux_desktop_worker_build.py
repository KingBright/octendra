"""Fail-closed build input fixtures. No Cargo, Portal, host SDK or release writes."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('linux_worker_builder', ROOT / 'scripts/build-linux-desktop-worker.py')
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


class LinuxWorkerBuildTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='linux-worker-sdk-fixture-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.sysroot = self.root / 'sysroot'
        self.sysroot.mkdir()
        self.header = self.sysroot / 'header.h'
        self.header.write_bytes(b'verified public header')
        self.manifest = self.root / 'manifest.json'
        self.data = {'component': 'octendra-linux-worker-sdk', 'suite': 'trixie', 'arch': 'amd64',
                     'root': str(self.root), 'sysroot': str(self.sysroot), 'packages': {'fixture': {}},
                     'signing_key_fingerprints': ['04B54C3CDCA79751B16BC6B5225629DF75B188BD'],
                     'files': {'sysroot/header.h': {'kind': 'file', 'sha256': builder.digest(self.header)}}}
        self.save()
        self.addCleanup(patch.stopall)
        patch.object(builder, 'primary', return_value=ROOT).start()
        patch.object(builder.release_current, 'primary', return_value=ROOT).start()

    def save(self):
        self.manifest.write_text(json.dumps(self.data))
        self.pin = builder.digest(self.manifest)

    def verify(self):
        return builder.verify_sdk(self.manifest, self.pin)

    def test_verified_receipt_and_bytes_are_required(self):
        self.assertEqual(self.verify()['arch'], 'amd64')
        self.header.write_bytes(b'mutated header')
        with self.assertRaisesRegex(ValueError, 'SDK bytes changed'):
            self.verify()

    def test_receipt_pin_cannot_be_replaced_with_self_asserted_metadata(self):
        self.manifest.write_text('{}')
        with self.assertRaisesRegex(ValueError, 'receipt hash changed'):
            self.verify()

    def test_unknown_arch_or_signing_anchor_is_refused(self):
        self.data['arch'] = 'arm64'; self.save()
        with self.assertRaisesRegex(ValueError, 'identity/signature'):
            self.verify()
        self.data['arch'] = 'amd64'; self.data['signing_key_fingerprints'] = []; self.save()
        with self.assertRaisesRegex(ValueError, 'identity/signature'):
            self.verify()

    def test_sdk_member_path_escape_and_root_override_are_refused(self):
        self.data['files']['../foreign'] = {'kind': 'file', 'sha256': '0' * 64}; self.save()
        with self.assertRaisesRegex(ValueError, 'unsafe SDK member'):
            self.verify()
        self.data['files'].pop('../foreign'); self.data['sysroot'] = str(self.root.parent); self.save()
        with self.assertRaisesRegex(ValueError, 'identity/signature'):
            self.verify()

    def test_symlink_parent_cannot_escape_the_sdk(self):
        foreign = self.root.parent / (self.root.name + '-foreign')
        foreign.mkdir(); self.addCleanup(lambda: foreign.rmdir())
        file = foreign / 'h'; file.write_bytes(b'outside'); self.addCleanup(file.unlink)
        (self.sysroot / 'alias').symlink_to(foreign, target_is_directory=True)
        self.data['files']['sysroot/alias/h'] = {'kind': 'file', 'sha256': builder.digest(file)}; self.save()
        with self.assertRaisesRegex(ValueError, 'SDK bytes changed'):
            self.verify()

    def test_manifest_symlink_or_writable_receipt_is_refused(self):
        alias = self.root / 'alias.json'; alias.symlink_to(self.manifest)
        with self.assertRaisesRegex(ValueError, 'owned regular'):
            builder.verify_sdk(alias, self.pin)
        self.manifest.chmod(0o666)
        with self.assertRaisesRegex(ValueError, 'owned regular'):
            self.verify()

    def test_cross_env_is_local_and_cannot_inherit_host_pkg_config_search(self):
        with patch.dict(os.environ, {'PKG_CONFIG_LIBDIR': '/foreign', 'PKG_CONFIG_PATH': '/foreign', 'CARGO_TARGET_DIR': '/foreign'}):
            env = builder.environment(self.data)
        self.assertEqual(env['CARGO_TARGET_DIR'], str(Path.home() / 'rust-target'))
        self.assertNotIn('/foreign', env['PKG_CONFIG_LIBDIR'])
        self.assertNotIn('PKG_CONFIG_PATH', env)
        self.assertEqual(env['PKG_CONFIG_ALLOW_CROSS'], '1')
        self.assertIn('--target=x86_64-unknown-linux-gnu', env['BINDGEN_EXTRA_CLANG_ARGS'])

    def test_unit_artifact_must_stay_within_the_established_linux_target(self):
        log = self.root / 'cargo.log'
        message = {'reason': 'compiler-artifact', 'profile': {'test': True},
                   'target': {'name': 'octendra-desktop-worker'}, 'executable': '/foreign/worker-test'}
        log.write_text(json.dumps(message))
        with self.assertRaisesRegex(ValueError, 'outside established target'):
            builder.test_artifacts(log, self.root)

    def test_passed_format_reuse_requires_same_membership_bytes_and_log(self):
        log = self.root / 'format.log'; log.write_text('passed')
        name = str(ROOT / 'desktop-worker/src/native.rs')
        before = {name: 'original'}
        previous = {'source_inputs': before, 'stages': {'format': {'state': 'finished', 'exit_code': 0, 'output_complete': True, 'log': str(log), 'sha256': builder.digest(log)}}}
        self.assertTrue(builder.can_reuse_format(previous, before))
        self.assertFalse(builder.can_reuse_format(previous, {name: 'changed'}))
        self.assertFalse(builder.can_reuse_format(previous, {**before, str(ROOT / 'desktop-worker/src/new.rs'): 'new'}))
        log.write_text('tampered')
        self.assertFalse(builder.can_reuse_format(previous, before))

    def test_fingerprint_includes_membership_and_mutation(self):
        self.assertNotEqual(builder.fingerprint({'a': 'x'}), builder.fingerprint({'a': 'y'}))
        self.assertNotEqual(builder.fingerprint({'a': 'x'}), builder.fingerprint({'a': 'x', 'b': 'y'}))


if __name__ == '__main__':
    unittest.main()
