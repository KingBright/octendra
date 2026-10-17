//! One fixed Linux action over the existing authenticated helper/Record protocol.
//! No installer, shell, script runner, network listener or grant creation.
use crate::{
    engine::{Backend, OutcomeUnknown, unix_time},
    executor::{command, output},
    filesystem::{Store, inspect_file, private_read, trusted_dir},
    privilege::{Action, PlanRequest},
    protocol::{Policy, Record, Snapshot, UnitState, digest, valid_id},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

pub const SERVICE: &str = "remote-hosts-code-agent.service";
pub const ACTION: &str = "managed_service_restart";

/// Installed by a separately approved administrator, never accepted in an IPC request.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceGrant {
    pub protocol: u32,
    pub device_id: String,
    pub grant_id: String,
    pub enabled: bool,
    pub not_before: u64,
    pub expires_at: u64,
    pub action: Action,
    pub unit: String,
    pub unit_sha256: String,
    pub executable: String,
    pub executable_sha256: String,
    pub agent_config: String,
    pub agent_config_sha256: String,
}
fn valid_sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn exact_path(value: &str) -> Result<()> {
    ensure!(
        value.starts_with('/') && value.len() < 512,
        "invalid_resource_path"
    );
    ensure!(
        value
            .split('/')
            .skip(1)
            .all(|p| !p.is_empty() && p != "." && p != ".."),
        "invalid_resource_components"
    );
    ensure!(
        !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "'\"\\;$`%".contains(c)),
        "unsafe_resource_path"
    );
    Ok(())
}
impl ServiceGrant {
    pub fn validate(&self, policy: &Policy, now: u64) -> Result<()> {
        policy.validate()?;
        ensure!(policy.allowed_gid != 0, "root_group_not_allowed");
        valid_id(&self.device_id)?;
        valid_id(&self.grant_id)?;
        ensure!(
            self.protocol == 1 && policy.platform == "linux",
            "unsupported_service_grant"
        );
        ensure!(
            self.device_id == policy.device_id && self.grant_id == policy.grant_id,
            "service_grant_identity_mismatch"
        );
        ensure!(
            self.enabled && now >= self.not_before && now < self.expires_at,
            "service_grant_disabled_or_expired"
        );
        ensure!(
            self.action == Action::ManagedServiceRestart && self.unit == SERVICE,
            "service_action_or_resource_not_allowed"
        );
        ensure!(
            [
                &self.unit_sha256,
                &self.executable_sha256,
                &self.agent_config_sha256
            ]
            .into_iter()
            .all(|s| valid_sha(s)),
            "invalid_service_digest"
        );
        exact_path(&self.executable)?;
        exact_path(&self.agent_config)?;
        ensure!(
            matches!(
                Path::new(&self.executable)
                    .file_name()
                    .and_then(|s| s.to_str()),
                Some("remote-hosts-code" | "octendra")
            ),
            "unsupported_service_executable"
        );
        ensure!(
            self.executable != self.agent_config,
            "overlapping_service_resources"
        );
        Ok(())
    }
}

#[allow(async_fn_in_trait)]
pub trait ServiceDriver {
    fn grant(&self) -> Result<ServiceGrant>;
    async fn inspect(&self, policy: &Policy, grant: &ServiceGrant) -> Result<Snapshot>;
    /// No caller-supplied program, argv, environment or service name.
    async fn restart(&self) -> Result<()>;
}
pub struct ManagedServiceBackend<D> {
    pub driver: D,
}
impl<D: ServiceDriver> Backend for ManagedServiceBackend<D> {
    fn action(&self) -> &'static str {
        ACTION
    }
    fn policy_binding(&self, policy: &Policy, now: u64) -> Result<String> {
        let grant = self.driver.grant()?;
        grant.validate(policy, now)?;
        digest(&(policy, grant))
    }
    fn validate_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        ensure!(
            snapshot.current_service_active,
            "managed_service_not_active"
        );
        ensure!(
            snapshot.units.len() == 1 && snapshot.units[0].name == SERVICE,
            "managed_service_snapshot_scope_mismatch"
        );
        Ok(())
    }
    async fn inspect(&self, policy: &Policy) -> Result<Snapshot> {
        let grant = self.driver.grant()?;
        grant.validate(policy, unix_time())?;
        self.driver.inspect(policy, &grant).await
    }
    async fn execute(
        &self,
        policy: &Policy,
        store: &Store,
        record: &mut Record,
    ) -> Result<Snapshot> {
        ensure!(
            record.plan.action == ACTION,
            "managed_service_action_mismatch"
        );
        ensure!(
            record.plan.policy_sha256 == self.policy_binding(policy, unix_time())?,
            "service_grant_changed"
        );
        let before = self.inspect(policy).await?;
        self.validate_snapshot(&before)?;
        ensure!(before == record.plan.snapshot, "service_resources_changed");
        record.steps.push("managed_service_restart_accepted".into());
        store.save(record)?; // Running was already fsynced by Engine, before restart.
        self.driver.restart().await?;
        let after = self.inspect(policy).await.map_err(|_| OutcomeUnknown)?;
        self.validate_snapshot(&after).map_err(|_| OutcomeUnknown)?;
        Ok(after)
    }
}

