#!/usr/bin/env python3
"""Local developer verification and isolated smoke-test runner.

Runs static contract/boundary checks, isolated binary smoke tests (init-gateway,
enroll owner/mode preservation, check, adapter-contract), and incremental or full
Cargo verification without touching production credentials or services.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib
import unittest

ROOT = Path(__file__).resolve().parents[1]


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def find_toolchain():
    env = dict(os.environ)
    candidates = []
    if env.get("CARGO_HOME"):
        candidates.append(Path(env["CARGO_HOME"]) / "bin")
    real_home = Path(os.path.expanduser("~"))
    candidates.append(real_home / ".cargo" / "bin")
    pw_home = Path("/Users") / os.environ.get("USER", "")
    if pw_home.name:
        candidates.append(pw_home / ".cargo" / "bin")
    candidates.extend([Path("/opt/homebrew/bin"), Path("/usr/local/bin")])

    blocked = []
    usable = []
    for directory in candidates:
        try:
            if directory.is_dir() and (directory / "cargo").is_file():
                usable.append(str(directory))
        except PermissionError:
            blocked.append(str(directory))
        except OSError:
            continue

    if usable:
        env["PATH"] = ":".join(usable + [env.get("PATH", "")])
    cargo = shutil.which("cargo", path=env.get("PATH"))
    rustc = shutil.which("rustc", path=env.get("PATH"))
    return {
        "available": bool(cargo and rustc),
        "cargo": cargo,
        "rustc": rustc,
        "blocked_paths": blocked,
        "env": env,
    }


def verify_static_contracts(root: Path):
    cargo_toml = tomllib.loads((root / "crates/remote-hosts-code/Cargo.toml").read_text())
    version = cargo_toml["package"]["version"]
    contract_path = root / "crates/remote-hosts-code/adapter-contract.json"
    contract = json.loads(contract_path.read_text())

    if contract.get("version") != version:
        raise ValueError(f"adapter-contract version {contract.get('version')} != Cargo.toml {version}")

    tools_bytes = json.dumps(contract["tools"], separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    computed_sha = sha256_bytes(tools_bytes)
    if contract.get("server_tools_sha256") != computed_sha or contract.get("adapter_tools_sha256") != computed_sha:
        raise ValueError("adapter-contract tools SHA-256 mismatch")

    missing_schemes = [
        t["name"]
        for t in contract["tools"]
        if not isinstance(t.get("_meta", {}).get("securitySchemes"), list)
    ]
    if missing_schemes:
        raise ValueError(f"tools missing _meta.securitySchemes: {missing_schemes}")

    skill_dir = root / "skills/remote-hosts-agent"
    if not (skill_dir / "SKILL.md").is_file():
        raise ValueError("missing skills/remote-hosts-agent/SKILL.md")

    for crate in ("remote-hosts-code", "remote-hosts-mcp", "remote-hosts-api", "remote-hosts-cli"):
        build_rs = root / "crates" / crate / "build.rs"
        if not build_rs.is_file() or "/usr/lib/swift" not in build_rs.read_text():
            raise ValueError(f"missing macOS Swift Concurrency rpath in {build_rs.relative_to(root)}")

    return {
        "version": version,
        "tool_count": len(contract["tools"]),
        "tools_sha256": computed_sha,
        "skill_revision": contract["skill_revision"],
    }


def run_python_fast_tests(root: Path):
    sys.path.insert(0, str(root / "scripts"))
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    for test_file in (
        "test_public_repository_boundaries.py",
        "test_release_gate_final.py",
        "test_acceptance_scope_032.py",
        "test_admin_helper_preparation.py",
    ):
        path = root / "scripts/tests" / test_file
        if path.is_file():
            suite.addTests(loader.discover(str(path.parent), pattern=test_file))
    runner = unittest.TextTestRunner(verbosity=0)
    result = runner.run(suite)
    if not result.wasSuccessful():
        raise RuntimeError(f"fast python unit tests failed: failures={len(result.failures)} errors={len(result.errors)}")
    return {"ran": result.testsRun, "passed": result.testsRun}


def run_binary_smoke(binary: Path, root: Path):
    with tempfile.TemporaryDirectory() as tmp:
        tmp_path = Path(tmp)
        gw_config = tmp_path / "gateway.json"
        pw_file = tmp_path / "gateway.password"
        gw_state = tmp_path / "gateway-state"
        ag_config = tmp_path / "agent.json"
        ag_state = tmp_path / "agent-state"
        ws_root = tmp_path / "workspace"
        ws_root.mkdir()

        subprocess.run(
            [
                str(binary),
                "init-gateway",
                "--config",
                str(gw_config),
                "--public-url",
                "https://fixture.example",
                "--bind",
                "127.0.0.1:18787",
                "--state-dir",
                str(gw_state),
                "--owner",
                "dev-fixture",
                "--password-file",
                str(pw_file),
            ],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        )
        before_stat = gw_config.stat()
        subprocess.run(
            [
                str(binary),
                "enroll",
                "--gateway-config",
                str(gw_config),
                "--agent-config",
                str(ag_config),
                "--name",
                "smoke-device",
                "--state-dir",
                str(ag_state),
                "--root",
                str(ws_root),
                "--allow-write",
                "--allow-exec",
            ],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        )
        after_stat = gw_config.stat()
        if stat.S_IMODE(after_stat.st_mode) != 0o600:
            raise RuntimeError(f"gateway.json mode changed to {oct(stat.S_IMODE(after_stat.st_mode))}")
        if (after_stat.st_uid, after_stat.st_gid) != (before_stat.st_uid, before_stat.st_gid):
            raise RuntimeError("gateway.json uid/gid changed across enroll")

        subprocess.run([str(binary), "check", "--config", str(gw_config)], cwd=root, check=True, capture_output=True, text=True)
        subprocess.run([str(binary), "check", "--config", str(ag_config), "--agent"], cwd=root, check=True, capture_output=True, text=True)
        return {
            "binary": str(binary.relative_to(root)) if binary.is_relative_to(root) else str(binary),
            "gateway_mode": oct(stat.S_IMODE(after_stat.st_mode)),
            "owner_preserved": True,
        }


def run_cargo_checks(root: Path, env: dict, mode: str):
    if mode == "full":
        report = root / "target" / "dev-verify" / f"proof-{os.getpid()}.json"
        report.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(
            [sys.executable, str(root / "scripts/check-code-source.py"), "--report", str(report)],
            cwd=root,
            env=env,
            check=True,
        )
        return {"mode": "full", "report": str(report)}

    commands = [
        ["cargo", "fmt", "-p", "remote-hosts-code", "-p", "remote-hosts-release", "--", "--check"],
        ["cargo", "clippy", "-p", "remote-hosts-code", "-p", "remote-hosts-mcp", "--all-targets", "--locked", "--", "-D", "warnings"],
        ["cargo", "test", "-p", "remote-hosts-mcp", "--lib", "--locked"],
        [
            "cargo",
            "test",
            "-p",
            "remote-hosts-code",
            "--lib",
            "--test",
            "review_regressions",
            "--test",
            "reliability_0102",
            "--test",
            "experience_contract",
            "--locked",
            "--",
            "--test-threads=1",
        ],
    ]
    for argv in commands:
        subprocess.run(argv, cwd=root, env=env, check=True)
    return {"mode": "quick", "commands_completed": len(commands)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=["auto", "static", "quick", "full"], default="auto")
    parser.add_argument("--binary", type=Path, help="optional compiled remote-hosts-code binary for isolated CLI smoke tests")
    args = parser.parse_args()

    summary = {"static_contract": verify_static_contracts(ROOT), "python_fast_tests": run_python_fast_tests(ROOT)}
    toolchain = find_toolchain()
    summary["toolchain"] = {
        "available": toolchain["available"],
        "cargo": toolchain["cargo"],
        "rustc": toolchain["rustc"],
        "blocked_paths": toolchain["blocked_paths"],
    }

    binary = args.binary
    if binary and binary.is_file() and os.access(binary, os.X_OK):
        summary["binary_smoke"] = run_binary_smoke(binary.resolve(), ROOT)

    if args.mode in ("quick", "full") or (args.mode == "auto" and toolchain["available"]):
        if not toolchain["available"]:
            summary["cargo_verification"] = {
                "state": "skipped_sandbox_blocked",
                "hint": "Grant Connected Folders access to ~/.cargo, ~/.rustup, and ~/rust-target or run scripts/dev-verify.py in a local terminal.",
            }
            if args.mode in ("quick", "full"):
                print(json.dumps(summary, indent=2))
                sys.exit(2)
        else:
            summary["cargo_verification"] = run_cargo_checks(
                ROOT, toolchain["env"], "full" if args.mode == "full" else "quick"
            )

    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
