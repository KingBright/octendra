#!/usr/bin/env python3
"""Restore only an explicitly selected, authorized Android shell bridge."""
import argparse
import hashlib
import json
import os
import pathlib
import plistlib
import re
import shlex
import shutil
import subprocess
import sys
import time

SHELL_CLASS = 'fun.hackerlife.octendra.ShellMain'
LABEL = 'fun.hackerlife.octendra.android-shell'


class ActivationError(Exception):
    pass


def run(command, timeout=15, env=None):
    result = subprocess.run(command, capture_output=True, text=True, timeout=timeout, env=env)
    if result.returncode:
        raise ActivationError('adb_or_verifier_unavailable')
    return result.stdout


def bridge_status(adb, package):
    if run(adb + ['get-state']).strip() != 'device':
        raise ActivationError('authorized_usb_device_unavailable')
    row = run(adb + ['shell', 'content', 'query', '--uri', f'content://{package}.bridge/status'])
    socket = re.search(r'socket=(octendra-\d+-[a-f0-9]{32})', row)
    uid = re.search(r'uid=(\d+)', row)
    connected = re.search(r'connected=([01])', row)
    if not socket or not uid or not connected:
        raise ActivationError('local_shell_consent_or_running_agent_required')
    return socket[1], uid[1], connected[1] == '1'


def installed_apk(adb, package):
    paths = run(adb + ['shell', 'pm', 'path', package]).splitlines()
    apk = next((s.removeprefix('package:') for s in paths if s.startswith('package:') and s.endswith('/base.apk')), None)
    if not apk or not apk.startswith('/data/app/'):
        raise ActivationError('installed_apk_unavailable')
    return apk


def verify_signer(adb, apk, args):
    cache = args.state_directory / 'current.apk'
    if cache.is_symlink():
        raise ActivationError('unsafe_apk_cache')
    run(adb + ['pull', apk, str(cache)], timeout=30)
    env = dict(os.environ, JAVA_HOME=str(args.java_home))
    env['PATH'] = str(args.java_home / 'bin') + os.pathsep + env.get('PATH', '')
    output = run([str(args.apksigner), 'verify', '--print-certs', str(cache)], timeout=30, env=env)
    certs = re.findall(r'Signer #\d+ certificate SHA-256 digest:\s*([0-9a-fA-F]{64})', output)
    if len(certs) != 1 or certs[0].lower() != args.certificate_sha256:
        raise ActivationError('installed_apk_signer_mismatch')


def activate(adb, socket, uid, apk, diagnostic_log=None):
    output = shlex.quote(diagnostic_log) if diagnostic_log else '/dev/null'
    command = f'CLASSPATH={shlex.quote(apk)} nohup app_process /system/bin {SHELL_CLASS} {socket} {uid} </dev/null >{output} 2>&1 &'
    run(adb + ['shell', command], timeout=10)


def watch(args):
    adb = [args.adb, '-s', args.serial]
    verified_apk = None
    rejected_apk = None
    last_attempt = 0.0
    attempts = 0
    activations = 0
    while True:
        state = 'waiting_for_authorized_usb_or_local_consent'
        try:
            socket, uid, connected = bridge_status(adb, args.package)
            if connected:
                attempts = 0
                state = 'shell_connected'
            elif time.monotonic() - last_attempt >= min(60, args.interval * 2 ** min(attempts, 3)):
                last_attempt = time.monotonic()
                apk = installed_apk(adb, args.package)
                if apk == rejected_apk:
                    raise ActivationError('installed_apk_signer_mismatch')
                if apk != verified_apk:
                    try:
                        verify_signer(adb, apk, args)
                    except ActivationError as error:
                        if str(error) == 'installed_apk_signer_mismatch':
                            rejected_apk = apk
                        raise
                    verified_apk = apk
                    rejected_apk = None
                fresh_socket, fresh_uid, connected = bridge_status(adb, args.package)
                if (fresh_socket, fresh_uid) != (socket, uid):
                    raise ActivationError('bridge_identity_changed')
                if not connected:
                    activate(adb, socket, uid, apk)
                    activations += 1
                    attempts += 1
                state = 'activation_requested' if not connected else 'shell_connected'
            else:
                state = 'waiting_for_activation_observation'
        except (ActivationError, subprocess.TimeoutExpired, OSError) as error:
            state = str(error) if isinstance(error, ActivationError) else 'usb_or_verifier_unavailable'
        status = {'state': state, 'observed_at': int(time.time()), 'serial': args.serial,
                  'activations': activations, 'permission_bypass': False}
        temporary = args.state_directory / 'status.json.tmp'
        temporary.write_text(json.dumps(status) + '\n')
        temporary.chmod(0o600)
        temporary.replace(args.state_directory / 'status.json')
        time.sleep(args.interval)


