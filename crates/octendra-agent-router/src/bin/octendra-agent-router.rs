//! Standalone per-machine Host Agent Router daemon and loopback RPC helper.

use std::{io::Write, net::SocketAddr, path::PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "octendra-agent-router",
    version,
    about = "Octendra per-machine Host Agent Router daemon and loopback RPC bridge"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the local loopback Host Agent Router HTTP daemon (`127.0.0.1:18790`).
    Daemon {
        /// Loopback bind address.
        #[arg(long, default_value = "127.0.0.1:18790")]
        bind: SocketAddr,
        /// Optional state storage directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Initialize or export a peer spoke connection bundle on the central Hub (`Mac-Studio`).
    InitHubBundle {
        /// Public or relay `/control` URL that spokes poll (`https://.../control`).
        #[arg(long)]
        hub_url: String,
        /// Output file path for the spoke bundle JSON.
        #[arg(long)]
        output: PathBuf,
        /// Optional state storage directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Import a peer spoke connection bundle on a remote Octendra computer.
    ImportSpokeBundle {
        /// Input bundle JSON path.
        #[arg(long)]
        input: PathBuf,
        /// Delete the temporary input file after importing.
        #[arg(long, default_value_t = false)]
        delete_input: bool,
        /// Optional state storage directory.
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Execute a single loopback HTTP RPC against the local Host Agent Router and print JSON.
    Rpc {
        /// Loopback router address.
        #[arg(long, default_value = "127.0.0.1:18790")]
        addr: SocketAddr,
        /// HTTP method (`GET`, `POST`, `DELETE`).
        #[arg(long, default_value = "GET")]
        method: String,
        /// Router path (for example `/v1/health`, `/v1/apps`, `/v1/sessions/turn`).
        #[arg(long)]
        path: String,
        /// Optional base64-encoded UTF-8 JSON request body.
        #[arg(long)]
        payload_b64: Option<String>,
        /// Optional timeout in milliseconds (`0` = no artificial client timeout; wait for real router state).
        #[arg(long, default_value_t = 0)]
        timeout_ms: u64,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon { bind, state_dir } => {
            let state = match state_dir {
                Some(dir) => octendra_agent_router::AgentRouterState::with_storage_dir(dir),
                None => octendra_agent_router::AgentRouterState::new(),
            };
            octendra_agent_router::serve(bind, state)
                .await
                .context("serve octendra-agent-router")?;
        }
        Command::InitHubBundle {
            hub_url,
            output,
            state_dir,
        } => {
            let state = match state_dir {
                Some(dir) => octendra_agent_router::AgentRouterState::with_storage_dir(dir),
                None => octendra_agent_router::AgentRouterState::new(),
            };
            state
                .init_or_export_hub_bundle(&hub_url, &output)
                .map_err(anyhow::Error::msg)?;
            let mut stdout = std::io::stdout().lock();
            writeln!(
                stdout,
                "{}",
                serde_json::json!({
                    "status": "ok",
                    "output": output.display().to_string(),
                })
            )
            .context("write stdout")?;
        }
        Command::ImportSpokeBundle {
            input,
            delete_input,
            state_dir,
        } => {
            let state = match state_dir {
                Some(dir) => octendra_agent_router::AgentRouterState::with_storage_dir(dir),
                None => octendra_agent_router::AgentRouterState::new(),
            };
            let hub_url = state
                .import_spoke_bundle(&input, delete_input)
                .map_err(anyhow::Error::msg)?;
            let mut stdout = std::io::stdout().lock();
            writeln!(
                stdout,
                "{}",
                serde_json::json!({
                    "status": "ok",
                    "hub_url": hub_url,
                })
            )
            .context("write stdout")?;
        }
        Command::Rpc {
            addr,
            method,
            path,
            payload_b64,
            timeout_ms,
        } => {
            let value = octendra_agent_router::rpc_loopback_json(
                addr,
                &method,
                &path,
                payload_b64.as_deref(),
                timeout_ms,
            )
            .await
            .map_err(anyhow::Error::msg)?;
            let out = serde_json::to_string(&value).context("serialize rpc json")?;
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "{out}").context("write stdout")?;
        }
    }
    Ok(())
}
