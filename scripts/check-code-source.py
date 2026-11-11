#!/usr/bin/env python3
"""Release source verification with explicit, input-bound test coverage.

All selected tests execute; a retained full run bounds omitted integration targets.
--check-receipt only compares inputs and never starts verification commands.
"""
import argparse
import datetime
import hashlib
import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

import build_process

import release_current
import pinned_source

ROOT = pathlib.Path(__file__).resolve().parents[1]
REQUIRED = {'fmt', 'clippy', 'rust_tests', 'python_tests', 'workspace'}
RUST_ARGS = ['--release', '--locked', '--no-fail-fast']
HARNESS_ARGS = ['--', '--test-threads=1', '--color', 'never']
FULL_RUST_COMMAND = ['bash', 'scripts/test.sh', *RUST_ARGS, *HARNESS_ARGS]
CORE_TESTS = {'integration', 'authorization_boundaries', 'oauth_clients',
              'file_transfers', 'terminal_regressions'}
# Shared dispatch, protocol, dependency, and unclassified edits use the full run.
# These bounded domains add their integration contracts to the mandatory core.
TEST_DOMAINS = (
    ({'auth', 'client_profiles', 'oauth_handoff', 'oauth_client_metadata_tests'},
     {'oauth_client_metadata_cli', 'owner_password_cli', 'owner_password_check_cli',
      'review_regressions', 'live_configuration', 'experience_contract'}),
    ({'files', 'files_sync', 'durable_transfer', 'transfers', 'transfer_control',
      'transfer_journal', 'transfer_receiver', 'transfer_watch', 'resumable'},
     {'transfers_040', 'result_atomicity', 'review_regressions', 'concurrency',
      'receipt_delivery_034', 'state_recovery_033', 'reliability_0102',
      'iteration_reliability', 'experience_contract'}),
    ({'terminal', 'terminal_io', 'terminal_output', 'terminal_sync', 'local_sudo'},
     {'terminal_read_latency', 'interaction_031', 'observations_032',
      'receipt_delivery_034', 'state_recovery_033', 'iteration_reliability',
      'concurrency', 'experience_contract'}),
    ({'token_output', 'diagnostics', 'payload_budget', 'code_payload',
      'permission_view', 'status_view'},
     {'experience_contract', 'experience_ab', 'gateway_latency', 'terminal_read_latency'}),
)
VERIFIER_INPUTS = {'scripts/check-code-source.py', 'scripts/release-code.py',
                   'scripts/tests/test_source_verification.py',
                   'scripts/tests/test_build_resource_limits.py',
                   'scripts/tests/test_release_current.py'}


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(1048576), b''):
            h.update(chunk)
    return h.hexdigest()


