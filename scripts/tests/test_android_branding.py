import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
NS = '{http://schemas.android.com/apk/res/android}'


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
            self.assertNotIn('io.remotehosts.agent', source)

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
            self.assertFalse((Path(directory) / '.local/share/remote-hosts-code/android-signing').exists())

    def test_build_does_not_publish_without_runtime_acceptance(self):
        result = subprocess.run([sys.executable, str(ROOT / 'scripts/build-android.py'), '--publish'],
                                cwd=ROOT, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Build success alone cannot publish', result.stderr)


if __name__ == '__main__':
    unittest.main()
