//! Verify only native-owned exported artifacts; callers never choose a file path.

#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used
)]

use super::ops::{Operation, root_for};
use anyhow::{Context, Result, bail};
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::Mode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

const MAX_ARTIFACT: u64 = 512 * 1024 * 1024;
const MAX_TOTAL: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub artifact_id: String,
    pub kind: String,
    pub mime_type: String,
    pub path: String,
    pub byte_length: u64,
    pub sha256: String,
    pub request_id: String,
    pub session_id: String,
}

fn current_euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

fn directory_check(f: &File) -> Result<()> {
    let m = f.metadata()?;
    if !m.is_dir() || m.uid() != current_euid() || m.mode() & 0o077 != 0 {
        bail!("invalid artifact directory")
    }
    Ok(())
}

fn open_at_safe(dir: &File, name: &OsStr, is_dir: bool) -> Result<File> {
    let mut flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
    if is_dir {
        flags |= OFlag::O_DIRECTORY;
    }
    let fd = openat(dir, Path::new(name), flags, Mode::empty())?;
    let f = File::from(fd);
    if is_dir {
        directory_check(&f)?;
    }
    Ok(f)
}

fn valid_signature(kind: &str, mime: &str, head: &[u8], tail: &[u8], length: u64) -> bool {
    match (kind, mime) {
        ("image", "image/png") => {
            length >= 45
                && head.starts_with(b"\x89PNG\r\n\x1a\n")
                && head.get(12..16) == Some(b"IHDR")
                && head
                    .get(16..24)
                    .is_some_and(|d| d[..4] != [0; 4] && d[4..] != [0; 4])
                && tail.ends_with(b"\0\0\0\0IEND\xaeB`\x82")
        }
        ("image", "image/jpeg") => {
            length >= 4 && head.starts_with(b"\xff\xd8\xff") && tail.ends_with(b"\xff\xd9")
        }
        ("image", "image/webp") => {
            head.len() >= 12
                && head.starts_with(b"RIFF")
                && &head[8..12] == b"WEBP"
                && head
                    .get(4..8)
                    .and_then(|s| <[u8; 4]>::try_from(s).ok())
                    .is_some_and(|b| u64::from(u32::from_le_bytes(b)) + 8 == length)
        }
        ("video", "video/mp4") => head.len() >= 12 && &head[4..8] == b"ftyp",
        ("video", "video/webm") => head.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
        ("document", "application/pdf") => {
            head.starts_with(b"%PDF-") && tail.windows(5).any(|w| w == b"%%EOF")
        }
        _ => false,
    }
}

pub fn verify_all_with_root(
    custom_root: Option<&Path>,
    operation: &Operation,
    artifacts: &[Artifact],
) -> Result<()> {
    if artifacts.len() > 16
        || artifacts
            .iter()
            .try_fold(0u64, |sum, a| sum.checked_add(a.byte_length))
            .is_none_or(|n| n > MAX_TOTAL)
    {
        bail!("artifact count or total size exceeds limit")
    }
    if artifacts.is_empty() {
        return Ok(());
    }
    let state = root_for(custom_root)?;
    let base = state
        .join(&operation.native_dir_id)
        .join("native")
        .join("artifacts");
    let root_fd_owned = nix::fcntl::open(
        &state,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?;
    let rootfd = File::from(root_fd_owned);
    directory_check(&rootfd)?;
    let native_id = uuid::Uuid::parse_str(&operation.native_dir_id)?.to_string();
    if native_id != operation.native_dir_id {
        bail!("invalid native directory id")
    }
    let mut ids = std::collections::HashSet::new();
    let mut paths = std::collections::HashSet::new();
    for item in artifacts {
        if !ids.insert(&item.artifact_id) || !paths.insert(&item.path) {
            bail!("duplicate artifact")
        }
        if item.request_id != operation.request_id
            || item.session_id != operation.session_id
            || item.artifact_id.is_empty()
            || item.artifact_id.len() > 128
            || !item
                .artifact_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || item.byte_length == 0
            || item.byte_length > MAX_ARTIFACT
            || item.sha256.len() != 64
            || !item
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("invalid artifact metadata")
        }
        let rel = Path::new(&item.path)
            .strip_prefix(&base)
            .context("artifact outside owned root")?;
        let components: Vec<_> = rel.components().collect();
        if components.is_empty()
            || components.len() > 8
            || components
                .iter()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            bail!("invalid artifact path")
        }
        let mut folder = open_at_safe(&rootfd, OsStr::new(&native_id), true)?;
        folder = open_at_safe(&folder, OsStr::new("native"), true)?;
        folder = open_at_safe(&folder, OsStr::new("artifacts"), true)?;
        for comp in &components[..components.len() - 1] {
            if let Component::Normal(name) = comp {
                folder = open_at_safe(&folder, name, true)?;
            }
        }
        let Some(Component::Normal(name)) = components.last() else {
            bail!("invalid artifact filename")
        };
        let mut file = open_at_safe(&folder, name, false)?;
        let before = file.metadata()?;
        if !before.is_file()
            || before.uid() != current_euid()
            || before.mode() & 0o077 != 0
            || before.nlink() != 1
            || before.len() != item.byte_length
        {
            bail!("invalid artifact file")
        }
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut head = Vec::new();
        let mut tail = Vec::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            bytes = bytes
                .checked_add(n as u64)
                .context("artifact size overflow")?;
            if bytes > item.byte_length {
                bail!("artifact grew")
            }
            hasher.update(&buf[..n]);
            if head.len() < 64 {
                let take = n.min(64 - head.len());
                head.extend_from_slice(&buf[..take]);
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 1024 {
                tail.drain(..tail.len() - 1024);
            }
        }
        let after = file.metadata()?;
        if bytes != item.byte_length
            || after.len() != before.len()
            || after.dev() != before.dev()
            || after.ino() != before.ino()
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec()
            || format!("{:x}", hasher.finalize()) != item.sha256
            || !valid_signature(&item.kind, &item.mime_type, &head, &tail, bytes)
        {
            bail!("artifact verification failed")
        }
    }
    Ok(())
}
