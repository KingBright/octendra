"""Opt-in macOS probe. Uses disposable counter jobs, never the real updater."""
import importlib.util
import os
import pathlib
import plistlib
import re
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

SCRIPTS = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
SPEC = importlib.util.spec_from_file_location('oneshot_probe_launcher', SCRIPTS/'launch-code-upgrade.py')
launcher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(launcher)


@unittest.skipUnless(sys.platform == 'darwin' and os.environ.get('OCTENDRA_LAUNCHD_PROBE') == '1',
                     'requires explicit disposable macOS launchd probe opt-in')
class LiveOneShotTest(unittest.TestCase):
    def counter_probe(self, exit_code):
        label = 'com.octendra.test.oneshot.'+uuid.uuid4().hex
        service = 'gui/%d/%s' % (os.getuid(), label)
        with tempfile.TemporaryDirectory(prefix='octendra-oneshot-fixture-') as tmp:
            directory = pathlib.Path(tmp)
            script = directory/'counter.py'
            script.write_text('import pathlib,sys\np=pathlib.Path(__file__).with_name("runs.txt")\nwith p.open("a") as f: f.write("run\\n")\nsys.exit('+str(exit_code)+')\n')
            paths = {'directory': directory, 'runner': script, 'request': directory/'unused-request.json'}
            spec = launcher.stable_plist(pathlib.Path(sys.executable).resolve(), paths)
            spec['Label'] = label
            job = directory/'job.plist'
            job.write_bytes(plistlib.dumps(spec)); job.chmod(0o600)
            result = subprocess.run(['launchctl', 'bootstrap', 'gui/%d' % os.getuid(), str(job)],
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, 'disposable probe bootstrap failed')
            try:
                marker = directory/'runs.txt'
                time.sleep(0.2)
                self.assertFalse(marker.exists(), 'dispatcher must wait for an explicit kickstart')
                started = subprocess.run(['launchctl', 'kickstart', service], capture_output=True, text=True, timeout=10)
                self.assertEqual(started.returncode, 0)
                deadline = time.monotonic()+15
                while not marker.exists() and time.monotonic() < deadline:
                    time.sleep(0.1)
                self.assertEqual(marker.read_text(), 'run\n')
                time.sleep(12)
                observed = subprocess.run(['launchctl', 'print', service], capture_output=True, text=True, timeout=5)
                self.assertEqual(observed.returncode, 0)
                fields = dict(re.findall(r'^\t(runs|last exit code) = ([^\n]+)$', observed.stdout, re.MULTILINE))
                self.assertEqual(marker.read_text(), 'run\n', 'one-shot job unexpectedly ran again')
                self.assertEqual(fields.get('runs'), '1')
                self.assertEqual(fields.get('last exit code'), str(exit_code))
                print('disposable launchd probe: runs=1, exit='+str(exit_code)+', no re-entry after 12 seconds', flush=True)
            finally:
                removed = subprocess.run(['launchctl', 'bootout', service], capture_output=True, timeout=10)
                self.assertEqual(removed.returncode, 0, 'disposable probe cleanup failed: '+label)

    def test_successful_exit_is_not_restarted(self):
        self.counter_probe(0)

    def test_failed_exit_is_not_restarted(self):
        self.counter_probe(7)


if __name__ == '__main__':
    unittest.main()
