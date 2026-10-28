#!/usr/bin/env python3
"""Offline, read-only projection of existing Code Gateway client metadata.

No config, owner, session, grant or token is opened. SQLite internally parses
the shared client JSON column; only the four public fields reach Python. This
does not claim that secret-bearing database pages are untouched by SQLite.
Run with the existing state owner's filesystem access; never change permissions.
"""
import contextlib
import json
import os
from pathlib import Path
import re
import sqlite3
import stat
import sys
import time
import urllib.parse


MAX_CLIENTS = 1000  # auth/clients.rs registration capacity.
PUBLIC_FIELDS = ("client_id", "client_name", "redirect_uris",
                 "token_endpoint_auth_method")
# Same namespaces/fields as auth.rs::oauth_state_kind and auth/clients.rs::Client.
# CASE bounds JSON parsing; no complete value or secret field is returned.
SQL = "SELECT kind,key," + ",".join(
    "CASE WHEN length(value)<=65536 AND json_valid(value) "
    "THEN json_extract(value,'$.%s') END" % field
    for field in PUBLIC_FIELDS
) + " FROM kv WHERE kind IN ('client','oauth2_client') AND expires>? " \
    "ORDER BY kind,key LIMIT 1001"


class MetadataError(Exception):
    def __init__(self, code):
        self.code = code
        super().__init__()


def error_code(error, stage):
    if isinstance(error, MetadataError):
        return error.code
    if isinstance(error, FileNotFoundError):
        return "state_path_missing"
    if isinstance(error, PermissionError):
        return "filesystem_access_denied"
    if isinstance(error, sqlite3.Error):
        # Inspect locally, but never return the SQLite message or its operands.
        message = str(error).lower()
        for marker, code in (
                ("unsafe use of", "sqlite_unsafe_schema_function"),
                ("no such function", "sqlite_json_function_unavailable"),
                ("not authorized", "sqlite_authorizer_denied"),
                ("not a database", "sqlite_not_database"),
                ("malformed database schema", "sqlite_schema_invalid"),
                ("database is locked", "sqlite_busy"),
                ("interrupted", "sqlite_query_budget_exceeded")):
            if marker in message:
                return code
        return "sqlite_operation_failed"
    return "public_metadata_invalid" if stage == "public_metadata" else "diagnostic_failed"


def authorizer(action, table, column, database, source):
    if action == sqlite3.SQLITE_SELECT:
        return sqlite3.SQLITE_OK
    if (action == sqlite3.SQLITE_READ and database == "main" and table == "kv"
            and column in ("kind", "key", "value", "expires")):
        return sqlite3.SQLITE_OK
    if (action == sqlite3.SQLITE_FUNCTION
            and column in ("length", "json_valid", "json_extract")):
        return sqlite3.SQLITE_OK
    return sqlite3.SQLITE_DENY


def regular(path):
    info = os.lstat(path)
    if not stat.S_ISREG(info.st_mode):
        raise MetadataError("state_file_not_regular")
    return info


def project(rows):
    if len(rows) > MAX_CLIENTS:
        raise MetadataError("client_count_exceeds_limit")
    clients, redacted = [], 0
    for kind, key, client_id, name, encoded_uris, method in rows:
        if (not isinstance(client_id, str)
                or not re.fullmatch(r"(?:rh2_)?[0-9a-f]{64}", client_id)
                or key != client_id
                or kind != ("oauth2_client" if client_id.startswith("rh2_")
                            else "client")):
            raise MetadataError("public_client_identity_invalid")
        if method not in ("none", "client_secret_basic", "client_secret_post"):
            raise MetadataError("public_auth_method_invalid")
        if name is not None and (not isinstance(name, str) or not name
                or len(name.encode("utf-8")) > 200
                or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in name)):
            raise MetadataError("public_client_name_invalid")
        # This JSON contains only the selected public redirect array, not Client.
        uris = json.loads(encoded_uris)
        if not isinstance(uris, list) or not 1 <= len(uris) <= 6:
            raise MetadataError("public_redirects_invalid")
        clean = []
        for uri in uris:
            if (not isinstance(uri, str) or len(uri.encode("utf-8")) > 4096
                    or any(ord(c) <= 32 or ord(c) == 127 for c in uri)):
                raise MetadataError("public_redirects_invalid")
            parsed = urllib.parse.urlsplit(uri)
            if (parsed.scheme not in ("http", "https") or not parsed.hostname
                    or parsed.username is not None or parsed.password is not None):
                raise MetadataError("public_redirects_invalid")
            parsed.port  # Validate malformed authorities without printing them.
            if parsed.query or parsed.fragment:
                uri = urllib.parse.urlunsplit((parsed.scheme, parsed.netloc,
                                              parsed.path, "", ""))
                redacted += 1
            clean.append(uri)
        clients.append({"client_name": name, "client_id": client_id,
                        "redirect_uris": clean,
                        "token_endpoint_auth_method": method})
    return {"clients": clients, "redacted_redirect_uris": redacted}


