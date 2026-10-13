//! Window-bound macOS private-state input PoC on the existing RH pipe.
//! No global event posting, activation, clipboard replacement or focus restoration.
//! PID/window targeting still requires live noninterference acceptance.
use super::*;
mod window;
use anyhow::{Context, Result, ensure};
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType, CGMouseButton, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use foreign_types::ForeignType;
use screencapturekit::{cm::CMSampleBuffer, prelude::*};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::c_void,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::unix::AsyncFd;
use uuid::Uuid;
use window::Target;
type Cf = *const c_void;
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGPreflightPostEventAccess() -> bool;
    fn CGSessionCopyCurrentDictionary() -> Cf;
    fn CGEventSetLocation(event: Cf, location: CGPoint);
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Cf);
    fn CFDictionaryGetValue(dictionary: Cf, key: Cf) -> Cf;
    fn CFGetTypeID(value: Cf) -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Cf) -> bool;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(value: Cf, kind: isize, output: *mut c_void) -> bool;
    fn CFStringCreateWithCString(allocator: Cf, value: *const i8, encoding: u32) -> Cf;
    fn CFEqual(a: Cf, b: Cf) -> bool;
}
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVBufferGetAttachment(buffer: Cf, key: Cf, mode: *mut u32) -> Cf;
    static kCVImageBufferYCbCrMatrixKey: Cf;
    static kCVImageBufferYCbCrMatrix_ITU_R_709_2: Cf;
    static kCVImageBufferColorPrimariesKey: Cf;
    static kCVImageBufferColorPrimaries_ITU_R_709_2: Cf;
    static kCVImageBufferTransferFunctionKey: Cf;
    static kCVImageBufferTransferFunction_ITU_R_709_2: Cf;
    static kCVImageBufferTransferFunction_sRGB: Cf;
}
unsafe extern "C" {
    fn proc_pidpath(pid: i32, path: *mut c_void, size: u32) -> i32;
}

/// Read-only preparation of a current-user, single-window capability identity.
pub fn window_target(pid: i32, window: u32) -> Result<serde_json::Value> {
    let session = Target::identity(pid, window)?;
    let target = Target::parse(&session)?;
    let (size, position) = target.geometry().context("window unavailable")?;
    Ok(
        serde_json::json!({"desktop_uid":uid(),"desktop_session":session,
        "desktop_id":format!("uid:{}:session:{}",uid(),session),"logical_size":size,
        "logical_position":position,"process_id":pid,"window_id":window,
        "process_started_us":target.started_us,"input_scope":"pid_private_state",
        "noninterference_verified":false,"same_application_concurrency_supported":false,
        "platform_status":platform_status()}),
    )
}
pub(super) fn window_scope_ready(session: &str, size: [u32; 2], position: [i32; 2]) -> bool {
    Target::parse(session).ok().and_then(|t| t.geometry()) == Some((size, position))
}

const APP_ID: &str = "io.github.KingBright.RemoteHosts.DesktopWorker";
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |v| v.as_millis() as u64)
}
fn uid() -> u32 {
    unsafe { nix::libc::getuid() }
}
/// Query only this process' system permissions; never modifies TCC or requests access.
pub fn platform_status() -> &'static str {
    if !console_owned() {
        return "macos_console_unavailable";
    }
    if !unsafe { CGPreflightScreenCaptureAccess() } {
        return "macos_screen_recording_required";
    }
    if !unsafe { CGPreflightPostEventAccess() } {
        return "macos_accessibility_required";
    }
    "screencapturekit"
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegistryGetRootEntry(port: u32) -> u32;
    fn IORegistryEntryCreateCFProperty(entry: u32, key: Cf, allocator: Cf, options: u32) -> Cf;
    fn IOObjectRelease(entry: u32) -> i32;
}
fn console_unlocked() -> bool {
    unsafe {
        let root = IORegistryGetRootEntry(0);
        if root == 0 {
            return false;
        }
        let key =
            CFStringCreateWithCString(std::ptr::null(), c"IOConsoleLocked".as_ptr(), 0x08000100);
        if key.is_null() {
            IOObjectRelease(root);
            return false;
        }
        let value = IORegistryEntryCreateCFProperty(root, key, std::ptr::null(), 0);
        CFRelease(key);
        IOObjectRelease(root);
        if value.is_null() {
            return false;
        }
        let unlocked = CFGetTypeID(value) == CFBooleanGetTypeID() && !CFBooleanGetValue(value);
        CFRelease(value);
        unlocked
    }
}

