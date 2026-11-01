# One UI interface for computers and phones

The next platform implementation and separate Octendra / Weavelo tasks are
documented in [the platform plan](ui-platform-implementation-plan.md). That plan
describes proposed work; the behavior below describes the current implementation.

Use the same two tools on every device:

```json
{"tool":"ui_observe","arguments":{"device":"MacBook-M2-Max"}}
{"tool":"ui_observe","arguments":{"device":"Android-Phone"}}
{"tool":"ui_act","arguments":{"device":"Android-Phone","observation_id":"<returned UUID>","action":{"type":"click","target":{"ref":"n12"}}}}
```

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
are scoped to the authenticated owner/device/view. An uncertain action is not
replayed; inspect its original `operation_id` through `operation_get` and
observe the UI. An input acknowledgement does not prove business success.

## Android background view

```json
{"tool":"ui_observe","arguments":{"device":"Android-Phone","view":"background"}}
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
