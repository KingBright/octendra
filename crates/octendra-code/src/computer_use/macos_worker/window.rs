//! Lifetime-bound, single-window macOS target. Uses public metadata only.
use anyhow::{Result, ensure};
use octendra_desktop_protocol::ui::SemanticElement;
use std::{
    ffi::c_void,
    mem::size_of,
    time::{Duration, Instant},
};
type Cf = *const c_void;
#[repr(C)]
struct CFRange {
    location: isize,
    length: isize,
}
#[repr(C)]
#[derive(Default)]
struct BsdInfo {
    ids: [u32; 12],
    comm: [u8; 16],
    name: [u8; 32],
    extra: [u32; 6],
    seconds: u64,
    micros: u64,
}
unsafe extern "C" {
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, size: i32) -> i32;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, window: u32) -> Cf;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: Cf);
    fn CFArrayGetCount(array: Cf) -> isize;
    fn CFArrayGetValueAtIndex(array: Cf, index: isize) -> Cf;
    fn CFDictionaryGetValue(dictionary: Cf, key: Cf) -> Cf;
    fn CFStringCreateWithCString(allocator: Cf, value: *const i8, encoding: u32) -> Cf;
    fn CFGetTypeID(value: Cf) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(value: Cf, kind: isize, out: *mut c_void) -> bool;
    fn CFBooleanGetTypeID() -> usize;
    fn CFBooleanGetValue(value: Cf) -> bool;
}
unsafe fn field(dictionary: Cf, key: &std::ffi::CStr) -> Cf {
    let key = unsafe { CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), 0x08000100) };
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    unsafe { CFRelease(key) };
    value
}
unsafe fn number(dictionary: Cf, key: &std::ffi::CStr) -> Option<f64> {
    let v = unsafe { field(dictionary, key) };
    let mut out = 0.0;
    if !v.is_null()
        && unsafe {
            CFGetTypeID(v) == CFNumberGetTypeID()
                && CFNumberGetValue(v, 13, (&mut out as *mut f64).cast())
        }
    {
        Some(out)
    } else {
        None
    }
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Target {
    pub pid: i32,
    pub window: u32,
    pub started_us: u64,
}
impl Target {
    /// Opaque backend identity remains within the existing desktop identity contract.
    pub fn parse(session: &str) -> Result<Self> {
        let s = session
            .strip_prefix("windowP")
            .ok_or_else(|| anyhow::anyhow!("window-scoped authority required"))?;
        let (pid, s) = s
            .split_once('S')
            .ok_or_else(|| anyhow::anyhow!("target identity"))?;
        let (start, window) = s
            .split_once('W')
            .ok_or_else(|| anyhow::anyhow!("target identity"))?;
        let t = Self {
            pid: pid.parse()?,
            window: window.parse()?,
            started_us: start.parse()?,
        };
        ensure!(
            t.pid > 0 && t.window > 0 && t.started_us > 0,
            "target identity bounds"
        );
        Ok(t)
    }
    pub fn identity(pid: i32, window: u32) -> Result<String> {
        let info = process(pid).ok_or_else(|| anyhow::anyhow!("target process unavailable"))?;
        ensure!(
            info.ids[5] == unsafe { nix::libc::getuid() } && info.ids[7] == info.ids[5],
            "target must belong to current user"
        );
        let t = Self {
            pid,
            window,
            started_us: info.seconds * 1_000_000 + info.micros,
        };
        ensure!(t.geometry().is_some(), "target window unavailable");
        Ok(format!("windowP{pid}S{}W{window}", t.started_us))
    }
    pub fn process_live(&self) -> bool {
        process(self.pid).is_some_and(|p| {
            p.ids[5] == unsafe { nix::libc::getuid() }
                && p.ids[7] == p.ids[5]
                && p.seconds * 1_000_000 + p.micros == self.started_us
        })
    }
    /// Exact pinned window geometry. Sibling popups or extra windows of the same
    /// process never receive events because capture and CGEvent window IDs stay
    /// bound to `self.window` and its retained AXUIElement lifetime.
    pub fn geometry(&self) -> Option<([u32; 2], [i32; 2])> {
        if !self.process_live() {
            return None;
        }
        unsafe {
            let list = CGWindowListCopyWindowInfo(0, 0);
            if list.is_null() {
                return None;
            }
            let mut geometry = None;
            for index in 0..CFArrayGetCount(list) {
                let d = CFArrayGetValueAtIndex(list, index);
                if number(d, c"kCGWindowOwnerPID") != Some(f64::from(self.pid))
                    || number(d, c"kCGWindowNumber") != Some(f64::from(self.window))
                {
                    continue;
                }
                let on = field(d, c"kCGWindowIsOnscreen");
                if on.is_null() || CFGetTypeID(on) != CFBooleanGetTypeID() || !CFBooleanGetValue(on)
                {
                    continue;
                }
                let bounds = field(d, c"kCGWindowBounds");
                if bounds.is_null() {
                    continue;
                }
                let Some(w) = number(bounds, c"Width") else {
                    continue;
                };
                let Some(h) = number(bounds, c"Height") else {
                    continue;
                };
                if w <= 1.0 || h <= 1.0 {
                    continue;
                }
                if let (Some(x), Some(y)) = (number(bounds, c"X"), number(bounds, c"Y")) {
                    geometry = Some(([w as u32, h as u32], [x as i32, y as i32]));
                    break;
                }
            }
            CFRelease(list);
            geometry
        }
    }
}

/// Lists current-user onscreen application windows (front-to-back order) with geometry and titles.
pub fn list_onscreen_user_windows() -> Vec<serde_json::Value> {
    let my_uid = unsafe { nix::libc::geteuid() };
    let list = unsafe { CGWindowListCopyWindowInfo(1 | 16, 0) };
    if list.is_null() {
        return Vec::new();
    }
    let mut windows = Vec::new();
    let count = unsafe { CFArrayGetCount(list) };
    for index in 0..count {
        let d = unsafe { CFArrayGetValueAtIndex(list, index) };
        if unsafe { number(d, c"kCGWindowLayer") }.is_some_and(|l| l.round() as i64 != 0) {
            continue;
        }
        let Some(pid_f) = (unsafe { number(d, c"kCGWindowOwnerPID") }) else {
            continue;
        };
        let Some(win_f) = (unsafe { number(d, c"kCGWindowNumber") }) else {
            continue;
        };
        let pid = pid_f.round() as i32;
        let window_id = win_f.round() as u32;
        if pid <= 0 || window_id == 0 {
            continue;
        }
        let Some(pinfo) = process(pid) else {
            continue;
        };
        if pinfo.ids[5] != my_uid || pinfo.ids[7] != my_uid {
            continue;
        }
        let bounds = unsafe { field(d, c"kCGWindowBounds") };
        if bounds.is_null() {
            continue;
        }
        let (Some(w), Some(h)) = (
            unsafe { number(bounds, c"Width") },
            unsafe { number(bounds, c"Height") },
        ) else {
            continue;
        };
        if w < 80.0 || h < 80.0 {
            continue;
        }
        let owner_name = cf_string(unsafe { field(d, c"kCGWindowOwnerName") }, 128).unwrap_or_default();
        if matches!(
            owner_name.as_str(),
            "Window Server"
                | "Dock"
                | "SystemUIServer"
                | "Control Center"
                | "Notification Center"
        ) {
            continue;
        }
        let window_title = cf_string(unsafe { field(d, c"kCGWindowName") }, 256).unwrap_or_default();
        let x = unsafe { number(bounds, c"X") }.unwrap_or(0.0);
        let y = unsafe { number(bounds, c"Y") }.unwrap_or(0.0);
        windows.push(serde_json::json!({
            "process_id": pid,
            "window_id": window_id,
            "app_name": owner_name,
            "window_title": window_title,
            "logical_size": [w as u32, h as u32],
            "logical_position": [x as i32, y as i32],
            "z_order": index,
        }));
        if windows.len() >= 64 {
            break;
        }
    }
    unsafe { CFRelease(list) };
    windows
}

/// Selects the frontmost onscreen current-user window matching optional `app_query`, `window_query`,
/// or `pid_filter`, returning full `window_target` capability metadata.
///
/// # Errors
///
/// Returns an error if no matching onscreen window is available or `Target::identity` rejects it.
pub fn select_onscreen_user_window(
    app_query: Option<&str>,
    window_query: Option<&str>,
    pid_filter: Option<i32>,
) -> Result<serde_json::Value> {
    let app_needle = app_query
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    let win_needle = window_query
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    for item in list_onscreen_user_windows() {
        let pid = item["process_id"].as_i64().unwrap_or(0) as i32;
        let window_id = item["window_id"].as_u64().unwrap_or(0) as u32;
        if let Some(expected_pid) = pid_filter
            && pid != expected_pid
        {
            continue;
        }
        let app_name = item["app_name"].as_str().unwrap_or("");
        let window_title = item["window_title"].as_str().unwrap_or("");
        if let Some(ref needle) = app_needle
            && !app_name.to_lowercase().contains(needle)
        {
            continue;
        }
        if let Some(ref needle) = win_needle
            && !window_title.to_lowercase().contains(needle)
        {
            continue;
        }
        if let Ok(mut target) = super::window_target(pid, window_id) {
            target["app_name"] = serde_json::json!(app_name);
            target["window_title"] = serde_json::json!(window_title);
            return Ok(target);
        }
    }
    anyhow::bail!("no matching onscreen current-user window found")
}
fn process(pid: i32) -> Option<BsdInfo> {
    let mut p = BsdInfo::default();
    let size = size_of::<BsdInfo>();
    let n = unsafe { proc_pidinfo(pid, 3, 0, (&mut p as *mut BsdInfo).cast(), size as i32) };
    if n == size as i32 && p.ids[3] == pid as u32 {
        Some(p)
    } else {
        None
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> Cf;
    fn AXUIElementCopyAttributeValue(element: Cf, attribute: Cf, value: *mut Cf) -> i32;
    fn AXUIElementGetPid(element: Cf, pid: *mut i32) -> i32;
    fn AXUIElementGetTypeID() -> usize;
    fn AXUIElementSetMessagingTimeout(element: Cf, timeout: f32) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: Cf, kind: u32, output: *mut c_void) -> bool;
    fn _AXUIElementGetWindow(element: Cf, window: *mut u32) -> i32;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(value: Cf) -> Cf;
    fn CFArrayGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringGetLength(the_string: Cf) -> isize;
    fn CFStringGetCharacterAtIndex(the_string: Cf, idx: isize) -> u16;
    fn CFStringGetBytes(
        the_string: Cf,
        range: CFRange,
        encoding: u32,
        loss_byte: u8,
        is_external_representation: bool,
        buffer: *mut u8,
        max_buf_len: isize,
        used_buf_len: *mut isize,
    ) -> isize;
    fn CFNumberIsFloatType(number: Cf) -> bool;
}
struct OwnedCf(Cf);
impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0);
        }
    }
}
const MAX_AX_DEPTH: usize = 20;
const MAX_AX_ELEMENTS: usize = 200;
const MAX_AX_VISITS: usize = 2000;
const MAX_AX_DURATION: Duration = Duration::from_millis(350);

