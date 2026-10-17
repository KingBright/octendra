"""Managed-service preparation fixtures; no root installation or service commands."""
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest
import uuid

SCRIPT=Path(__file__).resolve().parents[1]/'prepare-admin-helper.py'
spec=importlib.util.spec_from_file_location('managed_preparation',SCRIPT)
m=importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)

class ManagedPreparationTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.root=Path(self.tmp.name)
        self.binary=self.root/'elf'
        data=bytearray(120);data[:4]=b'\x7fELF';data[4]=2;data[5]=1
        struct.pack_into('<H',data,18,62);struct.pack_into('<Q',data,32,64)
        struct.pack_into('<H',data,54,56);struct.pack_into('<H',data,56,1);struct.pack_into('<I',data,64,1)
        self.binary.write_bytes(data)
    def tearDown(self):self.tmp.cleanup()
    def prepare(self,**kw):
        a=dict(binary=self.binary,output=self.root/'bundle',device_id=str(uuid.uuid4()),
               account='test',uid=1000,gid=1000,home='/unconfirmed',
               source_commit='a'*40,source_inputs_sha256='b'*64)
        a.update(kw);return m.prepare_managed(**a)
    def test_default_bundle_has_no_active_grant_or_installer_script(self):
        report=self.prepare();root=self.root/'bundle'
        self.assertFalse(report['default_enabled']);self.assertFalse(report['installed'])
        self.assertFalse(json.loads((root/'policy.json').read_text())['enabled'])
        grant=json.loads((root/'service-grant.json').read_text())
        self.assertFalse(grant['enabled']);self.assertEqual(grant['expires_at'],0)
        self.assertEqual(grant['unit'],'remote-hosts-code-agent.service')
        self.assertFalse(any(p.suffix in {'.py','.sh'} for p in root.iterdir()))
    def test_service_points_to_native_closed_profile_without_cleanup(self):
        self.prepare();text=(self.root/'bundle/service.template').read_text()
        self.assertIn('linux-helper serve',text);self.assertIn('--grant /var/lib/remote-hosts-admin/service-grant.json',text)
        self.assertNotIn(' apply ',text);self.assertNotIn('sing-box',text)
        self.assertNotIn('/etc/systemd/system /var',text)
    def test_manifest_covers_every_installable_member_and_source_identity(self):
        report=self.prepare();root=self.root/'bundle'
        self.assertEqual(report['protocol'],2);self.assertEqual(report['source_commit'],'a'*40)
        self.assertEqual(set(report['files']),{'remote-hosts-admin','policy.json','service-grant.json','service.template','INSTALL.txt'})
        for name,digest in report['files'].items():self.assertEqual(m.sha(root/name),digest)
        self.assertEqual(m.sha(root/'manifest.json'),report['manifest_sha256'])
    def test_no_root_uid_or_gid_scope(self):
        for args in [{'uid':0},{'gid':0}]:
            with self.assertRaises(ValueError):self.prepare(**args)
    def test_symlink_binary_and_non_linux_binary_are_rejected(self):
        link=self.root/'link';link.symlink_to(self.binary)
        with self.assertRaises(ValueError):self.prepare(binary=link)
        self.binary.write_bytes(b'#!/bin/sh\nexit 0')
        with self.assertRaises(ValueError):self.prepare()
    def test_output_is_private_and_never_replaced(self):
        self.prepare();self.assertEqual((self.root/'bundle').stat().st_mode&0o777,0o700)
        with self.assertRaises(FileExistsError):self.prepare()
    def test_instructions_require_native_root_bootstrap_and_original_uuid(self):
        self.prepare();text=(self.root/'bundle/INSTALL.txt').read_text()
        self.assertIn('root-owned, non-writable ancestors',text)
        self.assertIn('ORIGINAL operation UUID',text)
        self.assertIn('No install.py, shell installer',text)
        self.assertIn('Separate OS administrator installation',text)
        self.assertIn('do not delete it to re-enable',text)

if __name__=='__main__':unittest.main()
