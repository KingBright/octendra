import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import release_current


class RuntimeIdentityTests(unittest.TestCase):
    def test_verification_changes_keep_runtime_identity_but_product_changes_do_not(self):
        source = {'crates/octendra-code/src/lib.rs':'runtime', 'crates/octendra-code/tests/integration.rs':'test',
                  'android/app/src/main/Agent.kt':'android', 'android/app/src/androidTest/Checks.kt':'checks',
                  'scripts/fleet-upgrade.py':'controller', 'skills/octendra-agent/SKILL.md':'skill'}
        manifest = {'source_inputs': source, 'executable_files': ['scripts/fleet-upgrade.py']}
        original = release_current.runtime_revision(manifest)
        for name in ('crates/octendra-code/tests/integration.rs', 'android/app/src/androidTest/Checks.kt', 'scripts/fleet-upgrade.py'):
            source[name] += '-fixed'
            self.assertEqual(release_current.runtime_revision(manifest), original)
        for name in ('crates/octendra-code/src/lib.rs', 'android/app/src/main/Agent.kt', 'skills/octendra-agent/SKILL.md'):
            source[name] += '-changed'
            self.assertNotEqual(release_current.runtime_revision(manifest), original)
            source[name] = source[name].removesuffix('-changed')


if __name__ == '__main__':
    unittest.main()
