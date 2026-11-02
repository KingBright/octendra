# One UI interface for computers and phones

The next platform implementation and separate Octendra / Weavelo tasks are
documented in [the platform plan](ui-platform-implementation-plan.md). That plan
describes proposed work; the behavior below describes the current implementation.

Use the same two tools on every device:

```json
{"tool":"ui_observe","arguments":{"device":"Mac Studio AI workspace"}}
{"tool":"ui_observe","arguments":{"device":"Android-Phone","context":"host"}}
{"tool":"ui_act","arguments":{"device":"Android-Phone","observation_id":"<returned UUID>","action":{"type":"click","target":{"ref":"n12"}}}}
```

The gateway defaults to the selected device's configured persistent VM workspace.
`devices_list.ui_workspaces` lists its name, actual host and execution device,
stable context identity and current Agent connection state. The workspace name,
host name or guest name resolve to that same configured guest. Missing workspaces
return `ui_workspace_unavailable`; they never select the host or another device.
Explicit `context:"host"` on `ui_observe` selects an existing host session,
including the current Android display adapter. This mode is reported as
`host_session` and is not full VM isolation. `ui_act` inherits the observation's
execution context; it cannot switch contexts. Normal and recovered results keep
the original `execution_context` and actual execution device.

The gateway's optional private `ui_workspaces` configuration contains `id` (a
UUID identifying this persistent guest), `name`, `host_device_id` and
`execution_device_id`. Both devices must already be enrolled, and must differ.
Change the context UUID when replacing a VM, not when reconnecting it. A changed
binding rejects new input from the old observation. Exact admitted retries still
observe the original operation. These bindings do not create, start or attest a
VM; guest provisioning and real graphical/input acceptance remain O7 work.

An observation includes an image, its exact width/height, optional semantic
elements, a concrete `view` identity and the backend's supported actions.
Prefer an element `ref`. Coordinate targets use pixels in that returned image:
`{"x":320,"y":240}`. DPI, image scaling, display position and rotation stay
inside the adapter. `ui_act` returns a fresh observation by default. Set
`observe_after:false` only when an additional observation is unnecessary.

The tools reuse the existing connection authority: observation uses `code:read`;
actions use `code:read` plus `terminal:exec` and the Agent's existing execution
setting. No extra desktop OAuth scope or device-registration grant is needed.
Native provider authority and actual OS permissions are still enforced.

The shared action vocabulary is `click`, `double_click`, `type_text`, `fill`,
`scroll`, `drag`, `key`, `launch_app`, plus capability extensions such as
`long_press`, `secondary_click`, `pinch`, `navigate_back`, `go_home` and
`app_switcher`. `type_text` inserts at the current editor selection; `fill`
replaces the whole field, including clearing it with an empty string. Scroll
directions describe content traversal, so `down` moves to later content on both
wheel and touch backends. Keys are logical names such as `Enter` or `ArrowDown`;
optional modifiers are `shift`, `ctrl`, `alt`, `meta`.

Support is reported from the actual backend. macOS native window input supports
click, secondary click, Unicode text, fill, scroll, drag and logical keys.
Windows' existing UIA backend supports its standard-control click; Linux's
portal backend observes. The existing Weavelo source adapter supports bounded
click, scroll and unmodified key gestures. Unsupported actions return an
explicit error and supported alternatives. They never silently turn into a
different gesture.

Native worker identities, short execution leases, sequences and cleanup are
internal. There is no 30-second thinking lease in this interface. A changed
surface/geometry or changed targeted desktop image region returns a fresh
observation without sending input. Current-focus text/keys do not compare
unrelated pixels; explicit app launches use the live view binding and do not
require an unchanged page. Android refs are revalidated against the
actual node and can resolve a label's actionable ancestor. Observation records
on the native desktop adapter are scoped to the authenticated owner/device and
immutable observation ID. It retains up to 128 independent observations per owner/device;
another chat's observation does not overwrite yours. Cache eviction requests a
fresh observation and does not impose a deadline on the Agent's task.
Durable observations also bind the current Agent runtime and native capability
version. Restarting the Agent or changing that provider requires a fresh
observation; an ordinary connection retry does not create a new runtime.
Before native input, the adapter consumes that observation and advances the
source's mutation revision. Earlier observations of that same source require
refreshing; a second read alone does not invalidate them. Embedded/local native
adapters without a gateway workspace binding refuse default isolation requests
before opening a worker; host observation remains an explicit selection.
An uncertain action is not
replayed; inspect its original `operation_id` through `operation_get` and
observe the UI. An input acknowledgement does not prove business success.

