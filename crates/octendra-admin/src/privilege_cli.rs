//! Read-only CLI surface shared by every platform. Never installs or elevates.
use crate::privilege::{self, Action, NativeBackend, PlanRequest, PrivilegeBackend};
use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::ffi::OsString;

#[derive(Parser)]
#[command(
    name = "octendra-admin privilege",
    about = "Read-only administrator capability discovery and descriptive plans"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Status,
    /// Descriptive metadata only. Original execution outcome is unobserved here.
    Plan {
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        device_id: String,
        #[arg(long, value_enum)]
        action: Action,
        #[arg(long)]
        expected_policy_sha256: String,
        #[arg(long)]
        expected_target_sha256: String,
    },
}
fn run(cli: Cli) -> Result<Value> {
    let observed = NativeBackend.discover();
    match cli.command {
        Command::Status => Ok(serde_json::to_value(observed)?),
        Command::Plan {
            request_id,
            device_id,
            action,
            expected_policy_sha256,
            expected_target_sha256,
        } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs();
            // This local preview has no authenticated original receipt; never infer "not applied".
            let plan = privilege::plan(
                PlanRequest {
                    request_id,
                    device_id,
                    action,
                    expected_policy_sha256,
                    expected_target_sha256,
                },
                observed,
                None,
                true,
                now,
            )?;
            Ok(json!({"plan_sha256":plan.sha256()?,"plan":plan,"original_receipt_observed":false}))
        }
    }
}
/// Returns false for legacy commands, preserving the existing Unix CLI unchanged.
pub fn try_run() -> Result<bool> {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_none_or(|value| value != "privilege") {
        return Ok(false);
    }
    let cli = Cli::parse_from(std::iter::once(args[0].clone()).chain(args.into_iter().skip(2)));
    println!("{}", serde_json::to_string_pretty(&run(cli)?)?);
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_execute_or_install_command_exists() {
        for name in ["execute", "apply", "install", "grant", "revoke", "sudo"] {
            assert!(Cli::try_parse_from(["privilege", name]).is_err());
        }
        assert!(Cli::try_parse_from(["privilege", "status"]).is_ok());
    }
    #[test]
    fn status_reports_disabled_execution() {
        let value = run(Cli::try_parse_from(["privilege", "status"]).unwrap()).unwrap();
        assert_eq!(value["execution_available"], false);
        assert_eq!(value["arbitrary_root_commands"], false);
    }
}
