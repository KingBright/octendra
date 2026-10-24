//! Window-only Windows worker policy, shared with deterministic backend tests.
//! No global keyboard/mouse operations. One complete click maps to one UIA action.
use octendra_desktop_protocol::*;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowAuthority {
    pub owner_sid: String,
    pub session_id: u32,
    pub process_id: u32,
    pub process_created_100ns: u64,
    pub window_id: u64,
    pub scope_key: String,
    pub scope_value: u64,
}
impl WindowAuthority {
    pub fn desktop_id(&self) -> String {
        format!(
            "windows:{}:{}:{}:{}",
            self.session_id, self.process_id, self.process_created_100ns, self.window_id
        )
    }
}
/// OWNER RIGHTS denotes this object's current owner, not CREATOR OWNER or an
/// arbitrary principal. Resolve it only after establishing our object ownership.
pub(super) fn file_principal_trusted(owner: &str, current: &str, principal: &str) -> bool {
    owner == current
        && (principal == current || matches!(principal, "S-1-5-18" | "S-1-5-32-544" | "S-1-3-4"))
}
pub(super) fn file_grant_allowed(trusted: bool, private: bool, mask: u32) -> bool {
    let write_mask =
        0x10000000 | 0x40000000 | 0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x10000 | 0x40000 | 0x80000;
    trusted || (!private && mask & write_mask == 0)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Activation {
    Invoke,
    Toggle,
    Select,
}
/// Stable UIA control-type IDs: button, checkbox, list item and radio button.
/// A custom control exposing an arbitrary pattern does not enter this allowlist.
pub(super) fn activation(control_type: i32) -> Option<Activation> {
    match control_type {
        50000 => Some(Activation::Invoke),
        50002 => Some(Activation::Toggle),
        50007 | 50013 => Some(Activation::Select),
        _ => None,
    }
}
pub(super) fn approval_budget(expires: u64, now: u64) -> Option<Duration> {
    let remaining = expires.checked_sub(now)?;
    (remaining > 0 && remaining <= u64::from(MAX_LEASE_MS))
        .then_some(Duration::from_millis(remaining))
}
pub(super) fn valid_input(live: bool, captured: Instant, deadline: Instant, now: Instant) -> bool {
    live && now < deadline
        && now
            .checked_duration_since(captured)
            .is_some_and(|age| age < Duration::from_secs(1))
}
pub(super) fn bounded_texture(width: u32, height: u32, maximum: usize) -> bool {
    width > 0
        && height > 0
        && width <= 8192
        && height <= 8192
        && (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .is_some_and(|n| n <= maximum && n <= MAX_WORKER_PIXELS)
}

pub(super) struct Pixels {
    pub metadata: RawPixels,
    pub bytes: Vec<u8>,
    pub captured: Instant,
}
pub(super) trait Backend {
    fn live(&self) -> bool;
    fn open(&mut self) -> Result<(), ErrorCode>;
    fn capture(&mut self, maximum: usize) -> Result<Pixels, ErrorCode>;
    /// Prepare a supported UIA pattern, then recheck deadline/freshness immediately
    /// before its one invocation. A failed invocation has an unknown effect.
    fn invoke(
        &mut self,
        point: SourcePoint,
        captured: Instant,
        deadline: Instant,
    ) -> Result<(), ErrorCode>;
    fn close(&mut self) -> bool;
}

pub(super) fn click(frame: &FrameRef, actions: &[Action]) -> Result<SourcePoint, ErrorCode> {
    match actions {
        [
            Action::PointerMove { x, y },
            Action::ButtonDown {
                button: Button::Left,
            },
            Action::ButtonUp {
                button: Button::Left,
            },
        ] => frame.mapping.map(*x, *y),
        _ => Err(ErrorCode::InvalidActions),
    }
}

struct Epoch {
    binding: Binding,
    deadline: Instant,
    latest: Option<(FrameRef, Instant)>,
    frame_sequence: u64,
    sequence: u64,
}
pub(super) struct Worker<B: Backend> {
    backend: B,
    identity: Identity,
    authorization_deadline: Instant,
    size: [u32; 2],
    position: [i32; 2],
    maximum: usize,
    epoch: Option<Epoch>,
    spent: bool,
    poisoned: bool,
}
impl<B: Backend> Worker<B> {
    pub fn new(
        backend: B,
        identity: Identity,
        authorization_deadline: Instant,
        size: [u32; 2],
        position: [i32; 2],
        maximum: usize,
    ) -> Self {
        Self {
            backend,
            identity,
            authorization_deadline,
            size,
            position,
            maximum,
            epoch: None,
            spent: false,
            poisoned: false,
        }
    }
    pub fn budget(&self) -> Duration {
        self.epoch
            .as_ref()
            .map_or(self.authorization_deadline, |e| e.deadline)
            .saturating_duration_since(Instant::now())
    }
    pub fn shutdown(&mut self) -> bool {
        self.epoch = None;
        self.spent = true;
        self.backend.close()
    }
    fn checked(&self, binding: &Binding) -> Result<&Epoch, ErrorCode> {
        let epoch = self.epoch.as_ref().ok_or(ErrorCode::SessionClosed)?;
        if epoch.binding != *binding {
            return Err(ErrorCode::IdentityMismatch);
        }
        if Instant::now() >= epoch.deadline {
            return Err(ErrorCode::SessionClosed);
        }
        if !self.backend.live() {
            return Err(ErrorCode::StaleFrame);
        }
        Ok(epoch)
    }
    pub fn handle(&mut self, request: Request) -> (Result<Response, ErrorCode>, Option<Pixels>) {
        let result = self.perform(request);
        match result {
            Ok((reply, pixels)) => (Ok(reply), pixels),
            Err(error) => {
                // Once an effect may have been sent, this process cannot admit a
                // fresh epoch. The Agent journal fences the original operation.
                if error == ErrorCode::OutcomeUnknown {
                    self.poisoned = true;
                }
                if matches!(
                    error,
                    ErrorCode::OutcomeUnknown | ErrorCode::SessionClosed | ErrorCode::StaleFrame
                ) && !self.shutdown()
                {
                    self.poisoned = true;
                }
                (
                    Err(if self.poisoned {
                        ErrorCode::OutcomeUnknown
                    } else {
                        error
                    }),
                    None,
                )
            }
        }
    }
    fn perform(&mut self, request: Request) -> Result<(Response, Option<Pixels>), ErrorCode> {
        if self.poisoned {
            return Err(ErrorCode::OutcomeUnknown);
        }
        if matches!(request, Request::Capabilities {}) {
            return Ok((
                Response::Capabilities {
                    capabilities: Capabilities {
                        available: !self.spent && self.backend.live() && !self.budget().is_zero(),
                        backend: "windows_wgc_uia_standard_control_click_only".into(),
                        max_actions: 3,
                        ..Capabilities::default()
                    },
                },
                None,
            ));
        }
        if Instant::now() >= self.authorization_deadline {
            return Err(ErrorCode::SessionClosed);
        }
        match request {
            Request::Open { identity, lease_ms } => {
                if identity != self.identity {
                    return Err(ErrorCode::IdentityMismatch);
                }
                if self.spent || self.epoch.is_some() {
                    return Err(ErrorCode::DesktopBusy);
                }
                if lease_ms == 0 || lease_ms > MAX_LEASE_MS {
                    return Err(ErrorCode::InvalidRequest);
                }
                if !self.backend.live() {
                    return Err(ErrorCode::Unavailable);
                }
                self.backend.open()?;
                let deadline = (Instant::now() + Duration::from_millis(u64::from(lease_ms)))
                    .min(self.authorization_deadline);
                if !self.backend.live() || Instant::now() >= deadline {
                    return Err(ErrorCode::SessionClosed);
                }
                let binding = Binding {
                    identity,
                    epoch: Uuid::new_v4(),
                };
                let expires_at_ms = crate::now_ms() as u64
                    + deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis() as u64;
                self.epoch = Some(Epoch {
                    binding: binding.clone(),
                    deadline,
                    latest: None,
                    frame_sequence: 0,
                    sequence: 0,
                });
                Ok((
                    Response::Opened {
                        binding,
                        expires_at_ms,
                    },
                    None,
                ))
            }
            Request::Observe { binding } => {
                self.checked(&binding)?;
                let pixels = self.backend.capture(self.maximum)?;
                self.checked(&binding)?;
                let [w, h] = pixels.metadata.size;
                let length = (w as usize)
                    .checked_mul(h as usize)
                    .and_then(|n| n.checked_mul(4));
                if w == 0
                    || h == 0
                    || w > 8192
                    || h > 8192
                    || length != Some(pixels.bytes.len())
                    || pixels.bytes.len() > self.maximum
                    || pixels.metadata.encoding != "rgba8"
                    || pixels.metadata.color != [0; 4]
                    || pixels.metadata.plane_lengths != [pixels.bytes.len()]
                    || pixels.metadata.plane_sha256 != [crate::hash(&pixels.bytes)]
                {
                    return Err(ErrorCode::Unavailable);
                }
                let epoch = self.epoch.as_mut().ok_or(ErrorCode::SessionClosed)?;
                epoch.frame_sequence = epoch
                    .frame_sequence
                    .checked_add(1)
                    .ok_or(ErrorCode::SessionClosed)?;
                let frame = FrameRef {
                    binding,
                    source_revision: 1,
                    frame_sequence: epoch.frame_sequence,
                    mapping: FrameMapping {
                        frame_size: [w, h],
                        content: ContentRect {
                            left: 0,
                            top: 0,
                            width: w,
                            height: h,
                        },
                        logical_size: self.size,
                        desktop_position: self.position,
                        rotation: Rotation::R0,
                    },
                };
                epoch.latest = Some((frame.clone(), pixels.captured));
                Ok((Response::Observed { frame }, Some(pixels)))
            }
            Request::Act {
                frame,
                sequence,
                actions,
            } => {
                let epoch = self.checked(&frame.binding)?;
                if sequence
                    != epoch
                        .sequence
                        .checked_add(1)
                        .ok_or(ErrorCode::InvalidSequence)?
                {
                    return Err(ErrorCode::InvalidSequence);
                }
                let (latest, captured) = epoch.latest.as_ref().ok_or(ErrorCode::StaleFrame)?;
                if latest != &frame || !valid_input(true, *captured, epoch.deadline, Instant::now())
                {
                    return Err(ErrorCode::StaleFrame);
                }
                let point = click(&frame, &actions)?;
                let captured = *captured;
                let deadline = epoch.deadline;
                // Invalidate the frame before the first effect. Another Act must
                // observe again, even with a new operation/sequence.
                self.epoch.as_mut().ok_or(ErrorCode::SessionClosed)?.latest = None;
                self.backend.invoke(point, captured, deadline)?;
                self.epoch
                    .as_mut()
                    .ok_or(ErrorCode::SessionClosed)?
                    .sequence = sequence;
                Ok((
                    Response::Accepted {
                        sequence,
                        accepted_actions: actions.len(),
                        effect_confirmed: false,
                    },
                    None,
                ))
            }
            Request::Close { binding } => {
                self.checked(&binding)?;
                if !self.shutdown() {
                    return Err(ErrorCode::OutcomeUnknown);
                }
                Ok((Response::Closed {}, None))
            }
            Request::Capabilities {} => unreachable!(),
        }
    }
}
impl<B: Backend> Drop for Worker<B> {
    fn drop(&mut self) {
        self.backend.close();
    }
}

#[cfg(test)]
mod tests;
