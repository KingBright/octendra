//! Cross-platform privilege discovery and plans. No installer, executor or new journal.
use crate::protocol::{PLAN_TTL, Record, State, digest, valid_id};
use anyhow::{Result, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub const CONTRACT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
    Android,
    Unsupported,
}
impl Platform {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "linux" => Self::Linux,
            "macos" => Self::Macos,
            "windows" => Self::Windows,
            "android" => Self::Android,
            _ => Self::Unsupported,
        }
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    OrdinaryUser,
    AdbShell,
    Root,
    Unverified,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Helper {
    NotInstalled,
    DetectedUnverified,
    /// Set only after a root-peer-authenticated helper handshake, never path discovery.
    Verified,
    Unavailable,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    NotChecked,
    NotGranted,
    Revoked,
    Verified,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    BackendNotImplemented,
    PlatformUnsupported,
    IdentityUnverified,
    NoNewPrivilegesBlocksSudo,
    NoNewPrivilegesUnknown,
    HelperNotInstalled,
    HelperIdentityUnverified,
    GrantNotVerified,
    GrantNotGranted,
    GrantRevoked,
    AndroidAppSandbox,
    AndroidShellNotRoot,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    ManagedServiceRestart,
    VerifiedAgentUpgrade,
    ExactSourceDnsRepair,
}

/// Observations are inputs to discovery, never an authorization token.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub platform: Platform,
    pub identity: Identity,
    pub no_new_privileges: Option<bool>,
    pub helper: Helper,
    pub grant: Grant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub protocol: u32,
    pub platform: Platform,
    pub identity: Identity,
    pub no_new_privileges: Option<bool>,
    pub helper: Helper,
    pub grant: Grant,
    pub backend: String,
    pub execution_implemented: bool,
    pub execution_available: bool,
    pub arbitrary_root_commands: bool,
    pub reasons: Vec<Reason>,
}