/// Root-only native driver. Tests use an isolated driver, never systemctl.
pub struct LinuxServiceDriver {
    grant_path: PathBuf,
}
impl LinuxServiceDriver {
    pub fn new(grant_path: PathBuf) -> Self {
        Self { grant_path }
    }
}
pub(crate) fn trusted_resource(
    path: &Path,
    expected: &str,
    executable: bool,
) -> Result<crate::protocol::FileState> {
    trusted_dir(path.parent().context("resource_parent_missing")?, 0)?;
    let s = inspect_file(path.to_str().context("invalid_resource_utf8")?, 0)?
        .context("resource_missing")?;
    ensure!(
        s.uid == 0 && s.mode & 0o022 == 0 && s.sha256 == expected,
        "untrusted_or_changed_service_resource"
    );
    if executable {
        ensure!(
            s.mode & 0o111 != 0 && s.mode & 0o6000 == 0,
            "invalid_service_executable_mode"
        );
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)?;
        ensure!(
            file.metadata()?.ino() == s.inode && file.metadata()?.dev() == s.device,
            "service_executable_replaced"
        );
        let mut magic = [0; 4];
        file.read_exact(&mut magic)?;
        ensure!(
            magic == *b"\x7fELF",
            "scripts_and_non_elf_executables_rejected"
        );
    }
    Ok(s)
}

