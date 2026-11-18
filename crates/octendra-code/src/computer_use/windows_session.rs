//! Window-only Windows worker policy, shared with deterministic backend tests.
//! No global keyboard/mouse operations. Click, Fill, and Scroll map to bounded UIA patterns.
use octendra_desktop_protocol::*;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[cfg_attr(not(windows), allow(dead_code))]
pub(super) const MAX_UIA_DEPTH: usize = 20;
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) const MAX_UIA_ELEMENTS: usize = 200;
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) const MAX_UIA_VISITS: usize = 1500;
#[cfg_attr(not(windows), allow(dead_code))]
pub(super) const MAX_UIA_DURATION: Duration = Duration::from_millis(350);

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
/// WGC/DWM and UIA share physical pixels. Do not apply the monitor scale again.
pub(super) fn physical_point(
    position: [i32; 2],
    size: [u32; 2],
    point: SourcePoint,
) -> Option<[i32; 2]> {
    let mut result = [0; 2];
    for (axis, coordinate) in [point.x, point.y].into_iter().enumerate() {
        if size[axis] == 0
            || !coordinate.is_finite()
            || coordinate < 0.0
            || coordinate > f64::from(size[axis] - 1)
        {
            return None;
        }
        let offset = i64::from(position[axis]);
        let pixel = offset.checked_add(coordinate.round() as i64)?;
        result[axis] = i32::try_from(pixel).ok()?;
    }
    Some(result)
}

