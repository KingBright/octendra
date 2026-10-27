"""Thin fixed-current release adapter; shared helper owns all output transactions."""
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

import source_snapshot
import release_targets
import pinned_source

import lifecycle_preload_gate as preload_gate

COMPONENT = 'octendra-three-platform'
PLATFORMS = release_targets.PLATFORMS
DEFAULT_PLATFORMS = release_targets.DEFAULT_PLATFORMS


@contextlib.contextmanager
def admission():
    """Keep caller exceptions intact while checking the approved gate on exit."""
    body_error = None
    with preload_gate.admission():
        try:
            yield
        except BaseException as error:
            body_error = error
    if body_error is not None:
        raise body_error.with_traceback(body_error.__traceback__)


def selected_platforms(values=None):
    return release_targets.selected_platforms(values)


def primary(root):
    root = Path(root).resolve(strict=True)
    if not (root/'.git').is_dir() or (root/'.git').is_symlink():
        raise ValueError('release requires the primary main checkout, not a copied tree or linked worktree')
    branch = subprocess.check_output(['git', 'branch', '--show-current'], cwd=root, text=True).strip()
    top = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], cwd=root, text=True).strip()).resolve()
    if branch != 'main' or top != root:
        raise ValueError('release requires the project main checkout root')
    return root


def snapshot_evidence(snapshot, root, source_binding=None, authority_root=None):
    """Read archived input bytes using the current verifier, never execute a copy."""
    if source_binding:
        authority_root = primary(authority_root)
        binding = pinned_source.read(source_binding, authority_root)
        root = Path(root).resolve(strict=True)
        if root != Path(binding['source_root']) or snapshot is not None:
            raise ValueError('pinned source binding/root mismatch')
    else:
        root = primary(root)
    provider = source_snapshot.verifier(root).inputs
    if snapshot is None:
        inputs = provider(root)
        executable = source_snapshot.executable_files(root, inputs)
        manifest = {"source_inputs": inputs, "executable_files": executable,
                    "snapshot_id": source_snapshot.identity(inputs, executable),
                    "source_root": str(root), "scope": "primary main input fingerprint; no source copy"}
        if source_binding:
            manifest.update(source_binding=str(source_binding), authority_root=str(authority_root),
                            source_commit=binding['source_commit'], producer_commit=binding['producer_commit'],
                            scope='exact Git commit; producer-owned temporary input')
    else:
        try:
            manifest = source_snapshot.check(snapshot, input_provider=provider)
        except TypeError:
            manifest = source_snapshot.check(snapshot)
    if 'source_inputs' in manifest:
        check_inputs(root, manifest)
    return manifest


def check_inputs(root, manifest):
    if manifest.get('source_binding'):
        authority = primary(manifest['authority_root'])
        binding = pinned_source.read(manifest['source_binding'], authority)
        root = Path(root).resolve(strict=True)
        if root != Path(binding['source_root']) or binding['source_commit'] != manifest['source_commit']:
            raise ValueError('pinned source identity changed')
    else:
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
    preload_gate.require_admitted()
    path = Path.home()/'.codex/skills/rust-project-lifecycle/scripts/rust_storage.py'
    if not path.is_file() or path.is_symlink():
        raise ValueError('shared fixed-current lifecycle helper is unavailable; no package started')
    spec = importlib.util.spec_from_file_location('octendra_shared_storage', path)
    module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
    return module


def invoke(root, action, *args, required=True):
    try:
        with admission():
            return _invoke(root, action, *args, required=required)
    except preload_gate.Refusal as error:
        if required: raise
        return {'status': 'deferred', 'reason': str(error), 'artifacts_preserved': True}


def _invoke(root, action, *args, required=True):
    preload_gate.require_admitted()
    helper = Path.home()/'.codex/skills/rust-project-lifecycle/scripts/rust_storage.py'
    try:
        result = subprocess.run([sys.executable, '-B', str(helper), action, '--project', str(root), *args],
                                cwd=root, text=True, capture_output=True, check=False)
        raw = result.stdout.strip() or result.stderr.strip()
        try:
            report = json.loads(raw)
        except json.JSONDecodeError as error:
            # The runner bounds failure_detail. Save the complete bounded helper
            # receipt first and lead its summary with the actual final cause.
            diagnostics = root/'target/release-helper-failures'
            diagnostics.mkdir(parents=True, exist_ok=True)
            limit = 65536
            with tempfile.NamedTemporaryFile(mode='w', encoding='utf-8', dir=diagnostics,
                                             prefix=action+'-', suffix='.json', delete=False) as stream:
                json.dump({'action': action, 'exit_code': result.returncode,
                           'stdout': result.stdout[:limit], 'stderr': result.stderr[:limit],
                           'truncated': len(result.stdout) > limit or len(result.stderr) > limit}, stream)
                receipt = Path(stream.name).relative_to(root)
            tail = raw.splitlines()[-1] if raw.splitlines() else 'empty helper output'
            raise ValueError('non_json_helper:'+action+'; exit='+str(result.returncode)+
                             '; evidence='+repr(tail[:160])+'; receipt='+str(receipt)) from error
        if result.returncode or not isinstance(report, dict) or report.get('mode') == 'refused':
            raise ValueError('shared current transaction refused: '+str(report))
        return report
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        if required: raise
        return {'status': 'deferred', 'reason': str(error), 'artifacts_preserved': True}


@admission()
def preflight(root):
    invoke(root, 'release-setup', '--authority', str(root/'.rust-storage/registry.json'))
    return invoke(root, 'release-preflight')


def require_current_names(report):
    """Refuse a second current identity until the existing authority is migrated."""
    for component, record in report.get('current_components', {}).items():
        if (not component.startswith('octendra-')
                or any(not Path(name).name.startswith('octendra-') for name in record['files'])):
            raise ValueError(
                'current identity migration required in the existing lifecycle authority; '
                'preserve the registered outputs and do not admit a second current component'
            )


def files(root):
    return [root/'dist/octendra-code-current', root/'dist/octendra-code-current-bundle.tgz']


def stages(root):
    return [p.with_name('.'+p.name+'.pending') for p in files(root)]


def arguments(root, producer_pid):
    args = ['--component', COMPONENT, '--producer-pid', str(producer_pid)]
    for path in files(root): args.extend(['--file', str(path)])
    return args


@admission()
def admit(root):
    require_current_names(invoke(root, 'release-preflight'))
    directory = root/'dist'
    if any(p.is_symlink() for p in (directory, *directory.parents)):
        raise ValueError('current package directory must not traverse links')
    directory.mkdir(exist_ok=True)
    report = invoke(root, 'release-admit', *arguments(root, os.getpid()))
    if report.get('admission') != 'reserved' or report.get('staged_files') != list(map(str, stages(root))):
        raise ValueError('shared helper did not reserve the fixed current group')
    return report


@admission()
def validate_producer(root, producer_pid):
    helper = public_api()
    return helper.companion('rust_release').validate_producer(
        helper.release_api(), root, COMPONENT, files(root), producer_pid)


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024*1024), b''): value.update(block)
    return value.hexdigest()


@admission()
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
