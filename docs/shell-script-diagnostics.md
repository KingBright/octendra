# Shell script size rejection diagnostics

## Confirmed Forge request

The saved request at /Users/jinliang/Workspace/forge-collab/.rust-storage/forge-nas-prepare.request.json has SHA-256 f91fa855edccbcd1ede59a01d1be392f68e1ba266ed9cb711930bf75e88dc669. It calls remote_hosts_run_in_workspace with shell.posix and exactly one args element, explicit mutating coordination and six existing scopes. Its decoded script is nonempty, contains zero NUL bytes, and is 68,644 UTF-8 bytes (script SHA-256 cfe2cf9b3392bc8291e543089605221b5e72f7ea627ff3164a9e3721949ac48c).

The original error is argument <invalid-script> is not allowed for profile shell.posix, with no operation ID. The length exceeds the legacy 65,536-byte ceiling by 3,108 bytes. Python syntax is irrelevant to this string-structure validation; no Python interpreter or target shell is reached.

## Actual handling component

Forge tools/nas_release_transport.py reads its configured remote-hosts stdio MCP entry and spawns it for each request. The configured executable resolves to /Users/jinliang/.local/bin/remote-hosts, 32,806,832 bytes, SHA-256 2a88c68ce792f78137352df549955de684a7eee75e46d80406240921c580926f. Read-only inspection found the legacy 65536-byte error text and no 16 MiB text. Its CLI does not expose --version. An initialize-only handshake (zero tools called) reports serverInfo rmcp 2.1.0, protocol 2024-11-05: this is SDK identity, not a verified Remote Hosts application release or build commit.

Together, the matched original request, exact structural error and installed binary signature identify a legacy local MCP script-size rejection. Binary provenance cannot be assigned to a unique source commit merely from the error string. The NAS Code Gateway/Agent is not the component rejecting this request; do not label its unrelated wire version as the rejecting MCP version. No NAS command, rejected-script retry or alternative execution transport was used for this diagnosis.

## Minimal source change, no policy change or deployment

The current source already used 16 MiB before this change; the historical implementation used 64 KiB. Keep both deployed and source limits as they are. This change centralizes the existing source ceiling and shared structural validator, without increasing any limit, changing allowed commands, installing binaries or restarting anything.

- empty_shell_script: blank input.
- shell_script_contains_nul: NUL in input.
- script_too_large: actual_bytes, max_bytes and exact resolved script_sha256; no body or credentials in the diagnostic.

Catalog resolution and preconstructed profile validation share this check. Request resolution remains before idempotency lookup and operation publication. The original shell.posix caller shape is command_profile=shell.posix, args=[one complete script]; the resolver supplies sh -lc. An oversized script has no argument-reordering workaround under the deployed ceiling.

## Receipt identity and safe disposition

The fixed prepare .results.jsonl contains an older queued result and the current error. The old operation 01a14bcb-80b8-7d02-9f7f-8f14016d3c1d belongs to the old a3166236d065c82cfa99380f key; current local state names source c675924bf9ecc0485795a48c1f9f774137f0b444 and key forge-nas-current-399a92504f66a58bdde586d3-prepare, still submitting with no operation ID. The helper saves .response.json only on successful tool results, so an earlier response file is not evidence that the current request was accepted.

The diagnostic receipt binds both current request-file SHA and decoded script SHA. Forge may use that evidence through its owning workflow's original-request reconciliation; it must not adopt the old queued ID, overwrite historical receipts, delete the submitting journal or automatically resubmit. No Forge journal is changed by this work.

## Regression scope

Core tests cover deployed-legacy and existing-source boundary models, inclusive byte limits, UTF-8 byte counts, blank/NUL distinction, digest correlation and no plaintext leakage, and shared POSIX/PowerShell profile validation. The MCP regression queues only an isolated fixture operation, then verifies invalid requests using its same semantic key return structural errors rather than its receipt, and leave operation/output counts unchanged. No test executes the Forge script or contacts NAS.

Real-runtime fingerprint/read receipts and source test operation IDs are retained in target/dev-verify/shell-script-diagnostic.json. That local receipt is not a deployment or permission grant.
