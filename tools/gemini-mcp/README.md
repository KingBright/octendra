# Gemini Desktop MCP (Rust)

Standalone stdio MCP server plus asynchronous Rust client, developed on Mac Studio. Praxis and Telos are consumers via Octendra Host Agent Router; see [independent integration](INTEGRATION.md) for installation, multiple hosts and usage-aware scheduling. The binary embeds the native Swift bridge and installs its content-addressed copy in the private state directory at runtime; no checkout or Python runtime is required. The official Rust SDK is pinned to `rmcp` 3.9.0 and `Cargo.lock` records the dependency graph. There is no network listener.

**Implementation and live acceptance are separate.** The requested workflow is to finish the capability paths first, then run one unified acceptance pass. New paths below are experimental and have not yet been live-validated on Gemini 1.141.3.1394.

| Capability | Implemented path | Existing live evidence |
| --- | --- | --- |
| Text and continuation | Owned submission, observation, recovery and continuation | Two native turns completed; full MCP-to-Gemini live submission acceptance pending |
| Images | Mode selection and owned native export state machine | Earlier request returned a content-access error; no image collected |
| Video | Mode selection and owned native export state machine | Enabled menu observed; generation/export acceptance pending |
| Deep Research | Mode selection, versioned plan confirmation and bound full-report text proof | Unified plan/report acceptance pending |
| Canvas | Mode selection, bound card/panel text extraction and hash proof | Earlier document generated; new collection path acceptance pending |
| Usage | Visible usage snapshot and optional guarded settings refresh | Package labels/RPC strings inspected; current-account extraction acceptance pending |

Earlier deterministic tests covered the original stdio lifecycle and artifact boundaries. This iteration has only been compile-checked; new fixtures are prepared for the later unified run. A pending owned Canvas request from the earlier session still holds the native lease; no new generation is started and that lease is not force-removed. Existing drafts and other desktop tasks are preserved.

The transport is MCP over stdio → Rust operation worker → macOS Accessibility → the ordinary Gemini Desktop conversation. It is not a discovered private Gemini RPC endpoint and does not use AGY, Antigravity, or a Gemini API key. Inside the native observer, AX notifications trigger reinspection with bounded polling as a fallback. The personal agent calls `observe` for durable operation state; there is no Gemini webhook or automatic callback into the agent. After a native observation timeout, `observe` with `resume: true` continues the same request without sending the prompt again.

```sh
cd /Users/jinliang/workspace/octendra/tools/gemini-mcp
cargo build --locked
cargo test --locked
/Users/jinliang/rust-target/debug/gemini-desktop-client \
  /Users/jinliang/rust-target/debug/gemini-desktop-mcp capabilities
```

Point a stdio MCP host at the server binary as shown in `mcp-config.example.json`. Rust personal agents may depend on this crate and use `client::GeminiClient`:

```rust,no_run
use gemini_desktop_mcp::client::GeminiClient;
# async fn example() -> anyhow::Result<()> {
let c = GeminiClient::connect("/absolute/path/gemini-desktop-mcp").await?;
let id = uuid::Uuid::new_v4().to_string();
let operation = c.start("Explain the design", &id).await?;
let operation_id = operation["operation_id"].as_str().unwrap();
let status = c.observe(operation_id, false).await?;
println!("{status}");
c.close().await?;
# Ok(()) }
```

For capability and timeout selection, use typed options (the native capability probe determines availability):

```rust,no_run
# use gemini_desktop_mcp::client::{GeminiClient, StartOptions, Capability};
# async fn example(c: &GeminiClient) -> anyhow::Result<()> {
let options = StartOptions {
    prompt: "Explain the design".into(),
    request_id: uuid::Uuid::new_v4().to_string(),
    capability: Capability::Text,
    timeout_ms: 120_000,
};
// Persist options.request_id before the call so a lost reply can be recovered.
let operation = c.start_with_options(&options).await?;
# Ok(()) }
```

