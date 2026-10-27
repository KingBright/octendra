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
import release_targets
import pinned_source

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


def binary_files(target, platforms=None):
    selected = release_current.selected_platforms(platforms)
    return {name: target / relative for name, (platform, relative) in release_targets.BINARY_PATHS.items()
            if platform in selected}


def verify_linux_architecture(path, architecture):
    with path.open('rb') as stream:
        header = stream.read(64)
    machine = {'amd64': 62, 'arm64': 183}.get(architecture)
    if (machine is None or len(header) < 20 or header[:6] != b'\x7fELF\x02\x01'
            or int.from_bytes(header[18:20], 'little') != machine):
        raise ValueError('Linux artifact ELF architecture mismatch: ' + path.name)

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

@release_current.admission()
def main():
    p = argparse.ArgumentParser()
    p.add_argument('--version', required=True)
    p.add_argument('--verification', required=True, type=pathlib.Path)
    p.add_argument('--producer-pid', required=True, type=int)
    p.add_argument('--platforms', nargs='+', choices=release_current.PLATFORMS)
    p.add_argument('--source-binding', type=pathlib.Path)
    args = p.parse_args()
    platforms = release_current.selected_platforms(args.platforms)
    release_current.primary(ROOT)
    binding = pinned_source.read(args.source_binding, ROOT) if args.source_binding else None
    source_root = pathlib.Path(binding['source_root']) if binding else ROOT
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
    if binding and proof.get('source_commit') != binding['source_commit']:
        raise SystemExit('verification belongs to another source commit')
    if binding and (proof.get('producer_commit') != binding['producer_commit'] or
                    proof.get('producer_inputs', {}) != binding.get('producer_inputs', {})):
        raise SystemExit('verification producer tooling differs from original binding')
    spec = importlib.util.spec_from_file_location('release_source_check', source_root/'scripts/check-code-source.py')
    verifier = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(verifier)
    current = verifier.inputs(source_root)
    mismatches = sorted(name for name in current.keys() | proof['source_inputs'].keys()
                        if current.get(name) != proof['source_inputs'].get(name))
    if mismatches or not verifier.receipt_current(proof, source_root):
        raise SystemExit('verified source changed or evidence incomplete: ' + ', '.join(mismatches))
    target = release_current.established_target(source_root)
    files = {
        'upgrade-code-agent.py': source_root/'scripts/upgrade-code-agent.py',
        'agent_upgrade_support.py': source_root/'scripts/agent_upgrade_support.py',
        'launch-code-upgrade.py': source_root/'scripts/launch-code-upgrade.py',
        'code_upgrade_runner.py': source_root/'scripts/code_upgrade_runner.py',
        'macos_code_identity.py': source_root/'scripts/macos_code_identity.py',
        'upgrade-code-gateway.py': source_root/'scripts/upgrade-code-gateway.py',
        'check-code-gateway.py': source_root/'scripts/check-code-gateway.py',
        'check-terminal-delivery.py': source_root/'scripts/check-terminal-delivery.py',
        'release_receipts.py': source_root/'scripts/release_receipts.py',
        'maintenance_client.py': source_root/'scripts/maintenance_client.py',
        'release_targets.py': source_root/'scripts/release_targets.py',
        'sync_bundle.py': source_root/'scripts/sync_bundle.py',
        'release_client.py': source_root/'scripts/release_client.py',
        'native_release_client.py': source_root/'scripts/native_release_client.py',
        'publish-code.py': source_root/'scripts/publish-code.py',
        'check-collaboration.py': source_root/'scripts/check-collaboration.py',
        'install-code-agent.py': source_root/'scripts/install-code-agent.py',
        'upgrade-code-agent-linux.py': source_root/'scripts/upgrade-code-agent-linux.py',
        'upgrade-code-agent-windows.ps1': source_root/'scripts/upgrade-code-agent-windows.ps1',
        'launch-code-upgrade-windows.ps1': source_root/'scripts/launch-code-upgrade-windows.ps1',
        'fleet-upgrade.py': source_root/'scripts/fleet-upgrade.py',
        'gateway_self_upgrade_runner.py': source_root/'scripts/gateway_self_upgrade_runner.py',
        'upgrade_instance.py': source_root/'scripts/upgrade_instance.py',
    }
    files.update(binary_files(target, platforms))
    files.update(parallel_files(source_root))
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
    for name, path in files.items():
        if release_targets.BINARY_PLATFORMS.get(name) in ('linux', 'linux-arm64'):
            verify_linux_architecture(path, 'arm64' if name.endswith('arm64') else 'amd64')
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
        'snapshot_id':source_snapshot.identity(current, source_snapshot.executable_files(source_root, current)),
        'build_execution_root':str(source_root),
        'source_verification_sha256':digest(proof_path),
        'source_inputs':proof['source_inputs'], 'artifacts':artifacts,
        'included_platforms': list(platforms),
        'components': {
            'code': {'version': args.version, 'roles': ['agent', 'gateway']},
            'native_cli': {
                'source_version': tomllib.loads((source_root/'crates/octendra-cli/Cargo.toml').read_text())['package']['version'],
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
    if binding:
        manifest.update(source_commit=binding['source_commit'], producer_commit=binding['producer_commit'],
                        producer_authority_root=str(ROOT), producer_inputs=binding.get('producer_inputs', {}))
    (stage/'manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    (stage/'SHA256SUMS').write_text(''.join(digest(stage/name)+'  '+name+'\n' for name in sorted([*files,'manifest.json','source-verification.json'])))
    (stage/'README.md').write_text(release_readme(args.version))
    with tarfile.open(bundle,'x:gz') as archive:
        for path in sorted(dest.iterdir(), key=lambda p:p.name):
            if path.is_file(): archive.add(path,arcname=path.name)
    if verifier.inputs(source_root) != current or not verifier.receipt_current(proof, source_root):
        raise ValueError('source changed during package; current was not replaced')
    print(json.dumps({'staged_current': True, 'release_dir':str(dest), 'manifest_sha256':digest(dest/'manifest.json'),
                      'bundle':str(bundle),'bundle_sha256':digest(bundle),'bundle_size':bundle.stat().st_size,
                      'artifacts':artifacts, 'deployed':False}))

if __name__ == '__main__':
    main()
