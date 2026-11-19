#!/usr/bin/env python3
"""Build, package, sign, and deploy OctendraAgentRouter.app for macOS TCC continuity."""

import argparse
import json
import os
import pathlib
import plistlib
import shutil
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

APP_NAME = "Octendra Agent Router"
APP_BUNDLE_NAME = "OctendraAgentRouter.app"
BUNDLE_ID = "fun.hackerlife.octendra.agent-router"
MCP_HELPER_ID = "fun.hackerlife.octendra.gemini-desktop-mcp"
CLIENT_HELPER_ID = "fun.hackerlife.octendra.gemini-desktop-client"
LAUNCHD_LABEL = "fun.hackerlife.octendra.agent-router"
BIND_ADDR = "127.0.0.1:18790"
LSREGISTER_BIN = (
    "/System/Library/Frameworks/CoreServices.framework/Frameworks/"
    "LaunchServices.framework/Support/lsregister"
)


def run_security(*args: str, check: bool = True, capture: bool = False) -> subprocess.CompletedProcess[str]:
    op = args[0] if args else "security"
    res = subprocess.run(
        ["/usr/bin/security", *args],
        check=False,
        text=True,
        capture_output=capture,
        timeout=30,
    )
    res.args = ["/usr/bin/security", op]
    if check and res.returncode != 0:
        raise RuntimeError(f"security {op} failed with exit {res.returncode}")
    return res


def load_signing_identity() -> tuple[pathlib.Path, str, str]:
    signing_dir = pathlib.Path.home() / ".local/share/octendra/agent/signing"
    ident_path = signing_dir / "identity.json"
    if not ident_path.is_file():
        raise RuntimeError(f"missing signing identity metadata: {ident_path}")
    data = json.loads(ident_path.read_text())
    keychain = pathlib.Path(data.get("keychain") or data.get("keychain_path") or (signing_dir / "octendra-signing.keychain-db"))
    pw_path = pathlib.Path(data.get("keychain_password_path") or (signing_dir / "keychain-password"))
    sha1 = str(data.get("certificate_sha1") or "").strip().lower()
    if len(sha1) != 40 or not keychain.is_file() or not pw_path.is_file():
        raise RuntimeError(f"incomplete signing identity under {signing_dir}")
    password = pw_path.read_text().strip()

    existing = run_security("list-keychains", "-d", "user", capture=True).stdout.splitlines()
    existing_paths = [pathlib.Path(line.strip().strip('"')) for line in existing if line.strip()]
    if not any(p.expanduser().resolve() == keychain.expanduser().resolve() for p in existing_paths):
        run_security("list-keychains", "-d", "user", "-s", str(keychain), *map(str, existing_paths))

    run_security("unlock-keychain", "-p", password, str(keychain))
    run_security("set-keychain-settings", str(keychain))
    run_security(
        "set-key-partition-list",
        "-S",
        "apple-tool:,apple:,codesign:",
        "-s",
        "-k",
        password,
        str(keychain),
        capture=True,
    )
    ident_out = run_security("find-identity", "-v", "-p", "codesigning", str(keychain), capture=True).stdout
    if sha1.upper() not in ident_out or "CSSMERR_" in ident_out:
        raise RuntimeError(f"signing identity {sha1} not valid in {keychain}: {ident_out.strip()}")
    return keychain, sha1, f'certificate leaf = H"{sha1}"'


def build_binaries(repo_root: pathlib.Path, cargo_target_dir: pathlib.Path) -> None:
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(cargo_target_dir)
    for manifest, pkg_args in [
        (repo_root / "Cargo.toml", ["-p", "octendra-agent-router"]),
        (repo_root / "tools/gemini-mcp/Cargo.toml", []),
    ]:
        cmd = [
            "cargo",
            "build",
            "--manifest-path",
            str(manifest),
            *pkg_args,
            "--release",
            "--locked",
            "--offline",
            "-j",
            "4",
        ]
        subprocess.run(cmd, env=env, check=True)

    prune_bin = pathlib.Path.home() / ".local/bin/cargo-cache-prune"
    if prune_bin.is_file():
        subprocess.run([str(prune_bin), str(cargo_target_dir)], check=False)


