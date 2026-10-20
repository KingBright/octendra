# Windows Installation and Operations

Octendra runs natively on 64-bit Windows 10/11 and Windows Server. The release is cross-compiled
to the MSVC target from macOS or Linux with Rust and `cargo-xwin`; a Windows build machine, Visual
Studio, NSSM, and WinSW are not required.

## Install a Release

Extract the Windows ZIP, open Windows PowerShell in the extracted directory, and run:

```powershell
Set-ExecutionPolicy -Scope Process Bypass
.\octendra-service.ps1 Install
.\octendra-service.ps1 Status
```

Open <http://127.0.0.1:8787/admin> after both tasks report `Running`.

The installer does not require an administrator shell. It creates two current-user Task Scheduler
tasks that start at logon and restart after failures:

- `Octendra API`
- `Octendra Connector`

Current-user tasks are deliberate. The connector keeps access to the same SSH Agent, private keys,
`known_hosts`, user profile, and encrypted local vault used by Codex or Antigravity. A LocalSystem
Windows service would have a different identity and would make those credentials harder to use.

## Lifecycle

Run the installed manager at `%LOCALAPPDATA%\Octendra\bin\octendra-service.ps1`, or use the
copy in a newly extracted release for `Update`:

```powershell
.\octendra-service.ps1 Stage
.\octendra-service.ps1 Update
.\octendra-service.ps1 Restart
.\octendra-service.ps1 Start
.\octendra-service.ps1 Stop
.\octendra-service.ps1 Status
.\octendra-service.ps1 Logs
.\octendra-service.ps1 Ui
.\octendra-service.ps1 Skills
.\octendra-service.ps1 Doctor
.\octendra-service.ps1 OptimizeStorage
.\octendra-service.ps1 PrintConfig
```

`Stage` installs the new executable into a versioned release directory while the old executable is
still running. `Update` stages first, then asks the Rust CLI to verify that no operation, PTY input,
live PTY, or write lease would be interrupted. It switches the scheduled tasks only after that
drain gate passes. `-Force` is available only for an intentional interruption.

`OptimizeStorage` explicitly activates compressed writes, moves legacy PTY and command output into
compact Postcard plus Zstandard segments, then vacuums SQLite. A routine update keeps writing the
legacy-compatible format until this command runs, so resident old MCP children retain output. Run
it only after updating the API and connector and reloading every MCP child onto the same release. It
refuses migration while conversation work is active unless `-Force` is explicitly supplied; it does
not restart either scheduled task.

The UI remains a separate file. `Ui` replaces it atomically and does not restart the API or
connector. `Uninstall` removes the scheduled tasks but preserves local data; add `-PurgeData` only
when the database, vault key, artifacts, logs, and installed releases should also be deleted.

## Local Paths

| Purpose | Default path |
| --- | --- |
| Root | `%LOCALAPPDATA%\Octendra` |
| Configuration | `%LOCALAPPDATA%\Octendra\config\service.json` |
| Vault master key | `%LOCALAPPDATA%\Octendra\config\vault-master-password` |
| SQLite database | `%LOCALAPPDATA%\Octendra\data\octendra.sqlite` |
| Output artifacts | `%LOCALAPPDATA%\Octendra\data\artifacts` |
| Hot-swappable UI | `%LOCALAPPDATA%\Octendra\data\ui\admin.html` |
| Versioned binaries | `%LOCALAPPDATA%\Octendra\releases` |
| Stable MCP launcher | `%LOCALAPPDATA%\Octendra\bin\octendra-launcher.exe` |
| Logs | `%LOCALAPPDATA%\Octendra\logs` |
| Service manager | `%LOCALAPPDATA%\Octendra\bin\octendra-service.ps1` |

The generated vault key is protected with a current-user ACL. Host passwords and private keys stay
inside the encrypted database and are never written into `service.json`.
SQLx receives Windows database paths as `sqlite://C:/...`; the installer normalizes backslashes and
does not add the Unix-only third slash used by absolute macOS paths.

