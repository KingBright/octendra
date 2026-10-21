"""Coexisting instance upgrade selection; all files/processes/services are fixtures."""
import hashlib
import importlib.util
import json
import contextlib
import io
import sqlite3
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

SCRIPTS=Path(__file__).resolve().parents[1]
sys.path.insert(0,str(SCRIPTS))
import upgrade_instance as binding
spec=importlib.util.spec_from_file_location('bound_gateway_upgrade',SCRIPTS/'upgrade-code-gateway.py')
updater=importlib.util.module_from_spec(spec);spec.loader.exec_module(updater)


class UpgradeInstanceTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.base=Path(self.tmp.name).resolve();self.root=self.base/'octendra/gateway'
        self.config=self.root/'config/gateway.json';self.binary=self.root/'bin/octendra-code'
        self.state=self.root/'state';self.definition=self.base/'units/octendra-gateway.service'
        self.release=self.binary.parent/'releases/0.12.1'
        self.candidate=self.release/'octendra-code-linux-amd64';self.result=self.release/'result.json'
        for path in (self.config.parent,self.binary.parent,self.state,self.definition.parent,self.release):path.mkdir(parents=True,exist_ok=True)
        self.config.write_text(json.dumps({'state_dir':str(self.state),'public_url':'https://new.example.invalid'}));self.config.chmod(0o600)
        self.binary.write_bytes(b'new-instance previous binary');self.candidate.write_bytes(b'new-instance candidate')
        self.definition.write_text('new unit definition')
        (self.state/'state.sqlite').write_bytes(b'LIVE DATABASE MUST REMAIN')
        self.identity={'schema':1,'role':'gateway','service':'octendra-gateway.service',
            'executable':str(self.binary),'config':str(self.config),'state_dir':str(self.state),
            'definition':str(self.definition),'definition_sha256':hashlib.sha256(self.definition.read_bytes()).hexdigest()}
        self.save()
        self.proc=self.base/'proc/2468';self.proc.mkdir(parents=True)
        (self.proc/'exe').symlink_to(self.binary)
        (self.proc/'cgroup').write_text('0::/system.slice/octendra-gateway.service\n')
        self.legacy=self.base/'maintenance';self.legacy.mkdir()
        for name in ('binary','config','state.sqlite','service'):(self.legacy/name).write_bytes(('old-'+name).encode())
        self.original=self.snapshot(self.legacy)
        self.cfg_bytes=self.config.read_bytes();self.db_bytes=(self.state/'state.sqlite').read_bytes()

    def snapshot(self,path):
        return {str(p.relative_to(path)):(p.read_bytes() if p.is_file() else None,p.stat().st_mode,p.stat().st_mtime_ns) for p in (path,*path.rglob('*'))}

    def tearDown(self):
        self.assertEqual(self.original,self.snapshot(self.legacy))
        self.assertEqual(self.cfg_bytes,self.config.read_bytes())
        self.assertEqual(self.db_bytes,(self.state/'state.sqlite').read_bytes())

    def save(self):binding.identity_path(self.config).write_text(json.dumps(self.identity))

    def load(self,config=None,binary=None,service=None,pid=2468):
        return binding.load_gateway(config or self.config,binary or self.binary,service or 'octendra-gateway.service',pid,
                                    definition_root=self.definition.parent,proc_root=self.proc.parent)

    def test_selects_only_bound_new_service_with_legacy_present(self):
        i=self.load();self.assertEqual(i['service'],'octendra-gateway.service')
        binding.validate_release_paths(i,self.candidate,self.result,self.binary.parent/'releases')

    def test_missing_identity_fails_without_service_calls(self):
        binding.identity_path(self.config).unlink()
        with self.assertRaises((FileNotFoundError,RuntimeError)):self.load()

    def test_rejects_old_arbitrary_and_option_service_names(self):
        for value in ('octendra-code-gateway.service','elsewhere.service','--all','x.service; id'):
            with self.subTest(value=value),self.assertRaises(RuntimeError):self.load(service=value)

    def test_rejects_each_identity_path_mismatch(self):
        for key in ('executable','config','state_dir','definition'):
            with self.subTest(key=key):
                old=self.identity[key];self.identity[key]=str(self.legacy/key);self.save()
                with self.assertRaises(RuntimeError):self.load()
                self.identity[key]=old;self.save()

    def test_rejects_changed_service_definition(self):
        self.definition.write_text('ExecStart=other service')
        with self.assertRaises(RuntimeError):self.load()

    def test_rejects_wrong_running_process_and_cgroup(self):
        with self.assertRaises(RuntimeError):self.load(pid=None)
        (self.proc/'cgroup').write_text('0::/system.slice/octendra-code-gateway.service\n')
        with self.assertRaises(RuntimeError):self.load()
        (self.proc/'cgroup').write_text('0::/system.slice/octendra-gateway.service\n')
        (self.proc/'exe').unlink();(self.proc/'exe').symlink_to(self.legacy/'binary')
        with self.assertRaises(RuntimeError):self.load()

    def test_rejects_symlink_identity(self):
        path=binding.identity_path(self.config);content=path.read_bytes();path.unlink()
        outside=self.base/'outside';outside.write_bytes(content);path.symlink_to(outside)
        with self.assertRaises(RuntimeError):self.load()

    def test_rejects_cross_instance_and_dotdot_release_outputs(self):
        i=self.load()
        for candidate,result,backup in ((self.legacy/'binary',self.result,self.binary.parent/'releases'),
             (self.candidate,self.legacy/'result.json',self.binary.parent/'releases'),
             (self.candidate,self.result,self.legacy),
             (self.release/'../evil',self.result,self.binary.parent/'releases')):
            with self.assertRaises(RuntimeError):binding.validate_release_paths(i,candidate,result,backup)

    def test_gateway_cli_checks_binding_before_backups_or_service_stop(self):
        args=['upgrade-code-gateway.py','--candidate',str(self.candidate),'--sha256',hashlib.sha256(self.candidate.read_bytes()).hexdigest(),
              '--version','0.12.1','--result',str(self.result),'--binary-path',str(self.binary),
              '--config-path',str(self.config),'--backup-root',str(self.binary.parent/'releases'),
              '--service-name','octendra-gateway.service','--expected-pid','2468','--gateway-bind','127.0.0.1:18788']
        with mock.patch.object(sys,'argv',args),mock.patch.object(updater.os,'getuid',return_value=0),\
             mock.patch.object(binding,'load_gateway',side_effect=RuntimeError('fixture binding mismatch')),\
             mock.patch.object(updater.subprocess,'check_call') as call:
            with self.assertRaisesRegex(RuntimeError,'binding mismatch'):updater.main()
            call.assert_not_called()
        self.assertFalse(self.result.exists())

    def test_successful_cutover_and_failed_start_rollback_preserve_both_instances(self):
        database=self.state/'state.sqlite';database.unlink()
        with contextlib.closing(sqlite3.connect(database)) as db:
            db.execute('CREATE TABLE preserved(value TEXT)')
            db.execute("INSERT INTO preserved VALUES('synthetic user data')")
            db.commit()
        self.db_bytes=database.read_bytes()
        previous=self.binary.read_bytes();candidate_sha=hashlib.sha256(self.candidate.read_bytes()).hexdigest()
        args=['upgrade-code-gateway.py','--candidate',str(self.candidate),'--sha256',candidate_sha,
              '--version','0.12.1','--result',str(self.result),'--binary-path',str(self.binary),
              '--config-path',str(self.config),'--backup-root',str(self.binary.parent/'releases'),
              '--service-name','octendra-gateway.service','--expected-pid','2468','--gateway-bind','127.0.0.1:18788']
        original_load=binding.load_gateway;original_checksum=updater.checksum
        for fail_start in (False,True):
            with self.subTest(fail_start=fail_start):
                if self.result.exists():self.result.unlink()
                self.binary.write_bytes(previous)
                calls=[];start_count=[0]
                def command(argv,**kwargs):
                    calls.append(argv)
                    if argv[:2]==['systemctl','start']:
                        start_count[0]+=1
                        if fail_start and start_count[0]==1:raise RuntimeError('fixture new service start failed')
                    return 0
                def output(argv,**kwargs):
                    if argv[-1]=='release-manifest':return '{"instance_upgrade_protocol":1}'
                    return 'MainPID=2468' if argv[0]=='systemctl' else 'octendra-code 0.12.1'
                def load(config,binary,service,pid):
                    return original_load(config,binary,service,pid,definition_root=self.definition.parent,proc_root=self.proc.parent)
                with mock.patch.object(sys,'argv',args),mock.patch.object(updater.os,'getuid',return_value=0),\
                     mock.patch.object(binding,'load_gateway',side_effect=load),\
                     mock.patch.object(updater,'require_task_authorization_support'),\
                     mock.patch.object(updater.subprocess,'check_call',side_effect=command),\
                     mock.patch.object(updater.subprocess,'check_output',side_effect=output),\
                     mock.patch.object(updater.urllib.request,'urlopen',side_effect=lambda *a,**k:io.BytesIO(b'{"version":"0.12.1","file_transfer":true}')),\
                     mock.patch.object(updater,'checksum',side_effect=lambda p:candidate_sha if str(p).startswith('/proc/') else original_checksum(p)),\
                     mock.patch.object(updater.time,'strftime',return_value='failure' if fail_start else 'success'),\
                     contextlib.redirect_stdout(io.StringIO()):
                    if fail_start:
                        with self.assertRaises(SystemExit):updater.main()
                    else:updater.main()
                result=json.loads(self.result.read_text())
                self.assertEqual(result['state'],'failed' if fail_start else 'upgraded')
                self.assertEqual(self.binary.read_bytes(),previous if fail_start else self.candidate.read_bytes())
                service_calls=[c for c in calls if c[0]=='systemctl']
                self.assertTrue(service_calls)
                self.assertTrue(all(c[2]=='octendra-gateway.service' for c in service_calls))
                self.assertEqual(self.cfg_bytes,self.config.read_bytes());self.assertEqual(self.db_bytes,database.read_bytes())

    def test_runner_requires_process_and_service_identity(self):
        source=(SCRIPTS/'gateway_self_upgrade_runner.py').read_text()
        self.assertIn("p.add_argument('--expected-pid',type=int,required=True)",source)
        self.assertLess(source.index('identity=load_gateway'),source.index('with tarfile.open'))


if __name__=='__main__':unittest.main()
