"""Thin fixed-current release adapter; shared helper owns all output transactions."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

import source_snapshot

COMPONENT = 'remote-hosts-three-platform'
PLATFORMS = ('macos', 'linux', 'windows')


def selected_platforms(values=None):
    selected = PLATFORMS if values is None else tuple(values)
    if (not selected or len(set(selected)) != len(selected)
            or not set(selected).issubset(PLATFORMS)):
        raise ValueError('invalid release platform selection')
    return tuple(name for name in PLATFORMS if name in selected)


def primary(root):
    root = Path(root).resolve(strict=True)
    if not (root/'.git').is_dir() or (root/'.git').is_symlink():
        raise ValueError('release requires the primary main checkout, not a copied tree or linked worktree')
    branch = subprocess.check_output(['git', 'branch', '--show-current'], cwd=root, text=True).strip()
    top = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], cwd=root, text=True).strip()).resolve()
    if branch != 'main' or top != root:
        raise ValueError('release requires the project main checkout root')
    return root


def snapshot_evidence(snapshot, root):
    """Read archived input bytes using the current verifier, never execute a copy."""
    root = primary(root)
    provider = source_snapshot.verifier(root).inputs
    if snapshot is None:
        inputs = provider(root)
        executable = source_snapshot.executable_files(root, inputs)
        manifest = {"source_inputs": inputs, "executable_files": executable,
                    "snapshot_id": source_snapshot.identity(inputs, executable),
                    "source_root": str(root), "scope": "primary main input fingerprint; no source copy"}
    else:
        try:
            manifest = source_snapshot.check(snapshot, input_provider=provider)
        except TypeError:
            manifest = source_snapshot.check(snapshot)
    if 'source_inputs' in manifest:
        check_inputs(root, manifest)
    return manifest


def check_inputs(root, manifest):
    root = primary(root)
    inputs = source_snapshot.verifier(root).inputs(root)
    executable = source_snapshot.executable_files(root, inputs)
    if (inputs != manifest['source_inputs'] or executable != manifest['executable_files']
            or source_snapshot.identity(inputs, executable) != manifest['snapshot_id']):
        raise ValueError('main source differs from declared snapshot evidence; preserve edits and stop')
    return manifest


def established_target(root, env=None):
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--format-version', '1', '--no-deps', '--locked', '--offline'],
        cwd=root, env=env, text=True))
    target = Path(metadata['target_directory']).resolve(strict=True)
    if target != (Path.home()/'rust-target').resolve(strict=True):
        raise ValueError('release must reuse the established ~/rust-target; no slot or task target')
    return target


def public_api():
    path = Path.home()/'.codex/skills/rust-project-lifecycle/scripts/rust_storage.py'
    if not path.is_file() or path.is_symlink():
        raise ValueError('shared fixed-current lifecycle helper is unavailable; no package started')
    spec = importlib.util.spec_from_file_location('remote_hosts_shared_storage', path)
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module


def invoke(root, action, *args, required=True):
    helper = Path.home()/'.codex/skills/rust-project-lifecycle/scripts/rust_storage.py'
    try:
        result = subprocess.run([sys.executable, '-B', str(helper), action, '--project', str(root), *args],
                                cwd=root, text=True, capture_output=True, check=False)
        raw = result.stdout.strip() or result.stderr.strip()
        try:
            report = json.loads(raw)
        except json.JSONDecodeError as error:
            raise ValueError('shared current helper returned a non-JSON receipt; exit='+str(result.returncode)+'; evidence='+repr(raw[:4096])) from error
        if result.returncode or not isinstance(report, dict) or report.get('mode') == 'refused':
            raise ValueError('shared current transaction refused: '+str(report))
        return report
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        if required: raise
        return {'status': 'deferred', 'reason': str(error), 'artifacts_preserved': True}


def preflight(root):
    invoke(root, 'release-setup', '--authority', str(root/'.rust-storage/registry.json'))
    return invoke(root, 'release-preflight')


def files(root):
    return [root/'dist/remote-hosts-code-current', root/'dist/remote-hosts-code-current-bundle.tgz']


def stages(root):
    return [p.with_name('.'+p.name+'.pending') for p in files(root)]


def arguments(root, producer_pid):
    args = ['--component', COMPONENT, '--producer-pid', str(producer_pid)]
    for path in files(root): args.extend(['--file', str(path)])
    return args


def admit(root):
    directory = root/'dist'
    if any(p.is_symlink() for p in (directory, *directory.parents)):
        raise ValueError('current package directory must not traverse links')
    directory.mkdir(exist_ok=True)
    report = invoke(root, 'release-admit', *arguments(root, os.getpid()))
    if report.get('admission') != 'reserved' or report.get('staged_files') != list(map(str, stages(root))):
        raise ValueError('shared helper did not reserve the fixed current group')
    return report


def validate_producer(root, producer_pid):
    helper = public_api()
    return helper.companion('rust_release').validate_producer(
        helper.release_api(), root, COMPONENT, files(root), producer_pid)


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024*1024), b''): value.update(block)
    return value.hexdigest()


def publish(root):
    stage, bundle = stages(root)
    receipt = stage/'manifest.json'
    args = arguments(root, os.getpid()) + ['--validation-kind', 'unsigned-dev',
            '--validation-file', str(receipt), '--validation-digest', digest(receipt),
            '--expected-sha256', public_api().release_signature(stage),
            '--expected-sha256', digest(bundle)]
    report = invoke(root, 'release-publish', *args)
    if report.get('current_committed') is not True:
        raise ValueError('shared helper did not confirm current publication')
    return report


def abort(root):
    return invoke(root, 'release-preflight', '--abort-current', '--producer-pid', str(os.getpid()), required=False)
