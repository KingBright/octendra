//! Lifetime-bound, single-window macOS target. Uses public metadata only.
use anyhow::{Result, ensure};
use std::{ffi::c_void, mem::size_of};
type Cf = *const c_void;
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
    /// Single visible window only. Extra windows or popups are not silently targeted.
    pub fn geometry(&self) -> Option<([u32; 2], [i32; 2])> {
        if !self.process_live() {
            return None;
        }
        unsafe {
            let list = CGWindowListCopyWindowInfo(0, 0);
            if list.is_null() {
                return None;
            }
            let mut visible = 0;
            let mut geometry = None;
            for index in 0..CFArrayGetCount(list) {
                let d = CFArrayGetValueAtIndex(list, index);
                if number(d, c"kCGWindowOwnerPID") != Some(f64::from(self.pid)) {
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
                visible += 1;
                if number(d, c"kCGWindowNumber") == Some(f64::from(self.window))
                    && let (Some(x), Some(y)) = (number(bounds, c"X"), number(bounds, c"Y"))
                {
                    geometry = Some(([w as u32, h as u32], [x as i32, y as i32]));
                }
            }
            CFRelease(list);
            if visible == 1 { geometry } else { None }
        }
    }
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
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(value: Cf) -> Cf;
    fn CFArrayGetTypeID() -> usize;
}
struct OwnedCf(Cf);
impl Drop for OwnedCf {
    fn drop(&mut self) {
        unsafe {
            CFRelease(self.0);
        }
    }
}
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
        ensure!(
            unsafe {
                CFGetTypeID(windows.0) == CFArrayGetTypeID() && CFArrayGetCount(windows.0) == 1
            },
            "single accessibility window required"
        );
        let element = unsafe { CFArrayGetValueAtIndex(windows.0, 0) };
        ensure!(
            !element.is_null() && unsafe { CFGetTypeID(element) == AXUIElementGetTypeID() },
            "window accessibility unavailable"
        );
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
        target.process_live()
            && unsafe { AXUIElementGetPid(self.element.0, &mut pid) } == 0
            && pid == target.pid
            && target
                .geometry()
                .is_some_and(|g| ax_geometry(self.element.0) == Some(g))
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
}
