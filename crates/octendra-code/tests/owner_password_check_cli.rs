//! Artificial files only: never reads a deployed configuration or credential.
#![cfg(unix)]
use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

const PASSWORD: &[u8] = "fixture-only-password-测试".as_bytes();
fn hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let salt = SaltString::encode_b64(b"fixture-only-salt").unwrap();
        Argon2::default()
            .hash_password(PASSWORD, &salt)
            .unwrap()
            .to_string()
    })
}
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    password: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // macOS /var is a symlink; canonicalize only our artificial fixture root.
        let root = dir.path().canonicalize().unwrap();
        let config = root.join("gateway-fixture.json");
        let password = root.join("private-fixture.password");
        private(
            &config,
            serde_json::to_vec(&serde_json::json!({"password_hash":hash(),"other":"fixture-only"}))
                .unwrap(),
        );
        private(&password, PASSWORD);
        Self {
            _dir: dir,
            root,
            config,
            password,
        }
    }
    fn check(&self, token: &str, exit: i32) {
        check(&self.config, &self.password, token, exit);
    }
}
fn private(path: &Path, bytes: impl AsRef<[u8]>) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn check(config: &Path, password: &Path, token: &str, exit: i32) {
    let result = Command::new(env!("CARGO_BIN_EXE_octendra-code"))
        .args(["check-owner-password", "--config"])
        .arg(config)
        .arg("--password-file")
        .arg(password)
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(exit));
    assert_eq!(result.stdout, format!("{token}\n").as_bytes());
    assert!(result.stderr.is_empty(), "no diagnostic output");
}

#[test]
fn match_and_mismatch_preserve_exact_file_bytes_and_metadata() {
    let f = Fixture::new();
    let before: Vec<_> = [&f.config, &f.password]
        .map(|p| {
            let m = fs::metadata(p).unwrap();
            (
                fs::read(p).unwrap(),
                m.mode(),
                m.uid(),
                m.gid(),
                m.ino(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            )
        })
        .into();
    f.check("match", 0);
    for (i, p) in [&f.config, &f.password].into_iter().enumerate() {
        let m = fs::metadata(p).unwrap();
        assert_eq!(
            before[i],
            (
                fs::read(p).unwrap(),
                m.mode(),
                m.uid(),
                m.gid(),
                m.ino(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec()
            )
        );
    }
    private(&f.password, b"fixture-other-password");
    f.check("mismatch", 1);
    private(&f.password, [PASSWORD, b"\n"].concat());
    f.check("mismatch", 1); // No trimming or transformation of password bytes.
}

#[test]
fn readonly_private_files_are_supported() {
    let f = Fixture::new();
    for p in [&f.config, &f.password] {
        fs::set_permissions(p, fs::Permissions::from_mode(0o400)).unwrap();
    }
    f.check("match", 0);
}

#[test]
fn rejects_unsafe_permissions_on_either_file() {
    for config in [true, false] {
        let f = Fixture::new();
        let path = if config { &f.config } else { &f.password };
        for mode in [0o644, 0o640, 0o660, 0o700, 0o4600] {
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
            f.check("error", 2);
        }
    }
}

#[test]
fn rejects_symlink_files_parents_and_hardlinks() {
    for config in [true, false] {
        let f = Fixture::new();
        let target = if config { &f.config } else { &f.password };
        let alias = f.root.join("fixture-alias");
        symlink(target, &alias).unwrap();
        if config {
            check(&alias, &f.password, "error", 2);
        } else {
            check(&f.config, &alias, "error", 2);
        }
        fs::remove_file(&alias).unwrap();
        fs::hard_link(target, &alias).unwrap();
        f.check("error", 2);
    }
    let f = Fixture::new();
    let alias = f.root.join("parent-alias");
    symlink(&f.root, &alias).unwrap();
    check(&alias.join("gateway-fixture.json"), &f.password, "error", 2);
    check(
        &f.config,
        &alias.join("private-fixture.password"),
        "error",
        2,
    );
}

#[test]
fn rejects_missing_special_empty_and_oversized_inputs() {
    for config in [true, false] {
        let f = Fixture::new();
        let target = if config { &f.config } else { &f.password };
        fs::remove_file(target).unwrap();
        f.check("error", 2);
        fs::create_dir(target).unwrap();
        f.check("error", 2);
        fs::remove_dir(target).unwrap();
        nix::unistd::mkfifo(
            target,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        f.check("error", 2); // O_NONBLOCK avoids waiting for a FIFO writer.
        fs::remove_file(target).unwrap();
        private(target, b"");
        f.check("error", 2);
        private(
            target,
            vec![b'x'; if config { 1024 * 1024 + 1 } else { 4097 }],
        );
        f.check("error", 2);
    }
}

#[test]
fn malformed_secret_bearing_inputs_and_expensive_hashes_only_print_error() {
    let f = Fixture::new();
    for value in [
        "fixture-secret-invalid-json".to_owned(),
        "{\"password_hash\":\"fixture-secret-invalid-hash\"}".to_owned(),
        serde_json::json!({"password_hash":hash().replace("m=19456", "m=999999999")}).to_string(),
        format!(
            "{{\"password_hash\":\"{}\",\"password_hash\":\"fixture-secret\"}}",
            hash()
        ),
    ] {
        private(&f.config, value);
        f.check("error", 2);
    }
}

#[test]
fn parser_errors_never_echo_arguments_or_help() {
    for args in [
        vec!["check-owner-password"],
        vec!["check-owner-password", "--help"],
        vec![
            "check-owner-password",
            "--password",
            "fixture-secret-argument",
        ],
        vec!["check-owner-password", "--config", "fixture-secret-path"],
    ] {
        let result = Command::new(env!("CARGO_BIN_EXE_octendra-code"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert_eq!(result.stdout, b"error\n");
        assert!(result.stderr.is_empty());
    }
}