/// Deliberately narrow service profile: no shell, lifecycle hooks, environment,
/// namespace/path overrides, dependencies that could activate another service,
/// continuation syntax, duplicate directives or systemd execution prefixes.
pub(crate) fn validate_unit(text: &str, grant: &ServiceGrant, uid: u32, gid: u32) -> Result<()> {
    let expected_start = format!("{} agent --config {}", grant.executable, grant.agent_config);
    let mut section = "";
    let mut seen = std::collections::BTreeSet::new();
    let mut user = false;
    let mut group = false;
    let mut start = false;
    let mut nnp = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        ensure!(
            !line.ends_with('\\') && !line.chars().any(char::is_control),
            "unsupported_unit_syntax"
        );
        if line.starts_with('[') {
            ensure!(
                matches!(line, "[Unit]" | "[Service]" | "[Install]"),
                "unsupported_unit_section"
            );
            section = line;
            continue;
        }
        let (key, value) = line.split_once('=').context("invalid_unit_directive")?;
        ensure!(
            seen.insert((section.to_owned(), key.to_owned())),
            "duplicate_unit_directive"
        );
        match (section, key) {
            ("[Service]", "User") => {
                let resolved = value.parse::<u32>().ok().or_else(|| {
                    nix::unistd::User::from_name(value)
                        .ok()
                        .flatten()
                        .map(|u| u.uid.as_raw())
                });
                ensure!(
                    resolved == Some(uid) && uid != 0,
                    "service_user_not_authorized"
                );
                user = true;
            }
            ("[Service]", "Group") => {
                let resolved = value.parse::<u32>().ok().or_else(|| {
                    nix::unistd::Group::from_name(value)
                        .ok()
                        .flatten()
                        .map(|g| g.gid.as_raw())
                });
                ensure!(resolved == Some(gid), "service_group_not_authorized");
                group = true;
            }
            ("[Service]", "ExecStart") => {
                ensure!(value == expected_start, "service_command_not_allowed");
                start = true;
            }
            ("[Service]", "NoNewPrivileges") => {
                ensure!(
                    matches!(value, "yes" | "true"),
                    "service_must_keep_no_new_privileges"
                );
                nnp = true;
            }
            ("[Service]", "Type") => ensure!(value == "simple", "unsupported_service_type"),
            ("[Service]", "WorkingDirectory") => {
                ensure!(value == "/", "unsupported_service_working_directory")
            }
            ("[Service]", "Restart") => ensure!(
                matches!(value, "no" | "on-failure"),
                "unsupported_restart_policy"
            ),
            ("[Service]", "RestartSec") => ensure!(
                !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                "invalid_restart_delay"
            ),
            ("[Service]", "StandardOutput" | "StandardError") => {
                ensure!(value == "journal", "unsupported_service_output")
            }
            ("[Unit]", "Description" | "After" | "Before" | "Documentation") => {}
            ("[Install]", "WantedBy") => {
                ensure!(value == "multi-user.target", "unsupported_install_target")
            }
            _ => anyhow::bail!("unsupported_or_unsafe_service_directive"),
        }
    }
    ensure!(
        user && group && start && nnp,
        "required_service_identity_or_command_missing"
    );
    Ok(())
}
impl ServiceDriver for LinuxServiceDriver {
    fn grant(&self) -> Result<ServiceGrant> {
        ensure!(
            cfg!(target_os = "linux") && nix::unistd::geteuid().is_root(),
            "linux_root_helper_required"
        );
        trusted_dir(self.grant_path.parent().context("grant_parent_missing")?, 0)?;
        Ok(serde_json::from_slice(&private_read(&self.grant_path, 0)?)?)
    }
    async fn inspect(&self, policy: &Policy, grant: &ServiceGrant) -> Result<Snapshot> {
        ensure!(
            cfg!(target_os = "linux") && nix::unistd::geteuid().is_root(),
            "linux_root_helper_required"
        );
        let unit_path = PathBuf::from(format!("/etc/systemd/system/{SERVICE}"));
        let unit = trusted_resource(&unit_path, &grant.unit_sha256, false)?;
        ensure!(unit.size <= 65536, "service_unit_too_large");
        let mut bytes = Vec::new();
        OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&unit_path)?
            .take(65537)
            .read_to_end(&mut bytes)?;
        use sha2::{Digest, Sha256};
        ensure!(
            format!("{:x}", Sha256::digest(&bytes)) == grant.unit_sha256,
            "service_unit_changed"
        );
        validate_unit(
            std::str::from_utf8(&bytes)?,
            grant,
            policy.allowed_uid,
            policy.allowed_gid,
        )?;
        let executable =
            trusted_resource(Path::new(&grant.executable), &grant.executable_sha256, true)?;
        let config = trusted_resource(
            Path::new(&grant.agent_config),
            &grant.agent_config_sha256,
            false,
        )?;
        let controller = trusted_resource(
            Path::new("/usr/bin/systemctl"),
            &{
                trusted_dir(Path::new("/usr/bin"), 0)?;
                inspect_file("/usr/bin/systemctl", 0)?
                    .context("systemctl_missing")?
                    .sha256
            },
            true,
        )?;
        let (ok, text) = output(command("/usr/bin/systemctl", &["show", "--property=LoadState,ActiveState,UnitFileState,FragmentPath,DropInPaths,Transient,NeedDaemonReload", "--", SERVICE]), 16384).await?;
        ensure!(ok, "service_query_failed");
        let props: std::collections::BTreeMap<_, _> =
            text.lines().filter_map(|s| s.split_once('=')).collect();
        let get = |key| props.get(key).copied().context("missing_service_property");
        ensure!(
            get("LoadState")? == "loaded"
                && get("Transient")? == "no"
                && get("NeedDaemonReload")? == "no",
            "service_not_loaded_or_needs_reload"
        );
        ensure!(
            get("FragmentPath")? == unit_path.to_str().unwrap_or_default()
                && get("DropInPaths")?.is_empty(),
            "service_fragment_or_dropins_not_allowed"
        );
        let active = get("ActiveState")? == "active";
        Ok(Snapshot {
            files: vec![Some(unit), Some(executable), Some(config), Some(controller)],
            units: vec![UnitState {
                name: SERVICE.into(),
                load: get("LoadState")?.into(),
                active: get("ActiveState")?.into(),
                enabled: get("UnitFileState")?.into(),
                fragment: get("FragmentPath")?.into(),
                drop_ins: String::new(),
            }],
            current_service_active: active,
            ..Snapshot::default()
        })
    }
    async fn restart(&self) -> Result<()> {
        ensure!(
            cfg!(target_os = "linux") && nix::unistd::geteuid().is_root(),
            "linux_root_helper_required"
        );
        let mut cmd = command("/usr/bin/systemctl", &["restart", "--", SERVICE]);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = cmd.spawn().context("service_restart_not_started")?;
        // Once submitted, nonzero/timeout/wait failure can all hide a real restart.
        let result = tokio::time::timeout(Duration::from_secs(100), child.wait()).await;
        match result {
            Ok(Ok(status)) if status.success() => Ok(()),
            _ => Err(OutcomeUnknown.into()),
        }
    }
}

