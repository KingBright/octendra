//! Offline Windows pipe/SID policy. No Win32 collector, service, IPC or executor.
//! Inputs are untrusted observations; a passing fixture never authenticates a peer.
use crate::{
    privilege::{
        self, Action, Capability, Evidence, Grant, Helper, Identity, NextAction, PlanRequest,
        Platform, PrivilegePlan,
    },
    protocol::{Record, digest, valid_id},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const PIPE: &str = r"\\.\pipe\RemoteHosts.Admin.v1";
pub const HELPER_SERVICE: &str = "RemoteHostsAdmin";
pub const TARGET_SYSTEM_SERVICE: &str = "RemoteHostsAgent";
pub const LOCAL_SYSTEM: &str = "S-1-5-18";
/// Individual data rights plus synchronous I/O. Deliberately excludes
/// FILE_CREATE_PIPE_INSTANCE (0x4), generic rights, WRITE_DAC and WRITE_OWNER.
pub const CLIENT_ACCESS: u32 = 0x0010_0003;
/// Server read/write/attribute/control-read rights needed by CreateNamedPipe,
/// including create-instance; excludes DELETE, WRITE_DAC and WRITE_OWNER.
pub const SERVER_ACCESS: u32 = 0x0012_019f;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sid(String);
impl Sid {
    pub fn parse(value: &str) -> Result<Self> {
        ensure!(value.len() <= 184, "sid_too_long");
        let parts: Vec<_> = value.split('-').collect();
        ensure!(
            parts.len() >= 4
                && parts.len() <= 18
                && parts[0] == "S"
                && parts[1] == "1"
                && parts[2] == "5",
            "unsupported_sid"
        );
        for part in &parts[3..] {
            let number: u32 = part.parse()?;
            ensure!(number.to_string() == *part, "noncanonical_sid");
        }
        Ok(Self(value.into()))
    }
    fn components(&self) -> Vec<u32> {
        self.0
            .split('-')
            .skip(3)
            .map(|n| n.parse().expect("validated SID"))
            .collect()
    }
    fn account(&self) -> bool {
        matches!(self.components().as_slice(), [21, _, _, _, rid] if *rid >= 1000)
    }
    fn logon(&self) -> bool {
        matches!(self.components().as_slice(), [5, _, _])
    }
    fn service(&self) -> bool {
        matches!(self.components().as_slice(), [80, _, _, _, _, _])
    }
}
impl TryFrom<String> for Sid {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        Self::parse(&value)
    }
}
impl From<Sid> for String {
    fn from(value: Sid) -> Self {
        value.0
    }
}
fn checksum(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub protocol: u32,
    /// Existing device/grant identities; this module never generates identities.
    pub device_id: String,
    pub grant_id: String,
    pub enabled: bool,
    pub caller_sid: Sid,
    pub logon_sid: Sid,
    pub service_sid: Sid,
    pub helper_sha256: String,
    pub target_sha256: String,
    pub allowed_actions: Vec<Action>,
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.protocol == 2, "unsupported_windows_policy_protocol");
        valid_id(&self.device_id)?;
        valid_id(&self.grant_id)?;
        ensure!(
            self.caller_sid.account() && self.logon_sid.logon() && self.service_sid.service(),
            "unsupported_windows_sid_profile"
        );
        ensure!(
            checksum(&self.helper_sha256) && checksum(&self.target_sha256),
            "invalid_windows_policy_digest"
        );
        // First profile: one fixed system Agent service restart only. Neither
        // arbitrary SCM names nor user-service migration, upgrade or DNS repair.
        ensure!(
            self.allowed_actions == [Action::ManagedServiceRestart],
            "unsupported_windows_action_profile"
        );
        Ok(())
    }
    pub fn sha256(&self) -> Result<String> {
        self.validate()?;
        digest(self)
    }
    /// Expected descriptor model, never an ACL writer or SDDL installer.
    pub fn expected_descriptor(&self) -> Result<Descriptor> {
        self.validate()?;
        Ok(Descriptor {
            owner: Sid::parse(LOCAL_SYSTEM)?,
            protected: true,
            dacl: Some(vec![
                Ace {
                    sid: Sid::parse(LOCAL_SYSTEM)?,
                    access: SERVER_ACCESS,
                    inherited: false,
                    allow: true,
                },
                Ace {
                    sid: self.service_sid.clone(),
                    access: SERVER_ACCESS,
                    inherited: false,
                    allow: true,
                },
                Ace {
                    sid: self.logon_sid.clone(),
                    access: CLIENT_ACCESS,
                    inherited: false,
                    allow: true,
                },
            ]),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ace {
    pub sid: Sid,
    pub access: u32,
    pub inherited: bool,
    pub allow: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub owner: Sid,
    pub protected: bool,
    /// None is a NULL DACL and must never pass.
    pub dacl: Option<Vec<Ace>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    PipeImpersonation,
    ProcessPrimary,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpersonationLevel {
    Anonymous,
    Identification,
    Impersonation,
    Delegation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallerToken {
    pub source: TokenSource,
    pub level: ImpersonationLevel,
    /// TokenUser, not a group/display name or request-provided SID.
    pub user_sid: Sid,
    pub logon_sid: Sid,
    pub logon_group_enabled: bool,
    pub logon_group_is_logon_id: bool,
    pub logon_group_deny_only: bool,
    pub app_container: bool,
    pub restricted: bool,
}
/// A future native adapter must collect these from one retained pipe handle and
/// the token of its last read frame. JSON cannot establish their provenance.
/// There is intentionally no native CLI accepting these observations.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub pipe_name: String,
    pub local_client: bool,
    pub remote_clients_rejected: bool,
    pub first_instance_owned: bool,
    pub handle_inheritable: bool,
    pub descriptor: Descriptor,
    pub scm_service_name: String,
    pub scm_system_service: bool,
    pub scm_pid: u32,
    pub pipe_server_pid: u32,
    pub server_user_sid: Sid,
    pub server_service_sid: Sid,
    pub server_service_sid_enabled: bool,
    pub server_service_sid_deny_only: bool,
    pub server_image_sha256: String,
    pub target_service_name: String,
    pub target_system_service: bool,
    pub target_image_sha256: String,
    pub impersonation_succeeded: bool,
    pub revert_succeeded: bool,
    pub caller: Option<CallerToken>,
    /// Digest of the existing PlanRequest in the last frame read on that handle.
    pub frame_request_sha256: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Denial {
    Disabled,
    GrantNotVerified,
    GrantRevoked,
    RequestBinding,
    PipeBoundary,
    DescriptorMismatch,
    ServiceIdentity,
    CallerTokenUnavailable,
    CallerIdentity,
    OriginalOperationRequiresObservation,
}
#[derive(Debug, Serialize)]
pub struct Assessment {
    pub protocol: u32,
    pub offline_policy_checks_passed: bool,
    pub denials: Vec<Denial>,
    pub plan: PrivilegePlan,
    pub binding_sha256: String,
    /// Exact provided frame bytes, including newline; absent on unframed assessment.
    /// This alone is not OS provenance or peer authentication.
    pub frame_sha256: Option<String>,
    pub native_peer_authenticated: bool,
    pub grants_authority: bool,
}
/// Descriptive policy evaluation only: reuses shared action/plan/receipt/Grant
/// semantics. Does not persist, clear revocation, resolve unknown or dispatch.
pub fn assess(
    policy: &Policy,
    request: PlanRequest,
    observed: &Observation,
    grant: Grant,
    previous: Option<&Record>,
    outcome_unknown: bool,
    now: u64,
) -> Result<Assessment> {
    policy.validate()?;
    let mut denials = Vec::new();
    if !policy.enabled {
        denials.push(Denial::Disabled);
    }
    match grant {
        Grant::Verified => {}
        Grant::Revoked => denials.push(Denial::GrantRevoked),
        _ => denials.push(Denial::GrantNotVerified),
    }
    if request.device_id != policy.device_id
        || request.expected_policy_sha256 != policy.sha256()?
        || request.expected_target_sha256 != policy.target_sha256
        || !policy.allowed_actions.contains(&request.action)
        || observed.frame_request_sha256 != digest(&request)?
    {
        denials.push(Denial::RequestBinding);
    }
    if observed.pipe_name != PIPE
        || !observed.local_client
        || !observed.remote_clients_rejected
        || !observed.first_instance_owned
        || observed.handle_inheritable
    {
        denials.push(Denial::PipeBoundary);
    }
    if observed.descriptor != policy.expected_descriptor()? {
        denials.push(Denial::DescriptorMismatch);
    }
    if observed.scm_service_name != HELPER_SERVICE
        || !observed.scm_system_service
        || observed.scm_pid == 0
        || observed.scm_pid != observed.pipe_server_pid
        || observed.server_user_sid.0 != LOCAL_SYSTEM
        || observed.server_service_sid != policy.service_sid
        || !observed.server_service_sid_enabled
        || observed.server_service_sid_deny_only
        || observed.server_image_sha256 != policy.helper_sha256
        || observed.target_service_name != TARGET_SYSTEM_SERVICE
        || !observed.target_system_service
        || observed.target_image_sha256 != policy.target_sha256
    {
        denials.push(Denial::ServiceIdentity);
    }
    match &observed.caller {
        None => denials.push(Denial::CallerTokenUnavailable),
        Some(c) => {
            if !observed.impersonation_succeeded
                || !observed.revert_succeeded
                || c.source != TokenSource::PipeImpersonation
                || c.level != ImpersonationLevel::Impersonation
                || c.user_sid != policy.caller_sid
                || c.logon_sid != policy.logon_sid
                || !c.logon_group_enabled
                || !c.logon_group_is_logon_id
                || c.logon_group_deny_only
                || c.app_container
                || c.restricted
            {
                denials.push(Denial::CallerIdentity);
            }
        }
    }
    // Caller-provided fixture observations must not be promoted to verified
    // peer/grant capability. Preserve revocation as a shared blocking fact.
    let capability: Capability = privilege::discover(&Evidence {
        platform: Platform::Windows,
        identity: Identity::Unverified,
        no_new_privileges: None,
        helper: Helper::DetectedUnverified,
        grant: if grant == Grant::Revoked {
            Grant::Revoked
        } else {
            Grant::NotChecked
        },
    });
    let plan = privilege::plan(request, capability, previous, outcome_unknown, now)?;
    if plan.next_action != NextAction::BackendUnavailable {
        denials.push(Denial::OriginalOperationRequiresObservation);
    }
    Ok(Assessment {
        protocol: 1,
        offline_policy_checks_passed: denials.is_empty(),
        binding_sha256: digest(&(policy, observed, grant, &plan))?,
        denials,
        plan,
        frame_sha256: None,
        native_peer_authenticated: false,
        grants_authority: false,
    })
}
#[cfg(test)]
mod tests;