def sign_with_identity(
    target: pathlib.Path,
    identifier: str,
    keychain: pathlib.Path,
    sha1: str,
    leaf_req: str,
    req_dir: pathlib.Path,
) -> None:
    subprocess.run(["/usr/bin/xattr", "-cr", str(target)], check=False)
    req_file = req_dir / f"{identifier}.req"
    req_file.write_text(f'designated => identifier "{identifier}" and {leaf_req}\n')
    subprocess.run(
        [
            "/usr/bin/codesign",
            "--force",
            "--sign",
            sha1,
            "--keychain",
            str(keychain),
            "--identifier",
            identifier,
            "--requirements",
            str(req_file),
            "--timestamp=none",
            str(target),
        ],
        check=True,
    )


def verify_app_signature(app_path: pathlib.Path, sha1: str) -> None:
    subprocess.run(
        ["/usr/bin/codesign", "--verify", "--deep", "--strict", "--verbose=2", str(app_path)],
        check=True,
    )
    info = subprocess.run(
        ["/usr/bin/codesign", "-dv", "-r-", str(app_path)],
        check=True,
        capture_output=True,
        text=True,
    )
    combined = (info.stdout or "") + "\n" + (info.stderr or "")
    lower = combined.lower()
    if f'identifier "{BUNDLE_ID}"' not in lower or sha1.lower() not in lower:
        raise RuntimeError(f"unexpected codesign designated requirement on {app_path}:\n{combined}")
    if "cdhash" in lower:
        raise RuntimeError(f"codesign bound to ephemeral cdhash on {app_path}:\n{combined}")


def update_launchd_and_reload(repo_root: pathlib.Path, router_exe: pathlib.Path) -> None:
    home = pathlib.Path.home()
    log_dir = home / ".local/share/octendra/logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    plist_path = home / "Library/LaunchAgents" / f"{LAUNCHD_LABEL}.plist"
    plist_path.parent.mkdir(parents=True, exist_ok=True)

    plist_data = {
        "Label": LAUNCHD_LABEL,
        "ProgramArguments": [str(router_exe), "daemon", "--bind", BIND_ADDR],
        "WorkingDirectory": str(repo_root),
        "RunAtLoad": True,
        "KeepAlive": True,
        "ProcessType": "Interactive",
        "ThrottleInterval": 3,
        "EnvironmentVariables": {
            "HOME": str(home),
            "PATH": f"{home}/.local/bin:{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
        },
        "StandardOutPath": str(log_dir / "agent-router.out.log"),
        "StandardErrorPath": str(log_dir / "agent-router.err.log"),
    }
    tmp_plist = plist_path.with_suffix(".plist.tmp")
    with tmp_plist.open("wb") as f:
        plistlib.dump(plist_data, f)
    subprocess.run(["/usr/bin/plutil", "-lint", str(tmp_plist)], check=True)
    os.replace(tmp_plist, plist_path)

    uid = os.getuid()
    domain = f"gui/{uid}"
    service = f"{domain}/{LAUNCHD_LABEL}"
    subprocess.run(["/bin/launchctl", "bootout", service], check=False, capture_output=True)
    time.sleep(0.5)
    subprocess.run(["/bin/launchctl", "bootstrap", domain, str(plist_path)], check=True)
    subprocess.run(["/bin/launchctl", "kickstart", "-kp", service], check=False)


def http_get_json_no_proxy(url: str, timeout: float = 5.0) -> tuple[int, dict]:
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    req = urllib.request.Request(url, method="GET")
    try:
        with opener.open(req, timeout=timeout) as resp:
            return resp.status, json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as err:
        body = err.read().decode("utf-8", errors="replace")
        try:
            return err.code, json.loads(body)
        except Exception:
            return err.code, {"raw": body}


