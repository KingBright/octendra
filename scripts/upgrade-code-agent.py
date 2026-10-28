#!/usr/bin/env python3
"""Idempotent local upgrade. Run independently of the agent being replaced.

Use an explicitly one-shot launcher, never a KeepAlive or periodic updater.
Identical installed bytes cause no restart and do not replace the last receipt.
Installation requires stable authenticated gateway observation and actual local
poll acknowledgements. Functional release acceptance remains a separate gate.
Never prints or rewrites the device credential.
"""
import argparse
import contextlib
import errno
import fcntl
import hashlib
import json
import os
import pathlib
import re
import shutil
import sqlite3
import subprocess
import tempfile
import time
import uuid
from agent_upgrade_support import GatewayObservationError, gateway_observation, wait_ready, validate_instance_binding, renew_desktop_approval, restore_desktop_approval
from maintenance_client import MaintenanceLease
import macos_code_identity


@contextlib.contextmanager
def upgrade_lock(base):
    """Serialize decision, installation, rollback and receipt publication."""
    flags = os.O_RDWR | os.O_CREAT | getattr(os, 'O_NOFOLLOW', 0)
    fd = os.open(base / 'upgrade.lock', flags, 0o600)
    with os.fdopen(fd, 'r+b') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            if error.errno in (errno.EACCES, errno.EAGAIN):
                raise RuntimeError('another upgrade is running; no service changed') from error
            raise
        yield


def current_launchd_pid(text):
    """Read top-level launchctl fields, not nested resource-group state."""
    fields = dict(re.findall(r'^\t(state|pid) = ([^\n]+)$', text, re.MULTILINE))
    pid = fields.get('pid', '')
    return int(pid) if fields.get('state') == 'running' and pid.isdecimal() and int(pid) > 1 else None


def save_attempt(result_path, record):
    """Keep every attempt and atomically publish the latest complete receipt."""
    result_path.parent.mkdir(parents=True, exist_ok=True)
    history = result_path.parent / (result_path.stem + '-attempts')
    history.mkdir(mode=0o700, exist_ok=True)
    attempt_path = history / (uuid.uuid4().hex + '.json')
    record['attempt_receipt'] = str(attempt_path)
    payload = json.dumps(record, indent=2).encode()
    with attempt_path.open('xb') as file:
        file.write(payload)
        file.flush()
        os.fsync(file.fileno())
    with tempfile.NamedTemporaryFile(dir=result_path.parent, delete=False) as file:
        temporary = pathlib.Path(file.name)
        try:
            file.write(payload)
            file.flush()
            os.fsync(file.fileno())
            os.replace(temporary, result_path)
        finally:
            temporary.unlink(missing_ok=True)