def inputs(root):
    # Include path dependencies and fixtures, not only the primary crate. Added
    # and deleted files are part of identity, as are compiler/Cargo settings.
    paths = {root/n for n in ('AGENTS.md', '.gitattributes', 'Cargo.toml', 'Cargo.lock', 'rust-toolchain', 'rust-toolchain.toml', 'build.rs',
                             'rustfmt.toml', '.rustfmt.toml', 'clippy.toml', '.clippy.toml') if (root/n).is_file()}
    # sqlx::migrate!("../../migrations") and root fixtures are compiled/tested
    # inputs too. Include their membership, not only .rs files inside crates.
    for subtree in ('crates', 'desktop-worker', 'skills', '.cargo', '.github', 'migrations', 'fixtures', 'tests', 'assets', '.sqlx'):
        if (root/subtree).is_symlink():
            raise ValueError('unsupported linked verification subtree: '+subtree)
        for directory, children, files in os.walk(root/subtree, followlinks=False):
            children[:] = sorted(n for n in children if n not in ('target', '.git', 'node_modules', '__pycache__'))
            if any((pathlib.Path(directory)/n).is_symlink() for n in children):
                raise ValueError('linked directory cannot be omitted from verification: '+str(directory))
            paths.update(pathlib.Path(directory)/n for n in files)
    paths.update((root/'scripts').glob('*.py'))
    if (root/'scripts/test.sh').is_file():
        paths.add(root/'scripts/test.sh')
    paths.update((root/'scripts').glob('*.ps1'))
    for name in (
        'octendra-service',
        'octendra-systemd-service',
        'octendra-service.ps1',
        'octendra-code-gateway.service',
        'code-gateway.caddy',
    ):
        path = root/'scripts'/name
        if path.is_file():
            paths.add(path)
    for pattern in ('*.py', '*.rs', '*.mjs'):
        paths.update((root/'scripts/tests').rglob(pattern))
    # Android is part of mainline input identity. Keep generated SDK/build state,
    # signing material and the debug-only generated CA outside source snapshots.
    android = root/'android'
    if android.is_symlink():
        raise ValueError('unsupported linked verification subtree: android')
    for directory, children, files in os.walk(android, followlinks=False):
        children[:] = sorted(n for n in children if n not in
                             ('build', '.gradle', '.kotlin', '.idea', '.git'))
        if any((pathlib.Path(directory)/n).is_symlink() for n in children):
            raise ValueError('linked Android directory cannot be omitted from verification')
        for name in files:
            if name in ('local.properties', 'fixture_ca.pem') or pathlib.Path(name).suffix.lower() in (
                    '.jks', '.keystore', '.p12', '.pfx', '.key', '.apk', '.aab'):
                continue
            paths.add(pathlib.Path(directory)/name)
    result = {}
    for path in sorted(paths):
        name = str(path.relative_to(root))
        if path.is_symlink():
            # Do not claim to have verified inputs outside this repository.
            raise ValueError('unsupported symlink verification input: '+name)
        if path.is_file():
            result[name] = digest(path)
    return result


def publish(path, record):
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as f:
        tmp = pathlib.Path(f.name)
        try:
            f.write((json.dumps(record, indent=2)+'\n').encode())
            f.flush()
            os.fsync(f.fileno())
            os.replace(tmp, path)
        finally:
            tmp.unlink(missing_ok=True)


def run_command(argv, root, path, env, timeout, on_update=None):
    """Timeout is an inactivity budget, never a total compiler wall limit."""
    return build_process.run(argv, root, path, env, idle_timeout=timeout,
                             on_update=on_update)


def summarize(checks, logs):
    rust_path, py_path = logs/'rust_tests.log', logs/'python_tests.log'
    rust = rust_path.read_text(errors='replace') if rust_path.exists() else ''
    py = py_path.read_text(errors='replace') if py_path.exists() else ''
    counts = re.findall(r'test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored;', rust)
    ran = re.search(r'Ran (\d+) tests?', py)
    summary = re.search(r'^(?:FAILED|OK)(?: \(([^\n]*)\))?$', py, re.MULTILINE)
    detail = dict((k, int(v)) for k, v in re.findall(r'(failures|errors|skipped|expected failures|unexpected successes)=(\d+)', summary[1] or '')) if summary else {}
    py_failed = detail.get('failures', 0)+detail.get('errors', 0)+detail.get('unexpected successes', 0)
    py_skipped = detail.get('skipped', 0)
    py_expected_failures = detail.get('expected failures', 0)
    py_selected = int(ran[1]) if ran and summary else 0
    py_executed = max(0, py_selected-py_skipped)
    py_passed = max(0, py_executed-py_failed-py_expected_failures)
    rust_passed = sum(int(p) for p, f, i in counts)
    rust_failed = sum(int(f) for p, f, i in counts)
    rust_ignored = sum(int(i) for p, f, i in counts)
    rust_selected = rust_passed+rust_failed+rust_ignored
    rust_executed = rust_passed+rust_failed
    gates_ok = all(checks.get(name, {}).get('state') == 'finished' and checks[name].get('exit_code') == 0
                   for name in ('rust_tests', 'python_tests'))
    output_complete = all(checks.get(name, {}).get('output_complete', True)
                          and not checks.get(name, {}).get('output_truncated', False)
                          and not checks.get(name, {}).get('output_error')
                          for name in ('rust_tests', 'python_tests'))
    evidence_complete = (gates_ok and output_complete and rust_executed > 0 and py_executed > 0
                         and rust_failed == 0 and py_failed == 0
                         and py_selected >= py_skipped+py_failed+py_expected_failures)
    return {'passed': rust_passed+py_passed, 'failed': rust_failed+py_failed,
            'rust_passed': rust_passed, 'rust_failed': rust_failed, 'rust_executed': rust_executed,
            'python_passed': py_passed, 'python_failed': py_failed, 'python_executed': py_executed,
            'rust_selected': rust_selected, 'python_selected': py_selected,
            'python_skipped': py_skipped, 'python_expected_failures': py_expected_failures,
            'rust_ignored': rust_ignored, 'ignored_benchmarks': rust_ignored,
            'output_complete': output_complete, 'test_evidence_protocol': 2,
            'test_gates_completed_successfully': gates_ok,
            'evidence_complete': evidence_complete,
            'evidence_note': 'verification requires executed tests, complete output and no recorded failures; skipped/ignored tests are not executions' if gates_ok and not evidence_complete else None}


