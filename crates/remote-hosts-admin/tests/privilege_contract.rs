//! CLI acceptance of the read-only contract; never installs a helper.
use std::process::Command;

fn cli(args: &[&str]) -> serde_json::Value {
    let result = Command::new(env!("CARGO_BIN_EXE_remote-hosts-admin"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}
#[test]
fn native_status_does_not_claim_elevation() {
    let value = cli(&["privilege", "status"]);
    assert_eq!(value["protocol"], 1);
    assert_eq!(value["execution_implemented"], false);
    assert_eq!(value["execution_available"], false);
    assert_eq!(value["arbitrary_root_commands"], false);
    assert_eq!(value["grant"], "not_checked");
}
#[test]
fn preview_retains_original_handle_and_does_not_assume_unknown_was_not_applied() {
    let hash = "a".repeat(64);
    let id = "d84bfb53-1fc4-4075-940f-557db1d6cb08";
    let value = cli(&[
        "privilege",
        "plan",
        "--request-id",
        id,
        "--device-id",
        "933a488a-152e-44d3-8409-4b91733986c3",
        "--action",
        "exact-source-dns-repair",
        "--expected-policy-sha256",
        &hash,
        "--expected-target-sha256",
        &hash,
    ]);
    assert_eq!(value["plan"]["request"]["request_id"], id);
    assert_eq!(value["plan"]["execution_enabled"], false);
    assert_eq!(value["plan"]["grants_authority"], false);
    assert_eq!(value["plan"]["next_action"], "observe_original");
    assert_eq!(value["original_receipt_observed"], false);
    assert_eq!(value["plan_sha256"].as_str().unwrap().len(), 64);
}
#[test]
fn privileged_apply_is_not_a_supported_command() {
    let result = Command::new(env!("CARGO_BIN_EXE_remote-hosts-admin"))
        .args(["privilege", "apply"])
        .output()
        .unwrap();
    assert!(!result.status.success());
}
