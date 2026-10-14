#!/usr/bin/env python3
"""Build the existing Windows CLI/launcher into one validated current group."""
import argparse
import contextlib
import datetime
import json
import os
from pathlib import Path
import re
import shutil
import signal
import struct
import subprocess
import sys
import tomllib
import zipfile

import build_process
import release_current as shared

ROOT = Path(__file__).resolve().parents[1]
TARGET = 'x86_64-pc-windows-msvc'
COMPONENT = 'remote-hosts-windows-x86_64'
NAME = 'remote-hosts-windows-x86_64-current'
CACHE_ENV = ('CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR')


def defaults(root):
    directory = root / 'dist' / NAME
    return [directory, directory.with_suffix('.zip'), directory.with_suffix('.zip.sha256')]


def stages(root):
    return [path.with_name('.' + path.name + '.pending') for path in defaults(root)]


def arguments(root):
    result = ['--component', COMPONENT, '--producer-pid', str(os.getpid())]
    for path in defaults(root): result += ['--file', str(path)]
    return result


def settings(root, env):
    if env.get('REMOTE_HOSTS_WINDOWS_TARGET', TARGET) != TARGET:
        raise ValueError('This documented Windows x86_64 component requires ' + TARGET)
    override = Path(env.get('REMOTE_HOSTS_DIST_DIR', str(root / 'dist')))
    if not override.is_absolute(): override = root / override
    if '..' in override.parts or override != root / 'dist':
        raise ValueError('REMOTE_HOSTS_DIST_DIR must remain the established project dist')
    shared.primary(root)


def target_directory(root, env):
    def metadata(values):
        return json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version=1',
                                                  '--locked', '--offline'], cwd=root, env=values, text=True))
    base = metadata({key: value for key, value in env.items() if key not in CACHE_ENV})
    effective = metadata(env)
    expected = (Path.home() / 'rust-target').resolve(strict=True)
    if Path(base['target_directory']).resolve(strict=True) != expected:
        raise ValueError('Windows builds must reuse the established ~/rust-target')
    for key in ('target_directory', 'build_directory'):
        if key not in base or key not in effective:
            if any(name in env for name in CACHE_ENV):
                raise ValueError('Cargo cannot verify the requested cache override')
            continue
        if Path(base[key]).resolve() != Path(effective[key]).resolve():
            raise ValueError('Cache override would create another target/build directory')
    return expected


def tools_ready(root, env):
    for name in ('cargo', 'rustup', 'cargo-xwin', 'nasm'):
        if not shutil.which(name, path=env.get('PATH')):
            raise ValueError('Missing existing cross-build tool: ' + name)
    text = subprocess.check_output(['cargo', 'xwin', '--version'], cwd=root, env=env, text=True)
    match = re.search(r'\b(\d+)\.(\d+)\.(\d+)\b', text)
    if not match or tuple(map(int, match.groups())) < (0, 23, 0):
        raise ValueError('cargo-xwin 0.23.0 or newer is required')
    installed = subprocess.check_output(['rustup', 'target', 'list', '--installed'], cwd=root, env=env, text=True)
    if TARGET not in installed.splitlines():
        raise ValueError('The existing toolchain needs ' + TARGET + '; this entry does not install targets or SDKs')
    return match.group(0)


def inputs(root):
    record = shared.snapshot_evidence(None, root)
    extra = {name: shared.digest(root / name) for name in
             ('scripts/build-windows-cross.sh', 'docs/windows.md')}
    return record, extra


def unchanged(root, evidence):
    record, extra = evidence
    shared.check_inputs(root, record)
    if {name: shared.digest(root / name) for name in extra} != extra:
        raise ValueError('Windows packaging inputs changed; preserve edits and stop')


def validate_pe(path):
    if path.is_symlink() or not path.is_file():
        raise ValueError('Windows binary is missing or linked: ' + str(path))
    with path.open('rb') as stream:
        header = stream.read(64)
        if len(header) != 64 or header[:2] != b'MZ':
            raise ValueError('Windows binary is not PE: ' + str(path))
        offset = struct.unpack_from('<I', header, 60)[0]
        if offset > path.stat().st_size - 6:
            raise ValueError('Windows binary has an invalid PE offset')
        stream.seek(offset)
        if stream.read(6) != b'PE\0\0\x64\x86':
            raise ValueError('Windows binary is not x86_64 PE: ' + str(path))