def sha(path):
    h = hashlib.sha256()
    with path.open('rb') as file:
        for block in iter(lambda: file.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def atomic_copy(source, target):
    with tempfile.NamedTemporaryFile(dir=target.parent, delete=False) as out:
        temporary = pathlib.Path(out.name)
        try:
            with source.open('rb') as src:
                shutil.copyfileobj(src, out)
            out.flush()
            os.fsync(out.fileno())
            os.fchmod(out.fileno(), 0o755)
            os.replace(temporary, target)
        finally:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--candidate', required=True, type=pathlib.Path)
    parser.add_argument('--sha256', required=True)
    parser.add_argument('--version', required=True)
    parser.add_argument('--result', required=True, type=pathlib.Path)
    parser.add_argument('--idle-timeout', type=int, default=180)
    parser.add_argument('--instance-binding-json', help='Immutable existing-instance metadata from the one-shot launcher')
    args = parser.parse_args()
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?', args.version):
        parser.error('invalid release version')
    if not re.fullmatch(r'[0-9a-f]{64}', args.sha256):
        parser.error('sha256 must be 64 lowercase hexadecimal characters')
    if args.idle_timeout < 0:
        parser.error('idle-timeout must not be negative')
    os.umask(0o077)
    args.instance_binding = (validate_instance_binding(json.loads(args.instance_binding_json))
                             if args.instance_binding_json else None)
    base = pathlib.Path(args.instance_binding['base']) if args.instance_binding else pathlib.Path.home() / '.local/share/octendra-code'
    with upgrade_lock(base):
        perform_upgrade(args, base)


def perform_upgrade(args, base):
    binding = getattr(args, 'instance_binding', None)
    binary = pathlib.Path(binding['binary']) if binding else base / 'bin/octendra-code'
    config_path = pathlib.Path(binding['config']) if binding else base / 'agent.json'
    label = binding['service_label'] if binding else 'fun.hackerlife.octendra.agent'
    service = 'gui/%d/%s' % (os.getuid(), label)
    existing_certificate = binding.get('signing_certificate_sha1') if binding else None
    signing_base = pathlib.Path(binding['signing_base']) if binding and 'signing_base' in binding else base
    record = {'version': args.version, 'candidate_sha256': args.sha256, 'state': 'preflight'}
    backup = None
    changed = False
    old_version = None
    maintenance = None
    signed_candidate = None
    desktop_approval_update = None
    try:
        if sha(args.candidate) != args.sha256:
            raise RuntimeError('candidate checksum mismatch')
        if binding:
            validate_instance_binding(binding, installed=True)
            config = json.loads(config_path.read_text())
            if config.get('device_id') != binding['device_id']:
                raise RuntimeError('instance device identity changed; no enrollment or fallback')
        signing = (macos_code_identity.existing_status(existing_certificate) if existing_certificate
                   else macos_code_identity.status(signing_base))
        if signing.get('state') != 'ready':
            raise RuntimeError('local signing identity is not ready')
        signing_receipt = {key: signing[key] for key in
                           ('state', 'certificate_name', 'certificate_sha1', 'code_identifier', 'designated_requirement')}
        record['signing'] = signing_receipt.copy()
        if binding:
            subprocess.run(['codesign', '--verify', '--strict', '-R',
                            '='+signing['designated_requirement'], str(binary)], check=True, timeout=15)
            record['instance_binding'] = binding
        installed = sha(binary)
        installed_meta = macos_code_identity.installed_metadata(base)
        if (installed_meta and installed_meta.get('version') == args.version
                and installed_meta.get('candidate_sha256') == args.sha256
                and installed_meta.get('installed_sha256') == installed
                and installed_meta.get('certificate_sha1') == signing['certificate_sha1']):
            record.update(state='no_change', installed_sha256=installed,
                          service_changed=False, health='not_evaluated',
                          latest_receipt_preserved=args.result.exists())
            record['signing'].update(installed_sha256=installed)
            if not args.result.exists():
                save_attempt(args.result, record)
            print(json.dumps(record), flush=True)
            return
        with tempfile.NamedTemporaryFile(prefix='.signed-candidate-', dir=binary.parent, delete=False) as file:
            signed_candidate = pathlib.Path(file.name)
        signed = (macos_code_identity.sign_existing_copy(args.candidate, signed_candidate, existing_certificate)
                  if existing_certificate else macos_code_identity.sign_copy(args.candidate, signed_candidate, signing_base))
        record['signing'].update(installed_sha256=signed['installed_sha256'])
        version = subprocess.check_output([str(signed_candidate), '--version'], text=True, timeout=10).strip()
        if version != 'octendra-code ' + args.version:
            raise RuntimeError('signed candidate version mismatch')
        if not binding:
            config = json.loads(config_path.read_text())
        database = pathlib.Path(config['state_dir']) / 'state.sqlite'
        record['phase'] = 'gateway_preflight'
        remote_before = gateway_observation(config)
        record['gateway_preflight'] = remote_before.get('_transport', {})
        record['phase'] = 'candidate_validation'
        old_version = subprocess.check_output([str(binary), '--version'], text=True, timeout=10).strip().removeprefix('octendra-code ')
        subprocess.run([str(signed_candidate), 'check', '--agent', '--config', str(config_path)], check=True, stdout=subprocess.DEVNULL, timeout=15)
        record['phase'] = 'waiting_idle'
        maintenance=MaintenanceLease(config)
        maintenance.acquire(record)
        last_report=0.0
        deadline = time.monotonic() + args.idle_timeout
        while True:
            with contextlib.closing(sqlite3.connect(database.resolve().as_uri() + '?mode=ro', uri=True, timeout=5)) as db:
                active = db.execute("SELECT COUNT(*) FROM kv WHERE kind='terminal' AND json_extract(value,'$.state') IN ('running','starting')").fetchone()[0]
                busy = db.execute("SELECT COUNT(*) FROM kv WHERE kind='local_operation' AND json_extract(value,'$.state')='running'").fetchone()[0]
            if time.monotonic()-last_report>=10:
                record.update(active_terminals=active,active_operations=busy)
                maintenance.report(record);last_report=time.monotonic()
            if not active and not busy:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError('agent still has active work; upgrade not applied')
            time.sleep(1)
        backup_dir = base / 'releases' / ('before-' + args.version + '-' + time.strftime('%Y%m%dT%H%M%S') + '-' + uuid.uuid4().hex[:8])
        backup_dir.mkdir(parents=True, exist_ok=False)
        backup = backup_dir / 'octendra-code'
        shutil.copy2(binary, backup)
        record.update(backup=str(backup), previous_sha256=sha(backup))
        record['phase'] = 'installing'
        maintenance.report(record)
        if binding:
            validate_instance_binding(binding, installed=True)
        cutover_started = int(time.time())
        expected_installed_sha = record['signing']['installed_sha256']
        atomic_copy(signed_candidate, binary)
        changed = True
        if sha(binary) != expected_installed_sha:
            raise RuntimeError('installed binary does not match signed candidate checksum')
        record['desktop_approval'], desktop_approval_update = renew_desktop_approval(
            config, binary, record['previous_sha256'], expected_installed_sha)
        subprocess.run(['launchctl', 'kickstart', '-k', service], check=True, timeout=10)
        requires_lanes = tuple(int(n) for n in args.version.split('-')[0].split('+')[0].split('.')) >= (0, 3, 1)
        record['phase'] = 'verifying_readiness'
        readiness_binding = {'binding': binding} if binding else {}
        ready = wait_ready(base, config, args.version, cutover_started,
                           remote_before.get('last_seen', 0), require_lanes=requires_lanes,
                           previous_session=remote_before.get('session'), **readiness_binding)
        if sha(binary) != expected_installed_sha:
            raise RuntimeError('installed binary changed during readiness verification')
        installed_metadata = {'version': args.version, 'candidate_sha256': args.sha256,
                              'installed_sha256': expected_installed_sha,
                              'certificate_sha1': record['signing']['certificate_sha1'],
                              'code_identifier': record['signing']['code_identifier'],
                              'designated_requirement': record['signing']['designated_requirement']}
        macos_code_identity.save_installed_metadata(base, installed_metadata)
        record.update(state='upgraded', phase='completed', service_changed=True,
                      installed_sha256=expected_installed_sha, **ready)
    except Exception as error:
        record.update(state='failed', service_changed=changed, error=str(error))
        if isinstance(error, GatewayObservationError):
            record['diagnostic'] = error.diagnostic
        if changed and backup:
            try:
                if sha(backup) != record['previous_sha256']:
                    raise RuntimeError('rollback backup checksum mismatch')
                rollback_started = int(time.time())
                atomic_copy(backup, binary)
                restore_desktop_approval(desktop_approval_update)
                subprocess.run(['launchctl', 'kickstart', '-k', service], check=True, timeout=10)
                record['rollback'] = 'binary_restored; readiness_pending'
                record['rollback_readiness'] = wait_ready(base, config, old_version, rollback_started,
                    remote_before.get('last_seen', 0), require_lanes=False,
                    previous_session=remote_before.get('session'),
                    **({'binding': binding} if binding else {}))
                record['rollback'] = 'restored_previous_binary_and_verified_gateway'
            except Exception as rollback_error:
                record['rollback'] = 'failed: ' + str(rollback_error)
    if maintenance is not None:
        record["maintenance"]=maintenance.close(record)
    if signed_candidate is not None:
        signed_candidate.unlink(missing_ok=True)
    save_attempt(args.result, record)
    print(json.dumps(record), flush=True)
    if record['state'] != 'upgraded':
        raise SystemExit(1)


if __name__ == '__main__':
    main()
