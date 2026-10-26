# Octendra 0.12.1 platform acceptance — 2026-10-26

This is an evidence matrix, not a claim that every platform or the independent
new MCP connection has passed. Build success, target execution and live service
acceptance are separate gates.

The existing published desktop bundle is based on `fc778d0` (source identity
`018f7e2ac6ccdb296a9ee83eef3425f75ec0557f658b1a7735fd90734dfc6d01`). Its local
verification recorded 1,923 passed, zero failed, six Rust ignored benchmarks and
three Python skips. All three desktop builds and packaging exited zero. The
offline CLI change is `5a3326f`; reusable POSIX fixtures are `f30c964`. The new
CLI candidates below are test artifacts, not deployed replacements.

| Platform | Build / local checks | Actual execution evidence | Still unaccepted |
| --- | --- | --- | --- |
| macOS arm64 | Pinned Rust 1.99.1 native test build; seven Rust CLI tests and seven POSIX Python fixture tests passed | Mac Studio executed artificial match, mismatch, unsafe-file and redaction cases | New independent macOS Agent binding/deployment and real new-MCP read/edit/execute/transfer/recovery; this round did not upgrade Studio or Book |
| Linux x86_64 musl | Primary-main `cargo zigbuild` for the CLI exited zero with unchanged inputs | HO5 verified candidate SHA-256, started 0.12.1 and checked fixed argument-error output; operation `45872049-1798-4e1e-9073-eb094e29e5f2` exited zero | Full match/mismatch fixture is blocked by the original script upload's source-address policy; new-MCP business acceptance remains pending |
| Windows x86_64 MSVC | Primary-main `cargo xwin build` for the CLI exited zero with unchanged inputs | cube verified the exact candidate and passed four native startup/non-POSIX-error cases; operation `d9b8e24f-6f8c-4c5e-b2df-c552b63edfeb` exited zero | The verifier intentionally supports POSIX only; new-MCP read/edit/execute/transfer/recovery and GUI acceptance remain pending |
| Android | Current 0.12.1 Kotlin/Java debug compilation and 16 JVM tests passed, zero failed/skipped | Host JVM execution only; four JUnit XML receipts | Current signed release APK and dedicated-emulator/phone runtime acceptance, background stability, screen/action and new-origin connection; cached offline device metadata is not phone acceptance |
| Web status UI | Embedded JavaScript Node suite: five passed, zero failed/skipped | Node DOM/network fixtures only | Real browser compatibility, authenticated new-origin UI and reconnection behavior; browser/Gemini work remains with its original owner |

The current authenticated tool connection still reports Gateway 0.12.0. It was
used only as an explicitly identified transport/observer. Existing new Gateway,
HO5 and cube 0.12.1 runtime/readiness receipts remain valid historical evidence;
old-connection tests do not establish new-connection business acceptance.

## Repeatable focused checks

```sh
cargo test -p octendra-code --test owner_password_check_cli --locked --offline
python3 scripts/check-owner-password-fixture.py --binary /absolute/candidate/octendra-code
python3 scripts/test-android-unit.py
node --test crates/octendra-code/tests/status_live_test.cjs
```

The POSIX fixture accepts only a binary path and creates artificial files in its
own temporary directory. It never accepts a production config/password path.
Run permission-bit cases in the normal native environment: a restricted shell
was observed silently replacing `04600` with `0600`; that environment failure
is retained separately from the passing native receipt.

The Android unit entry prepares the existing debug-only public CA resource,
without changing the network-security XML. If no public fixture exists, OpenSSL
creates an artificial certificate and discards its private key to the null
device. No key is retained, signing environment variables are removed from the
child, and no APK is signed, installed or launched. Runtime TLS integration must
still use its existing dedicated fixture/key-pair preparation.

## Blockers and next steps

1. Original Linux script transfer `7c0d02ca-1f82-41f8-b07e-0ccec72bba80` is
   paused before HTTP with zero confirmed bytes. HO5 resolved the exact source
   host to `198.18.0.18` and `fc00::e`; the public-address guard correctly refused
   it. An authorized network owner must restore a legitimate public resolution.
   Observe and, only after that evidence, resume this same operation. Do not
   relax the guard, change source/path or re-upload under another key.
2. Select the already authorized independent new MCP connection, then run real
   read/edit/execute/transfer/recovery acceptance against its explicit device IDs.
   Do not create a new grant or count the old connection as a substitute.
3. Coordinate Android runtime and real-browser gates with their existing owners;
   no physical-device permission, enrollment or other-project emulator changes
   were made here.

Local evidence is under `.integration-artifacts/independent-chain/`, including
`owner-password-{linux,windows}-build.json`,
`owner-password-check-preparation-result.json`,
`android-jvm-unit-fixed-xml-20261026.json` and the status Node log. The original
Android session lost its execution environment before a unit receipt and has an
unknown exit code. Its next bounded run failed with a missing public debug CA;
the fixed run then passed all 16 tests. These are distinct retained attempts.

No production password/hash comparison, login, reset, grant, service deployment,
restart or access-policy change was performed in this round.
