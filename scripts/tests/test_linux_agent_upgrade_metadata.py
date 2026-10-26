"""Real local replacement and rollback must preserve private installed metadata."""
import importlib.util
import pathlib
import stat
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / 'upgrade-code-agent-linux.py'
SPEC = importlib.util.spec_from_file_location('linux_agent_metadata', SCRIPT)
UPDATER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(UPDATER)


class LinuxAgentMetadata(unittest.TestCase):
    def test_install_and_rollback_keep_existing_private_mode_and_owner(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            target = root/'installed';target.write_bytes(b'previous')
            candidate = root/'candidate';candidate.write_bytes(b'candidate')
            backup = root/'backup';backup.write_bytes(b'previous')
            candidate.chmod(0o755)
            for mode in (0o700, 0o750, 0o755):
                with self.subTest(mode=oct(mode)):
                    target.chmod(mode)
                    before = target.stat()
                    for source in (candidate, backup):
                        UPDATER.atomic_copy(source, target)
                        after = target.stat()
                        self.assertEqual(target.read_bytes(), source.read_bytes())
                        self.assertEqual((after.st_uid, after.st_gid, stat.S_IMODE(after.st_mode)),
                                         (before.st_uid, before.st_gid, mode))

    def test_failed_copy_preserves_original_bytes_metadata_and_has_no_temporary_file(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            target = root/'installed';target.write_bytes(b'previous');target.chmod(0o700)
            before = target.stat()
            with self.assertRaises(FileNotFoundError):
                UPDATER.atomic_copy(root/'absent', target)
            after = target.stat()
            self.assertEqual(target.read_bytes(), b'previous')
            self.assertEqual((after.st_ino, after.st_uid, after.st_gid, stat.S_IMODE(after.st_mode)),
                             (before.st_ino, before.st_uid, before.st_gid, 0o700))
            self.assertEqual(sorted(p.name for p in root.iterdir()), ['installed'])


if __name__ == '__main__':
    unittest.main()
