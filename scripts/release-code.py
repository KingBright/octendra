#!/usr/bin/env python3
"""Verify/build/package primary main inputs under the existing long-lived lease.

No deployment, credentials, service restart, git reset or implicit test-result reuse.
Status is durable and queryable without spawning another build. Only selected
fixed commands run; the source itself is executable input, not a sandbox.
"""
import argparse
import contextlib
import datetime
import importlib.util
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

import build_slot
import build_process
import source_snapshot
import release_current
import pinned_source

ROOT = pathlib.Path(__file__).resolve().parents[1]


@contextlib.contextmanager
def shared_build_lock(enabled):
    if enabled:
        with release_current.public_api().cleanup_lock():
            yield
    else:
        yield


@contextlib.contextmanager
def execution_source(commit, report, state):
    if commit is None:
        yield ROOT, None
        return
    durable = report.with_name(report.stem+'-source.json')
    if durable.exists() or durable.is_symlink():
        raise ValueError('original pinned-source receipt exists; observe that operation')
    receipt = pinned_source.prepare(ROOT, commit, report)
    state['source_binding'] = str(receipt)
    previous_bytecode = sys.dont_write_bytecode
    sys.dont_write_bytecode = True
    try:
        with durable.open('xb') as stream:
            stream.write(receipt.read_bytes())
            stream.flush(); os.fsync(stream.fileno())
        state['source_record'] = str(durable)
        yield pathlib.Path(pinned_source.read(receipt, ROOT)['source_root']), receipt
    finally:
        settled = all(stage.get('cleanup', {}).get('state') != 'unconfirmed'
                      for stage in state['stages'].values())
        if settled:
            try:
                state['source_cleanup'] = pinned_source.cleanup(receipt, ROOT)
            except Exception as error:
                state['source_cleanup'] = {'state': 'deferred', 'reason': str(error), 'input_preserved': True}
        else:
            state['source_cleanup'] = {'state': 'deferred', 'reason': 'original child cleanup unconfirmed', 'input_preserved': True}
        if state.get('source_record'):
            build_slot.atomic_json(durable, load(receipt))
        build_slot.atomic_json(report, state)
        sys.dont_write_bytecode = previous_bytecode


def load(path):
    return json.loads(path.read_text())


def stamp():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def prepare_build_limits(resource_api=None, platform=None):
    """Raise only the builder's soft limit before expensive compilation.

    macOS launchd commonly supplies 256 descriptors, which is insufficient for
    large Zig link steps. Never alter the system setting or hard resource limit.
    """
    if (platform or sys.platform) != 'darwin':
        return {'state': 'not_required', 'scope': 'builder_process_tree'}
    if resource_api is None:
        import resource as resource_api
    soft, hard = resource_api.getrlimit(resource_api.RLIMIT_NOFILE)
    minimum = 4096
    if soft != resource_api.RLIM_INFINITY and soft < minimum:
        if hard != resource_api.RLIM_INFINITY and hard < minimum:
            raise ValueError('build_fd_limit_insufficient: hard limit below 4096; no compiler started')
        resource_api.setrlimit(resource_api.RLIMIT_NOFILE, (minimum, hard))
    after, after_hard = resource_api.getrlimit(resource_api.RLIMIT_NOFILE)
    if after_hard != hard or (after != resource_api.RLIM_INFINITY and after < minimum):
        raise ValueError('build_fd_limit_unconfirmed; no compiler started')
    return {'state': 'ready', 'before_soft': soft, 'after_soft': after,
            'hard': hard, 'scope': 'builder_process_tree', 'global_limits_changed': False}


def current_status(report):
    report = pathlib.Path(report)
    if not report.exists():
        launch = report.with_name(report.stem+'-launch.json')
        result = load(launch)
        result['phase'] = 'worker_startup'
        result['observation'] = {'observed_at': stamp(), 'mutates_build': False}
        return result
    result = load(report)
    result['observation'] = {'observed_at': stamp(), 'mutates_build': False,
                             'seconds_since_update': max(0, time.time()-result.get('updated_epoch', time.time()))}
    return result


def progress_log(name, checkout, directory, fallback):
    """Observe the actual Cargo log, not only the verifier's stage summaries."""
    if name != 'verification':
        return fallback, None
    try:
        proof = load(directory/'verification.json')
        active = [(key, value) for key, value in proof.get('checks', {}).items()
                  if value.get('state') == 'running']
        if active:
            key, value = active[-1]
            path = checkout/value['log']
            if path.resolve().is_relative_to(checkout.resolve()) and path.is_file():
                return path, key
        return fallback, proof.get('state')
    except (OSError, ValueError, KeyError, TypeError):
        return fallback, None


