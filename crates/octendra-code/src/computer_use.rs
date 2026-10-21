//! Shared desktop wire contract; production routing remains unavailable.
pub use octendra_desktop_protocol::*;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) mod lifecycle;
#[cfg(target_os = "macos")]
pub mod macos_worker;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) mod transport;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub mod weavelo_worker;
#[cfg(windows)]
pub mod windows_target;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
