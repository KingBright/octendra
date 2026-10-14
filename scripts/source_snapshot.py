#!/usr/bin/env python3
"""Read-only input identity and legacy snapshot verification.

New build/test work executes primary main and checks inputs before/after each
stage. Historical source snapshots remain evidence only; creation is retired.
"""
import argparse
import hashlib
import importlib.util
import json
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
MANIFEST = 'source-snapshot.json'


def verifier(root):
    spec = importlib.util.spec_from_file_location('snapshot_source_verifier', root/'scripts/check-code-source.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def identity(files, executable):
    payload = {'files': files, 'executable_files': sorted(executable)}
    return hashlib.sha256(json.dumps(payload, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def executable_files(root, files):
    return sorted(name for name in files if (root/name).stat().st_mode & 0o111)


def copy_checked(source, destination, expected):
    """Retired: source files must never be copied into a build checkout."""
    raise ValueError('source copying is retired; run verification/builds in the primary main checkout')


def create(root, destination, input_provider=None):
    """Keep old callers fail-closed before any destination or source access."""
    raise ValueError('source snapshot creation is retired; run verification/builds in the primary main checkout')


def check(root, input_provider=None):
    root = root.resolve(strict=True)
    manifest = json.loads((root/MANIFEST).read_text())
    provider = input_provider or verifier(ROOT).inputs
    files = provider(root)
    executable = executable_files(root, files)
    if (manifest.get('state') != 'captured' or manifest.get('schema_version') != 1
            or files != manifest.get('source_inputs')
            or executable != manifest.get('executable_files')
            or identity(files, executable) != manifest.get('snapshot_id')):
        raise ValueError('snapshot changed; do not reuse its verification or build receipts')
    return manifest


def main():
    p = argparse.ArgumentParser()
    p.add_argument('--check', type=pathlib.Path, required=True,
                   help='verify existing legacy evidence without executing its source')
    args = p.parse_args()
    result = check(args.check, input_provider=verifier(ROOT).inputs)
    print(json.dumps({k: result[k] for k in ('state','snapshot_id','file_count','bytes')}))


if __name__ == '__main__':
    main()
