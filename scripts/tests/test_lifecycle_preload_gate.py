"""Native temporary directory locks and CAS; never real skills maintenance."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import importlib.util

SOURCE=Path(__file__).resolve().parents[1]/'lifecycle_preload_gate.py'
spec=importlib.util.spec_from_file_location('octendra_preload_gate_fixture',SOURCE)
g=importlib.util.module_from_spec(spec);spec.loader.exec_module(g)


class PreloadGateTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.root=Path(self.tmp.name).resolve();self.gate=g.Gate(self.root)

    def hold(self):return self.gate.change('begin','fixture-window','absent','a'*64)

    def test_legacy_open_reads_without_writing_and_requires_scoped_loader(self):
        with self.assertRaisesRegex(g.Refusal,'scoped'):g.require_admitted()
        with self.gate.admission():g.require_admitted()
        self.assertEqual(list(self.root.iterdir()),[])

    def home_alias(self):
        home=self.root/'real-home';home.mkdir(mode=0o700)
        skills=home/'.codex/skills';skills.mkdir(parents=True,mode=0o700)
        alias=self.root/'logical-home';alias.symlink_to(home,target_is_directory=True)
        return home,alias,skills

    def test_trusted_home_alias_uses_same_skills_inode_and_hold(self):
        home,alias,skills=self.home_alias()
        with patch.object(g.Path,'home',return_value=alias):
            gate=g.Gate()
            with gate.admission():
                g.require_admitted()
                with self.assertRaisesRegex(g.Refusal,'busy'):
                    g.Gate(skills).change('begin','fixture-window','absent','a'*64)
            hold=gate.change('begin','fixture-window','absent','a'*64)
            with self.assertRaisesRegex(g.Refusal,'held'):
                with g.Gate(skills).admission():pass
            self.assertEqual(gate.verify('fixture-window',hold['state_sha256'])['state'],'hold')
            gate.change('end','fixture-window',hold['state_sha256'])
            with gate.admission():g.require_admitted()
        self.assertEqual(gate.root,skills)

    def test_codex_and_skills_links_below_trusted_home_are_still_rejected(self):
        for component in ('.codex','skills'):
            with self.subTest(component=component),tempfile.TemporaryDirectory() as folder:
                base=Path(folder).resolve();home=base/'home';home.mkdir(mode=0o700)
                target=base/'target';target.mkdir(mode=0o700)
                if component=='.codex':
                    (target/'skills').mkdir();(home/'.codex').symlink_to(target,target_is_directory=True)
                else:
                    (home/'.codex').mkdir();(home/'.codex/skills').symlink_to(target,target_is_directory=True)
                with patch.object(g.Path,'home',return_value=home):
                    with self.assertRaises((OSError,g.Refusal)):
                        with g.admission():pass
                self.assertFalse((target/g.DIRECTORY).exists())

    def test_explicit_root_alias_is_not_normalized(self):
        _,alias,_=self.home_alias()
        with self.assertRaises((OSError,g.Refusal)):
            with g.Gate(alias/'.codex/skills').admission():pass

    def test_home_alias_retarget_during_admission_is_rejected(self):
        home,alias,_=self.home_alias();other=self.root/'other-home';other.mkdir(mode=0o700)
        (other/'.codex/skills').mkdir(parents=True,mode=0o700)
        with patch.object(g.Path,'home',return_value=alias),g.Gate().admission():
            alias.unlink();alias.symlink_to(other,target_is_directory=True)
            try:
                with self.assertRaisesRegex(g.Refusal,'root_changed'):g.require_admitted()
            finally:
                alias.unlink();alias.symlink_to(home,target_is_directory=True)

    def test_home_retarget_during_begin_refuses_success_and_preserves_original_hold(self):
        home,alias,skills=self.home_alias();other=self.root/'other-home';other.mkdir(mode=0o700)
        (other/'.codex/skills').mkdir(parents=True,mode=0o700)
        original=g.os.replace
        def replace(*args,**kwargs):
            result=original(*args,**kwargs)
            alias.unlink();alias.symlink_to(other,target_is_directory=True)
            return result
        with patch.object(g.Path,'home',return_value=alias),patch.object(g.os,'replace',side_effect=replace):
            with self.assertRaisesRegex(g.Refusal,'root_changed'):
                g.Gate().change('begin','fixture-window','absent','a'*64)
        self.assertTrue((skills/g.DIRECTORY/g.STATE).is_file())
        self.assertEqual(list((other/'.codex/skills').iterdir()),[])

    def test_directory_inode_replacement_still_invalidates_admission(self):
        skills=self.root/'skills';skills.mkdir(mode=0o700)
        with g.Gate(skills).admission():
            skills.rename(self.root/'prior-skills');skills.mkdir(mode=0o700)
            try:
                with self.assertRaisesRegex(g.Refusal,'root_changed|admission_changed'):g.require_admitted()
            finally:
                skills.rmdir();(self.root/'prior-skills').rename(skills)

    def test_hold_survives_original_writer_exit_and_explicit_end_restores(self):
        code="import sys;sys.path.insert(0,sys.argv[1]);import lifecycle_preload_gate as g;print(g.encoded(g.Gate(sys.argv[2]).change('begin','fixture-window','absent','a'*64)).decode())"
        child=subprocess.run([sys.executable,'-B','-c',code,str(Path(g.__file__).parent),str(self.root)],capture_output=True,text=True)
        self.assertEqual(child.returncode,0,child.stderr);hold=json.loads(child.stdout)
        self.assertEqual(self.gate.verify('fixture-window',hold['state_sha256'])['state'],'hold')
        with self.assertRaisesRegex(g.Refusal,'held'):
            with self.gate.admission():self.fail('new reader admitted')
        opened=self.gate.change('end','fixture-window',hold['state_sha256'])
        self.assertEqual(opened['state'],'open')
        with self.gate.admission():g.require_admitted()
        self.assertTrue((self.root/g.DIRECTORY/g.STATE).exists())

    def test_duplicates_cas_and_foreign_window_do_not_rewrite(self):
        hold=self.hold();raw=(self.root/g.DIRECTORY/g.STATE).read_bytes()
        self.assertEqual(self.gate.change('begin','fixture-window',hold['state_sha256'],'a'*64),hold)
        self.assertEqual(raw,(self.root/g.DIRECTORY/g.STATE).read_bytes())
        for action,window,pin in [('end','other',hold['state_sha256']),('end','fixture-window','0'*64),('begin','other',hold['state_sha256'])]:
            with self.assertRaises(g.Refusal):self.gate.change(action,window,pin,'a'*64)
        opened=self.gate.change('end','fixture-window',hold['state_sha256'])
        self.assertEqual(self.gate.change('end','fixture-window',opened['state_sha256']),opened)

    def test_begin_rejects_live_reader_without_creating_hold(self):
        with self.gate.admission():
            with self.assertRaisesRegex(g.Refusal,'busy'):self.hold()
        self.assertFalse((self.root/g.DIRECTORY).exists())
        self.hold()

    def test_independent_process_shared_lease_blocks_begin_until_normal_exit(self):
        code="import sys;sys.path.insert(0,sys.argv[1]);import lifecycle_preload_gate as g\nwith g.Gate(sys.argv[2]).admission():\n print('admitted',flush=True)\n sys.stdin.readline()\n"
        child=subprocess.Popen([sys.executable,'-B','-c',code,str(Path(g.__file__).parent),str(self.root)],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
        try:
            self.assertEqual(child.stdout.readline().strip(),'admitted')
            with self.assertRaisesRegex(g.Refusal,'busy'):self.hold()
            child.communicate('\n',timeout=10);self.assertEqual(child.returncode,0)
            self.assertEqual(self.hold()['state'],'hold')
        finally:
            if child.poll() is None:child.communicate('\n',timeout=10)

    def test_unknown_json_schema_mode_missing_state_and_pending_are_preserved(self):
        self.hold();state=self.root/g.DIRECTORY/g.STATE;valid=state.read_bytes()
        unknown=[b'{broken',b'{"schema":1,"schema":1}',valid.replace(b'"schema":1',b'"schema":2')]
        for raw in unknown:
            state.write_bytes(raw)
            with self.assertRaises(g.Refusal):
                with self.gate.admission():pass
            self.assertEqual(state.read_bytes(),raw)
        state.write_bytes(valid);state.chmod(0o644)
        with self.assertRaises(g.Refusal):
            with self.gate.admission():pass
        state.chmod(0o600);state.unlink()
        with self.assertRaises(g.Refusal):
            with self.gate.admission():pass
        pending=state.with_name(g.PENDING);pending.write_bytes(b'partial');pending.chmod(0o600)
        with self.assertRaises(g.Refusal):self.gate.change('end','fixture-window',g.sha(valid))
        self.assertEqual(pending.read_bytes(),b'partial')

    def test_symlink_state_or_protocol_directory_refuses_without_following(self):
        outside=self.root/'outside';outside.write_bytes(b'protected')
        (self.root/g.DIRECTORY).symlink_to(outside)
        with self.assertRaises((OSError,g.Refusal)):
            with self.gate.admission():pass
        self.assertEqual(outside.read_bytes(),b'protected')

    def test_partial_transition_crash_remains_unknown_and_does_not_auto_open(self):
        with patch.object(g.os,'replace',side_effect=OSError('fixture crash')):
            with self.assertRaises(OSError):self.hold()
        self.assertTrue((self.root/g.DIRECTORY/g.PENDING).exists())
        with self.assertRaisesRegex(g.Refusal,'unknown'):
            with self.gate.admission():pass
        with self.assertRaises(g.Refusal):self.hold()

    def test_end_refuses_unknown_installer_transaction(self):
        hold=self.hold();path=self.root/'.rust-project-lifecycle-update.json';path.write_bytes(b'unknown')
        with self.assertRaisesRegex(g.Refusal,'original_controller'):
            self.gate.change('end','fixture-window',hold['state_sha256'])
        self.assertEqual(self.gate.verify('fixture-window',hold['state_sha256'])['state'],'hold')
        self.assertEqual(path.read_bytes(),b'unknown')
