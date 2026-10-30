<img src="assets/brand/octendra/icon-128.png" alt="Octendra" width="96" height="96">

# Octendra

[中文 README](README.md) | **English**

> Current source, commands, and packages use Octendra. Deployment still requires a coordinated device cutover and acceptance. See [current naming and delivery status](docs/octendra-branding-20261020.md).

Octendra is a remote-operation and code-collaboration platform designed for AI agents and human operators. It combines a reusable SSH control plane with a separate, authenticated ChatGPT code gateway so an agent can work across your own computers without turning every product or device into a bespoke integration.

The project optimizes for four things: **low interaction cost, recoverable execution, explicit isolation, and verifiable delivery**. Long-running work is durable, retries do not silently replay side effects, file transfers are resumable, release evidence is tied to fixed source inputs, and each device remains an explicit target rather than an automatic failover destination.

## What it provides

### Remote operation plane

- Canonical host inventory, environments, routes and credential bindings.
- Reused native `russh` connections on macOS, Linux and Windows, with an optional OpenSSH compatibility backend.
- POSIX and PowerShell execution, persistent PTYs, bounded/redacted output and artifacts.
- Verified uploads/downloads over SFTP, framed exec channels or a selected bastion PTY.
- Per-conversation Workspace, PTY, operation, artifact and idempotency isolation while physical SSH transports are shared safely.
- Local encrypted credential storage, topology/knowledge persistence and peer synchronization.

### ChatGPT code gateway

The `octendra-code` service runs a NAS-side OAuth/MCP gateway plus outbound agents on selected computers. The gateway never exposes a local operator API directly; each device owns its own credential and permitted roots.

Current capabilities include:

- Version-bound workspaces, bounded code listing/search/reads and syntax-tree symbol ranges.
- Optimistic, journaled multi-file text edits with conflict detection.
- **Complete change review** that can include non-ignored untracked text/binary files instead of returning an empty Git diff.
- Durable terminals with stable output cursors and status replication into `operation_get`.
- Durable inbound/outbound file transfer, checkpoint recovery, explicit resume/cancel and SHA-256 verification.
- **Manifested one-way file-set synchronization (`files_sync`)**: preview a file set, transfer only changed payloads, preserve per-file target versions, and resume the same manifest without overwriting concurrent edits.
- **Workspace event handoff**: bounded per-workspace state transitions with replay cursors, plus terminal/transfer summaries for reconnecting conversations.
- **Maintenance drain protocol** for upgrades: stop admitting new execution while status/control/result delivery remains available; existing work is allowed to drain instead of being killed.
- Stable structured diagnostics with an error code, stage, outcome and recovery action rather than opaque `tool_failed` text.
- **Durable change sets and safe recovery**: multi-file edits retain before/after versions and `change_resume` continues only files whose state is still provably safe.
- **Explicit workspace GC**: preview is bound to a candidate fingerprint before apply; active work and small idempotency records remain protected.
- **Negotiated large-file capability**: the default stays 64 MiB, while 0.7+ agents can explicitly negotiate requests up to 256 MiB with a 256 MiB local free-space reserve. Source authorization reports available / expired / required states.
- Capability/schema fingerprints so clients can detect that their cached tool catalog is stale.
- Optional compact text responses while structured content remains complete.

The server currently advertises 27 code-gateway tools. A ChatGPT conversation can still expose fewer tools when the host keeps an older approved schema snapshot; server capability and host-visible capability are intentionally reported separately.

## Architecture

```mermaid
flowchart LR
  U[ChatGPT / Codex / Operator] -->|OAuth + MCP| G[NAS code gateway]
  G <-->|outbound HTTPS polling| M1[MacBook agent]
  G <-->|outbound HTTPS polling| M2[Mac Studio agent]
  M1 --> F1[Local workspaces / terminals / files]
  M2 --> F2[Local workspaces / terminals / files]

  U -->|MCP / CLI| C[Octendra control plane]
  C --> X[Reusable SSH connectors]
  X --> H[Servers / NAS / workstations]
```

The classic SSH control plane and the code gateway are related but independent. They do not share execution state or silently substitute one device for another.

## Quick start

### macOS service

```bash
scripts/octendra-service install
scripts/octendra-service status
```

Open the local console at `http://127.0.0.1:8787/admin`.

Common operations:

