//! Local administrator operation. Never opens OAuth/device state or starts a service.
mod terminal;

use anyhow::{Context, Result, bail, ensure};
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use nix::fcntl::OFlag;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{Read, Write},
    ops::Range,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, fchown},
    path::{Path, PathBuf},
};
use subtle::ConstantTimeEq;
use tokio::signal::unix::{SignalKind, signal};
use zeroize::Zeroizing;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// No password argument, environment lookup, password file, or noninteractive override.
pub async fn run(path: &Path) -> Result<()> {
    let mut interrupt = signal(SignalKind::interrupt()).context("observe interrupt signal")?;
    let mut terminate = signal(SignalKind::terminate()).context("observe termination signal")?;
    let mut hangup = signal(SignalKind::hangup()).context("observe terminal hangup")?;
    let mut quit = signal(SignalKind::quit()).context("observe quit signal")?;
    let mut suspend =
        signal(SignalKind::from_raw(nix::libc::SIGTSTP)).context("observe suspend signal")?;
    let mut terminal = terminal::Terminal::open()?;
    let pending = PendingReset::open(path)?;
    let replacement = tokio::select! {
        result = prepare(&pending, &mut terminal) => result?,
        _ = interrupt.recv() => bail!("password reset cancelled; configuration unchanged"),
        _ = terminate.recv() => bail!("password reset cancelled; configuration unchanged"),
        _ = hangup.recv() => bail!("password reset cancelled; configuration unchanged"),
        _ = quit.recv() => bail!("password reset cancelled; configuration unchanged"),
        _ = suspend.recv() => bail!("password reset cancelled; configuration unchanged"),
    };
    // Restore terminal settings before the synchronous commit. Cancellation cannot
    // discard a successful commit's result while waiting for more terminal input.
    drop(terminal);
    let Some(hash) = replacement else {
        eprintln!("Cancelled; configuration unchanged.");
        return Ok(());
    };
    pending.commit(&hash)?;
    eprintln!(
        "Owner password hash replaced. OAuth grants, device credentials and web sessions are unchanged.\n\
         Restart the gateway through its existing service manager to activate the new password.\n\
         Any original setup password file is unchanged and no longer represents the new password."
    );
    Ok(())
}

async fn prepare(
    pending: &PendingReset,
    terminal: &mut terminal::Terminal,
) -> Result<Option<Zeroizing<String>>> {
    terminal
        .write(
            format!(
                "Gateway configuration: {:?}\n\
                 Only its owner password hash will change. Existing authorizations remain intact.\n",
                pending.path
            )
            .as_bytes(),
        )
        .await?;
    let first = terminal.read_line("New owner password: ").await?;
    ensure!(!first.is_empty(), "password must not be empty");
    let second = terminal.read_line("Repeat new owner password: ").await?;
    ensure!(
        bool::from(first.as_slice().ct_eq(second.as_slice())),
        "passwords do not match; configuration unchanged"
    );
    let salt = SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes())?;
    let hash = Zeroizing::new(
        Argon2::default()
            .hash_password(&first, &salt)
            .context("cannot hash new password")?
            .to_string(),
    );
    drop(first);
    drop(second);
    let confirmation = terminal
        .read_line("Type RESET to commit (input hidden), or press Enter to cancel: ")
        .await?;
    Ok((confirmation.as_slice() == b"RESET").then_some(hash))
}

#[derive(Deserialize)]
struct ExistingGateway<'a> {
    #[serde(borrow)]
    password_hash: &'a RawValue,
    // Require an existing gateway document, while preserving unknown fields byte
    // for byte. Do not deserialize/re-serialize the device credentials.
    #[serde(rename = "owner")]
    _owner: String,
    #[serde(rename = "public_url")]
    _public_url: String,
    #[serde(rename = "bind")]
    _bind: String,
    #[serde(rename = "state_dir")]
    _state_dir: PathBuf,
    #[serde(rename = "devices")]
    _devices: serde::de::IgnoredAny,
    #[serde(rename = "redirect_uris")]
    _redirect_uris: serde::de::IgnoredAny,
}

struct PendingReset {
    path: PathBuf,
    parent: File,
    _locked_original: File,
    metadata: Metadata,
    original: Zeroizing<Vec<u8>>,
    hash_span: Range<usize>,
}

