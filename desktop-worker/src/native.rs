//! One short-lived desktop epoch. RP owns Portal, restricted FD, frames and held input.
//! No RP service, network, clipboard, GUI, global input or second task journal starts.
use crate::config::{APP_ID, Approval, now_ms};
use host::linux_frame::{OwnedFrame, PixelFormat, Transform};
use host::{
    PortalSourceKind, PreparedPortalCapture, prepare_pipewire_capture_bounded,
    request_registered_portal_control_capture,
};
use protocol::InputEvent;
use octendra_desktop_protocol::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use tokio::sync::watch;
use uuid::Uuid;

pub struct Reply {
    pub response: Response,
    pub pixels: Option<OwnedFrame>,
}
#[path = "../../crates/octendra-desktop-protocol/src/hold.rs"]
pub mod hold;
use hold::{Control, HoldError, HoldGuard};

struct Session {
    binding: Binding,
    capture: PreparedPortalCapture,
    cancel: watch::Sender<bool>,
    deadline: Instant,
    expires_at_ms: u64,
    last: Option<FrameRef>,
    observed_at: Option<Instant>,
    sequence: u64,
    holds: HoldGuard,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<Button>,
}
pub struct Worker {
    approval: Option<Approval>,
    session: Option<Session>,
    spent: bool,
    unknown: bool,
    request_id: Option<Uuid>,
}
impl Worker {
    pub fn new(approval: Option<Approval>) -> Self {
        Self {
            approval,
            session: None,
            spent: false,
            unknown: false,
            request_id: None,
        }
    }
    pub fn cancelled(&mut self) {
        self.unknown = true;
    }
    pub fn set_request(&mut self, id: Uuid) {
        self.request_id = Some(id);
    }
    pub fn wait_budget(&self) -> Duration {
        self.session.as_ref().map_or(Duration::from_secs(5), |s| {
            let deadline = s
                .holds
                .expires_at()
                .map_or(s.deadline, |at| at.min(s.deadline));
            s.holds.budget(deadline, Instant::now())
        })
    }
    pub fn handling_budget(&self) -> Duration {
        if self.session.is_none() {
            Duration::from_secs(25)
        } else {
            self.wait_budget()
        }
    }
    pub async fn shutdown(&mut self) -> Result<(), ErrorCode> {
        let result = crate::lifecycle::shutdown(self.request_id, self.shutdown_inner()).await;
        if result.is_err() {
            self.unknown = true;
        }
        result
    }
    async fn shutdown_inner(&mut self) -> Result<(), ErrorCode> {
        self.spent = true;
        let unresolved = self.unknown;
        if let Some(mut session) = self.session.take() {
            session.holds.stop();
            session.cancel.send_replace(true);
            session.capture.close_owned().await.map_err(|_| {
                self.unknown = true;
                ErrorCode::OutcomeUnknown
            })?;
        }
        if unresolved {
            Err(ErrorCode::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    pub async fn handle(&mut self, request: Request) -> Result<Reply, ErrorCode> {
        if matches!(request, Request::Capabilities {}) {
            // Gateway production capability remains unavailable until integration + live acceptance.
            return Ok(Reply {
                response: Response::Capabilities {
                    capabilities: Capabilities::default(),
                },
                pixels: None,
            });
        }
        if self.unknown {
            return Err(ErrorCode::OutcomeUnknown);
        }
        let approval = self
            .approval
            .as_ref()
            .filter(|a| a.enabled)
            .ok_or(ErrorCode::Unavailable)?
            .clone();
        if approval.expires_at_ms <= now_ms() || approval.verify_desktop().is_err() {
            self.shutdown().await?;
            return Err(ErrorCode::Unavailable);
        }
        if request.identity() != Some(&approval.identity) {
            return Err(ErrorCode::IdentityMismatch);
        }
        if self.session.as_ref().is_some_and(|s| {
            s.holds.budget(s.deadline, Instant::now()).is_zero() || s.capture.is_revoked()
        }) {
            self.shutdown().await?;
            return Err(ErrorCode::SessionClosed);
        }
        match request {
            Request::Open { identity, lease_ms } => {
                if self.spent || self.session.is_some() {
                    return Err(ErrorCode::DesktopBusy);
                }
                if lease_ms == 0 || lease_ms > MAX_LEASE_MS {
                    return Err(ErrorCode::InvalidRequest);
                }
                crate::lifecycle::phase(self.request_id, "approval_verified");
                crate::lifecycle::phase(self.request_id, "open_received");
                self.spent = true; // failed/cancelled Open never reopens on this process.
                let (cancel, rx) = watch::channel(false);
                crate::lifecycle::phase(self.request_id, "portal_request_started");
                let mut lease = request_registered_portal_control_capture(APP_ID, "", rx)
                    .await
                    .map_err(|_| ErrorCode::OutcomeUnknown)?;
                crate::lifecycle::phase(self.request_id, "portal_session_ready");
                let selected = lease.selected_source();
                if selected.kind != PortalSourceKind::Monitor
                    || selected.logical_size
                        != Some((
                            approval.logical_size[0] as i32,
                            approval.logical_size[1] as i32,
                        ))
                    || selected.logical_position
                        != Some((approval.logical_position[0], approval.logical_position[1]))
                {
                    if !lease.close().await {
                        return Err(ErrorCode::OutcomeUnknown);
                    }
                    return Err(ErrorCode::IdentityMismatch);
                }
                let capture = prepare_pipewire_capture_bounded(
                    lease,
                    approval.max_frame_bytes,
                    2 * approval.max_frame_bytes,
                )
                .await
                .map_err(|_| ErrorCode::OutcomeUnknown)?;
                crate::lifecycle::phase(self.request_id, "capture_attached");
                if !capture.input_available() {
                    capture.close_owned().await.map_err(|_| {
                        self.unknown = true;
                        ErrorCode::OutcomeUnknown
                    })?;
                    return Err(ErrorCode::Unavailable);
                }
                let remaining = approval
                    .expires_at_ms
                    .saturating_sub(now_ms())
                    .min(u64::from(lease_ms));
                if remaining == 0 {
                    capture.close_owned().await.map_err(|_| {
                        self.unknown = true;
                        ErrorCode::OutcomeUnknown
                    })?;
                    return Err(ErrorCode::SessionClosed);
                }
                let binding = Binding {
                    identity,
                    epoch: Uuid::new_v4(),
                };
                let expires_at_ms = now_ms().saturating_add(remaining);
                self.session = Some(Session {
                    binding: binding.clone(),
                    capture,
                    cancel,
                    deadline: Instant::now() + Duration::from_millis(remaining),
                    expires_at_ms,
                    last: None,
                    observed_at: None,
                    sequence: 0,
                    holds: HoldGuard::default(),
                    keys: BTreeSet::new(),
                    buttons: BTreeSet::new(),
                });
                Ok(Reply {
                    response: Response::Opened {
                        binding,
                        expires_at_ms,
                    },
                    pixels: None,
                })
            }
            Request::Observe { binding } => {
                let session = self.bound(&binding)?;
                let frame = session
                    .capture
                    .observe_owned_frame(Duration::from_secs(2), approval.max_frame_bytes)
                    .await
                    .map_err(|_| ErrorCode::Unavailable)?;
                if Instant::now() >= session.deadline || session.capture.is_revoked() {
                    self.shutdown().await?;
                    return Err(ErrorCode::SessionClosed);
                }
                // This adapter admits only full-source identity frames. No guessed crop/rotation map.
                if frame.source_format.crop.is_some_and(|c| {
                    c.x != 0
                        || c.y != 0
                        || c.width != frame.source_format.width
                        || c.height != frame.source_format.height
                }) || frame.source_format.transform != Transform::Identity
                    || !matches!(
                        frame.source_format.pixel_format,
                        PixelFormat::Nv12 | PixelFormat::I420
                    )
                    || !session.capture.input_available()
                {
                    self.shutdown().await?;
                    return Err(ErrorCode::StaleFrame);
                }
                if session.last.as_ref().is_some_and(|last| {
                    last.source_revision != frame.stamp.format_revision
                        || last.mapping.frame_size != [frame.width, frame.height]
                }) {
                    self.shutdown().await?;
                    return Err(ErrorCode::StaleFrame);
                }
                let frame_sequence = session
                    .last
                    .as_ref()
                    .map_or(1, |last| last.frame_sequence + 1);
                let reference = FrameRef {
                    binding,
                    source_revision: frame.stamp.format_revision,
                    frame_sequence,
                    mapping: FrameMapping {
                        frame_size: [frame.width, frame.height],
                        content: ContentRect {
                            left: 0,
                            top: 0,
                            width: frame.width,
                            height: frame.height,
                        },
                        logical_size: approval.logical_size,
                        desktop_position: approval.logical_position,
                        rotation: Rotation::R0,
                    },
                };
                session.last = Some(reference.clone());
                session.observed_at = Some(Instant::now());
                Ok(Reply {
                    response: Response::Observed { frame: reference },
                    pixels: Some(frame),
                })
            }
            Request::Act {
                frame,
                sequence,
                actions,
            } => {
                let session = self.bound(&frame.binding)?;
                if session
                    .observed_at
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(1))
                    || session.last.as_ref() != Some(&frame)
                {
                    return Err(ErrorCode::StaleFrame);
                }
                if sequence
                    != session
                        .sequence
                        .checked_add(1)
                        .ok_or(ErrorCode::InvalidSequence)?
                {
                    return Err(ErrorCode::InvalidSequence);
                }
                let (events, keys, buttons) =
                    convert(&frame.mapping, &actions, &session.keys, &session.buttons)?;
                let mut renewals = Vec::new();
                for action in &actions {
                    match action {
                        Action::KeyDown { usage } => renewals.push(Control::Key(*usage)),
                        Action::ButtonDown { button } => {
                            renewals.push(Control::Button(*button as u8))
                        }
                        Action::PointerMove { .. } => renewals
                            .extend(session.buttons.iter().map(|b| Control::Button(*b as u8))),
                        _ => {}
                    }
                }
                // A state-only renewal still needs a current, live capture revision.
                if session.capture.is_revoked()
                    || session
                        .capture
                        .check_input_frame(frame.source_revision, frame.mapping.frame_size)
                        .is_err()
                {
                    self.shutdown().await?;
                    return Err(ErrorCode::StaleFrame);
                }
                session
                    .holds
                    .renew(sequence, session.deadline, Instant::now(), &renewals)
                    .map_err(|e| match e {
                        HoldError::InvalidSequence => ErrorCode::InvalidSequence,
                        HoldError::Expired => ErrorCode::SessionClosed,
                    })?;
                session.sequence = sequence; // consume before any side effect or uncertainty.
                for (action, event) in events {
                    if session
                        .holds
                        .budget(session.deadline, Instant::now())
                        .is_zero()
                        || session
                            .observed_at
                            .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
                        || session.capture.is_revoked()
                        || session
                            .capture
                            .check_input_frame(frame.source_revision, frame.mapping.frame_size)
                            .is_err()
                        || !matches!(
                            tokio::time::timeout(
                                session
                                    .holds
                                    .budget(session.deadline, Instant::now())
                                    .min(Duration::from_millis(750)),
                                session.capture.inject_input(event)
                            )
                            .await,
                            Ok(Ok(()))
                        )
                    {
                        self.shutdown().await?;
                        return Err(ErrorCode::OutcomeUnknown);
                    }
                    // Cleanup follows actual posted state, not the unposted tail of a batch.
                    match action {
                        Action::KeyDown { usage } => {
                            session.keys.insert(usage);
                            session.holds.posted(Control::Key(usage));
                        }
                        Action::KeyUp { usage } => {
                            session.keys.remove(&usage);
                            session.holds.released(Control::Key(usage));
                        }
                        Action::ButtonDown { button } => {
                            session.buttons.insert(button);
                            session.holds.posted(Control::Button(button as u8));
                        }
                        Action::ButtonUp { button } => {
                            session.buttons.remove(&button);
                            session.holds.released(Control::Button(button as u8));
                        }
                        _ => {}
                    }
                }
                session.keys = keys;
                session.buttons = buttons;
                Ok(Reply {
                    response: Response::Accepted {
                        sequence,
                        accepted_actions: actions.len(),
                        effect_confirmed: false,
                    },
                    pixels: None,
                })
            }
            Request::Close { binding } => {
                self.bound(&binding)?;
                self.shutdown().await?;
                Ok(Reply {
                    response: Response::Closed {},
                    pixels: None,
                })
            }
            Request::Capabilities {} => unreachable!(),
        }
    }
    fn bound(&mut self, binding: &Binding) -> Result<&mut Session, ErrorCode> {
        let s = self.session.as_mut().ok_or(ErrorCode::SessionClosed)?;
        if &s.binding != binding {
            return Err(ErrorCode::IdentityMismatch);
        }
        Ok(s)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        self.capture.revoke();
    }
}
pub fn plane_hashes(frame: &OwnedFrame) -> Vec<String> {
    frame
        .planes
        .iter()
        .map(|p| format!("{:x}", Sha256::digest(p)))
        .collect()
}
type Converted = (Vec<(Action, InputEvent)>, BTreeSet<u16>, BTreeSet<Button>);
fn convert(
    mapping: &FrameMapping,
    actions: &[Action],
    held_keys: &BTreeSet<u16>,
    held_buttons: &BTreeSet<Button>,
) -> Result<Converted, ErrorCode> {
    if actions.is_empty() || actions.len() > MAX_ACTIONS {
        return Err(ErrorCode::InvalidActions);
    }
    let mut keys = held_keys.clone();
    let mut buttons = held_buttons.clone();
    let mut events = Vec::new();
    for action in actions {
        let event = match *action {
            Action::PointerMove { x, y } => {
                let point = mapping.map(x, y)?;
                let norm = |value: f64, size: u32| {
                    if size <= 1 {
                        0
                    } else {
                        (value / f64::from(size - 1) * 65535.0).round() as u16
                    }
                };
                InputEvent::MouseMoveAbsolute {
                    x: norm(point.x, mapping.logical_size[0]),
                    y: norm(point.y, mapping.logical_size[1]),
                }
            }
            Action::KeyDown { usage } => {
                let code = hid_to_rp(usage)?;
                if !keys.insert(usage) {
                    continue;
                }
                InputEvent::KeyDown(code)
            }
            Action::KeyUp { usage } => {
                let code = hid_to_rp(usage)?;
                if !keys.remove(&usage) {
                    return Err(ErrorCode::InvalidActions);
                }
                InputEvent::KeyUp(code)
            }
            Action::ButtonDown { button } => {
                if !buttons.insert(button) {
                    continue;
                }
                InputEvent::MouseDown(button_code(button))
            }
            Action::ButtonUp { button } => {
                if !buttons.remove(&button) {
                    return Err(ErrorCode::InvalidActions);
                }
                InputEvent::MouseUp(button_code(button))
            }
            Action::Wheel {
                horizontal,
                vertical,
            } => InputEvent::MouseScroll {
                delta_x: i32::from(horizontal),
                delta_y: i32::from(vertical),
            },
        };
        events.push((action.clone(), event));
    }
    Ok((events, keys, buttons))
}
fn button_code(button: Button) -> u8 {
    match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
    }
}
/// Physical USB HID usage -> RP's existing mac virtual key -> Portal evdev.
// Unsupported usages fail before any action; no layout/text/global injector fallback.
fn hid_to_rp(usage: u16) -> Result<u32, ErrorCode> {
    const LETTERS: [u32; 26] = [
        0x00, 0x0b, 0x08, 0x02, 0x0e, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2e, 0x2d, 0x1f,
        0x23, 0x0c, 0x0f, 0x01, 0x11, 0x20, 0x09, 0x0d, 0x07, 0x10, 0x06,
    ];
    const DIGITS: [u32; 10] = [0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1a, 0x1c, 0x19, 0x1d];
    const FUNCTION: [u32; 12] = [
        0x7a, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6d, 0x67, 0x6f,
    ];
    Ok(match usage {
        4..=29 => LETTERS[usize::from(usage - 4)],
        30..=39 => DIGITS[usize::from(usage - 30)],
        58..=69 => FUNCTION[usize::from(usage - 58)],
        40 => 0x24,
        41 => 0x35,
        42 => 0x33,
        43 => 0x30,
        44 => 0x31,
        79 => 0x7c,
        80 => 0x7b,
        81 => 0x7d,
        82 => 0x7e,
        224 => 0x3b,
        225 => 0x38,
        226 => 0x3a,
        227 => 0x37,
        228 => 0x3e,
        229 => 0x3c,
        230 => 0x3d,
        _ => return Err(ErrorCode::InvalidActions),
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn mapping() -> FrameMapping {
        FrameMapping {
            frame_size: [200, 100],
            content: ContentRect {
                left: 0,
                top: 0,
                width: 200,
                height: 100,
            },
            logical_size: [100, 50],
            desktop_position: [1000, 200],
            rotation: Rotation::R0,
        }
    }
    #[test]
    fn edge_is_source_local() {
        let (events, _, _) = convert(
            &mapping(),
            &[Action::PointerMove { x: 199.0, y: 99.0 }],
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(
            events
                .into_iter()
                .map(|(_, event)| event)
                .collect::<Vec<_>>(),
            vec![InputEvent::MouseMoveAbsolute { x: 65535, y: 65535 }]
        );
    }
    #[test]
    fn explicit_chord_preserves_modifier_order() {
        let actions = [
            Action::KeyDown { usage: 224 },
            Action::KeyDown { usage: 4 },
            Action::KeyUp { usage: 4 },
            Action::KeyUp { usage: 224 },
        ];
        let (events, keys, _) =
            convert(&mapping(), &actions, &BTreeSet::new(), &BTreeSet::new()).unwrap();
        assert_eq!(
            events
                .into_iter()
                .map(|(_, event)| event)
                .collect::<Vec<_>>(),
            vec![
                InputEvent::KeyDown(0x3b),
                InputEvent::KeyDown(0),
                InputEvent::KeyUp(0),
                InputEvent::KeyUp(0x3b)
            ]
        );
        assert!(keys.is_empty());
    }
    #[test]
    fn bad_tail_rejects_whole_batch() {
        assert!(
            convert(
                &mapping(),
                &[
                    Action::KeyDown { usage: 4 },
                    Action::KeyDown { usage: 65535 }
                ],
                &BTreeSet::new(),
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            convert(
                &mapping(),
                &[Action::KeyUp { usage: 4 }],
                &BTreeSet::new(),
                &BTreeSet::new()
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn default_never_opens_portal() {
        let mut worker = Worker::new(None);
        let identity = Identity {
            owner: "test".into(),
            task_id: Uuid::nil(),
            device_id: Uuid::nil(),
            desktop_id: "offline".into(),
        };
        assert!(matches!(
            worker
                .handle(Request::Open {
                    identity,
                    lease_ms: 1000
                })
                .await,
            Err(ErrorCode::Unavailable)
        ));
        let r = worker.handle(Request::Capabilities {}).await.unwrap();
        assert!(
            matches!(r.response, Response::Capabilities{capabilities} if !capabilities.available)
        );
    }
}
