use super::*;
use argon2::{PasswordHash, PasswordVerifier};
use std::os::unix::fs::{PermissionsExt, symlink};

fn fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.json");
    let bytes = br#"{
  "owner": "fixture-owner", "public_url": "https://gateway.example",
  "bind": "127.0.0.1:18787", "state_dir": "/fixture/state",
  "password_hash": "synthetic-old-hash",
  "devices": [{"id":"fixture-device","token_hash":"synthetic-device-hash","scopes":["code:read"]}],
  "redirect_uris": ["https://client.example/callback"],
  "future_field": {"number": 123456789012345678901234567890, "escaped": "\u0061"}
}
"#
    .to_vec();
    std::fs::write(&path, &bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (dir, path, bytes)
}

fn synthetic_hash() -> String {
    Argon2::default()
        .hash_password(
            b"synthetic-password-only",
            &SaltString::encode_b64(b"synthetic-fixture").unwrap(),
        )
        .unwrap()
        .to_string()
}

#[test]
fn only_hash_bytes_change_and_private_mode_owner_and_state_are_preserved() {
    let (dir, path, original) = fixture();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    let before = std::fs::metadata(&path).unwrap();
    let state = dir.path().join("state.sqlite");
    let setup = dir.path().join("setup-password.txt");
    std::fs::write(&state, b"synthetic OAuth and device state").unwrap();
    std::fs::write(&setup, b"synthetic setup password").unwrap();
    let pending = PendingReset::open(&path).unwrap();
    let span = pending.hash_span.clone();
    let hash = synthetic_hash();
    pending.commit(&hash).unwrap();
    let current = std::fs::read(&path).unwrap();
    let mut expected = original[..span.start].to_vec();
    expected.extend(serde_json::to_vec(&hash).unwrap());
    expected.extend(&original[span.end..]);
    assert_eq!(current, expected);
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(before.uid(), after.uid());
    assert_eq!(before.gid(), after.gid());
    assert_eq!(before.mode(), after.mode());
    assert_ne!(before.ino(), after.ino());
    assert!(
        Argon2::default()
            .verify_password(
                b"synthetic-password-only",
                &PasswordHash::new(&hash).unwrap()
            )
            .is_ok()
    );
    assert_eq!(
        std::fs::read(state).unwrap(),
        b"synthetic OAuth and device state"
    );
    assert_eq!(std::fs::read(setup).unwrap(), b"synthetic setup password");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[test]
fn dropping_a_prepared_reset_does_not_create_or_change_any_file() {
    let (dir, path, original) = fixture();
    drop(PendingReset::open(&path).unwrap());
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn concurrent_edit_replacement_or_mode_change_is_never_overwritten() {
    for change in ["contents", "replacement", "mode"] {
        let (dir, path, original) = fixture();
        let pending = PendingReset::open(&path).unwrap();
        match change {
            "contents" => std::fs::write(&path, b"concurrent administrator edit").unwrap(),
            "replacement" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::write(&path, &original).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            "mode" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
            }
            _ => unreachable!(),
        }
        let expected = std::fs::read(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().mode();
        let error = pending.commit("synthetic-replacement-hash").unwrap_err();
        assert!(error.to_string().contains("changed"));
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        assert_eq!(std::fs::metadata(&path).unwrap().mode(), mode);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn reset_lock_refuses_a_second_administrator() {
    let (_dir, path, _) = fixture();
    let first = PendingReset::open(&path).unwrap();
    assert!(PendingReset::open(&path).is_err());
    drop(first);
    assert!(PendingReset::open(&path).is_ok());
}

#[test]
fn symlinks_hard_links_special_files_and_shared_permissions_are_refused() {
    let (dir, path, original) = fixture();
    let link = dir.path().join("linked.json");
    symlink(&path, &link).unwrap();
    assert!(PendingReset::open(&link).is_err());
    std::fs::remove_file(&link).unwrap();
    std::fs::hard_link(&path, &link).unwrap();
    assert!(PendingReset::open(&path).is_err());
    std::fs::remove_file(&link).unwrap();
    let fifo = dir.path().join("config.fifo");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRUSR).unwrap();
    assert!(PendingReset::open(&fifo).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(PendingReset::open(&path).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(PendingReset::open(&path).is_err());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn changed_target_symlink_cannot_redirect_the_write() {
    let (dir, path, _) = fixture();
    let pending = PendingReset::open(&path).unwrap();
    let other = dir.path().join("other.json");
    std::fs::write(&other, b"other administrator data").unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&other, &path).unwrap();
    assert!(pending.commit("synthetic-replacement-hash").is_err());
    assert_eq!(std::fs::read(other).unwrap(), b"other administrator data");
    assert!(std::fs::symlink_metadata(path).unwrap().is_symlink());
}

#[test]
fn invalid_or_missing_gateway_is_not_created_and_errors_do_not_echo_contents() {
    let (dir, path, _) = fixture();
    for invalid in [
        r#"{"password_hash":"synthetic-sensitive-value"}"#,
        r#"{"password_hash":"synthetic-sensitive-value", broken json}"#,
    ] {
        std::fs::write(&path, invalid).unwrap();
        let error = PendingReset::open(&path).err().unwrap();
        assert!(!format!("{error:#}").contains("synthetic-sensitive-value"));
        assert_eq!(std::fs::read(&path).unwrap(), invalid.as_bytes());
    }
    std::fs::remove_file(&path).unwrap();
    assert!(PendingReset::open(&path).is_err());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}