def run_stage(name, argv, checkout, directory, report, state, env, timeout=1800, manifest=None):
    """Observe durable output and raw exit; timeout bounds inactivity only."""
    path = directory/(name+'.log')
    check = {'state': 'running', 'started_at': stamp(), 'log': str(path), 'exit_code': None}
    state['stages'][name] = check
    state.update(state='running', phase=name, phase_started_at=stamp(), activity='starting')

    def progress(value):
        check.update(value)
        state.update({key: value[key] for key in
                      ('activity', 'stage_elapsed_seconds', 'seconds_since_observed_progress',
                       'log_bytes', 'cpu_delta_seconds', 'child_processes_observed',
                       'heartbeat', 'progress', 'verification_gate', 'observed_log') if key in value})
        if value.get('process_id'):
            state['child_pid'] = value['process_id']
        state.update(updated_at=stamp(), updated_epoch=time.time())
        build_slot.atomic_json(report, state)

    build_slot.atomic_json(report, state)
    print(json.dumps({'phase': name, 'state': 'running', 'report': str(report)}), flush=True)
    outcome = build_process.run(
        argv, checkout, path, env, idle_timeout=timeout, on_update=progress,
        observed_log=lambda: progress_log(name, checkout, directory, path),
        exclude_root_cpu=name == 'verification')
    check.update(outcome, sha256=build_slot.digest(path) if path.is_file() else None)
    if outcome.get('cleanup', {}).get('state') != 'unconfirmed':
        state.pop('child_pid', None)
    state.update(updated_at=stamp(), updated_epoch=time.time())
    build_slot.atomic_json(report, state)
    print(json.dumps({'phase': name, 'state': check['state'], 'exit_code': check['exit_code'],
                      'output_complete': check['output_complete'], 'exit_receipt': check['exit_receipt'],
                      'elapsed_seconds': check['elapsed_seconds']}), flush=True)
    if outcome['state'] == 'interrupted':
        raise KeyboardInterrupt
    if outcome['state'] != 'finished' or outcome['exit_code'] != 0 or not outcome['output_complete']:
        raise RuntimeError('stage_failed:'+name+'; inspect saved log, do not duplicate the build')
    if manifest is not None:
        release_current.check_inputs(checkout, manifest)
    else:
        source_snapshot.check(checkout)


def pinned_rust_lld(source_root=None):
    """ARM musl final links retain Rust's A53 erratum fix unsupported by Zig cc.

    Keep Zig for cross C compilation and use the pinned Rust toolchain's LLD
    for the final ELF link. No global Cargo configuration or flags are changed.
    """
    channel = tomllib.loads(((source_root or ROOT)/'rust-toolchain.toml').read_text())['toolchain']['channel']
    prefix = ['rustup', 'run', channel, 'rustc']
    sysroot = pathlib.Path(subprocess.check_output(prefix + ['--print', 'sysroot'], text=True, timeout=30).strip())
    version = subprocess.check_output(prefix + ['-vV'], text=True, timeout=30)
    host = next(line.removeprefix('host: ') for line in version.splitlines() if line.startswith('host: '))
    linker = sysroot/'lib'/'rustlib'/host/'bin'/'rust-lld'
    if not linker.is_file() or not os.access(linker, os.X_OK):
        raise ValueError('arm64_musl_linker_unavailable: pinned rust-lld required; mitigation not removed')
    return linker


def arm64_musl_linker_configs(linker):
    # Rust's default --fix-cortex-a53-843419 is emitted only for the cc
    # flavor. Switching to LLD must explicitly preserve it, not merely link.
    overrides = ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS',
                 'CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS')
    if any(os.environ.get(name) for name in overrides):
        raise ValueError('arm64_musl_flag_override: cannot prove erratum mitigation; no build started')
    return (
        'target.aarch64-unknown-linux-musl.linker=' + json.dumps(str(linker)),
        'target.aarch64-unknown-linux-musl.rustflags=' +
        json.dumps(['-C', 'link-arg=--fix-cortex-a53-843419']),
    )


