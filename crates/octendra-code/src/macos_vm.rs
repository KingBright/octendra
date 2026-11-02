//! One persistent macOS guest through Apple's system Virtualization framework.
//! This maintenance entry owns initialization, not the external Agent's loop.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::CString,
    fs::OpenOptions,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Operation {
    Probe,
    Status,
    Prepare,
    Run,
}
impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Status => "status",
            Self::Prepare => "prepare",
            Self::Run => "run",
        }
    }
}
unsafe extern "C" {
    fn octendra_macos_vm_main(
        operation: *const std::ffi::c_char,
        directory: *const std::ffi::c_char,
    ) -> std::ffi::c_int;
}

pub fn execute(operation: Operation, directory: Option<&Path>) -> Result<()> {
    let directory = directory.map(PathBuf::from).map(Ok).unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".local/share/octendra/ai-workspace"))
            .context("home directory unavailable")
    })?;
    ensure!(
        directory.is_absolute(),
        "VM workspace needs an absolute directory"
    );
    let _lock = if matches!(operation, Operation::Prepare | Operation::Run) {
        if directory.exists() {
            let metadata = std::fs::symlink_metadata(&directory)?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "VM workspace must be a real directory"
            );
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "VM workspace directory must be private"
            );
        } else {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&directory)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(directory.join(".vm.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)
            .context("VM workspace already has an active owner; observe that process")?;
        Some(file)
    } else {
        None
    };
    let command = CString::new(operation.name())?;
    let path = CString::new(directory.to_str().context("VM directory must be UTF-8")?)?;
    // The CLI invokes this on its main OS thread; AppKit and VZ callbacks stay
    // on the main queue. Swift copies both strings before starting async work.
    let result = unsafe { octendra_macos_vm_main(command.as_ptr(), path.as_ptr()) };
    ensure!(
        result == 0,
        "VM operation failed; inspect its structured result"
    );
    Ok(())
}