Typed convenience methods cover capabilities, usage, focus, start, observe, continue_turn, confirm, collect and cancel. `ClientConfig` and `connect_with` configure executable arguments, environment and state directory independently of the host application. Confirmations require the exact returned `action_id` and `action_version`; they do not authorize arbitrary dialog clicks. `call` takes structured JSON for optional parameters. CLI usage: `gemini-desktop-client SERVER ACTION '[JSON object]'`. For example `... observe '{"operation_id":"UUID","resume":true}'` explicitly resumes observation of the existing native turn. `continue` submits a *new* prompt to the same verified completed session and accepts a fresh `request_id`; it never replaces resume. `start_after(previous_operation_id, &StartOptions)` starts a fresh session after an explicitly selected owned predecessor; the MCP `gemini_start` equivalent accepts optional `previous_operation_id`. The predecessor must be complete or have an explicitly verified native failure, and the native bridge performs its own binding and UI checks. Persist the new request UUID before calling; a fresh start and continuation cannot share one UUID.

The native capability probe reports actual implementation and readiness for text, image, video, deep research and canvas. The adapter accepts image completion only when the native bridge supplies a verified full-export receipt; support and live readiness must be read from the probe. Image and video completion requires a verified artifact of the matching media kind. Canvas text completes only with native `canvas_document_text` evidence matching the request, session and UTF-8 text hash; this does not claim PDF or native file export. Deep Research text requires a matching `research_report_text` proof, produced only after the native confirmed-plan and report-binding checks. `gemini_confirm` accepts the exact returned research action ID and version once; it cannot approve account or system permissions. Arbitrary filesystem attachments are not implemented. Human authorization, account and quota dialogs must be resolved in Gemini Desktop. A real owned Gemini turn is needed for business acceptance; the adapter does not establish quota attribution. The application must be running and ready with an empty draft, and macOS Accessibility must be granted to the actual native process.

Operation/request/session IDs and the launch intent are persisted before a detached worker starts. Disconnecting stdio does not cancel Gemini or resubmit a turn. A caller-generated `request_id` UUID is required for every start and new continuation; persist it before the call. Reuse it only with identical inputs; a changed request returns conflict. Observe after reconnect; if necessary explicitly resume the same operation. `unknown` is not success or failure, and `cancelled` requires a native receipt. `failed` is terminal only when the bound native receipt explicitly proves a failure; it retains the diagnostic and does not restart or permit continuation. Cancellation may report `worker_already_running` until the native binding exists. The helper owns the shared device-level native lease and same-session continuation verifies the prior response.

`gemini_focus` explicitly brings Gemini to the foreground. With no operation ID it requires an idle app and no active native lease; with `operation_id` it verifies and raises only the matching active bound window. It does not start the app, submit a prompt, or resume generation. A consumer can call it before a foreground-dependent action, while coordinating other desktop work at the host level.

`gemini_usage` accepts optional `refresh` (default false). Read-only observation does not navigate the UI. Explicit refresh can open the native usage settings and invoke Reload only while the device is idle and the relevant controls are uniquely bound; otherwise it returns an unknown/busy reason. It does not infer account entitlement, merge AGY quota or convert credits to generation counts. `captured_at` is a local observation time; `updated_label` preserves the application update indicator and `staleness` remains unknown where no server freshness is established. `remaining_percent` is derived only from a valid observed `percent_used`; credits retain their own unit.

The default state directory is `~/Library/Application Support/GeminiDesktopMCP`; the optional `GEMINI_DESKTOP_MCP_STATE_DIR` must be absolute, user-owned and mode 0700. Prompts, responses and embedded bridge are private state, with records and files mode 0600. `collect` returns the selected completed turn's text and verified native artifact metadata. Each artifact must belong to the operation and reside in its private native artifact directory; file type, size and SHA-256 are checked again on collection. It cannot read arbitrary caller-supplied paths.

For deterministic end-to-end lifecycle verification, run `(umask 077 && cargo test --locked --features test-fixture)`. This builds a separate fixture server with a compile-time fake native backend; it never operates Gemini. The normal server always uses the embedded real bridge.
