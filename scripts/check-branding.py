#!/usr/bin/env python3
"""Check active product names while preserving immutable historical evidence."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
OBSOLETE = re.compile(r"remote[-_ ]?(?:hosts|play)|\brh[_-][a-z]", re.IGNORECASE)
HISTORY = re.compile(
    r"^(?:acceptance/|migrations/|docs/(?:releases/|iterations/|"
    r"(?:android-|chatgpt-file-transfer-)?release-\d|iteration-|"
    r"[^/]*(?:review-\d|acceptance(?:[-.]|$))))"
)
HISTORICAL_PLANS = {
    'acceptance/task-continuation-0.10.26-01/README.md',
    'docs/automatic-history-retention-20260924.md',
    'docs/connectivity-heartbeat-20260924.md',
    'docs/experience-0.11.0-code-read-validation-20261015.md',
    'docs/gateway-release-entry.md',
    'docs/independent-assessment-20261016.md',
    'docs/mainline-android-transfer-20261005.md',
    'docs/observation-persistence-0.10.16.md',
    'docs/octendra-android-preparation.md',
    'docs/octendra-migration.md',
    'docs/product/DESKTOP-REVIEW-2026-09-22.md',
    'docs/product/ROADMAP-0.5.0.md',
    'docs/product/roadmap-0.5.0-evidence.json',
    'docs/shell-script-diagnostics.md',
}
LOCAL_EVIDENCE = (".integration-artifacts/", ".rust-storage/")
# Exact reviewed lines only: the fresh installer's rejected namespace set and
# an immutable-byte legacy fixture. No presentation, file or substring exemption.
LEGACY_NAMESPACE_GUARDS = {
    'scripts/parallel-install.py': {'78326bf9f9da3fb3988ac1d292423767ae981357aeb9761bb7414d2be9e3373a'},
    'scripts/tests/test_parallel_install.py': {
        '29ce43e425a7093dfe010f6963bcd19018b173afb460e267c91fcaf0c313ca27',
    },
}


def historical(name):
    return bool(HISTORY.search(name)) or name in HISTORICAL_PLANS


def inventory(root):
    names = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root,
    ).decode().split("\0")
    return sorted(
        name for name in set(names)
        if name and not name.startswith(LOCAL_EVIDENCE) and (root / name).is_file()
    )


def inspect(root):
    violations, history, scanned = [], [], 0
    for name in inventory(root):
        path = root / name
        if historical(name):
            history.append({"path": name, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
            continue
        scanned += 1
        if OBSOLETE.search(name):
            violations.append({"path": name, "kind": "filename"})
        try:
            text = path.read_text()
        except UnicodeError:
            continue
        for number, line in enumerate(text.splitlines(), 1):
            if OBSOLETE.search(line):
                if hashlib.sha256(line.encode()).hexdigest() in LEGACY_NAMESPACE_GUARDS.get(name, set()):
                    continue
                # Only locations are emitted: a scanner never echoes a secret
                # simply because it happens to contain a deprecated word.
                violations.append({"path": name, "line": number, "kind": "content"})
    return {
        "passed": not violations,
        "scope": "active tracked and new source, package inputs and current documentation",
        "scanned_files": scanned,
        "violations": violations,
        "preserved_history": history,
        "runtime_deployment_checked": False,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    result = inspect(ROOT)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key != "preserved_history"}))
    raise SystemExit(0 if result["passed"] else 1)


if __name__ == "__main__":
    main()
