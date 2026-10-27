# Read-only authorization boundary

The pre-fix source at `90539b7` contained reachable authorization gaps. A valid
owner-scoped MCP identity was required; this was not anonymous execution.
Ordinary account/device tool scopes were not enforced, queued jobs only checked
desktop observation scope, ordinary Agent execution ignored its write/exec
flags, and workspace creation/path aliases could choose directories beyond the
configured roots. Empty Agent roots also expanded to the filesystem root.

Enrollment still derived device scopes from `allow_write` and `allow_exec` and
configuration validation required roots. The gaps contradicted those existing
contracts; retiring per-task consent was not authorization to remove them.

## Enforcement

| Boundary | Required behavior |
| --- | --- |
| Gateway account | Every tool entry, including direct durable receipt recovery, checks its declared OAuth scopes before admission or cached response handling. Unknown tools fail closed. |
| Registered device | Device-scoped admission checks the selected registration. Queue selection and atomic claim recheck all required scopes, including previously dispatched jobs and both desktop worker scopes. |
| Local Agent | Ordinary write tools require `allow_write`; shell and terminal controls require `allow_exec`. No shell command becomes read-only merely because its text looks harmless. Typed desktop access retains its separate local approval/provider boundary and requires the Gateway account/device desktop scopes. |
| Code workspace | Roots must be explicit and nonempty. Creation, default/path aliases and existing workspace IDs must resolve within a configured canonical root. Existing roots are rechecked; file operations retain their directory-capability confinement. Workspace write/exec flags reflect the current account/device/local intersection. |
| Recovery and artifacts | Transfer controls and outcome reconciliation recheck authority for their target. Ordinary history remains observable with read authority or the original tool authority; desktop history always requires observation consent. Observation never renews execution authority. Device transfer endpoints and issued download links recheck current registered scopes. |

An explicitly authorized shell still has the existing local-user filesystem
authority outside code roots; this change does not describe shell execution as a
path sandbox. A read-only device must have no shell execution permission and no
desktop input registration. It can use only its authorized read/status interfaces.
No consent, permission, configured root or credential is widened automatically.

## Verification and delivery gate

Synthetic fixtures cover same-owner missing account scopes, read-only devices,
local disabled flags, independent positive write/exec cases, empty roots,
directory-prefix confusion, path/default aliases, old out-of-root workspace IDs,
symlink escape, moved roots, effective capabilities, recovery controls and cached
request receipts. The actual queue SQL is also exercised by
`scripts/tests/test_code_scope_dispatch.py`: 378 scope/state cases check both its
read candidate and atomic claim expressions without a production database.

The focused native commands are:

```sh
cargo test -p octendra-code --lib --test authorization_boundaries --locked --offline
cargo clippy -p octendra-code --lib --test authorization_boundaries --locked --offline -- -D warnings
python3 -B -m unittest discover -s scripts/tests -p test_code_scope_dispatch.py
```

This source repair does not update any installed runtime or make an old package
safe. No production bypass probe, credential read, Gateway deployment, robot
transfer, enrollment or installation was performed. ARM64 compilation and ELF
inspection remain a separate scheduled build; cross-compilation will not prove
target execution. Keep existing Reachy/RPi400 read-only restrictions in place
until an exact repaired artifact has the required validation and deployment
authorization.

## Follow-up review

The earlier canonical-path-only check left a validation/open race. Agent startup
now retains each explicitly configured directory handle. Every operation binds
its workspace below that handle with no-follow directory opens before resource
waits. Reads, edits, list/search traversal and binary transfer roots use the held
capability. Persisted workspace JSON never carries or restores a capability.
Workspace-local ignore rules remain available; global/parent ignore files are
not consulted beyond the authorized root.

Git cwd alone was insufficient: a subdirectory could discover its parent
repository, or a .git file could redirect to an outside gitdir/worktree. Review
now requires an ordinary in-root .git directory, rejects external Git metadata
and configuration indirection, explicitly selects the worktree/repository,
clears inherited Git overrides, and pins Unix cwd through its directory handle.
Root replacement and outside folder fixtures are synthetic. This is not a
sandbox for an independently hostile process continuously rewriting Git metadata
while review runs; that remaining concurrency boundary requires review before
claiming arbitrary concurrent-adversary confinement. Windows handle-pinned Git
review is currently fail-closed rather than silently using ambient cwd; restoring
compatible bounded Windows diff is still a delivery gap.

Old workspace-open responses intersect the latest *fresh* online Agent flags
with current account/device scopes. Offline/unreported local flags are unknown
and cannot advertise effective writes/exec. A cached local Agent response is
also clamped against its current local flags. Scope denial never expands grants.

New fixtures cover every file-transfer HTTP entry, current-scope checks against
original operations, paused upload recovery under a disabled local write flag,
and stale/changed Agent flags. Current Mac Studio verification passed 20 boundary,
462 library, 8 CLI, 5 file-transfer, 15 review and 36 recovery/sync tests (546
passes total), plus strict Clippy for library/binary/all test targets. One existing
Forge live-browser fixture remains explicitly ignored. The old 459/12 results
are retained as prior evidence, not substituted for current verification.
The queue test remains 378 distinct scope/state scenarios with two statements
per scenario, not 756 independent scenarios.

## Reachy-specific state-only candidate

Reachy status acceptance does not require Git. The explicit startup flag is
`octendra-code agent --config <existing-private-config> --state-only`. Existing
config format and ordinary Agent startup remain unchanged. This is software
preparation only, not permission to enroll, install or start any device.