def install_launch_agent(args):
    if sys.platform != 'darwin':
        raise ActivationError('launch_agent_requires_macos')
    root = pathlib.Path.home() / '.local/share/octendra/agent'
    target = root / 'helpers/android-activate-shell.py'
    source = pathlib.Path(__file__).read_bytes()
    plist = pathlib.Path.home() / 'Library/LaunchAgents' / (LABEL + '.plist')
    argv = [sys.executable, str(target), '--watch', '--serial', args.serial, '--adb', args.adb,
            '--package', args.package, '--interval', str(args.interval),
            '--certificate-sha256', args.certificate_sha256, '--apksigner', str(args.apksigner),
            '--java-home', str(args.java_home), '--state-directory', str(args.state_directory)]
    value = {'Label': LABEL, 'ProgramArguments': argv, 'RunAtLoad': True, 'KeepAlive': True,
             'ThrottleInterval': 10, 'ProcessType': 'Background', 'Umask': 0o077,
             'StandardOutPath': '/dev/null', 'StandardErrorPath': str(args.state_directory / 'stderr.log')}
    domain = f'gui/{os.getuid()}'
    existing = subprocess.run(['launchctl', 'print', domain + '/' + LABEL], capture_output=True)
    if existing.returncode == 0:
        old = plistlib.loads(plist.read_bytes())
        if len(old.get('ProgramArguments', [])) < 2 or old['ProgramArguments'][1] != str(target):
            raise ActivationError('existing_watch_service_identity_mismatch')
        run(['launchctl', 'bootout', domain + '/' + LABEL])
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_suffix('.py.tmp')
    temporary.write_bytes(source)
    temporary.chmod(0o700)
    temporary.replace(target)
    plist.parent.mkdir(parents=True, exist_ok=True)
    plist.write_bytes(plistlib.dumps(value))
    plist.chmod(0o600)
    run(['launchctl', 'bootstrap', domain, str(plist)])
    print(json.dumps({'service': LABEL, 'trigger': 'user_login', 'keep_alive': True,
                      'installed_sha256': hashlib.sha256(source).hexdigest()}))


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--serial', required=True)
    p.add_argument('--adb', default='adb')
    p.add_argument('--package', default='fun.hackerlife.octendra')
    p.add_argument('--diagnostic-log')
    p.add_argument('--watch', action='store_true')
    p.add_argument('--install-launch-agent', action='store_true')
    p.add_argument('--interval', type=int, default=10)
    p.add_argument('--certificate-sha256')
    p.add_argument('--apksigner', type=pathlib.Path, default=pathlib.Path.home() / 'Library/Android/sdk/build-tools/36.1.0/apksigner')
    p.add_argument('--java-home', type=pathlib.Path, default=pathlib.Path('/opt/homebrew/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home'))
    p.add_argument('--state-directory', type=pathlib.Path, default=pathlib.Path.home() / '.local/share/octendra/agent/private/android-shell-watch')
    args = p.parse_args()
    if not re.fullmatch(r'[A-Za-z][A-Za-z0-9_.]+', args.package):p.error('invalid package')
    if args.diagnostic_log and not re.fullmatch(r'/data/local/tmp/octendra-helper-[A-Za-z0-9-]+\.log', args.diagnostic_log):p.error('invalid helper diagnostic path')
    if not 5 <= args.interval <= 60:p.error('interval must be 5..60 seconds')
    if args.watch or args.install_launch_agent:
        if not args.certificate_sha256 or not re.fullmatch('[0-9a-fA-F]{64}', args.certificate_sha256):p.error('watch requires the existing owner signing certificate SHA-256')
        if not args.apksigner.is_file() or not (args.java_home / 'bin/java').is_file():p.error('existing apksigner and Java runtime required')
        args.certificate_sha256 = args.certificate_sha256.lower()
        args.adb = shutil.which(args.adb) or args.adb
        if not pathlib.Path(args.adb).is_file():p.error('existing authorized ADB runtime required')
        args.state_directory.mkdir(parents=True, exist_ok=True)
        args.state_directory.chmod(0o700)
        if args.install_launch_agent:install_launch_agent(args)
        else:watch(args)
        return
    adb = [args.adb, '-s', args.serial]
    socket, uid, connected = bridge_status(adb, args.package)
    if connected:
        print('Shell bridge is already connected; no activation repeated.')
        return
    activate(adb, socket, uid, installed_apk(adb, args.package), args.diagnostic_log)
    print('Activation requested. Verify shell_connected and shell_uid with android status; no ADB authorization is granted or bypassed.')


if __name__ == '__main__':
    main()
