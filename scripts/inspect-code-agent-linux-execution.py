#!/usr/bin/env python3
"""Read inherited Linux privilege flags without changing services or running sudo."""
from pathlib import Path
import argparse
import json
import os
import subprocess

PROPERTIES = (
    "MainPID", "FragmentPath", "DropInPaths", "NoNewPrivileges", "DynamicUser",
    "PrivateTmp", "PrivateDevices", "ProtectKernelTunables", "ProtectKernelModules",
    "ProtectKernelLogs", "ProtectClock", "LockPersonality", "MemoryDenyWriteExecute",
    "RestrictRealtime", "RestrictSUIDSGID", "SystemCallFilter", "RestrictAddressFamilies",
)
def process_security(pid, proc_root=Path("/proc")):
    values = {}
    try:
        text = (proc_root / str(pid) / "status").read_text()
    except OSError:
        return {"pid": pid, "available": False}
    for line in text.splitlines():
        key, separator, value = line.partition(":")
        if separator and key in ("Name", "PPid", "Uid", "Gid", "NoNewPrivs"):
            values[key] = value.strip()
    flag = values.get("NoNewPrivs")
    return {
        "pid": pid, "available": True, "name": values.get("Name"),
        "parent_pid": int(values.get("PPid", "0")),
        "uids": [int(x) for x in values.get("Uid", "").split()],
        "gids": [int(x) for x in values.get("Gid", "").split()],
        "no_new_privileges": {"0": False, "1": True}.get(flag),
    }
def classify(unit, agent, parent):
    flag = agent.get("no_new_privileges")
    if flag is True and unit.get("NoNewPrivileges") == "yes" and parent.get("no_new_privileges") is False:
        return "service_restriction_inherited"
    if flag is True and parent.get("no_new_privileges") is True:
        return "already_restricted_parent_chain"
    if flag is True:
        return "restriction_confirmed_origin_not_confirmed"
    if flag is False:
        return "no_new_privileges_not_set_sudo_policy_not_tested"
    return "process_restriction_unavailable"
def main():
    p = argparse.ArgumentParser()
    p.add_argument("--service", default="remote-hosts-code-agent.service")
    args = p.parse_args()
    if not args.service or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_.@-" for c in args.service):
        p.error("invalid service name")
    result = subprocess.run(
        ["systemctl", "--user", "show", args.service, "--property=" + ",".join(PROPERTIES)],
        check=True, capture_output=True, text=True, timeout=15,
    )
    unit = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    agent = process_security(int(unit.get("MainPID", "0")))
    parent = process_security(agent.get("parent_pid", 0))
    command = process_security(os.getpid())
    print(json.dumps({
        "service": args.service, "unit": unit, "agent": agent, "parent": parent,
        "diagnostic_command": command, "classification": classify(unit, agent, parent),
        "system_settings_changed": False, "sudo_started": False,
        "password_collection": "not_supported_in_tool_text_or_logs",
        "guidance": "Use existing login-user/SSH and sudo policy only after the selected service profile is approved. PTY does not remove an inherited flag.",
    }))
if __name__ == "__main__":
    main()