def inventory(directory):
    result = {}
    for path in sorted(directory.rglob('*')):
        if path.is_symlink(): raise ValueError('Linked package member is not accepted')
        if path.is_file() and path != directory / 'release.json':
            result[str(path.relative_to(directory))] = {'bytes': path.stat().st_size, 'sha256': shared.digest(path)}
    return result


def package(root, target, xwin, evidence, outcome):
    directory, archive, checksum = stages(root)
    sources = {'remote-hosts.exe': target / TARGET / 'release/remote-hosts.exe',
               'remote-hosts-launcher.exe': target / TARGET / 'release/remote-hosts-launcher.exe',
               'remote-hosts-service.ps1': root / 'scripts/remote-hosts-service.ps1',
               'admin.html': root / 'crates/remote-hosts-api/src/admin.html',
               'README-WINDOWS.md': root / 'docs/windows.md'}
    for name in ('remote-hosts.exe', 'remote-hosts-launcher.exe'): validate_pe(sources[name])
    hashes = {name: shared.digest(path) for name, path in sources.items()}
    directory.mkdir()  # The public transaction reserved this absent exact path.
    for name, source in sources.items():
        if source.is_symlink(): raise ValueError('Linked package input')
        shutil.copyfile(source, directory / name); (directory / name).chmod(0o644)
        if shared.digest(directory / name) != hashes[name] or shared.digest(source) != hashes[name]:
            raise ValueError('Package input changed while copying')
    skill = root / 'skills/remote-hosts-agent'
    if skill.is_symlink() or any(path.is_symlink() for path in skill.rglob('*')):
        raise ValueError('Linked skill package input')
    shutil.copytree(skill, directory / 'skills/remote-hosts-agent')
    unchanged(root, evidence)
    record = {'schema_version': 1, 'component': COMPONENT,
              'version': tomllib.loads((root / 'crates/remote-hosts-cli/Cargo.toml').read_text())['package']['version'],
              'commit': subprocess.check_output(['git', 'rev-parse', '--short=12', 'HEAD'], cwd=root, text=True).strip(),
              'target': TARGET, 'built_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'cargo_xwin': xwin, 'source_id': evidence[0]['snapshot_id'],
              'source_inputs': evidence[0]['source_inputs'], 'packaging_inputs': evidence[1],
              'artifacts': inventory(directory),
              'build': {key: outcome.get(key) for key in ('state', 'exit_code', 'raw_exit_code', 'output_complete', 'supervision_protocol')},
              'scope': 'Cross-build and package bytes only; no Authenticode or Windows runtime acceptance is asserted'}
    (directory / 'release.json').write_text(json.dumps(record, indent=2) + '\n')
    with zipfile.ZipFile(archive, 'x', compression=zipfile.ZIP_DEFLATED) as output:
        for path in sorted(directory.rglob('*')):
            output.write(path, str(Path(NAME) / path.relative_to(directory)))
    with zipfile.ZipFile(archive) as output:
        if output.testzip() is not None: raise ValueError('Windows ZIP checksum validation failed')
        actual = {name for name in output.namelist() if not name.endswith('/')}
        expected = {NAME + '/' + name for name in record['artifacts']} | {NAME + '/release.json'}
        if actual != expected: raise ValueError('Windows ZIP member set differs from manifest')
        import hashlib
        for name in expected:
            relative = Path(name).relative_to(NAME)
            with output.open(name) as source:
                digest = hashlib.file_digest(source, 'sha256').hexdigest()
            if digest != shared.digest(directory / relative): raise ValueError('Windows ZIP member changed')
    checksum.write_text(shared.digest(archive) + '  ' + defaults(root)[1].name + '\n')
    if inventory(directory) != record['artifacts']: raise ValueError('Windows package changed before publication')
    for name, path in sources.items():
        if shared.digest(path) != hashes[name]: raise ValueError('Windows artifact changed during packaging')
    unchanged(root, evidence)
    return record