/// Shared adapter check: callers retain their original operation UUID and attest
/// expected device/action/policy/target bindings; this does not grant authority.
/// Remote owner/session authentication remains the existing Gateway's job.
pub fn validate_operation_binding(expected: &PlanRequest, record: &Record) -> Result<()> {
    valid_id(&expected.request_id)?;
    valid_id(&expected.device_id)?;
    ensure!(
        expected.action == Action::ManagedServiceRestart && record.plan.action == ACTION,
        "operation_action_mismatch"
    );
    ensure!(
        record.plan.request_id == expected.request_id
            && record.plan.device_id == expected.device_id,
        "operation_identity_mismatch"
    );
    ensure!(
        record.plan.policy_sha256 == expected.expected_policy_sha256
            && digest(&record.plan.snapshot)? == expected.expected_target_sha256,
        "operation_policy_or_target_mismatch"
    );
    ensure!(
        digest(&record.plan)? == record.plan_sha256,
        "operation_plan_digest_mismatch"
    );
    Ok(())
}

#[allow(async_fn_in_trait)]
pub(crate) trait RecordClient {
    async fn request(&self, request: &crate::protocol::Request) -> Result<serde_json::Value>;
}
struct SocketClient<'a>(&'a Path);
impl RecordClient for SocketClient<'_> {
    async fn request(&self, request: &crate::protocol::Request) -> Result<serde_json::Value> {
        crate::transport::request(self.0, request).await
    }
}
fn bound_response(expected: &PlanRequest, value: serde_json::Value) -> Result<Record> {
    ensure!(
        value.get("ok").and_then(|v| v.as_bool()) == Some(true),
        "helper_rejected_observe_original"
    );
    let record: Record = serde_json::from_value(
        value
            .get("result")
            .context("helper_receipt_missing")?
            .clone(),
    )?;
    validate_operation_binding(expected, &record)?;
    Ok(record)
}
/// Uses the existing root-peer-checked socket. An unknown client outcome cannot
/// submit apply; reconnection must observe the original receipt first.
pub async fn apply_bound(
    socket: &Path,
    expected: &PlanRequest,
    original: &Record,
    outcome_unknown: bool,
) -> Result<Record> {
    apply_bound_using(&SocketClient(socket), expected, original, outcome_unknown).await
}
pub(crate) async fn apply_bound_using<C: RecordClient>(
    client: &C,
    expected: &PlanRequest,
    original: &Record,
    outcome_unknown: bool,
) -> Result<Record> {
    use crate::protocol::{Request, State};
    validate_operation_binding(expected, original)?;
    ensure!(
        !outcome_unknown && original.state != State::Running,
        "observe_original_before_apply"
    );
    ensure!(
        original.state == State::Prepared,
        "read_original_receipt_no_replay"
    );
    let current = bound_response(
        expected,
        client
            .request(&Request::Receipt {
                request_id: expected.request_id.clone(),
            })
            .await?,
    )?;
    ensure!(
        current.plan_sha256 == original.plan_sha256,
        "original_plan_changed"
    );
    if current.state != State::Prepared {
        return Ok(current);
    }
    // Exactly one submission. Any transport error is propagated; no reconnect/retry.
    bound_response(
        expected,
        client
            .request(&Request::Apply {
                request_id: expected.request_id.clone(),
                plan_sha256: original.plan_sha256.clone(),
            })
            .await?,
    )
}

#[cfg(test)]
mod tests;
