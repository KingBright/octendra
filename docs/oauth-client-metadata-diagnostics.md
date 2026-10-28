# Existing OAuth client metadata, offline

The native product command is:

```sh
octendra-code inspect-oauth-clients --state-db /absolute/path/to/state.sqlite
```

It uses the product's locked SQLite dependency and a new read-only connection,
without `Store::open`, schema initialization/migration, configuration reads or a
service lock. It runs before logging setup and suppresses argument-parser details.
Success prints one JSON object with the four public fields below, redaction count
and SQLite version; failure exits 2 with only a fixed `error` and `stage`. There is
no HTTP/MCP endpoint and no authentication or permission change. Deploy it only
as the verified normal product executable through the existing release/upgrade
flow; do not install an alternate executable or library under a diagnostic path.

The native command checks the existing ordinary four-column KV table, sets
`query_only=ON`, `trusted_schema=OFF` and memory-only temporary storage, and installs
a projection-only SQL authorizer and bounded query progress handler. It never
returns complete stored client JSON, sessions, secret hashes or tokens to Rust.
SQLite still parses the shared JSON column internally, as explained below.

The older Python preparation command remains:

`scripts/inspect-oauth-client-metadata.py --state-db /absolute/path/to/state.sqlite`
inspects existing Code Gateway registrations using the caller's existing filesystem
access. It does not open gateway configuration, start a server, authenticate, register
or revoke clients. Do not use `register-oauth-client` for this investigation.

The only client fields returned are `client_name`, public `client_id`, `redirect_uris`
and `token_endpoint_auth_method`. Query strings and fragments are omitted from redirect
URIs and their count is reported. A nonzero redaction count means the returned URI is
not sufficient for an exact callback comparison. Names are client supplied; a matching
name or callback alone does not identify the currently failing ChatGPT application.
An empty result does not establish the cause of a connection failure.

The projection follows `auth.rs::oauth_state_kind`, the public fields of
`auth/clients.rs::Client`, the registration capacity, and expiry semantics. It does not
reimplement authentication or claim that a client's credentials are valid. Registration
time is not stored on existing client rows and cannot be reconstructed from database
mtime or permanent expiry.

SQLite opens the specified existing database with `mode=ro` and `query_only=ON`.
The fixed SELECT projects JSON fields only from live `client`/`oauth2_client` rows;
complete JSON objects and secret fields never reach Python. An SQL authorizer rejects
other tables, writes, and unapproved functions. Results and errors are bounded. Failures
exit 2 with a fixed error code, stage, safe exception class and completed public checks;
they never include exception text, SQL operands, paths or partial client rows. Stages
distinguish `path_stat`, `sqlite_connect`, `sqlite_readonly_setup`,
`sqlite_json_capability`, `known_schema`, `public_projection`, `path_recheck` and
`public_metadata`. Checks include file existence/regularity/size, SQLite version,
JSON-function availability and the known four-column KV schema. For example,
`sqlite_unsafe_schema_function` identifies a trusted-schema/function incompatibility
without disabling protection; `state_path_missing` never triggers another-path search.

Public metadata and the secret hash share a JSON column. SQLite therefore internally
reads/parses that column and its database pages. This is not a promise that no
secret-bearing bytes enter SQLite internals. If that stronger restriction is required,
do not execute this diagnostic against the current storage format. No secret field is
selected, deserialized into Python, printed or copied. SQLite's normal live-WAL reader
may update shared-memory read marks; the script does not write the main database or
WAL. It deliberately does not use `immutable=1`, checkpoint, copy the database, delete
sidecars, or change permissions to obtain access.

The original NAS owner should stage the public script in an owned temporary diagnostic
directory, verify its supplied SHA-256, and execute it once against the exact approved
new-instance state path. Return only its JSON and exit status through the existing
maintenance channel. Keep the existing Gateway and OAuth state running unchanged.
On an error, report the fixed error; do not fall back to a raw dump or create a client.

## Temporary runtime compatibility

SQLite 3.40 mislabels built-in JSON functions as unsafe in schemas. With the
existing CHECK/expression-index schema, `trusted_schema=OFF` can reject the schema
before this diagnostic reads any client. Do not disable the guard, preload the
schema under a different trust setting, or alter the production database to pass.

A temporary compatible library may be used only for the diagnostic process and
its synthetic-test children. Its source must match the repository's locked
SQLite dependency and the official SQLite source hash. Before using `LD_PRELOAD`,
the original host owner must verify the artifact hash in an owner-controlled
temporary directory. Never export the variable globally, run `ldconfig`, replace
a system library, restart a service or copy production state off the host.

Verify the loaded version/source ID with an in-memory connection, then run the
synthetic suite under that same interpreter/library. Only after those pass should
the unchanged diagnostic inspect the already verified state path. The original
read-only/query-only/trusted-schema/authorizer/output guards remain active. An ELF
cross-build and a local test pass do not prove that the NAS loader/ABI works;
the host's synthetic preflight is mandatory before a production read. Remove only
the exact owned temporary library/scripts after all diagnostic processes exit,
keeping the non-sensitive receipt. The system Python and Gateway remain unchanged.

This is not an installation mechanism or permission to select another executable
directory. If the host rejects loading because the staging mount is `noexec`, stop
the temporary-library path. Do not relocate it, change mount options or permissions,
or repurpose an installed product's `bin` directory to bypass that restriction.
Only an existing supported diagnostic-runtime deployment mechanism could supply a
different approved input location; the current Gateway/Agent and restricted admin
helper installers do not provide one.

Synthetic verification, without production state or a Rust build:

```sh
python3 -m unittest discover -s scripts/tests -p test_oauth_client_metadata.py -v
```