def json_capability(connection):
    return connection.execute(
        "SELECT json_valid('{}'),json_extract('{\"probe\":1}','$.probe')"
    ).fetchone() == (1, 1)


def inspect(path, context=None):
    context = {} if context is None else context
    checks = context.setdefault("checks", {})
    context["stage"] = "path_stat"
    path = Path(path).absolute()
    checks["state_file_exists"] = os.path.lexists(path)
    before = regular(path)
    checks.update(state_file_regular=True, state_file_bytes=before.st_size)
    # Do not follow a sidecar symlink or use immutable=1, which can omit live WAL.
    for suffix in ("-wal", "-shm"):
        sidecar = Path(str(path) + suffix)
        if os.path.lexists(sidecar):
            regular(sidecar)
    checks["existing_sidecars_regular"] = True
    deadline = time.monotonic() + 3
    context["stage"] = "sqlite_connect"
    with contextlib.closing(sqlite3.connect(
            path.as_uri() + "?mode=ro", uri=True, timeout=2,
            isolation_level=None)) as connection:
        checks["sqlite_connected"] = True
        checks["sqlite_version"] = sqlite3.sqlite_version
        context["stage"] = "sqlite_readonly_setup"
        connection.execute("PRAGMA query_only=ON")
        connection.execute("PRAGMA trusted_schema=OFF")
        connection.execute("PRAGMA temp_store=MEMORY")
        context["stage"] = "sqlite_json_capability"
        checks["json_functions_available"] = json_capability(connection)
        if not checks["json_functions_available"]:
            raise MetadataError("sqlite_json_function_unavailable")
        context["stage"] = "known_schema"
        columns = connection.execute(
            "SELECT name,type FROM pragma_table_info('kv')"
        ).fetchall()
        checks["known_kv_schema"] = dict(columns) == {
            "kind": "TEXT", "key": "TEXT", "value": "TEXT", "expires": "INTEGER"}
        if not checks["known_kv_schema"]:
            raise MetadataError("state_schema_unexpected")
        connection.set_authorizer(authorizer)
        connection.set_progress_handler(
            lambda: int(time.monotonic() > deadline), 1000)
        context["stage"] = "public_projection"
        rows = connection.execute(SQL, (int(time.time()),)).fetchall()
        checks["public_projection_completed"] = True
        context["stage"] = "path_recheck"
        after = regular(path)
        if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino):
            raise MetadataError("state_file_changed")
        context["stage"] = "public_metadata"
        result = project(rows)
        checks["public_metadata_valid"] = True
        result["diagnostic"] = checks
        return result


def main(argv=None):
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 2 or args[0] != "--state-db":
        result, code = {"error": "invalid_arguments"}, 2
    else:
        context = {}
        try:
            result, code = inspect(args[1], context), 0
        except Exception as error:
            # Never print exceptions, paths, SQL parameters or partial rows.
            stage = context.get("stage", "path_stat")
            result, code = {"error": error_code(error, stage), "stage": stage,
                            "error_class": type(error).__name__,
                            "diagnostic": context.get("checks", {})}, 2
    sys.stdout.write(json.dumps(result, ensure_ascii=True) + "\n")
    return code


if __name__ == "__main__":
    raise SystemExit(main())