State-only startup requires exactly one explicitly selected existing directory
of nonsecret published text status, `allow_write=false`, `allow_exec=false`, and
a disjoint private Agent runtime directory. The private config file must also
be outside the exposed root. No root is guessed from robot home or another
project, and no status publisher or device/media driver is added.

The local policy permits **only** workspace_open, code_list and code_read.
It rejects Git diff, search/symbol parsing, terminal history/execution/control,
file upload/download/sync and transfer recovery, desktop observe/input/frame,
unknown tools and hydrated payload envelopes before cached receipt lookup,
provider dispatch or transfer. Known path aliases are still checked against the
pinned configured directory. Text reads retain the existing regular-file,
UTF-8 and byte limits; binary exports and camera/microphone/robot actions have
no allowed tool path.

Restricted startup uses a dedicated runtime store. It will not adopt a store
with previous general-mode evidence. An internal policy marker preserves the
same mode/root on restart; omitting --state-only on that store fails closed.
This marker grants nothing and is not a new credential or access registration.
Desktop approvals are not loaded; upgrade notifications and automatic release
pruning do not execute in state-only mode. Local Agent journaling/logging is
still necessary, in its separate private directory.

Windows general-mode bounded Git compatibility and strong concurrent Git
metadata isolation remain recorded gaps for the general product. They do not
authorize enabling Git on Reachy and are not prerequisites to its explicit
Git-denial fixture. The existing Windows positive Git regression remains in the
test suite rather than being silently skipped. The current Rust policy, directory
race, Git refusal and CLI fixtures passed on Mac Studio. This is not Windows or
ARM target execution evidence. ARM cross-compilation and separate authorized
device runtime acceptance remain necessary before calling a robot candidate
installable.

## Current minimal ARM verification

The final single-program ARM64 musl build passed on Mac Studio with pinned Rust
1.99.1. Zig remains the cross C driver; the pinned Rust LLD final link explicitly
retains --fix-cortex-a53-843419. Eleven focused ARM build-script fixtures pass.
The ELF is AArch64/ELF64 little-endian, has no PT_INTERP and no DT_NEEDED entries.
Four existing Linux-only compile warnings remain recorded. This does not prove
ARM device execution or constitute a published/approved installation package.
Full receipts, immutable source identities, prior failed attempts and artifact
SHA-256 remain under .integration-artifacts/independent-chain/.

## State-only startup Skill side effects (P2)

State-only startup now skips both global Skill synchronization and installed
Skill inspection. It leaves .codex and .gemini client directories untouched and
reports no installed global Skill revision/consistency. General-mode behavior
is unchanged. Three synthetic startup tests use explicit temporary homes to
prove no creation, preservation of existing bytes, and normal-mode compatibility.

After this fix, 20 boundary tests and focused Clippy passed again, followed by a
new ARM64 musl candidate with preserved A53 mitigation and checked ELF/SHA. The
full library was intentionally not rerun; earlier broad results remain earlier
evidence. Two separate general-mode Git clean/process filter security regressions
are retained with explicit known-risk ignore reasons; they were not executed or
counted as passing. State-only refuses Git before either path. Windows general
Git compatibility also remains a recorded functional gap.

The parent confirms the user's Reachy read-only installation approval at 10:05.
That approval is preserved. This source-validation operation did not install,
enroll, transfer to or run anything on Reachy. Source commit/push approval remains
pending and the previous automatic refusal was not retried. Preserve the explicit
--state-only startup flag in the approved installation flow. The current receipt
is readonly-state-only-p2-final-acceptance.json and supersedes the earlier ARM
candidate fingerprint.

## General-mode Git filter repair (focused Mac verification complete)

Synthetic repositories reproduced clean and process helper execution through the
old unstaged git diff command despite --no-ext-diff and --no-textconv. The new
candidate never asks Git to convert worktree contents: ls-files --stage enumerates
index objects, cat-file --batch reads raw blobs, held directory capabilities read
raw current bytes, and Rust renders the comparison. Staged diff retains the
existing no-ext-diff/no-textconv/ignore-submodules behavior. Snapshot input is
bounded to 64 MiB, rendered output to 16 MiB; over-capacity and unmerged indexes
fail clearly and require narrower paths or bounded file reads. Unix special
files are not opened, final symlinks are not followed, and native rendering has
a bounded comparison deadline in a blocking worker.

The unstaged view deliberately compares raw bytes: LFS/clean conversions and
line-ending transforms do not run. Binary versions include raw content hashes;
deletions and executable/symlink mode changes are represented. This changes the
unstaged rendering contract explicitly, without widening tool permissions.

Two native Git script regressions use the actual Rust plumbing argv and verify
non-execution even when a driver is introduced after index enumeration. They
pass. Seven compiled Rust review regressions, including the formerly deferred
clean/process cases, now pass without ignores; 20 authorization boundaries and
focused Clippy also pass. The symlink fixture uses read_link_contents solely to
inspect link text, never to follow an outside target. Verification used primary
main, unchanged input identity and the existing nonblocking shared/project lease.
This is focused Mac verification, not an all-platform or deployed-runtime claim.
Broader concurrent Git metadata/object-store isolation and Windows general Git
compatibility remain separate unverified gaps. The validated state-only denial
path is unchanged, and the prior P2 ARM artifact is not replaced by this work.

The final Git filter receipt is general-git-filter-final-verification.json. An
initial 6-pass/1-fail link-text compatibility result is preserved, followed by the
successful focused rerun. Both leases were released. No full-library repeat,
ARM rebuild, source commit/push or production deployment occurred. The P2 ARM
artifact retains its original SHA and must not be represented as containing this
later general-mode repair. Reachy read-only installation approval remains valid.
