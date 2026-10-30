//! Octendra gateway/device service alongside the existing managed SSH connector.
#![cfg_attr(windows, windows_subsystem = "windows")]
mod oauth_client_metadata;
#[cfg(unix)]
mod owner_password;
#[cfg(unix)]
mod owner_password_check;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use octendra_code::{AgentConfig, GatewayConfig, random, read_config, write_private};
use std::{fs::OpenOptions, net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(
    name = "octendra-code",
    version,
    about = "Octendra personal code gateway and device agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Offline read-only projection of existing public OAuth client metadata.
    InspectOauthClients {
        #[arg(long)]
        state_db: PathBuf,
    },
    #[cfg(target_os = "macos")]
    #[command(hide = true)]
    MaintenanceTask {
        #[command(flatten)]
        args: octendra_code::maintenance_tasks::ChildArgs,
    },
    #[command(hide = true)]
    SelfUpgradeWatchdog {
        #[arg(long)]
        spec: PathBuf,
    },
    /// Serve OAuth, MCP and device relay behind a TLS reverse proxy.
    Gateway {
        #[arg(long)]
        config: PathBuf,
    },
    /// Connect this device outbound and execute authorized code tools locally.
    Agent {
        #[arg(long)]
        config: PathBuf,
        /// Only read/list one explicit status directory; requires write/exec=false.
        #[arg(long)]
        state_only: bool,
        /// Also write diagnostics to the invoking console. Default Windows service mode has no window.
        #[arg(long)]
        foreground: bool,
    },
    /// Create private gateway configuration and a separate owner login password file.
    InitGateway {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        public_url: String,
        #[arg(long, default_value = "127.0.0.1:18787")]
        bind: String,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long, default_value = "jinliang")]
        owner: String,
        #[arg(long)]
        password_file: PathBuf,
    },
    /// Replace only the existing gateway owner's password hash from an interactive terminal.
    ResetOwnerPassword {
        #[arg(long)]
        config: PathBuf,
    },
    /// Compare a private password file with an on-disk config, offline and read-only.
    CheckOwnerPassword {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        password_file: PathBuf,
    },
    /// Add one independently revocable device and write its private agent config.
    Enroll {
        #[arg(long)]
        gateway_config: PathBuf,
        #[arg(long)]
        agent_config: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long, required = true)]
        root: Vec<PathBuf>,
        #[arg(long)]
        allow_write: bool,
        #[arg(long)]
        allow_exec: bool,
        #[arg(long, default_value = "/bin/zsh")]
        shell: PathBuf,
    },
    /// Pre-register an OAuth client and save credentials to a new private file.
    /// The exact callback must already be allowed in the gateway configuration.
    RegisterOauthClient {
        #[arg(long)]
        gateway_config: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long, required = true)]
        redirect_uri: Vec<String>,
        #[arg(long, default_value = "client_secret_basic", value_parser = ["none", "client_secret_basic", "client_secret_post"])]
        auth_method: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Print the static machine-readable tool/protocol contract used by release packaging.
    ReleaseManifest,
    /// Serve the generated tools over stdio with durable request receipts and automatic catalog reports.
    Adapter {
        #[arg(long)]
        config: PathBuf,
    },
    /// Generate or validate the adapter contract from the compiled Gateway catalog.
    AdapterContract {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        check: bool,
    },
    /// Validate configuration without starting services or printing credentials.
    Check {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        agent: bool,
    },
    /// Pre-authorize silent sudo execution once for this device agent (via private credential or sudoers NOPASSWD).
    AuthorizeSudo {
        #[arg(long)]
        config: PathBuf,
        /// Read the sudo password from this file (or stdin when omitted unless --sudoers is used alone).
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// Also install /etc/sudoers.d/octendra-code (NOPASSWD: ALL + !tty_tickets) validated via visudo.
        #[arg(long)]
        sudoers: bool,
        #[arg(long)]
        user: Option<String>,
    },
}
fn lock(dir: &std::path::Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("service.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .context("another service is already using this state directory")?;
    Ok(file)
}
#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "inspect-oauth-clients")
    {
        // No logger, config, service lock or parser diagnostics on this path.
        let (output, code) = match Cli::try_parse() {
            Ok(Cli {
                command: Command::InspectOauthClients { state_db },
            }) => oauth_client_metadata::run(&state_db).await,
            _ => (serde_json::json!({"error":"invalid_arguments"}), 2),
        };
        use std::io::Write;
        let _ = writeln!(std::io::stdout().lock(), "{output}");
        std::process::exit(code);
    }
    // Handle before any logger/service setup; never display parser diagnostics,
    // paths, configuration, password bytes or verification errors for this command.
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "check-owner-password")
    {
        let result = match Cli::try_parse() {
            Ok(Cli {
                command:
                    Command::CheckOwnerPassword {
                        config,
                        password_file,
                    },
            }) => {
                #[cfg(unix)]
                {
                    owner_password_check::check(&config, &password_file)
                }
                #[cfg(not(unix))]
                {
                    let _ = (config, password_file);
                    ("error", 2)
                }
            }
            _ => ("error", 2),
        };
        use std::io::Write;
        let _ = writeln!(std::io::stdout().lock(), "{}", result.0);
        std::process::exit(i32::from(result.1));
    }
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args().collect();
        if args.get(1).is_some_and(|v| v == "--approval") {
            ensure!(args.len() == 3, "worker requires one private approval path");
            return octendra_code::computer_use::windows_worker::run(std::path::Path::new(
                &args[2],
            ));
        }
        if args.get(1).is_some_and(|v| v == "desktop-window-authority") {
            ensure!(
                args.len() == 5,
                "approved window preparation requires PID, HWND and private output file"
            );
            return octendra_code::computer_use::windows_target::prepare_authority(
                args[2].parse()?,
                args[3].parse()?,
                std::path::Path::new(&args[4]),
            );
        }
        if args.get(1).is_some_and(|v| v == "desktop-window-target") {
            ensure!(args.len() == 4, "window target requires PID and window ID");
            let target = octendra_code::computer_use::windows_target::window_target(
                args[2].parse()?,
                args[3].parse()?,
            )?;
            println!("{}", serde_json::to_string(&target)?);
            return Ok(());
        }
    }
    #[cfg(target_os = "macos")]
    {
        let args: Vec<_> = std::env::args_os().collect();
        if args.get(1).is_some_and(|v| v == "--weavelo-approval") {
            ensure!(args.len() == 3, "worker requires one private approval path");
            return octendra_code::computer_use::weavelo_worker::run(std::path::Path::new(
                &args[2],
            ))
            .await;
        }
        if args.get(1).is_some_and(|v| v == "--approval") {
            ensure!(args.len() == 3, "worker requires one private approval path");
            return octendra_code::computer_use::macos_worker::run(std::path::Path::new(&args[2]))
                .await;
        }
        if args.get(1).is_some_and(|v| v == "desktop-window-target") {
            ensure!(
                args.len() == 4,
                "desktop-window-target requires PID and window ID"
            );
            let pid = args[2].to_str().context("process ID")?.parse()?;
            let window = args[3].to_str().context("window ID")?.parse()?;
            println!(
                "{}",
                octendra_code::computer_use::macos_worker::window_target(pid, window)?
            );
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args_os().skip(1).collect();
        let background = args.first().is_some_and(|v| v == "agent")
            && !args
                .iter()
                .any(|v| v == "--foreground" || v == "--help" || v == "-h");
        if !background {
            octendra_code::agent_log::attach_parent_console();
        }
    }
    let command = if std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "reset-owner-password")
    {
        // Clap's normal argument error can echo an unexpected positional value.
        // Never repeat a password accidentally supplied to this subcommand.
        match Cli::try_parse() {
            Ok(cli) => cli.command,
            Err(error)
                if matches!(
                    error.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                error.exit()
            }
            Err(_) => anyhow::bail!(
                "usage: octendra-code reset-owner-password --config PATH\n\
                 Password arguments and noninteractive confirmation are not accepted."
            ),
        }
    } else {
        Cli::parse().command
    };
    if let Command::ResetOwnerPassword { config } = &command {
        #[cfg(unix)]
        return owner_password::run(config).await;
        #[cfg(not(unix))]
        {
            let _ = config;
            anyhow::bail!("owner password reset requires a local or SSH POSIX terminal");
        }
    }
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    if let Command::Agent {
        config,
        foreground,
        state_only,
    } = &command
    {
        use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
        let c: AgentConfig = read_config(config)?;
        if *state_only {
            octendra_code::agent::Agent::validate_state_only_config_path(&c, config)?;
        }
        let _lock = lock(&c.state_dir)?;
        if !*state_only {
            let _ = octendra_code::auto_upgrade::prune_agent_releases_and_logs(&c.state_dir);
        }
        let file = std::sync::Mutex::new(octendra_code::agent_log::AgentLog::open(&c.state_dir)?);
        let writer = if *foreground || !cfg!(windows) {
            BoxMakeWriter::new(file.and(std::io::stderr))
        } else {
            BoxMakeWriter::new(file)
        };
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(writer)
            .init();
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            pid = std::process::id(),
            console = (*foreground || !cfg!(windows)),
            "agent starting; bounded local logs enabled"
        );
        let result = async {
            let agent = if *state_only {
                octendra_code::agent::Agent::new_state_only_with_config_path(c, config.clone())
                    .await?
            } else {
                octendra_code::agent::Agent::new_with_config_path(c, config.clone()).await?
            };
            agent.run().await
        }
        .await;
        if result.is_err() {
            tracing::error!("agent stopped with error; inspect preceding local diagnostics");
        }
        return result;
    }
    if let Command::Gateway { config } = &command {
        use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriterExt};
        let c: GatewayConfig = read_config(config)?;
        let bind: SocketAddr = c.bind.parse()?;
        ensure!(
            bind.ip().is_loopback(),
            "gateway must bind loopback behind Caddy"
        );
        let _lock = lock(&c.state_dir)?;
        let file = std::sync::Mutex::new(octendra_code::agent_log::AgentLog::open_named(
            &c.state_dir,
            "gateway.log",
        )?);
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(BoxMakeWriter::new(file.and(std::io::stderr)))
            .init();
        let gateway =
            octendra_code::gateway::Gateway::new_with_config_path(c, config.clone()).await?;
        let _ = octendra_code::auto_upgrade::prune_gateway_releases_and_logs(&gateway);
        let listener = tokio::net::TcpListener::bind(bind).await?;
        tracing::info!(%bind, "gateway listening; bounded logs and release retention enabled");
        let router = gateway.router()?;
        let serving = async {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await
        };
        tokio::select! {
            result = serving => result?,
            result = gateway.maintain_history() => result?,
        }
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    match command {
        #[cfg(target_os = "macos")]
        Command::MaintenanceTask { args } => {
            let value = octendra_code::maintenance_tasks::run_child(args);
            let (value, failed) = match value {
                Ok(value) => {
                    let failed = value["state"] == "failed"
                        || value["state"] == "awaiting_platform_authorization"
                        || value["recovery_required"] == true;
                    (value, failed)
                }
                Err(error) => (
                    serde_json::json!({"ok":false,"error":format!("{error:#}"),
                    "system_changes_confirmed":false,"next_action":"Observe the original operation and maintenance receipt; do not replay with a new request ID."}),
                    true,
                ),
            };
            println!("{}", serde_json::to_string(&value)?);
            if failed {
                std::process::exit(2);
            }
        }
        Command::SelfUpgradeWatchdog { spec } => {
            let receipt = octendra_code::auto_upgrade::run_self_upgrade_watchdog(&spec).await?;
            println!("{}", serde_json::to_string(&receipt)?);
            if receipt.get("state").and_then(serde_json::Value::as_str) != Some("upgraded") {
                std::process::exit(1);
            }
        }
        Command::Gateway { config } => {
            let c: GatewayConfig = read_config(&config)?;
            let bind: SocketAddr = c.bind.parse()?;
            ensure!(
                bind.ip().is_loopback(),
                "gateway must bind loopback behind Caddy"
            );
            let _lock = lock(&c.state_dir)?;
            let gateway =
                octendra_code::gateway::Gateway::new_with_config_path(c, config.clone()).await?;
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!(%bind,"gateway listening");
            let router = gateway.router()?;
            let serving = async {
                axum::serve(listener, router)
                    .with_graceful_shutdown(async {
                        let _ = tokio::signal::ctrl_c().await;
                    })
                    .await
            };
            tokio::select! {
                result = serving => result?,
                result = gateway.maintain_history() => result?,
            }
        }
        Command::Agent { .. } => unreachable!("agent handled before service logger initialization"),
        Command::ResetOwnerPassword { .. } => {
            unreachable!("password reset handled before service logger initialization")
        }
        Command::CheckOwnerPassword { .. } => unreachable!("offline check handled before logging"),
        Command::InspectOauthClients { .. } => {
            unreachable!("offline inspection handled before logging")
        }
        Command::InitGateway {
            config,
            public_url,
            bind,
            state_dir,
            owner,
            password_file,
        } => {
            octendra_code::validate_url(&public_url)?;
            ensure!(
                !config.exists() && !password_file.exists(),
                "refusing to overwrite existing configuration or password"
            );
            use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
            let password = random();
            let salt = SaltString::encode_b64(uuid::Uuid::new_v4().as_bytes())?;
            let password_hash = Argon2::default()
                .hash_password(password.as_bytes(), &salt)?
                .to_string();
            let c = GatewayConfig {
                public_url_aliases: vec![],
                forge_browser: None,
                public_url,
                bind,
                state_dir,
                owner,
                password_hash,
                devices: vec![],
                redirect_uris: vec!["https://chatgpt.com/connector_platform_oauth_redirect".into()],
                allowed_origins: octendra_code::default_mcp_client_origins(),
            };
            write_private(&password_file, password.as_bytes())?;
            write_private(&config, &serde_json::to_vec_pretty(&c)?)?;
            println!("Created gateway config and password file (values not displayed).");
        }
        Command::Enroll {
            gateway_config,
            agent_config,
            name,
            state_dir,
            root,
            allow_write,
            allow_exec,
            shell,
        } => {
            let id = octendra_code::enroll_device(
                &gateway_config,
                &agent_config,
                name,
                state_dir,
                root,
                allow_write,
                allow_exec,
                shell,
            )?;
            println!("Enrolled device {id}; reload gateway configuration before connecting.");
        }
        Command::RegisterOauthClient {
            gateway_config,
            name,
            redirect_uri,
            auth_method,
            output,
        } => {
            use std::io::Write;
            ensure!(!output.exists(), "refusing to overwrite OAuth credentials");
            let parent = output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| std::path::Path::new("."));
            std::fs::create_dir_all(parent)?;
            let mut file = tempfile::NamedTempFile::new_in(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.as_file()
                    .set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            let c: GatewayConfig = read_config(&gateway_config)?;
            let gateway = octendra_code::gateway::Gateway::new(c).await?;
            let client = gateway
                .auth
                .register_client(&serde_json::json!({
                    "client_name": name,
                    "redirect_uris": redirect_uri,
                    "token_endpoint_auth_method": auth_method,
                }))
                .await
                .map_err(|(status, error)| {
                    anyhow::anyhow!("OAuth client registration failed: {} {}", status, error.0)
                })?;
            let saved = (|| -> Result<()> {
                file.write_all(&serde_json::to_vec_pretty(&client)?)?;
                file.as_file().sync_all()?;
                file.persist_noclobber(&output)?;
                Ok(())
            })();
            if saved.is_err() {
                // A credential file failure must not leave a usable orphan client.
                if let Some(id) = client.get("client_id").and_then(serde_json::Value::as_str) {
                    gateway
                        .store
                        .take::<serde_json::Value>(
                            &octendra_code::auth::oauth_state_kind("client", id),
                            id,
                        )
                        .await?;
                }
            }
            saved?;
            println!(
                "OAuth client created; credentials saved to the requested private file (values not displayed)."
            );
        }
        Command::Adapter { config } => {
            octendra_code::adapter::serve(&config).await?;
        }
        Command::AdapterContract { output, check } => {
            octendra_code::contract::export(&output, check)?;
            println!(
                "{}",
                serde_json::json!({"state":"passed","checked":check,"path":output})
            );
        }
        Command::ReleaseManifest => {
            println!(
                "{}",
                serde_json::to_string(&octendra_code::release_manifest())?
            );
        }
        Command::Check { config, agent } => {
            if agent {
                let c: AgentConfig = read_config(&config)?;
                octendra_code::validate_url(&c.gateway_url)?;
                uuid::Uuid::parse_str(&c.device_id)?;
                ensure!(!c.roots.is_empty(), "no project roots");
            } else {
                let c: GatewayConfig = read_config(&config)?;
                c.validate_oauth_policy()?;
            }
            println!("Configuration valid; secrets not displayed.");
        }
        Command::AuthorizeSudo {
            config,
            password_file,
            sudoers,
            user,
        } => {
            let c: AgentConfig = read_config(&config)?;
            let mut stored_secret = None;
            if let Some(path) = password_file {
                let bytes = std::fs::read(&path)?;
                stored_secret = Some(octendra_code::local_sudo::store_credential(&c, &bytes)?);
            } else if !sudoers {
                use std::io::Read;
                let mut bytes = Vec::new();
                std::io::stdin().read_to_end(&mut bytes)?;
                stored_secret = Some(octendra_code::local_sudo::store_credential(&c, &bytes)?);
            }
            let sudoers_path = if sudoers {
                Some(octendra_code::local_sudo::configure_sudoers_rule(
                    user.as_deref(),
                )?)
            } else {
                None
            };
            println!(
                "{}",
                serde_json::json!({
                    "state": "authorized",
                    "device_id": c.device_id,
                    "credential_path": stored_secret,
                    "sudoers_path": sudoers_path,
                })
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod state_only_cli_tests {
    use super::*;
    #[test]
    fn state_only_is_explicit_and_general_agent_arguments_remain_compatible() {
        let general =
            Cli::try_parse_from(["octendra-code", "agent", "--config", "fixture.json"]).unwrap();
        assert!(matches!(
            general.command,
            Command::Agent {
                state_only: false,
                ..
            }
        ));
        let restricted = Cli::try_parse_from([
            "octendra-code",
            "agent",
            "--config",
            "fixture.json",
            "--state-only",
        ])
        .unwrap();
        assert!(matches!(
            restricted.command,
            Command::Agent {
                state_only: true,
                ..
            }
        ));
    }
}
