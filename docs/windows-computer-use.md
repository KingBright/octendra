# Windows computer use

This backend belongs to the Octendra Code Agent. The human-operated Weavelo
remote desktop remains a separate service and protocol. Its Windows FFmpeg
capture and desktop input injector are not a window-scoped Octendra backend.
Changing a Gateway URL does not add or replace platform implementations.

The Windows Agent uses the existing `desktop_worker` and `desktop_frame` tools,
operation journal, owner/task/device binding, short epoch, frame hashes and
artifact transport. Its private worker is the same verified Agent executable,
started with fixed arguments and parent-owned pipes. It has no shell or network
entry and never enrolls a device or creates its own desktop authorization.

## Supported window and input

WGC captures one pinned HWND. An explicitly prepared window-lifetime marker
rejects handle reuse; Agent startup never creates that marker. The existing
`desktop-window-target` inspection remains read-only. The separate
`desktop-window-authority PID HWND PRIVATE_FILE` preparation command writes only
the selected window's lifetime metadata and must not run before window approval;
it does not enroll a device or write `desktop-launch.json`.

The target must belong to the current user and
interactive session, have one visible window in its process, remain unminimized,
and retain its creation identity and physical bounds. Locked, elevated, foreign,
changed, unsupported and over-budget sources fail closed. Capture indicators and
Windows restrictions remain unchanged. Pixel dimensions must match DWM bounds;
there is no whole-desktop, screenshot-command or focus-changing fallback.

The backend name is `windows_wgc_uia_standard_control_click_only`. Its supported
`Act` is one complete three-action gesture:

1. `pointer_move` selects a point in the latest frame.
2. `button_down` selects the left button.
3. `button_up` completes the left-button gesture.

This is explicitly a semantic standard-control activation, not raw cursor motion
or held input. UI Automation resolves the point, verifies the element belongs to
the approved window, then uses Invoke for Button, Toggle for CheckBox, or
SelectionItem for RadioButton/ListItem. Other UIA control types are rejected even
when they advertise an activation pattern.
Unsupported/custom controls fail before any invocation. Arbitrary keys, typing,
dragging, wheels, other buttons and mixed gestures are rejected. No `SendInput`,
`SetFocus`, window activation, integrity bypass or global key cleanup is used.
`effect_confirmed=false` requires observing the result; returning from a UIA call
does not prove the intended application outcome.

An `Act` consumes its frame before invocation. Another action needs a new frame.
Stale frames, changed geometry, expired leases and changed identities reject
input. A possibly sent action that fails or loses its reply remains
`outcome_unknown`; neither a new connection nor a repeated operation replays it.
The parent closes or terminates only its owned worker on timeout/disconnection.

## Authorization and validation

The backend is disabled without a valid private `desktop-launch.json`. Windows
authority binds owner SID, session, process creation time, HWND, Agent/worker
hashes, frame budget and the existing capability identity/version. Files reject
reparse paths, foreign ownership and untrusted granting ACLs. Only newly created
worker/frame/lock files receive private ACLs; existing ACLs are not repaired.

Deployment does not enable `desktop:observe` or `desktop:input`. Both the device
authorization and explicitly selected window capability must be approved before
live capture/input acceptance. Existing maintenance entries and credentials stay
separate. A successful cross-build or recording-backend test is not Windows
desktop acceptance.

Fixed tests in `computer_use/windows_session/tests.rs` run the actual session
policy against a recording backend: read pixels, semantic gesture dispatch,
identity/ACL denial, unsupported actions, stale frames/sequences, timeout,
cancellation/disconnection, unknown effects and prevention of duplicate input.
The existing transport/lifecycle/delivery tests cover durable receipts and worker
cleanup. Native Windows compilation and authorized target execution are separate
gates; record them separately in the release evidence.

Frame freshness uses WGC's compositor timestamp in the
[QPC time domain](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.direct3d11captureframe.systemrelativetime).
Queued frames older than the Observe admission are discarded. Hash/COM startup
consumes the original approval deadline instead of restarting its clock.
