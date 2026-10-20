"""Existing long-lived producer lease; formal entries execute primary main.

Source checkout synchronization is retired. Locks and historical metadata remain;
callers provide primary main explicitly for execution.
"""
import contextlib
import hashlib
import json
import os
import pathlib
import tempfile
import time



def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
        temporary = pathlib.Path(stream.name)
        try:
            stream.write((json.dumps(value, indent=2)+'\n').encode())
            stream.flush()
            os.fsync(stream.fileno())
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


def no_links(path):
    path = pathlib.Path(path).absolute()
    if any(p.is_symlink() for p in (path, *path.parents)):
        raise ValueError('build slot paths must not contain symlinks')
    return path


class SlotBusy(RuntimeError):
    def __init__(self, owner):
        self.owner = owner
        super().__init__('build_slot_busy: observe the original report; do not start another build')


def process_alive(pid, *, group=False):
    if not isinstance(pid, int) or pid <= 1:
        return False
    try:
        (os.killpg if group else os.kill)(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def stale_owner_recoverable(previous):
    """Recover only after the lease is ours and the recorded process group is gone."""
    if process_alive(previous.get('pid')):
        return False
    try:
        report = json.loads(pathlib.Path(previous['report']).read_text())
    except (KeyError, OSError, ValueError, TypeError):
        return False
    child = report.get('child_pid')
    stages = list(report.get('stages', {}).values()) + [report.get('build', {})]
    groups = {group for stage in stages for group in stage.get('cleanup', {}).get('remaining_groups', [])}
    return ((child is None or not process_alive(child, group=True))
            and not any(process_alive(group, group=True) for group in groups))


@contextlib.contextmanager
def lease(directory, report, snapshot_id, execution_root=None):
    if os.name != 'posix':
        raise RuntimeError('build slot locking currently requires POSIX')
    import fcntl
    directory = no_links(directory)
    directory.mkdir(parents=True, exist_ok=True)
    marker = no_links(directory/'slot.json')
    no_links(directory/'owner.json')
    # Refuse to adopt a pre-existing populated directory owned by someone else.
    if not marker.exists() and any(p.name != '.lock' for p in directory.iterdir()):
        raise ValueError('build slot directory is not managed by this runner')
    fd = os.open(directory/'.lock', os.O_RDWR | os.O_CREAT | getattr(os, 'O_NOFOLLOW', 0), 0o600)
    with os.fdopen(fd, 'r+b') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            try:
                owner = json.loads((directory/'owner.json').read_text())
            except (OSError, ValueError):
                owner = {'state': 'owner_not_yet_published'}
            raise SlotBusy(owner) from None
        if marker.exists():
            if json.loads(marker.read_text()) != {'schema_version': 1, 'purpose': 'octendra-release-slot'}:
                raise ValueError('build slot identity mismatch')
        else:
            atomic_json(marker, {'schema_version': 1, 'purpose': 'octendra-release-slot'})
        prior_owner = directory/'owner.json'
        if prior_owner.exists():
            previous = json.loads(prior_owner.read_text())
            if previous.get('state') != 'released':
                if stale_owner_recoverable(previous):
                    previous.update(state='released', released_at=int(time.time()),
                                    recovery='stale_owner_no_live_process_or_process_group')
                    atomic_json(prior_owner, previous)
                else:
                    raise ValueError('build_slot_recovery_required: previous owner did not confirm cleanup; retain its report and inspect owned child processes before reuse')
        owner = {'pid': os.getpid(), 'report': str(pathlib.Path(report).absolute()),
                 'snapshot_id': snapshot_id, 'started_at': int(time.time()), 'state': 'running'}
        atomic_json(directory/'owner.json', owner)
        try:
            # A lease without an execution root is metadata-only, never a copied checkout.
            yield no_links(execution_root).resolve(strict=True) if execution_root is not None else None
        finally:
            unsettled = False
            if execution_root is not None and pathlib.Path(report).is_file():
                try:
                    saved = json.loads(pathlib.Path(report).read_text())
                    stages = list(saved.get('stages', {}).values()) + [saved.get('build', {})]
                    unsettled = any(stage.get('cleanup', {}).get('state') == 'unconfirmed' for stage in stages)
                except (OSError, ValueError, TypeError):
                    unsettled = True
            owner.update(state='recovery_required' if unsettled else 'released', released_at=int(time.time()))
            atomic_json(directory/'owner.json', owner)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for part in iter(lambda: stream.read(1048576), b''):
            h.update(part)
    return h.hexdigest()


def synchronize(snapshot, checkout):
    """Retired; old evidence and existing checkout bytes are preserved unchanged."""
    raise ValueError('source checkout synchronization is retired; execute the primary main checkout')


def cpu_time(text):
    days = 0.0
    if '-' in text:
        day, text = text.split('-', 1)
        days = float(day)*86400
    parts = [float(x) for x in text.split(':')]
    if not 1 <= len(parts) <= 3 or any(x < 0 for x in parts):
        raise ValueError('invalid process CPU time')
    total = 0.0
    for value in parts:
        total = total*60+value
    return days+total


def descendants(table, pid):
    """Parse only process ids, parent ids and CPU counters, never command arguments."""
    rows = {}
    for line in table.splitlines():
        values = line.split()
        if len(values) != 3:
            continue
        try:
            child, parent = int(values[0]), int(values[1])
            rows[child] = (parent, cpu_time(values[2]))
        except ValueError:
            continue
    selected = {pid}
    while True:
        following = selected | {child for child, (parent, _) in rows.items() if parent in selected}
        if following == selected:
            break
        selected = following
    return {child: rows[child][1] for child in selected if child in rows}
