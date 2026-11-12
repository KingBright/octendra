"""Fast shipping preserves byte identity without silently running full test gates."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import ship


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS/filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


runner = load('fast_release_fixture', 'release-code.py')
package = load('fast_package_fixture', 'package-code-release.py')


class FastShipTests(unittest.TestCase):
    def test_build_receipt_rejects_failed_builds_and_does_not_claim_tests(self):
        manifest = {'source_inputs': {'Cargo.toml': 'abc'}}
        proof = runner.build_only_receipt(manifest, '1.2.3', ['macos', 'linux'],
            {'macos_release': {'exit_code': 0}, 'linux_release': {'exit_code': 0}})
        self.assertEqual(proof['functional_tests']['status'], 'not_run')
        self.assertFalse(proof['functional_tests']['test_gates_completed_successfully'])
        verifier = mock.Mock()
        verifier.inputs.return_value = manifest['source_inputs']
        self.assertTrue(package.receipt_current(proof, Path('.'), verifier, ['macos', 'linux'], True))
        self.assertFalse(package.receipt_current(proof, Path('.'), verifier, ['macos', 'linux']))
        verifier.inputs.return_value = {'Cargo.toml': 'changed'}
        self.assertFalse(package.receipt_current(proof, Path('.'), verifier, ['macos'], True))
        with self.assertRaises(ValueError):
            runner.build_only_receipt(manifest, '1.2.3', ['linux'], {'linux_release': {'exit_code': 1}})

    def test_default_ship_builds_quick_and_full_checks_are_opt_in(self):
        for full in (False, True):
            with self.subTest(full=full), tempfile.TemporaryDirectory() as temp:
                report = Path(temp)/'release.json'

                def build(argv, **kwargs):
                    self.assertEqual('--quick' in argv, not full)
                    self.assertEqual('--full-tests' in argv, full)
                    report.write_text(json.dumps({'state': 'passed', 'version': '1.2.3'}))
                    return mock.Mock(returncode=0)

                argv = ['ship.py', '--build-only', '--report', str(report)] + (['--full-checks'] if full else [])
                with mock.patch.object(sys, 'argv', argv), mock.patch.object(ship.subprocess, 'run', side_effect=build), contextlib.redirect_stdout(io.StringIO()):
                    ship.main()

    def publish_fixture(self, root, bad_hash=False):
        binary = b'candidate bytes'
        manifest = {'version': '1.2.3', 'build_revision': 'a'*64,
            'artifacts': {'octendra-code-linux-amd64': {
                'sha256': hashlib.sha256(binary).hexdigest(), 'size': len(binary)}}}
        payload = io.BytesIO()
        with tarfile.open(fileobj=payload, mode='w:gz') as archive:
            for name, data in [('octendra-code-linux-amd64', binary), ('manifest.json', json.dumps(manifest).encode())]:
                info = tarfile.TarInfo(name)
                info.size = len(data)
                archive.addfile(info, io.BytesIO(data))
        data = payload.getvalue()
        expected = {'bundle_size': len(data), 'bundle_sha256': hashlib.sha256(data).hexdigest(),
            'manifest_sha256': hashlib.sha256(json.dumps(manifest).encode()).hexdigest()}
        if bad_hash:
            expected['bundle_sha256'] = '0'*64
        config_file = root/'gateway.json'
        config_file.write_text(json.dumps({'bind': '127.0.0.1:18788', 'public_url': 'https://gateway.example'}))
        (root/'octendra-code').write_bytes(b'old executable')
        (root/'octendra-code').chmod(0o755)
        old = root/'releases/0.9.0'
        old.mkdir(parents=True)
        (old/'evidence').write_text('preserve')
        config = {'bin_root': str(root), 'cfg': str(config_file), 'service': 'example.service'}
        health = mock.MagicMock()
        health.__enter__.return_value = io.StringIO(json.dumps(manifest))
        with mock.patch.object(sys, 'stdin', mock.Mock(buffer=io.BytesIO(data))), \
             mock.patch('subprocess.run') as run, mock.patch('urllib.request.urlopen', return_value=health), \
             contextlib.redirect_stdout(io.StringIO()):
            ship.remote_publish(config, expected)
        self.assertEqual((root/'octendra-code').read_bytes(), binary)
        self.assertEqual(json.loads((root/'releases/current.json').read_text())['version'], '1.2.3')
        self.assertEqual((old/'evidence').read_text(), 'preserve')
        self.assertFalse((root/'releases/.current-pending').exists())
        self.assertEqual(run.call_args.args[0], ['systemctl', 'restart', 'example.service'])

    def test_publish_verifies_bytes_and_preserves_old_versions(self):
        with tempfile.TemporaryDirectory() as temp:
            self.publish_fixture(Path(temp))

    def test_bad_upload_does_not_replace_gateway(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            with self.assertRaisesRegex(ValueError, 'bundle checksum'):
                self.publish_fixture(root, bad_hash=True)
            self.assertEqual((root/'octendra-code').read_bytes(), b'old executable')


if __name__ == '__main__':
    unittest.main()
