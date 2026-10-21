# Octendra: independent parallel installation

Run `parallel-install.py` on the target machine. Default `plan` only validates inputs. `stage` copies files into the new namespace; it does not register/start a service, create state, provision authentication or rewrite config. `activate` explicitly registers/starts only the new service. Activation submission is not readiness or end-to-end acceptance.

Keep the existing binaries, config, databases, services, reverse proxy, OAuth records and client connection as the maintenance channel. `fleet-upgrade.py`, `upgrade-code-*` and existing-instance update helpers are **not** this parallel installation entry.

## Explicit required inputs

- `--public-origin`: the confirmed new HTTPS origin on standard 443, without a path. `--legacy-origin` names the existing origin and must differ. Missing values stop before writes, with no old-origin lookup.
- `--package` and `--manifest-sha256`: verified unpacked bundle and its manifest SHA-256 from the delivery receipt.
- `--root`, `--state-dir`, `--config`: fresh paths containing an `octendra` directory. Maintenance namespaces and existing `octendra-code` roots are rejected. Config is exactly `NEW_ROOT/config/gateway.json` or `agent.json`; state and installed binary must not exist.
- `--new-auth-provisioned`: the config was independently provisioned for the new instance. The installer neither generates nor copies credentials. Unix config mode is 0600 or stricter and owned by the runtime account.

The operator fills known accounts/authorized roots from existing inventory, without broadening scope or asking the user to re-enter known inventory. No phone installation is included.

## Gateway on Linux/NAS

The candidate listener is `127.0.0.1:18788`; an occupied port stops preflight, with no fallback. Availability is an observation, not a reservation. New service is `octendra-gateway.service`, distinct from an existing `octendra-code-gateway.service`.

Example new NAS roots are `/volume1/@entware-opt/octendra` and `/volume1/docker/octendra`. These are examples, not filesystem/discovery defaults. Fill variables from the reviewed profile; leave new origin/auth unset until confirmed:

```sh
: "${NEW_ORIGIN:?set the confirmed new HTTPS origin}"
: "${LEGACY_ORIGIN:?set the known maintenance origin}"
: "${BUNDLE:?set the verified unpacked bundle path}"
: "${MANIFEST_SHA:?set the verified manifest SHA256}"
: "${NEW_ROOT:?set the new binary/config root}"
: "${NEW_STATE:?set the new state directory}"
: "${SERVICE_USER:?set the existing unprivileged account}"
: "${SERVICE_GROUP:?set its existing group}"
python3 "$BUNDLE/parallel-install.py" --role gateway \
  --package "$BUNDLE" --manifest-sha256 "$MANIFEST_SHA" \
  --root "$NEW_ROOT" --config "$NEW_ROOT/config/gateway.json" --state-dir "$NEW_STATE" \
  --public-origin "$NEW_ORIGIN" --legacy-origin "$LEGACY_ORIGIN" --port 18788 \
  --service-user "$SERVICE_USER" --service-group "$SERVICE_GROUP" --new-auth-provisioned
```

That is a read-only plan. Repeat identical arguments with `--action stage` to copy only the new binary and service candidate. After reviewing its receipt, use `--action activate` through the existing authorized administrative channel. Activation exclusively creates the new state leaf and `/etc/systemd/system/octendra-gateway.service`, then loads/starts that unit. It does not edit Caddy, DNS, old units, auth files or system security policy. Parent directories and the unprivileged account must exist. Provision new config with correct owner/traversal permissions; the installer does not change auth-file permissions to fix them.

For a verified existing numeric UID/GID without passwd/group entries, pass those explicit IDs as `--service-user`/`--service-group`. No account is created. The generated unit uses the existing root-owned OS Python with `-I -S` and a fixed launcher that clears supplementary groups, sets GID/UID, then execs the new binary. A user-writable interpreter is rejected. The new private config must already have that UID/GID. This matches the NAS's existing numeric runtime model without changing its old unit or security settings. `systemctl` probes and activation use systemd-219-compatible commands.

## Linux, macOS and Windows Agents

Use a separate per-user root such as `$HOME/.local/share/octendra/agent` or `%LOCALAPPDATA%\octendra\agent`. State is `NEW_ROOT/state`; config is `NEW_ROOT/config/agent.json`. Use the same three actions, starting with this read-only command:

```sh
python3 "$BUNDLE/parallel-install.py" --role agent \
  --package "$BUNDLE" --manifest-sha256 "$MANIFEST_SHA" \
  --root "$NEW_ROOT" --config "$NEW_ROOT/config/agent.json" --state-dir "$NEW_ROOT/state" \
  --public-origin "$NEW_ORIGIN" --legacy-origin "$LEGACY_ORIGIN" --new-auth-provisioned
```

The target installer uses Python 3.8 or later. Windows uses the existing Python launcher (`py -3`) followed by the same script/arguments; it does not install a runtime. No ExecutionPolicy change or Agent elevation. The new **Octendra Code Agent** task uses current-account InteractiveToken/LeastPrivilege and TASK_CREATE, never update/force or an old task/ACL copy. XML quotes paths with spaces. An existing task stops preflight.

PowerShell equivalent (the origin/auth inputs are intentionally not preset):

