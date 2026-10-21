#!/usr/bin/env python3
"""Package already-built candidates, tying them to verified source fingerprints.

Does not deploy, register devices, change credentials or claim runtime acceptance.
"""
import argparse
import datetime
import hashlib
import importlib.util
import json
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tomllib

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import release_current
import source_snapshot

ROOT = pathlib.Path(__file__).resolve().parents[1]

def parallel_files(root):
    return {'parallel-install.py':root/'scripts/parallel-install.py',
            'PARALLEL-INSTALL.md':root/'docs/parallel-octendra/README.md',
            'parallel-gateway.json.in':root/'docs/parallel-octendra/gateway.json.in',
            'parallel-new-host.caddy.in':root/'docs/parallel-octendra/new-host.caddy.in'}


def release_readme(version):
    return ('# Octendra '+version+'\n\n'
        'New independent installs start with PARALLEL-INSTALL.md and parallel-install.py. '
        'The default action is a read-only plan. New HTTPS origin and separately provisioned private authentication are required; neither falls back to the existing instance.\n'
        'Keep the existing binaries, configuration, state, services, reverse proxy and credentials unchanged. '
        'The parallel entry uses new paths and service identities and Gateway loopback port 18788.\n'
        'fleet-upgrade.py and upgrade-code-* are existing-instance maintenance tools, not this parallel installation entry.\n'
        'The bundle is the release identity; verify manifest.json and SHA256SUMS before installation. '
        'Source verification and cross-builds are not deployed-process or client acceptance receipts.\n')

def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as file:
        for data in iter(lambda: file.read(1024 * 1024), b''):
            h.update(data)
    return h.hexdigest()

def machine_contract(binary, version):
    try:
        contract = json.loads(subprocess.check_output(
            [str(binary), 'release-manifest'], text=True, timeout=30))
    except (subprocess.SubprocessError, json.JSONDecodeError) as error:
        raise ValueError('native machine contract unavailable') from error
    if contract.get('version') != version:
        raise ValueError('machine contract version mismatch')
    if (contract.get('machine_contract_protocol') != 1
            or not isinstance(contract.get('tool_count'), int) or contract['tool_count'] <= 0
            or contract.get('tool_schema_revision') != contract.get('tools_sha256')
            or len(contract.get('tool_schema_revision', '')) != 64):
        raise ValueError('native machine contract identity invalid')
    forbidden = {'client_schema_comparison', 'host_schema_status', 'refresh_required', 'refresh_guidance'}
    if forbidden.intersection(contract):
        raise ValueError('release machine contract contains session-specific fields')
    return contract

