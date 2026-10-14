//! Device-local sudo credentials. Password bytes only reach the real sudo validation process.
use crate::AgentConfig;

#[derive(Debug)]
pub(crate) struct Failure {
    pub code: &'static str,
    pub exit_code: Option<i32>,
}
impl Failure {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            exit_code: None,
        }
    }
}

pub(crate) async fn authorize(config: &AgentConfig) -> Result<(), Failure> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let program = std::path::Path::new("/usr/bin/sudo");
        let metadata = std::fs::metadata(program).map_err(|_| Failure::new("sudo_unavailable"))?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Failure::new("sudo_executable_untrusted"));
        }
        unix::authorize_with(program, || unix::credential(&config.device_id)).await
    }
    #[cfg(not(unix))]
    {
        let _ = config;
        Err(Failure::new("sudo_platform_unsupported"))
    }
}

#[cfg(unix)]
mod unix {
    use super::Failure;
    use std::{
        future::Future,
        path::Path,
        process::{ExitStatus, Stdio},
        time::Duration,
    };
    use tokio::{io::AsyncWriteExt, process::Command};
    use zeroize::Zeroizing;

    const SERVICE: &str = "fun.hackerlife.remote-hosts.sudo";
    const LIMIT: usize = 4096;

    fn account(device: &str) -> Result<String, Failure> {
        let device = uuid::Uuid::parse_str(device)
            .map_err(|_| Failure::new("sudo_device_identity_invalid"))?;
        Ok(format!("{}:{}", device, nix::unistd::geteuid().as_raw()))
    }

    fn validate_secret(secret: Zeroizing<Vec<u8>>) -> Result<Zeroizing<Vec<u8>>, Failure> {
        if secret.is_empty()
            || secret.len() > LIMIT
            || secret.iter().any(|b| matches!(b, 0 | b'\r' | b'\n'))
        {
            return Err(Failure::new("sudo_credential_invalid"));
        }
        Ok(secret)
    }

    pub(super) async fn credential(device: &str) -> Result<Zeroizing<Vec<u8>>, Failure> {
        let account = account(device)?;
        #[cfg(target_os = "macos")]
        {
            // The credential must already have been registered by the user in
            // this OS account's Keychain for this exact device. No vault export.
            let request = tokio::task::spawn_blocking(move || {
                security_framework::passwords::get_generic_password(SERVICE, &account)
                    .map(Zeroizing::new)
                    .map_err(|_| Failure::new("sudo_credential_unavailable"))
            });
            let secret = tokio::time::timeout(Duration::from_secs(20), request)
                .await
                .map_err(|_| Failure::new("sudo_credential_lookup_timed_out"))?
                .map_err(|_| Failure::new("sudo_credential_unavailable"))??;
            validate_secret(secret)
        }
        #[cfg(target_os = "linux")]
        {
            // Secret Service belongs to the Agent's current OS login session.
            // Only lookup is supported here; no tool can register or export a secret.
            let mut command = Command::new("/usr/bin/secret-tool");
            command
                .args(["lookup", "service", SERVICE, "account", &account])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            use tokio::io::AsyncReadExt;
            command.stdout(Stdio::piped());
            let mut child = command
                .spawn()
                .map_err(|_| Failure::new("sudo_credential_unavailable"))?;
            let mut output = child
                .stdout
                .take()
                .ok_or_else(|| Failure::new("sudo_credential_unavailable"))?
                .take((LIMIT + 2) as u64);
            let mut secret = Zeroizing::new(Vec::new());
            let lookup = async {
                output
                    .read_to_end(&mut secret)
                    .await
                    .map_err(|_| Failure::new("sudo_credential_unavailable"))?;
                if secret.len() > LIMIT + 1 {
                    return Err(Failure::new("sudo_credential_invalid"));
                }
                let status = child
                    .wait()
                    .await
                    .map_err(|_| Failure::new("sudo_credential_unavailable"))?;
                if !status.success() {
                    return Err(Failure::new("sudo_credential_unavailable"));
                }
                Ok(())
            };
            tokio::time::timeout(Duration::from_secs(20), lookup)
                .await
                .map_err(|_| Failure::new("sudo_credential_lookup_timed_out"))??;
            if secret.last() == Some(&b'\n') {
                secret.pop();
            }
            validate_secret(secret)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = account;
            Err(Failure::new("sudo_credential_backend_unsupported"))
        }
    }

