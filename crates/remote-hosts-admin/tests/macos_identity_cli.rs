#![cfg(unix)]
use std::process::Command;
#[test]
fn actual_binary_exposes_read_only_identity_entry() {
    let out = Command::new(env!("CARGO_BIN_EXE_remote-hosts-admin"))
        .args(["macos-helper", "verify-identity", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("--team-id") && text.contains("--expected-sha256"));
    assert!(!text.contains("--install") && !text.contains("--service"));
}
#[test]
fn arbitrary_helper_path_or_activation_is_not_accepted() {
    for flag in ["--binary", "--apply", "--install", "--service"] {
        let out = Command::new(env!("CARGO_BIN_EXE_remote-hosts-admin"))
            .args(["macos-helper", "verify-identity", flag])
            .output()
            .unwrap();
        assert!(!out.status.success());
    }
}
#[test]
fn invalid_identity_is_rejected_before_filesystem_or_signature_query() {
    let out = Command::new(env!("CARGO_BIN_EXE_remote-hosts-admin"))
        .args([
            "macos-helper",
            "verify-identity",
            "--team-id",
            "untrusted\" or true",
            "--expected-sha256",
            &"a".repeat(64),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid_apple_team_identifier"));
}
