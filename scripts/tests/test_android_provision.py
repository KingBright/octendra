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
        self.assertEqual(write.kwargs['input'], payload)
        self.assertNotIn('private-fixture', str(write.args))


if __name__ == '__main__':
    unittest.main()
