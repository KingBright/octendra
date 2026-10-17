//! Static package and installation review data. Never installs, starts services,
//! invokes a shell, resets revocation, or treats caller-owned files as root authority.
use crate::{
    engine::unix_time,
    filesystem::{inspect_file, trusted_dir},
    managed_service::{self, LinuxServiceDriver, ServiceDriver, ServiceGrant},
    privilege::Action,
    protocol::{PLAN_TTL, Plan, Policy, Record, Snapshot, State, digest, valid_id},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
pub const PROFILE: &str = "linux-managed-service-v1";
pub const INSTALL_ACTION: &str = "prepare_linux_managed_helper_installation";
pub const HELPER_SERVICE: &str = "remote-hosts-admin.service";
pub const HELPER_UNIT: &str = include_str!("assets/linux-managed-helper.service");
const NAMES: [&str; 5] = [
    "remote-hosts-admin",
    "policy.json",
    "service-grant.json",
    "service.template",
    "INSTALL.txt",
];
const JSON_LIMIT: u64 = 256 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub protocol: u32,
    pub profile: String,
    pub helper_version: String,
    pub platform: String,
    pub machine: String,
    pub device_id: String,
    pub account: String,
    pub source_commit: String,
    pub source_inputs_sha256: String,
    pub validation_kind: String,
    pub status: String,
    pub default_enabled: bool,
    pub files: BTreeMap<String, String>,
}
pub struct Bundle {
    pub manifest: Manifest,
    pub manifest_sha256: String,
    pub policy: Policy,
    pub grant: ServiceGrant,
}
pub fn bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file() && before.nlink() == 1 && before.len() <= limit,
        "input_not_bounded_single_link_regular_file"
    );
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= limit, "input_grew_too_large");
    Ok(data)
}
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&bytes(path, JSON_LIMIT)?)?)
}
fn sha(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}
fn valid_sha(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn write_new<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let data = serde_json::to_vec_pretty(value)?;
    ensure!(data.len() as u64 <= JSON_LIMIT, "output_record_too_large");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(&data)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?
    .sync_all()?;
    Ok(())
}
fn static_elf(data: &[u8]) -> Result<()> {
    ensure!(
        data.len() >= 64 && &data[..4] == b"\x7fELF" && data[4] == 2 && data[5] == 1,
        "linux_x86_64_elf_required"
    );
    ensure!(
        u16::from_le_bytes(data[18..20].try_into()?) == 62,
        "wrong_elf_machine"
    );
    let offset = usize::try_from(u64::from_le_bytes(data[32..40].try_into()?))?;
    let size = usize::from(u16::from_le_bytes(data[54..56].try_into()?));
    let count = usize::from(u16::from_le_bytes(data[56..58].try_into()?));
    ensure!(
        count > 0 && count <= 1024 && size >= 56,
        "invalid_elf_program_table"
    );
    let end = offset
        .checked_add(size.checked_mul(count).context("elf_bounds")?)
        .context("elf_bounds")?;
    ensure!(end <= data.len(), "truncated_elf_program_table");
    for i in 0..count {
        let p = offset + i * size;
        ensure!(
            u32::from_le_bytes(data[p..p + 4].try_into()?) != 3,
            "dynamic_elf_interpreter_not_allowed"
        );
    }
    Ok(())
}
pub fn verify(bundle: &Path, expected: &str) -> Result<Bundle> {
    ensure!(valid_sha(expected), "invalid_manifest_digest");
    let raw = bytes(&bundle.join("manifest.json"), JSON_LIMIT)?;
    ensure!(sha(&raw) == expected, "manifest_digest_mismatch");
    let m: Manifest = serde_json::from_slice(&raw)?;
    for entry in fs::read_dir(bundle)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_str().context("package_name_not_utf8")?;
        ensure!(
            NAMES.contains(&name)
                || matches!(
                    name,
                    "manifest.json" | "MANIFEST.sha256" | "prepared-validation.json"
                ),
            "unlisted_package_member"
        );
        let kind = entry.file_type()?;
        ensure!(
            kind.is_file() && !kind.is_symlink(),
            "non_regular_package_member"
        );
    }
    ensure!(
        m.protocol == 2
            && m.profile == PROFILE
            && m.platform == "linux"
            && m.machine == "x86_64"
            && m.helper_version == env!("CARGO_PKG_VERSION")
            && !m.default_enabled
            && m.status == "prepared_disabled"
            && m.validation_kind == "unsigned-dev",
        "unsupported_or_enabled_package"
    );
    valid_id(&m.device_id)?;
    ensure!(
        !m.account.is_empty()
            && m.account.len() < 64
            && m.account
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid_account"
    );
    ensure!(
        m.source_commit.len() == 40
            && m.source_commit.bytes().all(|b| b.is_ascii_hexdigit())
            && valid_sha(&m.source_inputs_sha256),
        "invalid_source_identity"
    );
    ensure!(
        m.files.len() == NAMES.len() && NAMES.iter().all(|n| m.files.contains_key(*n)),
        "unexpected_bundle_members"
    );
    let mut snapshots = BTreeMap::new();
    for name in NAMES {
        let data = bytes(
            &bundle.join(name),
            if name == "remote-hosts-admin" {
                128 * 1024 * 1024
            } else {
                JSON_LIMIT
            },
        )?;
        ensure!(m.files[name] == sha(&data), "bundle_member_digest_mismatch");
        snapshots.insert(name, data);
    }
    static_elf(&snapshots["remote-hosts-admin"])?;
    ensure!(
        snapshots["service.template"] == HELPER_UNIT.as_bytes(),
        "wrong_helper_service_entry"
    );
    let policy: Policy = serde_json::from_slice(&snapshots["policy.json"])?;
    policy.validate()?;
    let grant: ServiceGrant = serde_json::from_slice(&snapshots["service-grant.json"])?;
    ensure!(
        !policy.enabled
            && !grant.enabled
            && grant.not_before == 0
            && grant.expires_at == 0
            && policy.platform == "linux"
            && policy.device_id == m.device_id
            && grant.device_id == policy.device_id
            && grant.grant_id == policy.grant_id
            && grant.unit == managed_service::SERVICE
            && grant.action == Action::ManagedServiceRestart,
        "bundle_must_be_disabled_fixed_profile"
    );
    let mut probe = grant.clone();
    probe.enabled = true;
    probe.not_before = 0;
    probe.expires_at = u64::MAX;
    probe.validate(&policy, unix_time())?;
    Ok(Bundle {
        manifest: m,
        manifest_sha256: expected.into(),
        policy,
        grant,
    })
}
pub fn verify_report(bundle: &Path, expected: &str) -> Result<Value> {
    let b = verify(bundle, expected)?;
    Ok(
        json!({"profile":PROFILE,"manifest_sha256":expected,"helper_sha256":b.manifest.files["remote-hosts-admin"],
        "source_commit":b.manifest.source_commit,"device_id":b.manifest.device_id,"default_enabled":false,
        "installed":false,"execution_available":false,"grants_authority":false,
        "action":managed_service::ACTION,"resource":managed_service::SERVICE,
        "adapters":{"linux":"native_cli_preparation_and_fixed_service","macos":"signed_helper_adapter_required","windows":"service_acl_adapter_required","android":"platform_authorization_adapter_required"}}),
    )
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteEvidence {
    pub protocol: u32,
    pub manifest_sha256: String,
    pub account: String,
    pub policy: Policy,
    pub grant: ServiceGrant,
    pub snapshot: Snapshot,
    pub boot_id: String,
    pub observed_at: u64,
}
fn root_bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    trusted_dir(path.parent().context("resource_parent_missing")?, 0)?;
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.uid() == 0 && meta.mode() & 0o022 == 0,
        "site_resource_not_root_controlled"
    );
    bytes(path, limit)
}
pub async fn inspect_site(bundle: &Path, expected: &str) -> Result<SiteEvidence> {
    ensure!(
        cfg!(target_os = "linux") && nix::unistd::geteuid().is_root(),
        "site_inspection_requires_separate_linux_administrator"
    );
    let b = verify(bundle, expected)?;
    ensure!(
        std::env::consts::ARCH == b.manifest.machine,
        "site_architecture_mismatch"
    );
    managed_service::trusted_resource(
        &std::env::current_exe()?,
        &b.manifest.files["remote-hosts-admin"],
        true,
    )?;
    let user =
        nix::unistd::User::from_name(&b.manifest.account)?.context("site_account_missing")?;
    ensure!(
        user.uid.as_raw() == b.policy.allowed_uid && user.gid.as_raw() == b.policy.allowed_gid,
        "site_uid_gid_mismatch"
    );
    let mut policy = b.policy.clone();
    let home = user.dir.to_str().context("site_home_not_utf8")?.to_owned();
    ensure!(
        policy.home == "/unconfirmed" || policy.home == home,
        "site_home_mismatch"
    );
    policy.home = home;
    policy.validate()?;
    let unit_path = PathBuf::from(format!("/etc/systemd/system/{}", managed_service::SERVICE));
    let raw = root_bytes(&unit_path, 65536)?;
    let text = std::str::from_utf8(&raw)?;
    let line = text
        .lines()
        .find_map(|l| l.strip_prefix("ExecStart="))
        .context("site_exec_start_missing")?;
    let args: Vec<_> = line.split_ascii_whitespace().collect();
    ensure!(
        args.len() == 4 && args[1] == "agent" && args[2] == "--config",
        "site_service_command_not_supported"
    );
    let mut grant = b.grant.clone();
    grant.unit_sha256 = sha(&raw);
    grant.executable = args[0].into();
    grant.agent_config = args[3].into();
    grant.executable_sha256 = inspect_file(&grant.executable, 0)?
        .context("site_executable_missing")?
        .sha256;
    let config = root_bytes(Path::new(&grant.agent_config), 65536)?;
    grant.agent_config_sha256 = sha(&config);
    let now = unix_time();
    let mut probe = grant.clone();
    probe.enabled = true;
    probe.not_before = now;
    probe.expires_at = now.checked_add(PLAN_TTL).context("clock_overflow")?;
    probe.validate(&policy, now)?;
    managed_service::validate_unit(text, &probe, policy.allowed_uid, policy.allowed_gid)?;
    let value: Value =
        serde_json::from_slice(&config).context("site_agent_identity_json_invalid")?;
    ensure!(
        value.get("device_id").and_then(Value::as_str) == Some(b.manifest.device_id.as_str()),
        "site_enrolled_device_mismatch"
    );
    let snapshot = LinuxServiceDriver::new(PathBuf::new())
        .inspect(&policy, &probe)
        .await?;
    ensure!(snapshot.current_service_active, "site_service_not_active");
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    valid_id(&boot_id)?;
    Ok(SiteEvidence {
        protocol: 1,
        manifest_sha256: expected.into(),
        account: b.manifest.account,
        policy,
        grant,
        snapshot,
        boot_id,
        observed_at: now,
    })
}
fn site_binding(site: &SiteEvidence) -> Result<String> {
    digest(&(
        site.protocol,
        &site.manifest_sha256,
        &site.account,
        &site.policy,
        &site.grant,
        &site.snapshot,
        &site.boot_id,
    ))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedData {
    pub policy: Policy,
    pub grant: ServiceGrant,
    pub allow_unattended: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallReview {
    pub protocol: u32,
    pub record: Record,
    pub manifest_sha256: String,
    pub site: Option<SiteEvidence>,
    pub approved: Option<ApprovedData>,
    pub ready_for_confirmation: bool,
    pub blocked: Vec<String>,
    pub grants_authority: bool,
    pub execution_enabled: bool,
    pub destinations: BTreeMap<String, String>,
}
fn destinations() -> BTreeMap<String, String> {
    [
        (
            "remote-hosts-admin",
            "/var/lib/remote-hosts-admin/bin/remote-hosts-admin",
        ),
        ("policy.json", "/var/lib/remote-hosts-admin/policy.json"),
        (
            "service-grant.json",
            "/var/lib/remote-hosts-admin/service-grant.json",
        ),
        (
            "service.template",
            "/etc/systemd/system/remote-hosts-admin.service",
        ),
    ]
    .into_iter()
    .map(|(a, b)| (a.into(), b.into()))
    .collect()
}
fn review_binding(
    manifest: &str,
    site: &Option<SiteEvidence>,
    approved: &Option<ApprovedData>,
    paths: &BTreeMap<String, String>,
) -> Result<String> {
    digest(&(
        PROFILE,
        manifest,
        site.as_ref().map(site_binding).transpose()?,
        approved,
        paths,
    ))
}
fn make_review(
    b: &Bundle,
    id: &str,
    site: Option<SiteEvidence>,
    from: Option<u64>,
    until: Option<u64>,
    unattended: bool,
    now: u64,
) -> Result<InstallReview> {
    valid_id(id)?;
    ensure!(
        from.is_some() == until.is_some(),
        "grant_validity_requires_both_bounds"
    );
    let approved = if let (Some(site), Some(from), Some(until)) = (&site, from, until) {
        ensure!(unattended, "explicit_unattended_scope_required");
        let mut policy = site.policy.clone();
        policy.enabled = true;
        let mut grant = site.grant.clone();
        grant.enabled = true;
        grant.not_before = from;
        grant.expires_at = until;
        grant.validate(&policy, now)?;
        Some(ApprovedData {
            policy,
            grant,
            allow_unattended: true,
        })
    } else {
        None
    };
    let mut blocked = Vec::new();
    if site.is_none() {
        blocked.push("fresh_root_site_identity_and_resources_required".into());
    }
    if approved.is_none() {
        blocked.push("explicit_validity_and_unattended_scope_required".into());
    }
    let paths = destinations();
    let plan = Plan {
        protocol: 1,
        action: INSTALL_ACTION.into(),
        request_id: id.into(),
        device_id: b.policy.device_id.clone(),
        caller_uid: nix::unistd::geteuid().as_raw(),
        policy_sha256: review_binding(&b.manifest_sha256, &site, &approved, &paths)?,
        created_at: now,
        expires_at: now.checked_add(PLAN_TTL).context("clock_overflow")?,
        snapshot: site
            .as_ref()
            .map(|s| s.snapshot.clone())
            .unwrap_or_default(),
    };
    let record = Record {
        plan_sha256: digest(&plan)?,
        plan,
        state: State::Prepared,
        steps: vec!["installation_preparation_only_no_system_changes".into()],
        error: None,
        after: None,
        updated_at: now,
    };
    Ok(InstallReview {
        protocol: 1,
        record,
        manifest_sha256: b.manifest_sha256.clone(),
        site,
        approved,
        ready_for_confirmation: blocked.is_empty(),
        blocked,
        grants_authority: false,
        execution_enabled: false,
        destinations: paths,
    })
}
pub async fn prepare(
    bundle: &Path,
    expected: &str,
    id: &str,
    site_path: Option<&Path>,
    from: Option<u64>,
    until: Option<u64>,
    unattended: bool,
) -> Result<InstallReview> {
    let b = verify(bundle, expected)?;
    let site = if let Some(path) = site_path {
        let site: SiteEvidence = read_json(path)?;
        let now = unix_time();
        ensure!(
            site.protocol == 1 && site.observed_at <= now && now - site.observed_at < PLAN_TTL,
            "site_evidence_expired"
        );
        let fresh = inspect_site(bundle, expected).await?;
        ensure!(
            site_binding(&site)? == site_binding(&fresh)?,
            "site_evidence_changed"
        );
        Some(fresh)
    } else {
        None
    };
    make_review(&b, id, site, from, until, unattended, unix_time())
}
pub async fn render(
    bundle: &Path,
    plan_path: &Path,
    accepted: &str,
    output: &Path,
) -> Result<Value> {
    let review: InstallReview = read_json(plan_path)?;
    ensure!(
        review.record.plan_sha256 == accepted && digest(&review.record.plan)? == accepted,
        "installation_plan_digest_mismatch"
    );
    let b = verify(bundle, &review.manifest_sha256)?;
    let fresh = inspect_site(bundle, &review.manifest_sha256).await?;
    render_verified(b, review, accepted, output, fresh, unix_time())
}
fn render_verified(
    b: Bundle,
    review: InstallReview,
    accepted: &str,
    output: &Path,
    fresh: SiteEvidence,
    now: u64,
) -> Result<Value> {
    ensure!(
        review.protocol == 1
            && !review.grants_authority
            && !review.execution_enabled
            && review.ready_for_confirmation
            && review.blocked.is_empty(),
        "installation_review_not_confirmed"
    );
    ensure!(
        review.record.state == State::Prepared
            && review.record.plan.action == INSTALL_ACTION
            && now >= review.record.plan.created_at
            && now < review.record.plan.expires_at
            && review.record.plan_sha256 == accepted
            && digest(&review.record.plan)? == accepted,
        "installation_plan_digest_or_expiry_mismatch"
    );
    ensure!(
        review.destinations == destinations()
            && review.record.plan.device_id == b.policy.device_id
            && review.record.plan.policy_sha256
                == review_binding(
                    &review.manifest_sha256,
                    &review.site,
                    &review.approved,
                    &review.destinations
                )?,
        "installation_binding_mismatch"
    );
    let old = review.site.as_ref().context("site_evidence_required")?;
    ensure!(
        site_binding(old)? == site_binding(&fresh)?
            && review.record.plan.snapshot == fresh.snapshot,
        "installation_site_changed"
    );
    let approved = review.approved.as_ref().context("approved_scope_missing")?;
    ensure!(approved.allow_unattended, "unattended_scope_missing");
    let mut policy = fresh.policy;
    policy.enabled = true;
    let mut grant = fresh.grant;
    grant.enabled = true;
    grant.not_before = approved.grant.not_before;
    grant.expires_at = approved.grant.expires_at;
    ensure!(
        digest(&(policy.clone(), grant.clone()))?
            == digest(&(approved.policy.clone(), approved.grant.clone()))?,
        "approved_resources_changed"
    );
    grant.validate(&policy, now)?;
    fs::DirBuilder::new().mode(0o700).create(output)?;
    write_new(&output.join("policy.json"), &policy)?;
    write_new(&output.join("service-grant.json"), &grant)?;
    let files = BTreeMap::from([
        (
            "policy.json",
            sha(&bytes(&output.join("policy.json"), JSON_LIMIT)?),
        ),
        (
            "service-grant.json",
            sha(&bytes(&output.join("service-grant.json"), JSON_LIMIT)?),
        ),
    ]);
    let receipt = json!({"record":review.record,"rendered_files":files,"installed":false,"system_changes":false,
        "grants_authority":false,"next_action":"separate_os_administrator_installation_only","active_policy_sha256":digest(&(policy,grant))?});
    write_new(&output.join("render-receipt.json"), &receipt)?;
    Ok(receipt)
}
#[cfg(test)]
mod tests;
