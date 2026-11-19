#!/usr/bin/env python3
"""Owner CLI for Octendra Code Gateway fleet operations.

Uses the repository's existing OAuth bootstrap and native Rust MCP stdio
adapter (`release_client.Client`) so fleet status checks and remote commands
work cleanly from any terminal without depending on an IDE's MCP snapshot.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import sys

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from release_client import Client  # noqa: E402

DEFAULT_ORIGIN = os.environ.get("OCTENDRA_GATEWAY_ORIGIN", "https://octendra.hackerlife.fun:8443")
DEFAULT_PASSWORD_FILE = os.environ.get(
    "OCTENDRA_OWNER_PASSWORD_FILE",
    str(Path.home() / ".config/octendra/gateway-owner-password"),
)


def connect(origin: str, password_file: str) -> Client:
    pw = Path(password_file).expanduser()
    if not pw.is_file():
        raise SystemExit(
            f"Owner password file not found: {pw}\n"
            "Pass --password-file PATH or set OCTENDRA_OWNER_PASSWORD_FILE."
        )
    client = Client(origin.rstrip("/"), password_file=str(pw))
    client.login()
    return client


def cmd_devices(client: Client, as_json: bool) -> int:
    result = client.raw("devices_list", {})
    if as_json:
        print(json.dumps(result, indent=2, ensure_ascii=False))
        return 0
    devices = result.get("devices") if isinstance(result, dict) else []
    for d in devices or []:
        name = d.get("name") or d.get("device_id")
        online = "online" if d.get("online") else "offline"
        ver = d.get("agent_version") or d.get("version") or "-"
        plat = d.get("platform") or d.get("os") or "-"
        print(f"{name:28s}  {online:8s}  v={ver:10s}  platform={plat}")
    return 0


def cmd_status(client: Client, as_json: bool) -> int:
    result = client.raw("fleet_status", {})
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0


def cmd_exec(client: Client, device: str, cwd: str, command: str, wait_ms: int, sudo: bool, as_json: bool) -> int:
    workspace = f"{device}:{cwd}"
    args = {
        "workspace_id": workspace,
        "command": command,
        "wait_ms": wait_ms,
    }
    if sudo:
        args["sudo"] = True
    result = client.tool("terminal_exec", args)
    if as_json:
        print(json.dumps(result, indent=2, ensure_ascii=False))
        return 0
    output = result.get("output") if isinstance(result, dict) else None
    if output:
        sys.stdout.write(output)
        if not output.endswith("\n"):
            sys.stdout.write("\n")
    exit_code = result.get("exit_code") if isinstance(result, dict) else None
    return int(exit_code) if isinstance(exit_code, int) else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--origin", default=DEFAULT_ORIGIN, help="Octendra Gateway HTTPS origin")
    parser.add_argument("--password-file", default=DEFAULT_PASSWORD_FILE, help="Owner password file path")
    parser.add_argument("--json", action="store_true", help="Emit full structured JSON")
    sub = parser.add_subparsers(dest="cmd", required=True)

    sub.add_parser("devices", help="List enrolled devices, online state, and versions")
    sub.add_parser("status", help="Show full Gateway fleet_status summary")

    p_exec = sub.add_parser("exec", help="Execute a command on a Gateway-connected device")
    p_exec.add_argument("--device", required=True, help="Target device name or UUID (e.g. MacBook-M2-Max, HO5)")
    p_exec.add_argument("--cwd", default="/", help="Absolute working directory on target device")
    p_exec.add_argument("--wait-ms", type=int, default=30000, help="Inline wait milliseconds (default: 30000)")
    p_exec.add_argument("--sudo", action="store_true", help="Request pre-authorized local sudo on target Agent")
    p_exec.add_argument("command", nargs=argparse.REMAINDER, help="Command to run (prefix with --)")

    args = parser.parse_args()
    client = connect(args.origin, args.password_file)
    try:
        if args.cmd == "devices":
            return cmd_devices(client, args.json)
        if args.cmd == "status":
            return cmd_status(client, args.json)
        if args.cmd == "exec":
            parts = list(args.command)
            if parts and parts[0] == "--":
                parts = parts[1:]
            cmd_str = " ".join(parts).strip()
            if not cmd_str:
                parser.error("exec requires a command after --")
            return cmd_exec(client, args.device, args.cwd, cmd_str, args.wait_ms, args.sudo, args.json)
    finally:
        client.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
