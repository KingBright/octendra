"""Synthetic state only; never opens a production config or credential."""
import hashlib
import importlib.util
import contextlib
import io
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "inspect-oauth-client-metadata.py"
SPEC = importlib.util.spec_from_file_location("client_metadata", SCRIPT)
metadata = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(metadata)
SECRET = "SYNTHETIC_SECRET_MUST_NEVER_APPEAR"
PUBLIC_ID = "1" * 64
CONFIDENTIAL_ID = "rh2_" + "2" * 64


class MetadataTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "state.sqlite"
        with sqlite3.connect(self.path) as db:
            db.execute("CREATE TABLE kv (kind TEXT,key TEXT,value TEXT,expires INTEGER,"
                       "PRIMARY KEY(kind,key))")
        self.put("client", PUBLIC_ID, self.client(PUBLIC_ID, "ChatGPT", "none"))
        self.put("oauth2_client", CONFIDENTIAL_ID,
                 self.client(CONFIDENTIAL_ID, "Other fixture", "client_secret_post"))

    def client(self, client_id, name, method):
        return {"client_id": client_id, "client_name": name,
                "redirect_uris": ["https://chatgpt.com/connector_platform_oauth_redirect"],
                "token_endpoint_auth_method": method,
                "client_secret_hash": SECRET, "client_secret": SECRET,
                "access_token": SECRET, "owner": SECRET}

    def put(self, kind, key, value, expires=9223372036854775807):
        with sqlite3.connect(self.path) as db:
            db.execute("INSERT OR REPLACE INTO kv VALUES(?,?,?,?)",
                       (kind, key, json.dumps(value), expires))

    def run_cli(self, *args):
        p = subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True,
                           text=True, timeout=8)
        self.assertNotIn(SECRET, p.stdout + p.stderr)
        self.assertEqual(p.stderr, "")
        return p, json.loads(p.stdout)

    def test_four_public_fields_only_and_no_other_state_read(self):
        for kind in ("access", "refresh", "oauth2_access", "oauth2_refresh",
                     "pending", "code", "status_session", "runtime"):
            self.put(kind, SECRET, {"client_id": SECRET, "owner": SECRET,
                                   "session": SECRET, "token": SECRET})
        before = self.path.read_bytes()
        p, result = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 0)
        self.assertEqual(len(result["clients"]), 2)
        self.assertTrue(result["diagnostic"]["known_kv_schema"])
        self.assertEqual(result["diagnostic"]["sqlite_version"], sqlite3.sqlite_version)
        for row in result["clients"]:
            self.assertEqual(set(row), {"client_name", "client_id", "redirect_uris",
                                        "token_endpoint_auth_method"})
        self.assertEqual(hashlib.sha256(before).digest(),
                         hashlib.sha256(self.path.read_bytes()).digest())

    def test_current_committed_wal_is_not_silently_ignored(self):
        with sqlite3.connect(self.path) as writer:
            writer.execute("PRAGMA journal_mode=WAL")
            value = self.client("3" * 64, "Latest committed fixture", "none")
            writer.execute("INSERT INTO kv VALUES(?,?,?,?)",
                           ("client", "3" * 64, json.dumps(value), 9223372036854775807))
            writer.commit()
            self.assertTrue(Path(str(self.path) + "-wal").exists())
            before = self.path.read_bytes()
            wal = Path(str(self.path) + "-wal").read_bytes()
            p, result = self.run_cli("--state-db", str(self.path))
            self.assertEqual(p.returncode, 0)
            self.assertEqual(len(result["clients"]), 3)
            self.assertEqual(self.path.read_bytes(), before)
            self.assertEqual(Path(str(self.path) + "-wal").read_bytes(), wal)

    def test_expired_clients_are_not_current_registration_evidence(self):
        self.put("client", "4" * 64, self.client("4" * 64, "Expired", "none"), 1)
        p, result = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 0)
        self.assertEqual(len(result["clients"]), 2)

    def test_redirect_query_and_fragment_are_not_exposed(self):
        value = self.client(PUBLIC_ID, "Fixture", "none")
        value["redirect_uris"] = ["https://chatgpt.com/callback?token=" + SECRET + "#" + SECRET]
        self.put("client", PUBLIC_ID, value)
        p, result = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 0)
        self.assertEqual(result["redacted_redirect_uris"], 1)
        self.assertEqual(result["clients"][0]["redirect_uris"],
                         ["https://chatgpt.com/callback"])

    def test_invalid_public_metadata_never_prints_partial_rows_or_errors(self):
        for value in (SECRET, {"client_id": SECRET},
                      self.client(PUBLIC_ID, "Fixture", SECRET)):
            with self.subTest(value_type=type(value).__name__):
                self.put("client", PUBLIC_ID, value)
                p, result = self.run_cli("--state-db", str(self.path))
                self.assertEqual(p.returncode, 2)
                self.assertEqual(result["stage"], "public_metadata")
                self.assertIn(result["error"], ("public_client_identity_invalid",
                                                "public_auth_method_invalid"))
                self.assertNotIn("clients", result)

    def test_malformed_json_and_wrong_namespaces_fail_without_dump(self):
        with sqlite3.connect(self.path) as db:
            db.execute("UPDATE kv SET value=? WHERE key=?", (SECRET, PUBLIC_ID))
        p, _ = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 2)
        self.put("client", PUBLIC_ID, self.client(CONFIDENTIAL_ID, "Fixture", "none"))
        p, _ = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 2)

    def test_missing_database_and_symlinks_do_not_create_or_follow_state(self):
        missing = Path(self.temp.name) / SECRET
        p, _ = self.run_cli("--state-db", str(missing))
        self.assertEqual(p.returncode, 2)
        self.assertFalse(missing.exists())
        link = Path(self.temp.name) / "link.sqlite"
        link.symlink_to(self.path)
        p, _ = self.run_cli("--state-db", str(link))
        self.assertEqual(p.returncode, 2)

    def test_authorizer_refuses_unrelated_tables_and_writes(self):
        with sqlite3.connect(self.path) as db:
            db.execute("CREATE TABLE sessions (value TEXT)")
            db.execute("INSERT INTO sessions VALUES(?)", (SECRET,))
            db.commit()
            db.set_authorizer(metadata.authorizer)
            try:
                for query in ("SELECT * FROM sessions", "DELETE FROM kv", "PRAGMA user_version=1",
                              "SELECT load_extension('never-load-a-library')"):
                    with self.subTest(query=query), self.assertRaises(sqlite3.DatabaseError):
                        db.execute(query)
            finally:
                db.set_authorizer(None)

    def test_oversized_client_set_returns_no_partial_metadata(self):
        with sqlite3.connect(self.path) as db:
            for i in range(1001):
                client_id = "%064x" % (100 + i)
                db.execute("INSERT INTO kv VALUES(?,?,?,?)",
                           ("client", client_id,
                            json.dumps(self.client(client_id, "Fixture", "none")),
                            9223372036854775807))
        p, result = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 2)
        self.assertEqual(result["error"], "client_count_exceeds_limit")
        self.assertEqual(result["stage"], "public_metadata")
        self.assertNotIn("clients", result)

    def test_sidecar_symlinks_are_rejected(self):
        link = Path(str(self.path) + "-wal")
        link.symlink_to(self.path)
        p, _ = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 2)

    def test_argument_errors_never_echo_input(self):
        p, result = self.run_cli("--unexpected", SECRET, SECRET)
        self.assertEqual(p.returncode, 2)
        self.assertEqual(result, {"error": "invalid_arguments"})

    def call_main(self):
        output, errors = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            code = metadata.main(["--state-db", str(self.path)])
        self.assertNotIn(SECRET, output.getvalue() + errors.getvalue())
        self.assertEqual(errors.getvalue(), "")
        return code, json.loads(output.getvalue())

    def test_path_failure_is_distinct_and_does_not_echo_path(self):
        p, result = self.run_cli("--state-db", str(Path(self.temp.name) / SECRET))
        self.assertEqual(p.returncode, 2)
        self.assertEqual(result["stage"], "path_stat")
        self.assertEqual(result["error"], "state_path_missing")
        self.assertFalse(result["diagnostic"]["state_file_exists"])

    def test_connection_failure_is_classified_without_exception_text(self):
        with mock.patch.object(metadata.sqlite3, "connect", side_effect=
                               sqlite3.OperationalError("unable to open " + SECRET)):
            code, result = self.call_main()
        self.assertEqual(code, 2)
        self.assertEqual(result["stage"], "sqlite_connect")
        self.assertEqual(result["error_class"], "OperationalError")
        self.assertTrue(result["diagnostic"]["state_file_regular"])

    def test_missing_json_function_is_distinct(self):
        with mock.patch.object(metadata, "json_capability", side_effect=
                               sqlite3.OperationalError("no such function: " + SECRET)):
            code, result = self.call_main()
        self.assertEqual(code, 2)
        self.assertEqual(result["stage"], "sqlite_json_capability")
        self.assertEqual(result["error"], "sqlite_json_function_unavailable")
        self.assertTrue(result["diagnostic"]["sqlite_connected"])

    def test_unexpected_schema_does_not_return_schema_or_row_values(self):
        with sqlite3.connect(self.path) as db:
            db.execute("DROP TABLE kv")
            db.execute("CREATE TABLE kv (unrelated TEXT)")
            db.execute("INSERT INTO kv VALUES(?)", (SECRET,))
        p, result = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 2)
        self.assertEqual(result["stage"], "known_schema")
        self.assertEqual(result["error"], "state_schema_unexpected")
        self.assertFalse(result["diagnostic"]["known_kv_schema"])

    def test_known_gateway_expression_index_is_included_in_fixture(self):
        with sqlite3.connect(self.path) as db:
            db.execute("CREATE INDEX kv_kind_state_created ON kv "
                       "(kind,json_extract(value,'$.state'),json_extract(value,'$.created_at'))")
        p, result = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 0)
        self.assertTrue(result["diagnostic"]["json_functions_available"])
        self.assertTrue(result["diagnostic"]["known_kv_schema"])

    def test_historical_check_and_expression_index_keep_original_guards(self):
        with sqlite3.connect(self.path) as db:
            db.execute("ALTER TABLE kv RENAME TO old_kv")
            db.execute("CREATE TABLE kv (kind TEXT,key TEXT,value TEXT CHECK(json_valid(value)),"
                       "expires INTEGER,PRIMARY KEY(kind,key))")
            db.execute("INSERT INTO kv SELECT * FROM old_kv")
            db.execute("DROP TABLE old_kv")
            db.execute("CREATE INDEX kv_kind_state_created ON kv "
                       "(kind,json_extract(value,'$.state'),json_extract(value,'$.created_at'))")
        before = self.path.read_bytes()
        p, result = self.run_cli("--state-db", str(self.path))
        self.assertEqual(p.returncode, 0)
        self.assertEqual(len(result["clients"]), 2)
        self.assertEqual(self.path.read_bytes(), before)

    def test_unsafe_schema_errors_are_categorical_and_remain_denied(self):
        with mock.patch.object(metadata, "json_capability", side_effect=
                               sqlite3.DatabaseError("malformed database schema: unsafe use of " + SECRET)):
            code, result = self.call_main()
        self.assertEqual(code, 2)
        self.assertEqual(result["error"], "sqlite_unsafe_schema_function")
        self.assertEqual(result["stage"], "sqlite_json_capability")
        self.assertNotIn("clients", result)


if __name__ == "__main__":
    unittest.main()