## Direct Instance Sync

`service.json` keeps `PeerSyncBind` empty by default, so the full API stays on
`127.0.0.1:8787`. To accept direct synchronization from one approved Octendra peer, set
`PeerSyncBind` to an otherwise unused address such as `0.0.0.0:8788`, then run `Update` during a
drained maintenance window. This second listener exposes only health, identity, and authenticated
instance-sync routes; it does not expose the UI, credential inspection or management routes,
remote execution, files, PTYs, or topology administration. User-authorized SSH credentials sent
through those routes are peer-sealed and re-encrypted by the receiving Windows vault.

Use a private LAN, VPN, or SSH tunnel. A routed or public listener must be protected by HTTPS.

## Agent Integration

`Install`, `Update`, and `Skills` synchronize the bundled Skill into:

- `%USERPROFILE%\.codex\skills\octendra-agent`
- `%USERPROFILE%\.gemini\config\skills\octendra-agent`

Run `PrintConfig` to display the exact `mcp-stdio` command and paths for the current installation.
The MCP configuration points to the small stable native launcher, which reads
`config\current-binary.txt` and starts the selected version with inherited stdio. New conversations
therefore pick up a staged release without editing client configuration or keeping a PowerShell
proxy alive for every MCP process.
The API and connector are background tasks; MCP stdio remains a child owned by the agent client so
each conversation receives its own Agent Session while all conversations share the connector's
bounded SSH transport pool.

## SSH Behavior on Windows

The Windows connector uses the native Rust `russh` backend. It supports pooled exec channels,
PowerShell, native PTYs, SFTP upload/download, output artifacts, password fallback, and bounded
public-key bootstrap. It can authenticate through the Windows OpenSSH Agent named pipe, Pageant,
default user keys, or encrypted Octendra credentials.

The Unix-only OpenSSH `ControlMaster` compatibility backend is intentionally unavailable. Selecting
`--ssh-backend openssh` on Windows returns an explicit platform error instead of silently falling
back or spawning unmanaged SSH processes.

## Cross-Compile on macOS or Linux

Install the current toolchain and build the package:

```bash
rustup target add x86_64-pc-windows-msvc
cargo install cargo-xwin --version 0.23.0 --locked
# macOS
brew install nasm

scripts/build-windows-cross.sh
```

On Debian or Ubuntu, install `nasm` and `zip` from the system package manager. The build script
requires the already installed MSVC target, `cargo-xwin` 0.23.0 or newer and NASM, then compiles
with `--locked --release` and one job in primary main using the established `~/rust-target`.
It replaces the fixed `dist/octendra-windows-x86_64-current` directory, matching `.zip` and
`.zip.sha256` through the shared current transaction after validating package bytes. The archive
contains both native CLI and launcher executables, service manager, external admin UI, release
manifest, Windows guide, and Agent Skill. It records source identity, artifact hashes and the
build receipt; cross-compilation is not Windows runtime or signing acceptance. Alternate
`OCTENDRA_DIST_DIR` paths are refused; old version/commit packages and evidence are preserved.
Failure or the next entry recovers only this transaction's owned, inactive temporary paths.

The MSVC runtime is normally present on supported Windows systems. If Windows reports a missing
runtime DLL, install the current Microsoft Visual C++ Redistributable for x64.

## Troubleshooting

Use `Status` first, then `Logs`. Task Scheduler result `0` means the last process exited normally;
a running service normally shows task state `Running`. The task settings retry a failed process
after one minute and reject duplicate instances.

If an update is refused, let the reported operations, PTYs, input events, or write leases drain and
run `Update` again. Do not create a second connector or delete SQLite rows to bypass the gate.

If authentication behaves differently from an interactive terminal, confirm the scheduled tasks
belong to the same Windows account and that the user's OpenSSH Agent or Pageant session is available
after logon. Stored password credentials remain a valid fallback and can bootstrap a public key when
the target route supports it.
