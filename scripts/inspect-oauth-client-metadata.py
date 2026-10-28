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
    pass


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
        raise MetadataError()
    return info


def project(rows):
    if len(rows) > MAX_CLIENTS:
        raise MetadataError()
    clients, redacted = [], 0
    for kind, key, client_id, name, encoded_uris, method in rows:
        if (not isinstance(client_id, str)
                or not re.fullmatch(r"(?:rh2_)?[0-9a-f]{64}", client_id)
                or key != client_id
                or kind != ("oauth2_client" if client_id.startswith("rh2_")
                            else "client")
                or method not in ("none", "client_secret_basic", "client_secret_post")):
            raise MetadataError()
        if name is not None and (not isinstance(name, str) or not name
                or len(name.encode("utf-8")) > 200
                or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in name)):
            raise MetadataError()
        # This JSON contains only the selected public redirect array, not Client.
        uris = json.loads(encoded_uris)
        if not isinstance(uris, list) or not 1 <= len(uris) <= 6:
            raise MetadataError()
        clean = []
        for uri in uris:
            if (not isinstance(uri, str) or len(uri.encode("utf-8")) > 4096
                    or any(ord(c) <= 32 or ord(c) == 127 for c in uri)):
                raise MetadataError()
            parsed = urllib.parse.urlsplit(uri)
            if (parsed.scheme not in ("http", "https") or not parsed.hostname
                    or parsed.username is not None or parsed.password is not None):
                raise MetadataError()
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


def inspect(path):
    path = Path(path).absolute()
    before = regular(path)
    # Do not follow a sidecar symlink or use immutable=1, which can omit live WAL.
    for suffix in ("-wal", "-shm"):
        sidecar = Path(str(path) + suffix)
        if os.path.lexists(sidecar):
            regular(sidecar)
    deadline = time.monotonic() + 3
    with contextlib.closing(sqlite3.connect(
            path.as_uri() + "?mode=ro", uri=True, timeout=2,
            isolation_level=None)) as connection:
        connection.execute("PRAGMA query_only=ON")
        connection.execute("PRAGMA trusted_schema=OFF")
        connection.execute("PRAGMA temp_store=MEMORY")
        connection.set_authorizer(authorizer)
        connection.set_progress_handler(
            lambda: int(time.monotonic() > deadline), 1000)
        rows = connection.execute(SQL, (int(time.time()),)).fetchall()
        after = regular(path)
        if (before.st_dev, before.st_ino) != (after.st_dev, after.st_ino):
            raise MetadataError()
        return project(rows)


def main(argv=None):
    args = sys.argv[1:] if argv is None else argv
    if len(args) != 2 or args[0] != "--state-db":
        result, code = {"error": "invalid_arguments"}, 2
    else:
        try:
            result, code = inspect(args[1]), 0
        except Exception:
            # Never print exceptions, paths, SQL parameters or partial rows.
            result, code = {"error": "client_metadata_unavailable"}, 2
    sys.stdout.write(json.dumps(result, ensure_ascii=True) + "\n")
    return code


if __name__ == "__main__":
    raise SystemExit(main())