pub(super) struct HitNode {
    pub control_type: i32,
    pub process_id: u32,
    pub enabled: bool,
    pub offscreen: bool,
    pub bounds: [i32; 4],
}
pub(super) fn bounds_contain(bounds: [i32; 4], point: [i32; 2]) -> bool {
    let [left, top, right, bottom] = bounds;
    left <= point[0] && point[0] < right && top <= point[1] && point[1] < bottom
}
pub(super) fn is_value_control(control_type: i32) -> bool {
    matches!(control_type, 50003 | 50004 | 50030)
}
/// The caller proves the raw ancestry ends at the pinned window. Text/content
/// descendants may resolve to their nearest standard control, never past it.
pub(super) fn standard_control_ancestor(
    nodes: &[HitNode],
    root_reached: bool,
    process_id: u32,
    point: [i32; 2],
) -> Option<(usize, Activation)> {
    if !root_reached
        || nodes.is_empty()
        || nodes.len() > 64
        || nodes.iter().any(|node| node.process_id != process_id)
    {
        return None;
    }
    for (index, node) in nodes.iter().enumerate() {
        if let Some(operation) = activation(node.control_type) {
            return (node.enabled && !node.offscreen && bounds_contain(node.bounds, point))
                .then_some((index, operation));
        }
    }
    None
}
/// Nearest editable control ancestor (`Edit`, `ComboBox`, `Document`) below the pinned root.
pub(super) fn value_control_ancestor(
    nodes: &[HitNode],
    root_reached: bool,
    process_id: u32,
    point: [i32; 2],
) -> Option<usize> {
    if !root_reached
        || nodes.is_empty()
        || nodes.len() > 64
        || nodes.iter().any(|node| node.process_id != process_id)
    {
        return None;
    }
    for (index, node) in nodes.iter().enumerate() {
        if is_value_control(node.control_type) {
            return (node.enabled && !node.offscreen && bounds_contain(node.bounds, point))
                .then_some(index);
        }
    }
    None
}
/// Nearest scrollable container ancestor below/including the pinned root, verifying same process.
pub(super) fn scrollable_control_ancestor(
    nodes: &[HitNode],
    root_reached: bool,
    process_id: u32,
    point: [i32; 2],
    scrollable: &[bool],
) -> Option<usize> {
    if !root_reached
        || nodes.is_empty()
        || nodes.len() != scrollable.len()
        || nodes.len() > 64
        || nodes.iter().any(|node| node.process_id != process_id)
    {
        return None;
    }
    for (index, node) in nodes.iter().enumerate() {
        if scrollable[index] {
            return (node.enabled && !node.offscreen && bounds_contain(node.bounds, point))
                .then_some(index);
        }
    }
    None
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ScrollStep {
    NoAmount,
    SmallDecrement,
    LargeDecrement,
    SmallIncrement,
    LargeIncrement,
}
pub(super) fn scroll_steps(horizontal: i16, vertical: i16) -> (ScrollStep, ScrollStep) {
    let step = |delta: i16| match delta {
        0 => ScrollStep::NoAmount,
        1..=200 => ScrollStep::SmallDecrement,
        201.. => ScrollStep::LargeDecrement,
        -200..=-1 => ScrollStep::SmallIncrement,
        ..=-201 => ScrollStep::LargeIncrement,
    };
    (step(horizontal), step(vertical))
}
pub(super) fn window_bounds(
    position: [i32; 2],
    size: [u32; 2],
    rect: [i32; 4],
) -> Option<[u32; 4]> {
    if size[0] == 0 || size[1] == 0 || rect[2] <= rect[0] || rect[3] <= rect[1] {
        return None;
    }
    let lx0 = (i64::from(rect[0]) - i64::from(position[0])).clamp(0, i64::from(size[0]));
    let ly0 = (i64::from(rect[1]) - i64::from(position[1])).clamp(0, i64::from(size[1]));
    let lx1 = (i64::from(rect[2]) - i64::from(position[0])).clamp(0, i64::from(size[0]));
    let ly1 = (i64::from(rect[3]) - i64::from(position[1])).clamp(0, i64::from(size[1]));
    (lx1 > lx0 && ly1 > ly0).then_some([lx0 as u32, ly0 as u32, lx1 as u32, ly1 as u32])
}
pub(super) fn uia_role_name(control_type: i32) -> &'static str {
    match control_type {
        50000 => "Button",
        50002 => "CheckBox",
        50003 => "ComboBox",
        50004 => "Edit",
        50005 => "Hyperlink",
        50006 => "Image",
        50007 => "ListItem",
        50008 => "List",
        50009 => "Menu",
        50010 => "MenuBar",
        50011 => "MenuItem",
        50013 => "RadioButton",
        50014 => "ScrollBar",
        50015 => "Slider",
        50016 => "Spinner",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50023 => "Tree",
        50024 => "TreeItem",
        50025 => "Custom",
        50026 => "Group",
        50028 => "DataGrid",
        50030 => "Document",
        50031 => "SplitButton",
        50032 => "Window",
        50033 => "Pane",
        50036 => "DataItem",
        50037 => "TitleBar",
        50040 => "AppBar",
        _ => "Control",
    }
}
pub(super) fn uia_is_interactive(control_type: i32) -> bool {
    matches!(
        control_type,
        50000
            | 50002
            | 50003
            | 50004
            | 50005
            | 50007
            | 50011
            | 50013
            | 50015
            | 50016
            | 50019
            | 50024
            | 50030
            | 50031
            | 50036
    )
}
pub(super) fn uia_is_title_or_text(control_type: i32) -> bool {
    matches!(control_type, 50006 | 50020 | 50032 | 50037)
}
pub(super) fn redact_uia_value(is_password: bool, raw: Option<String>) -> Option<String> {
    if is_password {
        Some("<redacted>".to_string())
    } else {
        raw.filter(|s| !s.is_empty())
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
    fn elements(&self) -> Vec<ui::SemanticElement> {
        Vec::new()
    }
    /// Prepare a supported UIA pattern, then recheck deadline/freshness immediately
    /// before its one invocation. A failed invocation has an unknown effect.
    fn invoke(
        &mut self,
        point: SourcePoint,
        captured: Instant,
        deadline: Instant,
    ) -> Result<(), ErrorCode>;
    fn set_value(
        &mut self,
        _point: SourcePoint,
        _text: &str,
        _captured: Instant,
        _deadline: Instant,
    ) -> Result<(), ErrorCode> {
        Err(ErrorCode::InvalidActions)
    }
    fn scroll(
        &mut self,
        _point: SourcePoint,
        _horizontal: i16,
        _vertical: i16,
        _captured: Instant,
        _deadline: Instant,
    ) -> Result<(), ErrorCode> {
        Err(ErrorCode::InvalidActions)
    }
    fn close(&mut self) -> bool;
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum WindowsCommand {
    Click {
        point: SourcePoint,
    },
    Fill {
        point: SourcePoint,
        text: String,
    },
    Scroll {
        point: SourcePoint,
        horizontal: i16,
        vertical: i16,
    },
}

pub(super) fn parse_actions(
    frame: &FrameRef,
    actions: &[Action],
) -> Result<WindowsCommand, ErrorCode> {
    match actions {
        [
            Action::PointerMove { x, y },
            Action::ButtonDown {
                button: Button::Left,
            },
            Action::ButtonUp {
                button: Button::Left,
            },
        ] => Ok(WindowsCommand::Click {
            point: frame.mapping.map(*x, *y)?,
        }),
        [
            Action::PointerMove { x, y },
            Action::ButtonDown {
                button: Button::Left,
            },
            Action::ButtonUp {
                button: Button::Left,
            },
            Action::KeyDown { usage: mod_down },
            Action::KeyDown { usage: 4 },
            Action::KeyUp { usage: 4 },
            Action::KeyUp { usage: mod_up },
            Action::Unicode {
                text,
                key_down: true,
            },
            Action::Unicode {
                text: release,
                key_down: false,
            },
        ] if mod_down == mod_up
            && matches!(mod_down, 0xe0 | 0xe4 | 0xe3)
            && !text.is_empty()
            && text.len() <= 8192
            && !text.contains('\0')
            && release.is_empty() =>
        {
            Ok(WindowsCommand::Fill {
                point: frame.mapping.map(*x, *y)?,
                text: text.clone(),
            })
        }
        [
            Action::PointerMove { x, y },
            Action::ButtonDown {
                button: Button::Left,
            },
            Action::ButtonUp {
                button: Button::Left,
            },
            Action::KeyDown { usage: mod_down },
            Action::KeyDown { usage: 4 },
            Action::KeyUp { usage: 4 },
            Action::KeyUp { usage: mod_up },
            Action::KeyDown { usage: 42 },
            Action::KeyUp { usage: 42 },
        ] if mod_down == mod_up && matches!(mod_down, 0xe0 | 0xe4 | 0xe3) => {
            Ok(WindowsCommand::Fill {
                point: frame.mapping.map(*x, *y)?,
                text: String::new(),
            })
        }
        [
            Action::PointerMove { x, y },
            Action::Unicode {
                text,
                key_down: true,
            },
            Action::Unicode {
                text: release,
                key_down: false,
            },
        ] if text.len() <= 8192 && !text.contains('\0') && release.is_empty() => {
            Ok(WindowsCommand::Fill {
                point: frame.mapping.map(*x, *y)?,
                text: text.clone(),
            })
        }
        [
            Action::PointerMove { x, y },
            Action::Wheel {
                horizontal,
                vertical,
            },
        ] if (*horizontal != 0 || *vertical != 0)
            && horizontal.unsigned_abs() <= 1200
            && vertical.unsigned_abs() <= 1200 =>
        {
            Ok(WindowsCommand::Scroll {
                point: frame.mapping.map(*x, *y)?,
                horizontal: *horizontal,
                vertical: *vertical,
            })
        }
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
    pub fn handle(
        &mut self,
        request: Request,
    ) -> (
        Result<Response, ErrorCode>,
        Option<Pixels>,
        Vec<ui::SemanticElement>,
    ) {
        let result = self.perform(request);
        match result {
            Ok((reply, pixels, elements)) => (Ok(reply), pixels, elements),
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
                    Vec::new(),
                )
            }
        }
    }
    fn perform(
        &mut self,
        request: Request,
    ) -> Result<(Response, Option<Pixels>, Vec<ui::SemanticElement>), ErrorCode> {
        if self.poisoned {
            return Err(ErrorCode::OutcomeUnknown);
        }
        if matches!(request, Request::Capabilities {}) {
            return Ok((
                Response::Capabilities {
                    capabilities: Capabilities {
                        available: !self.spent && self.backend.live() && !self.budget().is_zero(),
                        backend: "windows_wgc_uia_standard_control_click_only".into(),
                        max_actions: MAX_ACTIONS,
                        ..Capabilities::default()
                    },
                },
                None,
                Vec::new(),
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
                    Vec::new(),
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
                let elements = self.backend.elements();
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
                Ok((Response::Observed { frame }, Some(pixels), elements))
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
                let command = parse_actions(&frame, &actions)?;
                let captured = *captured;
                let deadline = epoch.deadline;
                // Invalidate the frame before the first effect. Another Act must
                // observe again, even with a new operation/sequence.
                self.epoch.as_mut().ok_or(ErrorCode::SessionClosed)?.latest = None;
                match command {
                    WindowsCommand::Click { point } => {
                        self.backend.invoke(point, captured, deadline)?
                    }
                    WindowsCommand::Fill { point, text } => {
                        self.backend.set_value(point, &text, captured, deadline)?
                    }
                    WindowsCommand::Scroll {
                        point,
                        horizontal,
                        vertical,
                    } => self
                        .backend
                        .scroll(point, horizontal, vertical, captured, deadline)?,
                }
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
                    Vec::new(),
                ))
            }
            Request::Close { binding } => {
                self.checked(&binding)?;
                if !self.shutdown() {
                    return Err(ErrorCode::OutcomeUnknown);
                }
                Ok((Response::Closed {}, None, Vec::new()))
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
