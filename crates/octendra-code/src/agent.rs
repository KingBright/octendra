//! Outbound authenticated device worker; mutations are journaled before execution.
use crate::{
    AgentConfig,
    delivery::Delivery,
    files::{self, Workspace},
    gateway::{DeviceHello, Job},
    scheduler::{ActiveJobs, ActiveResource, Lane, ResourceFilter, Scheduler},
    store::Store,
    terminal::Terminals,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[path = "agent_liveness.rs"]
mod liveness;

#[derive(Clone)]
pub struct Agent {
    pub config: Arc<AgentConfig>,
    pub config_path: Option<PathBuf>,
    pub store: Store,
    pub(crate) terminals: Terminals,
    #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
    pub(crate) desktop_workers: crate::computer_use::transport::NativeWorkers,
    scheduler: Arc<Scheduler>,
    pub(crate) active: Arc<ActiveJobs>,
    progress: Arc<crate::progress::Registry>,
    readiness: Arc<crate::readiness::Readiness>,
    liveness: Arc<liveness::Liveness>,
    poll_health_revision: Arc<std::sync::atomic::AtomicU64>,
    history_access: Arc<tokio::sync::RwLock<()>>,
    legacy_feature_advertisement: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesktopJob {
    pub task_id: uuid::Uuid,
    pub capability_version: u64,
    pub request: crate::computer_use::Request,
    pub device_id: Option<uuid::Uuid>,
    // Gateway validates this envelope field; retain strict wire deserialization.
    #[allow(dead_code)]
    pub idempotency_key: Option<String>,
    // Gateway validates this envelope field; retain strict wire deserialization.
    #[allow(dead_code)]
    pub authorization_version: Option<u64>,
    // Gateway validates this envelope field; retain strict wire deserialization.
    #[allow(dead_code)]
    pub response_mode: Option<String>,
}
impl DesktopJob {
    pub(crate) fn identity(&self, job: &Job) -> Result<crate::computer_use::Identity> {
        let id = self
            .request
            .identity()
            .context("desktop identity missing")?;
        ensure!(
            id.owner == job.owner
                && id.task_id == self.task_id
                && id.device_id.to_string() == job.device_id
                && !id.task_id.is_nil()
                && self.device_id.is_none_or(|d| d == id.device_id)
                && self.capability_version > 0,
            "desktop job identity conflict"
        );
        Ok(id.clone())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DesktopFrameJob {
    pub task_id: uuid::Uuid,
    pub device_id: uuid::Uuid,
    pub capability_version: u64,
    pub observed_operation_id: uuid::Uuid,
    // Gateway validates this envelope field; retain strict wire deserialization.
    #[allow(dead_code)]
    #[serde(default)]
    pub idempotency_key: Option<String>,
    // Gateway validates this envelope field; retain strict wire deserialization.
    #[allow(dead_code)]
    pub authorization_version: Option<u64>,
    // Gateway validates this envelope field; retain strict wire deserialization.
    #[allow(dead_code)]
    pub response_mode: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct LocalOperation {
    fingerprint: String,
    state: String,
    result: Option<Value>,
    #[serde(default)]
    gateway_accepted: bool,
    #[serde(default)]
    resumable: bool,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    updated_at: i64,
}
impl Agent {
    pub async fn new(mut config: AgentConfig) -> Result<Self> {
        crate::validate_url(&config.gateway_url)?;
        uuid::Uuid::parse_str(&config.device_id)?;
        ensure!(config.device_token.len() >= 32, "device token too short");
        if config.roots.is_empty() {
            config.roots.push(PathBuf::from("/"));
        }
        for root in &mut config.roots {
            *root = root.canonicalize()?;
            ensure!(root.is_dir(), "root is not a directory");
        }
        config.state_dir = absolute(&config.state_dir)?;
        let store = Store::open(&config.state_dir).await?;
        store.install_agent_schema().await?;
        // An interrupted mutation has an unknown outcome; never blindly re-execute it.
        sqlx::query("UPDATE kv SET value=json_set(value,'$.state','unknown') WHERE kind='local_operation' AND json_extract(value,'$.state')='running' AND COALESCE(json_extract(value,'$.resumable'),0)=0").execute(&store.pool).await?;
        let terminals = Terminals::new(
            store.clone(),
            config.state_dir.join("terminals"),
            config.device_token.clone(),
        )
        .await?;
        store.put("runtime", "build", &json!({"version":env!("CARGO_PKG_VERSION"),"pid":std::process::id(),"started_at":crate::now()}), i64::MAX).await?;
        #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
        let desktop_workers = crate::computer_use::transport::NativeWorkers::load(
            &config.state_dir,
            &config.device_id,
        )
        .with_store(store.clone());
        Ok(Self {
            #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
            desktop_workers,
            config: Arc::new(config),
            config_path: None,
            store,
            terminals,
            scheduler: Arc::default(),
            active: Arc::default(),
            progress: Arc::default(),
            readiness: Arc::default(),
            liveness: Arc::default(),
            poll_health_revision: Arc::default(),
            history_access: Arc::default(),
            legacy_feature_advertisement: Arc::default(),
        })
    }
    pub async fn new_with_config_path(config: AgentConfig, config_path: PathBuf) -> Result<Self> {
        let mut agent = Self::new(config).await?;
        agent.config_path = Some(config_path);
        Ok(agent)
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
    pub async fn read_desktop_frame(&self, job: &Job) -> Result<Vec<u8>> {
        ensure!(
            crate::tools::desktop_session_tool(&job.tool) && job.device_id == self.config.device_id,
            "desktop job required"
        );
        uuid::Uuid::parse_str(&job.id)?;
        let args: DesktopJob = serde_json::from_value(job.arguments.clone())?;
        let identity = args.identity(job)?;
        let saved: LocalOperation = self
            .store
            .get("local_operation", &job.id)
            .await?
            .context("desktop result missing")?;
        ensure!(
            saved.fingerprint == crate::hash(serde_json::to_vec(job)?),
            "operation fingerprint conflict"
        );
        // Delivery may retire the result body after ACK; bounded frame metadata
        // is owned by the active desktop epoch, independently of receipt retention.
        let artifact = self
            .desktop_workers
            .artifact(uuid::Uuid::parse_str(&job.id)?)
            .await?;
        self.desktop_workers
            .read_frame(&artifact, &identity, args.capability_version)
            .await
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
    pub async fn revoke_desktop(&self, binding: &crate::computer_use::Binding) -> bool {
        self.desktop_workers.revoke(Some(binding)).await
    }
    pub async fn execute(&self, job: &Job) -> Result<Value> {
        self.execute_with_delivery(job, None).await
    }
    pub(crate) async fn execute_with_delivery(
        &self,
        job: &Job,
        delivery: Option<&Delivery>,
    ) -> Result<Value> {
        self.execute_journaled(job, delivery, |progress| async move {
            self.perform(job, &progress).await
        })
        .await
    }
    // Shared journal/receipt boundary. Offline mocks may substitute execution,
    // but are never registered in the production tool catalog.
    pub(crate) async fn execute_journaled<F, Fut>(
        &self,
        job: &Job,
        delivery: Option<&Delivery>,
        perform: F,
    ) -> Result<Value>
    where
        F: FnOnce(crate::progress::Progress) -> Fut,
        Fut: std::future::Future<Output = Result<Value>>,
    {
        uuid::Uuid::parse_str(&job.id).context("invalid operation id")?;
        ensure!(job.device_id == self.config.device_id, "wrong device");
        let _guard = self.scheduler.operations.lock(&job.id).await?;
        let fingerprint = crate::hash(serde_json::to_vec(job)?);
        if let Some(op) = self
            .store
            .get::<LocalOperation>("local_operation", &job.id)
            .await?
        {
            ensure!(
                op.fingerprint == fingerprint,
                "operation fingerprint conflict"
            );
            // A poll response dispatched before receipt acceptance can arrive
            // afterwards. Its compact receipt prevents re-execution, but must
            // not replace the authoritative Gateway result with a new payload.
            if op.gateway_accepted {
                return Ok(json!({"state":"already_completed","operation_id":job.id,
                    "result_owner":"gateway","next_action":"operation_get"}));
            }
            if !(op.resumable && op.result.is_none() && crate::durable_transfer::is_file(&job.tool))
            {
                let result = op.result.unwrap_or_else(||json!({"error":"outcome_unknown","operation_id":job.id,"recovery":"Inspect files, terminal status and local journal. This operation is never automatically repeated after runtime loss."}));
                if let Some(delivery) = delivery {
                    delivery.enqueue(&job.id, &result).await?;
                }
                return Ok(result);
            }
        }
        let progress = self.progress.start(&job.id);
        progress.phase("waiting_resource");
        self.store
            .put(
                "local_operation",
                &job.id,
                &LocalOperation {
                    fingerprint: fingerprint.clone(),
                    state: "running".into(),
                    result: None,
                    gateway_accepted: false,
                    resumable: crate::durable_transfer::is_file(&job.tool),
                    workspace_id: job.arguments["workspace_id"].as_str().map(str::to_owned),
                    tool: Some(job.tool.clone()),
                    updated_at: crate::now(),
                },
                i64::MAX,
            )
            .await?;
        progress.phase("running");
        let mut result = match perform(progress.clone()).await {
            Ok(v) => v,
            Err(e) => {
                crate::diagnostics::error(&job.tool, &e.to_string(), Some(&job.id), "agent_execute")
            }
        };
        if crate::durable_transfer::is_suspended(&result) {
            self.store
                .put(
                    "local_operation",
                    &job.id,
                    &LocalOperation {
                        fingerprint,
                        state: "paused".into(),
                        result: None,
                        gateway_accepted: false,
                        resumable: true,
                        workspace_id: job.arguments["workspace_id"].as_str().map(str::to_owned),
                        tool: Some(job.tool.clone()),
                        updated_at: crate::now(),
                    },
                    i64::MAX,
                )
                .await?;
            return Ok(result);
        }
        progress.phase(match result.get("state").and_then(Value::as_str) {
            Some("cancelled") => "cancelled",
            Some("failed") => "failed",
            _ if result.get("error").is_some() => "failed",
            _ => "completed",
        });
        if matches!(job.tool.as_str(), "file_upload" | "file_download") {
            result["progress"] = serde_json::to_value(progress.snapshot())?;
        }
        if result.is_object() {
            result["timing"] = progress.timings();
        }
        if serde_json::to_vec(&result)?.len() > 4 * 1024 * 1024 {
            result = json!({"error":"response_budget_exceeded","error_code":"response_budget_exceeded","outcome":"unknown","recovery_action":"inspect_original_operation_without_reexecution","automatic_replay_safe":false,"timing":progress.timings()});
        }
        let completed = LocalOperation {
            fingerprint,
            state: "done".into(),
            result: Some(result.clone()),
            gateway_accepted: false,
            resumable: false,
            workspace_id: job.arguments["workspace_id"].as_str().map(str::to_owned),
            tool: Some(job.tool.clone()),
            updated_at: crate::now(),
        };
        if let Some(delivery) = delivery {
            delivery.complete(&job.id, &completed, &result).await?;
        } else {
            self.store
                .put("local_operation", &job.id, &completed, i64::MAX)
                .await?;
        }
        Ok(result)
    }
    async fn perform(&self, job: &Job, progress: &crate::progress::Progress) -> Result<Value> {
        let v = &job.arguments;
        #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
        if crate::tools::desktop_session_tool(&job.tool) {
            // Typed catalog admission is independent of shell/code permissions.
            crate::tools::validate(&job.tool, v)?;
            let args: DesktopJob = serde_json::from_value(v.clone())?;
            if matches!(args.request, crate::computer_use::Request::Capabilities {}) {
                #[cfg(any(target_os = "linux", target_os = "macos", windows))]
                return Ok(serde_json::to_value(
                    crate::computer_use::Response::Capabilities {
                        capabilities: if job.tool == "desktop_observer" {
                            self.desktop_workers.observer_capabilities().await
                        } else {
                            self.desktop_workers.capabilities().await
                        },
                    },
                )?);
                #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
                return Ok(serde_json::to_value(
                    crate::computer_use::unavailable(&args.request)
                        .expect("capabilities are always available"),
                )?);
            }
            let identity = args.identity(job)?;
            if job.tool == "desktop_observer" {
                return self
                    .desktop_workers
                    .dispatch_observer(
                        uuid::Uuid::parse_str(&job.id)?,
                        &identity,
                        args.capability_version,
                        args.request,
                    )
                    .await;
            }
            return self
                .desktop_workers
                .dispatch(
                    uuid::Uuid::parse_str(&job.id)?,
                    &identity,
                    args.capability_version,
                    args.request,
                )
                .await;
        }

        #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
        if job.tool == "desktop_frame" {
            crate::tools::validate(&job.tool, v)?;
            let args: DesktopFrameJob = serde_json::from_value(v.clone())?;
            ensure!(
                args.device_id.to_string() == job.device_id,
                "desktop artifact device"
            );
            let artifact = self
                .desktop_workers
                .artifact(args.observed_operation_id)
                .await?;
            let identity = &artifact.binding.identity;
            ensure!(
                identity.owner == job.owner
                    && identity.task_id == args.task_id
                    && identity.device_id == args.device_id
                    && artifact.capability_version == args.capability_version,
                "desktop artifact owner/task/capability"
            );
            // Verify the existing bounded owned file, then reuse the file journal,
            // receiver and 4 MiB checkpoints. No client-selected root or path.
            drop(
                self.desktop_workers
                    .read_frame(&artifact, identity, args.capability_version)
                    .await?,
            );
            let mut export = job.clone();
            // Keep the desktop direction so the shared transfer code never
            // publishes a resumable file status for this ephemeral export.
            export.arguments = json!({"path":args.observed_operation_id.to_string(),
                "max_bytes":artifact.bytes,"expected_version":artifact.sha256});
            let ws = Workspace {
                id: format!("desktop:{}", args.observed_operation_id),
                device_id: job.device_id.clone(),
                root: self.config.state_dir.join("desktop-frames"),
            };
            let remaining = artifact
                .expires_at_ms
                .saturating_sub(crate::now_ms() as u64);
            let result = tokio::time::timeout(
                Duration::from_millis(remaining),
                crate::durable_transfer::run(
                    &self.config,
                    &ws,
                    &export,
                    progress,
                    &self.store,
                    self.scheduler.writes.clone(),
                ),
            )
            .await;
            // A desktop snapshot never gains file-transfer retention or replay.
            let data = self
                .config
                .state_dir
                .join("transfers-v2")
                .join(format!("{}.data", job.id));
            let removed = match tokio::fs::remove_file(data).await {
                Ok(()) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                _ => false,
            };
            let journal_expired = sqlx::query("UPDATE kv SET value=json_set(value,'$.phase','expired','$.data_cleaned',?,'$.expires_at',?) WHERE kind='transfer_local' AND key=?")
                .bind(removed).bind(crate::now()).bind(&job.id).execute(&self.store.pool).await.is_ok();
            let failure_code = match &result {
                Ok(Ok(value)) => value["diagnostic"]["code"]
                    .as_str()
                    .unwrap_or("desktop_export_failed")
                    .to_owned(),
                Ok(Err(_)) => "desktop_export_failed".into(),
                Err(_) => "desktop_artifact_expired".into(),
            };
            let completed = matches!(&result, Ok(Ok(value)) if value["state"] == "completed" && !crate::durable_transfer::is_suspended(value));
            let active = if completed && removed && journal_expired {
                self.desktop_workers
                    .read_frame(&artifact, identity, args.capability_version)
                    .await
                    .is_ok()
                    && (crate::now_ms() as u64) < artifact.expires_at_ms
            } else {
                false
            };
            if !active {
                let retired = self
                    .desktop_workers
                    .retire_artifact(args.observed_operation_id)
                    .await
                    .is_ok();
                // File-transfer checkpoints are already expired. Commit a terminal
                // receipt, and retire the observed frame so a new job cannot replay it.
                let mut terminal = json!({"operation_id":job.id,"state":"failed","error":"desktop_export_interrupted",
                    "error_code":"desktop_export_interrupted","outcome":"unknown","resumable":false,"pending":false,
                    "automatic_replay_safe":false,"cleanup_complete":removed && retired && journal_expired,
                    "diagnostic":{"code":failure_code},"next_action":"observe_a_new_frame"});
                if let Ok(Ok(value)) = &result
                    && let Some(revision) = value.get("transfer_revision")
                {
                    terminal["transfer_revision"] = revision.clone();
                }
                return Ok(terminal);
            }
            return result.map_err(|_| anyhow::anyhow!("desktop artifact expired"))?;
        }

        let _scope = crate::tools::scope(&job.tool).context("unsupported tool")?;
        // Revalidate nested types, bounds and required fields at the device boundary.
        crate::tools::validate(&job.tool, v)?;
        if job.tool == "workspace_open" {
            let _permit = self.scheduler.acquire(Lane::Write).await?;
            ensure!(
                files::text(v, "device_id")? == self.config.device_id,
                "wrong device"
            );
            let root = PathBuf::from(files::text(v, "root")?).canonicalize()?;
            ensure!(root.is_dir(), "root is not a directory");
            let id = format!("{}:{}", self.config.device_id, uuid::Uuid::new_v4());
            let ws = Workspace {
                id: id.clone(),
                device_id: self.config.device_id.clone(),
                root,
            };
            self.store.put("workspace", &id, &ws, i64::MAX).await?;
            return Ok(
                json!({"workspace":ws,"allow_write":true,"allow_exec":true,"agent_version":env!("CARGO_PKG_VERSION"),"file_transfer":true,"terminal_authority":"local_user; not confined by code roots"}),
            );
        }
        let id = files::text(v, "workspace_id")?;
        let ws: Workspace = match self.store.get("workspace", id).await? {
            Some(ws) => ws,
            None => {
                let candidate = id
                    .strip_prefix(&format!("{}:", self.config.device_id))
                    .unwrap_or(id);
                let target_dir = if candidate == "default" && !self.config.roots.is_empty() {
                    Some(self.config.roots[0].clone())
                } else if Path::new(candidate).is_dir() {
                    Some(PathBuf::from(candidate))
                } else {
                    None
                };
                if let Some(dir) = target_dir {
                    let root = dir.canonicalize()?;
                    let ws = Workspace {
                        id: id.to_owned(),
                        device_id: self.config.device_id.clone(),
                        root,
                    };
                    self.store.put("workspace", id, &ws, i64::MAX).await?;
                    ws
                } else {
                    bail!("workspace not found on this device")
                }
            }
        };
        ensure!(
            ws.device_id == self.config.device_id,
            "workspace device mismatch"
        );
        let canonical = ws.root.canonicalize()?;
        ensure!(
            canonical == ws.root && canonical.is_dir(),
            "workspace root moved"
        );
        crate::maintenance_tasks::require_capability(
            &job.tool,
            v,
            Some(std::env::consts::OS),
            crate::maintenance_tasks::current(true).as_ref(),
            true,
        )?;
        progress.phase("waiting_resource");
        let _write = if matches!(
            job.tool.as_str(),
            "code_apply_edits" | "change_resume" | "files_sync"
        ) {
            progress.phase("waiting_resource");
            Some(self.scheduler.writes.acquire(&ws.root).await?)
        } else {
            None
        };
        let _terminal = if job.tool == "terminal_input" {
            Some(
                self.scheduler
                    .terminals
                    .lock(files::text(v, "terminal_id")?)
                    .await?,
            )
        } else {
            None
        };
        // Resource waiters must not consume capacity intended for runnable work.
        // Controls never take the terminal-input lock, so cancellation stays independent.
        let permit = self.scheduler.acquire(Lane::for_tool(&job.tool)).await?;
        progress.phase("running");
        let history_access = if matches!(
            job.tool.as_str(),
            "terminal_read" | "change_resume" | "workspace_gc"
        ) {
            Some(self.history_access.clone().read_owned().await)
        } else {
            None
        };
        match job.tool.as_str() {
            "file_upload" | "file_download" => {
                crate::durable_transfer::run(
                    &self.config,
                    &ws,
                    job,
                    progress,
                    &self.store,
                    self.scheduler.writes.clone(),
                )
                .await
            }
            "workspace_context" => {
                if crate::maintenance_tasks::requested(&job.tool, v) {
                    crate::maintenance_tasks::read(&self.config, &ws, &job.owner, v)
                } else {
                    crate::workspace_context::read(&self.store, &self.config, &ws, v).await
                }
            }
            "workspace_gc" => crate::storage_gc::run(&self.config, &self.store, &ws, v).await,
            "terminal_exec" => {
                if crate::maintenance_tasks::requested(&job.tool, v) {
                    let launch = crate::maintenance_tasks::launch(
                        &self.config,
                        &ws,
                        &job.owner,
                        v,
                        &job.id,
                    )?;
                    self.terminals
                        .start_maintenance(&self.config, &ws, v, &job.id, launch)
                        .await
                } else {
                    self.terminals.start(&self.config, &ws, v, &job.id).await
                }
            }
            "terminal_read" => self.terminals.read(&ws, v).await,
            "terminal_input" => self.terminals.input(&ws, v).await,
            "terminal_cancel" => self.terminals.cancel(&ws, v).await,
            "code_diff" => crate::change_review::read(&ws, v).await,
            "change_resume" => {
                let original = files::text(v, "change_set_id")?;
                uuid::Uuid::parse_str(original).context("invalid change_set_id")?;
                let op: LocalOperation = self
                    .store
                    .get("local_operation", original)
                    .await?
                    .context("change_set_unavailable: original local operation not found")?;
                ensure!(
                    op.tool.as_deref() == Some("code_apply_edits")
                        && op.workspace_id.as_deref() == Some(&ws.id),
                    "change_set_unavailable: original edit belongs to another workspace or tool"
                );
                ensure!(
                    !(op.gateway_accepted && op.result.is_none()),
                    "history_expired: completed edit journal was retired; do not replay the original edit"
                );
                let journal = self
                    .config
                    .state_dir
                    .join("edits")
                    .join(format!("{original}.json"));
                let ws = ws.clone();
                let progress = progress.clone();
                tokio::task::spawn_blocking(move || {
                    let _history_access = history_access;
                    let _write_guard = _write;
                    let _execution_permit = permit;
                    progress.phase("running");
                    files::resume(&ws, &journal)
                })
                .await?
            }
            name => {
                let name = name.to_owned();
                let v = v.clone();
                let journal = self
                    .config
                    .state_dir
                    .join("edits")
                    .join(format!("{}.json", job.id));
                let progress = progress.clone();
                tokio::task::spawn_blocking(move || {
                    // Keep the write guard until the actual blocking edit finishes,
                    // even if its async observer is cancelled.
                    let _write_guard = _write;
                    let _execution_permit = permit;
                    progress.phase("running");
                    match name.as_str() {
                        "code_list" => files::list(&ws, &v),
                        "code_search" => files::search(&ws, &v),
                        "code_read" => files::read(&ws, &v),
                        "code_symbols" => files::symbols(&ws, &v),
                        "code_apply_edits" => files::apply(&ws, &v, &journal),
                        "files_sync" => crate::files_sync::run(&ws, &v, &journal),
                        _ => bail!("unsupported device tool"),
                    }
                })
                .await?
            }
        }
    }
    pub async fn run(&self) -> Result<()> {
        let result = tokio::try_join!(
            self.run_loop(None),
            crate::history_retention::agent_loop(self.config.clone(), self.history_access.clone())
        );
        #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
        self.desktop_workers.revoke(None).await;
        result?;
        Ok(())
    }
    /// Run the real device lifecycle with Skill files isolated below this
    /// instance's state directory. Test/embedded callers must not overwrite
    /// the operator's installed Codex or Gemini Skill files.
    pub async fn run_isolated(&self) -> Result<()> {
        let home = self.config.state_dir.join("isolated-skill-home");
        let result = tokio::try_join!(
            self.run_loop(Some(&home)),
            crate::history_retention::agent_loop(self.config.clone(), self.history_access.clone())
        );
        #[cfg(any(target_os = "linux", target_os = "macos", windows, test))]
        self.desktop_workers.revoke(None).await;
        result?;
        Ok(())
    }
    async fn run_loop(&self, skill_home: Option<&std::path::Path>) -> Result<()> {
        let client = liveness::network_client()?;
        let delivery = Delivery::new(self.store.clone(), self.config.clone()).await?;
        let skill_sync = match skill_home {
            Some(home) => crate::capabilities::sync_embedded_skill_at(home),
            None => crate::capabilities::sync_embedded_skill(),
        };
        if let Err(error) = skill_sync {
            tracing::error!(
                ?error,
                "failed to synchronize embedded Agent Skill; fleet convergence will remain false"
            );
        }
        let (skill_revision, skill_consistent) = match skill_home {
            Some(home) => crate::capabilities::installed_skill_revision_at(home),
            None => crate::capabilities::installed_skill_revision(),
        };
        let hello = DeviceHello {
            version: env!("CARGO_PKG_VERSION").into(),
            wire_protocol: Some(crate::capabilities::WIRE_PROTOCOL),
            tool_schema_revision: Some(crate::capabilities::tool_schema_revision().to_owned()),
            skill_revision,
            skill_consistent,
            platform: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            home_dir: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|v| std::path::PathBuf::from(v).to_string_lossy().into_owned()),
            session: crate::random(),
            roots: self
                .config
                .roots
                .iter()
                .map(|p| p.to_string_lossy().into())
                .collect(),
            allow_write: self.config.allow_write,
            allow_exec: self.config.allow_exec,
            transfer_limits: Some(crate::capabilities::TransferLimits::current()),
            maintenance_tasks: crate::maintenance_tasks::current(self.config.allow_exec),
        };
        // A launchd process marker is not readiness. Only successful authenticated
        // poll round-trips populate the per-lane timestamps for this exact session.
        self.store
            .put(
                "runtime",
                "readiness",
                &json!({"pid":std::process::id(),
            "session":hello.session,"version":hello.version,"started_at":crate::now(),
            "phase":"gateway_wait","lanes":{}}),
                i64::MAX,
            )
            .await?;
        self.readiness.start(&hello.session)?;
        let mut attempt = 0u32;
        loop {
            let health = async {
                client
                    .get(format!("{}/healthz", self.config.gateway_url))
                    .send()
                    .await?
                    .error_for_status()?
                    .json::<Value>()
                    .await
            }
            .await;
            if let Ok(v) = &health {
                let gateway_wire = v["wire_protocol"].as_u64().unwrap_or(0) as u32;
                if gateway_wire < crate::capabilities::MIN_GATEWAY_WIRE_PROTOCOL {
                    self.store.put("runtime", "gateway_compatibility", &json!({
                        "state":"gateway_version_incompatible",
                        "agent_version":env!("CARGO_PKG_VERSION"),
                        "agent_wire_protocol":crate::capabilities::WIRE_PROTOCOL,
                        "gateway_version":v["version"],
                        "gateway_wire_protocol":gateway_wire,
                        "required_gateway_wire_protocol":crate::capabilities::MIN_GATEWAY_WIRE_PROTOCOL,
                        "observed_at":crate::now()
                    }), i64::MAX).await?;
                    attempt = attempt.saturating_add(1);
                    tracing::error!(
                        gateway_wire,
                        required = crate::capabilities::MIN_GATEWAY_WIRE_PROTOCOL,
                        "gateway_version_incompatible; upgrade gateway before this agent"
                    );
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    continue;
                }
                if v["dispatch_protocol"] == 2
                    && v["resource_dispatch_protocol"] == 1
                    && v["transfer_protocol"] == 2
                {
                    self.store
                        .put(
                            "runtime",
                            "gateway_compatibility",
                            &json!({
                                "state":"compatible","gateway_version":v["version"],
                                "gateway_wire_protocol":gateway_wire,"observed_at":crate::now()
                            }),
                            i64::MAX,
                        )
                        .await?;
                    break;
                }
            }
            attempt = attempt.saturating_add(1);
            // No URL, body or credentials in diagnostics. Stay in this process on
            // transient startup outages instead of creating a launchd restart loop.
            tracing::warn!(
                attempt,
                "gateway not ready or protocol incompatible; retrying startup"
            );
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(4)))).await;
        }
        tokio::try_join!(
            self.run_lane(&client, &hello, Lane::Read, &delivery),
            self.run_lane(&client, &hello, Lane::Write, &delivery),
            self.run_lane(&client, &hello, Lane::Transfer, &delivery),
            self.run_lane(&client, &hello, Lane::Terminal, &delivery),
            self.run_lane(&client, &hello, Lane::Control, &delivery),
            self.heartbeat_loop(&client, &hello),
            self.keepalive_loop(&hello),
            delivery.run(&client),
        )?;
        Ok(())
    }
    async fn run_lane(
        &self,
        client: &reqwest::Client,
        hello: &DeviceHello,
        lane: Lane,
        delivery: &Delivery,
    ) -> Result<()> {
        let mut workers = tokio::task::JoinSet::new();
        let mut health = crate::poll_health::Health::default();
        let mut poll_client = client.clone();
        loop {
            while let Some(result) = workers.try_join_next() {
                Self::worker_result(result);
            }
            if lane != Lane::Control && crate::auto_upgrade::is_upgrade_draining() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            if workers.len() >= lane.limit() {
                if let Some(result) = workers.join_next().await {
                    Self::worker_result(result);
                }
                continue;
            }
            let polled = self.poll_job(&poll_client, hello, lane).await;
            if polled.is_ok()
                && let Some(event) = health.succeeded(std::time::Instant::now(), crate::now())
            {
                tracing::info!(?lane, details=%event, "device polling recovered");
                self.save_poll_health(hello, lane, event).await;
            }
            match polled {
                Ok(Some(job)) => {
                    // Reserve the job's canonical scope before the next poll,
                    // not after its worker happens to get CPU time.
                    let resource = self.resource_hint(&job).await;
                    let Some(lease) = self.active.enter_scoped(&job.id, resource)? else {
                        continue;
                    };
                    let agent = self.clone();
                    let delivery = delivery.clone();
                    workers.spawn(async move {
                        let _lease = lease;
                        agent.execute_report(&delivery, job).await
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    let failure = crate::poll_health::Failure::classify(&error);
                    // A timed-out pooled connection must not pin every retry to
                    // the same failed HTTP/2 session. This replaces transport,
                    // not the device identity, origin, job or durable receipt.
                    if matches!(failure.stage, "poll_send" | "poll_receive")
                        && let Ok(fresh) = liveness::network_client()
                    {
                        poll_client = fresh;
                    }
                    let (delay, event) =
                        health.failed(failure, std::time::Instant::now(), crate::now());
                    if let Some(event) = event {
                        // Typed fields only. Never format the raw HTTP error, URL,
                        // token or response body into a console or persistent log.
                        tracing::warn!(?lane, details=%event, "device polling unavailable; retrying observation only");
                        self.save_poll_health(hello, lane, event).await;
                    }
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
    async fn save_poll_health(&self, hello: &DeviceHello, lane: Lane, mut event: Value) {
        let lane = serde_json::to_value(lane).expect("static lane");
        event["lane"] = lane.clone();
        event["session"] = json!(hello.session);
        event["version"] = json!(env!("CARGO_PKG_VERSION"));
        let key = format!("poll_health_{}", lane.as_str().expect("lane string"));
        // A cancelled SQLite future can still commit later. Fence optional
        // telemetry by runtime session and revision before bounding its wait.
        let revision = self
            .poll_health_revision
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        event["publication_revision"] = json!(revision);
        let value = event.to_string();
        let update = sqlx::query("INSERT INTO kv(kind,key,value,expires) SELECT 'runtime',?,?,? WHERE EXISTS(SELECT 1 FROM kv WHERE kind='runtime' AND key='readiness' AND json_extract(value,'$.session')=?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value,expires=excluded.expires WHERE json_extract(kv.value,'$.session') IS NOT ? OR COALESCE(json_extract(kv.value,'$.publication_revision'),0)<?")
            .bind(&key).bind(value).bind(i64::MAX).bind(&hello.session)
            .bind(&hello.session).bind(revision as i64).execute(&self.store.pool);
        if !matches!(
            tokio::time::timeout(Duration::from_millis(250), update).await,
            Ok(Ok(_))
        ) {
            tracing::warn!(
                "poll health persistence unavailable; bounded log retains the transition"
            );
        }
    }
    fn worker_result(result: std::result::Result<Result<()>, tokio::task::JoinError>) {
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!("device worker failed; durable journal retained for recovery");
        }
    }
    async fn heartbeat_loop(&self, client: &reqwest::Client, hello: &DeviceHello) -> Result<()> {
        self.heartbeat_with_period(client, hello, Duration::from_secs(2))
            .await
    }
    async fn heartbeat_with_period(
        &self,
        client: &reqwest::Client,
        hello: &DeviceHello,
        period: Duration,
    ) -> Result<()> {
        let mut terminal_changes = self.terminals.subscribe_changes();
        let mut periodic = tokio::time::interval(period);
        periodic.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut acknowledged_terminal_fingerprint = String::new();
        let mut terminal_snapshot = crate::terminal_sync::SnapshotCache::default();
        let mut last_reconcile = tokio::time::Instant::now() - Duration::from_secs(30);
        let mut last_acknowledged = tokio::time::Instant::now() - Duration::from_secs(30);
        loop {
            tokio::select! {
                changed = terminal_changes.changed() => {
                    changed.context("terminal_change_channel_closed")?;
                    // Merge concurrent exits without spinning or opening a second
                    // heartbeat request. Output streaming keeps its periodic path.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                _ = periodic.tick() => {}
            }
            // Mark BEFORE reading/sending. An exit during an in-flight request
            // must wake the following iteration, not disappear with that reply.
            let revision = *terminal_changes.borrow_and_update();
            if self
                .readiness
                .flush(&self.store, &hello.session)
                .await
                .is_err()
            {
                tracing::warn!(
                    "readiness snapshot unavailable; original poll facts retained, work continues"
                );
            }
            let active = self.active.list()?;
            if last_reconcile.elapsed() >= Duration::from_secs(15) {
                match self.terminals.reconcile_orphans(30, &active).await {
                    Ok(count) if count > 0 => {
                        tracing::warn!(count, "reconciled orphaned terminal state")
                    }
                    Ok(_) => {}
                    Err(_) => tracing::warn!("terminal state reconciliation failed; will retry"),
                }
                last_reconcile = tokio::time::Instant::now();
            }
            if terminal_snapshot
                .refresh(
                    &self.store,
                    self.config.state_dir.join("terminals"),
                    revision,
                    self.terminals.has_live()?,
                )
                .await
                .is_err()
            {
                // Unavailable is not an authoritative empty snapshot. Keep the
                // old cache dirty; independent keepalive still renews the lease.
                tracing::warn!("terminal snapshot unavailable; retained for next observation");
                continue;
            }
            let terminals = &terminal_snapshot.terminals;
            let terminal_previews = &terminal_snapshot.previews;
            let progress = self.progress.snapshots();
            let mut progress_identity = serde_json::to_value(&progress)?;
            if let Some(items) = progress_identity.as_array_mut() {
                for item in items {
                    if let Some(object) = item.as_object_mut() {
                        for key in ["elapsed_ms", "average_bps", "instantaneous_bps"] {
                            object.remove(key);
                        }
                    }
                }
            }
            let fingerprint = crate::hash(serde_json::to_vec(&json!({
                "terminals":terminals,"previews":terminal_previews,
                "active":active,"progress":progress_identity,
            }))?);
            let running_terminal = terminals
                .iter()
                .any(|s| matches!(s.state.as_str(), "starting" | "running"));
            if fingerprint == acknowledged_terminal_fingerprint
                && ((active.is_empty() && !running_terminal)
                    || last_acknowledged.elapsed() < Duration::from_secs(10))
            {
                continue;
            }
            let mut request = serde_json::to_value(hello)?;
            request["active_operations"] = json!(active);
            request["progress"] = json!(progress);
            request["terminal_updates"] = json!(terminals);
            request["terminal_previews"] = json!(terminal_previews);
            let contact_started = tokio::time::Instant::now();
            let response = client
                .post(format!("{}/device/heartbeat", self.config.gateway_url))
                .bearer_auth(&self.config.device_token)
                .json(&request)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
            if let Ok(resp) = response
                && resp.status().is_success()
            {
                crate::auto_upgrade::check_and_spawn_agent_upgrade(self, client, resp.headers());
                acknowledged_terminal_fingerprint = fingerprint;
                last_acknowledged = tokio::time::Instant::now();
                self.liveness.observed(contact_started);
            }
        }
    }
    async fn resource_hint(&self, job: &Job) -> ActiveResource {
        let mut resource = ActiveResource::default();
        if matches!(
            job.tool.as_str(),
            "code_apply_edits" | "change_resume" | "files_sync"
        ) && let Some(id) = job.arguments.get("workspace_id").and_then(Value::as_str)
            && let Ok(Some(ws)) = self.store.get::<Workspace>("workspace", id).await
            && ws.device_id == self.config.device_id
        {
            resource.write_root = Some(ws.root);
        }
        if job.tool == "terminal_input"
            && let Some(id) = job.arguments.get("terminal_id").and_then(Value::as_str)
            && uuid::Uuid::parse_str(id).is_ok()
        {
            resource.terminal_input = Some(id.to_owned());
        }
        resource
    }
    async fn resource_filter(&self, lane: Lane) -> Result<ResourceFilter> {
        let mut filter = ResourceFilter::default();
        if !matches!(lane, Lane::Write | Lane::Terminal) {
            return Ok(filter);
        }
        let active = self.active.resources()?;
        if lane == Lane::Terminal {
            filter.terminal_inputs = active
                .into_iter()
                .filter_map(|r| r.terminal_input)
                .collect();
            return Ok(filter);
        }
        let mut roots = self.scheduler.writes.roots()?;
        roots.extend(active.into_iter().filter_map(|r| r.write_root));
        roots.sort();
        roots.dedup();
        if roots.is_empty() {
            return Ok(filter);
        }
        // Resolve aliases and ancestors using canonical stored roots, not guessed
        // logical IDs. Path-component separators prevent a/foo blocking a/foobar.
        let separator = std::path::MAIN_SEPARATOR.to_string();
        let rows: Vec<(String,)> = sqlx::query_as("SELECT key FROM kv WHERE kind='workspace' AND expires>? AND json_extract(value,'$.device_id')=? AND EXISTS (SELECT 1 FROM json_each(?) AS busy WHERE instr(rtrim(json_extract(kv.value,'$.root'),?) || ?, rtrim(busy.value,?) || ?)=1 OR instr(rtrim(busy.value,?) || ?, rtrim(json_extract(kv.value,'$.root'),?) || ?)=1) ORDER BY key LIMIT 1025")
            .bind(crate::now()).bind(&self.config.device_id).bind(serde_json::to_string(&roots)?)
            .bind(&separator).bind(&separator).bind(&separator).bind(&separator)
            .bind(&separator).bind(&separator).bind(&separator).bind(&separator)
            .fetch_all(&self.store.pool).await?;
        if rows.len() > 1024 {
            // Explicit bounded fallback for pathological alias counts. Reads and
            // controls stay available; no partial alias list is silently trusted.
            filter.all_writes = true;
        } else {
            filter.write_workspaces = rows.into_iter().map(|(id,)| id).collect();
        }
        Ok(filter)
    }
    async fn poll_job(
        &self,
        client: &reqwest::Client,
        hello: &DeviceHello,
        lane: Lane,
    ) -> Result<Option<Job>> {
        let mut request = serde_json::to_value(hello)?;
        request["lanes"] = json!([lane]);
        let filter = self.resource_filter(lane).await?;
        request["poll_wait_ms"] = json!(if filter.is_empty() { 20000 } else { 250 });
        request["resource_filter"] = serde_json::to_value(filter)?;
        if !self
            .legacy_feature_advertisement
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            request["runtime_features"] = json!(crate::capabilities::RuntimeFeatures::current());
        }
        // Heartbeat owns terminal-state replication. Independent poll lanes must
        // not repeat the same SQLite snapshot on every long poll.
        request["receipt_delivery"] = json!(
            tokio::time::timeout(
                Duration::from_millis(250),
                Delivery::status(&self.store, &self.config),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .filter(crate::delivery::Status::valid)
        );
        request["active_operations"] = json!(self.active.list()?);
        request["progress"] = json!(self.progress.snapshots());
        let contact_started = tokio::time::Instant::now();
        let mut response = client
            .post(format!("{}/device/poll", self.config.gateway_url))
            .bearer_auth(&self.config.device_token)
            .json(&request)
            .send()
            .await
            .map_err(|error| crate::poll_health::Failure::transport("poll_send", &error))?;
        // A confirmed 400 did not dispatch a job. Retry registration once with
        // only optional feature metadata omitted; never retry authentication or transport failure.
        if response.status() == reqwest::StatusCode::BAD_REQUEST
            && request.get("runtime_features").is_some()
        {
            request
                .as_object_mut()
                .expect("hello object")
                .remove("runtime_features");
            let fallback = client
                .post(format!("{}/device/poll", self.config.gateway_url))
                .bearer_auth(&self.config.device_token)
                .json(&request)
                .send()
                .await
                .map_err(|error| crate::poll_health::Failure::transport("poll_send", &error))?;
            if fallback.status().is_success() {
                self.legacy_feature_advertisement
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    "legacy Gateway accepted registration without optional runtime feature metadata"
                );
            }
            response = fallback;
        }
        if !response.status().is_success() {
            return Err(crate::poll_health::Failure::http(
                response.status().as_u16(),
                response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok()),
            )
            .into());
        }
        crate::auto_upgrade::check_and_spawn_agent_upgrade(self, client, response.headers());
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| crate::poll_health::Failure::transport("poll_receive", &error))?
        {
            if bytes.len().saturating_add(chunk.len()) > crate::payload_budget::POLL_BYTES {
                return Err(
                    crate::poll_health::Failure::at("response_too_large", "poll_receive").into(),
                );
            }
            bytes.extend_from_slice(&chunk);
        }
        let response: Value = serde_json::from_slice(&bytes)
            .map_err(|_| crate::poll_health::Failure::at("invalid_json", "poll_decode"))?;
        if response.get("job").is_none() {
            return Err(
                crate::poll_health::Failure::at("missing_job_field", "poll_validate").into(),
            );
        }
        let job = if response["job"].is_null() {
            None
        } else {
            let job: Job = serde_json::from_value(response["job"].clone())
                .map_err(|_| crate::poll_health::Failure::at("invalid_job", "poll_validate"))?;
            if Lane::for_tool(&job.tool) != lane || job.device_id != self.config.device_id {
                return Err(crate::poll_health::Failure::at(
                    "job_identity_mismatch",
                    "poll_validate",
                )
                .into());
            }
            Some(job)
        };
        let lane_name = serde_json::to_value(lane)?
            .as_str()
            .context("invalid lane")?
            .to_owned();
        // Poll lanes never wait for telemetry writes. The existing heartbeat
        // merges these real acknowledgements; failed lanes keep their old time.
        if self
            .readiness
            .record(&hello.session, &lane_name, crate::now())
            .is_err()
        {
            tracing::warn!("readiness cache unavailable; work continues");
        }
        self.liveness.observed(contact_started);
        Ok(job)
    }
    async fn execute_report(&self, delivery: &Delivery, wire: Job) -> Result<()> {
        if wire.arguments.get("code_payload").is_none() {
            self.execute_with_delivery(&wire, Some(delivery)).await?;
            return Ok(());
        }
        let payload_lock = format!("code-payload:{}", wire.id);
        let _payload_guard = self.scheduler.operations.lock(&payload_lock).await?;
        let metadata = &wire.arguments["code_payload"];
        // A redelivery after completion must not fetch cleaned temporary data.
        {
            let _guard = self.scheduler.operations.lock(&wire.id).await?;
            if let Some(op) = self
                .store
                .get::<LocalOperation>("local_operation", &wire.id)
                .await?
            {
                ensure!(
                    metadata["job_sha256"] == op.fingerprint,
                    "operation fingerprint conflict"
                );
                if !op.gateway_accepted {
                    let result = op.result.unwrap_or_else(
                        || json!({"error":"outcome_unknown","operation_id":wire.id}),
                    );
                    delivery.enqueue(&wire.id, &result).await?;
                }
                return Ok(());
            }
        }
        let job = match crate::code_payload::hydrate(
            &self.config,
            &self.store,
            self.scheduler.writes.clone(),
            &wire,
        )
        .await
        {
            Ok(job) => job,
            Err(error)
                if error
                    .downcast_ref::<crate::durable_transfer::Fault>()
                    .is_some_and(|fault| {
                        fault.state == "paused" && fault.code == "network_retry_exhausted"
                    }) =>
            {
                // Same operation redelivery resumes its verified checkpoint.
                return Err(error);
            }
            Err(_) => {
                self.reject_payload_receive(delivery, &wire).await?;
                crate::code_payload::cleanup(&self.config, &self.store, &wire.id).await?;
                return Ok(());
            }
        };
        if metadata["reconcile_only"] == true {
            self.reconcile_payload(delivery, &job).await?;
        } else {
            self.execute_with_delivery(&job, Some(delivery)).await?;
        }
        crate::code_payload::cleanup(&self.config, &self.store, &job.id).await?;
        Ok(())
    }
    async fn reject_payload_receive(&self, delivery: &Delivery, wire: &Job) -> Result<()> {
        let _guard = self.scheduler.operations.lock(&wire.id).await?;
        let fingerprint = files::text(&wire.arguments["code_payload"], "job_sha256")?.to_owned();
        ensure!(
            crate::transfers::valid_hash(&fingerprint),
            "invalid payload operation fingerprint"
        );
        if let Some(op) = self
            .store
            .get::<LocalOperation>("local_operation", &wire.id)
            .await?
        {
            ensure!(
                op.fingerprint == fingerprint,
                "operation fingerprint conflict"
            );
            if !op.gateway_accepted {
                let result = op
                    .result
                    .unwrap_or_else(|| json!({"error":"outcome_unknown","operation_id":wire.id}));
                delivery.enqueue(&wire.id, &result).await?;
            }
            return Ok(());
        }
        let result = json!({"error":"payload_receive_failed","error_code":"payload_receive_failed",
            "operation_id":wire.id,"failure_boundary":"device_content_receive","execution_state":"not_started",
            "outcome":"not_executed","retry_policy":"observe_original_receipt_before_resubmitting"});
        self.store
            .put(
                "local_operation",
                &wire.id,
                &LocalOperation {
                    fingerprint,
                    state: "done".into(),
                    result: Some(result.clone()),
                    gateway_accepted: false,
                    resumable: false,
                    workspace_id: wire.arguments["workspace_id"].as_str().map(str::to_owned),
                    tool: Some(wire.tool.clone()),
                    updated_at: crate::now(),
                },
                i64::MAX,
            )
            .await?;
        delivery.enqueue(&wire.id, &result).await?;
        Ok(())
    }
    async fn reconcile_payload(&self, delivery: &Delivery, job: &Job) -> Result<()> {
        let _guard = self.scheduler.operations.lock(&job.id).await?;
        let fingerprint = crate::hash(serde_json::to_vec(job)?);
        if let Some(op) = self
            .store
            .get::<LocalOperation>("local_operation", &job.id)
            .await?
        {
            ensure!(
                op.fingerprint == fingerprint,
                "operation fingerprint conflict"
            );
            if !op.gateway_accepted {
                let result = op
                    .result
                    .unwrap_or_else(|| json!({"error":"outcome_unknown","operation_id":job.id}));
                delivery.enqueue(&job.id, &result).await?;
            }
            return Ok(());
        }
        // This is a device acknowledgement of non-acceptance under the same
        // operation lock used by execution, never a Gateway timeout guess.
        let mut result = crate::diagnostics::error(
            &job.tool,
            "payload_too_large: original request never accepted",
            Some(&job.id),
            "agent_execute",
        );
        result["device_acceptance"] = json!("not_accepted");
        result["acceptance_evidence"] = json!("durable_agent_journal_checked_under_operation_lock");
        result["request_fingerprint"] = json!(fingerprint);
        self.store
            .put(
                "local_operation",
                &job.id,
                &LocalOperation {
                    fingerprint,
                    state: "done".into(),
                    result: Some(result.clone()),
                    gateway_accepted: false,
                    resumable: false,
                    workspace_id: job.arguments["workspace_id"].as_str().map(str::to_owned),
                    tool: Some(job.tool.clone()),
                    updated_at: crate::now(),
                },
                i64::MAX,
            )
            .await?;
        delivery.enqueue(&job.id, &result).await?;
        Ok(())
    }
}
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.into())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(test)]
#[path = "agent_payload_tests.rs"]
mod payload_tests;
#[cfg(test)]
#[path = "agent_scheduling_tests.rs"]
mod scheduling_tests;
#[cfg(all(test, unix))]
#[path = "terminal_completion_tests.rs"]
mod terminal_completion_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn change_resume_routes_only_to_original_workspace_change_set() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let device = uuid::Uuid::new_v4().to_string();
        let a = Agent::new(AgentConfig {
            gateway_url: "https://example.com:8443".into(),
            device_id: device.clone(),
            device_token: crate::random(),
            state_dir: state.path().into(),
            roots: vec![root.path().into()],
            allow_write: true,
            allow_exec: false,
            shell: "/bin/sh".into(),
        })
        .await
        .unwrap();
        let ws = Workspace {
            id: format!("{}:{}", device, uuid::Uuid::new_v4()),
            device_id: device.clone(),
            root: root.path().canonicalize().unwrap(),
        };
        a.store
            .put("workspace", &ws.id, &ws, i64::MAX)
            .await
            .unwrap();
        let original = Job {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: device.clone(),
            owner: "owner".into(),
            tool: "code_apply_edits".into(),
            arguments: json!({"workspace_id":ws.id,"idempotency_key":"edit","files":[{"path":"a.txt","expected_version":"absent","action":"create","content":"hello"}]}),
        };
        let first = a.execute(&original).await.unwrap();
        assert_eq!(first["state"], "completed");
        assert_eq!(first["change_set"]["change_set_id"], original.id);
        let resume = Job {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: device,
            owner: "owner".into(),
            tool: "change_resume".into(),
            arguments: json!({"workspace_id":ws.id,"idempotency_key":"resume","change_set_id":original.id}),
        };
        let second = a.execute(&resume).await.unwrap();
        assert_eq!(second["state"], "completed");
        assert_eq!(second["already_applied"], 1);
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "hello"
        );
    }
    #[tokio::test]
    async fn late_poll_after_receipt_acceptance_cannot_repeat_a_mutation() {
        use axum::{Json, Router, routing::post};
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/device/result",
                    post(|| async { Json(json!({"accepted":true})) }),
                ),
            )
            .await
            .unwrap();
        });
        let mut a = Agent::new(AgentConfig {
            gateway_url: "https://fixture.invalid".into(),
            device_id: uuid::Uuid::new_v4().to_string(),
            device_token: crate::random(),
            state_dir: state.path().into(),
            roots: vec![root.path().into()],
            allow_write: true,
            allow_exec: false,
            shell: "/bin/sh".into(),
        })
        .await
        .unwrap();
        // The production constructor must continue to reject HTTP origins.
        // Only this in-module fixture redirects delivery to its loopback server.
        Arc::make_mut(&mut a.config).gateway_url = gateway_url;
        let delivery = Delivery::new(a.store.clone(), a.config.clone())
            .await
            .unwrap();
        let sending = delivery.clone();
        let sender = tokio::spawn(async move { sending.run(&reqwest::Client::new()).await });
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: a.config.device_id.clone(),
            owner: "owner".into(),
            tool: "workspace_open".into(),
            arguments: json!({"device_id":a.config.device_id,"root":root.path(),"idempotency_key":"late-poll"}),
        };
        a.execute_with_delivery(&job, Some(&delivery))
            .await
            .unwrap();
        let accepted = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if a.store
                    .get::<LocalOperation>("local_operation", &job.id)
                    .await
                    .unwrap()
                    .is_some_and(|op| op.gateway_accepted)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        sender.abort();
        let _ = sender.await;
        server.abort();
        assert!(accepted.is_ok(), "receipt acceptance never completed");
        let late = a
            .execute_with_delivery(&job, Some(&delivery))
            .await
            .unwrap();
        assert_eq!(late["state"], "already_completed");
        let (workspaces,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM kv WHERE kind='workspace'")
                .fetch_one(&a.store.pool)
                .await
                .unwrap();
        assert_eq!(
            workspaces, 1,
            "the late request must not create another workspace"
        );
        let (outbox,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM receipt_outbox")
            .fetch_one(&a.store.pool)
            .await
            .unwrap();
        assert_eq!(
            outbox, 0,
            "a local tombstone must not replace the Gateway's original result"
        );
        let saved: Value = a
            .store
            .get("local_operation", &job.id)
            .await
            .unwrap()
            .unwrap();
        assert!(saved.get("result").is_none());
        let mut conflict = job;
        conflict.arguments["idempotency_key"] = json!("different");
        assert!(
            a.execute_with_delivery(&conflict, Some(&delivery))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deduplicates_mutations_and_rejects_other_devices() {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let device = uuid::Uuid::new_v4().to_string();
        let a = Agent::new(AgentConfig {
            gateway_url: "https://example.com:8443".into(),
            device_id: device.clone(),
            device_token: crate::random(),
            state_dir: state.path().into(),
            roots: vec![root.path().into()],
            allow_write: true,
            allow_exec: false,
            shell: "/bin/sh".into(),
        })
        .await
        .unwrap();
        let job = Job {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: device,
            owner: "owner".into(),
            tool: "workspace_open".into(),
            arguments: json!({"device_id":a.config.device_id,"root":root.path(),"idempotency_key":"open"}),
        };
        let first = a.execute(&job).await.unwrap();
        assert_eq!(first, a.execute(&job).await.unwrap());
        let mut bad = job;
        bad.device_id = uuid::Uuid::new_v4().to_string();
        assert!(a.execute(&bad).await.is_err());
    }
}
