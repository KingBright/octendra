use octendra_desktop_protocol::Identity;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::fs::MetadataExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const APP_ID: &str = "fun.hackerlife.octendra.desktop-worker";
pub const MAX_PIXELS_BYTES: usize = octendra_desktop_protocol::MAX_WORKER_PIXELS;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub enabled: bool,
    pub app_id: String,
    pub desktop_uid: u32,
    pub desktop_session: String,
    pub identity: Identity,
    pub worker_sha256: String,
    pub agent_sha256: String,
    pub expires_at_ms: u64,
    pub logical_size: [u32; 2],
    pub logical_position: [i32; 2],
    pub max_frame_bytes: usize,
    pub allow_input: bool,
}
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
fn status_number(field: &str) -> Result<u32, String> {
    let status = fs::read_to_string("/proc/self/status").map_err(|_| "process status")?;
    status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| "process identity".into())
}
pub fn hash_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|_| "executable read")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
impl Approval {
    pub fn load(path: &Path) -> Result<Self, String> {
        let actual_uid = status_number("Uid:")?;
        if actual_uid == 0 {
            return Err("root worker forbidden".into());
        }
        let meta = fs::symlink_metadata(path).map_err(|_| "approval metadata")?;
        if !meta.is_file()
            || meta.uid() != actual_uid
            || meta.mode() & 0o777 != 0o600
            || meta.len() > 16384
        {
            return Err("approval must be a bounded owned regular 0600 file".into());
        }
        let config: Self = serde_json::from_slice(&fs::read(path).map_err(|_| "approval read")?)
            .map_err(|_| "invalid approval")?;
        config.validate(actual_uid)?;
        if config.enabled {
            config.verify_desktop()?;
        }
        Ok(config)
    }
    pub fn validate(&self, actual_uid: u32) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if actual_uid == 0
            || self.desktop_uid != actual_uid
            || self.app_id != APP_ID
            || self.identity.owner.is_empty()
            || self.identity.desktop_id
                != format!("uid:{}:session:{}", actual_uid, self.desktop_session)
            || self.desktop_session.is_empty()
            || !self
                .desktop_session
                .chars()
                .all(|c| c.is_ascii_alphanumeric())
            || self.logical_size.contains(&0)
            || self.logical_size.iter().any(|n| *n > 8192)
            || self.identity.task_id.is_nil()
            || self.identity.device_id.is_nil()
            || self.max_frame_bytes == 0
            || self.max_frame_bytes > MAX_PIXELS_BYTES
            || self.expires_at_ms <= now_ms()
            || self.expires_at_ms > now_ms().saturating_add(600_000)
            || !self.allow_input
        {
            return Err("approval bounds or identity".into());
        }
        for hash in [&self.worker_sha256, &self.agent_sha256] {
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("approval executable hash".into());
            }
        }
        Ok(())
    }
    pub fn verify_desktop(&self) -> Result<(), String> {
        self.validate(status_number("Uid:")?)?;
        if hash_file(&std::env::current_exe().map_err(|_| "worker executable")?)?
            != self.worker_sha256
        {
            return Err("worker hash changed".into());
        }
        let parent = status_number("PPid:")?;
        if hash_file(Path::new(&format!("/proc/{parent}/exe")))? != self.agent_sha256 {
            return Err("worker must be a child of the approved Agent".into());
        }
        // Bounded local metadata query; never pass a session ID through a shell.
        let mut child = Command::new("loginctl")
            .args([
                "show-session",
                &self.desktop_session,
                "--property=User",
                "--property=Type",
                "--property=Active",
                "--property=LockedHint",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| "desktop session query")?;
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() < Duration::from_secs(1) => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("desktop session query timeout".into());
                }
            }
        };
        if !status.success() {
            return Err("desktop session missing".into());
        }
        let mut data = Vec::new();
        child
            .stdout
            .take()
            .ok_or("desktop session output")?
            .take(8193)
            .read_to_end(&mut data)
            .map_err(|_| "desktop session output")?;
        if data.len() > 8192 {
            return Err("desktop session output bound".into());
        }
        let properties = String::from_utf8(data).map_err(|_| "desktop session encoding")?;
        for expected in [
            format!("User={}", self.desktop_uid),
            "Type=wayland".into(),
            "Active=yes".into(),
            "LockedHint=no".into(),
        ] {
            if !properties.lines().any(|line| line == expected) {
                return Err("desktop session unsuitable".into());
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_does_not_grant() {
        let c: Approval = serde_json::from_str(include_str!("../approval.example.json")).unwrap();
        assert!(!c.enabled);
        assert!(c.validate(1000).is_ok());
    }
    #[test]
    fn enabled_requires_concrete_identity_and_hash() {
        let mut c: Approval =
            serde_json::from_str(include_str!("../approval.example.json")).unwrap();
        c.enabled = true;
        assert!(c.validate(1000).is_err());
        assert!(c.validate(0).is_err());
    }
}
