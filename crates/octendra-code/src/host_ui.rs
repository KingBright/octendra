//! Bridges local `Agent` desktop workers into `octendra-agent-router`'s `/v1/ui/*` endpoints.
#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::Result;
use octendra_agent_router::{HostUiBridge, UiBoxFuture};
use serde_json::{Value, json};
use std::path::Path;
#[cfg(target_os = "macos")]
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

use crate::{agent::Agent, ui_workspace::ExecutionContext};

/// Shared local UI bridge held by the embedded `octendra-agent-router` server and attached
/// once `Agent::new_with_config_path` finishes initializing its `Store` and `NativeWorkers`.
#[derive(Default)]
pub struct LocalAgentUiBridge {
    agent: RwLock<Option<Agent>>,
}

impl LocalAgentUiBridge {
    /// Creates an unattached bridge; call [`Self::attach`] once the [`Agent`] is ready.
    pub fn new() -> Self {
        Self::default()
    }

    /// Attaches the live [`Agent`] instance to serve local `/v1/ui/*` calls.
    pub async fn attach(&self, agent: Agent) {
        *self.agent.write().await = Some(agent);
    }

    async fn snapshot_agent(&self) -> Option<Agent> {
        self.agent.read().await.clone()
    }
}

fn read_existing_launch_approval(state_dir: &Path) -> Option<Value> {
    let path = state_dir.join("desktop-launch.json");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 65536 {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()
}

fn resolve_owner(
    state_dir: &Path,
    explicit_owner: Option<&str>,
    mem_owner: Option<&str>,
) -> String {
    if let Some(o) = explicit_owner.map(str::trim).filter(|s| !s.is_empty()) {
        return o.to_string();
    }
    if let Some(existing) = read_existing_launch_approval(state_dir)
        && let Some(o) = existing
            .pointer("/identity/owner")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return o.to_string();
    }
    if let Some(o) = mem_owner.map(str::trim).filter(|s| !s.is_empty()) {
        return o.to_string();
    }
    "owner".to_string()
}

#[cfg(target_os = "macos")]
fn read_previous_capability_version(state_dir: &Path) -> u64 {
    read_existing_launch_approval(state_dir)
        .and_then(|v| v.get("capability_version").and_then(Value::as_u64))
        .unwrap_or(0)
}

