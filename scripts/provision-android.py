#!/usr/bin/env python3
"""Configure an owner-signed Android agent through its authorized USB connection."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess
import tempfile
import time

import android_identity


def status(output):
    fields = dict(re.findall(r'(state|request_sha256|error_code|start|running)=([^,\n]*)', output))
    if 'state' not in fields:
        raise RuntimeError('provision_endpoint_unavailable')
    return {key: value.strip() for key, value in fields.items()}


def provision(adb, payload, timeout=30):
    request = hashlib.sha256(payload).hexdigest()
    uri = 'content://' + android_identity.PACKAGE + '.bridge/provision'

    def observe():
        result = subprocess.run(adb + ['shell', 'content', 'query', '--uri', uri + '/status'],
                                capture_output=True, text=True, timeout=10, check=True)
        return status(result.stdout)

    before = observe()
    # An interrupted caller observes the same in-flight request instead of resending.
    pending = before['state'] in {'receiving', 'applying'}
    complete = before.get('request_sha256') == request and before['state'] == 'configured'
    if not pending and not complete:
        result = subprocess.run(adb + ['exec-in', 'content', 'write', '--uri', uri],
                                input=payload, capture_output=True, timeout=15)
        if result.returncode:
            raise RuntimeError('provision_write_failed; inspect provision status before retrying')
    deadline = time.monotonic() + timeout
    while True:
        current = observe()
        if current.get('request_sha256') == request:
            if current['state'] == 'failed':
                raise RuntimeError(current.get('error_code') or 'configuration_failed')
            if current['state'] == 'configured':
                return {'state': 'running' if current.get('running') == '1' else 'configured',
                        'request_sha256': request, 'start': current.get('start'),
                        'running': current.get('running') == '1'}
        if time.monotonic() >= deadline:
            raise RuntimeError('provision_observation_timeout; observe original request ' + request)
        time.sleep(0.25)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--adb', default=str(Path.home() / 'Library/Android/sdk/platform-tools/adb'))
    parser.add_argument('--apksigner', type=Path, default=Path.home() / 'Library/Android/sdk/build-tools/36.1.0/apksigner')
    parser.add_argument('--java-home', type=Path, default=Path('/Library/Java/JavaVirtualMachines/ibm-semeru-open-17.jdk/Contents/Home'))
    args = parser.parse_args()
    payload = args.config.read_bytes()
    if not 0 < len(payload) <= 8192 or not isinstance(json.loads(payload), dict):
        parser.error('expected a device configuration JSON object of at most 8192 bytes')
    if args.config.stat().st_mode & 0o077:
        parser.error('device configuration must be an owner-private file')
    spec = importlib.util.spec_from_file_location('android_shell_activation', Path(__file__).with_name('android-activate-shell.py'))
    activation = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(activation)
    adb = [args.adb, '-s', args.serial]
    # Reuse the existing verifier; a connected USB host never authorizes a different signer.
    with tempfile.TemporaryDirectory(prefix='octendra-android-signer-') as directory:
        args.state_directory = Path(directory)
        args.certificate_sha256 = android_identity.OWNER_CERT_SHA256
        try:
            activation.verify_signer(adb, activation.installed_apk(adb, android_identity.PACKAGE), args)
        except activation.ActivationError as error:
            raise RuntimeError(str(error)) from None
        print(json.dumps(provision(adb, payload)))


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        # Never print a subprocess command/output or the private configuration.
        raise SystemExit(str(error) if isinstance(error, RuntimeError) else type(error).__name__) from None