def main():
    p = argparse.ArgumentParser()
    p.add_argument('--version', required=True)
    p.add_argument('--verification', required=True, type=pathlib.Path)
    p.add_argument('--producer-pid', required=True, type=int)
    p.add_argument('--platforms', nargs='+', choices=release_current.PLATFORMS)
    args = p.parse_args()
    platforms = release_current.selected_platforms(args.platforms)
    release_current.primary(ROOT)
    release_current.validate_producer(ROOT, args.producer_pid)
    proof_path = args.verification.resolve()
    proof = json.loads(proof_path.read_text())
    required = {'fmt', 'clippy', 'rust_tests', 'python_tests', 'workspace'}
    if (proof['version'] != args.version or proof.get('state') != 'passed'
            or not proof.get('source_inputs_unchanged')
            or proof['functional_tests']['failed'] != 0
            or not required.issubset(proof.get('checks', {}))
            or any(proof['checks'][name].get('exit_code') != 0 for name in required)):
        raise SystemExit('verification is incomplete, failed, stale or does not match release')
    spec = importlib.util.spec_from_file_location('release_source_check', ROOT/'scripts/check-code-source.py')
    verifier = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(verifier)
    current = verifier.inputs(ROOT)
    mismatches = sorted(name for name in current.keys() | proof['source_inputs'].keys()
                        if current.get(name) != proof['source_inputs'].get(name))
    if mismatches or not verifier.receipt_current(proof, ROOT):
        raise SystemExit('verified source changed or evidence incomplete: ' + ', '.join(mismatches))
    target = release_current.established_target(ROOT)
    files = {
        'octendra-code-macos-arm64': target/'release/octendra-code',
        'octendra-code-linux-amd64': target/'x86_64-unknown-linux-musl/release/octendra-code',
        'octendra-code-windows-amd64.exe': target/'x86_64-pc-windows-msvc/release/octendra-code.exe',
        'octendra-macos-arm64': target/'release/octendra',
        'octendra-linux-amd64': target/'x86_64-unknown-linux-musl/release/octendra',
        'octendra-windows-amd64.exe': target/'x86_64-pc-windows-msvc/release/octendra.exe',
        'upgrade-code-agent.py': ROOT/'scripts/upgrade-code-agent.py',
        'agent_upgrade_support.py': ROOT/'scripts/agent_upgrade_support.py',
        'launch-code-upgrade.py': ROOT/'scripts/launch-code-upgrade.py',
        'code_upgrade_runner.py': ROOT/'scripts/code_upgrade_runner.py',
        'macos_code_identity.py': ROOT/'scripts/macos_code_identity.py',
        'upgrade-code-gateway.py': ROOT/'scripts/upgrade-code-gateway.py',
        'check-code-gateway.py': ROOT/'scripts/check-code-gateway.py',
        'check-terminal-delivery.py': ROOT/'scripts/check-terminal-delivery.py',
        'release_receipts.py': ROOT/'scripts/release_receipts.py',
        'maintenance_client.py': ROOT/'scripts/maintenance_client.py',
        'release_targets.py': ROOT/'scripts/release_targets.py',
        'sync_bundle.py': ROOT/'scripts/sync_bundle.py',
        'release_client.py': ROOT/'scripts/release_client.py',
        'native_release_client.py': ROOT/'scripts/native_release_client.py',
        'publish-code.py': ROOT/'scripts/publish-code.py',
        'check-collaboration.py': ROOT/'scripts/check-collaboration.py',
        'install-code-agent.py': ROOT/'scripts/install-code-agent.py',
        'upgrade-code-agent-linux.py': ROOT/'scripts/upgrade-code-agent-linux.py',
        'upgrade-code-agent-windows.ps1': ROOT/'scripts/upgrade-code-agent-windows.ps1',
        'launch-code-upgrade-windows.ps1': ROOT/'scripts/launch-code-upgrade-windows.ps1',
        'fleet-upgrade.py': ROOT/'scripts/fleet-upgrade.py',
        'gateway_self_upgrade_runner.py': ROOT/'scripts/gateway_self_upgrade_runner.py',
        'upgrade_instance.py': ROOT/'scripts/upgrade_instance.py',
    }
    files.update(parallel_files(ROOT))
    binary_platforms = {
        'octendra-code-macos-arm64': 'macos',
        'octendra-code-linux-amd64': 'linux',
        'octendra-code-windows-amd64.exe': 'windows',
        'octendra-macos-arm64': 'macos',
        'octendra-linux-amd64': 'linux',
        'octendra-windows-amd64.exe': 'windows',
    }
    files = {name: path for name, path in files.items()
             if name not in binary_platforms or binary_platforms[name] in platforms}
    for path in files.values():
        if not path.is_file():
            raise SystemExit('missing artifact: ' + str(path))
    input_hashes = {name: digest(path) for name, path in files.items()}
    native = target/'release/octendra-code'
    if not native.is_file():
        raise SystemExit('missing native release machine contract executable')
    actual = subprocess.check_output([str(native), '--version'], text=True).strip()
    if actual != 'octendra-code ' + args.version:
        raise SystemExit('native artifact version mismatch')
    try:
        contract = machine_contract(native, args.version)
    except ValueError as error:
        raise SystemExit(str(error)) from error
    if 'windows' in platforms:
        for name in ('octendra-code-windows-amd64.exe', 'octendra-windows-amd64.exe'):
            if files[name].read_bytes()[:2] != b'MZ':
                raise SystemExit('Windows artifact is not a PE executable: '+name)
    # Compile helper source without importing it or running any deployment action.
    for path in files.values():
        if path.suffix == '.py':
            compile(path.read_text(), str(path), 'exec')
    dest, bundle = release_current.stages(ROOT)
    if dest.exists() or dest.is_symlink() or bundle.exists() or bundle.is_symlink():
        raise SystemExit('fixed transaction stage already exists; observe or recover the original operation')
    dest.mkdir()
    stage = dest
    artifacts = {}
    for name, source in files.items():
        shutil.copyfile(source, stage/name)
        (stage/name).chmod(0o755 if not name.endswith('.py') else 0o644)
        artifacts[name] = {'sha256':digest(stage/name), 'size':(stage/name).stat().st_size}
        if artifacts[name]['sha256'] != input_hashes[name] or digest(source) != input_hashes[name]:
            raise ValueError('artifact changed during package validation/copy: '+name)
    shutil.copyfile(proof_path, stage/'source-verification.json')
    manifest = {
        'schema_version':1, **contract,
        'default_install_strategy':'parallel-fresh-only',
        'packaged_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'snapshot_id':source_snapshot.identity(current, source_snapshot.executable_files(ROOT, current)),
        'build_execution_root':str(ROOT),
        'source_verification_sha256':digest(proof_path),
        'source_inputs':proof['source_inputs'], 'artifacts':artifacts,
        'included_platforms': list(platforms),
        'components': {
            'code': {'version': args.version, 'roles': ['agent', 'gateway']},
            'native_cli': {
                'source_version': tomllib.loads((ROOT/'crates/octendra-cli/Cargo.toml').read_text())['package']['version'],
                'roles': ['cli', 'mcp', 'api', 'connector'],
            },
        },
        'pending_platforms': [name for name in release_current.PLATFORMS if name not in platforms],
        'tests':proof['functional_tests'], 'deployed':False,
        'limitations':['64 MiB default per file; 0.7+ negotiated large-file mode permits explicit requests up to 256 MiB when storage reserve and agent capability gates pass; retained partials expire after 24 hours',
                        'inbound prefix recovery needs a strong validator or original expected SHA-256',
                        'expired authorization requires explicit transfer_resume with a refreshed file reference',
                        'publication already committed wins a concurrent cancellation',
                        'paused operations require an explicit resume; arbitrary shell outcomes never replay',
                        'workspace_context is scoped runtime state, not Git or chat-history reconstruction',
                        'native attachment and deployed-process acceptance are separate gates'],
    }
    (stage/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    (stage/'SHA256SUMS').write_text(''.join(digest(stage/name)+'  '+name+'\n' for name in sorted([*files,'manifest.json','source-verification.json'])))
    (stage/'README.md').write_text(release_readme(args.version))
    with tarfile.open(bundle,'x:gz') as archive:
        for path in sorted(dest.iterdir(), key=lambda p:p.name):
            if path.is_file(): archive.add(path,arcname=path.name)
    if verifier.inputs(ROOT) != current or not verifier.receipt_current(proof, ROOT):
        raise ValueError('source changed during package; current was not replaced')
    print(json.dumps({'staged_current': True, 'release_dir':str(dest), 'manifest_sha256':digest(dest/'manifest.json'),
                      'bundle':str(bundle),'bundle_sha256':digest(bundle),'bundle_size':bundle.stat().st_size,
                      'artifacts':artifacts, 'deployed':False}))

if __name__ == '__main__':
    main()
