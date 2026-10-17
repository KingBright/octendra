#![cfg(unix)]
use std::process::Command;
fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_remote-hosts-admin"))
}
#[test]
fn actual_binary_exposes_native_entry_chain() {
    let out = binary().args(["linux-helper", "--help"]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    for command in [
        "serve",
        "status",
        "plan",
        "apply",
        "receipt",
        "revoke",
        "verify-package",
        "prepare-install",
        "inspect-site",
        "render-approved",
    ] {
        assert!(text.contains(command), "{command}");
    }
}
#[test]
fn arbitrary_shell_and_resource_override_are_not_commands() {
    for args in [
        vec!["linux-helper", "shell", "-c", "id"],
        vec!["linux-helper", "apply", "--service", "sing-box"],
    ] {
        assert!(!binary().args(args).output().unwrap().status.success());
    }
}
#[test]
fn server_requires_real_linux_root_without_touching_paths() {
    if !cfg!(target_os = "linux") || !nix::unistd::geteuid().is_root() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("missing");
        let out = binary()
            .args(["linux-helper", "serve", "--policy"])
            .arg(&p)
            .arg("--state-dir")
            .arg(&p)
            .arg("--grant")
            .arg(&p)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(!p.exists());
    }
}
