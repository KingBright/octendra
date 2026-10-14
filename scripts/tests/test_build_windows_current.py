"""Tiny Windows PE/ZIP fixtures; no cross compilation, downloads or live authority."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
from types import SimpleNamespace as NS
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import build_windows_current as w

SHARED = Path.home() / '.codex/skills/rust-project-lifecycle/scripts/rust_storage.py'
spec = importlib.util.spec_from_file_location('windows_fixture_storage', SHARED)
h = importlib.util.module_from_spec(spec); spec.loader.exec_module(h)
r = h.companion('rust_release')


def pe(tail=b'fixture', machine=0x8664):
    header = bytearray(64);header[:2] = b'MZ';struct.pack_into('<I', header, 60, 64)
    return bytes(header) + b'PE\0\0' + struct.pack('<H', machine) + tail


class WindowsCurrentTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory();self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve() / 'project';self.root.mkdir()
        for name in ('scripts','docs','skills/remote-hosts-agent','crates/remote-hosts-cli','crates/remote-hosts-api/src'):
            (self.root / name).mkdir(parents=True, exist_ok=True)
        for name in ('check-code-source.py','build-windows-cross.sh','build_windows_current.py'):
            shutil.copyfile(SCRIPTS / name, self.root / 'scripts' / name)
        (self.root / 'Cargo.toml').write_text('[workspace]\n')
        (self.root / 'crates/remote-hosts-cli/Cargo.toml').write_text('[package]\nversion="1.2.3"\n')
        for name in ('docs/windows.md','scripts/remote-hosts-service.ps1','crates/remote-hosts-api/src/admin.html','skills/remote-hosts-agent/SKILL.md'):
            (self.root / name).write_text('synthetic ' + name)
        self.target = self.root / 'cache';out = self.target / w.TARGET / 'release';out.mkdir(parents=True)
        for name in ('remote-hosts.exe','remote-hosts-launcher.exe'):(out / name).write_bytes(pe())
        real_lock = h.lock
        self.calls = []
        for context in [patch.object(w.shared, 'primary', side_effect=lambda root: root),
                        patch.object(w, 'target_directory', return_value=self.target),
                        patch.object(w, 'tools_ready', return_value='0.23.0'),
                        patch.object(w.shared, 'public_api', return_value=h),
                        patch.object(w.shared, 'invoke', side_effect=self.invoke),
                        patch.object(w.subprocess, 'check_output', return_value='0123456789ab\n'),
                        patch.object(h, 'lock', side_effect=lambda root, global_cleanup=False: real_lock(root, global_cleanup=False)),
                        patch.object(r, 'backend', return_value=NS(assert_idle=lambda *args, **kwargs: None))]:
            context.start();self.addCleanup(context.stop)

    def invoke(self, root, action, *values, required=True):
        self.calls.append((action, values))
        args = NS(project=str(root), authority=None, output_root=[])
        repeated = {'file','expected_sha256','aux_stage','current_sha256'}
        index = 0
        while index < len(values):
            key = values[index].removeprefix('--').replace('-', '_');index += 1
            if key in ('abort_current','adopt_current'):setattr(args,key,True);continue
            value = values[index];index += 1
            if key == 'producer_pid':value = int(value)
            if key in repeated:setattr(args,key,getattr(args,key,[]) + [value])
            else:setattr(args,key,value)
        function = {'release-setup':r.setup,'release-preflight':r.preflight,'release-admit':r.admit,'release-publish':r.publish}[action]
        try:return function(h,args)
        except Exception as error:
            if required:raise
            return {'status':'deferred','reason':str(error)}

    def runner(self, command, root, log, env, **kwargs):
        log.write_text('fake Cargo cross-build\n')
        self.command, self.env = command, env
        return {'state':'finished','exit_code':0,'raw_exit_code':0,'output_complete':True,
                'cleanup':{'state':'not_required'},'supervision_protocol':2}

    def build(self, runner=None, **kwargs):return w.build(self.root, runner=runner or self.runner, **kwargs)

    def test_three_updates_keep_one_windows_group_and_old_history(self):
        old = self.root / 'dist/remote-hosts-windows-x86_64-1.0-commit';old.mkdir(parents=True)
        (old / 'keep.txt').write_text('history')
        for iteration in range(3):
            binary = self.target / w.TARGET / 'release/remote-hosts.exe';binary.write_bytes(pe(str(iteration).encode()))
            result = self.build();self.assertTrue(result['current_committed'])
            directory, archive, checksum = w.defaults(self.root)
            record = json.loads((directory / 'release.json').read_text())
            self.assertEqual(record['artifacts'], w.inventory(directory))
            self.assertIn('remote-hosts-launcher.exe', record['artifacts'])
            self.assertIn('skills/remote-hosts-agent/SKILL.md', record['artifacts'])
            self.assertEqual(checksum.read_text(), w.shared.digest(archive) + '  ' + archive.name + '\n')
            self.assertFalse(any(path.exists() for path in w.stages(self.root)))
            self.assertFalse((self.root / 'dist/.remote-hosts-windows-x86_64-build').exists())
        self.assertEqual((old / 'keep.txt').read_text(), 'history')
        registry = h.load(self.root)[2]
        self.assertEqual(list(registry['current_components']), [w.COMPONENT]);self.assertEqual(registry['entries'], [])
        self.assertFalse((self.root / '.rust-storage/release-snapshots').exists())

    def test_invalid_pe_preserves_current_and_cleans_owned_aux(self):
        self.build();before = [h.release_signature(path) for path in w.defaults(self.root)]
        (self.target / w.TARGET / 'release/remote-hosts.exe').write_bytes(b'not PE')
        with self.assertRaisesRegex(ValueError, 'not PE'):self.build()
        self.assertEqual([h.release_signature(path) for path in w.defaults(self.root)], before)
        self.assertFalse((self.root / 'dist/.remote-hosts-windows-x86_64-build').exists())

    def test_source_drift_aborts_without_replacing_current(self):
        self.build();before = h.release_signature(w.defaults(self.root)[0])
        def runner(*args, **kwargs):
            outcome = self.runner(*args, **kwargs);(self.root / 'Cargo.toml').write_text('author update');return outcome
        with self.assertRaisesRegex(ValueError, 'main source differs'):self.build(runner)
        self.assertEqual(h.release_signature(w.defaults(self.root)[0]), before)
        self.assertEqual((self.root / 'Cargo.toml').read_text(), 'author update')

    def test_original_build_flags_and_rustflags_preserved(self):
        self.build(env={'RUSTFLAGS':'-C opt-level=2'})
        self.assertEqual(self.command[:6], ['cargo','xwin','build','--locked','--release','--target'])
        self.assertIn('remote-hosts-cli', self.command);self.assertIn('remote-hosts-launcher', self.command)
        self.assertEqual(self.command[-2:], ['--jobs','1'])
        self.assertEqual(self.env['RUSTFLAGS'], '-C opt-level=2')
        self.assertNotIn('CARGO_TARGET_DIR', self.env)

    def test_failed_build_keeps_original_exit_and_aborts_after_settled(self):
        def runner(*args, **kwargs):
            outcome = self.runner(*args, **kwargs);outcome.update(exit_code=7,raw_exit_code=7);return outcome
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(subprocess.CalledProcessError) as error:self.build(runner)
        self.assertEqual(error.exception.returncode, 7)
        self.assertNotIn('current_update', h.load(self.root)[2])

    def test_cancelled_settled_child_cleans_stage(self):
        def runner(*args, **kwargs):
            outcome = self.runner(*args, **kwargs);outcome.update(state='interrupted',cleanup={'state':'complete'});return outcome
        with self.assertRaises(KeyboardInterrupt):self.build(runner)
        self.assertNotIn('current_update', h.load(self.root)[2])

    def test_unconfirmed_child_preserves_aux_and_transaction(self):
        def runner(*args, **kwargs):
            outcome = self.runner(*args, **kwargs);outcome.update(state='interrupted',cleanup={'state':'unconfirmed'});return outcome
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(KeyboardInterrupt):self.build(runner)
        self.assertTrue((self.root / 'dist/.remote-hosts-windows-x86_64-build/build.log').exists())
        self.assertIn('current_update', h.load(self.root)[2])
        self.assertFalse(any(action == 'release-preflight' and '--abort-current' in values for action,values in self.calls))

    def test_unknown_pending_never_adopted(self):
        pending = w.stages(self.root)[0];pending.mkdir(parents=True);(pending / 'author').write_text('keep')
        with self.assertRaises(h.Refusal):self.build()
        self.assertEqual((pending / 'author').read_text(), 'keep')

    def test_alternate_output_refused_before_transaction(self):
        with self.assertRaisesRegex(ValueError, 'REMOTE_HOSTS_DIST_DIR'):self.build(env={'REMOTE_HOSTS_DIST_DIR':'dist/task-version'})
        self.assertEqual(self.calls, [])

    def test_wrong_pe_architecture_is_rejected(self):
        binary = self.target / w.TARGET / 'release/remote-hosts.exe';binary.write_bytes(pe(machine=0xaa64))
        with self.assertRaisesRegex(ValueError, 'x86_64 PE'):w.validate_pe(binary)


class BoundaryTests(unittest.TestCase):
    def test_tools_probe_never_installs_missing_target(self):
        with patch.object(w.shutil, 'which', return_value='/existing/tool'), patch.object(w.subprocess, 'check_output', side_effect=['cargo-xwin 0.23.0','aarch64-apple-darwin\n']) as run:
            with self.assertRaisesRegex(ValueError, 'does not install'):w.tools_ready(Path('/synthetic'), {})
        self.assertEqual(run.call_args_list[-1].args[0], ['rustup','target','list','--installed'])

    def test_cache_override_checks_build_directory_as_well_as_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary).resolve();target = home / 'rust-target';target.mkdir()
            base = {'target_directory':str(target),'build_directory':str(target)}
            with patch.object(Path, 'home', return_value=home), patch.object(w.subprocess, 'check_output', side_effect=[json.dumps(base), json.dumps(dict(base,build_directory='/extra'))]):
                with self.assertRaisesRegex(ValueError, 'Cache override'):w.target_directory(home, {'CARGO_BUILD_BUILD_DIR':'/extra'})


if __name__ == '__main__': unittest.main()
