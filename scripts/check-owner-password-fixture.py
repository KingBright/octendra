#!/usr/bin/env python3
"""POSIX CLI acceptance using newly generated, disposable artificial files only.

No service is started. No real configuration/password path is accepted. The only
input is the candidate binary; init-gateway creates fixtures in our private tmp.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class OfflinePasswordFixture(unittest.TestCase):
    binary = None

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="octendra-password-fixture-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.config = self.root / "gateway.json"
        self.password = self.root / "fixture.password"
        result = self.run_cli("init-gateway", "--config", self.config,
                              "--password-file", self.password,
                              "--public-url", "https://fixture.invalid",
                              "--bind", "127.0.0.1:1", "--owner", "fixture-only",
                              "--state-dir", self.root / "unused-state")
        self.assertEqual(result.returncode, 0, "artificial fixture initialization")
        self.assertFalse((self.root / "unused-state").exists())

    def run_cli(self, *args):
        return subprocess.run([str(self.binary), *map(str, args)],
                              capture_output=True, timeout=20)

    def check(self, token, code, config=None, password=None):
        result = self.run_cli("check-owner-password", "--config", config or self.config,
                              "--password-file", password or self.password)
        self.assertEqual(result.returncode, code)
        self.assertEqual(result.stdout, (token + "\n").encode())
        self.assertEqual(result.stderr, b"")

    def test_match_and_no_content_or_metadata_changes(self):
        def snapshot(p):
            s = p.stat()
            return (p.read_bytes(), s.st_mode, s.st_uid, s.st_gid, s.st_ino,
                    s.st_mtime_ns, s.st_ctime_ns)
        before = [snapshot(p) for p in (self.config, self.password)]
        self.check("match", 0)
        self.assertEqual(before, [snapshot(p) for p in (self.config, self.password)])
        self.assertEqual(sorted(p.name for p in self.root.iterdir()),
                         ["fixture.password", "gateway.json"])

    def test_mismatch_and_exact_password_bytes(self):
        self.password.write_bytes(self.password.read_bytes() + b"\n")
        self.check("mismatch", 1)
        self.password.write_bytes(b"fixture-different-password")
        self.check("mismatch", 1)

    def test_private_readonly_and_unsafe_modes(self):
        for path in (self.config, self.password):
            path.chmod(0o400)
        self.check("match", 0)
        for path in (self.config, self.password):
            for mode in (0o644, 0o640, 0o660, 0o700, 0o4600):
                path.chmod(mode)
                self.check("error", 2)
            path.chmod(0o600)

    def test_symlink_parents_files_and_hardlinks(self):
        for path in (self.config, self.password):
            alias = self.root / "alias"
            alias.symlink_to(path)
            self.check("error", 2, **{"config" if path == self.config else "password": alias})
            alias.unlink()
            os.link(path, alias)
            self.check("error", 2)
            alias.unlink()
        alias = self.root / "parent-alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.check("error", 2, config=alias / self.config.name)
        self.check("error", 2, password=alias / self.password.name)

    def test_missing_special_empty_and_size_limits(self):
        for path, limit in ((self.config, 1024 * 1024), (self.password, 4096)):
            original = path.read_bytes()
            path.unlink()
            self.check("error", 2)
            path.mkdir()
            self.check("error", 2)
            path.rmdir()
            os.mkfifo(path, 0o600)
            self.check("error", 2)
            path.unlink()
            path.write_bytes(b"")
            path.chmod(0o600)
            self.check("error", 2)
            path.write_bytes(b"x" * (limit + 1))
            self.check("error", 2)
            path.write_bytes(original)

    def test_invalid_config_hash_and_work_parameters_are_redacted(self):
        original = json.loads(self.config.read_text())
        bad_budget = dict(original)
        bad_budget["password_hash"] = original["password_hash"].replace("m=19456", "m=999999999")
        for content in ("fixture-secret-not-json", '{"password_hash":"fixture-secret-bad-hash"}',
                        json.dumps(bad_budget)):
            self.config.write_text(content)
            self.check("error", 2)

    def test_argument_errors_are_redacted(self):
        for args in ((), ("--help",), ("--password", "fixture-secret-argument"),
                     ("--config", "fixture-secret-path")):
            result = self.run_cli("check-owner-password", *args)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(result.stdout, b"error\n")
            self.assertEqual(result.stderr, b"")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("POSIX fixture only")
    OfflinePasswordFixture.binary = args.binary.resolve(strict=True)
    unittest.main(argv=[__file__], verbosity=2)