```powershell
$NewOrigin = ''
$LegacyOrigin = ''
$Bundle = ''
$ManifestSha = ''
if (!$NewOrigin -or !$LegacyOrigin -or !$Bundle -or !$ManifestSha) {
    throw 'Fill the new origin, known maintenance origin and verified bundle receipt first'
}
$NewRoot = Join-Path $env:LOCALAPPDATA 'octendra\agent'
$Common = @('--role','agent','--package',$Bundle,'--manifest-sha256',$ManifestSha,
    '--root',$NewRoot,'--config',(Join-Path $NewRoot 'config\agent.json'),
    '--state-dir',(Join-Path $NewRoot 'state'),'--public-origin',$NewOrigin,
    '--legacy-origin',$LegacyOrigin,'--new-auth-provisioned')
py -3 (Join-Path $Bundle 'parallel-install.py') @Common
# After independent auth provisioning and a successful plan:
# py -3 (Join-Path $Bundle 'parallel-install.py') @Common --action stage
# py -3 (Join-Path $Bundle 'parallel-install.py') @Common --action activate
```

Linux uses the new per-user `octendra-agent.service`. macOS uses **fun.hackerlife.octendra.agent** in the current GUI domain. Existing units/plists/tasks stop installation even when inactive. macOS also requires `--signed-binary` and `--signing-receipt` from the existing signing owner, binding the bundle candidate SHA, signed SHA, certificate SHA-1 and new identifier. The installer verifies that signature; it does not sign, import/export keys, write TCC or grant privacy permissions. Required existing-scope privacy grants remain a supported System Settings operation.

An activation request is saved before registration. Failed/uncertain activation must be inspected, not replayed. The installer performs no old-channel rollback/deletion. Before retiring anything, verify actual new runtime path/version, connection, explicitly selected device, read/edit/exec, upload/download, screen observation and authorized input.

## New authentication is a separate authorized step

These steps are **not executed by plan/stage or this tooling-preparation task**:

1. After the new origin is confirmed, use the verified executable to explicitly run `init-gateway --config NEW_ROOT/config/gateway.json --public-url NEW_ORIGIN --bind 127.0.0.1:18788 --state-dir NEW_STATE --owner NEW_OWNER --password-file NEW_PRIVATE_PASSWORD_FILE` as the intended existing unprivileged service identity. Use new private paths only. It provisions a new owner login without displaying secret values; read its password privately on the host, never in chat/logs. Do not copy the old hash/database. First Gateway installation requires an empty devices list. If that identity is numeric with no account name, use the same fixed privilege-drop operation under the existing authorized root controller: trusted OS Python `-I -S -c 'import os,sys; os.setgroups([]); os.setgid(int(sys.argv[2])); os.setuid(int(sys.argv[1])); os.execv(sys.argv[3],sys.argv[3:])' UID GID VERIFIED_BINARY init-gateway ...`. This authentication step remains separately authorized and is not run by the installer.
2. Configure only exact needed OAuth callbacks: ChatGPT uses `https://chatgpt.com/connector_platform_oauth_redirect`; Gemini Spark uses bounded built-in Google dynamic-registration callback validation. Browser `allowed_origins` are not OAuth callback allowlists. No wildcard callbacks.
3. Explicitly `enroll --gateway-config NEW_GATEWAY_CONFIG --agent-config NEW_PRIVATE_AGENT_CONFIG --name DEVICE_NAME --state-dir NEW_AGENT_STATE --root EXISTING_AUTHORIZED_ROOT --shell EXISTING_SHELL` per device. Add write/exec flags only to match existing authorization. New connection credentials stay in private files/direct transfers, outside the model/chat. Keep the old device/connection identity unchanged. Fresh-instance enrollment is separate from installation preflight.
4. After new HTTPS health/discovery passes, add a **new** Gemini custom app at `NEW_ORIGIN/mcp` and complete the new owner consent in the user's browser; retain the existing app. ChatGPT likewise uses a separate connector/consent. Never reuse old access/refresh tokens. New status, manifest, PNG and MCP/OAuth branding derive their URLs from the configured origin.

## DNS, TLS and reverse proxy

Domain is runtime config; no binary rebuild is needed. The candidate destination is the existing NAS ingress, not Gateway port 18788. The private operator receipt supplies the confirmed public/DDNS target and edge/origin mapping. Do not infer an origin IP from proxied DNS responses.

The new hostname needs its own DNS record to that confirmed ingress, HTTPS/TLS coverage and a separate host route to `http://127.0.0.1:18788` preserving the new Host. If the existing edge uses an origin-port override, the new hostname needs an explicit matching rule. Keep old DNS, host route, backend port and OAuth issuer unchanged. The installer never creates/reloads these. Review exact DNS/TLS/proxy changes when the hostname is known. Until then tooling tests use synthetic fixtures; production origin/auth remain unset.

Use `parallel-new-host.caddy.in` as the reviewed new host-route template. Its hostname and TLS configuration are deliberately unfilled; do not apply it as-is. Sharing the existing TLS listener requires adding/loading a separate new host definition, with old host blocks unchanged; that proxy/DNS/TLS operation is not part of installation. If even loading a new host into the existing proxy is disallowed, a separately approved TLS listener/origin-port mapping is needed instead.

## Reused binary limitation: built-in self-upgrade

The reused 0.12 Gateway and Linux Agent still hardcode the canonical maintenance service names in their built-in upgrade restart paths. That capability is **not parallel-safe yet**. No binaries were rebuilt for this installer task. Fresh installation refuses existing release directories and never seeds a distribution pointer. The supplied new proxy template denies `/admin/gateway-upgrade`, `/admin/release-bundle`, `/device/upgrade-directive` and release artifact downloads. Keep these paths disabled and do not publish a release into the new instance or call its built-in upgrade APIs until runtime service identities are made configurable and verified. Ordinary new connection/read/edit/exec/transfer/screen acceptance is separate from this remaining upgrade capability. The old maintenance channel must stay in place throughout.