fn console_owned() -> bool {
    unsafe {
        if uid() == 0 || nix::libc::geteuid() != uid() {
            return false;
        }
        let d = CGSessionCopyCurrentDictionary();
        if d.is_null() {
            return false;
        }
        // These public SDK names are CFSTR macros, not exported framework symbols.
        let field = |name: &std::ffi::CStr| {
            let key = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), 0x08000100);
            if key.is_null() {
                return std::ptr::null();
            }
            let value = CFDictionaryGetValue(d, key);
            CFRelease(key);
            value
        };
        let number = field(c"kCGSSessionUserIDKey");
        let on = field(c"kCGSSessionOnConsoleKey");
        let login = field(c"kCGSessionLoginDoneKey");
        let mut user = -1i64;
        let boolean = |v: Cf| !v.is_null() && CFGetTypeID(v) == CFBooleanGetTypeID();
        let valid = !number.is_null()
            && CFGetTypeID(number) == CFNumberGetTypeID()
            && CFNumberGetValue(number, 4, (&mut user as *mut i64).cast())
            && user == i64::from(uid())
            && boolean(on)
            && CFBooleanGetValue(on)
            && boolean(login)
            && CFBooleanGetValue(login)
            && console_unlocked();
        CFRelease(d);
        valid
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    enabled: bool,
    app_id: String,
    desktop_uid: u32,
    desktop_session: String,
    identity: Identity,
    worker_sha256: String,
    agent_sha256: String,
    expires_at_ms: u64,
    logical_size: [u32; 2],
    logical_position: [i32; 2],
    max_frame_bytes: usize,
    allow_input: bool,
}
impl Approval {
    fn load(path: &Path) -> Result<Self> {
        let m = std::fs::symlink_metadata(path)?;
        ensure!(
            path.is_absolute()
                && m.is_file()
                && !m.file_type().is_symlink()
                && m.uid() == uid()
                && m.mode() & 0o777 == 0o600
                && m.len() <= 16384,
            "owned private desktop approval required"
        );
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        let actual = f.metadata()?;
        ensure!(
            actual.ino() == m.ino() && actual.dev() == m.dev(),
            "approval changed"
        );
        let mut b = Vec::new();
        Read::by_ref(&mut f).take(16385).read_to_end(&mut b)?;
        ensure!(b.len() <= 16384, "approval size");
        let a: Self = serde_json::from_slice(&b)?;
        ensure!(
            a.enabled
                && a.allow_input
                && a.app_id == APP_ID
                && a.desktop_uid == uid()
                && Target::parse(&a.desktop_session).is_ok()
                && !a.identity.owner.is_empty()
                && !a.identity.task_id.is_nil()
                && !a.identity.device_id.is_nil()
                && a.identity.desktop_id == format!("uid:{}:session:{}", uid(), a.desktop_session)
                && a.expires_at_ms > now_ms()
                && a.expires_at_ms <= now_ms().saturating_add(25_000 + u64::from(MAX_LEASE_MS))
                && a.max_frame_bytes > 0
                && a.max_frame_bytes <= MAX_WORKER_PIXELS,
            "desktop approval bounds/identity"
        );
        ensure!(
            digest(&std::fs::read(std::env::current_exe()?)?) == a.worker_sha256,
            "worker changed"
        );
        let mut parent = [0u8; 4096];
        let n = unsafe {
            proc_pidpath(
                nix::libc::getppid(),
                parent.as_mut_ptr().cast(),
                parent.len() as u32,
            )
        };
        ensure!(n > 0, "Agent parent unavailable");
        let end = parent
            .iter()
            .position(|v| *v == 0)
            .context("parent executable path")?;
        let parent = Path::new(std::str::from_utf8(&parent[..end])?);
        let m = std::fs::symlink_metadata(parent)?;
        ensure!(
            m.is_file()
                && !m.file_type().is_symlink()
                && m.uid() == uid()
                && m.mode() & 0o022 == 0
                && digest(&std::fs::read(parent)?) == a.agent_sha256,
            "approved Agent parent required"
        );
        ensure!(a.geometry_matches(), "desktop geometry changed");
        Ok(a)
    }
    fn geometry_matches(&self) -> bool {
        Target::parse(&self.desktop_session)
            .ok()
            .and_then(|t| t.geometry())
            == Some((self.logical_size, self.logical_position))
            && self.logical_size.iter().all(|n| *n > 0 && *n <= 8192)
    }
}
struct Pixels {
    bytes: Vec<u8>,
    metadata: RawPixels,
    captured: Instant,
}
fn pixel_color(
    buffer: &screencapturekit::cv::CVPixelBuffer,
) -> std::result::Result<[u32; 4], ErrorCode> {
    unsafe {
        let equal = |key, value| {
            let actual =
                CVBufferGetAttachment(buffer.as_ptr().cast_const(), key, std::ptr::null_mut());
            !actual.is_null() && CFEqual(actual, value)
        };
        if !equal(
            kCVImageBufferYCbCrMatrixKey,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2,
        ) || !equal(
            kCVImageBufferColorPrimariesKey,
            kCVImageBufferColorPrimaries_ITU_R_709_2,
        ) {
            return Err(ErrorCode::Unavailable);
        }
        let transfer = if equal(
            kCVImageBufferTransferFunctionKey,
            kCVImageBufferTransferFunction_ITU_R_709_2,
        ) {
            5
        } else if equal(
            kCVImageBufferTransferFunctionKey,
            kCVImageBufferTransferFunction_sRGB,
        ) {
            7
        } else {
            return Err(ErrorCode::Unavailable);
        };
        // Exact SPA enum mapping: video range, BT709 matrix, actual transfer, BT709 primaries.
        Ok([2, 3, transfer, 1])
    }
}
fn copy_pixels(
    sample: CMSampleBuffer,
    size: [u32; 2],
    maximum: usize,
) -> std::result::Result<Pixels, ErrorCode> {
    let b = sample.image_buffer().ok_or(ErrorCode::Unavailable)?;
    if b.pixel_format() != u32::from_be_bytes(*b"420v")
        || b.plane_count() != 2
        || [b.width() as u32, b.height() as u32] != size
    {
        return Err(ErrorCode::Unavailable);
    }
    let count = (size[0] as usize)
        .checked_mul(size[1] as usize)
        .ok_or(ErrorCode::InvalidRequest)?;
    if count.checked_add(count / 2).is_none_or(|n| n > maximum) {
        return Err(ErrorCode::Unavailable);
    }
    let color = pixel_color(&b)?;
    let guard = b.lock_read_only().map_err(|_| ErrorCode::Unavailable)?;
    let mut bytes = Vec::with_capacity(count + count / 2);
    let mut lengths = Vec::new();
    let mut hashes = Vec::new();
    for plane in 0..2 {
        let rows = size[1] as usize / if plane == 0 { 1 } else { 2 };
        let row_bytes = size[0] as usize;
        if guard.height_of_plane(plane) != rows || guard.bytes_per_row_of_plane(plane) < row_bytes {
            return Err(ErrorCode::Unavailable);
        }
        let start = bytes.len();
        for row in 0..rows {
            let data = guard.plane_row(plane, row).ok_or(ErrorCode::Unavailable)?;
            if data.len() < row_bytes {
                return Err(ErrorCode::Unavailable);
            }
            bytes.extend_from_slice(&data[..row_bytes]);
        }
        lengths.push(bytes.len() - start);
        hashes.push(digest(&bytes[start..]));
    }
    Ok(Pixels {
        bytes,
        metadata: RawPixels {
            encoding: "nv12".into(),
            size,
            color,
            plane_lengths: lengths,
            plane_sha256: hashes,
        },
        captured: Instant::now(),
    })
}
struct Output {
    latest: Arc<Mutex<Option<std::result::Result<Pixels, ErrorCode>>>>,
    size: [u32; 2],
    maximum: usize,
}
impl SCStreamOutputTrait for Output {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
        if kind != SCStreamOutputType::Screen
            || sample.frame_status() != Some(screencapturekit::cm::SCFrameStatus::Complete)
        {
            return;
        }
        if let Ok(mut slot) = self.latest.lock() {
            // A single retained owned frame. Its producer allocation is capped before copying.
            slot.take();
            *slot = Some(copy_pixels(sample, self.size, self.maximum));
        }
    }
}
struct Session {
    target: Target,
    lifetime: window::Lifetime,
    binding: Binding,
    deadline: Instant,
    stream: SCStream,
    latest: Arc<Mutex<Option<std::result::Result<Pixels, ErrorCode>>>>,
    last: Option<FrameRef>,
    observed: Option<Instant>,
    last_input: Option<Instant>,
    frame_sequence: u64,
    sequence: u64,
    keys: BTreeSet<u16>,
    buttons: BTreeSet<Button>,
    source: CGEventSource,
    pointer: CGPoint,
}
const INPUT_FRESHNESS: Duration = Duration::from_secs(1);
fn freshness_budget(at: Option<Instant>, now: Instant) -> Duration {
    at.and_then(|at| now.checked_duration_since(at))
        .and_then(|age| INPUT_FRESHNESS.checked_sub(age))
        .unwrap_or(Duration::ZERO)
}
fn request_wait_budget(
    deadline: Instant,
    last_input: Option<Instant>,
    holding: bool,
    now: Instant,
) -> Duration {
    let budget = deadline
        .saturating_duration_since(now)
        .min(Duration::from_secs(5));
    if holding {
        budget.min(freshness_budget(last_input, now))
    } else {
        budget
    }
}
/// Explicit USB HID keyboard-page mapping; unknown usages fail before any action.
fn mac_key(usage: u16) -> Option<u16> {
    const LETTERS: [u16; 26] = [
        0, 11, 8, 2, 14, 3, 5, 4, 34, 38, 40, 37, 46, 45, 31, 35, 12, 15, 1, 17, 32, 9, 13, 7, 16,
        6,
    ];
    const DIGITS: [u16; 10] = [18, 19, 20, 21, 23, 22, 26, 28, 25, 29];
    match usage {
        4..=29 => Some(LETTERS[(usage - 4) as usize]),
        30..=39 => Some(DIGITS[(usage - 30) as usize]),
        40 => Some(36),
        41 => Some(53),
        42 => Some(51),
        43 => Some(48),
        44 => Some(49),
        45 => Some(27),
        46 => Some(24),
        47 => Some(33),
        48 => Some(30),
        49 => Some(42),
        51 => Some(41),
        52 => Some(39),
        53 => Some(50),
        54 => Some(43),
        55 => Some(47),
        56 => Some(44),
        79 => Some(124),
        80 => Some(123),
        81 => Some(125),
        82 => Some(126),
        224 => Some(59),
        225 => Some(56),
        226 => Some(58),
        227 => Some(55),
        228 => Some(62),
        229 => Some(60),
        230 => Some(61),
        231 => Some(54),
        _ => None,
    }
}
fn ensure_target_event(event: &CGEvent, target: Target) {
    event.set_integer_value_field(
        core_graphics::event::EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER,
        i64::from(target.window),
    );
    event.set_integer_value_field(core_graphics::event::EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER_THAT_CAN_HANDLE_THIS_EVENT,i64::from(target.window));
}
fn mouse(button: Button, down: bool) -> (CGMouseButton, CGEventType) {
    match (button, down) {
        (Button::Left, true) => (CGMouseButton::Left, CGEventType::LeftMouseDown),
        (Button::Left, false) => (CGMouseButton::Left, CGEventType::LeftMouseUp),
        (Button::Right, true) => (CGMouseButton::Right, CGEventType::RightMouseDown),
        (Button::Right, false) => (CGMouseButton::Right, CGEventType::RightMouseUp),
        (Button::Middle, true) => (CGMouseButton::Center, CGEventType::OtherMouseDown),
        (Button::Middle, false) => (CGMouseButton::Center, CGEventType::OtherMouseUp),
    }
}
impl Session {
    fn flags(&self) -> CGEventFlags {
        let mut flags = CGEventFlags::empty();
        for (keys, flag) in [
            ([225, 229], CGEventFlags::CGEventFlagShift),
            ([224, 228], CGEventFlags::CGEventFlagControl),
            ([226, 230], CGEventFlags::CGEventFlagAlternate),
            ([227, 231], CGEventFlags::CGEventFlagCommand),
        ] {
            if keys.iter().any(|k| self.keys.contains(k)) {
                flags.insert(flag);
            }
        }
        flags
    }
    fn key(&mut self, usage: u16, down: bool) -> std::result::Result<(), ErrorCode> {
        let code = mac_key(usage).ok_or(ErrorCode::InvalidActions)?;
        let event = CGEvent::new_keyboard_event(self.source.clone(), code, down)
            .map_err(|_| ErrorCode::OutcomeUnknown)?;
        // A failed event constructor must not create ownership for a later release.
        if down {
            self.keys.insert(usage);
        } else {
            self.keys.remove(&usage);
        }
        event.set_flags(self.flags());
        ensure_target_event(&event, self.target);
        event.post_to_pid(self.target.pid);
        Ok(())
    }
    fn button(&mut self, button: Button, down: bool) -> std::result::Result<(), ErrorCode> {
        let (native, kind) = mouse(button, down);
        let event = CGEvent::new_mouse_event(self.source.clone(), kind, self.pointer, native)
            .map_err(|_| ErrorCode::OutcomeUnknown)?;
        event.set_flags(self.flags());
        ensure_target_event(&event, self.target);
        event.post_to_pid(self.target.pid);
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
        Ok(())
    }
    fn close(mut self) -> std::result::Result<(), ErrorCode> {
        let mut released = true;
        if (!self.keys.is_empty() || !self.buttons.is_empty())
            && (platform_status() != "screencapturekit" || !self.lifetime.live(self.target))
        {
            released = false;
        }
        for k in self
            .keys
            .clone()
            .into_iter()
            .filter(|_| self.lifetime.live(self.target))
            .collect::<Vec<_>>()
        {
            released &= self.key(k, false).is_ok();
        }
        for b in self
            .buttons
            .clone()
            .into_iter()
            .filter(|_| self.lifetime.live(self.target))
            .collect::<Vec<_>>()
        {
            released &= self.button(b, false).is_ok();
        }
        let stopped = self.stream.stop_capture().is_ok();
        if released && stopped {
            Ok(())
        } else {
            Err(ErrorCode::OutcomeUnknown)
        }
    }
}
struct Worker {
    approval: Approval,
    session: Option<Session>,
    spent: bool,
    poisoned: bool,
}
impl Worker {
    fn close(&mut self) -> std::result::Result<(), ErrorCode> {
        self.spent = true;
        let clean = self.session.take().map_or(Ok(()), Session::close);
        if clean.is_err() {
            self.poisoned = true;
        }
        if self.poisoned {
            Err(ErrorCode::OutcomeUnknown)
        } else {
            clean
        }
    }
    fn finish_reply(&mut self, result: &std::result::Result<Response, ErrorCode>) -> bool {
        if matches!(result, Err(ErrorCode::OutcomeUnknown)) {
            self.poisoned = true;
            let _ = self.close();
            return true;
        }
        matches!(result, Ok(Response::Closed {}))
    }
    async fn handle(
        &mut self,
        request: Request,
    ) -> (std::result::Result<Response, ErrorCode>, Option<Pixels>) {
        let result = self.perform(request).await;
        match result {
            Ok(v) => v,
            Err(e) => (Err(e), None),
        }
    }
    async fn perform(
        &mut self,
        request: Request,
    ) -> std::result::Result<(std::result::Result<Response, ErrorCode>, Option<Pixels>), ErrorCode>
    {
        if self.poisoned {
            return Err(ErrorCode::OutcomeUnknown);
        }
        if matches!(request, Request::Capabilities {}) {
            return Ok((
                Ok(Response::Capabilities {
                    capabilities: Capabilities {
                        available: platform_status() == "screencapturekit",
                        backend: platform_status().into(),
                        ..Capabilities::default()
                    },
                }),
                None,
            ));
        }
        if request.identity() != Some(&self.approval.identity) {
            return Err(ErrorCode::IdentityMismatch);
        }
        if self.approval.expires_at_ms <= now_ms()
            || !self.approval.geometry_matches()
            || platform_status() != "screencapturekit"
        {
            self.close()?;
            return Err(ErrorCode::Unavailable);
        }
        if self
            .session
            .as_ref()
            .is_some_and(|s| Instant::now() >= s.deadline)
        {
            self.close()?;
            return Err(ErrorCode::SessionClosed);
        }
        if self
            .session
            .as_ref()
            .is_some_and(|s| !s.lifetime.live(s.target))
        {
            self.close()?;
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
                self.spent = true;
                let content = SCShareableContent::get().map_err(|_| ErrorCode::Unavailable)?;
                let target = Target::parse(&self.approval.desktop_session)
                    .map_err(|_| ErrorCode::Unavailable)?;
                let window = content
                    .windows()
                    .into_iter()
                    .find(|w| {
                        w.window_id() == target.window
                            && w.owning_application()
                                .is_some_and(|a| a.process_id() == target.pid)
                    })
                    .ok_or(ErrorCode::Unavailable)?;
                let lifetime =
                    window::Lifetime::bind(target).map_err(|_| ErrorCode::Unavailable)?;
                let size = self.approval.logical_size.map(|v| v + (v % 2));
                let filter = SCContentFilter::create().with_window(&window).build();
                let mut config = SCStreamConfiguration::new();
                config.set_width(size[0]);
                config.set_height(size[1]);
                config.set_queue_depth(2);
                config.set_minimum_frame_interval(&CMTime::new(1, 10));
                config.set_pixel_format(
                    screencapturekit::stream::configuration::PixelFormat::YCbCr_420v,
                );
                config.set_color_space_name("kCGColorSpaceITUR_709");
                config.set_color_matrix("ITU_R_709_2");
                config.set_shows_cursor(true);
                let latest = Arc::new(Mutex::new(None));
                let mut stream = SCStream::new(&filter, &config);
                stream.add_output_handler(
                    Output {
                        latest: latest.clone(),
                        size,
                        maximum: self.approval.max_frame_bytes,
                    },
                    SCStreamOutputType::Screen,
                );
                let source = CGEventSource::new(CGEventSourceStateID::Private)
                    .map_err(|_| ErrorCode::Unavailable)?;
                let pointer = CGPoint::new(
                    f64::from(self.approval.logical_position[0]),
                    f64::from(self.approval.logical_position[1]),
                );
                stream
                    .start_capture()
                    .map_err(|_| ErrorCode::OutcomeUnknown)?;
                let remaining = self
                    .approval
                    .expires_at_ms
                    .saturating_sub(now_ms())
                    .min(u64::from(lease_ms));
                if remaining == 0 || !lifetime.live(target) {
                    stream
                        .stop_capture()
                        .map_err(|_| ErrorCode::OutcomeUnknown)?;
                    return Err(ErrorCode::SessionClosed);
                }
                let binding = Binding {
                    identity,
                    epoch: Uuid::new_v4(),
                };
                self.session = Some(Session {
                    target,
                    lifetime,
                    binding: binding.clone(),
                    deadline: Instant::now() + Duration::from_millis(remaining),
                    stream,
                    latest,
                    last: None,
                    observed: None,
                    last_input: None,
                    frame_sequence: 0,
                    sequence: 0,
                    keys: BTreeSet::new(),
                    buttons: BTreeSet::new(),
                    source,
                    pointer,
                });
                Ok((
                    Ok(Response::Opened {
                        binding,
                        expires_at_ms: now_ms() + remaining,
                    }),
                    None,
                ))
            }
            Request::Observe { binding } => {
                let s = self.session.as_mut().ok_or(ErrorCode::SessionClosed)?;
                if s.binding != binding {
                    return Err(ErrorCode::IdentityMismatch);
                }
                let pixels = loop {
                    let v = s
                        .latest
                        .lock()
                        .map_err(|_| ErrorCode::OutcomeUnknown)?
                        .take();
                    if let Some(v) = v {
                        let p = v?;
                        if p.captured.elapsed() <= Duration::from_secs(1) {
                            break p;
                        }
                    }
                    if Instant::now() >= s.deadline {
                        return Err(ErrorCode::SessionClosed);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                };
                if Instant::now() >= s.deadline
                    || !s.lifetime.live(s.target)
                    || !self.approval.geometry_matches()
                    || platform_status() != "screencapturekit"
                {
                    return Err(ErrorCode::Unavailable);
                }
                s.frame_sequence += 1;
                let frame = FrameRef {
                    binding,
                    source_revision: 1,
                    frame_sequence: s.frame_sequence,
                    mapping: FrameMapping {
                        frame_size: pixels.metadata.size,
                        logical_size: self.approval.logical_size,
                        desktop_position: self.approval.logical_position,
                        content: ContentRect {
                            left: 0,
                            top: 0,
                            width: pixels.metadata.size[0],
                            height: pixels.metadata.size[1],
                        },
                        rotation: Rotation::R0,
                    },
                };
                s.last = Some(frame.clone());
                s.observed = Some(pixels.captured);
                Ok((Ok(Response::Observed { frame }), Some(pixels)))
            }
            Request::Act {
                frame,
                sequence,
                actions,
            } => {
                let s = self.session.as_mut().ok_or(ErrorCode::SessionClosed)?;
                if s.last.as_ref() != Some(&frame)
                    || freshness_budget(s.observed, Instant::now()).is_zero()
                {
                    return Err(ErrorCode::StaleFrame);
                }
                if sequence
                    != s.sequence
                        .checked_add(1)
                        .ok_or(ErrorCode::InvalidSequence)?
                {
                    return Err(ErrorCode::InvalidSequence);
                }
                if actions.is_empty() || actions.len() > MAX_ACTIONS {
                    return Err(ErrorCode::InvalidActions);
                }
                // Validate the entire batch and balanced held state before the first OS event.
                let mut keys = s.keys.clone();
                let mut buttons = s.buttons.clone();
                for a in &actions {
                    match a {
                        Action::PointerMove { x, y } => {
                            frame.mapping.map(*x, *y)?;
                        }
                        Action::KeyDown { usage } => {
                            if mac_key(*usage).is_none() || !keys.insert(*usage) {
                                return Err(ErrorCode::InvalidActions);
                            }
                        }
                        Action::KeyUp { usage } => {
                            if !keys.remove(usage) {
                                return Err(ErrorCode::InvalidActions);
                            }
                        }
                        Action::ButtonDown { button } => {
                            if !buttons.insert(*button) {
                                return Err(ErrorCode::InvalidActions);
                            }
                        }
                        Action::ButtonUp { button } => {
                            if !buttons.remove(button) {
                                return Err(ErrorCode::InvalidActions);
                            }
                        }
                        Action::Wheel {
                            horizontal,
                            vertical,
                        } => {
                            if horizontal.unsigned_abs() > 1200 || vertical.unsigned_abs() > 1200 {
                                return Err(ErrorCode::InvalidActions);
                            }
                        }
                    }
                }
                s.sequence = sequence;
                s.last_input = Some(Instant::now());
                for a in &actions {
                    if freshness_budget(s.observed, Instant::now()).is_zero()
                        || Instant::now() >= s.deadline
                        || !s.lifetime.live(s.target)
                        || !self.approval.geometry_matches()
                        || platform_status() != "screencapturekit"
                    {
                        return Err(ErrorCode::OutcomeUnknown);
                    }
                    match *a {
                        Action::PointerMove { x, y } => {
                            let p = frame.mapping.map(x, y)?;
                            s.pointer = CGPoint::new(
                                p.x + f64::from(self.approval.logical_position[0]),
                                p.y + f64::from(self.approval.logical_position[1]),
                            );
                            let (button, kind) = if s.buttons.contains(&Button::Left) {
                                (CGMouseButton::Left, CGEventType::LeftMouseDragged)
                            } else if s.buttons.contains(&Button::Right) {
                                (CGMouseButton::Right, CGEventType::RightMouseDragged)
                            } else if s.buttons.contains(&Button::Middle) {
                                (CGMouseButton::Center, CGEventType::OtherMouseDragged)
                            } else {
                                (CGMouseButton::Left, CGEventType::MouseMoved)
                            };
                            let event =
                                CGEvent::new_mouse_event(s.source.clone(), kind, s.pointer, button)
                                    .map_err(|_| ErrorCode::OutcomeUnknown)?;
                            event.set_flags(s.flags());
                            ensure_target_event(&event, s.target);
                            event.post_to_pid(s.target.pid);
                        }
                        Action::KeyDown { usage } => s.key(usage, true)?,
                        Action::KeyUp { usage } => s.key(usage, false)?,
                        Action::ButtonDown { button } => s.button(button, true)?,
                        Action::ButtonUp { button } => s.button(button, false)?,
                        Action::Wheel {
                            horizontal,
                            vertical,
                        } => {
                            let event = CGEvent::new_scroll_event(
                                s.source.clone(),
                                ScrollEventUnit::PIXEL,
                                2,
                                i32::from(vertical),
                                i32::from(horizontal),
                                0,
                            )
                            .map_err(|_| ErrorCode::OutcomeUnknown)?;
                            event.set_flags(s.flags());
                            unsafe { CGEventSetLocation(event.as_ptr().cast(), s.pointer) };
                            ensure_target_event(&event, s.target);
                            event.post_to_pid(s.target.pid);
                        }
                    }
                }
                Ok((
                    Ok(Response::Accepted {
                        sequence,
                        accepted_actions: actions.len(),
                        effect_confirmed: false,
                    }),
                    None,
                ))
            }
            Request::Close { binding } => {
                if self.session.as_ref().is_none_or(|s| s.binding != binding) {
                    return Err(ErrorCode::IdentityMismatch);
                }
                self.close()?;
                Ok((Ok(Response::Closed {}), None))
            }
            Request::Capabilities {} => unreachable!(),
        }
    }
}
fn pipe(fd: i32) -> Result<AsyncFd<std::fs::File>> {
    let duplicate = unsafe { nix::libc::fcntl(fd, nix::libc::F_DUPFD_CLOEXEC, 3) };
    ensure!(duplicate >= 0, "private pipe fd");
    let file = unsafe { std::fs::File::from_raw_fd(duplicate) };
    let flags = unsafe { nix::libc::fcntl(file.as_raw_fd(), nix::libc::F_GETFL) };
    ensure!(
        flags >= 0
            && unsafe {
                nix::libc::fcntl(
                    file.as_raw_fd(),
                    nix::libc::F_SETFL,
                    flags | nix::libc::O_NONBLOCK,
                )
            } >= 0,
        "nonblocking pipe"
    );
    Ok(AsyncFd::new(file)?)
}
async fn read_exact(fd: &AsyncFd<std::fs::File>, mut b: &mut [u8]) -> Result<()> {
    while !b.is_empty() {
        let mut ready = fd.readable().await?;
        match ready.try_io(|v| {
            let mut f = v.get_ref();
            f.read(b)
        }) {
            Ok(Ok(0)) => anyhow::bail!("private pipe EOF"),
            Ok(Ok(n)) => b = &mut b[n..],
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {}
        }
    }
    Ok(())
}
async fn write_all(fd: &AsyncFd<std::fs::File>, mut b: &[u8]) -> Result<()> {
    while !b.is_empty() {
        let mut ready = fd.writable().await?;
        match ready.try_io(|v| {
            let mut f = v.get_ref();
            f.write(b)
        }) {
            Ok(Ok(0)) => anyhow::bail!("private pipe EOF"),
            Ok(Ok(n)) => b = &b[n..],
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {}
        }
    }
    Ok(())
}
/// Enter only as the hash-bound Agent child with its owned per-Open approval.
pub async fn run(path: &Path) -> Result<()> {
    let approval = Approval::load(path)?;
    let mut worker = Worker {
        approval,
        session: None,
        spent: false,
        poisoned: false,
    };
    let input = pipe(0)?;
    let output = pipe(1)?;
    let mut seen = BTreeSet::new();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        let remaining = worker.session.as_ref().map_or(Duration::from_secs(5), |s| {
            // Observe cannot renew the lifetime of an owned held key/button.
            request_wait_budget(
                s.deadline,
                s.last_input,
                !s.keys.is_empty() || !s.buttons.is_empty(),
                Instant::now(),
            )
        });
        let request = async {
            let mut size = [0; 4];
            read_exact(&input, &mut size).await?;
            let n = u32::from_be_bytes(size) as usize;
            ensure!(n > 0 && n <= MAX_WORKER_HEADER, "request size");
            let mut b = vec![0; n];
            read_exact(&input, &mut b).await?;
            Ok::<WorkerRequest, anyhow::Error>(serde_json::from_slice(&b)?)
        };
        let envelope = tokio::select! {biased;_=term.recv()=>break,_=tokio::signal::ctrl_c()=>break,
        v=tokio::time::timeout(remaining,request)=>match v{Ok(Ok(v))=>v,_=>break}};
        if !seen.insert(envelope.request_id) || seen.len() > 128 {
            break;
        }
        let handling_budget = worker
            .session
            .as_ref()
            .map_or(Duration::from_secs(25), |s| {
                request_wait_budget(
                    s.deadline,
                    s.last_input,
                    !s.keys.is_empty() || !s.buttons.is_empty(),
                    Instant::now(),
                )
            });
        let (result, pixels) = tokio::select! {biased;_=term.recv()=>break,_=tokio::signal::ctrl_c()=>break,
        v=tokio::time::timeout(handling_budget,worker.handle(envelope.request))=>match v{Ok(v)=>v,Err(_)=>{worker.poisoned=true;(Err(ErrorCode::OutcomeUnknown),None)}}};
        let closing = worker.finish_reply(&result);
        let reply = WorkerReply {
            request_id: envelope.request_id,
            result,
            pixels: pixels.as_ref().map(|p| p.metadata.clone()),
        };
        let header = serde_json::to_vec(&reply)?;
        let send = async {
            write_all(&output, &(header.len() as u32).to_be_bytes()).await?;
            write_all(&output, &header).await?;
            if let Some(p) = pixels {
                write_all(&output, &p.bytes).await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        if !matches!(
            tokio::time::timeout(Duration::from_secs(2), send).await,
            Ok(Ok(()))
        ) || closing
        {
            break;
        }
    }
    worker
        .close()
        .map_err(|_| anyhow::anyhow!("desktop cleanup outcome unknown"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hid_mapping_is_explicit_and_rejects_unmapped_usages() {
        assert_eq!(mac_key(4), Some(0));
        assert_eq!(mac_key(29), Some(6));
        assert_eq!(mac_key(30), Some(18));
        assert_eq!(mac_key(227), Some(55));
        assert_eq!(mac_key(231), Some(54));
        for usage in [0, 1, 2, 3, 50, 255, u16::MAX] {
            assert_eq!(mac_key(usage), None);
        }
    }
    #[test]
    fn input_safety_freshness_rejects_missing_future_and_expired_capture() {
        let start = Instant::now();
        assert_eq!(freshness_budget(None, start), Duration::ZERO);
        assert_eq!(
            freshness_budget(Some(start + Duration::from_nanos(1)), start),
            Duration::ZERO
        );
        assert_eq!(freshness_budget(Some(start), start), INPUT_FRESHNESS);
        assert_eq!(
            freshness_budget(Some(start), start + Duration::from_millis(999)),
            Duration::from_millis(1)
        );
        assert_eq!(
            freshness_budget(Some(start), start + INPUT_FRESHNESS),
            Duration::ZERO
        );
        assert_eq!(
            freshness_budget(
                Some(start),
                start + INPUT_FRESHNESS + Duration::from_nanos(1)
            ),
            Duration::ZERO
        );
    }
    #[test]
    fn input_safety_batch_drops_events_after_capture_expiry() {
        let captured = Instant::now();
        let mut posted = Vec::new();
        for (event, age_ms) in [("down", 900), ("wheel", 1000), ("another_down", 1001)] {
            if freshness_budget(Some(captured), captured + Duration::from_millis(age_ms)).is_zero()
            {
                break;
            }
            posted.push(event);
        }
        assert_eq!(posted, vec!["down"]);
    }
    #[test]
    fn input_safety_observe_cannot_extend_held_input_idle_budget() {
        let input = Instant::now();
        let deadline = input + Duration::from_secs(10);
        let refreshed_capture = input + Duration::from_millis(850);
        assert_eq!(
            freshness_budget(Some(refreshed_capture), refreshed_capture),
            INPUT_FRESHNESS
        );
        assert_eq!(
            request_wait_budget(deadline, Some(input), true, refreshed_capture),
            Duration::from_millis(150)
        );
        assert_eq!(
            request_wait_budget(deadline, Some(input), true, input + INPUT_FRESHNESS),
            Duration::ZERO
        );
        assert_eq!(
            request_wait_budget(deadline, None, true, input),
            Duration::ZERO
        );
    }
    #[test]
    fn input_safety_session_deadline_caps_wait_without_owned_input() {
        let start = Instant::now();
        assert_eq!(
            request_wait_budget(start + Duration::from_secs(10), None, false, start),
            Duration::from_secs(5)
        );
        assert_eq!(
            request_wait_budget(
                start + Duration::from_millis(100),
                Some(start),
                false,
                start
            ),
            Duration::from_millis(100)
        );
        assert_eq!(
            request_wait_budget(start, Some(start), false, start),
            Duration::ZERO
        );
    }
    #[tokio::test]
    async fn input_safety_unknown_reply_closes_and_keeps_replay_fenced() {
        let mut worker = Worker {
            approval: Approval {
                enabled: false,
                app_id: APP_ID.into(),
                desktop_uid: 0,
                desktop_session: String::new(),
                identity: Identity {
                    owner: "offline-test".into(),
                    task_id: Uuid::nil(),
                    device_id: Uuid::nil(),
                    desktop_id: String::new(),
                },
                worker_sha256: String::new(),
                agent_sha256: String::new(),
                expires_at_ms: 0,
                logical_size: [0, 0],
                logical_position: [0, 0],
                max_frame_bytes: 0,
                allow_input: false,
            },
            session: None,
            spent: false,
            poisoned: false,
        };
        assert!(!worker.finish_reply(&Err(ErrorCode::InvalidSequence)));
        assert!(!worker.spent);
        assert!(worker.finish_reply(&Err(ErrorCode::OutcomeUnknown)));
        assert!(worker.spent && worker.poisoned && worker.session.is_none());
        let (reply, pixels) = worker.handle(Request::Capabilities {}).await;
        assert!(matches!(reply, Err(ErrorCode::OutcomeUnknown)));
        assert!(pixels.is_none());
        assert!(worker.finish_reply(&Err(ErrorCode::OutcomeUnknown)));
        assert!(worker.spent && worker.poisoned);
    }
    #[test]
    fn pixel_budget_covers_4k_nv12_without_rgba_conversion() {
        let count = 3840usize * 2160;
        assert!(count + count / 2 <= MAX_WORKER_PIXELS);
        const { assert!(3840usize * 2160 * 4 > MAX_WORKER_PIXELS) };
    }
}
