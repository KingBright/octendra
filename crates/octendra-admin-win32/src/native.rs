//! The only unsafe boundary. Retains OS-owned buffers and handles until decoded.
use crate::{
    CallerIdentity,
    flow::{self, Api},
    token::{self, MAX_TOKEN, Span},
};
use anyhow::{Context, Result, ensure};
use std::{
    mem::size_of,
    os::windows::io::{AsRawHandle, BorrowedHandle},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_TOKEN, GetLastError, HANDLE},
    Security::{
        GetTokenInformation, RevertToSelf, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TokenGroups,
        TokenHasRestrictions, TokenImpersonationLevel, TokenIsAppContainer, TokenType, TokenUser,
    },
    System::{
        Pipes::{GetNamedPipeInfo, ImpersonateNamedPipeClient, PIPE_SERVER_END},
        Threading::{GetCurrentThread, OpenThreadToken},
    },
};
struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
struct Buffer {
    words: Vec<usize>,
    used: usize,
}
impl Buffer {
    fn query(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Self> {
        let mut size = 0;
        // SAFETY: valid live TOKEN_QUERY handle; size is an initialized DWORD.
        let result = unsafe { GetTokenInformation(token, class, ptr::null_mut(), 0, &mut size) };
        let error = unsafe { GetLastError() };
        ensure!(
            result == 0
                && error == ERROR_INSUFFICIENT_BUFFER
                && (4..=MAX_TOKEN).contains(&(size as usize)),
            "token_query_size_failed"
        );
        let capacity = size as usize;
        let mut words = vec![0usize; capacity.div_ceil(size_of::<usize>())];
        // SAFETY: aligned initialized allocation covers the exact advertised capacity.
        ensure!(
            unsafe {
                GetTokenInformation(
                    token,
                    class,
                    words.as_mut_ptr().cast(),
                    capacity as u32,
                    &mut size,
                )
            } != 0,
            "token_query_failed"
        );
        ensure!(
            size >= 4 && size as usize <= capacity,
            "token_query_size_changed"
        );
        Ok(Self {
            words,
            used: size as usize,
        })
    }
    fn span(&self) -> Span<'_> {
        // SAFETY: words stays owned and immovable while this slice is borrowed;
        // used was checked against allocation, and every byte was initialized.
        let bytes =
            unsafe { std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.used) };
        Span {
            bytes,
            base: self.words.as_ptr() as usize,
            pointer: size_of::<usize>(),
        }
    }
    fn scalar(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<u32> {
        let b = Self::query(token, class)?;
        ensure!(b.used == 4, "invalid_token_scalar_size");
        Ok(u32::from_le_bytes(b.span().bytes.try_into()?))
    }
}
fn open_thread() -> Result<Handle> {
    let mut handle = ptr::null_mut();
    // SAFETY: only the current thread, TOKEN_QUERY, no duplication/adjustment.
    // OpenAsSelf is needed for reading an impersonation token; never a primary fallback.
    ensure!(
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut handle) } != 0,
        "client_thread_token_unavailable"
    );
    ensure!(!handle.is_null(), "invalid_token_handle");
    Ok(Handle(handle))
}
struct Native<'a> {
    pipe: BorrowedHandle<'a>,
}
impl Api for Native<'_> {
    fn preflight(&mut self) -> Result<()> {
        let mut flags = 0;
        // SAFETY: borrowed OS handle stays alive; all optional outputs are null.
        ensure!(
            unsafe {
                GetNamedPipeInfo(
                    self.pipe.as_raw_handle(),
                    &mut flags,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            } != 0
                && flags & PIPE_SERVER_END != 0,
            "server_pipe_handle_required"
        );
        let mut handle = ptr::null_mut();
        // Existing thread impersonation is never overwritten or reverted.
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut handle) } != 0 {
            let _existing = Handle(handle);
            anyhow::bail!("preexisting_thread_token_denied");
        }
        ensure!(
            unsafe { GetLastError() } == ERROR_NO_TOKEN,
            "thread_identity_state_unavailable"
        );
        Ok(())
    }
    fn impersonate(&mut self) -> Result<()> {
        // SAFETY: validated live server pipe handle. No async yield or callback.
        ensure!(
            unsafe { ImpersonateNamedPipeClient(self.pipe.as_raw_handle()) } != 0,
            "pipe_impersonation_failed"
        );
        Ok(())
    }
    fn capture(&mut self) -> Result<CallerIdentity> {
        let token = open_thread()?;
        let kind = Buffer::scalar(token.0, TokenType)?;
        let level = Buffer::scalar(token.0, TokenImpersonationLevel)?;
        ensure!(
            kind == 2 && level == 2,
            "local_impersonation_token_required"
        );
        let app = Buffer::scalar(token.0, TokenIsAppContainer)?;
        let restricted = Buffer::scalar(token.0, TokenHasRestrictions)?;
        let user = Buffer::query(token.0, TokenUser)?;
        let groups = Buffer::query(token.0, TokenGroups)?;
        token::decode(user.span(), groups.span(), kind, level, app, restricted)
    }
    fn revert(&mut self) -> bool {
        // SAFETY: same synchronous thread; guard executes on every return/unwind.
        unsafe { RevertToSelf() != 0 }
    }
    fn fatal_revert_failure(&mut self) -> ! {
        std::process::abort()
    }
}
/// Collects the kernel token for the last message read on an existing server pipe.
/// The trusted caller must retain/exclusively use that connection across frame
/// read and collection. No request SID, username, PID or token handle is accepted.
/// This function does not read/bind a request, inspect DACL/SCM, or grant access.
/// Never call it from an installed service until real Windows acceptance completes.
pub fn collect_after_read(pipe: BorrowedHandle<'_>) -> Result<CallerIdentity> {
    flow::collect(&mut Native { pipe }).context("windows_client_identity_collection_failed")
}
