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
import stat
import shlex
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock
from types import SimpleNamespace
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
        (self.package/'manifest.json').write_text(json.dumps({'artifacts':artifacts,'instance_upgrade_protocol':1}))
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
        self.boundary = mock.patch.object(install, 'linux_unit_parent',
                                          return_value=contextlib.nullcontext(None))
        self.boundary.start();self.addCleanup(self.boundary.stop)

    def real_linux_boundary(self, alias=True):
        """Real path derivation/FD writes; synthetic root ownership needs no sudo."""
        import pwd
        self.target.stop();self.boundary.stop()
        system = self.base/'os'; system.mkdir()
        alias_path = system/'home'; canonical = system/'var/home'
        canonical.mkdir(parents=True)
        if alias:
            alias_path.symlink_to('var/home', target_is_directory=True)
            home = canonical/'fixture'
        else:
            alias_path.mkdir();home = alias_path/'fixture'
        home.mkdir(mode=0o700)
        self.service = home/'.config/systemd/user/octendra-agent.service'
        self.service.parent.mkdir(parents=True)
        self.home = home;self.home_alias = alias_path;self.system_home = canonical
        self.account_home = alias_path/'fixture'
        self.metadata_overrides = {}
        original_lstat = Path.lstat; original_fstat = os.fstat
        prefixes = set(home.parents)
        directories = prefixes | {home, home/'.config', home/'.config/systemd', self.service.parent}
        inode_paths = {(original_lstat(p).st_dev, original_lstat(p).st_ino):p
                       for p in directories}

        def metadata(path, info):
            values = {name:getattr(info,name) for name in dir(info) if name.startswith('st_')}
            if path in prefixes:
                values.update(st_uid=0, st_mode=stat.S_IFDIR | 0o755)
            if path == alias_path and alias:
                values.update(st_uid=0)
            values.update(self.metadata_overrides.get(path, {}))
            return SimpleNamespace(**values)

        def lstat(path):
            return metadata(path, original_lstat(path))

        def fstat(fd):
            info = original_fstat(fd)
            return metadata(inode_paths.get((info.st_dev,info.st_ino)), info)

        patches = [mock.patch.object(install,'LINUX_HOME_ALIAS',alias_path),
                   mock.patch.object(install,'LINUX_CANONICAL_HOME',canonical),
                   mock.patch.object(pwd,'getpwuid',return_value=SimpleNamespace(pw_dir=str(self.account_home))),
                   mock.patch.dict(os.environ, {'HOME':str(self.account_home),'XDG_CONFIG_HOME':''}),
                   mock.patch.object(Path,'lstat',lstat),mock.patch.object(os,'fstat',fstat)]
        for patch in patches:
            patch.start();self.addCleanup(patch.stop)

    def test_bazzite_real_home_derivation_plan_stage_activate_and_binding(self):
        self.real_linux_boundary()
        before = self.snapshot(self.root);home_env = os.environ['HOME']
        p = install.plan(self.args)
        self.assertEqual(p['destination'],self.service)
        self.assertEqual(before,self.snapshot(self.root))
        install.stage(p)
        identity = json.loads((self.root/'bin/upgrade-identity.json').read_text())
        self.assertEqual(identity['definition'],str(self.service))
        self.args.action='activate';p=install.plan(self.args);install.activate(p)
        self.assertEqual(self.service.read_bytes(),p['definition'])
        self.assertEqual(self.service.read_text().splitlines().count('NoNewPrivileges=true'),1)
        self.assertEqual(os.environ['HOME'],home_env)
        self.assertIn(mock.call(['systemctl','--user','start','octendra-agent.service']),self.checked.call_args_list)

    def test_linux_physical_home_and_canonical_home_environment(self):
        self.real_linux_boundary(alias=False)
        self.assertEqual(install.plan(self.args)['destination'],self.service)
        os.environ.pop('HOME')
        self.assertEqual(install.service_target('linux','agent','octendra-agent.service'),self.service)

    def test_linux_canonical_environment_and_same_xdg_directory_are_accepted(self):
        self.real_linux_boundary()
        os.environ['HOME']=str(self.home)
        os.environ['XDG_CONFIG_HOME']=str(self.account_home/'.config')
        self.assertEqual(install.plan(self.args)['destination'],self.service)

    def test_linux_absolute_system_alias_target_is_verified(self):
        self.real_linux_boundary()
        self.home_alias.unlink();self.home_alias.symlink_to(self.system_home,target_is_directory=True)
        self.assertEqual(install.plan(self.args)['destination'],self.service)

    def test_linux_canonical_account_home_accepts_verified_alias_environment(self):
        import pwd
        self.real_linux_boundary()
        with mock.patch.object(pwd,'getpwuid',return_value=SimpleNamespace(pw_dir=str(self.home))):
            self.assertEqual(install.plan(self.args)['destination'],self.service)

    def test_linux_alias_wrong_owner_target_or_reparse_refuses_before_writes(self):
        self.real_linux_boundary()
        for changes in ({'st_uid':os.getuid()},{'st_file_attributes':0x400}):
            with self.subTest(changes=changes):
                self.metadata_overrides[self.home_alias]=changes
                with self.assertRaises(install.Refusal):install.plan(self.args)
        self.metadata_overrides.clear()
        self.home_alias.unlink();self.home_alias.symlink_to('var/other',target_is_directory=True)
        with self.assertRaisesRegex(install.Refusal,'untrusted'):install.plan(self.args)
        self.assertFalse((self.root/'bin').exists());self.assertFalse(self.state.exists())

    def test_linux_full_chain_foreign_owner_write_bits_or_reparse_refuse(self):
        self.real_linux_boundary()
        for path in [self.system_home,self.home,self.home/'.config',self.service.parent]:
            for change in ({'st_uid':os.getuid()+1000},{'st_mode':stat.S_IFDIR|0o775},
                           {'st_mode':stat.S_IFDIR|0o757},{'st_file_attributes':0x400}):
                with self.subTest(path=path,change=change):
                    self.metadata_overrides[path]=change
                    install.probe_service.reset_mock()
                    with self.assertRaises(install.Refusal):install.plan(self.args)
                    install.probe_service.assert_not_called()
                    self.metadata_overrides.clear()
        self.assertFalse((self.root/'bin').exists())

    def test_linux_home_and_user_directory_symlinks_remain_rejected(self):
        self.real_linux_boundary()
        for path in [self.home,self.home/'.config',self.home/'.config/systemd',self.service.parent]:
            with self.subTest(path=path):
                moved=path.with_name(path.name+'-moved');path.rename(moved)
                path.symlink_to(moved,target_is_directory=True)
                try:
                    with self.assertRaises((install.Refusal,OSError)):install.plan(self.args)
                finally:
                    path.unlink();moved.rename(path)
        self.service.symlink_to(self.base/'missing-unit')
        with self.assertRaisesRegex(install.Refusal,'symlink'):install.plan(self.args)

    def test_linux_unsearchable_or_unwritable_user_directories_refuse_in_plan(self):
        self.real_linux_boundary()
        for path,mode in [(self.home,0o600),(self.home/'.config',0o600),
                          (self.service.parent,0o500)]:
            with self.subTest(path=path,mode=mode):
                self.metadata_overrides[path]={'st_mode':stat.S_IFDIR|mode}
                with self.assertRaises(install.Refusal):install.plan(self.args)
                self.metadata_overrides.clear()
        self.assertFalse((self.root/'bin').exists());self.assertFalse(self.state.exists())

    def test_linux_missing_or_nondirectory_unit_parent_refuses_in_plan(self):
        self.real_linux_boundary()
        parent=self.service.parent;parent.rmdir()
        with self.assertRaises(OSError):install.plan(self.args)
        parent.write_bytes(b'not a directory')
        with self.assertRaises(OSError):install.plan(self.args)
        self.assertFalse((self.root/'bin').exists())

    def test_linux_fake_home_xdg_and_ambiguous_account_home_refuse(self):
        self.real_linux_boundary()
        for name,value in [('HOME',str(self.base)),('XDG_CONFIG_HOME',str(self.base)),
                           ('HOME','../home'),('HOME',str(self.home/'../fixture'))]:
            with self.subTest(name=name,value=value),mock.patch.dict(os.environ,{name:value}):
                with self.assertRaises(install.Refusal):install.plan(self.args)
        with mock.patch.object(install.os,'geteuid',return_value=os.getuid()+1):
            with self.assertRaisesRegex(install.Refusal,'identity'):install.plan(self.args)
        self.assertFalse((self.root/'bin').exists())

    def test_linux_parent_replaced_after_plan_refuses_before_activation_request(self):
        self.real_linux_boundary()
        p=install.plan(self.args);install.stage(p);self.args.action='activate';p=install.plan(self.args)
        moved=self.service.parent.with_name('original-user');self.service.parent.rename(moved)
        self.service.parent.symlink_to(moved,target_is_directory=True)
        with self.assertRaises((install.Refusal,OSError)):install.activate(p)
        self.assertFalse((self.root/'activation-request.json').exists())
        self.assertFalse(self.state.exists())
        self.assertFalse(any('enable' in c.args[0] for c in self.checked.call_args_list))

    def test_linux_parent_swap_during_write_never_follows_link_or_starts_service(self):
        self.real_linux_boundary()
        p=install.plan(self.args);install.stage(p);self.args.action='activate';p=install.plan(self.args)
        original=install.exclusive;foreign=self.base/'foreign-unit-dir';foreign.mkdir()
        moved=self.service.parent.with_name('original-user')
        def racing_write(path,data,mode,**kwargs):
            if path==self.service:
                self.service.parent.rename(moved);self.service.parent.symlink_to(foreign,target_is_directory=True)
            return original(path,data,mode,**kwargs)
        with mock.patch.object(install,'exclusive',side_effect=racing_write):
            with self.assertRaises(install.Refusal):install.activate(p)
        self.assertFalse((foreign/self.service.name).exists())
        self.assertTrue((moved/self.service.name).exists())
        self.assertTrue((self.root/'activation-request.json').exists())
        self.assertFalse(any('enable' in c.args[0] for c in self.checked.call_args_list))
        with self.assertRaises((install.Refusal,OSError)):install.plan(self.args)

    def test_linux_alias_change_during_preparation_stops_service_commands(self):
        self.real_linux_boundary()
        p=install.plan(self.args);install.stage(p);self.args.action='activate';p=install.plan(self.args)
        original=install.exclusive
        def changed_alias(path,data,mode,**kwargs):
            value=original(path,data,mode,**kwargs)
            if path==self.service:
                self.home_alias.unlink();self.home_alias.symlink_to('var/other',target_is_directory=True)
            return value
        with mock.patch.object(install,'exclusive',side_effect=changed_alias):
            with self.assertRaises(install.Refusal):install.activate(p)
        self.assertFalse(any('enable' in c.args[0] for c in self.checked.call_args_list))

    def test_linux_existing_unit_race_still_exclusive_with_bound_parent(self):
        self.real_linux_boundary()
        p=install.plan(self.args);install.stage(p);self.args.action='activate';p=install.plan(self.args)
        self.service.write_bytes(b'foreign service')
        with self.assertRaises(FileExistsError):install.activate(p)
        self.assertEqual(self.service.read_bytes(),b'foreign service')
        self.assertFalse(any('enable' in c.args[0] for c in self.checked.call_args_list))

    def test_linux_legitimate_mount_boundaries_are_checked_per_component(self):
        self.real_linux_boundary()
        self.metadata_overrides[self.system_home]={'st_dev':999999}
        self.assertEqual(install.plan(self.args)['destination'],self.service)

    def save_config(self):
        self.config.write_text(json.dumps(self.cfg));self.config.chmod(0o600)

    def snapshot(self, root):
        return {str(p.relative_to(root)):(p.read_bytes() if p.is_file() else None,p.stat().st_mode,p.stat().st_mtime_ns)
                for p in [root,*root.rglob('*')]}

    def tearDown(self):
        self.assertEqual(self.old,self.snapshot(self.legacy),'legacy bytes, modes and mtimes must not change')

    def test_linux_stage_binds_upgrade_to_only_new_paths_and_definition(self):
        p=install.plan(self.args);install.stage(p)
        identity=json.loads((self.root/'bin/upgrade-identity.json').read_text())
        self.assertEqual(identity['service'],'octendra-agent.service')
        self.assertEqual(identity['executable'],str(p['binary']))
        self.assertEqual(identity['config'],str(self.config))
        self.assertEqual(identity['state_dir'],str(self.state))
        self.assertEqual(identity['definition_sha256'],install.digest(p['candidate']))
        self.assertEqual(p['candidate'].read_text().splitlines().count('NoNewPrivileges=true'),1)
        self.assertNotIn('SYNTHETIC-TEST-ONLY',json.dumps(identity))
        self.assertFalse(self.state.exists())

    def test_linux_private_umask_directive_and_macos_plist_key_remain_distinct(self):
        for role in ('agent','gateway'):
            self.args.service_user='fixture';self.args.service_group='fixture'
            unit=install.definition('linux',role,'octendra-'+role+'.service',
                                    Path('/usr/bin/true'),self.config,self.args).decode()
            self.assertEqual(unit.splitlines().count('UMask=0077'),1)
            self.assertNotIn('\nUmask=',unit)
        plist=plistlib.loads(install.definition('macos','agent',install.MAC_ID,
                                               Path('/usr/bin/true'),self.config,self.args))
        self.assertEqual(plist['Umask'],0o077)

    @unittest.skipUnless(shutil.which('systemd-analyze'), 'native systemd-analyze required')
    def test_native_systemd_parser_accepts_umask_and_detects_the_old_unknown_key(self):
        analyzer=shutil.which('systemd-analyze');binary=Path(shutil.which('true'))
        valid=install.definition('linux','agent','umask-valid.service',binary,self.config,self.args)
        invalid=valid.replace(b'\nUMask=0077\n',b'\nUmask=0077\n')
        results=[]
        for name,data in [('umask-valid.service',valid),('umask-invalid.service',invalid)]:
            path=self.base/name;path.write_bytes(data)
            result=subprocess.run([analyzer,'--user','--generators=no','--man=no','verify',str(path)],
                                  env=dict(os.environ,LC_ALL='C',SYSTEMD_LOG_COLOR='0'),
                                  capture_output=True,text=True,timeout=20)
            results.append(result)
        self.assertEqual(results[0].returncode,0,results[0].stderr)
        self.assertNotRegex(results[0].stderr,r'Unknown (key|lvalue|section)')
        # systemd may return zero for an unknown key: the warning must be checked.
        self.assertRegex(results[1].stderr,r'Unknown (key|lvalue).*Umask')

    def test_activation_refuses_tampered_upgrade_binding(self):
        p=install.plan(self.args);install.stage(p)
        path=self.root/'bin/upgrade-identity.json'
        identity=json.loads(path.read_text());identity['service']='other.service';path.write_text(json.dumps(identity))
        self.args.action='activate'
        with self.assertRaisesRegex(install.Refusal,'upgrade identity'):install.plan(self.args)
        self.assertFalse(self.state.exists())
        self.assertFalse(self.service.exists())

    def test_old_bundle_without_instance_binding_protocol_refuses(self):
        manifest=self.package/'manifest.json';value=json.loads(manifest.read_text());value.pop('instance_upgrade_protocol');manifest.write_text(json.dumps(value))
        self.args.manifest_sha256=install.digest(manifest)
        with self.assertRaisesRegex(install.Refusal,'instance-bound upgrade'):install.plan(self.args)
        self.assertFalse((self.root/'bin').exists())

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
                      'https://*.invalid','${NEW_DOMAIN}',
                      'https://127.0.0.1','https://local','https://not_a_domain.invalid'):
            with self.subTest(value=value):
                with self.assertRaises(install.Refusal):install.origin(value)

    def test_https_origin_preserves_nondefault_port_and_normalizes_default(self):
        cases={'https://new.example.invalid':'https://new.example.invalid',
               'https://NEW.EXAMPLE.INVALID:443/':'https://new.example.invalid',
               'https://new.example.invalid:8443/':'https://new.example.invalid:8443',
               'https://new.example.invalid:1':'https://new.example.invalid:1',
               'https://new.example.invalid:65535':'https://new.example.invalid:65535'}
        for value,expected in cases.items():
            with self.subTest(value=value):self.assertEqual(install.origin(value),expected)

    def test_invalid_port_or_ambiguous_https_origin_is_rejected(self):
        cases=('https://new.invalid:0','https://new.invalid:65536','https://new.invalid:-1',
               'https://new.invalid:+8443','https://new.invalid:abc','https://new.invalid:',
               'https://new.invalid:8443:443','https://[broken','https://[::1]:8443',
               'https://@new.invalid:8443','https://new.invalid:8443?',
               'https://new.invalid:8443#',' https://new.invalid:8443',
               'https://new.invalid:8443\n','https://new.in\tvalid:8443',
               'https://new.invalid:8443/extra','https://new.invalid.:8443',
               'https://127.1:8443','https://'+'a'*64+'.invalid:8443',
               'https://'+'.'.join(['a'*63]*4)+':8443')
        for value in cases:
            with self.subTest(value=value):
                with self.assertRaises(install.Refusal):install.origin(value)

    def test_nondefault_origin_plan_preserves_config_without_writes(self):
        self.args.public_origin='https://new.example.invalid:8443'
        self.cfg['gateway_url']=self.args.public_origin;self.save_config()
        before=self.snapshot(self.root);p=install.plan(self.args)
        self.assertEqual(p['origin'],'https://new.example.invalid:8443')
        self.assertEqual(install.public_plan(p)['origin'],self.cfg['gateway_url'])
        self.assertEqual(before,self.snapshot(self.root))
        self.assertFalse(self.state.exists())

    def test_explicit_default_port_is_not_a_distinct_legacy_instance(self):
        self.args.public_origin='https://same.example.invalid:443'
        self.args.legacy_origin='https://same.example.invalid'
        with mock.patch.object(Path,'read_bytes',side_effect=AssertionError('no auth read')):
            with self.assertRaisesRegex(install.Refusal,'different explicit'):install.plan(self.args)

    def test_nondefault_port_is_distinct_but_never_falls_back_to_legacy(self):
        self.args.public_origin='https://same.example.invalid:8443'
        self.args.legacy_origin='https://same.example.invalid'
        self.cfg['gateway_url']=self.args.public_origin;self.save_config()
        self.assertEqual(install.plan(self.args)['origin'],self.args.public_origin)
        self.cfg['gateway_url']=self.args.legacy_origin;self.save_config()
        with self.assertRaisesRegex(install.Refusal,'configuration origin differs'):install.plan(self.args)

    def test_agent_and_gateway_port_mismatch_stop_before_platform_checks(self):
        self.args.public_origin='https://new.example.invalid:8443'
        for role,key in (('agent','gateway_url'),('gateway','public_url')):
            with self.subTest(role=role):
                self.config.unlink();self.args.role=role
                self.config=self.root/'config'/(role+'.json');self.args.config=str(self.config)
                self.cfg={'state_dir':str(self.state),key:'https://new.example.invalid'}
                self.save_config()
                with mock.patch.object(install,'checked',side_effect=AssertionError('no platform commands')):
                    with self.assertRaisesRegex(install.Refusal,'configuration origin differs'):install.plan(self.args)
                self.assertFalse((self.root/'bin').exists());self.assertFalse(self.state.exists())

    def test_invalid_port_stops_before_reading_auth(self):
        self.args.public_origin='https://new.example.invalid:65536'
        with mock.patch.object(Path,'read_bytes',side_effect=AssertionError('no auth read')):
            with self.assertRaises(install.Refusal):install.plan(self.args)

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
        self.assertEqual(self.service.read_bytes(),p['definition'])
        self.assertIn(b'\nNoNewPrivileges=true\n',self.service.read_bytes())

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
        self.assertNotIn(b'NoNewPrivileges=',p['definition'])
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

    def test_windows_xml_can_be_passed_as_unicode_to_task_scheduler(self):
        # COM's XmlText/RegisterTask consume a BSTR, not the original UTF-8
        # bytes. An encoding declaration causes native error 0x8004131a.
        binary=PureWindowsPath('C:/Users/测试 User/octendra/agent/bin/octendra-code.exe')
        config=binary.parent.parent/'config/agent.json'
        text=install.windows_task(binary,config,'S-1-5-21-test').decode('utf-8')
        self.assertNotIn('<?xml',text)
        root=ET.fromstring(text)
        ns={'t':'http://schemas.microsoft.com/windows/2004/02/mit/task'}
        self.assertEqual(root.find('.//t:Command',ns).text,str(binary))
        self.assertEqual(root.find('.//t:Arguments',ns).text,
                         subprocess.list2cmdline(['agent','--config',str(config)]))

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
        verification = [call.args[0] for call in self.checked.call_args_list if '-R' in call.args[0]]
        self.assertEqual(verification, [['codesign', '--verify', '--strict', '-R',
            '=identifier "'+install.MAC_ID+'" and certificate leaf = H"'+'a'*40+'"', str(signed)]])

    def repair_fixture(self):
        self.real_linux_boundary()
        self.root = self.home/'.local/share/octendra/agent'
        (self.root/'config').mkdir(parents=True, mode=0o700)
        self.state = self.root/'state'; self.config = self.root/'config/agent.json'
        self.cfg['state_dir'] = str(self.state); self.save_config()
        self.args.root=str(self.root);self.args.config=str(self.config);self.args.state_dir=str(self.state)
        p=install.plan(self.args);install.stage(p)
        self.args.action='activate';p=install.plan(self.args);install.activate(p)
        old=p['definition'].replace(b'\nUMask=0077\n',b'\nUmask=0077\n')
        self.service.write_bytes(old);(self.root/'service/octendra-agent.service').write_bytes(old)
        identity_path=self.root/'bin/upgrade-identity.json'
        identity=json.loads(identity_path.read_text());identity['definition_sha256']=install.sha_bytes(old)
        identity_path.write_text(json.dumps(identity,indent=2)+'\n')
        marker=self.root/'parallel-install.json';record=json.loads(marker.read_text())
        record['definition_sha256']=install.sha_bytes(old);marker.write_text(json.dumps(record,indent=2)+'\n')
        self.repair_history=marker.read_bytes()
        self.repair_activation=(self.root/'activation-request.json').read_bytes()
        helper=self.package/'parallel-install.py';helper.write_bytes(SCRIPT.read_bytes())
        manifest=self.package/'manifest.json';data=json.loads(manifest.read_text())
        data['artifacts']['parallel-install.py']={'sha256':install.digest(helper),'size':helper.stat().st_size}
        manifest.write_text(json.dumps(data))
        self.args.action='repair-umask';self.args.repair_mode='plan'
        self.args.repair_request_id='873ea936-b6d8-45a3-bad2-958b6ba61f48'
        self.args.expected_definition_sha256=install.sha_bytes(old)
        self.args.expected_activation_sha256=install.sha_bytes(self.repair_activation)
        self.args.expected_stage_sha256=install.sha_bytes(self.repair_history)
        self.args.manifest_sha256=install.digest(manifest)
        patch=mock.patch.object(install,'__file__',str(helper));patch.start();self.addCleanup(patch.stop)
        producer=mock.patch.object(install,'repair_producer',return_value={'pid':4000000,'start_ticks':'1'})
        producer.start();self.addCleanup(producer.stop)
        self.real_producer_finished=install.repair_producer_finished
        ended=mock.patch.object(install,'repair_producer_finished',return_value=True)
        self.producer_finished=ended.start();self.addCleanup(ended.stop)
        self.repair_service_fields=dict(LoadState='loaded',ActiveState='inactive',SubState='dead',
            MainPID='0',ControlPID='0',Job='',FragmentPath=str(self.service),UMask='0022',NoNewPrivileges='yes')
        def manager(argv):
            if argv==['systemctl','--user','daemon-reload']:
                self.repair_service_fields['UMask']='0077';return ''
            if argv[:4]==['systemctl','--user','show','octendra-agent.service']:
                return '\n'.join(k+'='+v for k,v in self.repair_service_fields.items())
            raise AssertionError('unexpected manager action '+repr(argv))
        self.checked.reset_mock();self.checked.side_effect=manager
        return old,p['definition']

    def repair_run(self,mode):
        self.args.repair_mode=mode
        return install.repair_umask(self.args)

    def assert_repair_history(self):
        self.assertEqual((self.root/'parallel-install.json').read_bytes(),self.repair_history)
        self.assertEqual((self.root/'activation-request.json').read_bytes(),self.repair_activation)
        self.assertEqual(self.repair_service_fields['MainPID'],'0')
        actions=[c.args[0] for c in self.checked.call_args_list]
        self.assertTrue(all(c[2] in ('show','daemon-reload') for c in actions))

    def test_umask_repair_plan_observe_apply_and_readonly_repeat(self):
        old,desired=self.repair_fixture();before=self.snapshot(self.root)
        self.assertEqual(self.repair_run('plan')['state'],'repair_planned')
        self.assertEqual(self.repair_run('observe')['state'],'repair_planned')
        self.assertEqual(self.snapshot(self.root),before)
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))
        result=self.repair_run('apply');self.assertFalse(result['read_only'])
        self.assertEqual(self.service.read_bytes(),desired)
        self.assertEqual((self.root/'service/octendra-agent.service').read_bytes(),desired)
        self.assertEqual(json.loads((self.root/'bin/upgrade-identity.json').read_text())['definition_sha256'],install.sha_bytes(desired))
        after=self.snapshot(self.root)
        for mode in ('plan','observe','apply','recover'):
            self.assertTrue(self.repair_run(mode)['read_only'])
        self.assertEqual(after,self.snapshot(self.root))
        self.assertEqual([c.args[0][2] for c in self.checked.call_args_list].count('daemon-reload'),1)
        self.assert_repair_history()

    def test_umask_repair_completed_recognition_may_observe_later_active_service(self):
        self.repair_fixture();self.repair_run('apply')
        self.repair_service_fields.update(ActiveState='active',SubState='running',MainPID='12')
        self.assertTrue(self.repair_run('apply')['read_only'])

    def test_umask_repair_rejects_active_job_unknown_fragment_or_hardening(self):
        self.repair_fixture();before=self.snapshot(self.root)
        baseline=self.repair_service_fields.copy()
        for changes in ({'ActiveState':'active'},{'SubState':'start-pre'},{'MainPID':'12'},
                {'ControlPID':'12'},{'Job':'22'},{'LoadState':'not-found'},
                {'FragmentPath':'/tmp/other.service'},{'UMask':'0000'},{'NoNewPrivileges':'no'}):
            with self.subTest(changes=changes):
                self.repair_service_fields=baseline|changes
                with self.assertRaises(install.Refusal):self.repair_run('apply')
                self.assertEqual(self.snapshot(self.root),before)

    def test_umask_repair_rejects_wrong_external_anchors_or_request_id(self):
        self.repair_fixture();before=self.snapshot(self.root)
        for key,value in (('expected_definition_sha256','0'*64),('expected_activation_sha256','1'*64),
                          ('expected_stage_sha256','3'*64),('manifest_sha256','2'*64),('repair_request_id','../other')):
            with self.subTest(key=key):
                with mock.patch.object(self.args,key,value):
                    with self.assertRaises(install.Refusal):self.repair_run('apply')
                self.assertEqual(self.snapshot(self.root),before)

    def test_umask_repair_refuses_other_platform_role_root_and_paths(self):
        self.repair_fixture()
        for key,value in (('role','gateway'),('root',str(self.base/'octendra/other')),
                          ('config',str(self.root/'config/other.json')),('state_dir',str(self.base/'octendra/state'))):
            with mock.patch.object(self.args,key,value):
                with self.assertRaises(install.Refusal):self.repair_run('apply')
        with mock.patch.object(install,'host_platform',return_value='windows'):
            with self.assertRaises(install.Refusal):self.repair_run('apply')

    def test_umask_repair_refuses_unverified_or_changed_helper_and_package_binary(self):
        self.repair_fixture();helper=Path(install.__file__);original=helper.read_bytes()
        helper.write_bytes(original+b'\n# changed\n')
        with self.assertRaises(install.Refusal):self.repair_run('apply')
        helper.write_bytes(original)
        with mock.patch.object(install,'__file__',str(SCRIPT)):
            with self.assertRaises(install.Refusal):self.repair_run('apply')
        manifest=self.package/'manifest.json';data=json.loads(manifest.read_text())
        data['artifacts']['octendra-code-linux-amd64']['sha256']='a'*64
        manifest.write_text(json.dumps(data));self.args.manifest_sha256=install.digest(manifest)
        with self.assertRaises(install.Refusal):self.repair_run('apply')

    def test_umask_repair_rejects_any_other_unit_edit_even_with_a_matching_external_hash(self):
        old,_=self.repair_fixture()
        changed=old.replace(b'NoNewPrivileges=true',b'NoNewPrivileges=false')
        self.service.write_bytes(changed);self.args.expected_definition_sha256=install.sha_bytes(changed)
        with self.assertRaisesRegex(install.Refusal,'only the exact'):self.repair_run('apply')

    def test_umask_repair_rejects_modified_history_config_binary_identity_or_candidate(self):
        self.repair_fixture()
        paths=[self.root/'parallel-install.json',self.root/'activation-request.json',self.config,
               self.root/'bin/octendra-code',self.root/'bin/upgrade-identity.json',
               self.root/'service/octendra-agent.service',self.service]
        for path in paths:
            with self.subTest(path=path):
                data=path.read_bytes();path.write_bytes(data+b' ')
                with self.assertRaises((install.Refusal,ValueError)):self.repair_run('apply')
                path.write_bytes(data)
        self.assertFalse((self.root/install.REPAIR_FILES[0]).exists())

    def test_umask_repair_rejects_symlink_hardlink_foreign_owner_and_unsafe_mode(self):
        self.repair_fixture();unit=self.service;original=unit.read_bytes();mode=unit.stat().st_mode&0o777
        foreign=self.base/'foreign-unit';foreign.write_bytes(original)
        unit.unlink();unit.symlink_to(foreign)
        with self.assertRaises(install.Refusal):self.repair_run('apply')
        unit.unlink();os.link(foreign,unit)
        with self.assertRaises(install.Refusal):self.repair_run('apply')
        unit.unlink();unit.write_bytes(original);unit.chmod(mode)
        for unsafe in (0o666,0o4644):
            unit.chmod(unsafe)
            with self.assertRaises(install.Refusal):self.repair_run('apply')
        unit.chmod(mode)
        self.metadata_overrides[unit]={'st_uid':os.getuid()+1}
        # repair_read uses dir_fd stat, separately exercise owner rejection there.
        real=os.stat
        def other_owner(name,*a,**kw):
            info=real(name,*a,**kw)
            if name==unit.name:
                values={n:getattr(info,n) for n in dir(info) if n.startswith('st_')}
                return SimpleNamespace(**(values|{'st_uid':os.getuid()+1}))
            return info
        with mock.patch.object(os,'stat',side_effect=other_owner):
            with self.assertRaises(install.Refusal):self.repair_run('apply')
        self.assertEqual(foreign.read_bytes(),original)

    def test_umask_repair_rejects_root_directory_replacement_and_user_symlink(self):
        self.repair_fixture();directory=self.root/'service';moved=self.root/'service-original'
        directory.rename(moved);directory.symlink_to(moved,target_is_directory=True)
        with self.assertRaises((install.Refusal,OSError)):self.repair_run('apply')
        self.assertFalse((self.root/install.REPAIR_FILES[0]).exists())

    def test_umask_repair_existing_upgrade_directory_is_protected_even_empty_or_link(self):
        self.repair_fixture();releases=self.root/'bin/releases';releases.mkdir()
        with self.assertRaisesRegex(install.Refusal,'upgrade activity'):self.repair_run('apply')
        releases.rmdir();releases.symlink_to(self.base,target_is_directory=True)
        with self.assertRaisesRegex(install.Refusal,'upgrade activity'):self.repair_run('apply')

    def test_umask_repair_concurrent_controller_lock_refuses_before_request(self):
        import fcntl
        self.repair_fixture()
        with (self.root/'parallel-install.json').open('rb') as stream:
            fcntl.flock(stream,fcntl.LOCK_EX|fcntl.LOCK_NB)
            with self.assertRaisesRegex(install.Refusal,'another.*active'):self.repair_run('apply')
        self.assertFalse((self.root/install.REPAIR_FILES[0]).exists())

    def interrupted_repair_case(self, interrupted):
        _,desired=self.repair_fixture();real=install.repair_replace;calls=[]
        def interrupted_replace(*a,**kw):
            if len(calls)==interrupted:raise RuntimeError('simulated producer interruption')
            calls.append(1);return real(*a,**kw)
        if interrupted==3:
            def before_reload(argv):
                if argv[:3]==['systemctl','--user','show'] and self.service.read_bytes()==desired and len(calls)==3:
                    raise RuntimeError('simulated producer interruption')
                return '\n'.join(k+'='+v for k,v in self.repair_service_fields.items())
            self.checked.side_effect=before_reload
        with mock.patch.object(install,'repair_replace',side_effect=interrupted_replace):
            with self.assertRaises(RuntimeError):self.repair_run('apply')
        def manager(argv):
            if argv==['systemctl','--user','daemon-reload']:
                self.repair_service_fields['UMask']='0077';return ''
            return '\n'.join(k+'='+v for k,v in self.repair_service_fields.items())
        self.checked.side_effect=manager
        pending=self.snapshot(self.root)
        with self.assertRaisesRegex(install.Refusal,'never replay apply'):self.repair_run('apply')
        self.assertEqual(pending,self.snapshot(self.root))
        self.assertEqual(self.repair_run('observe')['state'],'repair_pending_recovery')
        self.assertEqual(self.repair_run('recover')['state'],'repair_complete')
        self.assertEqual(self.service.read_bytes(),desired);self.assert_repair_history()

    def test_umask_repair_interrupt_before_first_replace(self):self.interrupted_repair_case(0)
    def test_umask_repair_interrupt_after_unit_replace(self):self.interrupted_repair_case(1)
    def test_umask_repair_interrupt_after_candidate_replace(self):self.interrupted_repair_case(2)
    def test_umask_repair_interrupt_after_identity_replace(self):self.interrupted_repair_case(3)

    def test_umask_repair_unknown_reload_not_replayed_then_observed_success(self):
        self.repair_fixture()
        def unknown(argv):
            if argv==['systemctl','--user','daemon-reload']:raise subprocess.TimeoutExpired(argv,30)
            return '\n'.join(k+'='+v for k,v in self.repair_service_fields.items())
        self.checked.side_effect=unknown
        with self.assertRaises(subprocess.TimeoutExpired):self.repair_run('apply')
        self.checked.reset_mock()
        with self.assertRaisesRegex(install.Refusal,'unknown or ineffective'):self.repair_run('recover')
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))
        self.repair_service_fields['UMask']='0077'
        self.assertEqual(self.repair_run('recover')['state'],'repair_complete')
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))
        self.assert_repair_history()

    def test_umask_repair_live_original_producer_cannot_recover(self):
        self.repair_fixture()
        with mock.patch.object(install,'repair_replace',side_effect=RuntimeError('interrupted')):
            with self.assertRaises(RuntimeError):self.repair_run('apply')
        self.producer_finished.return_value=False
        with self.assertRaisesRegex(install.Refusal,'terminated original'):self.repair_run('recover')

    def test_umask_repair_foreign_changes_after_interrupt_are_never_overwritten(self):
        self.repair_fixture()
        with mock.patch.object(install,'repair_replace',side_effect=RuntimeError('interrupted')):
            with self.assertRaises(RuntimeError):self.repair_run('apply')
        self.service.write_bytes(b'foreign change')
        with self.assertRaises(install.Refusal):self.repair_run('recover')
        self.assertEqual(self.service.read_bytes(),b'foreign change')
        self.assertFalse((self.root/install.REPAIR_FILES[2]).exists())

    def test_umask_repair_changed_config_during_write_refuses_before_next_file(self):
        self.repair_fixture();real=install.repair_replace;calls=[]
        def changed(*a,**kw):
            real(*a,**kw);calls.append(1);self.config.write_bytes(self.config.read_bytes()+b' ')
        with mock.patch.object(install,'repair_replace',side_effect=changed):
            with self.assertRaisesRegex(install.Refusal,'immutable repair inputs changed'):self.repair_run('apply')
        self.assertEqual(len(calls),1)
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))

    def test_umask_repair_same_bytes_temporary_can_be_recovered_but_foreign_temp_refuses(self):
        _,desired=self.repair_fixture();real=install.os.replace
        def interrupted(*a,**kw):raise RuntimeError('before atomic replace')
        with mock.patch.object(install.os,'replace',side_effect=interrupted):
            with self.assertRaises(RuntimeError):self.repair_run('apply')
        temp=self.service.with_name('.'+self.service.name+'.umask-'+self.args.repair_request_id+'.tmp')
        self.assertEqual(temp.read_bytes(),desired)
        temp.write_bytes(b'foreign temp')
        with self.assertRaisesRegex(install.Refusal,'temporary differs'):self.repair_run('recover')
        self.assertEqual(temp.read_bytes(),b'foreign temp')
        temp.write_bytes(desired)
        self.assertEqual(self.repair_run('recover')['state'],'repair_complete')

    def test_umask_repair_corrupt_request_reload_or_result_stops_without_replay(self):
        self.repair_fixture()
        for name in install.REPAIR_FILES:
            path=self.root/name;path.write_bytes(b'{');path.chmod(0o600)
            with self.assertRaises((ValueError,install.Refusal)):self.repair_run('apply')
            path.unlink()
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))

    def test_umask_repair_foreign_request_uuid_refuses_own_partial_transaction(self):
        self.repair_fixture()
        with mock.patch.object(install,'repair_replace',side_effect=RuntimeError('interrupted')):
            with self.assertRaises(RuntimeError):self.repair_run('apply')
        self.args.repair_request_id='16bd34af-f708-41b2-9e67-6cf815cd8de5'
        with self.assertRaisesRegex(install.Refusal,'original immutable transaction'):self.repair_run('recover')

    def test_umask_repair_systemd_fragment_uses_only_the_verified_system_home_alias(self):
        self.repair_fixture()
        self.repair_service_fields['FragmentPath']=str(self.account_home/'.config/systemd/user/octendra-agent.service')
        self.assertEqual(self.repair_run('apply')['state'],'repair_complete')

    def test_umask_repair_systemd_fragment_user_symlink_is_refused(self):
        self.repair_fixture();alias=self.home/'user-config'
        alias.symlink_to(self.home/'.config',target_is_directory=True)
        self.repair_service_fields['FragmentPath']=str(alias/'systemd/user/octendra-agent.service')
        with self.assertRaises(install.Refusal):self.repair_run('apply')

    def test_umask_repair_linux_opath_unit_descriptor_is_not_used_for_fsync(self):
        self.repair_fixture();real_boundary=install.linux_unit_parent;real_fsync=os.fsync;bound=[]
        @contextlib.contextmanager
        def boundary(path):
            with real_boundary(path) as fd:
                bound.append(fd);yield fd
        def fsync(fd):
            if fd in bound:raise OSError('simulate Linux O_PATH descriptor fsync refusal')
            return real_fsync(fd)
        with mock.patch.object(install,'linux_unit_parent',boundary), mock.patch.object(os,'fsync',side_effect=fsync):
            self.assertEqual(self.repair_run('apply')['state'],'repair_complete')

    def test_umask_repair_cas_refuses_foreign_replacement_after_temp_write(self):
        self.repair_fixture();real=install.exclusive;changed=b'foreign concurrent unit'
        def replace_target(path,data,mode,**kw):
            real(path,data,mode,**kw)
            if path.name.startswith('.'+self.service.name+'.umask-'):
                self.service.write_bytes(changed)
        with mock.patch.object(install,'exclusive',side_effect=replace_target):
            with self.assertRaisesRegex(install.Refusal,'changed before replacement'):self.repair_run('apply')
        self.assertEqual(self.service.read_bytes(),changed)
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))

    def test_umask_repair_cas_refuses_same_bytes_new_inode_after_temp_write(self):
        self.repair_fixture();real=install.exclusive
        def replace_inode(path,data,mode,**kw):
            real(path,data,mode,**kw)
            if path.name.startswith('.'+self.service.name+'.umask-'):
                previous=self.service.read_bytes();mode=stat.S_IMODE(self.service.stat().st_mode)
                self.service.unlink();self.service.write_bytes(previous);self.service.chmod(mode)
        with mock.patch.object(install,'exclusive',side_effect=replace_inode):
            with self.assertRaisesRegex(install.Refusal,'changed before replacement'):self.repair_run('apply')

    def test_umask_repair_directory_changed_during_write_stops_before_next_write(self):
        self.repair_fixture();real=install.repair_replace;old=self.root/'service'
        def changed(*a,**kw):
            real(*a,**kw);old.rename(self.root/'service-original');old.mkdir(mode=0o700)
        with mock.patch.object(install,'repair_replace',side_effect=changed):
            with self.assertRaisesRegex(install.Refusal,'directory changed'):self.repair_run('apply')
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))

    def test_umask_repair_service_start_during_transaction_stops_before_next_write(self):
        self.repair_fixture();real=install.repair_replace;calls=[]
        def changed(*a,**kw):
            real(*a,**kw);calls.append(1)
            self.repair_service_fields.update(ActiveState='active',SubState='running',MainPID='99')
        with mock.patch.object(install,'repair_replace',side_effect=changed):
            with self.assertRaisesRegex(install.Refusal,'requires.*inactive'):self.repair_run('apply')
        self.assertEqual(len(calls),1)
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))

    def test_umask_repair_partial_json_request_is_preserved_and_never_replayed(self):
        self.repair_fixture();path=self.root/install.REPAIR_FILES[0]
        for data in (b'{',b'null',b'[]',b'{}'):
            path.write_bytes(data);path.chmod(0o600)
            for mode in ('plan','observe','apply','recover'):
                with self.assertRaises((install.Refusal,ValueError)):self.repair_run(mode)
                self.assertEqual(path.read_bytes(),data)
            path.unlink()

    def test_umask_repair_completed_binding_drift_refuses_readonly_repeat(self):
        self.repair_fixture();self.repair_run('apply');self.checked.reset_mock()
        self.service.write_bytes(b'foreign after completed')
        with self.assertRaises(install.Refusal):self.repair_run('apply')
        self.assertFalse(any(c.args[0][2]=='daemon-reload' for c in self.checked.call_args_list))

    def test_umask_repair_process_identity_reuse_alive_absent_and_invalid(self):
        self.repair_fixture()
        # Test the real function rather than the interruption fixture override.
        process_stat='123 (name with ) parentheses) '+' '.join(['S']+['0']*18+['44'])
        with mock.patch.object(Path,'read_text',return_value=process_stat):
            function=self.real_producer_finished
            self.assertFalse(function({'pid':123,'start_ticks':'44'}))
            self.assertTrue(function({'pid':123,'start_ticks':'43'}))
        with mock.patch.object(Path,'read_text',side_effect=FileNotFoundError):
            self.assertTrue(function({'pid':123,'start_ticks':'44'}))
        for invalid in (None,{}, {'pid':True,'start_ticks':'1'},{'pid':1,'start_ticks':'1'},
                        {'pid':123,'start_ticks':44},{'pid':123,'start_ticks':'bad'}):
            with self.assertRaises(install.Refusal):function(invalid)

    def test_umask_repair_corrupt_reload_on_partial_request_refuses_before_more_writes(self):
        self.repair_fixture()
        with mock.patch.object(install,'repair_replace',side_effect=RuntimeError('interrupted')):
            with self.assertRaises(RuntimeError):self.repair_run('apply')
        reload=self.root/install.REPAIR_FILES[1];reload.write_bytes(b'null');reload.chmod(0o600)
        before=self.snapshot(self.root)
        with self.assertRaises(install.Refusal):self.repair_run('recover')
        self.assertEqual(before,self.snapshot(self.root))

    def test_umask_repair_completed_reload_evidence_cannot_be_removed(self):
        self.repair_fixture();self.repair_run('apply')
        (self.root/install.REPAIR_FILES[1]).unlink()
        with self.assertRaises(install.Refusal):self.repair_run('observe')

    def test_umask_repair_restrictive_process_umask_preserves_exact_existing_file_modes(self):
        self.repair_fixture()
        paths=(self.service,self.root/'bin/upgrade-identity.json')
        modes={path:stat.S_IMODE(path.stat().st_mode) for path in paths}
        previous=os.umask(0o077)
        try:self.assertEqual(self.repair_run('apply')['state'],'repair_complete')
        finally:os.umask(previous)
        for path,mode in modes.items():
            self.assertEqual(stat.S_IMODE(path.stat().st_mode),mode)
        for name in install.REPAIR_FILES:
            self.assertEqual(stat.S_IMODE((self.root/name).stat().st_mode),0o600)

    def test_umask_repair_preserves_real_installer_private_0700_0600_modes(self):
        self.repair_fixture()
        paths={self.root/'bin/octendra-code':0o700,self.root/'bin/upgrade-identity.json':0o600,
               self.root/'service/octendra-agent.service':0o600,self.service:0o600}
        for path,mode in paths.items():path.chmod(mode)
        self.assertEqual(self.repair_run('plan')['state'],'repair_planned')
        self.assertEqual(self.repair_run('apply')['state'],'repair_complete')
        for path,mode in paths.items():self.assertEqual(stat.S_IMODE(path.stat().st_mode),mode)
        self.assertTrue(self.repair_run('apply')['read_only']);self.assert_repair_history()

    def test_umask_repair_private_modes_are_pinned_across_interruption(self):
        self.repair_fixture();self.service.chmod(0o600);(self.root/'bin/octendra-code').chmod(0o700)
        (self.root/'bin/upgrade-identity.json').chmod(0o600)
        with mock.patch.object(install,'repair_replace',side_effect=RuntimeError('interrupted')):
            with self.assertRaises(RuntimeError):self.repair_run('apply')
        before=self.snapshot(self.root);self.service.chmod(0o644)
        with self.assertRaisesRegex(install.Refusal,'original immutable transaction'):self.repair_run('recover')
        self.service.chmod(0o600)
        self.assertEqual(self.repair_run('recover')['state'],'repair_complete')
        self.assertEqual(stat.S_IMODE(self.service.stat().st_mode),0o600)

    def test_umask_repair_refuses_loss_of_binary_execute_or_other_mode_expansion(self):
        self.repair_fixture();binary=self.root/'bin/octendra-code'
        for mode in (0o600,0o777,0o775,0o4755):
            binary.chmod(mode)
            with self.assertRaises(install.Refusal):self.repair_run('apply')
        self.assertFalse((self.root/install.REPAIR_FILES[0]).exists())

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

    def test_brand_guard_preserves_only_exact_android_v1_decryption_alias_lines(self):
        spec=importlib.util.spec_from_file_location('android_cipher_brand_guard_fixture',SCRIPT.with_name('check-branding.py'))
        guard=importlib.util.module_from_spec(spec);spec.loader.exec_module(guard)
        for name,line in (('android/app/src/main/java/fun/hackerlife/octendra/ConfigCipher.kt',11),
                          ('android/app/src/test/java/fun/hackerlife/octendra/ConfigCipherTest.kt',25)):
            approved=(SCRIPT.parents[1]/name).read_text().splitlines()[line-1]
            target=self.base/name;target.parent.mkdir(parents=True,exist_ok=True)
            with mock.patch.object(guard,'inventory',return_value=[name]):
                target.write_text(approved+'\n');self.assertTrue(guard.inspect(self.base)['passed'])
                target.write_text(approved+' // changed\n');self.assertFalse(guard.inspect(self.base)['passed'])
            other='android/Other.kt';other_target=self.base/other;other_target.write_text(approved+'\n')
            with mock.patch.object(guard,'inventory',return_value=[other]):
                self.assertFalse(guard.inspect(self.base)['passed'])

    def test_new_proxy_template_preserves_scope_and_routes_upgrade_apis(self):
        text=(SCRIPT.parents[1]/'docs/parallel-octendra/new-host.caddy.in').read_text()
        self.assertIn('${OCTENDRA_NEW_HOST}',text)
        self.assertIn('${OCTENDRA_REVIEWED_TLS_CONFIGURATION}',text)
        self.assertIn('reverse_proxy 127.0.0.1:18788',text)
        self.assertNotIn('127.0.0.1:18787',text)
        for path in ('/admin/gateway-upgrade','/admin/release-bundle','/device/upgrade-directive','/device/release-artifact/*'):
            self.assertNotIn(path,text)
        self.assertNotIn('respond ',text)


if __name__=='__main__':unittest.main()
