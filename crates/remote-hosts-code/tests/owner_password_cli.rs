//! Synthetic PTY acceptance only. Never reads a real gateway or real password.
#![cfg(unix)]

use argon2::{Argon2, PasswordHash, PasswordVerifier};
use portable_pty::{Child, CommandBuilder, PtyPair, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

const PASSWORD: &str = "synthetic-PTY-password-测试";

fn fixture() -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.json");
    let original = serde_json::to_vec_pretty(&serde_json::json!({
        "owner":"fixture-owner", "public_url":"https://gateway.example",
        "bind":"127.0.0.1:18787", "state_dir":dir.path().join("unused-state"),
        "password_hash":"synthetic-old-hash", "devices":[], "redirect_uris":[],
        "future_field":{"must_remain":true}
    }))
    .unwrap();
    std::fs::write(&path, &original).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    (dir, path, original)
}

struct Session {
    _pair: PtyPair,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    receiver: Receiver<Vec<u8>>,
    output: Vec<u8>,
}

impl Session {
    fn start(path: &Path) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_remote-hosts-code"));
        command.arg("reset-owner-password");
        command.arg("--config");
        command.arg(path);
        let child = pair.slave.spawn_command(command).unwrap();
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        Self {
            _pair: pair,
            child,
            writer,
            receiver,
            output: Vec::new(),
        }
    }

    fn until(&mut self, marker: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !String::from_utf8_lossy(&self.output).contains(marker) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let chunk = self
                .receiver
                .recv_timeout(remaining)
                .unwrap_or_else(|error| {
                    let terminal = String::from_utf8_lossy(&self.output)
                        .replace(PASSWORD, "[synthetic password]")
                        .replace("synthetic-different-password", "[synthetic password]");
                    panic!("PTY progress: {error}; terminal output: {terminal}");
                });
            self.output.extend(chunk);
            assert!(self.output.len() < 32 * 1024, "bounded PTY output");
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn passwords(&mut self, second: &str) {
        self.until("New owner password: ");
        self.send(format!("{PASSWORD}\n").as_bytes());
        self.until("Repeat new owner password: ");
        self.send(format!("{second}\n").as_bytes());
    }

    fn finish(&mut self) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            while let Ok(chunk) = self.receiver.try_recv() {
                self.output.extend(chunk);
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                // Drain final terminal output without waiting indefinitely for a
                // reader whose PTY slave is still held by the fixture.
                while let Ok(chunk) = self.receiver.recv_timeout(Duration::from_millis(30)) {
                    self.output.extend(chunk);
                }
                assert!(!String::from_utf8_lossy(&self.output).contains(PASSWORD));
                return status.exit_code();
            }
            assert!(Instant::now() < deadline, "PTY process completion");
            if let Ok(chunk) = self.receiver.recv_timeout(Duration::from_millis(20)) {
                self.output.extend(chunk);
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn terminal_confirmation_replaces_only_the_hash_without_echoing_passwords() {
    let (dir, path, original) = fixture();
    let mut session = Session::start(&path);
    session.passwords(PASSWORD);
    session.until("Type RESET to commit");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    session.send(b"RESET\n");
    assert_eq!(session.finish(), 0);
    let mut current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let hash = current["password_hash"].as_str().unwrap();
    assert!(
        Argon2::default()
            .verify_password(PASSWORD.as_bytes(), &PasswordHash::new(hash).unwrap())
            .is_ok()
    );
    current["password_hash"] = "synthetic-old-hash".into();
    assert_eq!(
        current,
        serde_json::from_slice::<serde_json::Value>(&original).unwrap()
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn mismatched_password_cancel_and_control_c_leave_every_file_unchanged() {
    for case in ["mismatch", "cancel", "interrupt"] {
        let (dir, path, original) = fixture();
        let mut session = Session::start(&path);
        if case == "interrupt" {
            session.until("New owner password: ");
            session.send(b"\x03");
        } else {
            session.passwords(if case == "mismatch" {
                "synthetic-different-password"
            } else {
                PASSWORD
            });
            if case == "cancel" {
                session.until("Type RESET to commit");
                session.send(b"\n");
            }
        }
        let exit = session.finish();
        assert_eq!(exit == 0, case == "cancel");
        assert!(!String::from_utf8_lossy(&session.output).contains("synthetic-different-password"));
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

#[test]
fn concurrent_edit_during_terminal_prompt_is_preserved() {
    let (dir, path, _) = fixture();
    let mut session = Session::start(&path);
    session.passwords(PASSWORD);
    session.until("Type RESET to commit");
    std::fs::write(&path, b"synthetic concurrent administrator edit").unwrap();
    session.send(b"RESET\n");
    assert_ne!(session.finish(), 0);
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"synthetic concurrent administrator edit"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn pipe_input_and_password_flags_are_refused_without_touching_config() {
    let (dir, path, original) = fixture();
    let password_flag = format!("--password={PASSWORD}");
    for extra in [
        vec![],
        vec!["--password", PASSWORD],
        vec![password_flag.as_str()],
        vec![PASSWORD],
        vec!["--yes"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_remote-hosts-code"))
            .arg("reset-owner-password")
            .arg("--config")
            .arg(&path)
            .args(extra)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains(PASSWORD));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(PASSWORD));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}
