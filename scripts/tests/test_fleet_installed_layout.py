import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

SCRIPTS=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(SCRIPTS))
spec=importlib.util.spec_from_file_location('fleet_installed_layout',SCRIPTS/'fleet-upgrade.py')
fleet=importlib.util.module_from_spec(spec);spec.loader.exec_module(fleet)


class InstalledLayoutTests(unittest.TestCase):
    def fixture(self,root,platform):
        root=root.resolve()
        base=root/'.local/share/octendra/agent'
        for name in ('bin','config','signing'): (base/name).mkdir(parents=True)
        (base/'bin/octendra-code').write_text('installed runtime')
        (base/'config/agent.json').write_text(json.dumps({'device_id':'selected-device'}))
        (base/'signing/installed.json').write_text(json.dumps({'certificate_sha1':'a'*40}))
        definition=root/'Library/LaunchAgents/fun.hackerlife.octendra.agent.plist'
        definition.parent.mkdir(parents=True);definition.write_text('installed service')
        package=root/'package';package.mkdir()
        device={'device_id':'selected-device','capabilities':{'platform':platform,'arch':'aarch64','home_dir':str(root)}}
        artifact='octendra-code-'+platform+'-arm64'
        return base,package,device,{'artifacts':{artifact:{'sha256':'b'*64}}}

    def test_macos_uses_current_device_and_original_signer_and_retains_request_binding(self):
        with tempfile.TemporaryDirectory() as directory:
            base,package,device,manifest=self.fixture(Path(directory),'macos')
            (package/'launch-code-upgrade.py').write_text('import json,sys\nprint(json.dumps(sys.argv[1:]))\n')
            command=shlex.split(fleet.launch_command(device,str(package),manifest,'0.12.5'))
            result=subprocess.run(command,capture_output=True,text=True,check=True)
            self.assertIn('--instance-binding',json.loads(result.stdout))
            binding=(package/'instance-binding.json').read_bytes()
            saved=json.loads(binding)
            self.assertEqual(saved['config'],str(base/'config/agent.json'))
            self.assertEqual(saved['signing_certificate_sha1'],'a'*40)
            (base/'bin/octendra-code').write_text('already replaced runtime')
            subprocess.run(command,capture_output=True,text=True,check=True)
            self.assertEqual((package/'instance-binding.json').read_bytes(),binding)
            (base/'config/agent.json').write_text('{"device_id":"another-device"}')
            self.assertNotEqual(subprocess.run(command,capture_output=True).returncode,0)

    def test_linux_passes_canonical_configuration_and_service_to_existing_updater(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);base,package,device,manifest=self.fixture(root,'linux')
            fake=root/'bin';fake.mkdir();runner=fake/'systemd-run'
            runner.write_text('#!'+sys.executable+'\nimport json,sys\nprint(json.dumps(sys.argv[1:]))\n');runner.chmod(0o700)
            result=subprocess.run(shlex.split(fleet.launch_command(device,str(package),manifest,'0.12.5')),
                                  env=dict(os.environ,PATH=str(fake)+os.pathsep+os.environ['PATH']),capture_output=True,text=True,check=True)
            args=json.loads(result.stdout)
            self.assertEqual(args[args.index('--config-path')+1],str(base/'config/agent.json'))
            self.assertEqual(args[args.index('--binary-path')+1],str(base/'bin/octendra-code'))
            self.assertEqual(args[args.index('--service-name')+1],'octendra-agent.service')


if __name__=='__main__':unittest.main()
