#!/usr/bin/env python3
"""Build the independent Linux worker in primary main using the existing target.

No desktop grant, Portal request, network mutation, runtime installation or Agent
release is performed. Linux test executables are compiled, never called on Mac.
"""
import argparse
import datetime
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import time
import tomllib

import build_slot
import release_current

ROOT = Path(__file__).resolve().parents[1]
COMPONENT = 'octendra-linux-desktop-worker'
TARGET = 'x86_64-unknown-linux-gnu'
ZIG_TARGET = TARGET + '.2.41'
TOOLCHAIN = '1.99.1'
OUTPUT = ROOT / 'dist/octendra-linux-desktop-worker-current'
BUNDLE = ROOT / 'dist/octendra-linux-desktop-worker-current-bundle.tgz'


def read(path):
    return json.loads(Path(path).read_text())


def digest(path):
    return build_slot.digest(Path(path))


def primary(path):
    return release_current.primary(Path(path))


def input_files(rp):
    files = {ROOT / 'scripts/build-linux-desktop-worker.py', ROOT / 'scripts/build_slot.py',
             ROOT / 'scripts/build_process.py', ROOT / 'scripts/release_current.py',
             ROOT / 'scripts/check-desktop-worker-exits.py',
             ROOT / 'scripts/tests/test_linux_desktop_worker_build.py'}
    for directory in [ROOT / 'desktop-worker', ROOT / 'crates/octendra-desktop-protocol',
                      *[rp / name for name in ('host', 'protocol', 'remote_core', 'remote_platform')]]:
        for path in directory.rglob('*'):
            if path.is_file() and not path.is_symlink() and 'target' not in path.relative_to(directory).parts:
                if path.name in {'Cargo.toml', 'Cargo.lock'} or path.suffix in {'.rs', '.json', '.in'}:
                    files.add(path)
    return {str(path): digest(path) for path in sorted(files)}


