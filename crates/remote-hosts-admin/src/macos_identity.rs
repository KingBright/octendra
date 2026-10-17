//! Fixed macOS root-helper file identity. Read-only; neither IPC authentication
//! nor an execution grant. Never signs, installs, launches or executes the helper.
use crate::{
    filesystem::{inspect_file, trusted_dir},
    privilege::{self, Capability, Evidence, Grant, Helper, Identity, Platform},
    protocol::{FileState, digest},
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

pub const HELPER_PATH: &str = "/Library/PrivilegedHelperTools/com.remotehosts.admin";
pub const SIGNING_IDENTIFIER: &str = "com.remotehosts.admin";
#[derive(Subcommand)]
pub enum Command {
    /// Verify the fixed protected helper against independently reviewed identity.
    VerifyIdentity {
        #[arg(long)]
        team_id: String,
        #[arg(long)]
        expected_sha256: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedIdentity {
    pub team_id: String,
    pub sha256: String,
}
impl ExpectedIdentity {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.team_id.len() == 10
                && self
                    .team_id
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()),
            "invalid_apple_team_identifier"
        );
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid_helper_sha256"
        );
        Ok(())
    }
    fn requirement(&self) -> Result<String> {
        self.validate()?;
        Ok(format!(
            "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"{}\" and identifier \"{}\"",
            self.team_id, SIGNING_IDENTIFIER
        ))
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct FileIdentity {
    pub signing_identifier: String,
    pub team_id: String,
}
fn parse_identity(text: &str, expected: &ExpectedIdentity) -> Result<FileIdentity> {
    let mut identifier = None;
    let mut team = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("Identifier=") {
            ensure!(
                identifier.replace(value).is_none(),
                "duplicate_signing_identifier"
            );
        }
        if let Some(value) = line.strip_prefix("TeamIdentifier=") {
            ensure!(team.replace(value).is_none(), "duplicate_team_identifier");
        }
        ensure!(
            line.trim() != "Signature=adhoc",
            "ad_hoc_helper_signature_not_allowed"
        );
    }
    ensure!(
        identifier == Some(SIGNING_IDENTIFIER) && team == Some(expected.team_id.as_str()),
        "helper_signing_identity_mismatch"
    );
    Ok(FileIdentity {
        signing_identifier: SIGNING_IDENTIFIER.into(),
        team_id: expected.team_id.clone(),
    })
}
fn resource_binding(file: &FileState, expected: &ExpectedIdentity) -> Result<()> {
    ensure!(
        file.path == HELPER_PATH
            && file.sha256 == expected.sha256
            && file.uid == 0
            && file.mode & 0o022 == 0
            && file.mode & 0o111 != 0
            && file.mode & 0o6000 == 0,
        "untrusted_helper_file_binding"
    );
    Ok(())
}
fn verify_args(expected: &ExpectedIdentity) -> Result<Vec<String>> {
    Ok(vec![
        "--verify".into(),
        "--strict".into(),
        "--all-architectures".into(),
        format!("-R={}", expected.requirement()?),
        HELPER_PATH.into(),
    ])
}
#[allow(async_fn_in_trait)]
trait Driver {
    fn snapshot(&self) -> Result<FileState>;
    async fn codesign(&self, args: &[String]) -> Result<(bool, String)>;
}
struct Native;
impl Driver for Native {
    fn snapshot(&self) -> Result<FileState> {
        trusted_dir(Path::new("/Library/PrivilegedHelperTools"), 0)?;
        inspect_file(HELPER_PATH, 0)?.context("fixed_macos_helper_not_installed")
    }
    async fn codesign(&self, args: &[String]) -> Result<(bool, String)> {
        trusted_dir(Path::new("/usr/bin"), 0)?;
        let verifier = inspect_file("/usr/bin/codesign", 0)?.context("system_codesign_missing")?;
        ensure!(
            verifier.uid == 0 && verifier.mode & 0o022 == 0 && verifier.mode & 0o6000 == 0,
            "untrusted_system_codesign"
        );
        let mut command = crate::executor::command("/usr/bin/codesign", &[]);
        command
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("codesign_query_not_started")?;
        let stderr = child.stderr.take().context("codesign_stderr_missing")?;
        let read = async {
            let mut bytes = Vec::new();
            stderr.take(16385).read_to_end(&mut bytes).await?;
            ensure!(bytes.len() <= 16384, "codesign_metadata_limit");
            Ok::<_, anyhow::Error>(bytes)
        };
        let (bytes, status) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::try_join!(read, async { Ok::<_, anyhow::Error>(child.wait().await?) })
        })
        .await
        .context("codesign_query_timeout")??;
        Ok((status.success(), String::from_utf8(bytes)?))
    }
}
#[derive(Debug, Serialize)]
pub struct Verification {
    pub protocol: u32,
    pub file_identity_verified: bool,
    pub helper_path: &'static str,
    pub file: FileState,
    pub identity: FileIdentity,
    pub binding_sha256: String,
    pub capability: Capability,
    pub peer_authenticated: bool,
    pub grant_verified: bool,
    pub grants_authority: bool,
}
async fn verify_using<D: Driver>(
    driver: &D,
    expected: &ExpectedIdentity,
    identity: Identity,
) -> Result<Verification> {
    expected.validate()?;
    let before = driver.snapshot()?;
    resource_binding(&before, expected)?;
    let args = verify_args(expected)?;
    ensure!(
        driver.codesign(&args).await?.0,
        "helper_signature_or_requirement_rejected"
    );
    let (ok, text) = driver
        .codesign(&["--display".into(), "--verbose=4".into(), HELPER_PATH.into()])
        .await?;
    ensure!(ok, "helper_signature_metadata_unavailable");
    let observed = parse_identity(&text, expected)?;
    let after = driver.snapshot()?;
    resource_binding(&after, expected)?;
    ensure!(
        before == after,
        "helper_replaced_during_signature_verification"
    );
    let capability = privilege::discover(&Evidence {
        platform: Platform::Macos,
        identity,
        no_new_privileges: None,
        helper: Helper::DetectedUnverified,
        grant: Grant::NotChecked,
    });
    ensure!(
        !capability.execution_available && !capability.execution_implemented,
        "identity_check_cannot_grant_execution"
    );
    Ok(Verification {
        protocol: 1,
        file_identity_verified: true,
        helper_path: HELPER_PATH,
        binding_sha256: digest(&(expected, &before, &args))?,
        file: before,
        identity: observed,
        capability,
        peer_authenticated: false,
        grant_verified: false,
        grants_authority: false,
    })
}
pub async fn run(command: Command) -> Result<Verification> {
    let Command::VerifyIdentity {
        team_id,
        expected_sha256,
    } = command;
    let expected = ExpectedIdentity {
        team_id,
        sha256: expected_sha256,
    };
    expected.validate()?;
    ensure!(
        cfg!(target_os = "macos"),
        "macos_native_identity_verification_required"
    );
    let identity = if nix::unistd::geteuid().is_root() {
        Identity::Root
    } else {
        Identity::OrdinaryUser
    };
    verify_using(&Native, &expected, identity).await
}
#[cfg(test)]
mod tests;