fn ax_attribute(element: Cf, name: &std::ffi::CStr) -> Option<OwnedCf> {
    let key = unsafe { CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), 0x08000100) };
    if key.is_null() {
        return None;
    }
    let key = OwnedCf(key);
    let mut value = std::ptr::null();
    let error = unsafe { AXUIElementCopyAttributeValue(element, key.0, &mut value) };
    if error == 0 && !value.is_null() {
        Some(OwnedCf(value))
    } else {
        None
    }
}
fn cf_string(v: Cf, max_units: usize) -> Option<String> {
    if v.is_null() || unsafe { CFGetTypeID(v) != CFStringGetTypeID() } {
        return None;
    }
    let total = unsafe { CFStringGetLength(v) };
    if total <= 0 {
        return Some(String::new());
    }
    let mut len = total.min(max_units as isize);
    if len < total && len > 0 {
        let last = unsafe { CFStringGetCharacterAtIndex(v, len - 1) };
        if (0xD800..=0xDBFF).contains(&last) {
            len -= 1;
        }
    }
    if len <= 0 {
        return Some(String::new());
    }
    let mut buf = vec![0u8; (len as usize) * 4];
    let mut used = 0isize;
    let converted = unsafe {
        CFStringGetBytes(
            v,
            CFRange {
                location: 0,
                length: len,
            },
            0x08000100,
            0,
            false,
            buf.as_mut_ptr(),
            buf.len() as isize,
            &mut used,
        )
    };
    if converted <= 0 || used < 0 || used as usize > buf.len() {
        return None;
    }
    buf.truncate(used as usize);
    String::from_utf8(buf).ok()
}
fn ax_string(element: Cf, name: &std::ffi::CStr, max_units: usize) -> Option<String> {
    let v = ax_attribute(element, name)?;
    cf_string(v.0, max_units)
}
fn ax_bool(element: Cf, name: &std::ffi::CStr) -> Option<bool> {
    let v = ax_attribute(element, name)?;
    if !v.0.is_null() && unsafe { CFGetTypeID(v.0) == CFBooleanGetTypeID() } {
        Some(unsafe { CFBooleanGetValue(v.0) })
    } else {
        None
    }
}
fn ax_value(element: Cf) -> (Option<String>, Option<i64>, Option<bool>) {
    let Some(v) = ax_attribute(element, c"AXValue") else {
        return (None, None, None);
    };
    let tid = unsafe { CFGetTypeID(v.0) };
    if tid == unsafe { CFStringGetTypeID() } {
        return (cf_string(v.0, 240), None, None);
    }
    if tid == unsafe { CFBooleanGetTypeID() } {
        let b = unsafe { CFBooleanGetValue(v.0) };
        return (Some(if b { "true" } else { "false" }.into()), None, Some(b));
    }
    if tid == unsafe { CFNumberGetTypeID() } {
        if unsafe { CFNumberIsFloatType(v.0) } {
            let mut out = 0.0f64;
            if unsafe { CFNumberGetValue(v.0, 13, (&mut out as *mut f64).cast()) } && out.is_finite()
            {
                let formatted = format!("{out:.3}");
                let trimmed = formatted
                    .trim_end_matches('0')
                    .trim_end_matches('.')
                    .to_string();
                return (Some(trimmed), None, None);
            }
        } else {
            let mut out = 0i64;
            if unsafe { CFNumberGetValue(v.0, 4, (&mut out as *mut i64).cast()) } {
                return (Some(out.to_string()), Some(out), None);
            }
        }
    }
    (None, None, None)
}
fn ax_rect_f64(element: Cf) -> Option<[f64; 4]> {
    use core_graphics::geometry::{CGPoint, CGSize};
    let position = ax_attribute(element, c"AXPosition")?;
    let size = ax_attribute(element, c"AXSize")?;
    if unsafe {
        CFGetTypeID(position.0) != AXValueGetTypeID() || CFGetTypeID(size.0) != AXValueGetTypeID()
    } {
        return None;
    }
    let mut point = CGPoint::new(0.0, 0.0);
    let mut dimensions = CGSize::new(0.0, 0.0);
    if !unsafe {
        AXValueGetValue(position.0, 1, (&mut point as *mut CGPoint).cast())
            && AXValueGetValue(size.0, 2, (&mut dimensions as *mut CGSize).cast())
    } {
        return None;
    }
    if !point.x.is_finite()
        || !point.y.is_finite()
        || !dimensions.width.is_finite()
        || !dimensions.height.is_finite()
        || dimensions.width <= 0.0
        || dimensions.height <= 0.0
        || point.x.abs() > 1_000_000.0
        || point.y.abs() > 1_000_000.0
        || dimensions.width > 16384.0
        || dimensions.height > 16384.0
    {
        return None;
    }
    Some([point.x, point.y, dimensions.width, dimensions.height])
}
pub(super) fn logical_rect_to_frame_bounds(
    el_rect: [f64; 4],
    win_pos: [i32; 2],
    win_size: [u32; 2],
    frame_size: [u32; 2],
) -> Option<[u32; 4]> {
    if win_size[0] == 0 || win_size[1] == 0 || frame_size[0] == 0 || frame_size[1] == 0 {
        return None;
    }
    let [ex, ey, ew, eh] = el_rect;
    if !ex.is_finite()
        || !ey.is_finite()
        || !ew.is_finite()
        || !eh.is_finite()
        || ew <= 0.0
        || eh <= 0.0
    {
        return None;
    }
    let lx0 = (ex - f64::from(win_pos[0])).clamp(0.0, f64::from(win_size[0]));
    let ly0 = (ey - f64::from(win_pos[1])).clamp(0.0, f64::from(win_size[1]));
    let lx1 = (ex + ew - f64::from(win_pos[0])).clamp(0.0, f64::from(win_size[0]));
    let ly1 = (ey + eh - f64::from(win_pos[1])).clamp(0.0, f64::from(win_size[1]));
    if lx1 <= lx0 || ly1 <= ly0 {
        return None;
    }
    let sx = f64::from(frame_size[0]) / f64::from(win_size[0]);
    let sy = f64::from(frame_size[1]) / f64::from(win_size[1]);
    let px0 = (lx0 * sx).round().clamp(0.0, f64::from(frame_size[0])) as u32;
    let py0 = (ly0 * sy).round().clamp(0.0, f64::from(frame_size[1])) as u32;
    let px1 = (lx1 * sx).round().clamp(0.0, f64::from(frame_size[0])) as u32;
    let py1 = (ly1 * sy).round().clamp(0.0, f64::from(frame_size[1])) as u32;
    (px1 > px0 && py1 > py0).then_some([px0, py0, px1, py1])
}
fn is_interactive_ax(role: &str, subrole: Option<&str>) -> bool {
    matches!(
        role,
        "AXButton"
            | "AXCheckBox"
            | "AXRadioButton"
            | "AXPopUpButton"
            | "AXComboBox"
            | "AXMenuButton"
            | "AXMenuItem"
            | "AXMenuBarItem"
            | "AXTextField"
            | "AXSecureTextField"
            | "AXTextArea"
            | "AXSearchField"
            | "AXSlider"
            | "AXIncrementor"
            | "AXTab"
            | "AXTabButton"
            | "AXLink"
            | "AXDisclosureTriangle"
            | "AXSegmentedControl"
            | "AXColorWell"
            | "AXSwitch"
            | "AXCell"
            | "AXRow"
            | "AXOutlineRow"
            | "AXToolbarItem"
            | "AXDateField"
            | "AXTimeField"
            | "AXStepper"
    ) || matches!(
        subrole,
        Some(
            "AXSecureTextField"
                | "AXSwitch"
                | "AXToggle"
                | "AXCloseButton"
                | "AXMinimizeButton"
                | "AXZoomButton"
                | "AXFullScreenButton"
                | "AXSearchField"
        )
    )
}
fn is_title_ax(role: &str) -> bool {
    matches!(
        role,
        "AXStaticText" | "AXHeading" | "AXWindow" | "AXImage" | "AXDialog" | "AXSheet"
    )
}
#[allow(clippy::too_many_arguments)]
pub(super) fn build_semantic_element(
    ref_id: String,
    role: &str,
    subrole: Option<&str>,
    title: Option<String>,
    description: Option<String>,
    value_raw: (Option<String>, Option<i64>, Option<bool>),
    enabled: bool,
    focused: bool,
    selected: Option<bool>,
    bounds: [u32; 4],
) -> Option<SemanticElement> {
    let is_secure = role == "AXSecureTextField" || subrole == Some("AXSecureTextField");
    let effective_role = if is_secure {
        "AXSecureTextField".to_string()
    } else if subrole == Some("AXSwitch") {
        "AXSwitch".to_string()
    } else {
        role.to_string()
    };
    let interactive = is_interactive_ax(&effective_role, subrole);
    let is_title = is_title_ax(&effective_role);
    let (raw_str, raw_int, raw_bool) = value_raw;
    let value = if is_secure {
        Some("<redacted>".to_string())
    } else {
        raw_str.filter(|s| !s.is_empty())
    };
    let mut name = title
        .filter(|s| !s.is_empty())
        .or_else(|| description.filter(|s| !s.is_empty()))
        .unwrap_or_default();
    if name.is_empty()
        && !is_secure
        && matches!(effective_role.as_str(), "AXStaticText" | "AXHeading")
        && let Some(v) = &value
    {
        name = v.clone();
    }
    if !interactive && (!is_title || (name.is_empty() && value.is_none())) {
        return None;
    }
    let is_checkable = matches!(
        effective_role.as_str(),
        "AXCheckBox" | "AXRadioButton" | "AXSwitch"
    ) || matches!(subrole, Some("AXSwitch" | "AXToggle"));
    let checked = if is_checkable {
        raw_bool.or_else(|| {
            raw_int.and_then(|n| match n {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            })
        })
    } else {
        None
    };
    Some(SemanticElement {
        ref_id,
        role: effective_role,
        name,
        value,
        bounds,
        interactive,
        enabled,
        focused,
        checked,
        selected,
    })
}
fn ax_geometry(element: Cf) -> Option<([u32; 2], [i32; 2])> {
    use core_graphics::geometry::{CGPoint, CGSize};
    let position = ax_attribute(element, c"AXPosition")?;
    let size = ax_attribute(element, c"AXSize")?;
    if unsafe {
        CFGetTypeID(position.0) != AXValueGetTypeID() || CFGetTypeID(size.0) != AXValueGetTypeID()
    } {
        return None;
    }
    let mut point = CGPoint::new(0.0, 0.0);
    let mut dimensions = CGSize::new(0.0, 0.0);
    if !unsafe {
        AXValueGetValue(position.0, 1, (&mut point as *mut CGPoint).cast())
            && AXValueGetValue(size.0, 2, (&mut dimensions as *mut CGSize).cast())
    } {
        return None;
    }
    if !point.x.is_finite()
        || !point.y.is_finite()
        || !dimensions.width.is_finite()
        || !dimensions.height.is_finite()
        || point.x.abs() > 1_000_000.0
        || point.y.abs() > 1_000_000.0
        || !(2.0..=8192.0).contains(&dimensions.width)
        || !(2.0..=8192.0).contains(&dimensions.height)
    {
        return None;
    }
    Some((
        [
            dimensions.width.round() as u32,
            dimensions.height.round() as u32,
        ],
        [point.x.round() as i32, point.y.round() as i32],
    ))
}
/// Retain the original AX window object. A new window cannot be silently rebound
/// just because process metadata and a server window number happen to match.
pub(super) struct Lifetime {
    element: OwnedCf,
}
impl Lifetime {
    pub fn bind(target: Target) -> Result<Self> {
        let app = unsafe { AXUIElementCreateApplication(target.pid) };
        ensure!(!app.is_null(), "window accessibility unavailable");
        let app = OwnedCf(app);
        ensure!(
            unsafe { AXUIElementSetMessagingTimeout(app.0, 0.25) } == 0,
            "bounded accessibility messaging unavailable"
        );
        let windows = ax_attribute(app.0, c"AXWindows")
            .ok_or_else(|| anyhow::anyhow!("window accessibility unavailable"))?;
        let expected_geom = target
            .geometry()
            .ok_or_else(|| anyhow::anyhow!("target window unavailable"))?;
        let count = unsafe {
            ensure!(
                CFGetTypeID(windows.0) == CFArrayGetTypeID() && CFArrayGetCount(windows.0) >= 1,
                "window accessibility unavailable"
            );
            CFArrayGetCount(windows.0)
        };
        let mut exact_window = None;
        let mut geom_matches = Vec::new();
        for idx in 0..count {
            let candidate = unsafe { CFArrayGetValueAtIndex(windows.0, idx) };
            if candidate.is_null() || unsafe { CFGetTypeID(candidate) != AXUIElementGetTypeID() } {
                continue;
            }
            let mut wid = 0u32;
            if unsafe { _AXUIElementGetWindow(candidate, &mut wid) } == 0 && wid == target.window {
                exact_window = Some(candidate);
                break;
            }
            if ax_geometry(candidate) == Some(expected_geom) {
                geom_matches.push(candidate);
            }
        }
        let element = exact_window
            .or_else(|| {
                if geom_matches.len() == 1 {
                    Some(geom_matches[0])
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow::anyhow!("exact accessibility window not found"))?;
        let element = OwnedCf(unsafe { CFRetain(element) });
        ensure!(
            unsafe { AXUIElementSetMessagingTimeout(element.0, 0.25) } == 0,
            "bounded accessibility messaging unavailable"
        );
        let value = Self { element };
        ensure!(value.live(target), "accessibility window geometry conflict");
        Ok(value)
    }
    pub fn live(&self, target: Target) -> bool {
        let mut pid = 0;
        let mut wid = 0u32;
        let id_ok =
            unsafe { _AXUIElementGetWindow(self.element.0, &mut wid) } != 0 || wid == target.window;
        target.process_live()
            && unsafe { AXUIElementGetPid(self.element.0, &mut pid) } == 0
            && pid == target.pid
            && id_ok
            && target
                .geometry()
                .is_some_and(|g| ax_geometry(self.element.0) == Some(g))
    }
    /// Bounded recursive AX walk under the bound window element (keeping its 0.25s timeout).
    pub fn semantic_elements(
        &self,
        win_pos: [i32; 2],
        win_size: [u32; 2],
        frame_size: [u32; 2],
    ) -> Vec<SemanticElement> {
        struct WalkCtx {
            win_pos: [i32; 2],
            win_size: [u32; 2],
            frame_size: [u32; 2],
            started: Instant,
            visited: usize,
            elements: Vec<SemanticElement>,
        }
        fn walk(element: Cf, depth: usize, ctx: &mut WalkCtx) {
            if depth > MAX_AX_DEPTH
                || ctx.elements.len() >= MAX_AX_ELEMENTS
                || ctx.visited >= MAX_AX_VISITS
                || ctx.started.elapsed() >= MAX_AX_DURATION
            {
                return;
            }
            ctx.visited += 1;
            if ax_bool(element, c"AXHidden") == Some(true) {
                return;
            }
            let Some(role) = ax_string(element, c"AXRole", 64) else {
                return;
            };
            if let Some(rect) = ax_rect_f64(element)
                && let Some(bounds) =
                    logical_rect_to_frame_bounds(rect, ctx.win_pos, ctx.win_size, ctx.frame_size)
            {
                let subrole = ax_string(element, c"AXSubrole", 64);
                let interactive = is_interactive_ax(&role, subrole.as_deref());
                let is_title = is_title_ax(&role);
                if interactive || is_title {
                    let title = ax_string(element, c"AXTitle", 160);
                    let description = ax_string(element, c"AXDescription", 160);
                    let is_secure =
                        role == "AXSecureTextField" || subrole.as_deref() == Some("AXSecureTextField");
                    let value_raw = if is_secure {
                        (None, None, None)
                    } else {
                        ax_value(element)
                    };
                    let enabled = ax_bool(element, c"AXEnabled").unwrap_or(true);
                    let focused = ax_bool(element, c"AXFocused").unwrap_or(false);
                    let selected = ax_bool(element, c"AXSelected");
                    let ref_id = format!("n{}", ctx.elements.len() + 1);
                    if let Some(item) = build_semantic_element(
                        ref_id,
                        &role,
                        subrole.as_deref(),
                        title,
                        description,
                        value_raw,
                        enabled,
                        focused,
                        selected,
                        bounds,
                    ) {
                        ctx.elements.push(item);
                        if ctx.elements.len() >= MAX_AX_ELEMENTS {
                            return;
                        }
                    }
                }
            }
            if depth >= MAX_AX_DEPTH || ctx.started.elapsed() >= MAX_AX_DURATION {
                return;
            }
            let Some(children) = ax_attribute(element, c"AXChildren") else {
                return;
            };
            if unsafe { CFGetTypeID(children.0) != CFArrayGetTypeID() } {
                return;
            }
            let count = unsafe { CFArrayGetCount(children.0) };
            for idx in 0..count {
                if ctx.elements.len() >= MAX_AX_ELEMENTS
                    || ctx.visited >= MAX_AX_VISITS
                    || ctx.started.elapsed() >= MAX_AX_DURATION
                {
                    break;
                }
                let child = unsafe { CFArrayGetValueAtIndex(children.0, idx) };
                if !child.is_null() && unsafe { CFGetTypeID(child) == AXUIElementGetTypeID() } {
                    let _ = unsafe { AXUIElementSetMessagingTimeout(child, 0.25) };
                    walk(child, depth + 1, ctx);
                }
            }
        }
        let mut ctx = WalkCtx {
            win_pos,
            win_size,
            frame_size,
            started: Instant::now(),
            visited: 0,
            elements: Vec::new(),
        };
        walk(self.element.0, 0, &mut ctx);
        ctx.elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn target_identity_rejects_global_desktop_and_malformed_lifetime() {
        for value in [
            "console",
            "windowP0S1W2",
            "windowP1S0W2",
            "windowP1S2W0",
            "windowP1S2W3junk",
        ] {
            assert!(Target::parse(value).is_err());
        }
        let t = Target::parse("windowP123S456W789").unwrap();
        assert_eq!((t.pid, t.started_us, t.window), (123, 456, 789));
    }
    #[test]
    fn darwin_process_identity_layout_matches_public_sdk() {
        assert_eq!(size_of::<BsdInfo>(), 136);
    }
    #[test]
    fn logical_bounds_convert_and_clamp_to_captured_raster_pixels() {
        assert_eq!(
            logical_rect_to_frame_bounds([150.0, 250.0, 100.0, 40.0], [100, 200], [1000, 800], [1000, 800]),
            Some([50, 50, 150, 90])
        );
        // Scaled odd->even raster dimensions and clipped partially offscreen control.
        assert_eq!(
            logical_rect_to_frame_bounds([90.0, 190.0, 30.0, 20.0], [100, 200], [100, 50], [200, 100]),
            Some([0, 0, 40, 20])
        );
        // Completely outside window or zero-sized.
        assert_eq!(
            logical_rect_to_frame_bounds([0.0, 0.0, 50.0, 50.0], [100, 200], [500, 400], [500, 400]),
            None
        );
        assert_eq!(
            logical_rect_to_frame_bounds([120.0, 220.0, 0.0, 10.0], [100, 200], [500, 400], [500, 400]),
            None
        );
    }
    #[test]
    fn semantic_element_redacts_secure_fields_and_tracks_checked_states() {
        let secure_subrole = build_semantic_element(
            "n1".into(),
            "AXTextField",
            Some("AXSecureTextField"),
            Some("Password".into()),
            None,
            (Some("hunter2".into()), None, None),
            true,
            true,
            None,
            [10, 20, 210, 50],
        )
        .unwrap();
        assert_eq!(secure_subrole.role, "AXSecureTextField");
        assert_eq!(secure_subrole.value.as_deref(), Some("<redacted>"));
        assert!(secure_subrole.interactive && secure_subrole.focused);

        let secure_role = build_semantic_element(
            "n2".into(),
            "AXSecureTextField",
            None,
            None,
            Some("PIN".into()),
            (None, None, None),
            true,
            false,
            None,
            [10, 60, 210, 90],
        )
        .unwrap();
        assert_eq!(secure_role.name, "PIN");
        assert_eq!(secure_role.value.as_deref(), Some("<redacted>"));

        let checkbox = build_semantic_element(
            "n3".into(),
            "AXCheckBox",
            None,
            Some("Remember me".into()),
            None,
            (Some("1".into()), Some(1), None),
            true,
            false,
            Some(true),
            [10, 100, 120, 120],
        )
        .unwrap();
        assert_eq!(checkbox.checked, Some(true));
        assert_eq!(checkbox.selected, Some(true));

        assert!(
            build_semantic_element(
                "n4".into(),
                "AXGroup",
                None,
                None,
                None,
                (None, None, None),
                true,
                false,
                None,
                [0, 0, 100, 100],
            )
            .is_none()
        );
    }
}
