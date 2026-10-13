"""Release lifecycle tests use fake clocks, temporary files and mocked launchd."""
import argparse
import copy
import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import agent_upgrade_support as support

spec = importlib.util.spec_from_file_location('launcher031', SCRIPTS / 'launch-code-upgrade.py')
launcher = importlib.util.module_from_spec(spec)
spec.loader.exec_module(launcher)


class ReadyTests(unittest.TestCase):
    def setUp(self):
        self.local = {'running': True, 'pid': 42, 'runs': '1',
                      'build': {'pid': 42, 'version': '0.3.1', 'started_at': 100},
                      'readiness': {'pid': 42, 'version': '0.3.1', 'session': 'a' * 64,
                                    'phase': 'polling', 'lanes': {x: 119 for x in support.LANES}}}
        self.remote = {'ready': True, 'agent_version': '0.3.1', 'last_seen': 120, 'observed_at': 120, 'session': 'a' * 64}

    def test_matching_pid_alone_never_passes(self):
        local = copy.deepcopy(self.local)
        local.pop('readiness')
        self.assertIsNone(support.sample_identity(local, self.remote, '0.3.1', 100, 99, now=120))
        self.assertIsNotNone(support.sample_identity(self.local, self.remote, '0.3.1', 100, 99, now=120))

    def test_stale_missing_future_lanes_or_wrong_session_never_pass(self):
        for change in ('stale', 'missing', 'future', 'pid', 'session', 'phase'):
            local = copy.deepcopy(self.local)
            if change == 'stale': local['readiness']['lanes']['read'] = 100
            elif change == 'missing': del local['readiness']['lanes']['control']
            elif change == 'future': local['readiness']['lanes']['read'] = 999
            elif change == 'pid': local['pid'] = 99
            elif change == 'session': local['readiness']['session'] = 'b' * 64
            else: local['readiness']['phase'] = 'gateway_wait'
            self.assertIsNone(support.sample_identity(local, self.remote, '0.3.1', 100, 99, now=160), change)

    def test_gateway_must_advance_since_preflight(self):
        self.assertIsNone(support.sample_identity(self.local, self.remote, '0.3.1', 100, 120, now=120))
        remote = dict(self.remote, ready=False)
        self.assertIsNone(support.sample_identity(self.local, remote, '0.3.1', 100, 99, now=120))

    def test_stability_window_survives_repeated_checks_without_mutations(self):
        clock = [0.0]
        def advancing_local(_base):
            value = copy.deepcopy(self.local)
            value['readiness']['lanes'] = {lane: 120+int(clock[0]) for lane in support.LANES}
            return value
        def advancing_remote(_config, **_options):
            return dict(self.remote, last_seen=120+int(clock[0]), observed_at=120+int(clock[0]))
        with mock.patch.object(support.time, 'monotonic', side_effect=lambda: clock[0]), \
             mock.patch.object(support.time, 'time', side_effect=lambda: 120+int(clock[0])), \
             mock.patch.object(support.time, 'sleep', side_effect=lambda dt: clock.__setitem__(0, clock[0] + dt)), \
             mock.patch.object(support, 'local_observation', side_effect=advancing_local), \
             mock.patch.object(support, 'gateway_observation', side_effect=advancing_remote):
            result = support.wait_ready(pathlib.Path('/unused'), {}, '0.3.1', 100, 99, timeout=20, stable_seconds=5)
        self.assertGreaterEqual(result['stable_seconds'], 5)
        self.assertTrue(result['gateway_verified'])
        self.assertTrue(result['all_lanes_verified'])
        self.assertEqual(result['functional_acceptance'], 'separate_required_gate')

    def test_brief_gateway_outage_preserves_candidate_identity_but_requires_fresh_progress(self):
        clock = [0.0]
        def advancing_local(_base):
            value = copy.deepcopy(self.local)
            value['readiness']['lanes'] = {lane: 120+int(clock[0]) for lane in support.LANES}
            return value
        def intermittent_remote(_config, **_options):
            if 2 <= clock[0] < 5:
                raise support.GatewayObservationError({'stage':'gateway_readiness','category':'network_timeout',
                    'retryable':True,'attempts':1,'next_action':'retry_same_readonly_probe_with_backoff'})
            return dict(self.remote, last_seen=120+int(clock[0]), observed_at=120+int(clock[0]))
        with mock.patch.object(support.time, 'monotonic', side_effect=lambda: clock[0]), \
             mock.patch.object(support.time, 'time', side_effect=lambda: 120+int(clock[0])), \
             mock.patch.object(support.time, 'sleep', side_effect=lambda dt: clock.__setitem__(0, clock[0]+dt)), \
             mock.patch.object(support, 'local_observation', side_effect=advancing_local), \
             mock.patch.object(support, 'gateway_observation', side_effect=intermittent_remote):
            result = support.wait_ready(pathlib.Path('/unused'), {}, '0.3.1', 100, 99,
                                        timeout=20, stable_seconds=5, transient_grace_seconds=5)
        self.assertEqual(result['transient_interruptions'], 1)
        self.assertGreaterEqual(result['max_transient_gap_seconds'], 3)
        self.assertEqual(result['last_transient_category'], 'network_timeout')
        self.assertTrue(result['all_lanes_verified'])

    def test_stopped_candidate_fails_before_long_network_deadline(self):
        clock = [0.0]
        def local(_base):
            value = copy.deepcopy(self.local)
            value['readiness']['lanes'] = {lane: 120+int(clock[0]) for lane in support.LANES}
            if clock[0] >= 2:
                value['running'] = False
            return value
        def remote(_config, **_options):
            return dict(self.remote, last_seen=120+int(clock[0]), observed_at=120+int(clock[0]))
        with mock.patch.object(support.time, 'monotonic', side_effect=lambda: clock[0]), \
             mock.patch.object(support.time, 'time', side_effect=lambda: 120+int(clock[0])), \
             mock.patch.object(support.time, 'sleep', side_effect=lambda dt: clock.__setitem__(0, clock[0]+dt)), \
             mock.patch.object(support, 'local_observation', side_effect=local), \
             mock.patch.object(support, 'gateway_observation', side_effect=remote):
            with self.assertRaisesRegex(support.CandidateProcessError, 'candidate_not_running'):
                support.wait_ready(pathlib.Path('/unused'), {}, '0.3.1', 100, 99,
                                   timeout=30, stable_seconds=5, dead_process_grace_seconds=2)
        self.assertLess(clock[0], 10)

    def test_changing_pid_session_times_out_instead_of_flapping_success(self):
        clock = [0.0]
        def changing(_base):
            local = copy.deepcopy(self.local)
            pid = 42 + int(clock[0])
            local['pid'] = local['build']['pid'] = local['readiness']['pid'] = pid
            return local
        with mock.patch.object(support.time, 'monotonic', side_effect=lambda: clock[0]), \
             mock.patch.object(support.time, 'time', return_value=120), \
             mock.patch.object(support.time, 'sleep', side_effect=lambda dt: clock.__setitem__(0, clock[0] + dt)), \
             mock.patch.object(support, 'local_observation', side_effect=changing), \
             mock.patch.object(support, 'gateway_observation', return_value=self.remote):
            with self.assertRaisesRegex(RuntimeError, 'readiness_timeout'):
                support.wait_ready(pathlib.Path('/unused'), {}, '0.3.1', 100, 99, timeout=10, stable_seconds=5)


