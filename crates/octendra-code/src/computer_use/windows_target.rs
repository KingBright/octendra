//! Read-only window inspection and explicit approved-window lifetime preparation.
//! This module never injects input, activates windows, or captures another window.
//! WGC pixels and UIA semantic operations must be verified before admission is enabled.
use super::windows_session::WindowAuthority;
use anyhow::{Context, Result, ensure};
use std::{mem::size_of, ptr};
use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, RECT, WAIT_TIMEOUT},
    Security::{EqualSid, GetTokenInformation, IsValidSid, TOKEN_QUERY, TOKEN_USER, TokenUser},
    System::{
        RemoteDesktop::ProcessIdToSessionId,
        Threading::{
            GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
    },
    UI::WindowsAndMessaging::{
        EnumWindows, GetPropW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, RemovePropW,
        SetPropW,
    },
};
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn token_user(process: HANDLE) -> Result<Vec<usize>> {
    let mut token = ptr::null_mut();
    ensure!(
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } != 0,
        "target process token unavailable"
    );
    let token = OwnedHandle(token);
    let mut bytes = 0;
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut bytes);
    }
    ensure!(
        (size_of::<TOKEN_USER>()..=65536).contains(&(bytes as usize)),
        "token metadata bound"
    );
    let mut data = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    ensure!(
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                data.as_mut_ptr().cast(),
                bytes,
                &mut bytes,
            )
        } != 0,
        "target process token unavailable"
    );
    let sid = unsafe { (*(data.as_ptr().cast::<TOKEN_USER>())).User.Sid };
    let start = data.as_ptr() as usize;
    let end = start + data.len() * size_of::<usize>();
    ensure!(
        !sid.is_null()
            && (sid as usize) >= start
            && (sid as usize).checked_add(8).is_some_and(|v| v <= end),
        "token user pointer bound"
    );
    // A SID's first two bytes contain revision and subauthority count. Validate
    // the complete variable-sized value before calling an API that dereferences it.
    let count = unsafe { *((sid as *const u8).add(1)) } as usize;
    ensure!(
        (sid as usize)
            .checked_add(8 + 4 * count)
            .is_some_and(|v| v <= end)
            && unsafe { IsValidSid(sid) } != 0,
        "invalid target process user"
    );
    Ok(data)
}
fn sid(data: &[usize]) -> windows_sys::Win32::Security::PSID {
    unsafe { (*(data.as_ptr().cast::<TOKEN_USER>())).User.Sid }
}
fn creation(process: HANDLE) -> Result<u64> {
    let mut values = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    let [created, exited, kernel, user] = &mut values;
    ensure!(
        unsafe { GetProcessTimes(process, created, exited, kernel, user) } != 0,
        "target process creation time unavailable"
    );
    let value = u64::from(created.dwLowDateTime) | (u64::from(created.dwHighDateTime) << 32);
    ensure!(value > 0, "target process creation time invalid");
    Ok(value)
}
fn session(pid: u32) -> Result<u32> {
    let mut value = 0;
    ensure!(
        unsafe { ProcessIdToSessionId(pid, &mut value) } != 0 && value != 0,
        "interactive target session required"
    );
    Ok(value)
}
fn geometry(window: HWND) -> Option<([u32; 2], [i32; 2])> {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { IsWindowVisible(window) == 0 || IsIconic(window) != 0 } {
        return None;
    }
    // DWM bounds are physical pixels and exclude invisible resize borders.
    // WGC must report exactly these dimensions before input can be admitted.
    if unsafe {
        windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
            windows::Win32::Foundation::HWND(window),
            windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut r as *mut RECT).cast(),
            size_of::<RECT>() as u32,
        )
    }
    .is_err()
    {
        return None;
    }
    let width = i64::from(r.right) - i64::from(r.left);
    let height = i64::from(r.bottom) - i64::from(r.top);
    if !(2..=8192).contains(&width)
        || !(2..=8192).contains(&height)
        || r.left.unsigned_abs() > 1_000_000
        || r.top.unsigned_abs() > 1_000_000
    {
        return None;
    }
    Some(([width as u32, height as u32], [r.left, r.top]))
}
struct WindowCount {
    pid: u32,
    count: u32,
}
unsafe extern "system" fn count_window(window: HWND, value: LPARAM) -> i32 {
    let state = unsafe { &mut *(value as *mut WindowCount) };
    let mut pid = 0;
    if unsafe { GetWindowThreadProcessId(window, &mut pid) } != 0
        && pid == state.pid
        && geometry(window).is_some()
    {
        state.count = state.count.saturating_add(1);
    }
    1
}
/// Pins the process object; PID reuse cannot redirect the scope to a new process.
pub struct Target {
    process: OwnedHandle,
    pid: u32,
    window: HWND,
    created_100ns: u64,
    session: u32,
    marker: Option<(Vec<u16>, usize)>,
}
impl Target {
    pub fn bind_expected(expected: &WindowAuthority) -> Result<Self> {
        ensure!(
            super::windows_authority::current_sid()? == expected.owner_sid,
            "target SID changed"
        );
        let mut target = Self::bind(expected.process_id, expected.window_id)?;
        ensure!(
            target.created_100ns == expected.process_created_100ns
                && target.session == expected.session_id,
            "target process/session changed"
        );
        ensure!(
            expected.scope_key.starts_with("Octendra.Window.")
                && expected.scope_key.len() <= 128
                && expected
                    .scope_key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
                && expected.scope_value > 0
                && expected.scope_value <= usize::MAX as u64,
            "window scope marker invalid"
        );
        let key: Vec<u16> = expected.scope_key.encode_utf16().chain(Some(0)).collect();
        ensure!(
            unsafe { GetPropW(target.window, key.as_ptr()) } as usize
                == expected.scope_value as usize,
            "window object replaced or authority revoked"
        );
        target.marker = Some((key, expected.scope_value as usize));
        Ok(target)
    }
    pub fn bind(pid: u32, window: u64) -> Result<Self> {
        ensure!(
            pid > 0 && window > 0 && window <= usize::MAX as u64,
            "target identity bounds"
        );
        let window = window as usize as HWND;
        let process = OwnedHandle(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        });
        ensure!(!process.0.is_null(), "target process unavailable");
        let ours = token_user(unsafe { GetCurrentProcess() })?;
        let theirs = token_user(process.0)?;
        ensure!(
            unsafe { EqualSid(sid(&ours), sid(&theirs)) } != 0,
            "target must belong to current user"
        );
        let session = session(pid)?;
        // The caller's session is checked through its process ID, without exposing its SID.
        let current_pid = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
        ensure!(
            session == self::session(current_pid)?,
            "target must belong to current session"
        );
        let created_100ns = creation(process.0)?;
        ensure!(
            super::windows_authority::input_desktop_available(),
            "interactive input desktop unavailable"
        );
        let ours = super::windows_authority::integrity(unsafe { GetCurrentProcess() })?;
        let theirs = super::windows_authority::integrity(process.0)?;
        ensure!(
            ours < 0x3000 && theirs <= ours,
            "elevated/protected target is outside desktop authority"
        );
        let target = Self {
            process,
            pid,
            window,
            created_100ns,
            session,
            marker: None,
        };
        target
            .live_geometry()
            .context("target window unavailable")?;
        Ok(target)
    }
    pub fn live_geometry(&self) -> Option<([u32; 2], [i32; 2])> {
        if self.marker.as_ref().is_some_and(
            |(key, value)| unsafe { GetPropW(self.window, key.as_ptr()) } as usize != *value,
        ) || !super::windows_authority::input_desktop_available()
            || unsafe { WaitForSingleObject(self.process.0, 0) } != WAIT_TIMEOUT
            || creation(self.process.0).ok() != Some(self.created_100ns)
            || session(self.pid).ok() != Some(self.session)
        {
            return None;
        }
        let mut actual = 0;
        if unsafe { GetWindowThreadProcessId(self.window, &mut actual) } == 0 || actual != self.pid
        {
            return None;
        }
        let mut count = WindowCount {
            pid: self.pid,
            count: 0,
        };
        if unsafe {
            EnumWindows(
                Some(count_window),
                (&mut count as *mut WindowCount) as LPARAM,
            )
        } == 0
            || count.count != 1
        {
            return None;
        }
        geometry(self.window)
    }
}
/// Explicitly approved-window preparation only: lifetime metadata, never a device
/// grant or desktop-launch authority. This is not called during Agent startup.
pub fn prepare_authority(pid: u32, window: u64, path: &std::path::Path) -> Result<()> {
    use std::io::Write;
    let target = Target::bind(pid, window)?;
    let scope_key = format!("Octendra.Window.{}", uuid::Uuid::new_v4());
    // The UUID property name pins the object lifetime. A unit value avoids
    // lossy UInt64 round-trips in JSON-based Windows provisioning clients.
    let scope_value = 1u64;
    let authority = WindowAuthority {
        owner_sid: super::windows_authority::current_sid()?,
        session_id: target.session,
        process_id: pid,
        process_created_100ns: target.created_100ns,
        window_id: window,
        scope_key: scope_key.clone(),
        scope_value,
    };
    let key: Vec<u16> = scope_key.encode_utf16().chain(Some(0)).collect();
    let mut file = super::windows_authority::create_private(path)?;
    ensure!(
        unsafe { GetPropW(target.window, key.as_ptr()) }.is_null(),
        "window scope marker exists"
    );
    ensure!(
        unsafe { SetPropW(target.window, key.as_ptr(), scope_value as usize as HANDLE) } != 0,
        "window scope marker denied"
    );
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec_pretty(&authority)?)?;
        file.sync_all()?;
        Target::bind_expected(&authority)?;
        Ok(())
    })();
    if result.is_err()
        && unsafe { GetPropW(target.window, key.as_ptr()) } as usize == scope_value as usize
    {
        unsafe {
            RemovePropW(target.window, key.as_ptr());
        }
    }
    result
}
pub fn window_target(pid: u32, window: u64) -> Result<serde_json::Value> {
    let target = Target::bind(pid, window)?;
    let (size, position) = target.live_geometry().context("target window changed")?;
    Ok(serde_json::json!({
        "process_id":pid,"window_id":window,"process_created_100ns":target.created_100ns,
        "desktop_session_id":target.session,"logical_size":size,"logical_position":position,
        "geometry_coordinate_space":"DWM physical pixels; WGC dimensions must match",
        "scope":"current_user_current_session_single_window",
        "input_backend":"uia_standard_control_click_only","capture_backend":"wgc_window",
        "available":false,"noninterference_verified":false,
        "same_application_concurrency_supported":false
    }))
}
