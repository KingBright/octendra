"""Architecture selection only: artificial bytes, no Cargo, SSH, install or device."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import release_current


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / filename)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


runner = load('arm64_release_runner', 'release-code.py')
package = load('arm64_package', 'package-code-release.py')
fleet = load('arm64_fleet', 'fleet-upgrade.py')


class LinuxArm64Tests(unittest.TestCase):
    def device(self, arch, platform='linux'):
        return {'device_id': 'artificial-device',
                'capabilities': {'platform': platform, 'arch': arch}}

    def manifest(self):
        return {'artifacts': {
            'octendra-code-linux-amd64': {'sha256': 'a' * 64},
            'octendra-code-linux-arm64': {'sha256': 'b' * 64},
            'octendra-code-macos-arm64': {'sha256': 'c' * 64},
            'octendra-code-windows-amd64.exe': {'sha256': 'd' * 64}}}

    def test_legacy_default_and_explicit_arm64_selection(self):
        self.assertEqual(release_current.selected_platforms(), ('macos', 'linux', 'windows'))
        self.assertEqual(release_current.selected_platforms(['linux-arm64']), ('linux-arm64',))
        for value in [[], ['linux-arm64', 'linux-arm64'], ['linux-armv7']]:
            with self.assertRaises(ValueError):
                release_current.selected_platforms(value)

    def test_arm64_build_selects_only_native_contract_and_arm_target(self):
        commands = runner.build_commands(['linux-arm64'])
        self.assertEqual([name for name, _ in commands], ['macos_release', 'linux_arm64_release'])
        arm = commands[-1][1]
        self.assertEqual(arm[arm.index('--target') + 1], 'aarch64-unknown-linux-musl')
        self.assertEqual(arm.count('-p'), 2)
        self.assertNotIn('x86_64-unknown-linux-musl', arm)

    def test_package_paths_are_architecture_specific_and_opt_in(self):
        root = Path('/artificial/shared-target')
        arm = package.binary_files(root, ['linux-arm64'])
        self.assertEqual(set(arm), {'octendra-code-linux-arm64', 'octendra-linux-arm64'})
        self.assertEqual(arm['octendra-code-linux-arm64'],
                         root / 'aarch64-unknown-linux-musl/release/octendra-code')
        old = package.binary_files(root, None)
        self.assertEqual(len(old), 6)
        self.assertFalse(any('linux-arm64' in name for name in old))

    def test_linux_machine_aliases_select_matching_artifact_and_hash(self):
        for arch, suffix, digest in [('aarch64', 'arm64', 'b'), ('arm64', 'arm64', 'b'),
                                     ('x86_64', 'amd64', 'a'), ('amd64', 'amd64', 'a')]:
            with self.subTest(arch=arch):
                command = fleet.launch_command(self.device(arch), '/artificial/stage', self.manifest(), '0.12.1')
                self.assertIn('/octendra-code-linux-' + suffix, command)
                self.assertIn(digest * 64, command)

    def test_unknown_missing_and_unsupported_architectures_fail_closed(self):
        for arch, platform in [(None, 'linux'), ('', 'linux'), ('armv7l', 'linux'),
                               ('mips64', 'linux'), ('x86_64', 'macos'), ('arm64', 'windows')]:
            with self.subTest(arch=arch, platform=platform), self.assertRaises(ValueError):
                fleet.launch_command(self.device(arch, platform), '/artificial/stage', self.manifest(), '0.12.1')

    def test_missing_arm_artifact_does_not_fall_back_to_amd64(self):
        manifest = self.manifest()
        del manifest['artifacts']['octendra-code-linux-arm64']
        with self.assertRaises(ValueError):
            fleet.launch_command(self.device('aarch64'), '/artificial/stage', manifest, '0.12.1')

    def test_version_convergence_does_not_bypass_architecture_preflight(self):
        device = dict(self.device('mips64'), online=True, converged=True)
        with self.assertRaises(ValueError):
            fleet.preflight_agents([device], self.manifest())
        device['online'] = False
        fleet.preflight_agents([device], self.manifest())  # Offline remains pending, not dispatched.

    def test_elf_machine_check_rejects_renamed_amd64_and_unknown_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / 'candidate'
            header = bytearray(64)
            header[:6] = b'\x7fELF\x02\x01'
            header[18:20] = (183).to_bytes(2, 'little')
            path.write_bytes(header)
            package.verify_linux_architecture(path, 'arm64')
            with self.assertRaises(ValueError):
                package.verify_linux_architecture(path, 'amd64')
            path.write_bytes(b'MZ artificial non-ELF')
            with self.assertRaises(ValueError):
                package.verify_linux_architecture(path, 'arm64')


if __name__ == '__main__':
    unittest.main()