def snapshot_identity(root):
    current = inputs(root)
    executable = sorted(name for name in current if (root/name).stat().st_mode & 0o111)
    identity = hashlib.sha256(json.dumps({'files':current,'executable_files':executable},
                               sort_keys=True,separators=(',', ':')).encode()).hexdigest()
    path = root/'source-snapshot.json'
    if path.exists():
        manifest = json.loads(path.read_text())
        if manifest.get('state') != 'captured' or manifest.get('snapshot_id') != identity:
            raise ValueError('snapshot input identity changed')
    return identity


def full_baseline(path, root):
    """Only a complete full-workspace run with intact evidence can omit targets."""
    try:
        path = path.resolve(strict=True)
        if not path.is_relative_to(root.resolve()):
            return None
        proof = json.loads(path.read_text())
        check = proof['checks']['rust_tests']
        tests = proof['functional_tests']
        log = root/check['log']
        if (proof.get('state') != 'passed' or not proof.get('source_inputs_unchanged')
                or not isinstance(proof.get('source_inputs'), dict) or not proof['source_inputs']
                or proof.get('execution_root') != str(root.resolve())
                or not REQUIRED.issubset(proof['checks'])
                or any(proof['checks'][name].get('exit_code') != 0 for name in REQUIRED)
                or check.get('state') != 'finished' or check.get('command') != FULL_RUST_COMMAND
                or not check.get('output_complete') or check.get('output_truncated')
                or check.get('output_error') or digest(log) != check.get('sha256')
                or not tests.get('evidence_complete') or not tests.get('test_gates_completed_successfully')
                or tests.get('failed') != 0 or tests.get('rust_executed', 0) <= 0
                or proof.get('rust_test_plan', {}).get('mode', 'full') != 'full'):
            return None
        counts = re.findall(r'test result: \w+\. (\d+) passed; (\d+) failed;', log.read_text(errors='replace'))
        if (not counts or sum(int(f) for p, f in counts) != 0
                or sum(int(p) for p, f in counts) != tests['rust_executed']):
            return None
        return proof
    except (OSError, ValueError, KeyError, TypeError):
        return None


def baseline_path(root):
    candidates = [root/'dist/octendra-code-current/source-verification.json']
    marker = root/'target/source-verification/latest-full.json'
    try:
        record = json.loads(marker.read_text())
        path = pathlib.Path(record['path'])
        if digest(path) == record['sha256']:
            candidates.append(path)
    except (OSError, ValueError, KeyError, TypeError):
        pass
    # Existing release receipts include successful verification from a failed
    # cross-build. Reuse that coverage rather than requiring a published binary.
    candidates.extend((root/'target').glob('**/verification.json'))
    candidates = sorted({p for p in candidates if p.is_file()},
                        key=lambda p: p.stat().st_mtime, reverse=True)
    return next((p for p in candidates if full_baseline(p, root)), None)


