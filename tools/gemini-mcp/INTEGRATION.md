# Independent integration

This directory is a standalone Cargo package under `remote_hosts/tools/gemini-mcp`. Praxis and Telos (via Octendra) are consumers; the server does not call a Praxis daemon, connect to the NAS, read a Praxis task database, or require the surrounding repository at runtime. Copying this directory preserves the complete source package. The native Swift adapter is embedded in the server binary.

## Install and connect

On the Mac running Gemini Desktop, with Rust and Apple command-line tools installed:

```sh
cd /path/to/gemini-mcp
cargo install --locked --path . --root "$HOME/.local" --bin gemini-desktop-mcp --bin gemini-desktop-client
```

Use a caller-owned persistent Cargo target directory for repeated builds. Installing does not start Gemini, grant Accessibility permission, or register any MCP host automatically. The resulting server requires macOS, Gemini Desktop, Apple command-line tools for the embedded Swift adapter, and Accessibility permission for the executing process. The Rust client library itself can be integrated separately from the native server.

Configure any stdio MCP host using `mcp-config.example.json`, replacing the executable path with its actual installation path. The server writes protocol messages to stdout. Do not wrap it in a shell that prints banners to stdout. No extra Praxis/Telos configuration, Python runtime, API key, or AGY connection is required.

For a Rust project, a local source dependency is enough:

```toml
[dependencies]
gemini-desktop-mcp = { path = "/path/to/gemini-mcp" }
```

The crate exports `client::GeminiClient`. A repository dependency can also be pinned to a reviewed commit instead of a moving branch. The crate is not claimed to be published to crates.io. Each application chooses where to persist its own request and operation identifiers.

## Configurable Rust client

```rust,no_run
use gemini_desktop_mcp::client::{ClientConfig, GeminiClient};
# async fn connect() -> anyhow::Result<()> {
let mut config = ClientConfig::new("/absolute/path/to/gemini-desktop-mcp");
config.state_dir = Some("/absolute/private/state/gemini".into());
let client = GeminiClient::connect_with(config).await?;
let usage = client.usage().await?;
println!("{usage}");
client.close().await?;
# Ok(()) }
```

`ClientConfig` also accepts executable arguments and environment overrides. These are local application configuration, not model-supplied shell commands. The library launches the executable directly. A selected state directory is validated by the server; existing directories must already have private permissions.

## Host lifecycle

1. Connect and discover capabilities and current readiness. When a requested action needs the foreground, use explicit `focus(None)` while idle or `focus(Some(operation_id))` for the matching active operation; coordinate desktop ownership with other consumers.
2. Persist a fresh request UUID before submitting. Reuse that UUID only for an identical request after a lost response.
3. Store the returned operation and session IDs. Observe the operation while Gemini runs. A disconnected MCP stream does not cancel an already accepted operation.
4. Resolve an explicit, bound workflow confirmation through the confirmation interface. Account login and macOS permissions remain human actions; a generation-plan confirmation is a different category.
5. Collect only a completed result. Media files are returned as verified local artifacts on the Gemini machine; a remote consumer must explicitly transfer them through its own authorized transport.
6. Use continuation for a new message in the same conversation, `start_after` for a new conversation after an owned predecessor, and resume for recovering observation without resubmission.

`unknown` means the outcome is unresolved, not permission to submit the prompt again. `waiting_human` means the host should present the reason and suspend retries. Preserve operation IDs across host restarts. The package persists native execution records, not the host application's business tasks.

## Multiple consumers

Different personal agents, Telos, and Praxis can launch their own stdio processes. They must share the device-level native exclusion mechanism (`~/Library/Caches/GeminiDesktopMCP/`) because they operate the same Gemini UI. Separate `GEMINI_DESKTOP_MCP_STATE_DIR` directories isolate local records; they do not create parallel Gemini sessions or bypass the global native lease. Keep each state directory absolute, owned by the current user and mode 0700. A busy response should cause the host to defer work, not kill another client or remove a lease.

For a personal agent on another machine, run the server on the Mac that owns Gemini and transport stdio with the host's existing managed remote-execution connection. The server does not expose an unauthenticated network port. Files in collection responses remain local to that Mac. A native HTTP service and remote file download endpoint are not included.

## Usage-aware scheduling

Use the usage tool independently of generation. Treat a snapshot as observations from Gemini Desktop, with source, collection time, upstream update age when available, quota bucket labels, and explicit unknown fields. A fresh scrape can still display cached upstream information. Do not turn a UI timestamp into a guarantee of a fresh server-side balance.

Keep percentage usage, remaining credits and request counts in separate units. Do not infer a number of image/video/research jobs from credits, or a model-specific balance from an unlabeled bucket. Missing data is not zero and not unlimited. Unknown account attribution must not be merged across users. Gemini Desktop and AGY/Antigravity budgets are separate unless their equivalence is independently established.

The consumer owns scheduling policy: defer on an observed exhausted bucket, use an unambiguous reset timestamp when available, and apply its own freshness and unknown-data policy. The MCP adapter reports observations and does not silently spend quota to estimate capacity. No background polling job is installed; hosts choose their refresh cadence.

## Implementation and acceptance

Capability implementation, live validation and current readiness are separate fields. Consult the README status table and native capability response. Adding a selector or parsing a fixture is not proof of real generation or quota accuracy. All functional implementation is being completed before the next unified live acceptance pass, per the requested development sequence.