impl PendingReset {
    fn open(path: &Path) -> Result<Self> {
        let name = path.file_name().context("config requires a file name")?;
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .context("config parent must already exist")?;
        let parent = File::open(&directory)?;
        let parent_metadata = parent.metadata()?;
        ensure!(
            parent_metadata.is_dir() && parent_metadata.mode() & 0o022 == 0,
            "config parent must not be writable by group or other users"
        );
        let path = directory.join(name);
        let file = open_existing(&path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "config must be a regular file without hard links"
        );
        ensure!(
            metadata.mode() & 0o7077 == 0,
            "config must have private owner-only permissions and no special mode bits"
        );
        let user = nix::unistd::geteuid();
        ensure!(
            user.is_root() || metadata.uid() == user.as_raw(),
            "run as the config owner or administrator"
        );
        fs2::FileExt::try_lock_exclusive(&file)
            .context("another administrator is updating this config")?;
        let original = read_bounded(&file)?;
        let fields: ExistingGateway<'_> = serde_json::from_slice(&original)
            .map_err(|_| anyhow::anyhow!("invalid gateway JSON"))?;
        let old_hash: Zeroizing<String> = Zeroizing::new(
            serde_json::from_str(fields.password_hash.get())
                .map_err(|_| anyhow::anyhow!("gateway password_hash must be a string"))?,
        );
        ensure!(
            !old_hash.is_empty(),
            "gateway password_hash must not be empty"
        );
        let start = (fields.password_hash.get().as_ptr() as usize)
            .checked_sub(original.as_ptr() as usize)
            .context("password field is not in the original document")?;
        let end = start
            .checked_add(fields.password_hash.get().len())
            .filter(|end| *end <= original.len())
            .context("invalid password field location")?;
        let pending = Self {
            path,
            parent,
            _locked_original: file,
            metadata,
            original,
            hash_span: start..end,
        };
        pending.ensure_unchanged()?;
        Ok(pending)
    }

    fn ensure_unchanged(&self) -> Result<()> {
        let current_parent =
            std::fs::metadata(self.path.parent().context("config parent missing")?)?;
        let parent = self.parent.metadata()?;
        ensure!(
            current_parent.dev() == parent.dev()
                && current_parent.ino() == parent.ino()
                && current_parent.mode() & 0o022 == 0,
            "config parent changed; configuration not replaced"
        );
        let file = open_existing(&self.path)?;
        let before = file.metadata()?;
        ensure!(
            same_file(&self.metadata, &before),
            "config changed during reset; configuration not replaced"
        );
        let bytes = read_bounded(&file)?;
        ensure!(
            bytes.as_slice() == self.original.as_slice() && same_file(&before, &file.metadata()?),
            "config changed during reset; configuration not replaced"
        );
        Ok(())
    }

    fn commit(self, hash: &str) -> Result<()> {
        self.ensure_unchanged()?;
        let directory = self.path.parent().context("config parent")?;
        let mut staged = tempfile::NamedTempFile::new_in(directory)?;
        let metadata = staged.as_file().metadata()?;
        if metadata.uid() != self.metadata.uid() || metadata.gid() != self.metadata.gid() {
            fchown(
                staged.as_file(),
                Some(self.metadata.uid()),
                Some(self.metadata.gid()),
            )
            .context("cannot preserve config ownership; configuration not replaced")?;
        }
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(
                self.metadata.mode() & 0o777,
            ))?;
        staged.write_all(&self.original[..self.hash_span.start])?;
        staged.write_all(&serde_json::to_vec(hash)?)?;
        staged.write_all(&self.original[self.hash_span.end..])?;
        staged.as_file().sync_all()?;
        // Detect enrollment/manual edits made while the administrator was typing.
        // flock serializes reset commands; other configuration writers must still
        // be coordinated by the administrator during this local maintenance step.
        self.ensure_unchanged()?;
        staged
            .persist(&self.path)
            .map_err(|error| anyhow::anyhow!("config replacement failed: {}", error.error))?;
        self.parent.sync_all().context(
            "password hash replaced, but directory sync failed; verify the config before retrying",
        )?;
        Ok(())
    }
}

fn open_existing(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC).bits())
        .open(path)
        .context("cannot open existing regular config without following a symlink")
}

fn read_bounded(file: &File) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_CONFIG_BYTES,
        "gateway config exceeds the supported 1 MiB bound"
    );
    Ok(bytes)
}

fn same_file(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.mode() == b.mode()
        && a.nlink() == b.nlink()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

#[cfg(test)]
mod tests;
