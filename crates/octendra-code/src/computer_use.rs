//! Shared desktop wire contract and explicitly authorized platform workers.
pub use octendra_desktop_protocol::*;
#[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
pub(crate) mod lifecycle;
#[cfg(target_os = "macos")]
pub mod macos_worker;
#[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
pub(crate) mod transport;
pub(crate) mod weavelo_route;
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub mod weavelo_worker;
#[cfg(windows)]
pub(crate) mod windows_authority;
#[cfg(windows)]
mod windows_native;
#[cfg(any(windows, test))]
mod windows_session;
#[cfg(windows)]
pub mod windows_target;
#[cfg(windows)]
pub mod windows_worker;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
