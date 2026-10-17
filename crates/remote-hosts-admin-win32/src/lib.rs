//! Narrow Win32 caller-token collector. No service, pipe creation or execution.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#[cfg(any(windows, test))]
mod flow;
#[cfg(windows)]
#[allow(unsafe_code)]
mod native;
#[cfg(any(windows, test))]
mod token;
#[cfg(windows)]
pub use native::collect_after_read;

/// Kernel-derived caller data. No public constructor, Deserialize or claimed SID input.
/// This identifies a last-read client token, not an authenticated command or grant.
#[derive(Debug)]
pub struct CallerIdentity {
    user_sid: String,
    logon_sid: String,
    logon_attributes: u32,
}
impl CallerIdentity {
    pub fn user_sid(&self) -> &str {
        &self.user_sid
    }
    pub fn logon_sid(&self) -> &str {
        &self.logon_sid
    }
    pub fn logon_attributes(&self) -> u32 {
        self.logon_attributes
    }
}
