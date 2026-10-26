//! Offline, read-only comparison. Never opens gateway state or starts a service.
use argon2::{Argon2, PasswordHash, PasswordVerifier, password_hash::Error};
use nix::fcntl::{OFlag, open, openat};
use nix::sys::stat::Mode;
use serde::Deserialize;
use std::{
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};
use zeroize::Zeroizing;

const MAX_CONFIG: u64 = 1024 * 1024;
const MAX_PASSWORD: u64 = 4096;

pub fn check(config: &Path, password: &Path) -> (&'static str, u8) {
    match compare(config, password) {
        Ok(true) => ("match", 0),
        Ok(false) => ("mismatch", 1),
        Err(()) => ("error", 2),
    }
}

#[derive(Deserialize)]
struct Credential<'a> {
    #[serde(borrow)]
    password_hash: &'a str,
}

fn compare(config: &Path, password: &Path) -> Result<bool, ()> {
    let config = read_private(config, MAX_CONFIG)?;
    let password = read_private(password, MAX_PASSWORD)?;
    let credential: Credential<'_> = serde_json::from_slice(&config).map_err(|_| ())?;
    let hash = PasswordHash::new(credential.password_hash).map_err(|_| ())?;
    // Bound PHC-controlled work before Argon2 allocates memory. These bounds cover
    // existing default-generated hashes; unsupported parameters are errors.
    for (name, ceiling) in [("m", 65536), ("t", 10), ("p", 16)] {
        let value = hash.params.get_decimal(name).ok_or(())?;
        if value == 0 || value > ceiling {
            return Err(());
        }
    }
    // Same verification operation as auth::Auth::owner_password_valid; no HTTP,
    // rate counters, sessions, grants or credential writes. Password bytes are exact.
    match Argon2::default().verify_password(&password, &hash) {
        Ok(()) => Ok(true),
        Err(Error::Password) => Ok(false),
        Err(_) => Err(()),
    }
}

fn read_private(path: &Path, limit: u64) -> Result<Zeroizing<Vec<u8>>, ()> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(|_| ())?.join(path)
    };
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => parts.push(name),
            _ => return Err(()),
        }
    }
    let (name, parents) = parts.split_last().ok_or(())?;
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK;
    let mut directory = open("/", flags | OFlag::O_DIRECTORY, Mode::empty()).map_err(|_| ())?;
    // Open each component relative to a pinned directory descriptor, including
    // parents: a symlink swap cannot redirect the read to another file.
    for parent in parents {
        directory = openat(
            &directory,
            Path::new(parent),
            flags | OFlag::O_DIRECTORY,
            Mode::empty(),
        )
        .map_err(|_| ())?;
    }
    let file =
        File::from(openat(&directory, Path::new(name), flags, Mode::empty()).map_err(|_| ())?);
    let before = file.metadata().map_err(|_| ())?;
    let uid = nix::unistd::geteuid().as_raw();
    if !before.is_file()
        || before.nlink() != 1
        || !matches!(before.mode() & 0o7777, 0o400 | 0o600)
        || (uid != 0 && before.uid() != uid)
        || before.len() == 0
        || before.len() > limit
    {
        return Err(());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    (&file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    let after = file.metadata().map_err(|_| ())?;
    if bytes.len() as u64 != before.len()
        || after.len() != before.len()
        || after.mode() != before.mode()
        || after.uid() != before.uid()
        || after.gid() != before.gid()
        || after.nlink() != before.nlink()
        || after.mtime() != before.mtime()
        || after.mtime_nsec() != before.mtime_nsec()
        || after.ctime() != before.ctime()
        || after.ctime_nsec() != before.ctime_nsec()
    {
        return Err(());
    }
    Ok(bytes)
}
