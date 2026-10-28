"""Runtime observations and authority-preserving support for verified Agent upgrades."""
import contextlib
import errno
import http.client
import hashlib
import socket
import ssl
import urllib.error
import json
import os
import pathlib
import plistlib
import re
import sqlite3
import stat
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request
import uuid

LANES = {'read', 'write', 'transfer', 'terminal', 'control'}


def _desktop_approval_bytes(path):
    owner = os.getuid()
    parent = path.parent.lstat()
    info = path.lstat()
    if (not stat.S_ISDIR(parent.st_mode) or path.parent.is_symlink()
            or parent.st_uid != owner or stat.S_IMODE(parent.st_mode) != 0o700
            or not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
            or info.st_uid != owner or stat.S_IMODE(info.st_mode) != 0o600
            or info.st_size > 16384):
        raise ValueError('desktop_approval_not_private')
    with path.open('rb') as source:
        actual = os.fstat(source.fileno())
        if (actual.st_dev, actual.st_ino) != (info.st_dev, info.st_ino):
            raise ValueError('desktop_approval_changed')
        return source.read(16385)


def _replace_desktop_approval(path, expected, replacement):
    if _desktop_approval_bytes(path) != expected:
        raise ValueError('desktop_approval_changed')
    with tempfile.NamedTemporaryFile(dir=path.parent, prefix='.octendra-desktop-', delete=False) as output:
        temporary = pathlib.Path(output.name)
        try:
            os.fchmod(output.fileno(), 0o600)
            output.write(replacement); output.flush(); os.fsync(output.fileno())
            if _desktop_approval_bytes(path) != expected:
                raise ValueError('desktop_approval_changed')
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


def renew_desktop_approval(config, binary, previous_sha256, installed_sha256):
    """Rebind an existing grant only after the updater verifies its installed artifact.

    No grant is created or enabled. Device, target, input consent and worker path
    stay unchanged. The original bytes remain in memory for the existing updater's
    failure recovery; they are never included in the public upgrade receipt.
    """
    path = pathlib.Path(config['state_dir']) / 'desktop-launch.json'
    if not path.exists() and not path.is_symlink():
        return {'state': 'not_configured'}, None
    try:
        original = _desktop_approval_bytes(path)
        grant = json.loads(original)
        if grant.get('enabled') is not True or grant.get('observe') is not True:
            return {'state': 'disabled_preserved'}, None
        if grant.get('identity', {}).get('device_id') != config['device_id']:
            raise ValueError('desktop_approval_device_mismatch')
        version = grant.get('capability_version')
        if type(version) is not int or not 0 < version < 2**64 - 1:
            raise ValueError('desktop_approval_version_invalid')
        if grant.get('agent_sha256') != previous_sha256:
            raise ValueError('desktop_approval_previous_agent_mismatch')
        binary = pathlib.Path(binary)
        if hashlib.sha256(binary.read_bytes()).hexdigest() != installed_sha256:
            raise ValueError('desktop_approval_installed_agent_mismatch')
        worker = pathlib.Path(grant['worker_path'])
        if worker.is_symlink() or not worker.is_file():
            raise ValueError('desktop_approval_worker_untrusted')
        if worker.samefile(binary):
            if grant.get('worker_sha256') != previous_sha256:
                raise ValueError('desktop_approval_previous_worker_mismatch')
            grant['worker_sha256'] = installed_sha256
        elif hashlib.sha256(worker.read_bytes()).hexdigest() != grant.get('worker_sha256'):
            raise ValueError('desktop_approval_worker_changed')
        grant['agent_sha256'] = installed_sha256
        grant['capability_version'] = version + 1
        replacement = (json.dumps(grant, indent=2) + '\n').encode()
        if len(replacement) > 16384:
            raise ValueError('desktop_approval_too_large')
        _replace_desktop_approval(path, original, replacement)
        return ({'state': 'renewed', 'capability_version': version + 1},
                {'path': path, 'original': original, 'replacement': replacement})
    except (OSError, ValueError, KeyError, TypeError) as error:
        reason = str(error) if isinstance(error, ValueError) and str(error).startswith('desktop_approval_') else 'desktop_approval_unavailable'
        return {'state': 'requires_reapproval', 'reason': reason}, None


