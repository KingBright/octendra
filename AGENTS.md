# Repository delivery rules

## Build and deployment authority

- Build, test, package, and deploy on the owner's authorized development machines. Mac Studio is the primary release builder; build in the primary main directory and reuse the established `~/rust-target`; never create a source snapshot, slot checkout, or another Cargo target. Real build/test/acceptance/package runs only in that main tree; packaging may copy only verified final products.
- Do not add or enable GitHub Actions workflows for builds, tests, release gates, or deployments. Do not wait for GitHub check-runs. Git hosting is source storage only; optional release mirroring must never block delivery.
- Use the pinned local Rust toolchain and Cargo to produce macOS arm64, Linux x86_64 musl, and Windows x86_64 MSVC artifacts. Use the existing cargo-zigbuild/cargo-xwin linkers and SDKs. A successful cross-build is not target-platform execution evidence.
- Verify one unchanged fingerprint of the primary main inputs (an existing snapshot may be checked as read-only evidence), preserve test and build receipts, then transfer its verified bundle directly through the authorized Remote Hosts or configured management channel. Do not download deployment input from GitHub Releases.
- Develop only in the actual primary main directory; do not create task checkouts or linked worktrees. Preserve unrelated worktree changes. Stage explicit task files only.
- Upgrade Gateway first, then Agents using existing maintenance leases, platform signing, exact SHA-256 validation, health observations, and rollback paths. Never bypass local macOS signing or device authorization.
- Do not restart other projects or kill unrelated compiler processes. Cancel only operations whose ownership is confirmed.
- Run bounded regression tests on the appropriate owned target machine. Investigate hangs locally; disabling cloud CI does not authorize skipping the equivalent tests.
- Keep existing published version directories and their evidence intact. Only the registered fixed current group may be atomically replaced through the shared lifecycle transaction. A retry preserves its original request/operation and evidence. Ambiguous execution requires observation, not replay.
- Completion means deployed runtime identity plus real read/edit/execute/transfer/recovery acceptance. Report unknown, skipped, failed, and passed separately. Keep full evidence locally; return compact summaries.

## Existing entry points

- Agents invoke `bash scripts/test.sh` for the existing `cargo test --workspace` command in primary main, reusing the configured cache.
- `scripts/source_snapshot.py`: read-only input identity and legacy evidence checks; new source snapshots and checkout synchronization are retired.
- `scripts/release-code.py`: local verification and three-platform Cargo cross-builds in primary main under the existing metadata-only lease; no cloud runner and no deployment.
- `scripts/fleet-upgrade.py`: verified local bundle distribution and per-platform updaters. Use only a verified local package and explicit deployment configuration.
- `scripts/check-code-gateway.py`: live acceptance, with authoritative results rather than presentation-only compaction.

The current release coordinator still contains transitional Python helpers. Rust produces the executables; do not describe the whole coordinator as Python-free until those helpers are actually replaced and tested.

Formal packaging uses `scripts/release-code.py` and `release_current.py` with the existing `~/.codex/skills/rust-project-lifecycle/SKILL.md` and `references/operations.md`: one fixed `dist/remote-hosts-code-current` platform group plus its current bundle, necessary incremental cache only. The public current transaction alone reserves temporary sibling stages, validates and replaces current, and recovers owned failed/interrupted stages once activity has ended; no generation/task package directories or retained rollback copies. Cleanup is delegated to the shared lifecycle helper and its sole `managed_release_retention.py` adapter; active paths and unknown historical output remain protected.

The Windows CLI/launcher entry `scripts/build-windows-cross.sh` is a separate product component: it uses `build_windows_current.py` and the same public current transaction for fixed `dist/remote-hosts-windows-x86_64-current`, its ZIP and checksum. Reuse primary main and `~/rust-target`; alternate output roots and version/commit package directories are refused. Original historical Windows bundles remain protected.
