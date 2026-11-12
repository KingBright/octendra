# Repository delivery rules

## Daily delivery loop

- Use `main` and the single existing primary worktree. Do not create feature branches, additional checkouts or linked worktrees, and do not use merge-based delivery.
- Make the smallest correct change, run the relevant bounded checks, commit the explicit source files, push `main`, and verify the remote contains the commit. Do not leave a completed code iteration uncommitted.
- Keep upgrades direct: reuse the existing builder and installer, preserve required platform signing and device authorization, verify the delivered hash and live behavior, and fix issues on `main`. Do not add release gates, staging branches or rollback machinery without a demonstrated requirement.
- Default to `bash scripts/ship.sh`: incremental build/package, publish Gateway, let Agents update on their next poll, and observe versions once. Full tests, clippy, manual per-device installation and full acceptance matrices are not default release gates. Use `--full-checks` when explicitly requested or when a concrete failure warrants it; fix problems on main and ship again.
- Automatic upgrades must not put the device into a global drain or suspend task polling. If real commands are running, defer only that device's cutover until a later poll; stale history/receipt rows must not block it. An unfinished watchdog blocks only another upgrade attempt, never normal work.
- For the current brand migration, retain the usable old instances until their data and device connections have been accepted on Octendra, then retire them as requested. This temporary fallback does not require a separate rollback project.

## Build and deployment authority

- Build, test, package, and deploy on the owner's authorized development machines. Mac Studio is the primary release builder; default builds use primary main and the established `~/rust-target`. The explicit `release-code.py --source-commit <full SHA>` entry alone may materialize that exact Git tree in the producer-owned fixed `target/release-slot/source` directory. Verification, compilation and packaging use that one bound input root; publication authority stays in primary main, source/producer identities are recorded separately, and the owned input is checked and removed when the original operation ends. Preserve primary worktree/index/WIP. Do not create other source snapshots, worktrees, arbitrary execution roots or Cargo targets; packaging may copy only verified final products.
- Do not add or enable GitHub Actions workflows for builds, tests, release gates, or deployments. Do not wait for GitHub check-runs. Git hosting is source storage only; optional release mirroring must never block delivery.
- Use the pinned local Rust toolchain and Cargo to produce macOS arm64, Linux x86_64 musl, and Windows x86_64 MSVC artifacts. Use the existing cargo-zigbuild/cargo-xwin linkers and SDKs. A successful cross-build is not target-platform execution evidence.
- Verify one unchanged fingerprint of the primary main inputs (an existing snapshot may be checked as read-only evidence), preserve test and build receipts, then transfer its verified bundle directly through the authorized Octendra or configured management channel. Do not download deployment input from GitHub Releases.
- Develop only in the actual primary main directory; do not create task checkouts or linked worktrees. Preserve unrelated worktree changes. Stage explicit task files only.
- Upgrade Gateway first, then Agents with platform signing, exact SHA-256 validation and health observations. Explicit maintenance leases remain available for targeted manual recovery; automatic updates do not acquire a global drain. Fix failures directly; do not introduce additional rollback paths. Never bypass local macOS signing or device authorization.
- Prefer the existing native automatic Agent updater, including its persistent macOS signing identity. Use `scripts/launch-code-upgrade.py --start` only for targeted macOS recovery. Never use `launchctl submit` or a repeating installer: submit jobs can retain KeepAlive and reinstall obsolete candidates. Observe the original result before another launch.
- Do not restart other projects or kill unrelated compiler processes. Cancel only operations whose ownership is confirmed.
- Run bounded regression checks for the code being changed. Full workspace tests remain available on demand; they do not block every fast release. Record skipped checks as not run, never passed.
- Keep existing published version directories and their evidence intact. Only the registered fixed current group may be atomically replaced through the shared lifecycle transaction. A retry preserves its original request/operation and evidence. Ambiguous execution requires observation, not replay.
- Default completion means observed Gateway/Agent runtime identity and health. Run read/edit/execute/transfer/recovery checks only for the affected behavior or a concrete failure. Report offline/deferred devices and unknown, skipped, failed, and passed separately. Keep full evidence locally; return compact summaries.

## Existing entry points

- Agents invoke `bash scripts/test.sh` for the existing `cargo test --workspace` command in primary main or the explicitly bound fixed-commit input above, reusing the configured cache.
- `scripts/source_snapshot.py`: read-only input identity and legacy evidence checks; its general source-copy APIs and checkout synchronization remain retired. Explicit fixed-commit input ownership belongs only to `pinned_source.py` under the original producer lease.
- `scripts/ship.sh`: default one-command incremental build and direct Gateway publication; existing native Agents converge automatically. Private destination settings live in `~/.config/octendra/ship.json` or existing `OCTENDRA_GATEWAY_*` variables. `--full-checks` opts into the full suite; `--build-only` skips publication.
- `scripts/release-code.py --quick`: local incremental Cargo builds and packaging with an honest build-only receipt. The existing full verification mode remains available; no cloud runner and no deployment.
- `scripts/fleet-upgrade.py`: verified local bundle distribution and per-platform updaters. Use only a verified local package and explicit deployment configuration.
- `scripts/launch-code-upgrade.py`: macOS immutable request preparation and explicit one-shot launch through the stable dispatcher; KeepAlive and automatic triggers are disabled.
- `scripts/check-code-gateway.py`: live acceptance, with authoritative results rather than presentation-only compaction.

The current release coordinator still contains transitional Python helpers. Rust produces the executables; do not describe the whole coordinator as Python-free until those helpers are actually replaced and tested.

Formal packaging uses `scripts/release-code.py` and `release_current.py` with the existing `~/.codex/skills/rust-project-lifecycle/SKILL.md` and `references/operations.md`: one fixed `dist/octendra-code-current` platform group plus its current bundle, necessary incremental cache only. The public current transaction alone reserves temporary sibling stages, validates and replaces current, and recovers owned failed/interrupted stages once activity has ended; no generation/task package directories or retained rollback copies. Cleanup is delegated to the shared lifecycle helper and its sole `managed_release_retention.py` adapter; active paths and unknown historical output remain protected.

The Windows CLI/launcher entry `scripts/build-windows-cross.sh` is a separate product component: it uses `build_windows_current.py` and the same public current transaction for fixed `dist/octendra-windows-x86_64-current`, its ZIP and checksum. Reuse primary main and `~/rust-target`; alternate output roots and version/commit package directories are refused. Original historical Windows bundles remain protected.