def fingerprint(inputs):
    return hashlib.sha256(json.dumps(inputs, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def verify_sdk(path, expected_sha256):
    path = Path(path)
    release_current.primary(ROOT)
    if path.is_symlink() or not path.is_file() or path.stat().st_uid != os.getuid() or path.stat().st_mode & 0o022:
        raise ValueError('owned regular SDK manifest required')
    if digest(path) != expected_sha256:
        raise ValueError('original verified SDK receipt hash changed')
    data = read(path)
    base = path.parent.resolve()
    if (data.get('component') != 'octendra-linux-worker-sdk' or data.get('suite') != 'trixie'
            or data.get('arch') != 'amd64' or Path(data.get('root', '')).resolve() != base
            or Path(data.get('sysroot', '')).resolve() != base / 'sysroot'
            or not data.get('files') or not data.get('packages')
            or '04B54C3CDCA79751B16BC6B5225629DF75B188BD' not in data.get('signing_key_fingerprints', [])):
        raise ValueError('SDK identity/signature receipt required')
    for name, row in data['files'].items():
        relative = Path(name)
        if relative.is_absolute() or '..' in relative.parts:
            raise ValueError('unsafe SDK member')
        member = base / relative
        if row.get('kind') == 'symlink':
            if not member.is_symlink() or os.readlink(member) != row['target'] or not member.resolve().is_relative_to(base):
                raise ValueError('SDK symlink changed')
        elif member.is_symlink() or not member.resolve().is_relative_to(base) or not member.is_file() or digest(member) != row.get('sha256'):
            raise ValueError('SDK bytes changed')
    return data


def environment(sdk):
    sysroot = Path(sdk['sysroot'])
    env = os.environ.copy()
    env['CARGO_TARGET_DIR'] = str(Path.home() / 'rust-target')
    env['PKG_CONFIG_ALLOW_CROSS'] = '1'
    env.pop('PKG_CONFIG_PATH', None)
    env['PKG_CONFIG_SYSROOT_DIR'] = str(sysroot)
    env['PKG_CONFIG_LIBDIR'] = os.pathsep.join(str(sysroot / path) for path in
        ['usr/lib/x86_64-linux-gnu/pkgconfig', 'usr/share/pkgconfig'])
    includes = f'-isystem {sysroot}/usr/include -isystem {sysroot}/usr/include/x86_64-linux-gnu'
    env['CFLAGS_x86_64_unknown_linux_gnu'] = includes
    env['BINDGEN_EXTRA_CLANG_ARGS'] = f'--target={TARGET} --sysroot={sysroot} {includes}'
    env['LIBCLANG_PATH'] = '/opt/homebrew/opt/llvm/lib'
    env['CLANG_PATH'] = '/opt/homebrew/opt/llvm/bin/clang'
    return env


def patch_arguments(rp):
    args = []
    for name, path in [('host', rp / 'host'), ('protocol', rp / 'protocol'),
                       ('screencapturekit', rp / 'third_party/screencapturekit')]:
        args += ['--config', f'patch.crates-io.{name}.path="{path}"']
    return args


def test_artifacts(log, target):
    result = []
    for line in Path(log).read_text(errors='replace').splitlines():
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if (value.get('reason') == 'compiler-artifact' and value.get('profile', {}).get('test')
                and value.get('target', {}).get('name') == 'octendra-desktop-worker' and value.get('executable')):
            executable = Path(value['executable']).resolve()
            if not executable.is_relative_to(target / TARGET):
                raise ValueError('test artifact outside established target')
            result.append({'path': str(executable), 'sha256': digest(executable)})
    if not result:
        raise ValueError('Linux worker unit executable was not compiled')
    return result


def load_coordinator():
    spec = importlib.util.spec_from_file_location('owned_release_coordinator', ROOT / 'scripts/release-code.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def arguments(pid):
    return ['--component', COMPONENT, '--producer-pid', str(pid),
            '--file', str(OUTPUT), '--file', str(BUNDLE)]



def can_reuse_format(previous, inputs):
    check = previous.get('stages', {}).get('format', {})
    names = {name for name in inputs if name.startswith(str(ROOT / 'desktop-worker/src') + '/') and name.endswith('.rs')}
    old = previous.get('source_inputs', {})
    old_names = {name for name in old if name.startswith(str(ROOT / 'desktop-worker/src') + '/') and name.endswith('.rs')}
    return (bool(names) and names == old_names and all(inputs[name] == old[name] for name in names)
            and check.get('state') == 'finished' and check.get('exit_code') == 0
            and check.get('output_complete') is True and Path(check.get('log', '')).is_file()
            and digest(check['log']) == check.get('sha256'))


def run(args):
    report = Path(args.report).absolute()
    previous = None
    if report.exists():
        previous = read(report)
        if (not args.resume_failed or previous.get('state') not in {'failed', 'interrupted'}
                or build_slot.process_alive(previous.get('pid')) or previous.get('package')
                or previous.get('publication', {}).get('current_committed')
                or any(v.get('cleanup', {}).get('state') == 'unconfirmed' for v in previous.get('stages', {}).values())
                or previous.get('recovery', {}).get('status') not in {'completed'}):
            raise ValueError('original request is running/uncertain or not settled; observe, never duplicate')
    root = primary(ROOT)
    rp = primary(args.rp_root)
    sdk = verify_sdk(args.sdk_manifest, args.sdk_sha256)
    inputs = input_files(rp)
    identity = fingerprint(inputs)
    logs = report.with_name(report.stem + '-logs')
    logs.mkdir(parents=True, exist_ok=previous is not None)
    if previous is not None:
        logs = logs / ('attempt-' + str(len(previous.get('prior_attempts', [])) + 2))
        logs.mkdir(exist_ok=False)
    state = {'schema_version': 1, 'state': 'running', 'phase': 'prepare', 'pid': os.getpid(),
             'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'stages': {},
             'source_inputs': inputs, 'input_fingerprint': identity, 'execution_root': str(root),
             'rp_root': str(rp), 'target': TARGET, 'glibc_floor': '2.41', 'deployed': False,
             'sdk_manifest': str(Path(args.sdk_manifest).absolute()), 'sdk_manifest_sha256': digest(args.sdk_manifest),
             'native_execution': 'not performed; pending real target acceptance', 'portal': 'not requested'}
    if previous is not None:
        state['prior_attempts'] = previous.get('prior_attempts', []) + [{k: v for k, v in previous.items() if k != 'prior_attempts'}]
        state['request_started_at'] = previous.get('request_started_at', previous['started_at'])
        state['changed_inputs'] = sorted(name for name in inputs.keys() | previous['source_inputs'].keys() if inputs.get(name) != previous['source_inputs'].get(name))
    coordinator = load_coordinator()
    admitted = committed = False
    def save():
        state['updated_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        build_slot.atomic_json(report, state)
    def unchanged():
        if input_files(rp) != inputs:
            raise ValueError('worker input bytes/membership changed; preserve edits and stop')
    try:
        save()
        with build_slot.lease(root / 'target/release-slot', report, identity, execution_root=root):
            state['build_process_limits'] = coordinator.prepare_build_limits()
            version = subprocess.check_output(['rustc', '--version'], text=True)
            if not version.startswith('rustc ' + TOOLCHAIN + ' '):
                raise ValueError('pinned Rust toolchain required')
            env = environment(sdk)
            target = release_current.established_target(root, env)
            state['cargo_target_dir'] = str(target)
            root_fingerprint = release_current.snapshot_evidence(None, root)
            state['primary_main_input_fingerprint'] = root_fingerprint['snapshot_id']
            # Normal lifecycle entry; no registration file is edited by this script.
            preflight = release_current.invoke(root, 'release-preflight')
            release_current.require_current_names(preflight)
            state['preflight'] = preflight
            state['admission'] = release_current.invoke(root, 'release-admit', *arguments(os.getpid()))
            admitted = True
            stage, bundle_stage = [p.with_name('.' + p.name + '.pending') for p in (OUTPUT, BUNDLE)]
            if state['admission'].get('staged_files') != [str(stage), str(bundle_stage)]:
                raise ValueError('unexpected public current staging group')
            save()
            commands = [
                ('format', ['rustup', 'run', TOOLCHAIN, 'rustfmt', '--edition', '2024', '--config', 'skip_children=true', '--check', *map(str, sorted((root / 'desktop-worker/src').glob('*.rs')))]),
                ('entry_tests', [sys.executable, '-B', '-m', 'unittest', 'discover', '-s', 'scripts/tests', '-p', 'test_linux_desktop_worker_build.py', '-v']),
                ('cross_build', ['cargo', 'zigbuild', '--manifest-path', str(root / 'desktop-worker/Cargo.toml'), '--release', '--locked', '--offline', '--target', ZIG_TARGET, '--tests', '--bins', '--message-format=json-render-diagnostics', *patch_arguments(rp)]),
            ]
            for name, command in commands:
                if name == 'format' and previous is not None and can_reuse_format(previous, inputs):
                    state['stages']['format'] = {**previous['stages']['format'], 'reused_from_producer': previous['pid']}
                    save()
                else:
                    coordinator.run_stage(name, command, root, logs, report, state, env, manifest=root_fingerprint)
                unchanged()
            binary = target / TARGET / 'release/octendra-desktop-worker'
            units = test_artifacts(logs / 'cross_build.log', target)
            if binary.read_bytes()[:4] != b'\x7fELF':
                raise ValueError('Linux ELF worker required')
            state['worker'] = {'path': str(binary), 'sha256': digest(binary), 'size': binary.stat().st_size}
            state['compiled_linux_tests'] = units
            unchanged()
            stage.mkdir()
            shutil.copy2(binary, stage / 'octendra-desktop-worker')
            for name in ['approval.example.json', 'fun.hackerlife.octendra.desktop-worker.desktop.in']:
                shutil.copy2(root / 'desktop-worker' / name, stage / name)
            artifacts = {p.name: {'sha256': digest(p), 'size': p.stat().st_size} for p in stage.iterdir()}
            manifest = {**{key: state[key] for key in ['input_fingerprint', 'source_inputs', 'execution_root', 'rp_root', 'target', 'glibc_floor', 'sdk_manifest_sha256', 'native_execution', 'portal']},
                        'component': COMPONENT, 'version': tomllib.loads((root / 'desktop-worker/Cargo.toml').read_text())['package']['version'],
                        'artifacts': artifacts, 'compiled_linux_tests': units,
                        'fixed_checks': {k: {'exit_code': v['exit_code'], 'log_sha256': v['sha256']} for k, v in state['stages'].items()},
                        'permissions_enabled': False, 'runtime_libraries_bundled': False,
                        'limitations': ['Cross-build and compiled unit executables are not Linux execution evidence.', 'No desktop scope, local launch approval, Portal selection or persistent permission is enabled.', 'Native target validation and user-selected Open/Observe/Close remain required.']}
            (stage / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
            (stage / 'SHA256SUMS').write_text(''.join(digest(p) + '  ' + p.name + '\n' for p in sorted(stage.iterdir())))
            with tarfile.open(bundle_stage, 'w:gz') as archive:
                for path in sorted(stage.iterdir()):
                    archive.add(path, arcname=path.name, recursive=False)
            unchanged()
            verify_sdk(args.sdk_manifest, args.sdk_sha256)
            state['phase'] = 'publication'; save()
            state['publication'] = release_current.invoke(root, 'release-publish', *arguments(os.getpid()),
                '--validation-kind', 'unsigned-dev', '--validation-file', str(stage / 'manifest.json'),
                '--validation-digest', digest(stage / 'manifest.json'),
                '--expected-sha256', release_current.public_api().release_signature(stage), '--expected-sha256', digest(bundle_stage))
            committed = state['publication'].get('current_committed') is True
            if not committed:
                raise ValueError('public worker publication not confirmed')
            unchanged()
            state.update(state='passed', phase='finished', source_inputs_unchanged=True,
                         package={'path': str(OUTPUT), 'bundle': str(BUNDLE), 'manifest_sha256': digest(OUTPUT / 'manifest.json'),
                                  'bundle_sha256': digest(BUNDLE), 'artifacts': artifacts})
            save()
    except BaseException as error:
        state.update(state='interrupted' if isinstance(error, KeyboardInterrupt) else 'failed',
                     failure_type=type(error).__name__, failure_detail=str(error)[:1000])
        # Abort only from this same live producer after its child groups settled.
        if admitted and not committed and not state.get('worker'):
            settled = all(v.get('cleanup', {}).get('state') != 'unconfirmed' for v in state['stages'].values())
            state['recovery'] = release_current.invoke(root, 'release-preflight', '--abort-current', '--producer-pid', str(os.getpid()), required=False) if settled else {'status': 'deferred', 'reason': 'owned children not confirmed stopped'}
        if admitted and not committed and state.get('worker'):
            state['recovery'] = {'status': 'deferred', 'reason': 'verified candidate preserved; observe original current transaction'}
        save()
        raise
    print(json.dumps({'state': state['state'], 'report': str(report), 'package': state['package']}))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--rp-root', type=Path)
    p.add_argument('--sdk-manifest', type=Path)
    p.add_argument('--sdk-sha256')
    p.add_argument('--report', type=Path, required=True)
    p.add_argument('--status', action='store_true')
    p.add_argument('--resume-failed', action='store_true', help='same request only after confirmed stopped producer/children and completed lifecycle abort')
    args = p.parse_args()
    if args.status:
        state = read(args.report)
        print(json.dumps({k: state.get(k) for k in ['state', 'phase', 'pid', 'updated_at', 'worker', 'package', 'failure_detail']}))
    else:
        if args.rp_root is None or args.sdk_manifest is None or args.sdk_sha256 is None:
            p.error('--rp-root, --sdk-manifest and original --sdk-sha256 are required for the single producer run')
        run(args)


if __name__ == '__main__':
    main()
