//! Embedded native `Gemini.app` macOS Accessibility bridge and durable operation state machine.
//!
//! Replaces the legacy standalone `gemini-desktop-mcp` and `gemini-desktop-client` binaries while
//! preserving the on-disk `~/Library/Application Support/GeminiDesktopMCP` state contract and
//! `bridge.swift` runner. Zero `unsafe` code is used anywhere in this module.

#[cfg(target_os = "macos")]
pub mod artifact;
#[cfg(target_os = "macos")]
pub mod ops;

#[cfg(target_os = "macos")]
pub use ops::dispatch;

#[cfg(not(target_os = "macos"))]
use serde_json::Value;

/// Non-macOS stub so cross-platform Gateway (`linux-amd64`) and Windows builds link cleanly.
#[cfg(not(target_os = "macos"))]
pub fn dispatch(_action: &str, _payload: &Value) -> anyhow::Result<Value> {
    anyhow::bail!("Gemini.app 桌面原生驱动仅支持在本机 macOS 上运行")
}
