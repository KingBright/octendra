# Connect and upgrade with fewer steps

## Configure a client

Use a preset instead of finding individual callback URLs:

```sh
octendra-code configure-clients --config /private/gateway.json \
  --client chatgpt --client gemini --client grok --client grok-bot \
  --client codex --client antigravity --probe
```

The command preserves existing registrations and credentials, adds only the selected exact client callbacks/origins, prints the MCP URL and tests discovery and the authentication challenge. Add `--check` for a read-only report. A gateway started with its configuration path loads these settings on the next request. Discovery, successful login and a real tool call are separate reported facts. Multiple authorization windows keep separate cookies, so completing one does not invalidate another.

## Add a device

Use the existing `octendra-code enroll` command against the gateway's private configuration. A running gateway loads valid device registrations without interrupting existing connections. Copy the generated private configuration to the intended device and start its agent. Listener, public URL/owner and storage changes need a normal service restart; incomplete or invalid saves keep the last accepted settings.

For Android, import the JSON with **导入配置并连接**, or use the authorized USB provisioning command in [android-agent.md](android-agent.md). Connecting also enables boot recovery and the existing shell helper; these controls can be disabled individually. No additional task grant is required. OS signing, USB trust and accessibility remain the platform's own capabilities.

## Let AI work directly

With a known device and root, call a code/file/terminal tool directly using `workspace_id="device-name:/absolute/root"` or `device-name:default`. The existing workspace is reused automatically. Use `devices_list` only when the target is unknown. A never-connected device reports `not_reported` compatibility instead of claiming an old protocol.

Invalid arguments return `argument_error` with a field, reason and allowed schema bounds, without echoing argument values. `code_read` accepts byte budgets from four bytes to one MiB; small reads return an exact continuation cursor.

## Upgrade all targets

Use the verified local bundle and the existing fleet command:

```sh
python3 scripts/fleet-upgrade.py --version RELEASE_VERSION \
  --package dist/octendra-code-current --config /private/deployment.json \
  --report-dir /private/original-deployment
```

The same invocation waits up to one day for offline desktop nodes and launches each returning node once using the original package. `--wait-online-seconds` changes that observation period; zero finishes the current online scope. An uncertain mutation stays attached to its original receipt. macOS continues to use the signed one-shot `launch-code-upgrade.py --start` dispatcher. There is no repeating updater service.

The live fleet report includes Android's separate signed APK lane. Formal desktop and Android builds embed the same source fingerprint; convergence compares it alongside the version. A missing build identity remains unknown. Build success and cross-compilation do not establish real device acceptance.
