//! Native entry chain for one Linux helper profile. Never invokes sudo or a shell.
use crate::{
    installation,
    managed_service::{self, RecordClient},
    privilege::{
        self, Action, Capability, Evidence, Grant, Helper, Identity, PlanRequest, Platform, Reason,
    },
    protocol::{Record, Request, State, digest, valid_id},
};
use anyhow::{Context, Result, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Subcommand)]
pub enum Command {
    /// Verify a static, disabled bundle as data; allowed on preparation hosts.
    VerifyPackage {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        manifest_sha256: String,
    },
    /// Produce a shared Record review. No installation or system mutation.
    PrepareInstall {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        manifest_sha256: String,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        site: Option<PathBuf>,
        #[arg(long)]
        not_before: Option<u64>,
        #[arg(long)]
        expires_at: Option<u64>,
        #[arg(long)]
        allow_unattended: bool,
        #[arg(long)]
        output: PathBuf,
    },
    /// Root-only read-only site inspection from a separately verified native executable.
    InspectSite {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        manifest_sha256: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Render approved data files, never copy them to system paths or start services.
    RenderApproved {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        accept_plan_sha256: String,
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Root-only fixed-profile server entry. Policies remain disabled until separately approved.
    Serve {
        #[arg(long)]
        policy: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        grant: PathBuf,
    },
    Status {
        #[arg(long)]
        device_id: String,
    },
    Plan {
        #[arg(long)]
        device_id: String,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        expected_policy_sha256: Option<String>,
        #[arg(long)]
        output: PathBuf,
    },
    Apply {
        #[arg(long)]
        device_id: String,
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        accept_plan_sha256: String,
        #[arg(long)]
        outcome_unknown: bool,
    },
    Receipt {
        #[arg(long)]
        device_id: String,
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    Revoke {
        #[arg(long)]
        device_id: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedPlan {
    pub protocol: u32,
    pub binding: PlanRequest,
    pub record: Record,
}
impl SavedPlan {
    fn validate(&self, device: &str, uid: u32) -> Result<()> {
        ensure!(
            self.protocol == 1
                && self.binding.device_id == device
                && self.record.plan.caller_uid == uid,
            "saved_plan_identity_mismatch"
        );
        managed_service::validate_operation_binding(&self.binding, &self.record)
    }
}
struct Socket<'a>(&'a Path);
impl RecordClient for Socket<'_> {
    async fn request(&self, request: &Request) -> Result<Value> {
        crate::transport::request(self.0, request).await
    }
}
fn result(value: Value) -> Result<Value> {
    ensure!(value["ok"] == true, "helper_rejected_observe_original");
    Ok(value
        .get("result")
        .context("helper_result_missing")?
        .clone())
}
async fn handshake<C: RecordClient>(client: &C, device: &str, uid: u32) -> Result<Value> {
    valid_id(device)?;
    let status = result(client.request(&Request::Status).await?)?;
    ensure!(
        status["protocol"] == 1
            && status["available"] == true
            && status["device_id"] == device
            && status["caller_uid"] == uid
            && status["allowed_action"] == managed_service::ACTION
            && status["arbitrary_commands"] == false
            && status["enabled"].is_boolean(),
        "helper_identity_or_profile_mismatch"
    );
    Ok(status)
}
fn capability(status: &Value) -> Capability {
    let granted = status["enabled"] == true
        && status["policy_sha256"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
    let mut cap = privilege::discover(&Evidence {
        platform: Platform::Linux,
        identity: Identity::OrdinaryUser,
        no_new_privileges: std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| privilege::parse_no_new_privileges(&s)),
        helper: Helper::Verified,
        grant: if status["revoked"] == true {
            Grant::Revoked
        } else if granted {
            Grant::Verified
        } else {
            Grant::NotGranted
        },
    });
    cap.reasons.retain(|r| *r != Reason::BackendNotImplemented);
    cap.backend = "linux_local_fixed_service_helper".into();
    cap.execution_implemented = true;
    cap.execution_available = granted && status["revoked"] != true;
    cap
}
fn checked_record(value: Value, device: &str, id: &str, uid: u32) -> Result<Record> {
    let record: Record = serde_json::from_value(result(value)?)?;
    ensure!(
        record.plan.device_id == device
            && record.plan.request_id == id
            && record.plan.caller_uid == uid
            && record.plan.action == managed_service::ACTION
            && record.plan_sha256 == digest(&record.plan)?,
        "helper_record_binding_mismatch"
    );
    Ok(record)
}
fn saved(record: Record) -> Result<SavedPlan> {
    Ok(SavedPlan {
        protocol: 1,
        binding: PlanRequest {
            request_id: record.plan.request_id.clone(),
            device_id: record.plan.device_id.clone(),
            action: Action::ManagedServiceRestart,
            expected_policy_sha256: record.plan.policy_sha256.clone(),
            expected_target_sha256: digest(&record.plan.snapshot)?,
        },
        record,
    })
}
async fn plan_using<C: RecordClient>(
    client: &C,
    device: &str,
    id: &str,
    uid: u32,
    expected: Option<&str>,
) -> Result<SavedPlan> {
    valid_id(id)?;
    let status = handshake(client, device, uid).await?;
    ensure!(
        capability(&status).execution_available,
        "managed_service_grant_unavailable"
    );
    let policy = status["policy_sha256"]
        .as_str()
        .context("helper_policy_binding_missing")?;
    if let Some(expected) = expected {
        ensure!(expected == policy, "expected_policy_mismatch");
    }
    let record = checked_record(
        client
            .request(&Request::Plan {
                request_id: id.into(),
            })
            .await?,
        device,
        id,
        uid,
    )?;
    ensure!(
        record.plan.policy_sha256 == policy,
        "helper_policy_changed_during_plan"
    );
    saved(record)
}
async fn apply_using<C: RecordClient>(
    client: &C,
    device: &str,
    uid: u32,
    plan: &SavedPlan,
    accepted: &str,
    unknown: bool,
) -> Result<Record> {
    plan.validate(device, uid)?;
    ensure!(
        accepted == plan.record.plan_sha256,
        "explicit_plan_digest_required"
    );
    ensure!(
        capability(&handshake(client, device, uid).await?).execution_available,
        "managed_service_grant_unavailable_read_receipt"
    );
    managed_service::apply_bound_using(client, &plan.binding, &plan.record, unknown).await
}
pub async fn run(command: Command, socket: &Path) -> Result<Value> {
    match command {
        Command::VerifyPackage {
            bundle,
            manifest_sha256,
        } => return installation::verify_report(&bundle, &manifest_sha256),
        Command::PrepareInstall {
            bundle,
            manifest_sha256,
            request_id,
            site,
            not_before,
            expires_at,
            allow_unattended,
            output,
        } => {
            let review = installation::prepare(
                &bundle,
                &manifest_sha256,
                &request_id,
                site.as_deref(),
                not_before,
                expires_at,
                allow_unattended,
            )
            .await?;
            installation::write_new(&output, &review)?;
            return Ok(
                json!({"record":review.record,"ready_for_confirmation":review.ready_for_confirmation,"grants_authority":false,"output":output}),
            );
        }
        Command::InspectSite {
            bundle,
            manifest_sha256,
            output,
        } => {
            let site = installation::inspect_site(&bundle, &manifest_sha256).await?;
            installation::write_new(&output, &site)?;
            return Ok(
                json!({"site":site,"output":output,"system_changes":false,"grants_authority":false}),
            );
        }
        Command::RenderApproved {
            plan,
            accept_plan_sha256,
            bundle,
            output,
        } => return installation::render(&bundle, &plan, &accept_plan_sha256, &output).await,
        Command::Serve {
            policy,
            state_dir,
            grant,
        } => {
            #[cfg(target_os = "linux")]
            {
                crate::transport::serve_managed_service(&policy, &state_dir, socket, &grant)
                    .await?;
                return Ok(json!({"server_stopped":true}));
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = (policy, state_dir, grant);
                anyhow::bail!("linux_native_server_required_no_platform_fallback");
            }
        }
        other => {
            ensure!(
                cfg!(target_os = "linux"),
                "linux_native_client_required_no_platform_fallback"
            );
            run_client(other, &Socket(socket), nix::unistd::geteuid().as_raw()).await
        }
    }
}
async fn run_client<C: RecordClient>(command: Command, client: &C, uid: u32) -> Result<Value> {
    match command {
        Command::Status { device_id } => {
            let status = handshake(client, &device_id, uid).await?;
            Ok(
                json!({"capability":capability(&status),"device_id":device_id,"action":managed_service::ACTION,"policy_sha256":status["policy_sha256"],"peer":"kernel_verified_root"}),
            )
        }
        Command::Plan {
            device_id,
            request_id,
            expected_policy_sha256,
            output,
        } => {
            let plan = plan_using(
                client,
                &device_id,
                &request_id,
                uid,
                expected_policy_sha256.as_deref(),
            )
            .await?;
            installation::write_new(&output, &plan)?;
            Ok(
                json!({"record":plan.record,"binding":plan.binding,"output":output,"system_changes":false}),
            )
        }
        Command::Apply {
            device_id,
            plan,
            accept_plan_sha256,
            outcome_unknown,
        } => {
            let plan: SavedPlan = installation::read_json(&plan)?;
            let record = apply_using(
                client,
                &device_id,
                uid,
                &plan,
                &accept_plan_sha256,
                outcome_unknown,
            )
            .await?;
            let execution = match record.state {
                State::Succeeded => "succeeded",
                State::Running => "unknown",
                State::Failed => "failed",
                State::Prepared => "not_executed",
            };
            Ok(
                json!({"execution_state":execution,"record":record,"next_action":if execution=="succeeded" {"read_original_receipt"} else {"observe_original_no_replay"}}),
            )
        }
        Command::Receipt {
            device_id,
            request_id,
            output,
        } => {
            valid_id(&request_id)?;
            handshake(client, &device_id, uid).await?;
            let record = checked_record(
                client
                    .request(&Request::Receipt {
                        request_id: request_id.clone(),
                    })
                    .await?,
                &device_id,
                &request_id,
                uid,
            )?;
            if let Some(path) = output {
                installation::write_new(&path, &saved(record.clone())?)?;
            }
            Ok(
                json!({"record":record,"system_changes":false,"next_action":"read_original_receipt"}),
            )
        }
        Command::Revoke { device_id } => {
            handshake(client, &device_id, uid).await?;
            let value = result(client.request(&Request::Revoke).await?)?;
            ensure!(
                value["revoked"] == true && value["enabled"] == false,
                "revocation_receipt_invalid_observe_original"
            );
            Ok(value)
        }
        _ => anyhow::bail!("not_a_helper_client_command"),
    }
}
#[cfg(test)]
mod tests;