def build_commands(platforms=None, arm64_linker=None, source_root=None):
    selected = release_current.selected_platforms(platforms)
    native = ['cargo', 'build', '-p', 'octendra-code', '-p', 'octendra-cli', '--release', '--locked']
    stages = [('macos_release', native)]  # Native release-manifest producer is always required.
    for platform, label, target, driver in [
        ('linux', 'linux_release', 'x86_64-unknown-linux-musl', ['cargo', 'zigbuild']),
        ('windows', 'windows_release', 'x86_64-pc-windows-msvc', ['cargo', 'xwin', 'build']),
        ('linux-arm64', 'linux_arm64_release', 'aarch64-unknown-linux-musl', ['cargo', 'zigbuild']),
    ]:
        if platform in selected:
            args = driver + native[2:] + ['--target', target]
            if platform == 'linux-arm64':
                linker = arm64_linker if arm64_linker is not None else (pinned_rust_lld(source_root) if source_root else pinned_rust_lld())
                for config in arm64_musl_linker_configs(linker):
                    args += ['--config', config]
            stages.append((label, args))
    return stages


@release_current.admission()
def run_pipeline(snapshot, report, slot, verify_only=False, expected_source_id=None, platforms=None, source_commit=None):
    platforms = release_current.selected_platforms(platforms)
    snapshot = snapshot.resolve(strict=True) if snapshot is not None else None
    report = report.absolute()
    if source_commit:
        release_current.primary(ROOT)
        if snapshot is not None:
            raise ValueError('source-commit and legacy snapshot evidence are mutually exclusive')
        source_commit = pinned_source.resolve(ROOT, source_commit)
    manifest = release_current.snapshot_evidence(snapshot, ROOT) if not source_commit else None
    if expected_source_id is not None and (manifest is None or manifest['snapshot_id'] != expected_source_id):
        raise ValueError('main inputs changed after launch; preserve the original operation and edits')
    version = tomllib.loads((ROOT/'crates/octendra-code/Cargo.toml').read_text())['package']['version'] if not source_commit else None
    report.parent.mkdir(parents=True, exist_ok=True)
    if report.exists():
        previous = load(report)
        if (previous.get('source_commit') != source_commit
                or (not source_commit and previous.get('snapshot_id') != manifest['snapshot_id'])
                or previous.get('verify_only') != verify_only
                or tuple(previous.get('platforms', release_current.DEFAULT_PLATFORMS)) != platforms):
            raise ValueError('report belongs to different inputs or mode; never overwrite')
        print(json.dumps({'state': 'existing_attempt', 'original_state': previous['state'],
                          'report': str(report), 'action': 'observe_original; no build started'}), flush=True)
        return previous
    state = {'schema_version': 1, 'version': version, 'state': 'preparing', 'phase': 'lease',
             'snapshot_id': manifest['snapshot_id'] if manifest else None, 'source_commit': source_commit,
             'snapshot': str(snapshot) if snapshot is not None else None, 'verify_only': verify_only, 'platforms': list(platforms),
             'started_at': stamp(), 'pid': os.getpid(), 'stages': {}, 'deployed': False}
    # Atomic claim prevents racing requests with the same report id.
    with report.open('x') as stream:
        json.dump(state, stream, indent=2)
    admitted = False
    committed = False
    try:
        with shared_build_lock(bool(source_commit)), build_slot.lease(slot, report, manifest['snapshot_id'] if manifest else source_commit, execution_root=ROOT):
            with execution_source(source_commit, report, state) as (checkout, source_binding):
                if source_binding:
                    manifest = release_current.snapshot_evidence(None, checkout, source_binding, ROOT)
                    if manifest['source_commit'] != source_commit:
                        raise ValueError('pinned input differs from requested source commit')
                    version = tomllib.loads((checkout/'crates/octendra-code/Cargo.toml').read_text())['package']['version']
                    state.update(version=version, snapshot_id=manifest['snapshot_id'], producer_commit=manifest['producer_commit'])
                state['build_process_limits'] = prepare_build_limits()
                build_slot.atomic_json(report, state)
                state['source_validation'] = {'execution_root': str(checkout), 'snapshot_is_evidence_only': True,
                                              'source_commit': source_commit}
                release_current.check_inputs(checkout, manifest)
                state['current_preflight'] = release_current.preflight(ROOT)
                if not verify_only:
                    state['current_admission'] = release_current.admit(ROOT)
                    admitted = True
                directory = report.parent/(report.stem+'-logs')
                directory.mkdir(exist_ok=False)
                state['execution_root'] = str(checkout)
                env = dict(os.environ, CARGO_TERM_COLOR='never', PYTHONUNBUFFERED='1',
                           CARGO_BUILD_JOBS=str(min(4, max(1, os.cpu_count() or 1))))
                if source_binding:
                    env.update(CARGO_TARGET_DIR=str((pathlib.Path.home()/'rust-target').resolve(strict=True)),
                               PYTHONDONTWRITEBYTECODE='1', GIT_CEILING_DIRECTORIES=str(checkout.parent))
                state['cargo_target_dir'] = str(release_current.established_target(checkout, env))
                # Cold toolchain startup on a busy workstation is not a lease failure.
                state.update(phase='toolchain_probe', state='running')
                build_slot.atomic_json(report, state)
                state['toolchain'] = subprocess.check_output(['rustc', '-vV'], cwd=checkout, env=env, text=True, timeout=60).strip()
                verify_argv = [sys.executable, str(ROOT/'scripts/check-code-source.py'), '--report', str(directory/'verification.json')]
                if source_binding: verify_argv += ['--source-binding', str(source_binding)]
                run_stage('verification', verify_argv,
                          checkout, directory, report, state, env, timeout=5400, manifest=manifest)
                proof = load(directory/'verification.json')
                verifier = source_snapshot.verifier(checkout)
                if not verifier.receipt_current(proof, checkout):
                    raise ValueError('source verification failed or is no longer current')
                state['verification'] = {'path': str(directory/'verification.json'), 'sha256': build_slot.digest(directory/'verification.json'),
                                         'tests': proof['functional_tests']}
                # The completed same-source receipt supersedes the last sampled
                # progress tick, which may still have said running.
                state['verification_gate'] = 'passed'
                build_slot.atomic_json(report, state)
                if not verify_only:
                    commands = build_commands(platforms, source_root=checkout) if source_binding else build_commands(platforms)
                    for name, args in commands:
                        run_stage(name, args, checkout, directory, report, state, env, manifest=manifest)
                    package_argv = [sys.executable, str(ROOT/'scripts/package-code-release.py'), '--version', version,
                                         '--verification', str(directory/'verification.json'), '--producer-pid', str(os.getpid()),
                                         '--platforms', *platforms]
                    if source_binding: package_argv += ['--source-binding', str(source_binding)]
                    run_stage('package', package_argv,
                              checkout, directory, report, state, env, manifest=manifest)
                    release, bundle = release_current.stages(ROOT)
                    packaged = load(release/'manifest.json')
                    if packaged['source_inputs'] != manifest['source_inputs']:
                        raise ValueError('packaged source differs from verified snapshot')
                    for filename, info in packaged['artifacts'].items():
                        if pathlib.Path(filename).name != filename or build_slot.digest(release/filename) != info['sha256']:
                            raise ValueError('packaged artifact checksum mismatch')
                    if not bundle.is_file():
                        raise ValueError('release bundle missing after package stage')
                    release_current.check_inputs(checkout, manifest)
                    state['current_publication'] = release_current.publish(ROOT)
                    committed = True
                    release, bundle = release_current.files(ROOT)
                    state['package'] = {'path': str(release), 'manifest_sha256': build_slot.digest(release/'manifest.json'),
                                        'bundle': str(bundle), 'bundle_sha256': build_slot.digest(bundle),
                                        'bundle_size': bundle.stat().st_size, 'artifacts': packaged['artifacts']}
                if source_binding:
                    release_current.snapshot_evidence(None, checkout, source_binding, ROOT)
                else:
                    release_current.snapshot_evidence(snapshot, checkout)
                release_current.check_inputs(checkout, manifest)
                state.update(state='passed', phase='finished', source_inputs_unchanged=True)
    except build_slot.SlotBusy as error:
        state.update(state='busy', phase='lease', owner=error.owner,
                     next_action='observe owner.report; this attempt did not start compilation')
    except (Exception, KeyboardInterrupt) as error:
        state.update(state='interrupted' if isinstance(error, KeyboardInterrupt) else 'failed',
                     failure_type=type(error).__name__, next_action='inspect the saved stage log; do not rerun an unknown operation')
        if isinstance(error, (ValueError, FileExistsError, RuntimeError, subprocess.TimeoutExpired)):
            state['failure_detail'] = str(error)[:400]
    finally:
        if admitted and not committed:
            settled = all(stage.get('cleanup', {}).get('state') != 'unconfirmed'
                          for stage in state['stages'].values())
            state['current_recovery'] = (release_current.abort(ROOT) if settled else
                {'status': 'deferred', 'reason': 'owned child cleanup unconfirmed', 'artifacts_preserved': True})
        state.update(updated_at=stamp(), updated_epoch=time.time())
        build_slot.atomic_json(report, state)
    print(json.dumps({k: state.get(k) for k in ('state', 'version', 'phase', 'snapshot_id', 'verification', 'package', 'next_action')}), flush=True)
    return state


