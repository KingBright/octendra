//! Interpreter-free one-shot installation core.
use crate::evidence;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    pub schema: u32,
    pub version: String,
    pub candidate: PathBuf,
    pub candidate_sha256: String,
    pub target: PathBuf,
    pub backup_dir: PathBuf,
    pub result: PathBuf,
    #[serde(default)]
    pub restart: Option<RestartSpec>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallReceipt {
    pub schema: u32,
    pub state: String,
    pub version: String,
    pub candidate_sha256: String,
    pub previous_sha256: Option<String>,
    pub installed_sha256: Option<String>,
    pub backup: Option<PathBuf>,
    pub service_changed: bool,
    pub rollback: Option<String>,
    pub error_code: Option<String>,
}
fn valid_version(v: &str) -> bool {
    let p: Vec<_> = v.split('.').collect();
    p.len() == 3
        && p.iter()
            .all(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))
}
fn valid_hash(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn copy_atomic(source: &Path, target: &Path) -> Result<()> {
    let parent = target.parent().context("install_target_parent_missing")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    let mut input = fs::File::open(source)?;
    std::io::copy(&mut input, &mut temp)?;
    temp.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    temp.persist(target).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn save(path: &Path, value: &InstallReceipt) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    evidence::atomic(path, value)
}
pub fn install(r: &InstallRequest) -> Result<InstallReceipt> {
    ensure!(
        r.schema == 1 && valid_version(&r.version) && valid_hash(&r.candidate_sha256),
        "invalid_install_request"
    );
    ensure!(
        r.candidate.is_absolute()
            && r.target.is_absolute()
            && r.backup_dir.is_absolute()
            && r.result.is_absolute(),
        "absolute_install_paths_required"
    );
    ensure!(r.candidate != r.target, "candidate_target_alias");
    ensure!(
        evidence::hash_file(&r.candidate)?.sha256 == r.candidate_sha256,
        "candidate_checksum_mismatch"
    );
    let v = Command::new(&r.candidate)
        .arg("--version")
        .output()
        .context("candidate_version_probe_failed")?;
    ensure!(
        v.status.success()
            && String::from_utf8_lossy(&v.stdout).trim()
                == format!("remote-hosts-code {}", r.version),
        "candidate_version_mismatch"
    );
    if r.result.exists() {
        let old: InstallReceipt = evidence::load(&r.result)?;
        ensure!(
            old.version == r.version && old.candidate_sha256 == r.candidate_sha256,
            "install_receipt_identity_conflict"
        );
        return Ok(old);
    }
    fs::create_dir_all(&r.backup_dir)?;
    let previous = r
        .target
        .is_file()
        .then(|| evidence::hash_file(&r.target))
        .transpose()?;
    let backup = previous
        .as_ref()
        .map(|_| r.backup_dir.join("remote-hosts-code.previous"));
    if let Some(p) = &backup {
        ensure!(!p.exists(), "backup_already_exists");
        copy_atomic(&r.target, p)?;
    }
    let mut out = InstallReceipt {
        schema: 1,
        state: "installing".into(),
        version: r.version.clone(),
        candidate_sha256: r.candidate_sha256.clone(),
        previous_sha256: previous.as_ref().map(|x| x.sha256.clone()),
        installed_sha256: None,
        backup: backup.clone(),
        service_changed: false,
        rollback: None,
        error_code: None,
    };
    let attempt = (|| -> Result<()> {
        copy_atomic(&r.candidate, &r.target)?;
        out.service_changed = true;
        let installed = evidence::hash_file(&r.target)?;
        ensure!(
            installed.sha256 == r.candidate_sha256,
            "installed_checksum_mismatch"
        );
        out.installed_sha256 = Some(installed.sha256);
        if let Some(restart) = &r.restart {
            ensure!(
                restart.program.is_absolute(),
                "absolute_restart_program_required"
            );
            let status = Command::new(&restart.program)
                .args(&restart.args)
                .status()
                .context("service_restart_failed")?;
            ensure!(status.success(), "service_restart_failed");
            out.state = "restarted_needs_readiness".into();
        } else {
            out.state = "installed_needs_restart".into();
        }
        Ok(())
    })();
    if let Err(e) = attempt {
        out.state = "failed".into();
        out.error_code = Some(
            e.to_string()
                .split(':')
                .next()
                .unwrap_or("install_failed")
                .into(),
        );
        if out.service_changed
            && let Some(p) = &backup
        {
            match copy_atomic(p, &r.target) {
                Ok(()) => out.rollback = Some("restored_previous_binary".into()),
                Err(_) => out.rollback = Some("rollback_failed".into()),
            }
        }
        save(&r.result, &out)?;
        anyhow::bail!(
            "native_install_failed: {}",
            out.error_code.as_deref().unwrap_or("unknown")
        );
    }
    save(&r.result, &out)?;
    Ok(out)
}
