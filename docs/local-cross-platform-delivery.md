# Development-machine build and direct deployment

## Decision

GitHub Actions is not a build, verification, packaging, or deployment dependency. The repository workflow has been disabled and removed. Source commits may still be pushed to the Git remote; optional artifact mirroring is outside the critical path.

## Pipeline

1. Confirm the development worktree, toolchain, available disk, and existing build lease. Capture immutable declared inputs with `scripts/source_snapshot.py`.
2. On Mac Studio, invoke the snapshot's `scripts/release-code.py`. It uses the existing stable build slot, the pinned Rust toolchain, and independent step receipts.
3. Run the source checks locally. Build the three targets with:

   ```sh
   cargo build -p octendra-code --release --locked
   cargo zigbuild -p octendra-code --release --locked --target x86_64-unknown-linux-musl
   cargo xwin build -p octendra-code --release --locked --target x86_64-pc-windows-msvc
   ```

   The macOS builder must be arm64. Cross-compilation also needs the platform linker/SDK already installed on the development host; `--target` alone does not replace these.
4. Package and validate the local manifest, source receipt, sizes, and SHA-256 values. Archive the source commit, snapshot identity, toolchain, target, commands, and test counts with this bundle.
5. Transfer the same bundle directly with Octendra file tools or a previously authorized management route. Verify on receipt before extraction. The distribution source is the development machine, not a GitHub release URL.
6. Upgrade Gateway first, then Agents with the existing independent updaters. Preserve credentials, configuration, service identity, macOS signing, backup and rollback receipts.
7. Execute target-platform tests on owned machines. A Linux-only hang must be bounded and diagnosed on Linux, not relabeled as a cloud-only problem.
8. Accept each installed executable by hash and version, followed by real command success/failure, output consistency, file roundtrip, precise edits, idempotency, task recovery, and status checks.

## Daily test scope

For a small edit, select the affected package and unit tests or integration test
through the existing entry point. Keep release optimization and reuse the
established `~/rust-target` cache:

```sh
bash scripts/test.sh -p octendra-code --release --locked --lib token_output::tests::
bash scripts/test.sh -p octendra-code --release --locked --test oauth_clients
```

Choose the filter or integration target that covers the change; an empty test
selection is not a passing regression check. Include callers when changing a
shared library or protocol. With no package selection, `bash scripts/test.sh`
still runs the whole workspace. Arguments after `--` remain test-harness options.

The default `scripts/check-code-source.py` verification still runs formatting,
Clippy, Python tests, and workspace checking. Its Rust gate uses release mode
and a serial harness. Given a complete prior full-workspace receipt with an
intact test log, it runs all `octendra-code` library unit tests plus the core
integration, authorization, OAuth, file-transfer, and terminal regressions.
Changes relative to that full baseline add related domain or edited integration
targets. Dependency/manifest, shared dispatch, unknown-source, deleted-input,
or shared-fixture changes require the full workspace. Without a valid full
baseline, it also runs the full workspace. Fixed-commit inputs retain full tests.

The receipt lists the baseline digest, changed inputs, selected targets and
omitted integration programs. Omitted targets are not counted as executed or
passed. Packaging validates that the executed command matches this plan and that
the baseline evidence remains intact. Narrowed runs never become full baselines.
An explicit full pass is available through both existing entry points:

```sh
python3 scripts/check-code-source.py --report target/full-source-check.json --full-tests
python3 scripts/release-code.py --report target/full-release.json --full-tests
```

Observe an existing release operation instead of restarting the same request.
Manual package/filter checks are not release receipts. Producer builds default
to at most eight Cargo jobs; an explicit `CARGO_BUILD_JOBS` value is preserved.
This does not increase test-harness concurrency or change optimization settings.

The 2026-11-11 release run spent 36m39s compiling the Rust tests and about 7m
executing them (44m27s combined). With the same workstation's existing cache,
the affected plan selected six Rust test programs instead of 51: compilation
took 1m16s, the Rust gate 4m12s, and all verification gates 5m08s. It executed
594 Rust and 799 Python tests successfully; two Rust benchmarks and three Python
tests remained ignored/skipped. These timings include cache-state differences
and are not a cold-build guarantee. The following native release build reported
622 fresh artifacts, no recompiled artifact, and completed in 0.52s.

## Recovery

A running operation is observed by its original handle. A completed step is reused only when its inputs and artifacts are still verified. A failed node does not trigger rebuilds or restarts on already accepted nodes. Never overwrite an immutable released version.

The cloud workflow's historical results remain historical evidence. They are not an active gate and will not be polled or restarted by this delivery path. Local source and target checks replace them; tests are not removed to manufacture a successful release.

## Linux ARM64 software preparation

`linux` retains its existing Linux AMD64 meaning. The default selection remains
`macos linux windows`; Linux ARM64 is the explicit `linux-arm64` selection for
`scripts/release-code.py` and `scripts/package-code-release.py`. It builds both
`octendra-code` and the native `octendra` CLI for `aarch64-unknown-linux-musl` and
packages them as `octendra-code-linux-arm64` and `octendra-linux-arm64` in the
same registered fixed current group. It does not create another component,
output root or retained package generation. Omitted platforms remain pending.
The native macOS machine-contract producer is still required by formal packaging.

When the shared Cargo window is assigned, the minimal new-target build is:

```sh
cargo zigbuild -p octendra-code -p octendra-cli --release --locked --offline \
  --target aarch64-unknown-linux-musl
```

Use primary main and the established `~/rust-target`. The pinned toolchain must
first have the `aarch64-unknown-linux-musl` standard library installed; the local
2026-10-26 inspection found only the ARM64 GNU standard library, and no reusable
ARM64 Linux executable or ARM64 binary in the published current package.
No build, standard-library installation, packaging or deployment is implied by
the script fixtures. Current native binaries and runtime receipts remain intact.

Linux package validation checks 64-bit little-endian ELF `e_machine` (62 for
AMD64, 183 for ARM64). Fleet Agent selection requires an advertised architecture:
`aarch64`/`arm64` selects ARM64 and `x86_64`/`amd64` selects AMD64. A missing,
unsupported architecture or missing matching artifact fails before Agent bundle
export/staging; it never falls back to another architecture. The standalone fresh
installer also selects the host architecture without changing its permission,
identity, signing or activation requirements. The existing NAS SSH gateway
maintenance path remains the AMD64 gateway path; this change does not perform or
authorize a gateway migration.

Reachy Mini/RPi400 software transfer, enrollment, installation, network changes,
media/motion actions and ARM target execution acceptance are separate gates and
were not performed as part of this preparation.

## Current implementation boundary

The runtime and cross-built artifacts are Rust. The existing coordinator/updater helpers include Python and PowerShell. Their replacement by the native Rust release executor is a separate, testable migration, not a prerequisite to removing cloud builds now.
