//! Existing-current-user Windows authority. Never repairs an ACL or elevates.
//! Private ACLs apply only to newly-created worker/frame/lock files.
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::Read,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, Prefix},
    ptr,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                GetSecurityInfo, SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, GetAce, GetTokenInformation, IsValidSid,
            OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
            TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{PCWSTR, PWSTR},
};

struct Allocation(*mut core::ffi::c_void);
impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.0)));
        }
    }
}
fn sid_string(sid: PSID) -> Result<String> {
    ensure!(
        !sid.0.is_null() && unsafe { IsValidSid(sid) }.as_bool(),
        "invalid SID"
    );
    let mut text = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(sid, &mut text)?;
    }
    let _allocation = Allocation(text.0.cast());
    Ok(unsafe { text.to_string()? })
}
pub fn current_sid() -> Result<String> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
    }
    let result = (|| {
        let mut length = 0;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut length) };
        ensure!(
            (size_of::<TOKEN_USER>()..=65536).contains(&(length as usize)),
            "token bound"
        );
        let mut bytes = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(bytes.as_mut_ptr().cast()),
                length,
                &mut length,
            )?;
        }
        let sid = unsafe { (*(bytes.as_ptr().cast::<TOKEN_USER>())).User.Sid };
        let start = bytes.as_ptr() as usize;
        let end = start + bytes.len() * size_of::<usize>();
        ensure!(
            (sid.0 as usize) >= start && (sid.0 as usize).checked_add(8).is_some_and(|n| n <= end),
            "token SID bound"
        );
        let count = unsafe { *((sid.0 as *const u8).add(1)) } as usize;
        ensure!(
            (sid.0 as usize)
                .checked_add(8 + 4 * count)
                .is_some_and(|n| n <= end),
            "token SID length"
        );
        sid_string(sid)
    })();
    unsafe {
        CloseHandle(token)?;
    }
    result
}
pub fn no_reparse(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "absolute local path required");
    ensure!(
        matches!(path.components().next(),Some(Component::Prefix(p)) if matches!(p.kind(),Prefix::Disk(_)|Prefix::VerbatimDisk(_))),
        "local disk path required"
    );
    for p in std::iter::once(path).chain(path.ancestors().skip(1)) {
        match std::fs::symlink_metadata(p) {
            Ok(m) => ensure!(m.file_attributes() & 0x400 == 0, "reparse path forbidden"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn validate_handle(file: &File, private: bool) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.file_attributes() & 0x400 == 0,
        "reparse handle forbidden"
    );
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            HANDLE(file.as_raw_handle()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            Some(&mut dacl),
            None,
            Some(&mut sd),
        )
    };
    ensure!(status.0 == 0, "file authority query failed");
    let _allocation = Allocation(sd.0);
    let ours = current_sid()?;
    ensure!(
        sid_string(owner)? == ours && !dacl.is_null(),
        "owned non-null DACL required"
    );
    let acl = unsafe { &*dacl };
    ensure!(
        acl.AclSize as usize >= size_of::<ACL>() && acl.AceCount <= 128,
        "DACL bound"
    );
    let start = dacl as usize;
    let end = start + acl.AclSize as usize;
    for i in 0..u32::from(acl.AceCount) {
        let mut ace = ptr::null_mut();
        unsafe {
            GetAce(dacl, i, &mut ace)?;
        }
        ensure!(
            (ace as usize) >= start
                && (ace as usize)
                    .checked_add(size_of::<ACE_HEADER>())
                    .is_some_and(|n| n <= end),
            "ACE header bound"
        );
        let h = unsafe { &*(ace.cast::<ACE_HEADER>()) };
        ensure!(
            (ace as usize)
                .checked_add(h.AceSize as usize)
                .is_some_and(|n| n <= end),
            "ACE bound"
        );
        // Inherit-only ACEs do not grant rights on this object; deny ACEs cannot add authority.
        if h.AceFlags & 8 != 0 || h.AceType == 1 {
            continue;
        }
        ensure!(
            h.AceType == 0 && h.AceSize as usize >= size_of::<ACCESS_ALLOWED_ACE>() + 4,
            "unsupported granting ACE"
        );
        let allowed = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
        let raw = (&allowed.SidStart as *const u32).cast::<u8>();
        let count = unsafe { *raw.add(1) } as usize;
        ensure!(
            (raw as usize)
                .checked_add(8 + 4 * count)
                .is_some_and(|n| n <= ace as usize + h.AceSize as usize),
            "ACE SID bound"
        );
        let principal = sid_string(PSID(raw.cast_mut().cast()))?;
        let trusted = principal == ours || principal == "S-1-5-18" || principal == "S-1-5-32-544";
        ensure!(
            super::windows_session::file_grant_allowed(trusted, private, allowed.Mask),
            "untrusted file ACE grants authority"
        );
    }
    Ok(())
}
fn open(path: &Path, write: bool, create: bool, exclusive: bool) -> Result<File> {
    no_reparse(path)?;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let sddl = format!(
        "O:{}D:P(A;;FA;;;{})(A;;FA;;;SY)",
        current_sid()?,
        current_sid()?
    );
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    if create {
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                1,
                &mut descriptor,
                None,
            )?;
        }
    }
    let _allocation = (!descriptor.0.is_null()).then(|| Allocation(descriptor.0));
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_READ.0 | READ_CONTROL.0 | if write { GENERIC_WRITE.0 } else { 0 },
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            create.then_some(&attributes as *const _),
            if exclusive {
                CREATE_NEW
            } else if create {
                OPEN_ALWAYS
            } else {
                OPEN_EXISTING
            },
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )?
    };
    Ok(unsafe { File::from_raw_handle(handle.0) })
}
pub fn owned_file(path: &Path, mode: u32, max: u64) -> Result<Vec<u8>> {
    directory(path.parent().context("owned file parent")?)?;
    let mut f = open(path, false, false, false)?;
    validate_handle(&f, mode & 0o077 == 0)?;
    ensure!(
        f.metadata()?.is_file() && f.metadata()?.len() <= max,
        "owned file bound"
    );
    let mut b = Vec::new();
    std::io::Read::by_ref(&mut f)
        .take(max + 1)
        .read_to_end(&mut b)?;
    ensure!(b.len() as u64 <= max, "owned file grew");
    Ok(b)
}
pub fn directory(path: &Path) -> Result<()> {
    let f = open(path, false, false, false)?;
    ensure!(f.metadata()?.is_dir(), "owned directory required");
    validate_handle(&f, false)
}
pub fn create_private(path: &Path) -> Result<File> {
    directory(path.parent().context("private file parent")?)?;
    let f = open(path, true, true, true)?;
    validate_handle(&f, true)?;
    Ok(f)
}
pub fn lock(path: &Path) -> Result<File> {
    directory(path.parent().context("lock parent")?)?;
    let f = open(path, true, true, false)?;
    validate_handle(&f, true)?;
    fs2::FileExt::try_lock_exclusive(&f).context("desktop busy across Agents/tasks")?;
    Ok(f)
}