def rust_test_plan(root, current, full=False, baseline=None):
    def full_plan(reason):
        return {'protocol': 1, 'mode': 'full', 'reason': reason,
                'command': FULL_RUST_COMMAND, 'selected_targets': ['workspace'],
                'omitted_integration_targets': []}
    if full:
        return full_plan('full workspace requested')
    path = baseline if baseline is not None else baseline_path(root)
    proof = full_baseline(path, root) if path is not None else None
    if proof is None:
        return full_plan('no complete full-workspace baseline with intact logs')
    before = proof['source_inputs']
    changed = sorted(n for n in before.keys() | current.keys() if before.get(n) != current.get(n))
    available = {p.stem for p in (root/'crates/octendra-code/tests').glob('*.rs')}
    selected = set(CORE_TESTS)
    for name in changed:
        if name not in current:
            return full_plan('deleted input requires full coverage: '+name)
        if name in VERIFIER_INPUTS or name.startswith('android/'):
            continue
        # Legacy receipts did not fingerprint this wrapper; its original full
        # command and log are checked above. Later edits invalidate selection.
        if name == 'scripts/test.sh' and name not in before:
            continue
        path_name = pathlib.PurePosixPath(name)
        if path_name.parent == pathlib.PurePosixPath('crates/octendra-code/tests') and path_name.suffix == '.rs':
            selected.add(path_name.stem)
            continue
        if name.startswith('crates/octendra-code/tests/experience_contract/'):
            selected.add('experience_contract')
            continue
        if path_name.parent == pathlib.PurePosixPath('crates/octendra-code/src') and path_name.suffix == '.rs':
            domain = next((targets for sources, targets in TEST_DOMAINS if path_name.stem in sources), None)
            if domain is not None:
                selected.update(domain)
                continue
        return full_plan('shared or unclassified input requires full coverage: '+name)
    if not selected.issubset(available):
        return full_plan('required integration target missing')
    command = ['bash', 'scripts/test.sh', '-p', 'octendra-code', '--lib', *RUST_ARGS]
    for target in sorted(selected):
        command.extend(['--test', target])
    command.extend(HARNESS_ARGS)
    return {'protocol': 1, 'mode': 'affected', 'reason': 'core plus changed-domain contracts; remaining targets covered by full baseline',
            'baseline': {'path': str(path.resolve()), 'sha256': digest(path)},
            'changed_inputs': changed, 'command': command,
            'selected_targets': ['octendra-code::lib', *sorted(selected)],
            'omitted_integration_targets': sorted(available-selected),
            'other_workspace_tests': 'unchanged since full baseline; omitted, not executed in this run'}


def test_plan_current(proof, root):
    plan = proof.get('rust_test_plan')
    if plan is None:
        return not proof.get('test_policy_protocol')  # Existing receipts/custom fixtures.
    try:
        if plan.get('protocol') != 1:
            return False
        if plan['mode'] == 'full':
            return (plan['command'] == FULL_RUST_COMMAND
                    and plan['selected_targets'] == ['workspace']
                    and not plan['omitted_integration_targets']
                    and proof['checks']['rust_tests']['command'] == FULL_RUST_COMMAND)
        path = pathlib.Path(plan['baseline']['path'])
        return (plan['mode'] == 'affected' and digest(path) == plan['baseline']['sha256']
                and rust_test_plan(root, proof['source_inputs'], baseline=path) == plan
                and proof['checks']['rust_tests']['command'] == plan['command'])
    except (OSError, ValueError, KeyError, TypeError):
        return False