#[cfg(all(target_os = "macos", unix))]
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("desktop-launch.json");
    let tmp = path.with_file_name(format!(".{file_name}.tmp.{}.{now_ns}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(all(target_os = "macos", not(unix)))]
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

async fn bind_window_impl(agent: &Agent, req: &Value) -> Result<Value> {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (agent, req);
        anyhow::bail!(
            "dynamic window binding via /v1/ui/bind_window is currently supported on macOS only"
        )
    }

    #[cfg(target_os = "macos")]
    {
        let target_obj = req.get("target").filter(|v| v.is_object());
        let app_query = req
            .get("app")
            .and_then(Value::as_str)
            .or_else(|| target_obj.and_then(|t| t.get("app").and_then(Value::as_str)));
        let window_query = req
            .get("window")
            .and_then(Value::as_str)
            .or_else(|| target_obj.and_then(|t| t.get("window").and_then(Value::as_str)));
        let pid_filter = req
            .get("pid")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok());
        let allow_input = req
            .get("input")
            .or_else(|| req.get("allow_input"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let explicit_owner = req.get("owner").and_then(Value::as_str);

        let selected = crate::computer_use::macos_worker::select_onscreen_user_window(
            app_query,
            window_query,
            pid_filter,
        )?;

        let desktop_uid = selected
            .get("desktop_uid")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .context("selected macOS window missing desktop_uid")?;
        let desktop_session = selected
            .get("desktop_session")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .context("selected macOS window missing desktop_session")?
            .to_string();
        let logical_size: [u32; 2] = serde_json::from_value(
            selected
                .get("logical_size")
                .cloned()
                .context("selected macOS window missing logical_size")?,
        )?;
        let logical_position: [i32; 2] = serde_json::from_value(
            selected
                .get("logical_position")
                .cloned()
                .context("selected macOS window missing logical_position")?,
        )?;

        let mem_cap = agent.desktop_workers.current_capability().await;
        let mem_ver = mem_cap.as_ref().map_or(0, |(v, _)| *v);
        let disk_ver = read_previous_capability_version(&agent.config.state_dir);
        let next_version = mem_ver.max(disk_ver).saturating_add(1);
        let owner = resolve_owner(
            &agent.config.state_dir,
            explicit_owner,
            mem_cap.as_ref().map(|(_, id)| id.owner.as_str()),
        );
        let device_uuid = uuid::Uuid::parse_str(&agent.config.device_id)
            .context("agent device_id must be a valid UUID")?;
        let exe: PathBuf = std::env::current_exe().context("resolve current_exe failed")?;
        let exe_bytes = std::fs::read(&exe).context("read current_exe failed")?;
        let exe_hash = crate::hash(&exe_bytes);

        let approval = crate::computer_use::transport::LaunchApproval {
            weavelo: None,
            enabled: true,
            observe: true,
            input: allow_input,
            capability_version: next_version,
            identity: crate::computer_use::CapabilityIdentity {
                owner: owner.clone(),
                device_id: device_uuid,
                desktop_id: format!("uid:{desktop_uid}:session:{desktop_session}"),
            },
            desktop_uid,
            desktop_session,
            logical_size,
            logical_position,
            max_frame_bytes: crate::computer_use::MAX_WORKER_PIXELS,
            worker_path: exe,
            worker_sha256: exe_hash.clone(),
            agent_sha256: exe_hash,
        };
        let encoded = serde_json::to_vec_pretty(&approval)?;
        let grant_path = agent.config.state_dir.join("desktop-launch.json");
        write_private_atomic(&grant_path, &encoded)?;

        let adopted = agent
            .desktop_workers
            .adopt_explicit_approval_file(&agent.config.device_id)
            .await?;

        Ok(json!({
            "device_id": agent.config.device_id,
            "capability_version": next_version,
            "identity": adopted,
            "app_name": selected.get("app_name").cloned().unwrap_or(Value::Null),
            "window_title": selected.get("window_title").cloned().unwrap_or(Value::Null),
            "logical_size": logical_size,
            "logical_position": logical_position,
            "input_enabled": allow_input,
        }))
    }
}

impl HostUiBridge for LocalAgentUiBridge {
    fn local_ui_status(&self) -> UiBoxFuture<'_, Value> {
        Box::pin(async move {
            let Some(agent) = self.snapshot_agent().await else {
                return json!({
                    "attached": false,
                    "reason": "agent_initializing",
                });
            };
            #[cfg(any(target_os = "linux", target_os = "macos", windows))]
            let (capabilities, current_capability) = (
                agent.desktop_workers.capabilities().await,
                agent.desktop_workers.current_capability().await,
            );
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let (capabilities, current_capability) = (
                crate::computer_use::Capabilities::default(),
                None::<(u64, crate::computer_use::CapabilityIdentity)>,
            );

            #[cfg(target_os = "macos")]
            let onscreen_windows =
                Value::Array(crate::computer_use::macos_worker::list_onscreen_user_windows());
            #[cfg(not(target_os = "macos"))]
            let onscreen_windows = json!([]);

            json!({
                "attached": true,
                "device_id": agent.config.device_id,
                "capabilities": capabilities,
                "capability_version": current_capability.as_ref().map(|(v, _)| *v),
                "capability_identity": current_capability.as_ref().map(|(_, id)| id),
                "onscreen_windows": onscreen_windows,
            })
        })
    }

    fn bind_local_window(&self, req: Value) -> UiBoxFuture<'_, Result<Value, String>> {
        Box::pin(async move {
            let Some(agent) = self.snapshot_agent().await else {
                return Err("local host agent is still initializing".into());
            };
            bind_window_impl(&agent, &req)
                .await
                .map_err(|e| format!("{e:#}"))
        })
    }

    fn execute_local_ui(
        &self,
        tool: String,
        args: Value,
    ) -> UiBoxFuture<'_, Option<Result<Value, String>>> {
        Box::pin(async move {
            let agent = self.snapshot_agent().await?;
            let mem_cap = agent.desktop_workers.current_capability().await;
            let owner = resolve_owner(
                &agent.config.state_dir,
                None,
                mem_cap.as_ref().map(|(_, id)| id.owner.as_str()),
            );
            let job = crate::gateway::Job {
                ui_context: Some(ExecutionContext::host_session(
                    agent.config.device_id.clone(),
                    "local",
                )),
                id: uuid::Uuid::new_v4().to_string(),
                device_id: agent.config.device_id.clone(),
                owner,
                tool,
                arguments: args,
            };
            Some(
                crate::ui::perform(&agent, &job)
                    .await
                    .map_err(|e| format!("{e:#}")),
            )
        })
    }
}
