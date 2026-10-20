//! Synthetic loopback SSH regression: no real credentials, sudo, or host configuration.
use super::*;
use russh::server::{self, Server as _, Session};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Mutex;

#[derive(Clone)]
struct FixtureServer {
    input: Arc<Mutex<Vec<u8>>>,
    commands: Arc<Mutex<Vec<String>>>,
    marker: String,
    exit_code: u32,
}
impl server::Server for FixtureServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}
impl server::Handler for FixtureServer {
    type Error = russh::Error;
    async fn auth_password(&mut self, _: &str, _: &str) -> Result<server::Auth, Self::Error> {
        Ok(server::Auth::Accept)
    }
    async fn channel_open_session(
        &mut self,
        _: russh::Channel<server::Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }
    async fn exec_request(
        &mut self,
        channel: russh::ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.commands
            .lock()
            .await
            .push(String::from_utf8_lossy(data).into_owned());
        session.channel_success(channel)?;
        Ok(())
    }
    async fn data(
        &mut self,
        _: russh::ChannelId,
        data: &[u8],
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.input.lock().await.extend_from_slice(data);
        Ok(())
    }
    async fn channel_eof(
        &mut self,
        channel: russh::ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // A command-specific NOPASSWD target succeeds; an authentication-required
        // target fails. Both must see EOF before they finish, with no password data.
        session.data(
            channel,
            format!("target EOF\n{} {}\n", self.marker, self.exit_code).into_bytes(),
        )?;
        session.exit_status_request(channel, self.exit_code)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

struct FixtureCredentials(Arc<AtomicUsize>);
#[async_trait]
impl SshCredentialProvider for FixtureCredentials {
    async fn credential_for(
        &self,
        _: AccessPathId,
    ) -> Result<SshCredentialSecret, SshCredentialError> {
        Ok(SshCredentialSecret {
            password: Some(SecretString::from("virtual SSH login".to_owned())),
            private_key_pem: None,
            private_key_passphrase: None,
            use_ssh_agent: false,
        })
    }
    async fn sudo_password_for(&self, _: AccessPathId) -> Result<SecretString, SshCredentialError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(SecretString::from(
            "virtual sudo credential never requested".to_owned(),
        ))
    }
}

async fn assert_credential_free_exec(exit_code: u32) -> Result<(), Box<dyn std::error::Error>> {
    let input = Arc::new(Mutex::new(Vec::new()));
    let commands = Arc::new(Mutex::new(Vec::new()));
    let secret_reads = Arc::new(AtomicUsize::new(0));
    let operation_id = OperationId::new();
    let mut server = FixtureServer {
        input: Arc::clone(&input),
        commands: Arc::clone(&commands),
        marker: format!("OCTENDRA_EXEC_DONE_{operation_id}"),
        exit_code,
    };
    let key = russh::keys::PrivateKey::new(
        russh::keys::ssh_key::private::KeypairData::Ed25519(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7; 32]),
        ),
        "synthetic fixture",
    )?;
    let config = Arc::new(server::Config {
        keys: vec![key],
        ..Default::default()
    });
    let socket = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = socket.local_addr()?.port();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let running = server.run_on_socket(config, &socket);
        let _ = ready_tx.send(running.handle());
        running.await
    });
    let handle = ready_rx.await?;
    let pool = octendra_db::connect_sqlite("sqlite::memory:").await?;
    octendra_db::migrate(&pool).await?;
    let repositories = octendra_db::Repositories::new(pool);
    let access_path_id = AccessPathId::new();
    let transport = RusshTransport::new(
        RusshTransportConfig {
            access_path_id,
            address: "127.0.0.1".into(),
            port,
            username: "fixture".into(),
            windows: false,
            use_exec_file_transfer: false,
            allow_exec_file_transfer_fallback: false,
            host_key_policy: HostKeyPolicy::Accept,
            known_hosts_path: None,
            connect_timeout_seconds: 5,
            inactivity_timeout_seconds: 10,
            idle_ttl_seconds: 10,
            keepalive_seconds: 0,
            max_concurrent_channels: 1,
            max_new_connections_per_minute: 2,
            max_new_ssh_handshakes_per_10_min: 2,
        },
        Arc::new(FixtureCredentials(Arc::clone(&secret_reads))),
        repositories.authorized_key_bootstrap,
    );
    let result = transport
        .exec(ExecRequest {
            operation_id,
            host_id: HostId::new(),
            access_path_id,
            profile: CommandProfile {
                name: "sudo.posix".into(),
                program: "sudo".into(),
                args: vec!["-n".into(), "--".into(), "cat".into()],
                class: octendra_core::CommandClass::Sensitive,
                timeout_seconds: 5,
                output_limit_bytes: 4096,
                requires_tty: false,
            },
        })
        .await;
    handle.shutdown("synthetic fixture complete".into());
    task.await??;
    let result = result?;
    assert_eq!(result.exit_code, i32::try_from(exit_code).ok());
    assert_eq!(secret_reads.load(Ordering::SeqCst), 0);
    assert!(
        input.lock().await.is_empty(),
        "target must never receive credential stdin"
    );
    let commands = commands.lock().await;
    assert_eq!(commands.len(), 1);
    assert!(!commands[0].contains("sudo -n true"));
    assert!(!commands[0].contains("'-S'"));
    Ok(())
}

#[tokio::test]
async fn ssh_nopasswd_target_receives_eof_without_loading_sudo_credentials()
-> Result<(), Box<dyn std::error::Error>> {
    assert_credential_free_exec(0).await
}

#[tokio::test]
async fn ssh_password_required_target_fails_without_credential_fallback()
-> Result<(), Box<dyn std::error::Error>> {
    assert_credential_free_exec(1).await
}
