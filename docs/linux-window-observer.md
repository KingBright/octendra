# Linux window observation

The native Linux worker has a window-only observation mode. A local launch
approval with `observe=true`, `input=false`, `logical_size=[0,0]` and
`logical_position=[0,0]` selects this mode. It does not enable that approval,
enroll desktop scopes or grant an operating-system permission.

The worker uses the registered `fun.hackerlife.octendra.desktop-worker` identity
and a ScreenCast-only Portal session. It requests exactly one Window source,
no persistence, no restoration, no RemoteDesktop devices, no Clipboard and no
EIS connection. The KDE picker requires the user to choose a window. The same
Portal connection, restricted PipeWire descriptor and capture generation remain
bound to the existing task/device/epoch, frame revisions and durable operations.
Cancellation and unknown outcomes retain the existing cleanup and replay rules.

Returned geometry is a source-local pixel canvas. It does not identify a
compositor position, an independently specified PID or an accessible object.
The selected stream is the authority; window titles and reused node numbers are
not persistent identities. A changed frame size/revision closes the worker and
requires another explicit selection instead of silently mapping another source.

Every Act request is rejected by both the Agent transport and the native worker
before input, hold renewal, sequence consumption or a new child is possible.
Capabilities identify `linux_portal_window_observe_only` with zero actions.
The legacy monitor/control path is preserved and is not used by this mode.

This is not a single-window input backend. KDE RemoteDesktop keyboard and button
events have no target-window argument; stream-relative pointer coordinates do
not establish keyboard focus or a compositor-enforced window boundary.

## Delivery and acceptance boundaries

The standalone worker uses the existing Weavelo host library through explicit
path dependencies at validation/build time. That library's registered window
entry and private D-Bus fixture tests are separate source changes. The Code
Agent's normal Linux cross-build does not build the standalone worker or prove
PipeWire/compositor execution. A native worker build and real user-selected
Open/Observe/Close acceptance remain separate requirements.

The current desktop_worker catalog retains its existing desktop consent
metadata. This implementation does not rewrite account/device scopes or claim
that a code-only connection can observe a screen. No production desktop approval
or persistent Portal permission is installed by source verification or packaging.