def receipt_current(proof, root):
    return (proof.get('state') == 'passed' and proof.get('source_inputs_unchanged') is True
            and REQUIRED.issubset(proof.get('checks', {}))
            and all(proof['checks'][name].get('exit_code') == 0 for name in REQUIRED)
            and proof.get('functional_tests', {}).get('failed') == 0
            and proof.get('functional_tests', {}).get('test_gates_completed_successfully') is True
            and proof.get('functional_tests', {}).get('evidence_complete') is True
            and proof.get('source_inputs') == inputs(root)
            and proof.get('snapshot_id') == snapshot_identity(root)
            and test_plan_current(proof, root))


@release_current.admission()
def run_verification(root, report, gates=None, timeout=900, logs_root=None, source_binding=None, full_tests=False):
    report = report.resolve()
    report.parent.mkdir(parents=True, exist_ok=True)
    # Claim destination before any expensive command; never discover a collision
    # only after rerunning all tests, and never overwrite an earlier receipt.
    with report.open('x') as f:
        json.dump({'state': 'preparing', 'deployed': False}, f)
    result = {'state': 'preparing', 'checks': {}, 'source_inputs': {}, 'deployed': False}
    run = datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')+'-'+uuid.uuid4().hex[:8]
    logs = (logs_root if logs_root is not None else root/'target/source-verification')/run
    logs.mkdir(parents=True)
    native_gates = gates is None
    try:
        result.update(version=tomllib.loads((root/'crates/octendra-code/Cargo.toml').read_text())['package']['version'],
                      run_id=run, scope='native source verification, not deployment', source_inputs=inputs(root), state='running',
                      execution_root=str(root.resolve()), snapshot_id=snapshot_identity(root))
        if source_binding:
            result.update(source_commit=source_binding['source_commit'], producer_commit=source_binding['producer_commit'],
                          producer_inputs=source_binding.get('producer_inputs', {}))
        publish(report, result)
        # Budget only confirmed inactivity. CPU, output or process activity
        # keeps a cold compiler running regardless of its elapsed duration.
        gate_timeouts = {'rust_tests': 2700, 'workspace': 1800} if gates is None else {}
        if gates is None:
            # Verify the profile shipped by release-code.py. Reuse its existing
            # cache instead of contending with unrelated interactive dev builds.
            result['test_policy_protocol'] = 1
            result['rust_test_plan'] = rust_test_plan(root, result['source_inputs'],
                                                     full=full_tests or source_binding is not None)
            gates = [
                ('fmt', ['cargo', 'fmt', '--all', '--', '--check']),
                ('clippy', ['cargo', 'clippy', '-p', 'octendra-code', '-p', 'octendra-release', '--all-targets', '--release', '--locked', '--', '-D', 'warnings']),
                ('python_tests', [sys.executable, '-W', 'error::ResourceWarning', '-m', 'unittest', 'discover', '-s', 'scripts/tests', '-v']),
                # Concurrency fixtures create their own parallel actors. Keep
                # unrelated fsync-heavy cases from distorting their watchdogs;
                # do not weaken assertions, deadlines, or in-test concurrency.
                ('rust_tests', result['rust_test_plan']['command']),
                ('workspace', ['cargo', 'check', '--workspace', '--release', '--locked']),
            ]
        env = dict(os.environ, CARGO_TERM_COLOR='never', PYTHONUNBUFFERED='1')
        for name, argv in gates:
            path = logs/(name+'.log')
            label = str(path.relative_to(root)) if path.is_relative_to(root) else str(path)
            started = time.monotonic()
            command_timeout = gate_timeouts.get(name, timeout)
            result['checks'][name] = {'state': 'running', 'exit_code': None,
                                      'command': list(argv), 'working_directory': str(root.resolve()),
                                      'log': label, 'idle_timeout_seconds': command_timeout,
                                      'wall_timeout_seconds': None}
            publish(report, result)
            def progress(value):
                result['checks'][name].update(value, log=label)
                publish(report, result)
            try:
                outcome = run_command(argv, root, path, env, command_timeout, on_update=progress)
            except Exception as error:
                outcome = {'state': 'start_or_collection_failed', 'exit_code': None, 'failure_type': type(error).__name__}
            result['checks'][name].update(outcome, log=label, elapsed_seconds=time.monotonic()-started,
                                         output_complete=outcome.get('output_complete', outcome['state'] == 'finished' and path.is_file()),
                                         output_truncated=False, step_index=len(result['checks'])-1,
                                         sha256=digest(path) if path.exists() else None)
            publish(report, result)
            print(json.dumps({'gate': name, **result['checks'][name]}), flush=True)
            if outcome['state'] != 'finished' or outcome['exit_code'] != 0:
                result['state'] = 'interrupted' if outcome['state'] == 'interrupted' else 'failed'
                break
        else:
            result['state'] = 'passed'
    except (Exception, KeyboardInterrupt) as error:
        result.update(state='interrupted' if isinstance(error, KeyboardInterrupt) else 'failed', failure_type=type(error).__name__)
    try:
        after = inputs(root)
        result['source_inputs_unchanged'] = (result['source_inputs'] == after
                                             and result.get('snapshot_id') == snapshot_identity(root))
        if not result['source_inputs_unchanged']:
            result['check_outcome_before_stale'] = result['state']
            result['state'] = 'stale'
            before = result['source_inputs']
            result['changed_inputs'] = sorted(k for k in before.keys() | after.keys() if before.get(k) != after.get(k))
    except Exception as error:
        result.update(state='stale', source_inputs_unchanged=False, input_failure_type=type(error).__name__)
    result.update(functional_tests=summarize(result['checks'], logs), verified_at=datetime.datetime.now(datetime.timezone.utc).isoformat())
    if 'rust_test_plan' in result:
        result['functional_tests'].update(rust_test_scope=result['rust_test_plan']['mode'],
            omitted_rust_integration_targets=result['rust_test_plan']['omitted_integration_targets'])
    if result['state'] == 'passed' and not result['functional_tests']['evidence_complete']:
        result['state'] = 'failed'
        result['failure_type'] = 'IncompleteTestEvidence'
    publish(report, result)
    if native_gates and result['state'] == 'passed' and result['rust_test_plan']['mode'] == 'full':
        marker = root/'target/source-verification/latest-full.json'
        marker.parent.mkdir(parents=True, exist_ok=True)
        publish(marker, {'path': str(report), 'sha256': digest(report)})
    print(json.dumps({'state': result['state'], 'report': str(report), **result['functional_tests']}), flush=True)
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--report', type=pathlib.Path, required=True)
    parser.add_argument('--check-receipt', action='store_true')
    parser.add_argument('--full-tests', action='store_true',
                        help='Compile and execute every workspace test target in release mode')
    parser.add_argument('--source-binding', type=pathlib.Path,
                        help='Original producer-owned fixed-commit record; no arbitrary source root')
    args = parser.parse_args()
    if args.check_receipt:
        try:
            current = receipt_current(json.loads(args.report.read_text()), ROOT)
        except Exception:
            current = False
        print(json.dumps({'state': 'current' if current else 'stale_or_invalid', 'report': str(args.report)}))
        raise SystemExit(0 if current else 1)
    if os.name == 'posix':
        def interrupted(_signal, _frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, interrupted)
    binding = pinned_source.read(args.source_binding, release_current.primary(ROOT)) if args.source_binding else None
    root = pathlib.Path(binding['source_root']) if binding else ROOT
    result = run_verification(root, args.report,
                              logs_root=args.report.resolve().parent/'source-check-logs' if binding else None,
                              source_binding=binding, full_tests=args.full_tests)
    if result['state'] != 'passed':
        raise SystemExit(1)


if __name__ == '__main__':
    main()
