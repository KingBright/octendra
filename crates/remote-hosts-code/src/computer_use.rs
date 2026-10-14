//! Shared desktop wire contract; production routing remains unavailable.
pub use remote_hosts_desktop_protocol::*;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod transport;
#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