def start_pipeline(snapshot, report, verify_only=False, platforms=None, source_commit=None):
    """Submit once; observation or launcher timeout never owns the worker group."""
    platforms = release_current.selected_platforms(platforms)
    snapshot = snapshot.resolve(strict=True) if snapshot is not None else None
    report = report.absolute()
    if source_commit:
        release_current.primary(ROOT)
        if snapshot is not None: raise ValueError('source-commit and snapshot are mutually exclusive')
        source_commit = pinned_source.resolve(ROOT, source_commit)
    manifest = release_current.snapshot_evidence(snapshot, ROOT) if not source_commit else {'snapshot_id': None}
    report.parent.mkdir(parents=True, exist_ok=True)
    launch = report.with_name(report.stem+'-launch.json')
    driver_log = report.with_name(report.stem+'-driver.log')
    if report.exists() or launch.exists():
        print(json.dumps({'state': 'existing_attempt', 'report': str(report),
                          'launch_receipt': str(launch), 'action': 'observe_original; no process started'}), flush=True)
        return
    # An interrupted launch claim remains evidence; no automatic second spawn.
    intent = {'schema_version': 1, 'state': 'launching', 'snapshot_id': manifest['snapshot_id'],
              'snapshot': str(snapshot) if snapshot is not None else None, 'report': str(report), 'driver_log': str(driver_log),
              'verify_only': verify_only, 'platforms': list(platforms), 'started_at': stamp(), 'launcher_pid': os.getpid()}
    if source_commit: intent['source_commit'] = source_commit
    try:
        with launch.open('x') as stream:
            json.dump(intent, stream, indent=2);stream.flush();os.fsync(stream.fileno())
    except FileExistsError:
        print(json.dumps({'state': 'existing_attempt', 'launch_receipt': str(launch),
                          'action': 'observe_original; no process started'}), flush=True)
        return
    worker_pid = None
    try:
        argv = [sys.executable, str(pathlib.Path(__file__).resolve()), '--report', str(report)]
        if source_commit: argv.extend(['--source-commit', source_commit])
        else: argv.extend(['--expected-source-id', manifest['snapshot_id']])
        if platforms != release_current.DEFAULT_PLATFORMS:
            argv.extend(['--platforms', *platforms])
        if snapshot is not None:
            argv.extend(['--snapshot', str(snapshot)])
        if verify_only:
            argv.append('--verify-only')
        # posix_spawn has no un-reaped Popen object in this short-lived launcher.
        # The new session owns its stdout file and survives a tool's observation timeout.
        with driver_log.open('xb', buffering=0) as output, open(os.devnull, 'rb') as null:
            actions = [(os.POSIX_SPAWN_DUP2, null.fileno(), 0),
                       (os.POSIX_SPAWN_DUP2, output.fileno(), 1),
                       (os.POSIX_SPAWN_DUP2, output.fileno(), 2)]
            worker_pid = os.posix_spawn(sys.executable, argv,
                                       dict(os.environ, PYTHONUNBUFFERED='1', PYTHONDONTWRITEBYTECODE='1'),
                                       file_actions=actions, setsid=True)
        intent.update(state='submitted', worker_pid=worker_pid, worker_process_group=worker_pid)
        build_slot.atomic_json(launch, intent)
    except BaseException as error:
        # If spawn succeeded, retain its identity; never cancel it on observation failure.
        intent.update(state='submitted_receipt_error' if worker_pid else 'launch_failed',
                      failure_type=type(error).__name__, worker_pid=worker_pid)
        build_slot.atomic_json(launch, intent)
        raise
    print(json.dumps(intent), flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--snapshot', type=pathlib.Path)
    parser.add_argument('--source-commit', help='Exact full Git commit; temporary input owned by the original producer')
    parser.add_argument('--report', type=pathlib.Path, required=True)
    parser.add_argument('--status', action='store_true')
    parser.add_argument('--start', action='store_true', help='Submit one detached, durable build; never replay an existing report/launch')
    parser.add_argument('--verify-only', action='store_true')
    parser.add_argument('--platforms', nargs='+', choices=release_current.PLATFORMS,
                        help='Explicit package platforms; omitted platforms remain pending, never accepted')
    parser.add_argument('--expected-source-id', help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.status:
        print(json.dumps(current_status(args.report)), flush=True)
        return
    if os.name != 'posix':
        parser.error('a POSIX build host is required')
    if args.start:
        start_pipeline(args.snapshot, args.report, args.verify_only, args.platforms, args.source_commit)
        return
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    result = run_pipeline(args.snapshot, args.report, ROOT/'target/release-slot', args.verify_only, args.expected_source_id, args.platforms, args.source_commit)
    if result['state'] != 'passed':
        raise SystemExit(1)


if __name__ == '__main__':
    main()