def restore_desktop_approval(update):
    if update is not None:
        _replace_desktop_approval(update['path'], update['replacement'], update['original'])


def validate_instance_binding(binding, *, installed=False):
    """Bind an existing macOS installation using public metadata, never config bytes.

    This is an explicit layout, not enrollment or discovery. The caller supplies
    the already-known device ID, original binary/definition hashes and signing
    home. There is no host list, path fallback or creation of signing material.
    """
    fields = {'schema', 'base', 'binary', 'config', 'service_definition',
              'service_label', 'device_id', 'binary_sha256',
              'definition_sha256'}
    if (not isinstance(binding, dict) or binding.get('schema') != 1 or
            set(binding) not in (fields | {'signing_base'}, fields | {'signing_certificate_sha1'})):
        raise ValueError('invalid instance binding fields')
    if 'signing_certificate_sha1' in binding and not re.fullmatch(r'[0-9a-fA-F]{40}', binding['signing_certificate_sha1']):
        raise ValueError('invalid existing signing certificate fingerprint')
    if not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._-]{0,127}', binding['service_label']):
        raise ValueError('invalid instance service label')
    if str(uuid.UUID(binding['device_id'])) != binding['device_id']:
        raise ValueError('invalid instance device ID')
    for key in ('binary_sha256', 'definition_sha256'):
        if not re.fullmatch(r'[0-9a-f]{64}', binding[key]):
            raise ValueError('invalid instance checksum')
    paths = {}
    path_fields = ['base', 'binary', 'config', 'service_definition']
    if 'signing_base' in binding:
        path_fields.append('signing_base')
    for key in path_fields:
        path = pathlib.Path(binding[key])
        if not path.is_absolute() or path.resolve(strict=True) != path:
            raise ValueError('instance paths must be explicit canonical existing paths')
        info = path.lstat()
        if info.st_uid != os.getuid() or info.st_mode & 0o022:
            raise ValueError('instance path ownership or write permissions unsafe')
        if key in ('base', 'signing_base'):
            if not stat.S_ISDIR(info.st_mode):
                raise ValueError('instance directory required')
        elif not stat.S_ISREG(info.st_mode):
            raise ValueError('instance regular file required')
        if key == 'config' and (info.st_mode & 0o077 or info.st_size > 65536):
            raise ValueError('instance private configuration mode or size unsafe')
        paths[key] = path
    for key in ('binary', 'config'):
        if not paths[key].is_relative_to(paths['base']):
            raise ValueError('instance binary/config must remain below explicit base')
        parent = paths[key].parent
        while parent != paths['base']:
            info = parent.stat()
            if info.st_uid != os.getuid() or info.st_mode & 0o022:
                raise ValueError('instance ancestor ownership or write permissions unsafe')
            parent = parent.parent
    info = paths['service_definition'].parent.stat()
    if info.st_uid != os.getuid() or info.st_mode & 0o022:
        raise ValueError('instance service directory ownership or write permissions unsafe')
    fd = os.open(paths['service_definition'], os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as file:
        definition = file.read(65537)
    if (len(definition) > 65536 or
            hashlib.sha256(definition).hexdigest() != binding['definition_sha256']):
        raise ValueError('instance service definition changed')
    spec = plistlib.loads(definition)
    if (spec.get('Label') != binding['service_label'] or
            spec.get('ProgramArguments') != [binding['binary'], 'agent', '--config', binding['config']]):
        raise ValueError('instance service does not bind the selected binary/config')
    if installed:
        with paths['binary'].open('rb') as file:
            digest = hashlib.file_digest(file, 'sha256').hexdigest()
        if digest != binding['binary_sha256']:
            raise ValueError('instance installed binary changed')
    return dict(binding)


def instance_dispatcher_label(binding):
    identity = json.dumps({key: binding[key] for key in ('base', 'service_label')}, sort_keys=True)
    return 'fun.hackerlife.octendra.upgrade.' + hashlib.sha256(identity.encode()).hexdigest()[:16]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class CandidateProcessError(RuntimeError):
    """The selected candidate process stopped or changed identity during readiness."""


class GatewayObservationError(RuntimeError):
    """Only allowlisted diagnostics, never request URLs, headers or exception text."""
    def __init__(self, diagnostic):
        self.diagnostic = diagnostic
        super().__init__('gateway_readiness_unavailable: '+diagnostic['category']+
                         '; attempts='+str(diagnostic.get('attempts', 1))+
                         '; next_action='+diagnostic['next_action'])


def gateway_error(error):
    result = {'stage': 'gateway_readiness', 'category': 'unexpected_client_error',
              'retryable': False, 'next_action': 'inspect_updater_environment'}
    if isinstance(error, urllib.error.HTTPError):
        result.update(category='http_rejected', http_status=error.code,
                      retryable=error.code in (408, 425, 500, 502, 503, 504),
                      next_action='inspect_gateway_or_edge_status')
        if error.code in (401, 403):
            result['next_action'] = 'review_gateway_auth_or_edge_policy; do_not_change_credentials_automatically'
        if error.code == 429:
            result['next_action'] = 'respect_server_rate_limit_before_retry'
        ray = error.headers.get('CF-Ray') if error.headers else None
        if ray and re.fullmatch(r'[A-Za-z0-9-]{1,80}', ray):
            result['cf_ray'] = ray
        error.close()
        return result
    reason = error.reason if isinstance(error, urllib.error.URLError) else error
    if isinstance(reason, ssl.SSLCertVerificationError):
        result.update(category='tls_certificate', next_action='inspect_trust_store_and_certificate; keep_verification_enabled')
        code = getattr(reason, 'verify_code', None)
        if isinstance(code, int): result['verify_code'] = code
    elif isinstance(reason, ssl.SSLError):
        result.update(category='tls_handshake', retryable=True,
                      next_action='retry_same_readonly_probe_with_backoff')
    elif isinstance(reason, (TimeoutError, socket.timeout)):
        result.update(category='network_timeout', retryable=True, next_action='retry_same_readonly_probe_with_backoff')
    elif isinstance(reason, socket.gaierror):
        result.update(category='dns_resolution', retryable=reason.errno == socket.EAI_AGAIN,
                      next_action='inspect_configured_gateway_dns')
    elif isinstance(reason, (http.client.IncompleteRead, http.client.RemoteDisconnected)):
        result.update(category='incomplete_response', retryable=True, next_action='retry_same_readonly_probe_with_backoff')
    elif isinstance(reason, OSError):
        transient = {errno.ECONNRESET, errno.ECONNREFUSED, errno.ECONNABORTED,
                     errno.ENETUNREACH, errno.EHOSTUNREACH, errno.ETIMEDOUT, errno.EPIPE}
        result.update(category='network_connection', retryable=reason.errno in transient,
                      next_action='inspect_network_connectivity')
        if isinstance(reason.errno, int): result['errno'] = reason.errno
    elif isinstance(reason, (ValueError, UnicodeError)):
        result.update(category='invalid_response', next_action='inspect_gateway_response_without_disclosing_body')
    return result


def gateway_observation(config, *, attempts=3, request_timeout=5):
    if type(attempts) is not int or not 1 <= attempts <= 3 or not 0 < request_timeout <= 10:
        raise ValueError('invalid readiness retry bounds')
    origin = config['gateway_url'].rstrip('/')
    parsed = urllib.parse.urlsplit(origin)
    if (parsed.scheme != 'https' or not parsed.hostname or parsed.username or parsed.password
            or parsed.query or parsed.fragment or parsed.path not in ('', '/')):
        raise RuntimeError('readiness requires a configured HTTPS gateway origin')
    opener = urllib.request.build_opener(NoRedirect())
    start = time.monotonic()
    for attempt in range(1, attempts+1):
        request = urllib.request.Request(origin + '/device/readiness', headers={
            'Authorization': 'Bearer ' + config['device_token'],
            'User-Agent': 'Octendra-Updater/0.3.1', 'Accept': 'application/json',
            'Cache-Control': 'no-cache',
        })
        try:
            with opener.open(request, timeout=request_timeout) as response:
                body = response.read(32769)
                if len(body) > 32768:
                    raise ValueError('oversized readiness')
                value = json.loads(body)
        except Exception as error:
            diagnostic = gateway_error(error)
            diagnostic.update(attempts=attempt, elapsed_ms=int((time.monotonic()-start)*1000))
            if not diagnostic['retryable'] or attempt == attempts:
                raise GatewayObservationError(diagnostic) from None
            time.sleep(min(0.5 * 2**(attempt-1), 2))
            continue
        if (not isinstance(value, dict) or value.get('readiness_protocol') != 1
                or value.get('device_id') != config['device_id']):
            raise GatewayObservationError({'stage':'gateway_readiness',
                'category':'identity_or_protocol_mismatch', 'retryable':False,
                'attempts':attempt, 'next_action':'verify_original_device_and_gateway_mapping'})
        value['_transport'] = {'attempts':attempt, 'elapsed_ms':int((time.monotonic()-start)*1000)}
        return value


def local_observation(base, binding=None):
    config_path = pathlib.Path(binding['config']) if binding else base / 'agent.json'
    config = json.loads(config_path.read_text())
    database = pathlib.Path(config['state_dir']) / 'state.sqlite'
    label = binding['service_label'] if binding else 'fun.hackerlife.octendra.agent'
    service = 'gui/%d/%s' % (os.getuid(), label)
    process = subprocess.run(['launchctl', 'print', service], text=True, capture_output=True, timeout=5)
    fields = dict(re.findall(r'^\t(state|pid|runs) = ([^\n]+)$', process.stdout, re.MULTILINE))
    pid = int(fields['pid']) if fields.get('pid', '').isdecimal() else None
    with contextlib.closing(sqlite3.connect(database.resolve().as_uri() + '?mode=ro', uri=True, timeout=5)) as db:
        rows = db.execute("SELECT key,value FROM kv WHERE kind='runtime' AND key IN ('build','readiness')").fetchall()
    runtime = {key: json.loads(value) for key, value in rows}
    return {'running': process.returncode == 0 and fields.get('state') == 'running' and bool(pid and pid > 1),
            'pid': pid, 'runs': fields.get('runs'), **runtime}


def sample_identity(local, remote, version, after, gateway_baseline, require_lanes=True,
                    now=None, previous_session=None):
    now = int(time.time()) if now is None else now
    build = local.get('build', {})
    seen, observed = remote.get('last_seen'), remote.get('observed_at')
    started = build.get('started_at')
    # A declared ready flag is insufficient. Use the gateway's own time for its
    # lease age, and tolerate a small clock offset without accepting stale caches.
    if not (type(seen) is int and type(observed) is int and type(started) is int
            and 0 <= observed-seen < 45 and abs(now-observed) <= 90
            and after <= started <= now and local.get('running') is True
            and local.get('pid') == build.get('pid') and build.get('version') == version
            and remote.get('ready') is True and remote.get('agent_version') == version
            and seen > gateway_baseline and isinstance(remote.get('session'), str)
            and len(remote['session']) == 64 and remote['session'] != previous_session):
        return None
    if require_lanes:
        ready = local.get('readiness', {})
        lanes = ready.get('lanes', {})
        if not (isinstance(lanes, dict) and ready.get('pid') == local['pid']
                and ready.get('version') == version and ready.get('session') == remote['session']
                and ready.get('phase') == 'polling' and LANES.issubset(lanes)
                and all(type(lanes[lane]) is int and after <= lanes[lane] <= now
                        and now-lanes[lane] <= 45 for lane in LANES)):
            return None
    return local['pid'], remote['session'], local.get('runs')


def _same_local_candidate(local, identity, version, after):
    if identity is None:
        return False
    build = local.get('build', {})
    ready = local.get('readiness', {})
    return (local.get('running') is True and local.get('pid') == identity[0]
            and build.get('pid') == identity[0] and build.get('version') == version
            and type(build.get('started_at')) is int and build['started_at'] >= after
            and ready.get('pid') == identity[0] and ready.get('session') == identity[1]
            and ready.get('version') == version)


def wait_ready(base, config, version, after, gateway_baseline, timeout=240,
               stable_seconds=15, require_lanes=True, previous_session=None,
               transient_grace_seconds=45, dead_process_grace_seconds=10, binding=None):
    """Require stable identity and continued poll progress without flapping on brief outages.

    Cached samples never count as progress. Once a candidate identity has been
    observed, a bounded transient transport/poll gap may preserve that identity;
    final success still requires fresh gateway and lane advancement afterwards.
    A genuinely stopped/replaced candidate fails much earlier than the overall
    network grace window, while rollback remains the caller's responsibility.
    """
    if (timeout <= stable_seconds or stable_seconds < 1
            or transient_grace_seconds < 0 or dead_process_grace_seconds < 1):
        raise ValueError('invalid readiness time bounds')
    room = max(1, timeout-stable_seconds-1)
    transient_grace_seconds = min(transient_grace_seconds, room)
    dead_process_grace_seconds = min(dead_process_grace_seconds, room)
    deadline = time.monotonic() + timeout
    stable_since = previous = first_seen = None
    first_lanes = {}
    samples = 0
    last_error = 'not_ready'
    interruption_since = dead_since = None
    transient_interruptions = 0
    max_transient_gap = 0.0
    last_transient_category = None
    while time.monotonic() < deadline:
        now_mono = time.monotonic()
        try:
            local = local_observation(base, binding) if binding else local_observation(base)
            if previous is not None and local.get('running') is not True:
                dead_since = dead_since or now_mono
                last_error = 'candidate_process_not_running'
                if now_mono-dead_since >= dead_process_grace_seconds:
                    raise CandidateProcessError('readiness_candidate_not_running')
                time.sleep(1)
                continue
            dead_since = None
            remote = gateway_observation(config, attempts=1)
            identity = sample_identity(local, remote, version, after, gateway_baseline,
                                       require_lanes, previous_session=previous_session)
            if identity is not None:
                lanes = local.get('readiness', {}).get('lanes', {})
                if identity != previous:
                    stable_since = now_mono
                    samples = 0
                    first_seen = remote['last_seen']
                    first_lanes = dict(lanes)
                elif interruption_since is not None:
                    gap = now_mono-interruption_since
                    max_transient_gap = max(max_transient_gap, gap)
                    if gap > transient_grace_seconds:
                        stable_since = now_mono
                        samples = 0
                        first_seen = remote['last_seen']
                        first_lanes = dict(lanes)
                previous = identity
                interruption_since = None
                samples += 1
                advanced_lanes = sorted(lane for lane in LANES
                    if lanes.get(lane, 0) > first_lanes.get(lane, 0))
                progressing = remote['last_seen'] > first_seen and (
                    not require_lanes or set(advanced_lanes) == LANES)
                if (progressing and samples >= 3
                        and time.monotonic()-stable_since >= stable_seconds):
                    return {'pid': local['pid'], 'started_at': local['build']['started_at'],
                            'gateway_verified': True, 'readiness': 'stable_authenticated_control_plane',
                            'session': remote['session'], 'runs': local.get('runs'),
                            'stable_seconds': time.monotonic()-stable_since, 'samples': samples,
                            'gateway_seen_advanced': True, 'advanced_lanes': advanced_lanes,
                            'all_lanes_verified': require_lanes,
                            'transient_interruptions': transient_interruptions,
                            'max_transient_gap_seconds': max_transient_gap,
                            'last_transient_category': last_transient_category,
                            'functional_acceptance': 'separate_required_gate'}
                last_error = 'waiting_for_continued_poll_progress'
            elif previous is not None and _same_local_candidate(local, previous, version, after):
                if interruption_since is None:
                    interruption_since = now_mono
                    transient_interruptions += 1
                if now_mono-interruption_since > transient_grace_seconds:
                    previous = stable_since = None
                    samples = 0
                last_error = 'candidate_alive_waiting_for_poll_recovery'
            else:
                previous = stable_since = None
                interruption_since = None
                samples = 0
                last_error = 'pid_session_or_poll_acknowledgements_not_ready'
        except GatewayObservationError as error:
            last_error = 'gateway_'+error.diagnostic['category']
            if not error.diagnostic['retryable']:
                raise
            last_transient_category = error.diagnostic['category']
            if previous is not None:
                try:
                    local = local_observation(base, binding) if binding else local_observation(base)
                except Exception:
                    local = {}
                if _same_local_candidate(local, previous, version, after):
                    if interruption_since is None:
                        interruption_since = now_mono
                        transient_interruptions += 1
                    if now_mono-interruption_since <= transient_grace_seconds:
                        time.sleep(1)
                        continue
            previous = stable_since = None
            interruption_since = None
            samples = 0
        except CandidateProcessError:
            raise
        except Exception:
            previous = stable_since = None
            interruption_since = None
            samples = 0
            last_error = 'local_observation_unavailable'
        time.sleep(1)
    raise RuntimeError('readiness_timeout: ' + last_error)
