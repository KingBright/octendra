---
name: octendra-agent
description: Operate and maintain the user's transport-first Octendra system for managed SSH machines, infrastructure topology, and direct instance synchronization of inventory, knowledge, and user-authorized encrypted credentials. Use for arbitrary POSIX/PowerShell commands over pooled connections, persistent PTYs, host inventory and deduplication, cluster and per-host service topology, encrypted credentials, password fallback and public-key bootstrap, direct/FRP/VPN/proxy/jump/bastion routes, file transfer, output artifacts, runtime health and errors, peer sync, and MCP setup in Codex or Antigravity. Do not invent domain-specific tools when the remote CLI can run through the generic channel.
---

# Octendra Agent Skill

Use this skill for Octendra code editing across authorized devices (`octendra-code`) and for managed SSH operations (`octendra_*` MCP tools).

## 1-Step Fast Paths (Lowest Token & Tool-Call Overhead)

### 1. Code Gateway (`octendra-code` / `octendra_code_execute`)
- **Direct 1-call execution**: `code_read`, `code_search`, `code_apply_edits`, `terminal_exec`, `files_sync`, and `file_upload` accept `workspace_id` as an existing Workspace UUID **or** `"<device_name_or_id>:/abs/path"` (or `":default"` / `"/abs/path"` on a single-device Gateway). The Gateway and Agent automatically resolve device names and open/reuse the workspace.
- **`idempotency_key` is optional**: Omit `idempotency_key` for normal interactive or iterative calls; supply an explicit key only when you want deterministic deduplication across network retries.
- **Default 10s inline wait**: `terminal_exec` waits up to `10000ms` by default (`wait_ms` configurable up to `60000ms`) and returns up to 32–64 KB of output inline in the first response.
- **Sudo (`sudo: true`)**: On an Agent with pre-authorized local sudo (`octendra-code authorize-sudo --config <agent.json>` or `--sudoers`), pass `sudo: true` to `terminal_exec`. Never send passwords in chat, tool arguments, `terminal_input`, command text, or logs.

### 2. Managed SSH (`octendra_*` MCP tools)
- **Direct 1-call execution**: Call `octendra_run_in_workspace` directly with `host_id: "<host_name | display_name | ip_address | uuid>"` (leave `workspace_id: ""`), `command_profile: "shell.posix"`, `args: ["..."]`, and `wait_timeout_ms: 10000`. It automatically prepares/reuses a healthy Workspace, waits for completion, and returns compacted stdout/stderr inline in `completion.output`!
- **One-time sudo (`sudo: true`)**: Pass `sudo: true` on `octendra_run_in_workspace` (or run `sudo -n ...` in `shell.posix`). Noninteractive SSH uses the target's existing `sudo -n` authority and EOF stdin; it never injects Vault passwords or reuses the SSH login password for sudo. If existing OS authorization is insufficient, the command fails without a credential fallback.
- **Host registration**: Use `octendra_ensure_host` to register or update a host, route, and encrypted credential in one idempotent call.

## Connection Authority & Sudo Rules

- Code operations reuse the authenticated connection, account scopes, enrolled device scopes, and Agent local flags. There is no separate task grant or per-command business approval.
- Client OAuth tokens retain the deployed 1-hour access / 30-day refresh policy. Authenticated confidential clients may retry a consumed refresh token for 30 seconds and receive the current valid successor with its remaining access lifetime. Public clients retain reuse detection; a client ID alone never authorizes replay. Family revocation remains durable.
- Never silently substitute an SSH host ID for a Code Gateway device ID or fail over an offline device to a different machine.
- Secrets stay in credential-only tools or local `authorize-sudo` storage (`0600 sudo.secret` / OS Keychain / `/etc/sudoers.d/octendra-code`).

## Core Rules

1. Prefer `octendra_*` MCP tools over shelling out to `ssh` or editing SQLite directly.
2. Reuse `idle` or `working` workspaces; `octendra_run_in_workspace` with `host_id` automatically recovers when a previous session's workspace expired.
3. Treat `blocked`, `exhausted`, `throttled`, `rate_limited`, `connector_offline`, `auth_failed`, `host_key_changed`, and `ssh_route_unsupported` as stop-and-diagnose states (except `pty_input_required` on an active PTY).
4. Keep output bounded. When `completion.output` or `terminal_exec` inline output is sufficient, do not make extra read calls. Use `octendra_get_workspace_result` or `terminal_read` only for pagination or running processes.
5. Every `shell.posix` or `shell.powershell` request declares `coordination_mode` (`read_only` for pure observation, `mutating` for side effects) and a lowercase `coordination_scope` (default `"host"`).
6. If any Code mutation returns `outcome_unknown`, inspect the original operation with `operation_get` and resolve with `outcome_resolve` (`verified_applied` or `verified_not_applied`) only after verifying side effects. Omitting a retry key never bypasses the unknown-outcome guard.

## Octendra Code Observation Contract

For Code tools, submit once with a stable idempotency key when retry deduplication is needed, then observe `operation_get` with the same `operation_id`, including while its terminal is running. Use `wait_ms`, state `cursor` and byte `terminal_cursor` only when actually exposed by the host. `terminal_read` is the full-log or missing-range recovery path, not a mandatory second task. A compact preview may use `output_ref: "#/output"` to reference the identical top-level output; do not treat it as missing bytes. Byte cursors apply to sanitized UTF-8, not compressed character counts. `output_complete` means capture is sealed; also inspect range completeness, gaps, truncation and the exit code.

## Reference Router (Load On Demand Only)

- Host inventory, deduplication, or route updates: [host-registry.md](references/host-registry.md)
- Advanced PTY workflows, file transfers, and lease scopes: [mcp-workflows.md](references/mcp-workflows.md)
- SSH/route errors, bastion menus (`10.36.31.20`), and capacity diagnosis: [connection-and-errors.md](references/connection-and-errors.md)
- Bastion large-file MinIO relay: [minio-relay.md](references/minio-relay.md)
- Cluster/service topology snapshots: [topology-and-inventory.md](references/topology-and-inventory.md)
- Cross-instance encrypted sync: [instance-sync.md](references/instance-sync.md)
- Local service installation, signing recovery, and MCP setup: [setup-and-runtime.md](references/setup-and-runtime.md)

## Reporting

Report decision-changing facts only: target/Workspace, action, final state, and key output or verification evidence.
