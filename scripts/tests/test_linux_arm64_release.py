"""Architecture selection only: artificial bytes, no Cargo, SSH, install or device."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

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
        linker = Path('/artificial/pinned Rust/lib/rustlib/host/bin/rust-lld')
        commands = runner.build_commands(['linux-arm64'], arm64_linker=linker)
        self.assertEqual([name for name, _ in commands], ['macos_release', 'linux_arm64_release'])
        arm = commands[-1][1]
        self.assertEqual(arm[arm.index('--target') + 1], 'aarch64-unknown-linux-musl')
        self.assertEqual(arm.count('-p'), 2)
        self.assertNotIn('x86_64-unknown-linux-musl', arm)
        self.assertEqual(arm[:2], ['cargo', 'zigbuild'])
        self.assertEqual(arm[arm.index('--config') + 1],
                         'target.aarch64-unknown-linux-musl.linker=' + json.dumps(str(linker)))
        self.assertIn('target.aarch64-unknown-linux-musl.rustflags=' +
                      json.dumps(['-C', 'link-arg=--fix-cortex-a53-843419']), arm)
        self.assertFalse(any('RUSTFLAGS' in arg or 'target-cpu=' in arg for arg in arm))

    def test_external_flag_override_fails_before_any_arm_build(self):
        for name in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
                     'CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS'):
            with mock.patch.dict(runner.os.environ, {name: 'artificial-flags'}):
                with self.assertRaisesRegex(ValueError, 'cannot prove erratum mitigation'):
                    runner.build_commands(['linux-arm64'], arm64_linker=Path('/fixture/rust-lld'))
                # Ordinary platform selection remains compatible.
                self.assertEqual(len(runner.build_commands()), 3)

    def test_legacy_builds_do_not_query_or_override_arm_linker(self):
        with mock.patch.object(runner, 'pinned_rust_lld', side_effect=AssertionError('ARM only')):
            commands = runner.build_commands()
        self.assertEqual([name for name, _ in commands],
                         ['macos_release', 'linux_release', 'windows_release'])
        self.assertFalse(any('--config' in args for _, args in commands))

    def test_arm_linker_comes_from_pinned_rust_and_missing_linker_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            linker = root/'lib/rustlib/artificial-host/bin/rust-lld'
            linker.parent.mkdir(parents=True)
            linker.write_text('artificial linker fixture; never executed')
            linker.chmod(0o700)
            replies = [str(root) + '\n', 'host: artificial-host\n']
            with mock.patch.object(runner.subprocess, 'check_output', side_effect=replies) as query:
                self.assertEqual(runner.pinned_rust_lld(), linker)
                self.assertEqual(query.call_args_list[0].args[0][:2], ['rustup', 'run'])
            linker.unlink()
            with mock.patch.object(runner.subprocess, 'check_output', side_effect=replies):
                with self.assertRaisesRegex(ValueError, 'mitigation not removed'):
                    runner.pinned_rust_lld()

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
