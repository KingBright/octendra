#!/usr/bin/env python3
"""Local developer verification runner, workspace-queued host build bridge, and build cache pruner.

Key responsibilities:
1. Reuse the single established `~/rust-target` (`CARGO_TARGET_DIR`) so builds never
   create duplicate target trees inside the repository.
2. Automatically prune superseded hashed binaries/rlibs/dSYMs in `deps/`, stale
   sessions in `incremental/`, and retired slot `cargo-target`/`checkout` trees
   after every build/test run (or via `--prune`), keeping only the minimal newest
   artifacts required for fast incremental compilation.
3. Provide a workspace-queued host worker (`--serve`) under `.local-deployment/dev-runner/`
   so sandboxed agent sessions can trigger compilation, tests, and cache pruning
   without adding large directories to Connected Folders.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
import uuid

import lifecycle_preload_gate as preload_gate
import release_current

ROOT = Path(__file__).resolve().parents[1]
QUEUE_DIR = ROOT / ".local-deployment" / "dev-runner"
ALLOWED_EXEC_PREFIXES = ("cargo", "rustc", "rustup", "python3", sys.executable)
HASHED_ARTIFACT_RE = re.compile(r"^(?P<stem>.+)-(?P<hash>[0-9a-f]{16})(?P<suffix>\..+)?$")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def real_user_home() -> Path:
    pw_home = Path("/Users") / os.environ.get("USER", "")
    if pw_home.name and pw_home != Path("/Users"):
        return pw_home
    return Path(os.path.expanduser("~"))


def find_toolchain():
    env = dict(os.environ)
    user_home = real_user_home()
    established_target = user_home / "rust-target"
    env.setdefault("CARGO_TARGET_DIR", str(established_target))
    env.setdefault("CARGO_TERM_COLOR", "never")
    env.setdefault("PYTHONUNBUFFERED", "1")

    candidates = []
    if env.get("CARGO_HOME"):
        candidates.append(Path(env["CARGO_HOME"]) / "bin")
    candidates.append(user_home / ".cargo" / "bin")
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
        "cargo_target_dir": env["CARGO_TARGET_DIR"],
        "blocked_paths": blocked,
        "env": env,
    }


def remove_path(path: Path) -> int:
    """Remove a file or directory tree and return approximate reclaimed bytes."""
    try:
        if path.is_symlink() or path.is_file():
            size = path.lstat().st_size
            path.unlink(missing_ok=True)
            return size
        if path.is_dir():
            size = sum(
                f.lstat().st_size
                for f in path.rglob("*")
                if f.is_file() or f.is_symlink()
            )
            shutil.rmtree(path, ignore_errors=True)
            return size
    except OSError:
        return 0
    return 0


WORKSPACE_ARTIFACT_PREFIXES = (
    "octendra",
    "liboctendra",
    "review_regressions",
    "reliability_",
    "experience_contract",
    "desktop_worker",
    "adapter_",
    "durable_",
    "history_",
    "maintenance_",
    "oauth_",
    "resource_",
    "security_",
    "storage_",
    "terminal_",
    "transfer_",
    "workspace_",
)


def is_workspace_stem(stem: str) -> bool:
    return stem.startswith(WORKSPACE_ARTIFACT_PREFIXES)


def prune_cargo_target_dir(target_dir: Path) -> dict:
    """Keep only the newest hash per workspace crate/test stem in deps/ and newest session in incremental/."""
    reclaimed = 0
    removed_entries = 0
    removed_paths: list[str] = []
    try:
        if not target_dir.is_dir():
            return {
                "target_dir": str(target_dir),
                "accessible": False,
                "reclaimed_bytes": 0,
                "removed_entries": 0,
                "removed_paths": [],
            }
    except PermissionError:
        return {
            "target_dir": str(target_dir),
            "accessible": False,
            "reclaimed_bytes": 0,
            "removed_entries": 0,
            "removed_paths": [],
        }

    profile_roots = [target_dir / "debug", target_dir / "release"]
    try:
        for child in target_dir.iterdir():
            if child.is_dir() and child.name not in ("debug", "release", "tmp", ".rustc_info.json"):
                for prof in ("debug", "release"):
                    if (child / prof).is_dir():
                        profile_roots.append(child / prof)
    except OSError:
        pass

    for prof_dir in profile_roots:
        deps_dir = prof_dir / "deps"
        if deps_dir.is_dir():
            groups: dict[str, dict[str, list[Path]]] = {}
            for entry in deps_dir.iterdir():
                m = HASHED_ARTIFACT_RE.match(entry.name)
                if not m or not is_workspace_stem(m.group("stem")):
                    continue
                groups.setdefault(m.group("stem"), {}).setdefault(m.group("hash"), []).append(entry)

            for _stem, by_hash in groups.items():
                if len(by_hash) <= 3:
                    continue
                ranked = sorted(
                    by_hash.items(),
                    key=lambda item: max((p.lstat().st_mtime for p in item[1] if p.exists()), default=0.0),
                    reverse=True,
                )
                for _old_hash, paths in ranked[3:]:
                    for p in paths:
                        reclaimed += remove_path(p)
                        removed_entries += 1
                        removed_paths.append(str(p))

        inc_dir = prof_dir / "incremental"
        if inc_dir.is_dir():
            for crate_dir in inc_dir.iterdir():
                if not crate_dir.is_dir() or not is_workspace_stem(crate_dir.name):
                    continue
                sessions = [s for s in crate_dir.iterdir() if s.is_dir()]
                if len(sessions) > 2:
                    sessions.sort(key=lambda p: p.lstat().st_mtime, reverse=True)
                    for old_session in sessions[2:]:
                        reclaimed += remove_path(old_session)
                        removed_entries += 1
                        removed_paths.append(str(old_session))

        build_dir = prof_dir / "build"
        if build_dir.is_dir():
            bgroups: dict[str, dict[str, list[Path]]] = {}
            for entry in build_dir.iterdir():
                m = HASHED_ARTIFACT_RE.match(entry.name)
                if not m or not is_workspace_stem(m.group("stem")):
                    continue
                bgroups.setdefault(m.group("stem"), {}).setdefault(m.group("hash"), []).append(entry)
            for _stem, by_hash in bgroups.items():
                if len(by_hash) <= 3:
                    continue
                ranked = sorted(
                    by_hash.items(),
                    key=lambda item: max((p.lstat().st_mtime for p in item[1] if p.exists()), default=0.0),
                    reverse=True,
                )
                for _old_hash, paths in ranked[3:]:
                    for p in paths:
                        reclaimed += remove_path(p)
                        removed_entries += 1
                        removed_paths.append(str(p))

    return {
        "target_dir": str(target_dir),
        "accessible": True,
        "reclaimed_bytes": reclaimed,
        "reclaimed_mib": round(reclaimed / (1024 * 1024), 2),
        "removed_entries": removed_entries,
        "removed_paths": removed_paths,
    }


def prune_workspace_Build_bloat(root: Path, established_target: Path | None = None) -> dict:
    """Explicit lifecycle maintenance: clean retired slot checkouts/targets when no build slot is active."""
    sys.path.insert(0, str(root / "scripts"))
    import build_slot

    reclaimed = 0
    actions = []
    any_active_build = False
    repo_target = root / "target"
    if repo_target.is_dir():
        for slot_dir in sorted(repo_target.iterdir()):
            if not slot_dir.is_dir():
                continue
            owner_file = slot_dir / "owner.json"
            alive = False
            if owner_file.is_file():
                try:
                    owner = json.loads(owner_file.read_text())
                    alive = build_slot.process_alive(owner.get("pid"))
                except Exception:
                    alive = False
            if alive:
                any_active_build = True
                continue

            checkout_dir = slot_dir / "checkout"
            if checkout_dir.is_dir():
                freed = remove_path(checkout_dir)
                reclaimed += freed
                actions.append({"removed": str(checkout_dir.relative_to(root)), "reclaimed_mib": round(freed / (1024 * 1024), 2)})

            slot_cargo_target = slot_dir / "cargo-target"
            if slot_cargo_target.is_dir():
                if slot_dir.name == "admin-maintenance-task-slot":
                    freed = remove_path(slot_cargo_target)
                    reclaimed += freed
                    actions.append({"removed": str(slot_cargo_target.relative_to(root)), "reclaimed_mib": round(freed / (1024 * 1024), 2)})
                else:
                    res = prune_cargo_target_dir(slot_cargo_target)
                    reclaimed += res["reclaimed_bytes"]
                    if res["reclaimed_bytes"] > 0:
                        actions.append({
                            "pruned": str(slot_cargo_target.relative_to(root)),
                            "reclaimed_mib": res["reclaimed_mib"],
                            "removed_paths": res["removed_paths"],
                        })

    for sub in ("logs", "results"):
        d = QUEUE_DIR / sub
        if d.is_dir():
            files = sorted(d.iterdir(), key=lambda p: p.lstat().st_mtime, reverse=True)
            for old in files[10:]:
                reclaimed += remove_path(old)

    target_prune = None
    if established_target is not None:
        if any_active_build:
            target_prune = {
                "target_dir": str(established_target),
                "skipped": "active_build_slot_lease",
                "reclaimed_bytes": 0,
                "removed_entries": 0,
                "removed_paths": [],
            }
        else:
            target_prune = prune_cargo_target_dir(established_target)
            reclaimed += target_prune["reclaimed_bytes"]

    return {
        "total_reclaimed_mib": round(reclaimed / (1024 * 1024), 2),
        "workspace_actions": actions,
        "established_target_prune": target_prune,
    }


def worker_status(queue_dir: Path = QUEUE_DIR):
    hb = queue_dir / "worker.json"
    if not hb.is_file():
        return {"online": False, "reason": "worker.json not found"}
    try:
        data = json.loads(hb.read_text())
        age = time.time() - float(data.get("updated_at", 0))
        return {
            "online": age < 600.0,
            "age_seconds": round(age, 2),
            "pid": data.get("pid"),
            "cargo": data.get("cargo"),
            "rustc": data.get("rustc"),
            "cargo_target_dir": data.get("cargo_target_dir"),
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

    established_target = Path(toolchain["cargo_target_dir"])
    hb_path = queue_dir / "worker.json"
    startup_mtime = Path(__file__).stat().st_mtime
    print(
        json.dumps(
            {
                "state": "serving",
                "queue_dir": str(queue_dir.relative_to(ROOT)),
                "pid": os.getpid(),
                "cargo": toolchain["cargo"],
                "rustc": toolchain["rustc"],
                "cargo_target_dir": str(established_target),
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
                        "cargo_target_dir": str(established_target),
                    },
                    indent=2,
                )
                + "\n"
            )
            hb_tmp.replace(hb_path)

            if Path(__file__).stat().st_mtime != startup_mtime:
                os.execv(sys.executable, [sys.executable, str(Path(__file__).resolve()), "--serve"])
            for req_file in sorted(req_dir.glob("*.json")):
                try:
                    with release_current.admission():
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

                except preload_gate.Refusal as error:
                    print(json.dumps({"state": "admission_blocked", "reason": str(error), "request_preserved": req_file.exists()}), flush=True)
                    return

            if once:
                break
            time.sleep(0.25)
    finally:
        if not once:
            hb_path.unlink(missing_ok=True)


def verify_static_contracts(root: Path):
    cargo_toml = tomllib.loads((root / "crates/octendra-code/Cargo.toml").read_text())
    version = cargo_toml["package"]["version"]
    contract_path = root / "crates/octendra-code/adapter-contract.json"
    contract = json.loads(contract_path.read_text())

    if contract.get("version") != version:
        raise ValueError(f"adapter-contract version {contract.get('version')} != Cargo.toml {version}")

    tool_keys = ("name", "title", "description", "inputSchema", "outputSchema", "annotations", "execution", "icons", "_meta")
    ann_keys = ("title", "readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint")
    ordered_tools = [
        {
            k: ({ak: t[k][ak] for ak in ann_keys if ak in t[k]} if k == "annotations" and isinstance(t[k], dict) else t[k])
            for k in tool_keys
            if k in t
        }
        for t in contract["tools"]
    ]
    tools_bytes = json.dumps(ordered_tools, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
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

    skill_dir = root / "skills/octendra-agent"
    if not (skill_dir / "SKILL.md").is_file():
        raise ValueError("missing skills/octendra-agent/SKILL.md")

    for crate in ("octendra-code", "octendra-mcp", "octendra-api", "octendra-cli"):
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


@release_current.admission()
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
        step = execute_step([sys.executable, str(root / "scripts/check-code-source.py"), "--report", str(report)], root, toolchain)
        return {"mode": "full", "report": str(report.relative_to(root)), "cache_prune": step.get("cache_prune")}

    commands = [
        ["cargo", "fmt", "-p", "octendra-code", "-p", "octendra-release", "--", "--check"],
        ["cargo", "clippy", "-p", "octendra-code", "-p", "octendra-release", "--all-targets", "--locked", "--", "-D", "warnings"],
        ["cargo", "test", "-p", "octendra-mcp", "--lib", "--locked"],
        [
            "cargo",
            "test",
            "-p",
            "octendra-code",
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
    batch_script = (
        "import os, subprocess, sys; os.umask(0o077); "
        f"cmds = {commands!r}; "
        "[subprocess.run(c, check=True) for c in cmds]"
    )
    step = execute_step([sys.executable, "-c", batch_script], root, toolchain)
    return {"mode": "quick", "commands_completed": len(commands), "cache_prune": step.get("cache_prune")}


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=["auto", "static", "quick", "full"], default="auto")
    parser.add_argument("--serve", action="store_true", help="run host build worker watching .local-deployment/dev-runner")
    parser.add_argument("--status", action="store_true", help="check host build worker status")
    parser.add_argument("--prune", action="store_true", help="prune superseded build artifacts and retired slot targets immediately")
    parser.add_argument("--release", action="store_true", help="run full release-code.py packaging pipeline and prune build cache")
    parser.add_argument("--deploy", type=Path, metavar="CONFIG_JSON", help="deploy verified current release to Gateway and observe Agent auto-upgrade convergence")
    parser.add_argument("--binary", type=Path, help="optional compiled octendra-code binary for isolated CLI smoke tests")
    parser.add_argument("--exec", nargs=argparse.REMAINDER, help="execute an allowed build/test command directly or via host worker")
    args = parser.parse_args()

    toolchain = find_toolchain()
    established_target = Path(toolchain["cargo_target_dir"])

    if args.serve:
        serve_worker()
        return
    if args.status:
        print(json.dumps(worker_status(), indent=2))
        return
    if args.prune:
        print(json.dumps(prune_workspace_Build_bloat(ROOT, established_target), indent=2))
        return
    if args.release:
        report = ROOT / "target" / "dev-verify" / f"release-{int(time.time())}-{os.getpid()}.json"
        report.parent.mkdir(parents=True, exist_ok=True)
        res = execute_step([sys.executable, str(ROOT / "scripts/release-code.py"), "--report", str(report)], ROOT, toolchain)
        print(json.dumps({"release_report": str(report.relative_to(ROOT)), **res}, indent=2))
        return
    if args.deploy:
        cfg = args.deploy.resolve(strict=True)
        ver = tomllib.loads((ROOT / "crates/octendra-code/Cargo.toml").read_text())["package"]["version"]
        pkg = ROOT / "dist" / "octendra-code-current"
        rep_dir = ROOT / "target" / "dev-verify" / f"deploy-{ver}-{int(time.time())}"
        res = execute_step(
            [
                sys.executable,
                str(ROOT / "scripts/fleet-upgrade.py"),
                "--version",
                ver,
                "--package",
                str(pkg),
                "--config",
                str(cfg),
                "--report-dir",
                str(rep_dir),
            ],
            ROOT,
            toolchain,
        )
        print(json.dumps({"deploy_report_dir": str(rep_dir.relative_to(ROOT)), **res}, indent=2))
        return
    if args.exec:
        res = execute_step(args.exec, ROOT, toolchain)
        print(json.dumps(res, indent=2))
        return

    summary = {
        "static_contract": verify_static_contracts(ROOT),
        "python_fast_tests": run_python_fast_tests(ROOT),
    }
    worker = worker_status()
    summary["toolchain"] = {
        "direct_available": toolchain["available"],
        "worker_online": worker.get("online", False),
        "cargo": toolchain["cargo"] or worker.get("cargo"),
        "rustc": toolchain["rustc"] or worker.get("rustc"),
        "cargo_target_dir": toolchain["cargo_target_dir"],
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