@contextlib.contextmanager
def cancellation():
    def stop(_signal, _frame): raise KeyboardInterrupt
    previous = {sig: signal.signal(sig, stop) for sig in (signal.SIGINT, signal.SIGTERM)}
    try: yield
    finally:
        for sig, handler in previous.items(): signal.signal(sig, handler)


def build(root=ROOT, env=None, runner=build_process.run):
    env = dict(os.environ if env is None else env)
    settings(root, env)
    target = target_directory(root, env)
    xwin = tools_ready(root, env)
    evidence = inputs(root)
    before = shared.preflight(root)
    if before.get('status') != 'completed': raise ValueError('Current recovery is deferred; preserve and inspect the original producer')
    dist = root / 'dist'
    if any(path.is_symlink() for path in (dist, *dist.parents)): raise ValueError('Linked output parent')
    dist.mkdir(exist_ok=True)
    auxiliary = dist / '.remote-hosts-windows-x86_64-build'
    admitted = False; settled = True
    with cancellation():
        try:
            reservation = shared.invoke(root, 'release-admit', *arguments(root), '--aux-stage', str(auxiliary))
            admitted = True
            if (reservation.get('admission') != 'reserved' or reservation.get('staged_files') != list(map(str, stages(root)))
                    or reservation.get('owned_aux_stages') != [str(auxiliary)]):
                raise ValueError('Shared helper did not reserve the exact Windows current group')
            command = ['cargo', 'xwin', 'build', '--locked', '--release', '--target', TARGET,
                       '-p', 'remote-hosts-cli', '-p', 'remote-hosts-launcher', '--bins', '--jobs', '1']
            settled = False
            outcome = runner(command, root, auxiliary / 'build.log', dict(env, CARGO_BUILD_JOBS='1'), idle_timeout=1800)
            settled = outcome.get('cleanup', {}).get('state') in ('complete', 'not_required')
            if outcome.get('state') == 'interrupted': raise KeyboardInterrupt
            if outcome.get('state') != 'finished' or outcome.get('exit_code') != 0 or not outcome.get('output_complete') or not settled:
                path = auxiliary / 'build.log'
                if path.is_file():
                    with path.open('rb') as stream:
                        stream.seek(max(0, path.stat().st_size - 32768))
                        print(stream.read().decode('utf-8', errors='replace'), file=sys.stderr)
                raise subprocess.CalledProcessError(outcome.get('exit_code') or 1, command)
            unchanged(root, evidence)
            record = package(root, target, xwin, evidence, outcome)
            directory, archive, checksum = stages(root)
            args = arguments(root) + ['--validation-kind', 'unsigned-dev', '--validation-file', str(directory / 'release.json'),
                    '--validation-digest', shared.digest(directory / 'release.json'),
                    '--expected-sha256', shared.public_api().release_signature(directory),
                    '--expected-sha256', shared.digest(archive), '--expected-sha256', shared.digest(checksum)]
            result = shared.invoke(root, 'release-publish', *args)
            if result.get('current_committed') is not True: raise ValueError('Windows current publication was not confirmed')
            return dict(result, component=COMPONENT, version=record['version'], files=list(map(str, defaults(root))))
        finally:
            if admitted:
                recovery = shared.abort(root) if settled else {'status': 'deferred', 'reason': 'owned child cleanup unconfirmed'}
                if recovery.get('status') == 'deferred': print(json.dumps({'current_recovery': recovery}), file=sys.stderr)


def main():
    argparse.ArgumentParser(description=__doc__).parse_args()
    try: print(json.dumps(build())); return 0
    except KeyboardInterrupt: return 130
    except subprocess.CalledProcessError as error: return error.returncode if error.returncode > 0 else 128 - error.returncode
    except (OSError, ValueError, RuntimeError) as error: print(str(error), file=sys.stderr); return 2


if __name__ == '__main__': sys.exit(main())