pub fn integrity(process: *mut core::ffi::c_void) -> Result<u32> {
    use windows::Win32::Security::{TOKEN_MANDATORY_LABEL, TokenIntegrityLevel};
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(HANDLE(process), TOKEN_QUERY, &mut token)?;
    }
    let result = (|| {
        let mut length = 0;
        let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut length) };
        ensure!(
            (size_of::<TOKEN_MANDATORY_LABEL>()..=65536).contains(&(length as usize)),
            "integrity token bound"
        );
        let mut b = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
        unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(b.as_mut_ptr().cast()),
                length,
                &mut length,
            )?;
        }
        let sid = unsafe { (*(b.as_ptr().cast::<TOKEN_MANDATORY_LABEL>())).Label.Sid };
        let start = b.as_ptr() as usize;
        let end = start + b.len() * size_of::<usize>();
        ensure!(
            (sid.0 as usize) >= start && (sid.0 as usize).checked_add(8).is_some_and(|n| n <= end),
            "integrity SID bound"
        );
        let n = unsafe { *((sid.0 as *const u8).add(1)) } as usize;
        ensure!(
            n > 0
                && (sid.0 as usize)
                    .checked_add(8 + 4 * n)
                    .is_some_and(|n| n <= end),
            "integrity SID length"
        );
        ensure!(
            unsafe { IsValidSid(sid) }.as_bool(),
            "integrity SID invalid"
        );
        Ok(unsafe {
            std::ptr::read_unaligned((sid.0 as *const u8).add(8 + 4 * (n - 1)).cast::<u32>())
        })
    })();
    unsafe {
        CloseHandle(token)?;
    }
    result
}
pub fn input_desktop_available() -> bool {
    use windows::Win32::System::{StationsAndDesktops::*, Threading::GetCurrentThreadId};
    fn name(handle: HANDLE) -> windows::core::Result<Vec<u16>> {
        let mut b = [0u16; 128];
        let mut length = 0;
        unsafe {
            GetUserObjectInformationW(
                handle,
                UOI_NAME,
                Some(b.as_mut_ptr().cast()),
                256,
                Some(&mut length),
            )?;
        }
        if length < 2 || length > 256 || length % 2 != 0 {
            return Err(windows::core::Error::from_thread());
        }
        Ok(b[..length as usize / 2].to_vec())
    }
    unsafe {
        let Ok(current) = GetThreadDesktop(GetCurrentThreadId()) else {
            return false;
        };
        let Ok(active) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
        else {
            return false;
        };
        let equal =
            matches!((name(HANDLE(current.0)),name(HANDLE(active.0))),(Ok(a),Ok(b)) if a==b);
        let closed = CloseDesktop(active).is_ok();
        equal && closed
    }
}
pub fn process_identity(pid: u32) -> serde_json::Value {
    use windows::Win32::{
        Foundation::FILETIME,
        System::{
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    };
    let observed = (|| -> windows::core::Result<_> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
            let mut values = [FILETIME::default(); 4];
            let [created, exit, kernel, user] = &mut values;
            let times = GetProcessTimes(h, created, exit, kernel, user);
            let closed = CloseHandle(h);
            times?;
            closed?;
            let mut session = 0;
            ProcessIdToSessionId(pid, &mut session)?;
            Ok((
                u64::from(values[0].dwLowDateTime) | (u64::from(values[0].dwHighDateTime) << 32),
                session,
            ))
        }
    })();
    match observed {
        Ok((created, session)) => {
            serde_json::json!({"pid":pid,"identity_state":"observed","created_100ns":created,"session_id":session})
        }
        Err(_) => serde_json::json!({"pid":pid,"identity_state":"unavailable"}),
    }
}
pub fn validate_launch(a: &super::transport::LaunchApproval, device: &str) -> Result<()> {
    use super::windows_target::Target;
    ensure!(
        a.enabled && a.observe && a.input && a.capability_version > 0 && a.weavelo.is_none(),
        "Windows desktop authority unavailable"
    );
    ensure!(
        a.identity.device_id.to_string() == device
            && !a.identity.owner.is_empty()
            && a.identity.owner.len() <= 128
            && a.desktop_uid == 0
            && a.identity.desktop_id == a.windows.desktop_id()
            && a.desktop_session == a.windows.session_id.to_string(),
        "Windows desktop identity mismatch"
    );
    ensure!(
        a.max_frame_bytes > 0
            && a.max_frame_bytes <= octendra_desktop_protocol::MAX_WORKER_PIXELS
            && a.logical_size.iter().all(|n| *n > 0 && *n <= 8192)
            && a.logical_position
                .iter()
                .all(|n| n.unsigned_abs() <= 1_000_000),
        "desktop geometry/budget"
    );
    ensure!(
        (a.logical_size[0] as usize)
            .checked_mul(a.logical_size[1] as usize)
            .and_then(|n| n.checked_mul(4))
            .is_some_and(|n| n <= a.max_frame_bytes),
        "Windows RGBA window exceeds approved frame budget"
    );
    let target = Target::bind_expected(&a.windows)?;
    ensure!(
        target.live_geometry() == Some((a.logical_size, a.logical_position)),
        "target geometry changed"
    );
    let executable = std::env::current_exe()?;
    no_reparse(&a.worker_path)?;
    ensure!(
        executable.canonicalize()? == a.worker_path.canonicalize()?,
        "Windows worker must be this Agent executable"
    );
    ensure!(
        crate::hash(owned_file(&executable, 0o755, 256 * 1024 * 1024)?) == a.agent_sha256
            && a.worker_sha256 == a.agent_sha256
            && crate::hash(owned_file(&a.worker_path, 0o755, 256 * 1024 * 1024)?)
                == a.worker_sha256,
        "Agent/worker hash changed"
    );
    Ok(())
}
