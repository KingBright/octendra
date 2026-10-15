#!/usr/bin/env python3
"""Platform-scoped release wrapper delegating to canonical release-code.py and release_current.

Retains `updater_helpers(platform)` for helper manifest compatibility while routing
all platform-scoped verification and packaging through the single `release_current`
fixed-current transaction and shared `build_slot` lease in `release-code.py`.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import release_current
import source_snapshot


def updater_helpers(platform):
    helpers = [
        "upgrade-code-agent.py" if platform == "macos" else "upgrade-code-agent-linux.py",
        "agent_upgrade_support.py",
        "maintenance_client.py",
        "macos_code_identity.py",
    ]
    if platform == "macos":
        helpers += ["launch-code-upgrade.py", "code_upgrade_runner.py"]
    return helpers


def load_canonical_runner():
    spec = importlib.util.spec_from_file_location("release_code_canonical", ROOT / "scripts" / "release-code.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", "--snapshot", dest="source_root", default=str(ROOT))
    parser.add_argument("--snapshot-id", help="expected in-place source fingerprint; required for --apply")
    parser.add_argument("--report", type=Path, help="durable report; an existing attempt is observation-only")
    parser.add_argument("--version", required=True)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--platform", choices=list(release_current.PLATFORMS), default="linux")
    parser.add_argument("--verify-only", action="store_true", help="run fixed checks without packaging or deployment")
    parser.add_argument("--verification-scope", choices=["all"], default="all")
    args = parser.parse_args()

    if not re.fullmatch(r"[0-9]+[.][0-9]+[.][0-9]+", args.version):
        parser.error("invalid semantic version")
    source = Path(args.source_root).resolve()
    if source != ROOT:
        parser.error("build in the actual primary checkout")
    release_current.primary(source)
    cargo_version = tomllib.loads((source / "crates/remote-hosts-code/Cargo.toml").read_text())["package"]["version"]
    if cargo_version != args.version:
        parser.error(f"source version mismatch: Cargo.toml has {cargo_version}, requested {args.version}")

    manifest = release_current.snapshot_evidence(None, source)
    if args.apply and args.snapshot_id != manifest["snapshot_id"]:
        parser.error("source fingerprint required or changed; preflight again")
    if args.snapshot_id and args.snapshot_id != manifest["snapshot_id"]:
        parser.error("source fingerprint changed")

    report = (
        args.report or source / ".integration-artifacts" / f"scoped-release-{args.version}-{args.platform}.json"
    ).resolve()
    target = release_current.established_target(source)
    current_dir, _ = release_current.files(source)

    if not args.apply:
        print(
            json.dumps(
                {
                    "state": "preflight_passed",
                    "version": args.version,
                    "snapshot_id": manifest["snapshot_id"],
                    "platform": args.platform,
                    "source_root": str(source),
                    "source_copied": False,
                    "cargo_target_dir": str(target),
                    "output": str(current_dir),
                    "report": str(report),
                    "free_gib": round(shutil.disk_usage(target).free / 2**30, 2),
                }
            ),
            flush=True,
        )
        return

    runner = load_canonical_runner()
    state = runner.run_pipeline(
        None,
        report,
        source / "target/release-slot",
        verify_only=args.verify_only,
        expected_source_id=manifest["snapshot_id"],
        platforms=[args.platform],
    )
    if state.get("state") != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
