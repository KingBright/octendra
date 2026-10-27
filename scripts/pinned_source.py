"""One explicit Git commit as temporary release input, never a worktree/fork.

The existing producer lease owns the fixed directory. Main, its index and WIP
are never changed. Publication still belongs to the primary project's authority.
"""
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile


def atomic_json(path, value):
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as file:
        temporary = Path(file.name)
        try:
            os.fchmod(file.fileno(), 0o600)
            file.write((json.dumps(value, indent=2)+'\n').encode())
            file.flush(); os.fsync(file.fileno())
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


def resolve(root, commit):
    if not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{40}|[0-9a-f]{64}', commit):
        raise ValueError('source-commit requires an exact full Git commit SHA')
    actual = subprocess.check_output(['git', 'rev-parse', '--verify', commit+'^{commit}'], cwd=root, text=True).strip()
    if actual != commit:
        raise ValueError('source-commit is not the exact commit object')
    return actual


def binding_path(root):
    return root/'target/release-slot/source-owner.json'


def source_path(root):
    return root/'target/release-slot/source'


def digest(path):
    with path.open('rb') as file:
        return hashlib.file_digest(file, 'sha256').hexdigest()


def tree(root):
    rows = {}
    for path in root.rglob('*'):
        name = path.relative_to(root).as_posix()
        if path.is_symlink():
            raise ValueError('pinned input contains a symlink: '+name)
        if path.is_file():
            rows[name] = {'sha256': digest(path), 'executable': bool(path.stat().st_mode & 0o111)}
    return rows


def prepare(root, commit, report):
    """Called only inside the original build_slot lease; no arbitrary roots."""
    commit = resolve(root, commit)
    owner = json.loads((root/'target/release-slot/owner.json').read_text())
    if owner.get('pid') != os.getpid() or owner.get('state') != 'running':
        raise ValueError('pinned source requires the original producer lease')
    destination, receipt = source_path(root), binding_path(root)
    if receipt.is_symlink():
        raise ValueError('pinned source owner record must not be a symlink')
    if destination.exists() or destination.is_symlink():
        raise ValueError('pinned source occupied; observe/recover original source-owner.json')
    if receipt.exists() and json.loads(receipt.read_text()).get('state') != 'removed':
        raise ValueError('original pinned source outcome requires observation')
    # git archive reads immutable objects, not main's working files or index.
    archive = subprocess.check_output(['git', 'archive', '--format=tar', commit], cwd=root)
    value = {'schema': 1, 'state': 'preparing', 'authority_root': str(root),
             'source_root': str(destination), 'source_commit': commit,
             'producer_commit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
             'producer_pid': os.getpid(), 'report': str(report)}
    controls = ['release-code.py', 'build_slot.py', 'build_process.py', 'pinned_source.py',
                'release_current.py', 'source_snapshot.py', 'check-code-source.py',
                'package-code-release.py', 'lifecycle_preload_gate.py']
    value['producer_inputs'] = {str(Path('scripts')/name): digest(root/'scripts'/name)
                                for name in controls if (root/'scripts'/name).is_file()}
    atomic_json(receipt, value)
    destination.mkdir(mode=0o700)
    info = destination.stat()
    value.update(device=info.st_dev, inode=info.st_ino)
    atomic_json(receipt, value)
    expected = {}
    try:
        listing = subprocess.check_output(['git', 'ls-tree', '-rz', '--full-tree', commit], cwd=root)
        for row in listing.split(b'\0'):
            if not row: continue
            header, raw = row.split(b'\t', 1)
            mode, kind, blob = header.decode().split()
            name = raw.decode('utf-8')
            path = PurePosixPath(name)
            if mode not in ('100644', '100755') or kind != 'blob' or path.is_absolute() or '..' in path.parts:
                raise ValueError('unsupported pinned Git input: '+name)
            expected[name] = (mode == '100755', blob)
        seen = set()
        with tarfile.open(fileobj=io.BytesIO(archive), mode='r:') as stream:
            for member in stream:
                name = member.name.rstrip('/')
                path = PurePosixPath(name)
                if path.is_absolute() or '..' in path.parts:
                    raise ValueError('unsafe pinned archive path')
                if member.isdir():
                    (destination/name).mkdir(parents=True, exist_ok=True)
                    continue
                if not member.isfile() or name not in expected or name in seen:
                    raise ValueError('pinned archive membership differs from Git tree')
                data = stream.extractfile(member).read()
                executable, blob = expected[name]
                algorithm = hashlib.sha1 if len(blob) == 40 else hashlib.sha256
                if algorithm(b'blob '+str(len(data)).encode()+b'\0'+data).hexdigest() != blob:
                    raise ValueError('pinned archive bytes differ from Git blob: '+name)
                output = destination/name; output.parent.mkdir(parents=True, exist_ok=True)
                with output.open('xb') as file: file.write(data)
                output.chmod(0o755 if executable else 0o644)
                seen.add(name)
        if seen != set(expected):
            raise ValueError('pinned archive omitted Git source files')
        value.update(state='ready', files=tree(destination))
        atomic_json(receipt, value)
        return receipt
    except BaseException:
        # Extraction bytes belong to this producer, but an incomplete owner
        # receipt remains explicit if the process disappears before cleanup.
        if destination.stat().st_ino == value['inode']:
            shutil.rmtree(destination)
            value.update(state='removed', cleanup='failed_extraction_removed')
            atomic_json(receipt, value)
        raise


def read(receipt, authority):
    receipt, authority = Path(receipt), Path(authority).resolve(strict=True)
    if receipt != binding_path(authority) or receipt.is_symlink():
        raise ValueError('source binding is not the primary producer record')
    if any(path.is_symlink() for path in receipt.parents):
        raise ValueError('pinned source control path changed to a symlink')
    value = json.loads(receipt.read_text())
    source = source_path(authority)
    if (value.get('schema') != 1 or value.get('state') != 'ready' or
            value.get('authority_root') != str(authority) or value.get('source_root') != str(source)
            or source.is_symlink()):
        raise ValueError('invalid pinned source binding')
    info = source.stat()
    if (info.st_dev, info.st_ino) != (value['device'], value['inode']):
        raise ValueError('pinned source directory identity changed')
    owner = json.loads((authority/'target/release-slot/owner.json').read_text())
    if owner.get('state') != 'running' or owner.get('pid') != value['producer_pid'] or owner.get('report') != value['report']:
        raise ValueError('pinned producer lease changed')
    os.kill(value['producer_pid'], 0)
    if any(not (authority/name).is_file() or digest(authority/name) != expected
           for name, expected in value['producer_inputs'].items()):
        raise ValueError('producer tooling changed during the original operation')
    observed = tree(source)
    # Verification may generate only its normal target/log scratch; these are
    # not source inputs. Bytecode is suppressed by the producer environment.
    selected = {name: row for name, row in observed.items() if not name.startswith('target/')}
    if selected != value['files']:
        raise ValueError('pinned source changed; preserve original input for inspection')
    return value


def cleanup(receipt, authority):
    value = read(receipt, authority)
    if value['producer_pid'] != os.getpid():
        raise ValueError('only the original producer removes its pinned input')
    shutil.rmtree(source_path(authority))
    value.update(state='removed', cleanup='owned_source_removed')
    atomic_json(Path(receipt), value)
    return {'state': 'removed', 'source_commit': value['source_commit'],
            'source_root': value['source_root'], 'main_worktree_changed': False}
