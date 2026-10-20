//! Offline administrator closure of the ORIGINAL uncertain Record. No execution,
//! replay, grant reset, service mutation, or assertion that restart did/didn't occur.
use crate::{
    engine::unix_time,
    filesystem::{Store, private_read, trusted_dir},
    managed_service::{self, LinuxServiceDriver, ServiceDriver},
    protocol::{Policy, Record, Snapshot, State, digest, valid_id},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const BASE: &str = "/var/lib/octendra-admin";
fn validate_record(policy: &Policy, record: &Record, device: &str, id: &str) -> Result<()> {
    valid_id(device)?;
    valid_id(id)?;
    policy.validate()?;
    ensure!(
        policy.device_id == device
            && record.plan.device_id == device
            && record.plan.request_id == id
            && record.plan.caller_uid == policy.allowed_uid
            && record.plan.action == managed_service::ACTION
            && record.plan.protocol == 1
            && record.plan_sha256 == digest(&record.plan)?,
        "settlement_original_binding_mismatch"
    );
    Ok(())
}
fn observation_sha(snapshot: &Snapshot, boot: &str) -> Result<String> {
    valid_id(boot)?;
    ensure!(
        snapshot.current_service_active
            && snapshot.units.len() == 1
            && snapshot.units[0].name == managed_service::SERVICE,
        "settlement_service_not_verified"
    );
    digest(&(snapshot, boot))
}
#[allow(clippy::too_many_arguments)]
fn close_verified(
    store: &Store,
    policy: &Policy,
    device: &str,
    id: &str,
    expected_record: &str,
    expected_observation: &str,
    snapshot: Snapshot,
    boot: &str,
    now: u64,
) -> Result<Record> {
    ensure!(store.revoked()?, "settlement_requires_persisted_revocation");
    let mut record = store
        .load(id)?
        .context("settlement_original_receipt_missing")?;
    validate_record(policy, &record, device, id)?;
    if record.state == State::ClosedUnknown {
        return Ok(record);
    }
    ensure!(
        record.state == State::Running,
        "only_original_running_unknown_can_be_closed"
    );
    ensure!(
        digest(&record)? == expected_record,
        "settlement_original_record_changed"
    );
    let observation = observation_sha(&snapshot, boot)?;
    ensure!(
        observation == expected_observation,
        "settlement_observation_changed"
    );
    ensure!(
        record.plan.snapshot.files == snapshot.files,
        "settlement_original_resources_changed"
    );
    ensure!(now >= record.updated_at, "settlement_clock_moved_backwards");
    record.steps.push(format!(
        "administrator_closed_unknown_original_sha256={expected_record}"
    ));
    record
        .steps
        .push(format!("administrator_observed_sha256={observation}"));
    record
        .steps
        .push("execution_outcome_unproven_no_replay_no_grant_reset".into());
    record.after = Some(snapshot);
    record.state = State::ClosedUnknown;
    record.updated_at = now;
    // Original plan, request UUID, previous error and steps remain intact.
    store.save(&record)?;
    Ok(record)
}
pub async fn run(device: &str, id: &str, accepted: Option<(&str, &str)>) -> Result<Value> {
    ensure!(
        cfg!(target_os = "linux") && nix::unistd::geteuid().is_root(),
        "settlement_requires_separate_linux_root_session"
    );
    valid_id(device)?;
    valid_id(id)?;
    let base = Path::new(BASE);
    let state = base.join("receipts");
    trusted_dir(base, 0)?;
    trusted_dir(&state, 0)?;
    let policy: Policy = serde_json::from_slice(&private_read(&base.join("policy.json"), 0)?)?;
    let store = Store {
        dir: state.clone(),
        owner: 0,
    };
    ensure!(store.revoked()?, "settlement_requires_persisted_revocation");
    // Same lifetime lock as the server. Never stop it, signal it, or bypass contention.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(state.join("helper.lock"))?;
    let m = lock.metadata()?;
    ensure!(
        m.is_file() && m.nlink() == 1 && m.uid() == 0 && m.mode() & 0o077 == 0,
        "untrusted_original_helper_lock"
    );
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("helper_active_stop_separately_before_settlement")?;
    let record = store
        .load(id)?
        .context("settlement_original_receipt_missing")?;
    validate_record(&policy, &record, device, id)?;
    if record.state == State::ClosedUnknown {
        return Ok(
            json!({"record":record,"administratively_closed":true,"execution_outcome":"unknown",
            "replay_allowed":false,"grant_revoked":true,"system_changes":false}),
        );
    }
    ensure!(
        record.state == State::Running,
        "only_original_running_unknown_can_be_closed"
    );
    let driver = LinuxServiceDriver::new(base.join("service-grant.json"));
    let grant = driver.grant()?;
    // Expiry/revocation must not prevent reviewing original records. Bind original
    // grant bytes; never alter the grant's time window or enabled field on disk.
    ensure!(
        digest(&(policy.clone(), grant.clone()))? == record.plan.policy_sha256,
        "settlement_original_policy_or_grant_changed"
    );
    let snapshot = driver.inspect(&policy, &grant).await?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    let observation = observation_sha(&snapshot, &boot)?;
    ensure!(
        snapshot.files == record.plan.snapshot.files,
        "settlement_original_resources_changed"
    );
    let original = digest(&record)?;
    if let Some((expected_record, expected_observation)) = accepted {
        let closed = close_verified(
            &store,
            &policy,
            device,
            id,
            expected_record,
            expected_observation,
            snapshot,
            &boot,
            unix_time(),
        )?;
        return Ok(
            json!({"record":closed,"administratively_closed":true,"execution_outcome":"unknown",
            "replay_allowed":false,"grant_revoked":true,"system_changes":false}),
        );
    }
    Ok(
        json!({"record":record,"record_sha256":original,"observation":snapshot,
        "observation_sha256":observation,"boot_id":boot,"administratively_closed":false,
        "execution_outcome":"unknown","replay_allowed":false,"grant_revoked":true,"system_changes":false,
        "next_action":"review_original_and_current_state_then_explicitly_acknowledge_outcome_unproven"}),
    )
}
#[cfg(test)]
mod tests;