class LauncherTests(unittest.TestCase):
    def test_plist_has_no_reentry_triggers_or_shell_wrapper(self):
        p = pathlib.Path('/absolute')
        value = launcher.make_plist('label', p/'python', p/'script', p/'candidate', 'a'*64, '0.3.1', p/'result', p)
        self.assertIs(value['RunAtLoad'], True)
        self.assertIs(value['KeepAlive'], False)
        for key in ('StartInterval', 'StartCalendarInterval', 'WatchPaths', 'QueueDirectories', 'Sockets'):
            self.assertNotIn(key, value)
        self.assertEqual(value['ProgramArguments'][0], '/absolute/python')
        self.assertNotIn('sh', value['ProgramArguments'])
        stable = launcher.stable_plist(p/'python', {'runner':p/'runner','request':p/'request','directory':p})
        self.assertIs(stable['RunAtLoad'], False)
        self.assertEqual(stable['Label'], 'com.remote-hosts.code-upgrade')

    def test_prepare_snapshots_updater_and_reuses_same_job(self):
        with tempfile.TemporaryDirectory() as d:
            base = pathlib.Path(d)
            candidate = base/'candidate'
            candidate.write_bytes(b'candidate')
            args = argparse.Namespace(candidate=candidate, sha256=launcher.sha(candidate), version='0.3.1')
            record = launcher.prepare(args, base)
            self.assertEqual(record, launcher.prepare(args, base))
            directory = pathlib.Path(record['directory'])
            self.assertEqual((directory/'upgrade-code-agent.py').read_bytes(), (SCRIPTS/'upgrade-code-agent.py').read_bytes())
            self.assertEqual((directory/'macos_code_identity.py').read_bytes(), (SCRIPTS/'macos_code_identity.py').read_bytes())
            (directory/'upgrade-code-agent.py').write_text('changed')
            with self.assertRaisesRegex(ValueError, 'changed'):
                launcher.prepare(args, base)

    def test_bootstrap_is_requested_once_even_after_completion(self):
        with tempfile.TemporaryDirectory() as d:
            candidate = pathlib.Path(d)/'candidate'
            candidate.write_bytes(b'fixture')
            record = launcher.prepare(argparse.Namespace(candidate=candidate, sha256=launcher.sha(candidate), version='0.3.1'), pathlib.Path(d))
            unloaded={'loaded':False,'running':False,'service':'gui/501/com.remote-hosts.code-upgrade'}
            with mock.patch.object(launcher, 'service_state', side_effect=[unloaded, unloaded]), \
                 mock.patch.object(launcher, 'cleanup_legacy_jobs', return_value=0), \
                 mock.patch.object(launcher.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
                self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'started')
                pathlib.Path(record['result']).write_text('{"state":"upgraded"}')
                self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'already_requested')
            self.assertEqual(run.call_count, 2)
            self.assertEqual(run.call_args_list[0].args[0][1], 'bootstrap')
            self.assertEqual(run.call_args_list[1].args[0][1], 'kickstart')

    def test_uncertain_bootstrap_outcome_is_not_automatically_replayed(self):
        with tempfile.TemporaryDirectory() as d:
            candidate = pathlib.Path(d)/'candidate'
            candidate.write_bytes(b'fixture')
            record = launcher.prepare(argparse.Namespace(candidate=candidate, sha256=launcher.sha(candidate), version='0.3.1'), pathlib.Path(d))
            unloaded={'loaded':False,'running':False,'service':'gui/501/com.remote-hosts.code-upgrade'}
            with mock.patch.object(launcher, 'service_state', side_effect=[unloaded, unloaded]), \
                 mock.patch.object(launcher, 'cleanup_legacy_jobs', return_value=0), \
                 mock.patch.object(launcher.subprocess, 'run', side_effect=subprocess.TimeoutExpired('launchctl', 10)) as run:
                self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'kickstart_outcome_unknown')
                self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'already_requested')
            run.assert_called_once()

    def test_shared_preparation_lock_blocks_a_second_dispatch(self):
        import fcntl
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            candidate = root/'candidate'; candidate.write_bytes(b'fixture')
            record = launcher.prepare(argparse.Namespace(candidate=candidate, sha256=launcher.sha(candidate), version='0.3.1'), root)
            with (pathlib.Path(record['directory']).parent/'.prepare.lock').open('a+b') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                with mock.patch.object(launcher, 'service_state') as state, mock.patch.object(launcher.subprocess, 'run') as run:
                    self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'updater_busy')
                state.assert_not_called(); run.assert_not_called()
            self.assertFalse((pathlib.Path(record['directory'])/'start-requested.json').exists())

    def test_scheduled_dispatcher_blocks_replacement_without_a_pid(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            candidate = root/'candidate'; candidate.write_bytes(b'fixture')
            record = launcher.prepare(argparse.Namespace(candidate=candidate, sha256=launcher.sha(candidate), version='0.3.1'), root)
            scheduled = {'loaded': True, 'running': False, 'state': 'spawn scheduled', 'pid': None}
            with mock.patch.object(launcher, 'service_state', return_value=scheduled), mock.patch.object(launcher.subprocess, 'run') as run:
                self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'updater_busy')
            run.assert_not_called()
            self.assertFalse((pathlib.Path(record['directory'])/'start-requested.json').exists())

    def test_unknown_previous_dispatch_cannot_be_replaced_by_a_new_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            candidate = root/'candidate'; candidate.write_bytes(b'fixture')
            first = launcher.prepare(argparse.Namespace(candidate=candidate, sha256=launcher.sha(candidate), version='0.3.1'), root)
            second = launcher.prepare(argparse.Namespace(candidate=candidate, sha256=launcher.sha(candidate), version='0.3.2'), root)
            paths = launcher.stable_paths(root); paths['directory'].mkdir()
            original = json.dumps({'directory': first['directory'], 'prepared_sha256': launcher.sha(pathlib.Path(first['directory'])/'prepared.json')}).encode()
            paths['request'].write_bytes(original)
            (pathlib.Path(first['directory'])/'start-requested.json').write_text('{"state":"kickstart_requested"}')
            idle = {'loaded': True, 'running': False, 'state': 'not running', 'pid': None}
            with mock.patch.object(launcher, 'service_state', return_value=idle), mock.patch.object(launcher.subprocess, 'run') as run:
                self.assertEqual(launcher.start_once(second, require_identity=False)['state'], 'updater_busy')
            self.assertEqual(paths['request'].read_bytes(), original)
            self.assertFalse((pathlib.Path(second['directory'])/'start-requested.json').exists())
            run.assert_not_called()

    def test_service_identity_uses_arguments_and_ignores_environment_text(self):
        argv = '\n'.join('\t\t'+a for a in ['/python', '/fixture/upgrade-code-agent.py', '--candidate', '/candidate', '--sha256', 'a'*64, '--version', '0.3.1', '--result', '/result'])
        output = '\tstate = spawn scheduled\n\targuments = {\n'+argv+'\n\t}\n'
        with mock.patch.object(launcher.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, stdout=output)):
            self.assertTrue(launcher.service_state('gui/501', 'legacy')['updater_job'])
        fake = '\tstate = spawn scheduled\n\tenvironment = {\n\t\tFAKE = upgrade-code-agent.py --candidate --sha256 --version --result\n\t}\n'
        with mock.patch.object(launcher.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, stdout=fake)):
            self.assertFalse(launcher.service_state('gui/501', 'legacy')['updater_job'])

    def test_cleanup_recognizes_both_legacy_names_without_touching_unknown_jobs(self):
        old = 'com.remote-hosts.code-agent-upgrade.0.10.35.v7.20261012'
        previous = 'com.remote-hosts.code-upgrade.abcdef'
        unknown = 'com.remote-hosts.code-agent-upgrade.unrelated'
        listing = '\n'.join([old, previous, unknown, launcher.STABLE_LABEL, 'com.remote-hosts.code-agent'])
        def state(domain, label):
            return {'loaded': True, 'running': False, 'pid': None, 'state': 'spawn scheduled',
                    'service': domain+'/'+label, 'updater_job': label != unknown}
        with mock.patch.object(launcher, 'service_state', side_effect=state), \
             mock.patch.object(launcher.subprocess, 'run', side_effect=[subprocess.CompletedProcess([], 0, stdout=listing), subprocess.CompletedProcess([], 0), subprocess.CompletedProcess([], 0)]) as run:
            self.assertEqual(launcher.cleanup_legacy_jobs('gui/501'), 2)
        removed = {call.args[0][2] for call in run.call_args_list if call.args[0][1] == 'bootout'}
        self.assertEqual(removed, {'gui/501/'+old, 'gui/501/'+previous})

    def test_live_or_exiting_legacy_updater_and_failed_cleanup_block_launch(self):
        label = 'com.remote-hosts.code-agent-upgrade.0.10.35.v7'
        for running, pid, boot_exit in [(True, 42, 0), (False, 42, 0), (False, None, 5)]:
            with self.subTest(running=running, pid=pid, exit=boot_exit):
                state = {'loaded': True, 'running': running, 'pid': pid, 'updater_job': True, 'service': 'gui/501/'+label}
                outputs = [subprocess.CompletedProcess([], 0, stdout=label), subprocess.CompletedProcess([], boot_exit)]
                with mock.patch.object(launcher, 'service_state', return_value=state), mock.patch.object(launcher.subprocess, 'run', side_effect=outputs) as run:
                    self.assertIsNone(launcher.cleanup_legacy_jobs('gui/501'))
                self.assertEqual(run.call_count, 1 if pid else 2)

    def test_failed_domain_observation_does_not_claim_legacy_cleanup(self):
        with mock.patch.object(launcher.subprocess, 'run', return_value=subprocess.CompletedProcess([], 5)):
            self.assertIsNone(launcher.cleanup_legacy_jobs('gui/501'))

    def test_scoped_bundle_can_prepare_the_canonical_launcher(self):
        import ast
        import shutil
        tree = ast.parse((SCRIPTS/'release-code-scoped.py').read_text())
        definition = next(node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'updater_helpers')
        namespace = {}
        exec(compile(ast.Module(body=[definition], type_ignores=[]), 'release-code-scoped.py', 'exec'), namespace)
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp); package = root/'package'; package.mkdir()
            for name in namespace['updater_helpers']('macos'):
                shutil.copy2(SCRIPTS/name, package/name)
            spec = importlib.util.spec_from_file_location('packaged_oneshot_fixture', package/'launch-code-upgrade.py')
            packaged = importlib.util.module_from_spec(spec); spec.loader.exec_module(packaged)
            candidate = package/'candidate'; candidate.write_bytes(b'fixture')
            record = packaged.prepare(argparse.Namespace(candidate=candidate, sha256=packaged.sha(candidate), version='0.3.1'), root/'owner')
            self.assertEqual(packaged.validate_prepared(record), record)
            self.assertTrue((package/'code_upgrade_runner.py').is_file())


if __name__ == '__main__':
    unittest.main()
