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

The independent `desktop_observer` catalog entry uses the existing typed desktop
protocol, limited to Capabilities/Open/Observe/Close. Both the account and device
must explicitly have `desktop:observe`; `desktop:input` is not required. Admission,
queue delivery, result observation and `desktop_frame` export recheck this read
permission. Older Agents without `desktop_observer_v1` cannot receive the new
entry. Catalog discovery describes the tool independently of authorization.

The observer accepts only a locally approved Linux window launch with input
false and zero desktop geometry. It refuses an input-capable or Weavelo launch.
Act is excluded from its schema and rejected again before transport state can
change. The original `desktop_worker` consent pair and control checks remain
unchanged. These tests do not install a production approval, grant scopes or
perform Portal capture. A code-only device remains unable to observe.

## Independent worker build

`scripts/build-linux-desktop-worker.py` builds only the standalone worker and its
unit executable from primary main, using the existing `~/rust-target`. It accepts
an existing signed Debian trixie amd64 development SDK and its independently
pinned manifest SHA-256. Its GNU target preserves the deployed worker ABI
(`libpipewire-0.3.so.0`); the glibc floor is 2.41. The Agent/CLI musl release is a
separate component and is not rebuilt by this entry.

The command requires `--rp-root`, `--sdk-manifest`, `--sdk-sha256`, and a new
`--report`. Observe that same report using `--status`; never start another request
to check progress. The existing project lease and public current transaction own
`dist/octendra-linux-desktop-worker-current` and its current bundle. No alternate
source checkout or Cargo target directory is created. The bundle contains the
worker, a disabled approval example, the uninstalled desktop-entry template,
checksums and the source/build manifest. Linux tests are cross-compiled; target
execution and actual Portal selection are reported separately.