    async fn validate(program: &Path, password: Option<&[u8]>) -> Result<ExitStatus, Failure> {
        let mut command = Command::new(program);
        if password.is_some() {
            command.args(["-S", "-p", "", "-v"]);
        } else {
            command.args(["-n", "-v"]);
        }
        // This child validates sudo only. It never executes the user's command,
        // so a cached/NOPASSWD path cannot pass unread password bytes to that command.
        command
            .stdin(if password.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove("SUDO_ASKPASS")
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| Failure::new("sudo_unavailable"))?;
        let work = async {
            if let Some(password) = password
                && let Some(mut input) = child.stdin.take()
            {
                // If sudo exits before reading, its exit status remains the
                // authority. No input is retained, logged or retried.
                if input.write_all(password).await.is_ok() {
                    let _ = input.write_all(b"\n").await;
                }
            }
            child
                .wait()
                .await
                .map_err(|_| Failure::new("sudo_validation_wait_failed"))
        };
        match tokio::time::timeout(Duration::from_secs(20), work).await {
            Ok(status) => status,
            Err(_) => {
                let _ = child.kill().await;
                Err(Failure::new("sudo_validation_timed_out"))
            }
        }
    }

    pub(super) async fn authorize_with<P, F>(program: &Path, credential: P) -> Result<(), Failure>
    where
        P: FnOnce() -> F,
        F: Future<Output = Result<Zeroizing<Vec<u8>>, Failure>>,
    {
        if validate(program, None).await?.success() {
            return Ok(());
        }
        let password = validate_secret(credential().await?)?;
        let status = validate(program, Some(&password)).await?;
        if status.success() {
            Ok(())
        } else {
            Err(Failure {
                code: "sudo_authentication_failed",
                exit_code: status.code(),
            })
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn executable(root: &Path, body: &str) -> std::path::PathBuf {
            let path = root.join("fake-sudo");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }

        #[tokio::test]
        async fn cached_sudo_does_not_read_a_credential() {
            let temp = tempfile::tempdir().unwrap();
            let path = executable(temp.path(), "test \"$1\" = -n && test \"$2\" = -v");
            authorize_with(&path, || async {
                panic!("a cached OS authorization must not read the credential")
            })
            .await
            .unwrap();
        }

        #[tokio::test]
        async fn credential_reaches_only_validation_stdin_and_errors_remain_visible() {
            let temp = tempfile::tempdir().unwrap();
            let path = executable(
                temp.path(),
                r#"test "$1" = -n && exit 1
test "$1" = -S && test "$2" = -p && test "$3" = "" && test "$4" = -v || exit 9
IFS= read -r input
test "$input" = 'synthetic$ password with spaces'
"#,
            );
            authorize_with(&path, || async {
                Ok(Zeroizing::new(b"synthetic$ password with spaces".to_vec()))
            })
            .await
            .unwrap();
            let failure = authorize_with(&path, || async {
                Ok(Zeroizing::new(b"wrong synthetic password".to_vec()))
            })
            .await
            .unwrap_err();
            assert_eq!(failure.code, "sudo_authentication_failed");
            assert_eq!(failure.exit_code, Some(1));
            assert!(!format!("{failure:?}").contains("password"));
        }

        #[test]
        fn credentials_and_identity_are_bounded_without_command_classification() {
            assert!(account("not-a-device").is_err());
            let a = account("00000000-0000-4000-8000-000000000001").unwrap();
            let b = account("00000000-0000-4000-8000-000000000002").unwrap();
            assert_ne!(a, b);
            for secret in [b"".as_slice(), b"one\ntwo", b"one\rtwo", b"one\0two"] {
                assert!(validate_secret(Zeroizing::new(secret.to_vec())).is_err());
            }
        }
    }
}