```bash
scripts/octendra-service update
scripts/octendra-service restart
scripts/octendra-service logs
scripts/octendra-service ui
scripts/octendra-service skills
```

### Windows

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\octendra-service.ps1 Install
.\octendra-service.ps1 Status
```

See [Windows Installation and Operations](docs/windows.md) for Task Scheduler services, paths and troubleshooting.

### ChatGPT code gateway

Start with [ChatGPT Code Gateway](docs/chatgpt-code-gateway.md). The normal flow is:

1. `devices_list` to choose an explicitly authorized device.
2. `workspace_open` once, then reuse that device-bound workspace.
3. Search only when the path/range is unknown; batch known reads.
4. Apply version-checked edits and inspect the complete change review.
5. Run tests in a durable terminal and observe the original operation instead of rerunning it.
6. Use durable file transfer or `files_sync` for artifacts and multi-file deliveries.
7. Use `workspace_context` and event cursors to continue after reconnecting.

## Reliability model

Octendra treats an acknowledgement, an execution result, an installation, and an acceptance result as different facts.

- Mutating requests use stable idempotency keys and durable local records.
- After an uncertain crash, arbitrary commands are **not** replayed automatically.
- File transfer recovery reuses the original operation and verified checkpoint identity.
- Multi-file writes are per-file atomic; a partial batch reports exactly what was applied and never promises a fake filesystem-wide transaction.
- Release inputs are fingerprinted in primary main; tests and artifacts must match that unchanged input identity.
- Gateway upgrades happen before agents when protocol compatibility requires it.
- Agent upgrades use a maintenance lease, stable readiness checks and per-target acceptance. One busy device must not prevent a healthy peer from completing its own release path.

## Current release

The current Gateway/Agent and Android source version is **0.12.0**. Source verification, built artifacts, and deployed runtimes are separate acceptance stages. See [this naming change](docs/octendra-branding-20261020.md) for delivery status; historical release evidence remains in `docs/releases/`.

## Development and release

The workspace pins Rust `1.99.1`.

```bash
cargo fmt --all
cargo test --workspace
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Code-gateway releases execute in primary main with a fixed input fingerprint and the established `~/rust-target`:

```bash
python3 scripts/release-code.py --report target/<iteration>/pipeline.json
```

Do not restart an unknown build or publication action merely because its observer timed out. Inspect the original report/operation first.

## Repository layout

```text
crates/
  octendra-domain       Shared entities and state types
  octendra-core         Policy, supervision, redaction and transport traits
  octendra-vault        Encrypted local credentials
  octendra-db           SQLx migrations and repositories
  octendra-connector    SSH transports, pools, PTYs and workers
  octendra-api          HTTP API and admin console
  octendra-mcp          Main MCP server and request schemas
  octendra-sync         Peer synchronization
  octendra-cli          Service/admin CLI
  octendra-code         ChatGPT code gateway and local agent
migrations/                 Database schema migrations
scripts/                    Install, upgrade, validation and release automation
skills/                     Repository-owned agent skills
docs/                       Architecture, operations, releases and product backlog
```

## Documentation

Start from [Documentation Index](docs/README.md). Important references:

- [Architecture and Runtime](docs/architecture-and-runtime.md)
- [Deployment and Operations](docs/deployment-and-operations.md)
- [ChatGPT Code Gateway](docs/chatgpt-code-gateway.md)
- [Product Backlog](docs/product/BACKLOG.md)
- [Product Workflow](docs/product/README.md)
- [0.5.0 Roadmap](docs/product/ROADMAP-0.5.0.md)
- [0.7.1 Release Evidence](docs/releases/0.7.1/RELEASE.md)
- [Windows Installation and Operations](docs/windows.md)

## Security summary

Credentials are encrypted locally and plaintext secrets are never returned by credential tools. Code gateway OAuth scopes and device permissions are checked independently. Workspace file tools are capability-confined to authorized roots, while terminal execution intentionally has the local user's OS authority and therefore is **not** a filesystem sandbox. Output redaction is best effort, not a substitute for keeping secrets out of commands. File URLs are short-lived bearer capabilities and should be treated as private.

Octendra does not bypass ChatGPT or operating-system safety/permission decisions. Host-visible tool approval, OS elevation, Full Disk Access and similar controls remain external boundaries.
