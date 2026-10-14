import importlib.util
from pathlib import Path
import tempfile
import unittest

PATH = Path(__file__).resolve().parents[1] / "inspect-code-agent-linux-execution.py"
spec = importlib.util.spec_from_file_location("execution_diagnostic", PATH)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class ExecutionContextTests(unittest.TestCase):
    def test_unit_restriction_is_distinguished_from_already_restricted_parent(self):
        agent = {"no_new_privileges": True}
        self.assertEqual(module.classify({"NoNewPrivileges": "yes"}, agent, {"no_new_privileges": False}), "service_restriction_inherited")
        self.assertEqual(module.classify({"NoNewPrivileges": "no"}, agent, {"no_new_privileges": True}), "already_restricted_parent_chain")
    def test_absent_flag_and_unrestricted_flag_do_not_claim_sudo_authority(self):
        self.assertEqual(module.classify({}, {}, {}), "process_restriction_unavailable")
        self.assertEqual(module.classify({}, {"no_new_privileges": False}, {}), "no_new_privileges_not_set_sudo_policy_not_tested")
    def test_selected_status_metadata_never_returns_environment_or_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); process = root / "42"; process.mkdir()
            (process / "status").write_text("Name:\tagent\nPPid:\t21\nUid:\t1000 1000 1000 1000\nGid:\t1000 1000 1000 1000\nNoNewPrivs:\t1\nUnrelatedField:\tdo-not-export\n")
            (process / "environ").write_bytes(b"PASSWORD=do-not-read")
            value = module.process_security(42, root)
            self.assertTrue(value["no_new_privileges"])
            self.assertEqual(value["parent_pid"], 21)
            self.assertNotIn("do-not-export", str(value))
            self.assertNotIn("PASSWORD", str(value))
            self.assertEqual((process / "environ").read_bytes(), b"PASSWORD=do-not-read")
    def test_missing_process_is_unavailable_not_unrestricted(self):
        with tempfile.TemporaryDirectory() as directory:
            value = module.process_security(42, Path(directory))
            self.assertFalse(value["available"])
            self.assertNotIn("no_new_privileges", value)

if __name__ == "__main__":
    unittest.main()
