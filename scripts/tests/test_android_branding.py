import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
NS = '{http://schemas.android.com/apk/res/android}'
sys.path.insert(0, str(ROOT / 'scripts'))
import android_identity


class AndroidBrandingTests(unittest.TestCase):
    def test_manifest_components_resolve_in_migrated_namespace(self):
        manifest = ET.parse(ROOT / 'android/app/src/main/AndroidManifest.xml')
        for tag in ('activity', 'service', 'provider', 'receiver'):
            for node in manifest.iter(tag):
                name = node.attrib[NS + 'name']
                relative = 'fun.hackerlife.octendra' + name if name.startswith('.') else name
                self.assertTrue((ROOT / 'android/app/src/main/java' / (relative.replace('.', '/') + '.kt')).is_file(), relative)
        gradle = (ROOT / 'android/app/build.gradle.kts').read_text()
        self.assertIn('applicationId = "fun.hackerlife.octendra"', gradle)
        self.assertIn('namespace = "fun.hackerlife.octendra"', gradle)

    def test_installed_shell_activation_targets_retained_release_class(self):
        activation = (ROOT / 'scripts/android-activate-shell.py').read_text()
        bridge = (ROOT / 'android/app/src/main/java/fun/hackerlife/octendra/ShellBridge.kt').read_text()
        rules = (ROOT / 'android/app/proguard-rules.pro').read_text()
        for source in (activation, bridge, rules):
            self.assertIn('fun.hackerlife.octendra.ShellMain', source)
            self.assertNotIn('io.octendra.agent', source)

    def test_approved_icon_bytes_are_preserved(self):
        for original, installed in (
            ('icon-192.png', 'mipmap-nodpi/ic_launcher.png'),
            ('android-foreground.png', 'drawable-nodpi/brand_icon_foreground.png'),
            ('android-monochrome.png', 'drawable-nodpi/brand_icon_monochrome.png'),
            ('android-monochrome.png', 'drawable-nodpi/ic_agent.png'),
        ):
            self.assertEqual(
                hashlib.sha256((ROOT / 'assets/brand/octendra' / original).read_bytes()).digest(),
                hashlib.sha256((ROOT / 'android/app/src/main/res' / installed).read_bytes()).digest(),
            )

    def test_existing_signing_mode_fails_without_creating_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            env = dict(os.environ, HOME=directory)
            result = subprocess.run([sys.executable, str(ROOT / 'scripts/build-android.py'), '--existing-signing'],
                                    cwd=ROOT, env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('refusing to create a replacement', result.stderr)
            self.assertFalse((Path(directory) / '.local/share/octendra-code/android-signing').exists())

    def test_build_does_not_publish_without_runtime_acceptance(self):
        result = subprocess.run([sys.executable, str(ROOT / 'scripts/build-android.py'), '--publish'],
                                cwd=ROOT, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Build success alone cannot publish', result.stderr)

    def test_apk_identity_rejects_wrong_version_or_signing_key(self):
        identity = android_identity.release_identity(ROOT)
        badging = (f"package: name='{identity['package']}' versionCode='{identity['version_code']}' "
                   f"versionName='{identity['version']}'\n")
        signature = f"Signer #1 certificate SHA-256 digest: {android_identity.OWNER_CERT_SHA256}\n"
        android_identity.verify_apk(badging, signature, identity)
        with self.assertRaisesRegex(ValueError, 'declared release identity'):
            android_identity.verify_apk(badging.replace("versionCode='1200'", "versionCode='1'"), signature, identity)
        with self.assertRaisesRegex(ValueError, 'Owner signing identity'):
            android_identity.verify_apk(badging, signature.replace(android_identity.OWNER_CERT_SHA256, 'a' * 64), identity)

    def test_signing_alias_is_resolved_by_certificate_not_display_name(self):
        fingerprint = ':'.join(android_identity.OWNER_CERT_SHA256[i:i+2]
                               for i in range(0, 64, 2)).upper()
        listing = f"Alias name: existing-owner-entry\nEntry type: PrivateKeyEntry\n SHA256: {fingerprint}\n"
        self.assertEqual(android_identity.owner_alias(listing), 'existing-owner-entry')
        for invalid in (listing + listing, listing.replace('PrivateKeyEntry', 'trustedCertEntry'),
                        listing.replace(fingerprint, ':'.join(['AA'] * 32))):
            with self.assertRaisesRegex(ValueError, 'exactly one'):
                android_identity.owner_alias(invalid)

    def test_signing_selection_does_not_modify_material_or_echo_tool_output(self):
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            password, key = folder/'password', folder/'release.p12'
            password.write_text('synthetic-test-only'); key.write_bytes(b'synthetic-keystore')
            password.chmod(0o600); key.chmod(0o600)
            before = {p.name: (p.read_bytes(), p.stat().st_mode, p.stat().st_mtime_ns) for p in folder.iterdir()}
            failure = subprocess.CompletedProcess([], 1, 'private fixture diagnostics', 'private fixture diagnostics')
            with mock.patch.object(android_identity.subprocess, 'run', return_value=failure) as run:
                with self.assertRaisesRegex(ValueError, 'output withheld') as raised:
                    android_identity.configure_signing({}, folder)
            self.assertNotIn('private fixture diagnostics', str(raised.exception))
            self.assertNotIn('synthetic-test-only', repr(run.call_args.args))
            self.assertNotIn('-genkeypair', run.call_args.args[0])
            self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mode, p.stat().st_mtime_ns) for p in folder.iterdir()})


if __name__ == '__main__':
    unittest.main()