/// Platform backends only observe local facts in this first increment.
/// Future executors must authenticate the peer and root-owned policy themselves.
pub trait PrivilegeBackend {
    fn observe(&self) -> Evidence;
    fn discover(&self) -> Capability {
        discover(&self.observe())
    }
}
pub struct NativeBackend;
impl PrivilegeBackend for NativeBackend {
    fn observe(&self) -> Evidence {
        let platform = Platform::current();
        #[cfg(unix)]
        let identity = match nix::unistd::geteuid().as_raw() {
            0 => Identity::Root,
            2000 if platform == Platform::Android => Identity::AdbShell,
            _ => Identity::OrdinaryUser,
        };
        #[cfg(not(unix))]
        let identity = Identity::Unverified;
        let no_new_privileges = if matches!(platform, Platform::Linux | Platform::Android) {
            std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|text| parse_no_new_privileges(&text))
        } else {
            None
        };
        let socket = match platform {
            Platform::Linux => Some("/run/remote-hosts-admin/helper.sock"),
            Platform::Macos => Some("/Library/Application Support/RemoteHostsAdmin/helper.sock"),
            _ => None,
        };
        let helper = match socket {
            Some(path) => match std::fs::symlink_metadata(path) {
                Ok(_) => Helper::DetectedUnverified,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Helper::NotInstalled,
                Err(_) => Helper::Unavailable,
            },
            None => Helper::Unavailable,
        };
        Evidence {
            platform,
            identity,
            no_new_privileges,
            helper,
            grant: Grant::NotChecked,
        }
    }
}
pub fn parse_no_new_privileges(status: &str) -> Option<bool> {
    let values: Vec<_> = status
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == "NoNewPrivs").then_some(value.trim())
        })
        .collect();
    match values.as_slice() {
        ["0"] => Some(false),
        ["1"] => Some(true),
        _ => None,
    }
}
pub fn discover(e: &Evidence) -> Capability {
    let mut reasons = vec![Reason::BackendNotImplemented];
    match e.platform {
        Platform::Unsupported => reasons.push(Reason::PlatformUnsupported),
        Platform::Linux | Platform::Android => match e.no_new_privileges {
            Some(true) => reasons.push(Reason::NoNewPrivilegesBlocksSudo),
            None => reasons.push(Reason::NoNewPrivilegesUnknown),
            _ => {}
        },
        _ => {}
    }
    if e.identity == Identity::Unverified {
        reasons.push(Reason::IdentityUnverified);
    }
    if e.platform == Platform::Android {
        match e.identity {
            Identity::OrdinaryUser => reasons.push(Reason::AndroidAppSandbox),
            Identity::AdbShell => reasons.push(Reason::AndroidShellNotRoot),
            _ => {}
        }
    }
    match e.helper {
        Helper::NotInstalled => reasons.push(Reason::HelperNotInstalled),
        Helper::DetectedUnverified => reasons.push(Reason::HelperIdentityUnverified),
        Helper::Unavailable | Helper::Verified => {}
    }
    match e.grant {
        Grant::NotChecked => reasons.push(Reason::GrantNotVerified),
        Grant::NotGranted => reasons.push(Reason::GrantNotGranted),
        Grant::Revoked => reasons.push(Reason::GrantRevoked),
        Grant::Verified => {}
    }
    let backend = match e.platform {
        Platform::Linux => "systemd_helper",
        Platform::Macos => "signed_system_helper",
        Platform::Windows => "service_named_pipe",
        Platform::Android => match e.identity {
            Identity::Root => "android_root",
            Identity::AdbShell => "android_adb_shell",
            _ => "android_application",
        },
        Platform::Unsupported => "unsupported",
    };
    Capability {
        protocol: CONTRACT_VERSION,
        platform: e.platform,
        identity: e.identity,
        no_new_privileges: e.no_new_privileges,
        helper: e.helper,
        grant: e.grant,
        backend: backend.into(),
        execution_implemented: false,
        execution_available: false,
        arbitrary_root_commands: false,
        reasons,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    /// Existing operation identity, not a second task identity.
    pub request_id: String,
    pub device_id: String,
    pub action: Action,
    pub expected_policy_sha256: String,
    pub expected_target_sha256: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    BackendUnavailable,
    ObserveOriginal,
    ReadOriginalReceipt,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivilegePlan {
    pub protocol: u32,
    pub request: PlanRequest,
    pub observed: Capability,
    pub created_at: u64,
    pub expires_at: u64,
    pub execution_enabled: bool,
    /// A descriptive plan cannot attest an authenticated grant.
    pub grants_authority: bool,
    pub next_action: NextAction,
}
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
/// Reuses legacy receipt states. No persistence and no execute/replay entry point.
/// Adapters must pass the actual original receipt/uncertainty, never infer it from exit codes.
pub fn plan(
    request: PlanRequest,
    observed: Capability,
    previous: Option<&Record>,
    outcome_unknown: bool,
    now: u64,
) -> Result<PrivilegePlan> {
    valid_id(&request.request_id)?;
    valid_id(&request.device_id)?;
    ensure!(
        valid_digest(&request.expected_policy_sha256)
            && valid_digest(&request.expected_target_sha256),
        "invalid_expected_digest"
    );
    let mut next_action = if outcome_unknown {
        NextAction::ObserveOriginal
    } else {
        NextAction::BackendUnavailable
    };
    if let Some(record) = previous {
        ensure!(
            record.plan.request_id == request.request_id
                && record.plan.device_id == request.device_id,
            "original_receipt_identity_mismatch"
        );
        next_action = match record.state {
            State::Running | State::ClosedUnknown => NextAction::ObserveOriginal,
            State::Succeeded | State::Failed if !outcome_unknown => NextAction::ReadOriginalReceipt,
            _ => next_action,
        };
    }
    Ok(PrivilegePlan {
        protocol: CONTRACT_VERSION,
        request,
        observed,
        created_at: now,
        expires_at: now
            .checked_add(PLAN_TTL)
            .ok_or_else(|| anyhow::anyhow!("invalid_clock"))?,
        execution_enabled: false,
        grants_authority: false,
        next_action,
    })
}
impl PrivilegePlan {
    pub fn sha256(&self) -> Result<String> {
        digest(self)
    }
}

#[cfg(test)]
mod tests;
