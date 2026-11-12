#!/usr/bin/env python3
"""One-command incremental delivery. Full checks are explicitly opt-in."""
import argparse
import inspect
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def deployment_config(path):
    config = json.loads(path.read_text()) if path.is_file() else {}
    for key in ('ssh_host', 'ssh_port', 'bin_root', 'cfg', 'service'):
        value = os.environ.get('OCTENDRA_GATEWAY_'+key.upper())
        if value:
            config[key] = value
    config.setdefault('ssh_port', 222)
    config.setdefault('service', 'octendra-gateway.service')
    for key in ('ssh_host', 'bin_root', 'cfg'):
        if not config.get(key):
            raise ValueError('Set '+key+' in '+str(path)+' or OCTENDRA_GATEWAY_'+key.upper())
    if str(config['ssh_host']).startswith('-'):
        raise ValueError('invalid SSH destination')
    config['ssh_port'] = int(config['ssh_port'])
    if not 1 <= config['ssh_port'] <= 65535:
        raise ValueError('invalid SSH port')
    if not all(Path(config[key]).is_absolute() for key in ('bin_root', 'cfg')):
        raise ValueError('absolute Gateway paths required')
    return config


def remote_publish(config, expected):
    # Runs on the existing Linux Gateway; stdin is the verified local bundle.
    import fcntl
    import hashlib
    import json
    import os
    from pathlib import Path
    import shutil
    import subprocess
    import sys
    import tarfile
    import time
    import urllib.parse
    import urllib.request

    def digest(path):
        h = hashlib.sha256()
        with path.open('rb') as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b''):
                h.update(block)
        return h.hexdigest()

    def write_json(path, data):
        temp = path.with_name(path.name+'.tmp')
        temp.write_text(json.dumps(data)+'\n')
        temp.chmod(0o644)
        os.replace(str(temp), str(path))

    root = Path(config['bin_root'])
    releases = root/'releases'
    releases.mkdir(exist_ok=True)
    with (releases/'.ship.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        pending = releases/'.current-pending'
        pending.mkdir()  # Observe an interrupted original attempt instead of replaying it.
        archive_path = pending/'bundle.tgz'
        with archive_path.open('xb') as stream:
            shutil.copyfileobj(sys.stdin.buffer, stream)
        if (archive_path.stat().st_size != expected['bundle_size']
                or digest(archive_path) != expected['bundle_sha256']):
            raise ValueError('Gateway bundle checksum mismatch')
        stage = pending/'package'
        stage.mkdir()
        with tarfile.open(str(archive_path), 'r:gz') as archive:
            names = set()
            for member in archive.getmembers():
                if not member.isfile() or Path(member.name).name != member.name or member.name in names:
                    raise ValueError('bundle must contain unique flat regular files')
                names.add(member.name)
                with archive.extractfile(member) as src, (stage/member.name).open('xb') as dst:
                    shutil.copyfileobj(src, dst)
        if digest(stage/'manifest.json') != expected['manifest_sha256']:
            raise ValueError('Gateway manifest checksum mismatch')
        manifest = json.loads((stage/'manifest.json').read_text())
        executable = root/'octendra-code'
        installed = executable.stat()
        for name, info in manifest['artifacts'].items():
            if Path(name).name != name or digest(stage/name) != info['sha256']:
                raise ValueError('Gateway artifact checksum mismatch: '+name)
        for path in stage.iterdir():
            path.chmod(0o755 if path.name.startswith('octendra-') else 0o644)
            os.chown(str(path), installed.st_uid, installed.st_gid)
        stage.chmod(0o755)
        os.chown(str(stage), installed.st_uid, installed.st_gid)
        candidate = stage/'octendra-code-linux-amd64'
        subprocess.run([str(candidate), 'check', '--config', config['cfg']], check=True,
                       stdout=subprocess.DEVNULL, timeout=30)
        current = releases/manifest['version']
        if current.exists():
            raise ValueError('published version already exists; increment the patch version')
        os.replace(str(stage), str(current))
        gateway_sha = manifest['artifacts']['octendra-code-linux-amd64']['sha256']
        next_binary = root/'.octendra-code-ship'
        shutil.copyfile(str(current/'octendra-code-linux-amd64'), str(next_binary))
        next_binary.chmod(installed.st_mode & 0o777)
        os.chown(str(next_binary), installed.st_uid, installed.st_gid)
        os.replace(str(next_binary), str(executable))
        write_json(releases/'current.json', {'version': manifest['version'],
            'release_version': manifest['version'], 'installed_sha256': gateway_sha,
            'updated_at': int(time.time())})
        subprocess.run(['systemctl', 'restart', config['service']], check=True, timeout=60)
        gateway = json.loads(Path(config['cfg']).read_text())
        health_request = urllib.request.Request('http://'+gateway['bind']+'/healthz',
            headers={'Host': urllib.parse.urlsplit(gateway['public_url']).netloc})
        deadline = time.monotonic()+30
        while time.monotonic() < deadline:
            try:
                with urllib.request.urlopen(health_request, timeout=3) as response:
                    health = json.load(response)
                if (health.get('version') == manifest['version']
                        and health.get('build_revision') == manifest['build_revision']):
                    break
            except (OSError, ValueError):
                pass
            time.sleep(1)
        else:
            raise RuntimeError('Gateway identity not observed; inspect original attempt')
        result = {'state': 'published', 'version': manifest['version'],
            'build_revision': manifest['build_revision'], 'gateway_sha256': digest(executable),
            'agents': 'automatic_on_next_poll', 'verified_at': int(time.time())}
        write_json(releases/'ship-result.json', result)
        shutil.rmtree(str(pending))  # Only this transaction's upload; published versions stay intact.
        print(json.dumps(result))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=Path.home()/'.config/octendra/ship.json')
    parser.add_argument('--full-checks', action='store_true', help='Also run full tests, fmt and clippy before publishing')
    parser.add_argument('--build-only', action='store_true', help='Build/package locally without publishing')
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    config = None if args.build_only else deployment_config(args.config)
    directory = ROOT/'.integration-artifacts'/('ship-'+time.strftime('%Y%m%d-%H%M%S'))
    report = args.report.absolute() if args.report else directory/'release.json'
    command = [sys.executable, str(ROOT/'scripts/release-code.py'), '--report', str(report),
               '--platforms', 'macos', 'linux', 'windows', 'linux-arm64',
               '--full-tests' if args.full_checks else '--quick']
    report.parent.mkdir(parents=True, exist_ok=True)
    log_path = report.with_name(report.stem+'-build.log')
    with log_path.open('x') as log:
        print('Building incrementally'+(' with full checks' if args.full_checks else '')+'…', flush=True)
        built = subprocess.run(command, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT)
    state = json.loads(report.read_text()) if report.is_file() else {}
    if built.returncode or state.get('state') != 'passed':
        raise SystemExit('Build failed; inspect '+str(report)+' and '+str(log_path))
    if args.build_only:
        print(json.dumps({'state': 'built', 'version': state['version'], 'report': str(report)}))
        return
    package = state['package']
    source = inspect.getsource(remote_publish)+'\nremote_publish('+repr(config)+', '+repr(package)+')\n'
    print('Publishing Gateway; Agents update automatically…', flush=True)
    ssh = ['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10', '-p', str(config['ssh_port']),
           config['ssh_host'], 'python3 -c '+shlex.quote(source)]
    with Path(package['bundle']).open('rb') as bundle:
        result = subprocess.run(ssh, stdin=bundle)
    if result.returncode:
        raise SystemExit('Publication failed; inspect the original Gateway .current-pending and '+str(report))


if __name__ == '__main__':
    main()
