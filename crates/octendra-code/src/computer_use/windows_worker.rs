//! Bounded private-pipe child. No network, shell, enrollment or authority creation.
use super::{
    transport::WorkerApproval, windows_authority, windows_native::Native, windows_session::Worker,
};
use anyhow::{Result, ensure};
use octendra_desktop_protocol::*;
use std::{
    collections::BTreeSet,
    io::Write,
    path::Path,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
        Pipes::PeekNamedPipe,
        WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
    },
};

struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
fn input(handle: HANDLE, buffer: &mut Vec<u8>, deadline: Instant) -> Result<Option<WorkerRequest>> {
    loop {
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if buffer.len() >= 4 {
            let size = u32::from_be_bytes(buffer[..4].try_into()?) as usize;
            ensure!(size > 0 && size <= MAX_WORKER_HEADER, "request bound");
            if buffer.len() >= 4 + size {
                let request = serde_json::from_slice(&buffer[4..4 + size])?;
                buffer.drain(..4 + size);
                return Ok(Some(request));
            }
        }
        let mut available = 0;
        if unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut available), None) }.is_err() {
            return Ok(None);
        }
        if available == 0 {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let mut chunk = vec![0u8; (available as usize).min(8192)];
        // Read the inherited pipe directly. A buffered stdin reader can retain
        // bytes that PeekNamedPipe cannot see, then wait forever for more input.
        let mut n = 0;
        unsafe {
            windows::Win32::Storage::FileSystem::ReadFile(
                handle,
                Some(&mut chunk),
                Some(&mut n),
                None,
            )?;
        }
        let n = n as usize;
        if n == 0 {
            return Ok(None);
        }
        ensure!(
            buffer.len() + n <= MAX_WORKER_HEADER + 4,
            "framing buffer bound"
        );
        buffer.extend_from_slice(&chunk[..n]);
    }
}
pub fn run(path: &Path) -> Result<()> {
    let bytes = windows_authority::owned_file(path, 0o600, 16384)?;
    let a: WorkerApproval = serde_json::from_slice(&bytes)?;
    let now = crate::now_ms() as u64;
    ensure!(
        a.enabled
            && a.allow_input
            && a.capability_version.is_some_and(|v| v > 0)
            && a.app_id == "fun.hackerlife.octendra.desktop-worker"
            && a.weavelo.is_none()
            && a.desktop_uid == 0
            && !a.identity.owner.is_empty()
            && !a.identity.task_id.is_nil()
            && !a.identity.device_id.is_nil()
            && a.identity.desktop_id == a.windows.desktop_id()
            && a.desktop_session == a.windows.session_id.to_string()
            && a.windows.owner_sid == windows_authority::current_sid()?
            && a.expires_at_ms > now
            && a.expires_at_ms - now <= u64::from(MAX_LEASE_MS)
            && a.max_frame_bytes > 0
            && a.max_frame_bytes <= MAX_WORKER_PIXELS,
        "private Windows worker authority invalid"
    );
    let executable = std::env::current_exe()?;
    ensure!(
        crate::hash(windows_authority::owned_file(
            &executable,
            0o755,
            256 * 1024 * 1024
        )?) == a.worker_sha256
            && a.worker_sha256 == a.agent_sha256,
        "worker binary changed"
    );
    unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)?;
    }
    let _com = Com;
    let input_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
    let output_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE)? };
    use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    ensure!(
        unsafe { GetFileType(input_handle) } == FILE_TYPE_PIPE
            && unsafe { GetFileType(output_handle) } == FILE_TYPE_PIPE,
        "worker requires parent-owned pipes"
    );
    // Hash/COM startup must consume the original authorization budget, never
    // restart its clock after a slow validation or cold platform startup.
    let remaining =
        super::windows_session::approval_budget(a.expires_at_ms, crate::now_ms() as u64)
            .ok_or_else(|| anyhow::anyhow!("worker approval expired during startup"))?;
    let deadline = Instant::now() + remaining;
    let backend = Native::new(
        a.windows,
        a.logical_size,
        a.logical_position,
        a.max_frame_bytes,
    )?;
    let mut worker = Worker::new(
        backend,
        a.identity,
        deadline,
        a.logical_size,
        a.logical_position,
        a.max_frame_bytes,
    );
    let mut buffer = Vec::new();
    let mut seen = BTreeSet::new();
    let mut unknown = false;
    while let Some(request) = input(
        input_handle,
        &mut buffer,
        (Instant::now() + worker.budget()).min(deadline),
    )? {
        ensure!(
            seen.insert(request.request_id) && seen.len() <= 128,
            "duplicate/bounded worker request"
        );
        let closing = matches!(request.request, Request::Close { .. });
        let (response, pixels) = worker.handle(request.request);
        unknown = response == Err(ErrorCode::OutcomeUnknown);
        let reply = WorkerReply {
            request_id: request.request_id,
            result: response,
            pixels: pixels.as_ref().map(|p| p.metadata.clone()),
        };
        let header = serde_json::to_vec(&reply)?;
        ensure!(header.len() <= MAX_WORKER_HEADER, "worker reply bound");
        let mut output = std::io::stdout().lock();
        output.write_all(&(header.len() as u32).to_be_bytes())?;
        output.write_all(&header)?;
        if let Some(p) = pixels {
            output.write_all(&p.bytes)?;
        }
        output.flush()?;
        if closing || unknown {
            break;
        }
    }
    ensure!(
        worker.shutdown() && !unknown,
        "Windows worker outcome unknown"
    );
    Ok(())
}
