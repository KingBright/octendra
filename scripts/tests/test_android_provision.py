import hashlib
import importlib.util
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
spec = importlib.util.spec_from_file_location('android_provision', Path(__file__).resolve().parents[1] / 'provision-android.py')
provision = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provision)


class ProvisionTests(unittest.TestCase):
    def test_uncertain_inflight_write_is_observed_without_resubmission(self):
        payload = b'{"device_token":"private-fixture"}'
        sha = hashlib.sha256(payload).hexdigest()
        observations = ['Row: 0 state=applying, request_sha256=' + sha + ', error_code=, start=, running=0',
                        'Row: 0 state=configured, request_sha256=' + sha + ', error_code=, start=requested, running=1']
        with patch.object(provision.subprocess, 'run', side_effect=[subprocess.CompletedProcess([], 0, row, '') for row in observations]) as run:
            self.assertTrue(provision.provision(['adb', '-s', 'approved-device'], payload)['running'])
        self.assertTrue(all('write' not in call.args[0] for call in run.call_args_list))

    def test_private_payload_only_goes_to_stdin_and_status_keeps_foreground_failure_honest(self):
        payload = b'{"device_token":"private-fixture"}'
        sha = hashlib.sha256(payload).hexdigest()
        results = [subprocess.CompletedProcess([], 0, 'Row: 0 state=not_started, request_sha256=, running=0', ''),
                   subprocess.CompletedProcess([], 0, b'', b''),
                   subprocess.CompletedProcess([], 0, 'Row: 0 state=configured, request_sha256=' + sha + ', start=platform_foreground_required, running=0', '')]
        with patch.object(provision.subprocess, 'run', side_effect=results) as run:
            result = provision.provision(['adb', '-s', 'approved-device'], payload)
        self.assertEqual(result['state'], 'configured')
        self.assertFalse(result['running'])
        write = run.call_args_list[1]
        self.assertEqual(write.args[0][3:7], ['shell', '-T', 'content', 'write'])
        self.assertEqual(write.kwargs['input'], payload)
        self.assertNotIn('private-fixture', str(write.args))

    def test_pipe_failure_before_hash_is_reported_without_waiting_for_timeout(self):
        results = [subprocess.CompletedProcess([], 0, 'Row: 0 state=not_started, request_sha256=, running=0', ''),
                   subprocess.CompletedProcess([], 0, b'', b''),
                   subprocess.CompletedProcess([], 0, 'Row: 0 state=failed, request_sha256=, error_code=configuration_io_error, running=0', '')]
        with patch.object(provision.subprocess, 'run', side_effect=results), patch.object(provision.time, 'sleep') as sleep:
            with self.assertRaisesRegex(RuntimeError, '^configuration_io_error$'):
                provision.provision(['adb', '-s', 'approved-device'], b'{}')
        sleep.assert_not_called()

    def test_unrelated_failed_request_is_not_mistaken_for_the_inflight_request(self):
        payload = b'{}'
        sha = hashlib.sha256(payload).hexdigest()
        rows = ['Row: 0 state=applying, request_sha256=another-request, running=0',
                'Row: 0 state=failed, request_sha256=, error_code=configuration_io_error, running=0',
                'Row: 0 state=configured, request_sha256=' + sha + ', start=requested, running=1']
        with patch.object(provision.subprocess, 'run', side_effect=[subprocess.CompletedProcess([], 0, row, '') for row in rows]) as run, patch.object(provision.time, 'sleep'):
            self.assertTrue(provision.provision(['adb'], payload)['running'])
        self.assertTrue(all('write' not in call.args[0] for call in run.call_args_list))


if __name__ == '__main__':
    unittest.main()
