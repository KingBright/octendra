"""Explicit installation layouts: artificial files, no signing/network/services."""
import argparse
import contextlib
import hashlib
import importlib.util
import io
import json
import os
import pathlib
import plistlib
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
import agent_upgrade_support as support


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS/filename)
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


launcher = module('bound_launcher', 'launch-code-upgrade.py')
updater = module('bound_updater', 'upgrade-code-agent.py')
runner = module('bound_runner', 'code_upgrade_runner.py')


class InstanceUpgradeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.tmp.name).resolve()
        self.base = self.root/'arbitrary installation'
        self.binary = self.base/'executables/client'
        self.config = self.base/'private/settings.json'
        self.binary.parent.mkdir(parents=True)
        self.config.parent.mkdir()
        self.binary.write_bytes(b'old signed fixture')
        self.binary.chmod(0o700)
        self.state = self.base/'state'; self.state.mkdir()
        self.device = '11111111-1111-4111-8111-111111111111'
        self.config.write_text(json.dumps({'device_id': self.device, 'state_dir': str(self.state)}))
        self.config.chmod(0o600)
        self.signer = self.root/'existing signer'; self.signer.mkdir()
        self.definition = self.root/'existing.plist'
        self.label = 'example.existing.client'
        self.definition.write_bytes(plistlib.dumps({'Label': self.label,
            'ProgramArguments': [str(self.binary), 'agent', '--config', str(self.config)]}))
        self.definition.chmod(0o600)
        self.binding = {'schema': 1, 'base': str(self.base), 'binary': str(self.binary),
            'config': str(self.config), 'service_definition': str(self.definition),
            'service_label': self.label, 'signing_base': str(self.signer), 'device_id': self.device,
            'binary_sha256': launcher.sha(self.binary), 'definition_sha256': launcher.sha(self.definition)}
        self.candidate = self.root/'candidate'; self.candidate.write_bytes(b'new signed fixture')
        self.args = argparse.Namespace(candidate=self.candidate, sha256=launcher.sha(self.candidate),
            version='0.12.1', result=self.root/'result.json', idle_timeout=0,
            instance_binding=self.binding)

    def tearDown(self):
        self.tmp.cleanup()

    def prepare(self):
        return launcher.prepare(self.args, self.base)

    def test_binding_validation_never_reads_private_config_or_signing_material(self):
        original = pathlib.Path.open
        def checked(path, *args, **kwargs):
            if path == self.config or path.is_relative_to(self.signer):
                self.fail('private input read during metadata preparation')
            return original(path, *args, **kwargs)
        with mock.patch.object(pathlib.Path, 'open', checked):
            self.assertEqual(support.validate_instance_binding(self.binding, installed=True), self.binding)

    def test_wrong_definition_binary_config_permissions_and_symlink_are_rejected(self):
        for field, value in [('service_label', 'another.client'), ('binary_sha256', '0'*64),
                             ('device_id', '../invalid'), ('unexpected', 'credential')]:
            with self.subTest(field=field), self.assertRaises((ValueError, TypeError)):
                support.validate_instance_binding({**self.binding, field: value}, installed=True)
        self.config.chmod(0o644)
        with self.assertRaisesRegex(ValueError, 'private configuration'):
            support.validate_instance_binding(self.binding)
        self.config.chmod(0o600)
        target = self.root/'config-copy'; self.config.rename(target)
        self.config.symlink_to(target)
        with self.assertRaisesRegex(ValueError, 'canonical'):
            support.validate_instance_binding(self.binding)

    def test_writable_parent_cannot_replace_a_private_configuration(self):
        self.config.parent.chmod(0o777)
        with self.assertRaisesRegex(ValueError, 'ancestor ownership or write permissions'):
            support.validate_instance_binding(self.binding)

    def test_prepare_freezes_binding_and_runner_forwards_same_metadata(self):
        record = self.prepare()
        self.assertEqual(self.prepare(), record)
        self.assertEqual(record['instance_binding'], self.binding)
        self.assertNotEqual(record['dispatcher_label'], launcher.STABLE_LABEL)
        request = self.root/'request.json'
        request.write_text(json.dumps({'directory': record['directory'],
            'prepared_sha256': launcher.sha(pathlib.Path(record['directory'])/'prepared.json')}))
        with mock.patch.object(sys, 'argv', ['runner', '--request', str(request)]), \
             mock.patch.object(runner.os, 'execv') as execute:
            runner.main()
        argv = execute.call_args.args[1]
        self.assertEqual(json.loads(argv[argv.index('--instance-binding-json')+1]), self.binding)
        changed = {**self.binding, 'device_id': '22222222-2222-4222-8222-222222222222'}
        self.args.instance_binding = changed
        self.assertNotEqual(self.prepare()['directory'], record['directory'])

    def test_changed_frozen_binding_is_rejected_before_launch(self):
        record = self.prepare()
        record['instance_binding'] = {**self.binding, 'device_id': '22222222-2222-4222-8222-222222222222'}
        with mock.patch.object(launcher.subprocess, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'binding changed'):
                launcher.start_once(record, require_identity=False)
        run.assert_not_called()

    def test_bound_dispatcher_does_not_cleanup_other_jobs_and_never_replays(self):
        record = self.prepare()
        idle = {'loaded': False, 'running': False, 'service': 'unused'}
        with mock.patch.object(launcher, 'service_state', return_value=idle), \
             mock.patch.object(launcher, 'cleanup_legacy_jobs') as cleanup, \
             mock.patch.object(launcher.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run:
            self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'started')
            self.assertEqual(launcher.start_once(record, require_identity=False)['state'], 'already_requested')
        cleanup.assert_not_called()
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args.args[0][-1], 'gui/%d/%s' % (os.getuid(), record['dispatcher_label']))
        plist = plistlib.loads(launcher.stable_paths(self.base)['plist'].read_bytes())
        self.assertFalse(plist['KeepAlive']); self.assertFalse(plist['RunAtLoad'])
        self.assertEqual(plist['Label'], record['dispatcher_label'])

    def execute_upgrade(self, *, fail_readiness=False, change_definition=False):
        with contextlib.closing(sqlite3.connect(self.state/'state.sqlite')) as db, db:
            db.execute('CREATE TABLE kv(kind TEXT,key TEXT,value TEXT)')
        signing = {'state': 'ready', 'certificate_name': 'existing fixture', 'certificate_sha1': 'a'*40,
            'code_identifier': 'fun.hackerlife.octendra.agent', 'designated_requirement': 'fixture requirement'}
        def sign(source, target, base):
            self.assertEqual(base, self.binding.get('signing_certificate_sha1', self.signer))
            target.write_bytes(source.read_bytes())
            return {'installed_sha256': launcher.sha(target)}
        def version(argv, **kwargs):
            return 'octendra-code 0.12.0' if argv[0] == str(self.binary) else 'octendra-code 0.12.1'
        def report(record):
            if change_definition and record.get('phase') == 'installing':
                self.definition.write_bytes(b'changed after maintenance wait')
        status_name = 'existing_status' if 'signing_certificate_sha1' in self.binding else 'status'
        sign_name = 'sign_existing_copy' if 'signing_certificate_sha1' in self.binding else 'sign_copy'
        with mock.patch.object(updater.macos_code_identity, status_name, return_value=signing) as status, \
             mock.patch.object(updater.macos_code_identity, 'installed_metadata', return_value=None), \
             mock.patch.object(updater.macos_code_identity, sign_name, side_effect=sign), \
             mock.patch.object(updater.macos_code_identity, 'save_installed_metadata'), \
             mock.patch.object(updater, 'gateway_observation', return_value={'last_seen': 1}), \
             mock.patch.object(updater, 'wait_ready', side_effect=[RuntimeError('fixture outage'), {}]
                if fail_readiness else [{}]) as ready, \
             mock.patch.object(updater, 'MaintenanceLease') as lease, \
             mock.patch.object(updater.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run, \
             mock.patch.object(updater.subprocess, 'check_output', side_effect=version), \
             contextlib.redirect_stdout(io.StringIO()):
            lease.return_value.report.side_effect = report
            lease.return_value.close.return_value = {'released': True}
            if fail_readiness or change_definition:
                with self.assertRaises(SystemExit):
                    updater.perform_upgrade(self.args, self.base)
            else:
                updater.perform_upgrade(self.args, self.base)
        status.assert_called_once_with(self.binding.get('signing_certificate_sha1', self.signer))
        return run, ready, json.loads(self.args.result.read_text())

    def test_bound_upgrade_preserves_config_and_uses_selected_service_readiness(self):
        original = self.config.read_bytes()
        runs, ready, result = self.execute_upgrade()
        self.assertEqual(result['state'], 'upgraded')
        self.assertEqual(self.config.read_bytes(), original)
        self.assertEqual(self.binary.read_bytes(), self.candidate.read_bytes())
        self.assertEqual(ready.call_args.kwargs['binding'], self.binding)
        kicks = [c.args[0] for c in runs.call_args_list if c.args[0][:2] == ['launchctl', 'kickstart']]
        self.assertEqual(kicks, [['launchctl', 'kickstart', '-k', 'gui/%d/%s' % (os.getuid(), self.label)]])

    def test_existing_system_certificate_needs_no_managed_keychain_directory(self):
        self.binding.pop('signing_base')
        self.binding['signing_certificate_sha1'] = 'a'*40
        self.signer.rmdir()
        record = self.prepare()
        self.assertEqual(record['instance_binding'], self.binding)
        with mock.patch.object(updater.macos_code_identity, 'create') as create, \
             mock.patch.object(updater.macos_code_identity, 'authorize') as authorize:
            _, _, result = self.execute_upgrade()
        self.assertEqual(result['state'], 'upgraded')
        create.assert_not_called(); authorize.assert_not_called()

    def test_wrong_device_identity_fails_before_gateway_or_service_change(self):
        self.config.write_text(json.dumps({'device_id': 'another-device', 'state_dir': str(self.state)}))
        signing = {'state': 'ready', 'certificate_name': 'fixture', 'certificate_sha1': 'a'*40,
            'code_identifier': 'fun.hackerlife.octendra.agent', 'designated_requirement': 'fixture'}
        def sign(source, destination, base):
            destination.write_bytes(source.read_bytes())
            return {'installed_sha256': launcher.sha(destination)}
        with mock.patch.object(updater.macos_code_identity, 'status', return_value=signing) as identity, \
             mock.patch.object(updater.macos_code_identity, 'installed_metadata', return_value=None), \
             mock.patch.object(updater.macos_code_identity, 'sign_copy', side_effect=sign), \
             mock.patch.object(updater.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run, \
             mock.patch.object(updater.subprocess, 'check_output', return_value='octendra-code 0.12.1'), \
             mock.patch.object(updater, 'gateway_observation') as gateway, contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit):
                updater.perform_upgrade(self.args, self.base)
        gateway.assert_not_called()
        identity.assert_not_called()
        self.assertFalse(any(c.args[0][0] == 'launchctl' for c in run.call_args_list))
        self.assertEqual(self.binary.read_bytes(), b'old signed fixture')

    def test_dispatcher_identity_is_stable_across_versions_and_distinct_between_instances(self):
        first = self.prepare()
        self.args.version = '0.12.2'
        second = self.prepare()
        self.assertEqual(first['dispatcher_label'], second['dispatcher_label'])
        self.assertNotEqual(first['directory'], second['directory'])
        self.assertNotEqual(first['dispatcher_label'], support.instance_dispatcher_label(
            {**self.binding, 'base': str(self.root/'other-existing-installation')}))

    def test_bound_failure_rolls_back_same_instance_without_rewriting_config(self):
        original = self.config.read_bytes()
        runs, ready, result = self.execute_upgrade(fail_readiness=True)
        self.assertEqual(result['rollback'], 'restored_previous_binary_and_verified_gateway')
        self.assertEqual(self.binary.read_bytes(), b'old signed fixture')
        self.assertEqual(self.config.read_bytes(), original)
        self.assertEqual(ready.call_count, 2)
        self.assertTrue(all(c.kwargs['binding'] == self.binding for c in ready.call_args_list))

    def test_changed_service_after_wait_does_not_install_or_restart(self):
        runs, ready, result = self.execute_upgrade(change_definition=True)
        self.assertEqual(result['state'], 'failed')
        self.assertFalse(result['service_changed'])
        self.assertEqual(self.binary.read_bytes(), b'old signed fixture')
        ready.assert_not_called()
        self.assertFalse(any(c.args[0][:2] == ['launchctl', 'kickstart'] for c in runs.call_args_list))

    def test_readiness_uses_explicit_configuration_and_service_label(self):
        with contextlib.closing(sqlite3.connect(self.state/'state.sqlite')) as db, db:
            db.execute('CREATE TABLE kv(kind TEXT,key TEXT,value TEXT)')
        running = subprocess.CompletedProcess([], 0, stdout='\tstate = running\n\tpid = 123\n')
        with mock.patch.object(support.subprocess, 'run', return_value=running) as run:
            self.assertTrue(support.local_observation(self.base, self.binding)['running'])
        self.assertEqual(run.call_args.args[0], ['launchctl', 'print', 'gui/%d/%s' % (os.getuid(), self.label)])


if __name__ == '__main__':
    unittest.main()
