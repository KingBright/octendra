import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

SOURCE=Path(__file__).resolve().parents[1]/'android-activate-shell.py'
spec=importlib.util.spec_from_file_location('android_shell_recovery',SOURCE)
helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)


class EndWatch(Exception):pass


class ShellRecoveryTests(unittest.TestCase):
    def args(self,root):
        return SimpleNamespace(adb='adb',serial='approved-device',package='fun.hackerlife.octendra',
            interval=10,state_directory=Path(root),java_home=Path('/jdk'),apksigner=Path('/sdk/apksigner'),certificate_sha256='a'*64)

    def clocks(self,iterations):
        value={'now':1000,'sleeps':0}
        def sleep(seconds):
            value['now']+=seconds;value['sleeps']+=1
            if value['sleeps']>=iterations:raise EndWatch()
        return value,sleep

    def test_disconnected_bridge_is_verified_and_activated_once_then_observed(self):
        with tempfile.TemporaryDirectory() as root:
            now,sleep=self.clocks(2)
            rows=[('socket','10001',False),('socket','10001',False),('socket','10001',True)]
            with mock.patch.object(helper,'bridge_status',side_effect=rows),mock.patch.object(helper,'installed_apk',return_value='/data/app/owner/base.apk'),mock.patch.object(helper,'verify_signer') as verify,mock.patch.object(helper,'activate') as activate,mock.patch.object(helper.time,'monotonic',side_effect=lambda:now['now']),mock.patch.object(helper.time,'sleep',side_effect=sleep):
                with self.assertRaises(EndWatch):helper.watch(self.args(root))
            self.assertEqual(verify.call_count,1);self.assertEqual(activate.call_count,1)
            status=json.loads((Path(root)/'status.json').read_text())
            self.assertEqual(status['state'],'shell_connected');self.assertEqual(status['activations'],1)

    def test_usb_or_local_consent_failure_never_launches_a_helper(self):
        with tempfile.TemporaryDirectory() as root:
            now,sleep=self.clocks(3)
            with mock.patch.object(helper,'bridge_status',side_effect=helper.ActivationError('local_shell_consent_or_running_agent_required')),mock.patch.object(helper,'verify_signer') as verify,mock.patch.object(helper,'activate') as activate,mock.patch.object(helper.time,'sleep',side_effect=sleep):
                with self.assertRaises(EndWatch):helper.watch(self.args(root))
            verify.assert_not_called();activate.assert_not_called()

    def test_changed_bridge_identity_during_verification_is_not_activated(self):
        with tempfile.TemporaryDirectory() as root:
            now,sleep=self.clocks(1)
            with mock.patch.object(helper,'bridge_status',side_effect=[('old','10001',False),('new','10001',False)]),mock.patch.object(helper,'installed_apk',return_value='/data/app/owner/base.apk'),mock.patch.object(helper,'verify_signer'),mock.patch.object(helper,'activate') as activate,mock.patch.object(helper.time,'monotonic',side_effect=lambda:now['now']),mock.patch.object(helper.time,'sleep',side_effect=sleep):
                with self.assertRaises(EndWatch):helper.watch(self.args(root))
            activate.assert_not_called()
            self.assertEqual(json.loads((Path(root)/'status.json').read_text())['state'],'bridge_identity_changed')

    def test_an_unapproved_apk_signature_cannot_receive_shell_authority(self):
        with tempfile.TemporaryDirectory() as root:
            args=self.args(root)
            with mock.patch.object(helper,'run',side_effect=['', 'Signer #1 certificate SHA-256 digest: '+'b'*64]):
                with self.assertRaisesRegex(helper.ActivationError,'signer_mismatch'):
                    helper.verify_signer(['adb'],'/data/app/other/base.apk',args)

    def test_activation_observation_backoff_prevents_duplicate_helpers(self):
        with tempfile.TemporaryDirectory() as root:
            now,sleep=self.clocks(2)
            with mock.patch.object(helper,'bridge_status',return_value=('socket','10001',False)),mock.patch.object(helper,'installed_apk',return_value='/data/app/owner/base.apk'),mock.patch.object(helper,'verify_signer'),mock.patch.object(helper,'activate') as activate,mock.patch.object(helper.time,'monotonic',side_effect=lambda:now['now']),mock.patch.object(helper.time,'sleep',side_effect=sleep):
                with self.assertRaises(EndWatch):helper.watch(self.args(root))
            self.assertEqual(activate.call_count,1)
            self.assertEqual(json.loads((Path(root)/'status.json').read_text())['state'],'waiting_for_activation_observation')


    def test_rejected_signer_is_cached_until_the_installed_apk_changes(self):
        with tempfile.TemporaryDirectory() as root:
            now,sleep=self.clocks(3)
            with mock.patch.object(helper,'bridge_status',return_value=('socket','10001',False)),mock.patch.object(helper,'installed_apk',return_value='/data/app/other/base.apk'),mock.patch.object(helper,'verify_signer',side_effect=helper.ActivationError('installed_apk_signer_mismatch')) as verify,mock.patch.object(helper,'activate') as activate,mock.patch.object(helper.time,'monotonic',side_effect=lambda:now['now']),mock.patch.object(helper.time,'sleep',side_effect=sleep):
                with self.assertRaises(EndWatch):helper.watch(self.args(root))
            self.assertEqual(verify.call_count,1);activate.assert_not_called()


if __name__=='__main__':unittest.main()
