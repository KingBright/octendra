#!/usr/bin/env python3
"""Local developer verification runner and workspace-queued host build bridge.

Why this exists:
- Adding ~/.cargo, ~/.rustup, or ~/rust-target to Connected Folders can trigger
  large workspace backups.
- Running `/opt/homebrew/bin/python3 scripts/dev-verify.py --serve` once in a
  normal local terminal starts a lightweight file-queue worker rooted at
  `.local-deployment/dev-runner/` (which is gitignored and excluded from source
  verification inputs).
- When invoked inside the agent sandbox (`--mode quick`, `--mode full`, or
  `--exec ...`), `dev-verify.py` automatically delegates Cargo/test commands to
  the host worker if direct toolchain access is blocked by the sandbox.
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
import time
import tomllib
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
QUEUE_DIR = ROOT / ".local-deployment" / "dev-runner"
ALLOWED_EXEC_PREFIXES = ("cargo", "rustc", "rustup", "python3", sys.executable)


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


def worker_status(queue_dir: Path = QUEUE_DIR):
    hb = queue_dir / "worker.json"
    if not hb.is_file():
        return {"online": False, "reason": "worker.json not found"}
    try:
        data = json.loads(hb.read_text())
        age = time.time() - float(data.get("updated_at", 0))
        return {
            "online": age < 15.0,
            "age_seconds": round(age, 2),
            "pid": data.get("pid"),
            "cargo": data.get("cargo"),
            "rustc": data.get("rustc"),
        }
    except Exception as error:
        return {"online": False, "reason": str(error)}


def delegate_to_worker(argv: list[str], timeout: int = 1800, queue_dir: Path = QUEUE_DIR):
    status = worker_status(queue_dir)
    if not status.get("online"):
        raise RuntimeError(
            "Host build worker is not running. Start it in a local terminal with:\n"
            "  /opt/homebrew/bin/python3 scripts/dev-verify.py --serve"
        )
    req_dir = queue_dir / "requests"
    res_dir = queue_dir / "results"
    log_dir = queue_dir / "logs"
    for d in (req_dir, res_dir, log_dir):
        d.mkdir(parents=True, exist_ok=True)

    job_id = f"{int(time.time() * 1000)}-{uuid.uuid4().hex[:8]}"
    req_path = req_dir / f"{job_id}.json"
    tmp_path = req_dir / f".{job_id}.json.tmp"
    res_path = res_dir / f"{job_id}.json"
    log_path = log_dir / f"{job_id}.log"

    payload = {
        "id": job_id,
        "argv": list(argv),
        "submitted_at": time.time(),
        "timeout": timeout,
    }
    tmp_path.write_text(json.dumps(payload, indent=2) + "\n")
    tmp_path.replace(req_path)

    started = time.time()
    offset = 0
    while time.time() - started < timeout:
        if log_path.is_file():
            data = log_path.read_bytes()
            if len(data) > offset:
                sys.stdout.write(data[offset:].decode("utf-8", errors="replace"))
                sys.stdout.flush()
                offset = len(data)
        if res_path.is_file():
            if log_path.is_file():
                data = log_path.read_bytes()
                if len(data) > offset:
                    sys.stdout.write(data[offset:].decode("utf-8", errors="replace"))
                    sys.stdout.flush()
            return json.loads(res_path.read_text())
        time.sleep(0.25)
    raise TimeoutError(f"Timed out waiting for worker job {job_id}")


def serve_worker(queue_dir: Path = QUEUE_DIR, once: bool = False):
    toolchain = find_toolchain()
    if not toolchain["available"] and not once:
        raise RuntimeError("cargo/rustc not found in current environment")
    req_dir = queue_dir / "requests"
    res_dir = queue_dir / "results"
    log_dir = queue_dir / "logs"
    for d in (req_dir, res_dir, log_dir):
        d.mkdir(parents=True, exist_ok=True)

    hb_path = queue_dir / "worker.json"
    print(
        json.dumps(
            {
                "state": "serving",
                "queue_dir": str(queue_dir.relative_to(ROOT)),
                "pid": os.getpid(),
                "cargo": toolchain["cargo"],
                "rustc": toolchain["rustc"],
            }
        ),
        flush=True,
    )
    try:
        while True:
            hb_tmp = queue_dir / ".worker.json.tmp"
            hb_tmp.write_text(
                json.dumps(
                    {
                        "pid": os.getpid(),
                        "updated_at": time.time(),
                        "cargo": toolchain["cargo"],
                        "rustc": toolchain["rustc"],
                    },
                    indent=2,
                )
                + "\n"
            )
            hb_tmp.replace(hb_path)

            for req_file in sorted(req_dir.glob("*.json")):
                job = json.loads(req_file.read_text())
                job_id = job["id"]
                argv = job["argv"]
                req_file.unlink(missing_ok=True)
                log_path = log_dir / f"{job_id}.log"
                res_path = res_dir / f"{job_id}.json"
                res_tmp = res_dir / f".{job_id}.json.tmp"

                if not argv or not any(
                    argv[0] == prefix or argv[0].endswith("/" + prefix)
                    for prefix in ALLOWED_EXEC_PREFIXES
                ):
                    result = {"id": job_id, "exit_code": 126, "error": f"disallowed command: {argv}"}
                else:
                    started = time.time()
                    with log_path.open("wb") as log_stream:
                        proc = subprocess.run(
                            argv,
                            cwd=ROOT,
                            env=toolchain["env"],
                            stdout=log_stream,
                            stderr=subprocess.STDOUT,
                            timeout=int(job.get("timeout", 1800)),
                            check=False,
                        )
                    result = {
                        "id": job_id,
                        "argv": argv,
                        "exit_code": proc.returncode,
                        "elapsed_seconds": round(time.time() - started, 3),
                        "log": str(log_path.relative_to(ROOT)),
                    }
                res_tmp.write_text(json.dumps(result, indent=2) + "\n")
                res_tmp.replace(res_path)

            if once:
                break
            time.sleep(0.25)
    finally:
        if not once:
            hb_path.unlink(missing_ok=True)


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


def execute_step(argv: list[str], root: Path, toolchain: dict):
    if toolchain["available"]:
        subprocess.run(argv, cwd=root, env=toolchain["env"], check=True)
        return {"via": "direct", "exit_code": 0}
    res = delegate_to_worker(argv)
    if res.get("exit_code") != 0:
        raise RuntimeError(f"worker command failed ({res.get('exit_code')}): {argv}")
    return {"via": "worker", "exit_code": 0, "job_id": res.get("id")}


def run_cargo_checks(root: Path, toolchain: dict, mode: str):
    if mode == "full":
        report = root / "target" / "dev-verify" / f"proof-{int(time.time())}-{os.getpid()}.json"
        report.parent.mkdir(parents=True, exist_ok=True)
        execute_step([sys.executable, str(root / "scripts/check-code-source.py"), "--report", str(report)], root, toolchain)
        return {"mode": "full", "report": str(report.relative_to(root))}

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
        execute_step(argv, root, toolchain)
    return {"mode": "quick", "commands_completed": len(commands)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=["auto", "static", "quick", "full"], default="auto")
    parser.add_argument("--serve", action="store_true", help="run host build worker watching .local-deployment/dev-runner")
    parser.add_argument("--status", action="store_true", help="check host build worker status")
    parser.add_argument("--binary", type=Path, help="optional compiled remote-hosts-code binary for isolated CLI smoke tests")
    parser.add_argument("--exec", nargs=argparse.REMAINDER, help="execute an allowed build/test command directly or via host worker")
    args = parser.parse_args()

    if args.serve:
        serve_worker()
        return
    if args.status:
        print(json.dumps(worker_status(), indent=2))
        return

    toolchain = find_toolchain()
    if args.exec:
        res = execute_step(args.exec, ROOT, toolchain)
        print(json.dumps(res, indent=2))
        return

    summary = {"static_contract": verify_static_contracts(ROOT), "python_fast_tests": run_python_fast_tests(ROOT)}
    worker = worker_status()
    summary["toolchain"] = {
        "direct_available": toolchain["available"],
        "worker_online": worker.get("online", False),
        "cargo": toolchain["cargo"] or worker.get("cargo"),
        "rustc": toolchain["rustc"] or worker.get("rustc"),
    }

    binary = args.binary
    if binary and binary.is_file() and os.access(binary, os.X_OK):
        summary["binary_smoke"] = run_binary_smoke(binary.resolve(), ROOT)

    can_run_cargo = toolchain["available"] or worker.get("online", False)
    if args.mode in ("quick", "full") or (args.mode == "auto" and can_run_cargo):
        if not can_run_cargo:
            summary["cargo_verification"] = {
                "state": "waiting_for_host_worker",
                "start_worker_command": "/opt/homebrew/bin/python3 scripts/dev-verify.py --serve",
            }
            if args.mode in ("quick", "full"):
                print(json.dumps(summary, indent=2))
                sys.exit(2)
        else:
            summary["cargo_verification"] = run_cargo_checks(
                ROOT, toolchain, "full" if args.mode == "full" else "quick"
            )

    print(json.dumps(summary, indent=2))


if __name__ == "__main__":
    main()