## Native macOS workspace maintenance

Apple silicon builds include the system Virtualization backend in
`octendra-code`; no separate hypervisor application is required. The maintenance
entry is `octendra-code ai-workspace probe|status|prepare|run`. `probe` is read-only.
`prepare` installs Apple's supported macOS restore image into one persistent
workspace, and `run` opens its independent console. The default private data
directory is `~/.local/share/octendra/ai-workspace`. Its stable hardware identity,
system disk and application data are reused. A second owner is rejected rather
than starting another VM against the same disk.

The existing macOS signing identity is preserved, with Apple's virtualization
entitlement. Initial account, automatic login and SSH provisioning use the public
macOS 27 API. Generated guest credentials stay in a private local file. No host
directory, clipboard, microphone or audio is attached by default. Closing the
console hides it and leaves the guest running.

On macOS 14+, SIGTERM/SIGINT suspend the VM into its private directory; the next
`run` restores that exact state. A failed restoration reports its error and keeps
the saved state. Older hosts can request guest shutdown; that request alone is
not shutdown completion. `status` labels its runtime record as recorded state,
not live acceptance. This maintenance backend does not enroll a guest Agent or
make `ui_observe`/`ui_act` available inside it: enrollment, graphical input and
host/guest parallel acceptance remain separate work.

## Android background view

```json
{"tool":"ui_observe","arguments":{"device":"Android-Phone","context":"host","view":"background"}}
{"tool":"ui_act","arguments":{"device":"Android-Phone","observation_id":"<returned UUID>","action":{"type":"launch_app","app":"com.android.settings"}}}
```

`active` selects the main phone screen. `background` creates/reuses the owned
AI display through the existing authorized shell helper. Subsequent observations
may select its concrete `display:N` identity. The returned `source` includes
display ID, generation and rotation for media/input integration. Main and
background observations are independent. An empty new display returns
`image_unavailable` until an app renders; its observation can already be used
with `launch_app`. No placeholder pixels are presented as a real capture.
Android 14+ and genuine shell/display
authority are required for a display with its own focus that cannot steal the
main display's top focus. There is no fallback that silently disrupts the main
screen. An app already visible on main stays there; use the main view to operate
it. Separate concurrent copies of arbitrary apps are not promised.

The helper's Binder is handed through Octendra's existing UID-authenticated
local bridge. Only the enrolled app UID can invoke it, and only displays it owns
accept launch/key operations. No public phone listener, second model account,
task approval or per-action business confirmation is added. Real Android
accessibility, shell activation, protected windows and first unlock after boot still
apply. On helper loss, the old display generation is invalid; no action is
replayed on a replacement view.

The low-level display/context integration adapts selected
[Mika v0.1.30 source](https://github.com/wangzan101/Mika/tree/29307104cdbb0c52754ee21da3a7b25ce28f937f).
Its Apache-2.0 license and Mika/scrcpy notices ship in the APK under
`assets/licenses/mika`. Mika's task/model runtime is not embedded.

AI images are bounded sparse snapshots returned as native MCP image blocks,
with base64 omitted from text. The ImageReader drains native frames and copies
pixels only on observation. Weavelo video and high-frequency input remain on
their native real-time transport. Native video fan-out and shared human/AI
ownership remain follow-up Weavelo integration work. This change does not modify
Weavelo while its separate four-device task is running. An MCP snapshot is not a video
presentation or latency measurement.