def main() -> None:
    parser = argparse.ArgumentParser(description="Install signed OctendraAgentRouter.app and reload LaunchAgent")
    parser.add_argument("--skip-build", action="store_true", help="Reuse existing release binaries from CARGO_TARGET_DIR")
    args = parser.parse_args()

    repo_root = pathlib.Path(__file__).resolve().parent.parent
    cargo_target_dir = pathlib.Path(
        os.environ.get("CARGO_TARGET_DIR") or (pathlib.Path.home() / "rust-target")
    ).expanduser()

    if not args.skip_build:
        build_binaries(repo_root, cargo_target_dir)

    release_dir = cargo_target_dir / "release"
    bins = {
        "router": release_dir / "octendra-agent-router",
        "mcp": release_dir / "gemini-desktop-mcp",
        "client": release_dir / "gemini-desktop-client",
    }
    for name, bin_path in bins.items():
        if not bin_path.is_file():
            raise RuntimeError(f"missing release binary ({name}): {bin_path}")

    keychain, sha1, leaf_req = load_signing_identity()

    apps_dir = pathlib.Path.home() / "Applications"
    apps_dir.mkdir(parents=True, exist_ok=True)
    target_app = apps_dir / APP_BUNDLE_NAME
    local_bin = pathlib.Path.home() / ".local/bin"
    local_bin.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="octendra-app-stage-", dir=apps_dir) as tmp_dir_str:
        tmp_dir = pathlib.Path(tmp_dir_str)
        req_dir = tmp_dir / "reqs"
        req_dir.mkdir(parents=True, exist_ok=True)

        stage_app = tmp_dir / APP_BUNDLE_NAME
        contents = stage_app / "Contents"
        macos_dir = contents / "MacOS"
        helpers_dir = contents / "Helpers"
        macos_dir.mkdir(parents=True, exist_ok=True)
        helpers_dir.mkdir(parents=True, exist_ok=True)

        info_plist = {
            "CFBundleDevelopmentRegion": "zh_CN",
            "CFBundleDisplayName": APP_NAME,
            "CFBundleExecutable": "octendra-agent-router",
            "CFBundleIdentifier": BUNDLE_ID,
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleName": APP_NAME,
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": "0.1.0",
            "CFBundleVersion": "0.1.0",
            "LSMinimumSystemVersion": "14.0",
            "LSUIElement": True,
            "NSHighResolutionCapable": True,
        }
        with (contents / "Info.plist").open("wb") as f:
            plistlib.dump(info_plist, f)

        staged_router = macos_dir / "octendra-agent-router"
        staged_mcp = helpers_dir / "gemini-desktop-mcp"
        staged_client = helpers_dir / "gemini-desktop-client"
        for src, dst in [
            (bins["router"], staged_router),
            (bins["mcp"], staged_mcp),
            (bins["client"], staged_client),
        ]:
            shutil.copy2(src, dst)
            os.chmod(dst, 0o755)

        sign_with_identity(staged_mcp, MCP_HELPER_ID, keychain, sha1, leaf_req, req_dir)
        sign_with_identity(staged_client, CLIENT_HELPER_ID, keychain, sha1, leaf_req, req_dir)
        sign_with_identity(stage_app, BUNDLE_ID, keychain, sha1, leaf_req, req_dir)
        verify_app_signature(stage_app, sha1)

        # Install CLI copies under ~/.local/bin using atomic replace
        for src, name, ident in [
            (bins["router"], "octendra-agent-router", BUNDLE_ID),
            (bins["mcp"], "gemini-desktop-mcp", MCP_HELPER_ID),
            (bins["client"], "gemini-desktop-client", CLIENT_HELPER_ID),
        ]:
            cli_stage = tmp_dir / f"cli-{name}"
            shutil.copy2(src, cli_stage)
            os.chmod(cli_stage, 0o755)
            sign_with_identity(cli_stage, ident, keychain, sha1, leaf_req, req_dir)
            os.replace(cli_stage, local_bin / name)

        backup_app = tmp_dir / f"{APP_BUNDLE_NAME}.old"
        if target_app.exists():
            shutil.move(str(target_app), str(backup_app))
        shutil.move(str(stage_app), str(target_app))

    verify_app_signature(target_app, sha1)
    if pathlib.Path(LSREGISTER_BIN).is_file():
        subprocess.run([LSREGISTER_BIN, "-f", str(target_app)], check=False)

    router_exe = target_app / "Contents/MacOS/octendra-agent-router"
    update_launchd_and_reload(repo_root, router_exe)

    deadline = time.time() + 25.0
    health_payload = None
    while time.time() < deadline:
        try:
            code, health_payload = http_get_json_no_proxy(f"http://{BIND_ADDR}/v1/health", timeout=2.0)
            if code == 200 and health_payload.get("status") == "ok":
                break
        except Exception:
            pass
        time.sleep(0.5)
    else:
        raise RuntimeError(f"agent-router failed health check at http://{BIND_ADDR}/v1/health")

    r_code, readiness = http_get_json_no_proxy(f"http://{BIND_ADDR}/v1/gemini/readiness", timeout=30.0)
    print(
        json.dumps(
            {
                "app_bundle": str(target_app),
                "bundle_id": BUNDLE_ID,
                "certificate_sha1": sha1,
                "health": health_payload,
                "readiness_http_code": r_code,
                "readiness": readiness,
            },
            indent=2,
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
