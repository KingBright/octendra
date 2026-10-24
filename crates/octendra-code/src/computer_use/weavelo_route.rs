//! Transport-independent identity for the existing Weavelo bridge.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    MainDisplay,
    Display(u32),
    Window(u32),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub peer_device_id: String,
    pub source: Source,
    pub connection_id: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Explicit owner-approved remote key space; never inferred from the executor OS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_platform: Option<InputPlatform>,
    pub profile: PathBuf,
    pub generation: String,
    pub gui_executable_sha256: String,
    pub target: Target,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputPlatform {
    Macos,
}
