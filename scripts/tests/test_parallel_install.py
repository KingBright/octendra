"""Fresh-install boundaries using temporary directories and mocked OS managers."""
import copy
import hashlib
import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path, PureWindowsPath
import plistlib
import socket
import shlex
import subprocess
import tempfile
import unittest
from unittest import mock
import xml.etree.ElementTree as ET

SCRIPT = Path(__file__).resolve().parents[1]/'parallel-install.py'
spec = importlib.util.spec_from_file_location('parallel_install_fixture', SCRIPT)
install = importlib.util.module_from_spec(spec); spec.loader.exec_module(install)


class ParallelInstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name).resolve()
        self.root = self.base/'octendra/agent'; (self.root/'config').mkdir(parents=True)
        self.state = self.root/'state'; self.config = self.root/'config/agent.json'
        self.cfg = dict(gateway_url='https://new.example.invalid',device_id='test-device',
            device_token='SYNTHETIC-TEST-ONLY',state_dir=str(self.state),roots=[str(self.base/'work')],
            allow_write=False,allow_exec=False,shell='/bin/sh')
        self.save_config()
        self.package = self.base/'package'; self.package.mkdir()
        self.binary = b'fixture binary only'
        artifacts = {}
        for name in ('linux-amd64','macos-arm64','windows-amd64.exe'):
            p=self.package/('octendra-code-'+name);p.write_bytes(self.binary)
            artifacts[p.name]={'sha256':install.digest(p),'size':len(self.binary)}
        (self.package/'manifest.json').write_text(json.dumps({'artifacts':artifacts}))
        self.legacy = self.base/'remote-hosts-code';self.legacy.mkdir()
        for name in ('binary','config','database','service','proxy'):
            (self.legacy/name).write_bytes(('original-'+name).encode())
        self.old = self.snapshot(self.legacy)
        self.args = install.parser().parse_args(['--role','agent','--package',str(self.package),
            '--manifest-sha256',install.digest(self.package/'manifest.json'),
            '--root',str(self.root),'--config',str(self.config),'--state-dir',str(self.state),
            '--public-origin','https://new.example.invalid','--legacy-origin','https://old.example.invalid',
            '--new-auth-provisioned'])
        self.host = mock.patch.object(install,'host_platform',return_value='linux');self.host.start();self.addCleanup(self.host.stop)
        self.probe = mock.patch.object(install,'probe_service');self.probe.start();self.addCleanup(self.probe.stop)
        self.check = mock.patch.object(install,'checked',return_value='');self.checked=self.check.start();self.addCleanup(self.check.stop)
        self.service = self.base/'units/octendra-agent.service';self.service.parent.mkdir()
        self.target = mock.patch.object(install,'service_target',return_value=self.service);self.target.start();self.addCleanup(self.target.stop)

    def save_config(self):
        self.config.write_text(json.dumps(self.cfg));self.config.chmod(0o600)

    def snapshot(self, root):
        return {str(p.relative_to(root)):(p.read_bytes() if p.is_file() else None,p.stat().st_mode,p.stat().st_mtime_ns)
                for p in [root,*root.rglob('*')]}

    def tearDown(self):
        self.assertEqual(self.old,self.snapshot(self.legacy),'legacy bytes, modes and mtimes must not change')

    def test_default_plan_has_no_writes_or_service_activation(self):
        before=self.snapshot(self.root);p=install.plan(self.args)
        self.assertEqual(before,self.snapshot(self.root));self.assertFalse(self.state.exists())
        self.assertEqual(p['label'],'octendra-agent.service')
        self.assertNotIn('SYNTHETIC-TEST-ONLY',json.dumps(install.public_plan(p)))
        self.assertEqual(self.checked.call_args.args[0][1:3],['check','--agent'])

    def test_missing_origin_stops_before_configuration_read(self):
        self.args.public_origin=None
        with mock.patch.object(Path,'read_bytes',side_effect=AssertionError('must not read auth')):
            with self.assertRaisesRegex(install.Refusal,'missing new'):install.plan(self.args)

    def test_real_cli_missing_domain_stops_without_auth_lookup_or_writes(self):
        output=io.StringIO()
        with mock.patch.object(install.sys,'argv',['parallel-install.py','--role','gateway']),contextlib.redirect_stderr(output):
            code=install.main()
        self.assertEqual(code,2);self.assertIn('missing new --public-origin',output.getvalue())
        self.assertFalse((self.root/'bin').exists())

    def test_unknown_secret_argument_is_not_echoed(self):
        output=io.StringIO()
        with mock.patch.object(install.sys,'argv',['parallel-install.py','--role','agent','--device-token','SYNTHETIC-DO-NOT-ECHO']),contextlib.redirect_stderr(output):
            code=install.main()
        self.assertEqual(code,2);self.assertNotIn('SYNTHETIC-DO-NOT-ECHO',output.getvalue())

    def test_missing_new_auth_stops_without_writes(self):
        self.args.new_auth_provisioned=False
        with self.assertRaisesRegex(install.Refusal,'no credentials'):install.plan(self.args)
        self.assertFalse((self.root/'bin').exists())

    def test_legacy_origin_is_not_a_fallback(self):
        self.args.public_origin=self.args.legacy_origin
        with self.assertRaisesRegex(install.Refusal,'different explicit'):install.plan(self.args)

    def test_exact_https_origin_only(self):
        for value in ('http://new.invalid','https://user:pass@new.invalid','https://new.invalid/path',
                      'https://new.invalid:8443','https://*.invalid','${NEW_DOMAIN}',
                      'https://127.0.0.1','https://local','https://not_a_domain.invalid'):
            with self.subTest(value=value):
                with self.assertRaises(install.Refusal):install.origin(value)

    def test_unfilled_auth_template_cannot_pass_by_setting_attestation(self):
        self.cfg['device_token']='${NEW_DEVICE_TOKEN}';self.save_config()
        with self.assertRaisesRegex(install.Refusal,'unfilled placeholders'):install.plan(self.args)
        self.assertFalse((self.root/'bin').exists())

    def test_old_install_component_rejected(self):
        for name in install.BLOCKED_COMPONENTS:
            self.args.root=str(self.base/name/'octendra')
            with self.assertRaisesRegex(install.Refusal,'legacy install'):install.plan(self.args)
        for name in (value.replace('-',' ').title() for value in install.BLOCKED_COMPONENTS):
            self.args.root=str(self.base/name/'octendra')
            with self.assertRaisesRegex(install.Refusal,'legacy install'):install.plan(self.args)

    def test_symlink_path_rejected(self):
        link=self.base/'link';link.symlink_to(self.root,target_is_directory=True)
        self.args.root=str(link)
        with self.assertRaisesRegex(install.Refusal,'symlink'):install.plan(self.args)

    def test_existing_state_and_database_rejected(self):
        self.state.mkdir();(self.state/'database').write_bytes(b'existing')
        with self.assertRaisesRegex(install.Refusal,'existing database'):install.plan(self.args)
        self.assertEqual((self.state/'database').read_bytes(),b'existing')

    def test_config_state_and_origin_must_match_explicit_parameters(self):
        for key,value in (('state_dir',str(self.legacy)),('gateway_url',self.args.legacy_origin)):
            saved=self.cfg[key];self.cfg[key]=value;self.save_config()
            with self.assertRaises(install.Refusal):install.plan(self.args)
            self.cfg[key]=saved;self.save_config()

    def test_private_config_mode_required(self):
        self.config.chmod(0o644)
        with self.assertRaisesRegex(install.Refusal,'0600'):install.plan(self.args)

    def test_binary_and_manifest_pins_required(self):
        self.args.manifest_sha256='0'*64
        with self.assertRaisesRegex(install.Refusal,'trust anchor'):install.plan(self.args)
        self.args.manifest_sha256=install.digest(self.package/'manifest.json')
        (self.package/'octendra-code-linux-amd64').write_bytes(b'changed')
        with self.assertRaisesRegex(install.Refusal,'checksum'):install.plan(self.args)

    def test_occupied_installation_is_never_replaced(self):
        (self.root/'bin').mkdir();existing=self.root/'bin/octendra-code';existing.write_bytes(b'existing')
        with self.assertRaisesRegex(install.Refusal,'occupied'):install.plan(self.args)
        self.assertEqual(existing.read_bytes(),b'existing')

    def test_other_data_in_new_root_is_not_adopted(self):
        data=self.root/'existing-database.sqlite';data.write_bytes(b'existing fixture')
        before=self.snapshot(self.root)
        with self.assertRaisesRegex(install.Refusal,'occupied'):install.plan(self.args)
        self.assertEqual(before,self.snapshot(self.root))

    def test_stage_only_copies_new_binary_and_definition(self):
        original=self.config.read_bytes();p=install.plan(self.args);install.stage(p)
        self.assertEqual(p['binary'].read_bytes(),self.binary)
        self.assertEqual(self.config.read_bytes(),original);self.assertFalse(self.state.exists())
        self.assertFalse(self.service.exists())
        self.assertNotIn('SYNTHETIC-TEST-ONLY',p['marker'].read_text())
        with self.assertRaisesRegex(install.Refusal,'occupied'):install.plan(self.args)

    def test_changed_config_stops_before_stage(self):
        p=install.plan(self.args);self.cfg['allow_exec']=True;self.save_config()
        with self.assertRaisesRegex(install.Refusal,'changed after'):install.stage(p)
        self.assertFalse((self.root/'bin').exists())

    def test_unknown_activation_never_replays(self):
        p=install.plan(self.args);install.stage(p);self.args.action='activate'
        (self.root/'activation-request.json').write_text('{}')
        with self.assertRaisesRegex(install.Refusal,'never replayed'):install.plan(self.args)

    def test_service_file_race_cannot_overwrite_existing_file(self):
        p=install.plan(self.args);install.stage(p);self.args.action='activate';p=install.plan(self.args)
        self.service.write_bytes(b'foreign service')
        with self.assertRaises(FileExistsError):install.activate(p)
        self.assertEqual(self.service.read_bytes(),b'foreign service')
        self.assertTrue((self.root/'activation-request.json').exists())
        self.assertFalse(any('enable' in call.args[0] for call in self.checked.call_args_list))

    def test_linux_activation_only_starts_new_label(self):
        p=install.plan(self.args);install.stage(p);self.args.action='activate';p=install.plan(self.args)
        install.activate(p)
        self.assertIn(mock.call(['systemctl','--user','enable','octendra-agent.service']),self.checked.call_args_list)
        self.assertIn(mock.call(['systemctl','--user','start','octendra-agent.service']),self.checked.call_args_list)
        self.assertFalse(any('--now' in call.args[0] for call in self.checked.call_args_list))
        self.assertTrue(self.service.exists());self.assertTrue(self.state.exists())

    def test_gateway_activation_never_requests_or_assumes_elevation(self):
        p=install.plan(self.args);p['role']='gateway'
        with mock.patch.object(install.os,'geteuid',return_value=501):
            with self.assertRaisesRegex(install.Refusal,'no elevation'):install.activate(p)
        self.assertFalse((self.root/'activation-request.json').exists());self.assertFalse(self.state.exists())

    def test_socket_conflict_has_no_fallback_or_public_listener(self):
        with socket.socket() as listener:
            listener.bind(('127.0.0.1',0));port=listener.getsockname()[1]
            with self.assertRaisesRegex(install.Refusal,'occupied'):install.port_available(port)
        install.port_available(port)
        with self.assertRaises(install.Refusal):install.port_available(18787)

    def test_gateway_candidate_port_and_new_service_identity(self):
        import pwd
        import grp
        self.config.unlink();self.args.role='gateway';self.config=self.root/'config/gateway.json';self.args.config=str(self.config)
        self.args.service_user=pwd.getpwuid(os.getuid()).pw_name
        self.args.service_group=grp.getgrgid(os.getgid()).gr_name
        self.cfg={'public_url':self.args.public_origin,'bind':'127.0.0.1:18788','state_dir':str(self.state),
                  'owner':'fixture-owner','password_hash':'$argon2id$fixture-only','devices':[],
                  'redirect_uris':[],'allowed_origins':['https://gemini.google.com'],'forge_browser':None}
        self.save_config()
        with mock.patch.object(install,'port_available') as probe:
            p=install.plan(self.args)
        probe.assert_called_once_with(18788)
        self.assertEqual(p['label'],'octendra-gateway.service')
        line=next(x for x in p['definition'].decode().splitlines() if x.startswith('ExecStart='))
        self.assertEqual(shlex.split(line.split('=',1)[1])[1:3],['gateway','--config'])
        self.assertFalse(self.state.exists());self.assertFalse(self.service.exists())

    def test_numeric_runtime_uses_fixed_isolated_privilege_drop_without_creating_accounts(self):
        self.config.unlink();self.args.role='gateway';self.config=self.root/'config/gateway.json';self.args.config=str(self.config)
        self.args.service_user=str(os.getuid());self.args.service_group=str(os.getgid())
        self.cfg={'public_url':self.args.public_origin,'bind':'127.0.0.1:18788','state_dir':str(self.state),
                  'owner':'fixture','password_hash':'$argon2id$fixture-only','devices':[],'redirect_uris':[]}
        self.save_config()
        with mock.patch.object(install,'port_available'),mock.patch.object(install,'trusted_python',return_value=Path('/usr/bin/python3')):
            p=install.plan(self.args)
        line=next(x for x in p['definition'].decode().splitlines() if x.startswith('ExecStart='))
        argv=shlex.split(line.split('=',1)[1])
        self.assertEqual(argv[:4],['/usr/bin/python3','-I','-S','-c'])
        self.assertEqual(argv[4],install.DROP_CODE)
        self.assertEqual(argv[5:7],[self.args.service_user,self.args.service_group])
        self.assertNotIn('User=',p['definition'].decode())

    def test_numeric_launcher_drops_all_privileges_before_exec_and_fails_closed(self):
        from types import SimpleNamespace
        for failing in (None,'groups','gid','uid'):
            calls=[]
            def record(name,*values):
                calls.append((name,values))
                if name==failing:raise OSError('fixture privilege drop denied')
            fake_os=SimpleNamespace(setgroups=lambda v:record('groups',v),setgid=lambda v:record('gid',v),
                setuid=lambda v:record('uid',v),execv=lambda *v:record('exec',*v))
            fake_sys=SimpleNamespace(argv=['-c','18787','18787','/new octendra/bin/code','gateway','--config','/new octendra/config/gateway.json'])
            def load(name,*args):return fake_os if name=='os' else fake_sys
            scope={'__builtins__':{'__import__':load,'int':int}}
            if failing:
                with self.assertRaises(OSError):exec(install.DROP_CODE,scope)
                self.assertNotIn('exec',[x[0] for x in calls])
            else:
                exec(install.DROP_CODE,scope)
                self.assertEqual([x[0] for x in calls],['groups','gid','uid','exec'])
                self.assertEqual(calls[-1][1],(fake_sys.argv[3],fake_sys.argv[3:]))

    def test_no_existing_upgrade_distribution_is_inherited(self):
        (self.root/'config/releases').mkdir()
        with self.assertRaisesRegex(install.Refusal,'upgrade release pointers'):install.plan(self.args)

    def test_gateway_cannot_bind_publicly_or_reuse_legacy_devices(self):
        self.config.unlink();self.args.role='gateway';self.config=self.root/'config/gateway.json';self.args.config=str(self.config)
        for bind,devices in (('0.0.0.0:18788',[]),('127.0.0.1:18788',[{'id':'legacy'}])):
            self.cfg={'public_url':self.args.public_origin,'bind':bind,'state_dir':str(self.state),
                      'owner':'fixture','password_hash':'$argon2id$fixture-only','devices':devices,'redirect_uris':[]}
            self.save_config()
            with self.assertRaises(install.Refusal):install.plan(self.args)

    def test_unknown_or_existing_linux_service_refuses(self):
        self.probe.stop()
        for rc,out,err in ((0,'LoadState=loaded',''),(1,'','unknown-status')):
            with mock.patch.object(subprocess,'run',return_value=subprocess.CompletedProcess([],rc,out,err)):
                with self.assertRaises(install.Refusal):install.probe_service('linux','agent','octendra-agent.service',self.service)
        with mock.patch.object(subprocess,'run',return_value=subprocess.CompletedProcess([],0,'LoadState=not-found\n','')) as query:
            install.probe_service('linux','agent','octendra-agent.service',self.service)
        self.assertNotIn('--value',query.call_args.args[0])

    def test_unit_escaping_preserves_spaces_quotes_and_percent(self):
        value='/new octendra/quote"/%service/$scope/config.json'
        quoted=install.unit_quote(value)
        self.assertIn('%%service',quoted);self.assertIn('\\"',quoted);self.assertTrue(quoted.startswith('"'))
        self.assertIn('$$scope',quoted)

    def test_windows_xml_is_interactive_limited_and_quotes_paths(self):
        binary=PureWindowsPath('C:/Users/Test User/octendra/agent/bin/octendra-code.exe')
        config=PureWindowsPath('C:/Users/Test User/octendra/agent/config/agent.json')
        xml=install.windows_task(binary,config,'S-1-5-21-test')
        root=ET.fromstring(xml);ns={'t':'http://schemas.microsoft.com/windows/2004/02/mit/task'}
        self.assertEqual(root.find('.//t:RunLevel',ns).text,'LeastPrivilege')
        self.assertEqual(root.find('.//t:LogonType',ns).text,'InteractiveToken')
        self.assertEqual(root.find('.//t:Command',ns).text,str(binary))
        self.assertEqual(root.find('.//t:Arguments',ns).text,subprocess.list2cmdline(['agent','--config',str(config)]))

    def test_windows_registration_is_create_only_without_policy_or_acl_override(self):
        with mock.patch.object(install,'host_platform',return_value='windows'), mock.patch.object(install,'task_sid',return_value='S-1-5-21-test'):
            p=install.plan(self.args);install.stage(p);self.args.action='activate';p=install.plan(self.args);install.activate(p)
        command=self.checked.call_args.args[0]
        self.assertIn("RegisterTask('Octendra Code Agent',$x,2,$u,$null,3,$null)",command[-1])
        for forbidden in ('ExecutionPolicy','RunAs','Set-Acl','Stop-ScheduledTask','DeleteTask'):
            self.assertNotIn(forbidden,' '.join(command))

    def test_mac_refuses_unsigned_and_uses_existing_new_identity(self):
        with mock.patch.object(install,'host_platform',return_value='macos'):
            with self.assertRaisesRegex(install.Refusal,'signing owner'):install.plan(self.args)
            signed=self.base/'signed';signed.write_bytes(b'signed fixture')
            receipt=self.base/'signing.json';receipt.write_text(json.dumps({'candidate_sha256':hashlib.sha256(self.binary).hexdigest(),
                'installed_sha256':install.digest(signed),'certificate_sha1':'a'*40,'code_identifier':install.MAC_ID}))
            self.args.signed_binary=str(signed);self.args.signing_receipt=str(receipt)
            p=install.plan(self.args)
        self.assertEqual(plistlib.loads(p['definition'])['Label'],install.MAC_ID)
        self.assertTrue(any('-R' in call.args[0] and install.MAC_ID in ' '.join(call.args[0]) for call in self.checked.call_args_list))

    def test_brand_guard_exempts_only_exact_rejected_namespace_line(self):
        s=importlib.util.spec_from_file_location('brand_guard_fixture',SCRIPT.with_name('check-branding.py'))
        guard=importlib.util.module_from_spec(s);s.loader.exec_module(guard)
        approved=next(line for line in SCRIPT.read_text().splitlines() if line.startswith('BLOCKED_COMPONENTS ='))
        name='scripts/parallel-install.py';target=self.base/name;target.parent.mkdir(exist_ok=True)
        with mock.patch.object(guard,'inventory',return_value=[name]):
            target.write_text(approved+'\n');self.assertTrue(guard.inspect(self.base)['passed'])
            target.write_text(approved+' # modified\n');self.assertFalse(guard.inspect(self.base)['passed'])
            old_name=next(x for x in install.BLOCKED_COMPONENTS if x!='octendra-code')
            target.write_text('title = '+repr(old_name)+'\n');self.assertFalse(guard.inspect(self.base)['passed'])

    def test_new_proxy_template_preserves_unfilled_scope_and_denies_unsafe_upgrades(self):
        text=(SCRIPT.parents[1]/'docs/parallel-octendra/new-host.caddy.in').read_text()
        self.assertIn('${OCTENDRA_NEW_HOST}',text)
        self.assertIn('${OCTENDRA_REVIEWED_TLS_CONFIGURATION}',text)
        self.assertIn('reverse_proxy 127.0.0.1:18788',text)
        self.assertNotIn('127.0.0.1:18787',text)
        for path in ('/admin/gateway-upgrade','/admin/release-bundle','/device/upgrade-directive','/device/release-artifact/*'):
            self.assertIn(path,text)


if __name__=='__main__':unittest.main()
