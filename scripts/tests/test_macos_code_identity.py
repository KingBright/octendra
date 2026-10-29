import importlib.util
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "macos_code_identity.py"
SPEC = importlib.util.spec_from_file_location("macos_code_identity_test", SCRIPT)
IDENTITY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(IDENTITY)


class MacOsCodeIdentityTests(unittest.TestCase):
    def test_keychain_failures_and_timeouts_never_echo_secret_arguments(self):
        secret = 'artificial-secret-for-redaction-test'
        command = ['/usr/bin/security', 'unlock-keychain', '-p', secret, '/private/keychain']
        with mock.patch.object(IDENTITY.subprocess, 'run', return_value=subprocess.CompletedProcess(command, 51, secret, secret)):
            with self.assertRaisesRegex(RuntimeError, 'keychain_unlock_rejected') as failure:
                IDENTITY._security(*command[1:])
        self.assertNotIn(secret, str(failure.exception))
        self.assertNotIn(secret, repr(failure.exception.args))
        with mock.patch.object(IDENTITY.subprocess, 'run', side_effect=subprocess.TimeoutExpired(command, 30, output=secret, stderr=secret)):
            with self.assertRaises(subprocess.TimeoutExpired) as timed_out:
                IDENTITY._security(*command[1:])
        self.assertNotIn(secret, str(timed_out.exception))
        self.assertNotIn(secret, repr(timed_out.exception.cmd))
        self.assertIsNone(timed_out.exception.output)

    def test_existing_identity_lookup_requires_exact_public_fingerprint(self):
        fingerprint = 'a'*40
        for listing, expected in [
            (f'1) {fingerprint.upper()} "historical approved subject"\n', 'certificate_available'),
            (f'1) {fingerprint} "subject" (CSSMERR_TP_NOT_TRUSTED)\n', 'authorization_required'),
            (f'1) {"b"*40} "subject containing {fingerprint}"\n', 'authorization_required')]:
            with mock.patch.object(IDENTITY, '_security', return_value=subprocess.CompletedProcess([], 0, listing)) as security:
                self.assertEqual(IDENTITY.existing_status(fingerprint)['state'], expected)
            self.assertEqual(security.call_args.args, ('find-identity', '-v', '-p', 'codesigning'))

    def test_existing_identity_signs_copy_without_unlock_create_or_trust(self):
        fingerprint = 'a'*40
        with tempfile.TemporaryDirectory() as tmp:
            source = pathlib.Path(tmp)/'release'; source.write_bytes(b'artificial immutable release')
            destination = pathlib.Path(tmp)/'signed'
            with mock.patch.object(IDENTITY, '_security', return_value=subprocess.CompletedProcess([], 0, f'1) {fingerprint} "existing"\n')) as security, \
                 mock.patch.object(IDENTITY.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as run, \
                 mock.patch.object(IDENTITY, 'create') as create, mock.patch.object(IDENTITY, 'authorize') as authorize:
                result = IDENTITY.sign_existing_copy(source, destination, fingerprint)
            self.assertEqual(source.read_bytes(), b'artificial immutable release')
            self.assertEqual(result['installed_sha256'], IDENTITY.sha(destination))
            self.assertEqual(security.call_count, 1)
            create.assert_not_called(); authorize.assert_not_called()
            self.assertEqual(run.call_count, 2)
            for call in run.call_args_list:
                self.assertEqual(call.args[0][0], '/usr/bin/codesign')
                self.assertNotIn('--keychain', call.args[0])
            self.assertIn('='+result['designated_requirement'], run.call_args.args[0])

    def test_missing_existing_identity_never_signs_or_creates_replacement(self):
        with mock.patch.object(IDENTITY, '_security', return_value=subprocess.CompletedProcess([], 0, '')), \
             mock.patch.object(IDENTITY.subprocess, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'no replacement identity'):
                IDENTITY.sign_existing_copy('/unused', '/unused', 'a'*40)
        run.assert_not_called()

    def authorization_required(self, base):
        paths = IDENTITY._paths(base)
        return {
            "state": "authorization_required",
            "certificate_name": IDENTITY.CERT_NAME,
            "certificate_sha1": "a" * 40,
            "certificate": str(paths["certificate"]),
            "keychain": str(paths["keychain"]),
            "code_identifier": IDENTITY.CODE_IDENTIFIER,
            "designated_requirement": f'identifier "{IDENTITY.CODE_IDENTIFIER}" and certificate leaf = H"{"a" * 40}"',
        }

    def test_authorize_scopes_one_time_trust_to_code_signing(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            waiting = self.authorization_required(base)
            ready = {**waiting, "state": "ready"}
            completed = subprocess.CompletedProcess([], 0, "", "")
            with mock.patch.object(IDENTITY, "status", side_effect=[waiting, ready]), \
                 mock.patch.object(IDENTITY, "_security", return_value=completed) as security:
                result = IDENTITY.authorize(base)
            self.assertEqual(result["state"], "ready")
            args = security.call_args.args
            self.assertEqual(args[:5], ("add-trusted-cert", "-r", "trustRoot", "-p", "codeSign"))
            self.assertNotIn("-k", args)
            self.assertEqual(security.call_args.kwargs["timeout"], 120)

    def test_valid_identity_rejects_untrusted_or_unsearchable_identity(self):
        fingerprint = "a" * 40
        keychain = pathlib.Path("/tmp/octendra-signing.keychain-db")
        trusted = subprocess.CompletedProcess([], 0,
            f'1) {fingerprint.upper()} "{IDENTITY.CERT_NAME}"\n', "")
        untrusted = subprocess.CompletedProcess([], 0,
            f'1) {fingerprint.upper()} "{IDENTITY.CERT_NAME}" (CSSMERR_TP_NOT_TRUSTED)\n', "")
        searchable = subprocess.CompletedProcess([], 0, f'"{keychain}"\n', "")
        missing = subprocess.CompletedProcess([], 0, '"/Users/test/Library/Keychains/login.keychain-db"\n', "")
        with mock.patch.object(IDENTITY, "_security", return_value=untrusted):
            self.assertFalse(IDENTITY._valid_identity(keychain, fingerprint))
        with mock.patch.object(IDENTITY, "_security", side_effect=[trusted, missing]):
            self.assertFalse(IDENTITY._valid_identity(keychain, fingerprint))
        with mock.patch.object(IDENTITY, "_security", side_effect=[trusted, searchable]):
            self.assertTrue(IDENTITY._valid_identity(keychain, fingerprint))

    def test_existing_certificate_subject_does_not_change_fingerprint_identity(self):
        fingerprint = "a" * 40
        keychain = pathlib.Path("/tmp/octendra-signing.keychain-db")
        existing = subprocess.CompletedProcess([], 0,
            f'1) {fingerprint.upper()} "Approved historical subject"\n', "")
        listed = subprocess.CompletedProcess([], 0, f'"{keychain}"\n', "")
        with mock.patch.object(IDENTITY, "_security", side_effect=[existing, listed]):
            self.assertTrue(IDENTITY._valid_identity(keychain, fingerprint))
        certificates = subprocess.CompletedProcess([], 0,
            "SHA-1 hash: " + "b" * 40 + "\nSHA-1 hash: " + fingerprint + "\n", "")
        with mock.patch.object(IDENTITY, "_security", return_value=certificates):
            self.assertEqual(IDENTITY._certificate_sha1(keychain, fingerprint), fingerprint)
            with self.assertRaises(RuntimeError):
                IDENTITY._certificate_sha1(keychain, "c" * 40)
            with self.assertRaises(RuntimeError):
                IDENTITY._certificate_sha1(keychain)

    def test_missing_installation_identity_never_creates_or_authorizes_replacement(self):
        import sys
        source = SCRIPT.with_name("install-code-agent.py")
        spec = importlib.util.spec_from_file_location("install_identity_fixture", source)
        installer = importlib.util.module_from_spec(spec)
        with mock.patch.dict(sys.modules, {"macos_code_identity": IDENTITY}):
            spec.loader.exec_module(installer)
        with tempfile.TemporaryDirectory() as tmp:
            with mock.patch.object(sys, "argv", [str(source), "--binary", tmp + "/candidate", "--config", tmp + "/agent.json"]), \
                 mock.patch.object(installer.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)), \
                 mock.patch.object(IDENTITY, "status", return_value={"state": "missing"}), \
                 mock.patch.object(IDENTITY, "create") as create, \
                 mock.patch.object(IDENTITY, "authorize") as authorize:
                with self.assertRaisesRegex(SystemExit, "no replacement identity"):
                    installer.main()
                create.assert_not_called()
                authorize.assert_not_called()

    def test_ensure_keychain_search_list_prepends_private_keychain_once(self):
        keychain = pathlib.Path("/tmp/octendra-signing.keychain-db")
        listed = subprocess.CompletedProcess([], 0, '"/Users/test/Library/Keychains/login.keychain-db"\n', "")
        completed = subprocess.CompletedProcess([], 0, "", "")
        with mock.patch.object(IDENTITY, "_security", side_effect=[listed, completed]) as security:
            self.assertTrue(IDENTITY._ensure_keychain_search_list(keychain))
        self.assertEqual(security.call_args_list[1].args[:5],
                         ("list-keychains", "-d", "user", "-s", keychain.resolve()))

    def test_authorize_timeout_remains_recoverable_and_does_not_claim_success(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            waiting = self.authorization_required(base)
            with mock.patch.object(IDENTITY, "status", return_value=waiting), \
                 mock.patch.object(IDENTITY, "_ensure_keychain_search_list", return_value=False), \
                 mock.patch.object(IDENTITY, "_security", side_effect=subprocess.TimeoutExpired("security", 120)):
                result = IDENTITY.authorize(base)
            self.assertEqual(result["state"], "authorization_required")
            self.assertEqual(result["authorization_outcome"], "timed_out_without_confirmation")
            self.assertIn("logged_in_user_session", result["next_action"])

    def test_sign_copy_embeds_explicit_stable_designated_requirement(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = pathlib.Path(tmp)
            paths = IDENTITY._paths(base)
            paths["root"].mkdir(parents=True)
            paths["password"].write_text("fixture-password\n")
            paths["keychain"].write_bytes(b"fixture-keychain")
            source = base / "candidate"
            destination = base / "installed"
            source.write_bytes(b"candidate-bytes")
            ready = {
                "state": "ready",
                "certificate_name": IDENTITY.CERT_NAME,
                "certificate_sha1": "a" * 40,
                "keychain": str(paths["keychain"]),
                "code_identifier": IDENTITY.CODE_IDENTIFIER,
                "designated_requirement": f'identifier "{IDENTITY.CODE_IDENTIFIER}" and certificate leaf = H"{"a" * 40}"',
            }
            embedded = []

            def run(command, **_kwargs):
                if command[0] == "/usr/bin/codesign" and "--requirements" in command:
                    requirement = pathlib.Path(command[command.index("--requirements") + 1])
                    embedded.append(requirement.read_text())
                return subprocess.CompletedProcess(command, 0, "", "")

            requirement_output = "Executable=fixture\ndesignated => " + ready["designated_requirement"] + "\n"
            with mock.patch.object(IDENTITY, "status", return_value=ready), \
                 mock.patch.object(IDENTITY, "_security"), \
                 mock.patch.object(IDENTITY.subprocess, "run", side_effect=run), \
                 mock.patch.object(IDENTITY.subprocess, "check_output", return_value=requirement_output):
                result = IDENTITY.sign_copy(source, destination, base)
            self.assertEqual(embedded, ["designated => " + ready["designated_requirement"] + "\n"])
            self.assertEqual(result["installed_sha256"], IDENTITY.sha(source))


if __name__ == "__main__":
    unittest.main()
