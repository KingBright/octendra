"""Installed Linux instance binding. Read-only; no credentials or service writes."""
import hashlib
import json
import os
from pathlib import Path


def identity_path(config):
    return Path(config).parent.parent/'bin/upgrade-identity.json'


def exact_path(value):
    p=Path(value)
    if not p.is_absolute() or '..' in p.parts or any(x.is_symlink() for x in (p,*p.parents)):
        raise RuntimeError('instance path is not absolute/normalized or contains a symlink')
    if not p.exists():
        raise RuntimeError('bound instance path is missing')
    return p


def namespace(path):
    names=[''.join(c for c in x.lower() if c not in '-_ ') for x in path.parts]
    return 'octendra' in path.parts and not any(x=='octendracode' or x.startswith(('remotehosts','remoteplay')) for x in names)


def load_gateway(config, binary, service, pid, *, definition_root=None, proc_root=Path('/proc')):
    config=exact_path(config);binary=exact_path(binary)
    path=exact_path(identity_path(config))
    identity=json.loads(path.read_text())
    keys={'schema','role','service','executable','config','state_dir','definition','definition_sha256'}
    if set(identity)!=keys or identity['schema']!=1 or identity['role']!='gateway' or identity['service']!='octendra-gateway.service' or service!=identity['service']:
        raise RuntimeError('parallel Gateway service identity mismatch')
    root=config.parent.parent
    cfg=json.loads(config.read_text())
    state=exact_path(cfg['state_dir'])
    definition=exact_path((definition_root or Path('/etc/systemd/system'))/'octendra-gateway.service')
    if (not namespace(root) or not namespace(state) or config!=root/'config/gateway.json'
            or binary!=root/'bin/octendra-code'
            or identity['config']!=str(config) or identity['executable']!=str(binary)
            or identity['state_dir']!=str(state) or identity['definition']!=str(definition)
            or hashlib.sha256(definition.read_bytes()).hexdigest()!=identity['definition_sha256']):
        raise RuntimeError('parallel Gateway path/service-definition binding mismatch')
    if not isinstance(pid,int) or pid<=1:
        raise RuntimeError('parallel Gateway process identity required')
    proc=proc_root/str(pid)
    if os.readlink(proc/'exe')!=str(binary):
        raise RuntimeError('running Gateway executable mismatch')
    if not any(line.split(':',2)[-1].split('/').count(service) for line in (proc/'cgroup').read_text().splitlines()):
        raise RuntimeError('Gateway process is outside its bound service')
    return identity


def validate_release_paths(identity, candidate, result, backup):
    releases=Path(identity['executable']).parent/'releases'
    if Path(backup)!=releases:
        raise RuntimeError('backup directory differs from instance releases')
    for value in (candidate,result):
        path=Path(value)
        if not path.is_absolute() or '..' in path.parts or releases not in path.parents:
            raise RuntimeError('upgrade output is outside instance releases')
        if any(p.is_symlink() for p in (path,*path.parents)):
            raise RuntimeError('upgrade output contains a symlink')
