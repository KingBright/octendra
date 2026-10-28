import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from agent_upgrade_support import renew_desktop_approval, restore_desktop_approval


def digest(data):
    return hashlib.sha256(data).hexdigest()


class DesktopApprovalUpgradeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.root.chmod(0o700)
        self.binary = self.root / 'octendra-code'
        self.binary.write_bytes(b'verified-new-binary')
        self.binary.chmod(0o700)
        self.old = digest(b'approved-old-binary')
        self.new = digest(self.binary.read_bytes())
        self.config = {'state_dir': str(self.root), 'device_id': 'original-device'}
        self.path = self.root / 'desktop-launch.json'
        self.grant = {'enabled': True, 'observe': True, 'input': False,
                      'identity': {'device_id': 'original-device', 'owner': 'original-owner'},
                      'capability_version': 7, 'worker_path': str(self.binary),
                      'agent_sha256': self.old, 'worker_sha256': self.old,
                      'weavelo': {'target': {'peer_device_id': 'original-peer'},
                                  'gui_executable_sha256': 'c' * 64}}

    def write_grant(self):
        data = json.dumps(self.grant).encode()
        self.path.write_bytes(data)
        self.path.chmod(0o600)
        return data

    def renew(self):
        return renew_desktop_approval(self.config, self.binary, self.old, self.new)

    def test_compatible_upgrade_renews_hashes_and_version_without_expanding_scope(self):
        original = self.write_grant()
        receipt, update = self.renew()
        self.assertEqual(receipt, {'state': 'renewed', 'capability_version': 8})
        current = json.loads(self.path.read_bytes())
        expected = dict(self.grant, agent_sha256=self.new, worker_sha256=self.new,
                        capability_version=8)
        self.assertEqual(current, expected)
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)
        self.assertNotIn('original-owner', json.dumps(receipt))
        restore_desktop_approval(update)
        self.assertEqual(self.path.read_bytes(), original)

    def test_missing_or_disabled_authority_is_never_enabled(self):
        self.assertEqual(self.renew(), ({'state': 'not_configured'}, None))
        self.assertFalse(self.path.exists())
        self.grant['enabled'] = False
        original = self.write_grant()
        self.assertEqual(self.renew(), ({'state': 'disabled_preserved'}, None))
        self.assertEqual(self.path.read_bytes(), original)

    def test_changed_device_old_hash_and_installed_bytes_require_reapproval(self):
        for change, expected in [
            ({'identity': {'device_id': 'other'}}, 'desktop_approval_device_mismatch'),
            ({'agent_sha256': 'd' * 64}, 'desktop_approval_previous_agent_mismatch'),
            ({'worker_sha256': 'd' * 64}, 'desktop_approval_previous_worker_mismatch'),
        ]:
            with self.subTest(expected=expected):
                original_grant = self.grant.copy()
                self.grant.update(change)
                original = self.write_grant()
                receipt, update = self.renew()
                self.assertEqual(receipt['reason'], expected)
                self.assertIsNone(update)
                self.assertEqual(self.path.read_bytes(), original)
                self.grant = original_grant
        original = self.write_grant()
        receipt, update = renew_desktop_approval(self.config, self.binary, self.old, 'e' * 64)
        self.assertEqual(receipt['reason'], 'desktop_approval_installed_agent_mismatch')
        self.assertIsNone(update)
        self.assertEqual(self.path.read_bytes(), original)

    def test_changed_or_nonprivate_authority_is_not_overwritten(self):
        original = self.write_grant()
        self.path.chmod(0o644)
        self.assertEqual(self.renew()[0]['reason'], 'desktop_approval_not_private')
        self.assertEqual(self.path.read_bytes(), original)
        self.path.chmod(0o600)
        _, update = self.renew()
        self.path.write_bytes(b'owner-revoked-the-grant')
        with self.assertRaisesRegex(ValueError, 'desktop_approval_changed'):
            restore_desktop_approval(update)
        self.assertEqual(self.path.read_bytes(), b'owner-revoked-the-grant')

    def test_separate_unchanged_worker_retains_its_own_hash(self):
        worker = self.root / 'worker'
        worker.write_bytes(b'separate-worker')
        self.grant.update(worker_path=str(worker), worker_sha256=digest(worker.read_bytes()))
        self.write_grant()
        self.assertEqual(self.renew()[0]['state'], 'renewed')
        self.assertEqual(json.loads(self.path.read_bytes())['worker_sha256'], digest(worker.read_bytes()))


if __name__ == '__main__':
    unittest.main()
