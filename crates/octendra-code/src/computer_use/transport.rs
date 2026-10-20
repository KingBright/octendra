//! Fixed-argv private worker pipe. Authority is independent of shell/code grants.
//! The Agent journal owns results/replay; this module owns one short-lived child and pixel cache.
use super::lifecycle::{Tracker, interrupted, process_identity};
use anyhow::{Context, Result, bail, ensure};
use octendra_desktop_protocol::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};
use uuid::Uuid;

const APP_ID: &str = "fun.hackerlife.octendra.desktop-worker";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchApproval {
    pub enabled: bool,
    pub observe: bool,
    pub input: bool,
    pub capability_version: u64,
    pub identity: CapabilityIdentity,
    pub desktop_uid: u32,
    pub desktop_session: String,
    pub logical_size: [u32; 2],
    pub logical_position: [i32; 2],
    pub max_frame_bytes: usize,
    pub worker_path: PathBuf,
    pub worker_sha256: String,
    pub agent_sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerApproval {
    enabled: bool,
    app_id: String,
    desktop_uid: u32,
    desktop_session: String,
    identity: Identity,
    worker_sha256: String,
    agent_sha256: String,
    expires_at_ms: u64,
    logical_size: [u32; 2],
    logical_position: [i32; 2],
    max_frame_bytes: usize,
    allow_input: bool,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[cfg(target_os = "linux")]
fn uid() -> Result<u32> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let values: Vec<u32> = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .context("uid missing")?
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        values.len() == 4 && values.iter().all(|n| *n == values[0]) && values[0] != 0,
        "unprivileged desktop Agent required"
    );
    Ok(values[0])
}
#[cfg(all(test, not(any(target_os = "linux", target_os = "macos"))))]
fn uid() -> Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(std::env::current_exe()?)?.uid())
}
#[cfg(target_os = "macos")]
fn uid() -> Result<u32> {
    let actual = unsafe { nix::libc::getuid() };
    ensure!(
        actual != 0 && actual == unsafe { nix::libc::geteuid() },
        "unprivileged desktop Agent required"
    );
    Ok(actual)
}
fn owned_file(path: &Path, mode: u32, max: u64, owner: u32) -> Result<Vec<u8>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        path.is_absolute()
            && meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.uid() == owner
            && meta.mode() & 0o777 == mode
            && meta.len() <= max,
        "owned regular file required"
    );
    // O_NOFOLLOW is supplied by the existing nix dependency; no shell or links.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)?;
    use std::io::Read;
    let actual = file.metadata()?;
    ensure!(
        actual.ino() == meta.ino() && actual.dev() == meta.dev(),
        "file changed"
    );
    let mut bytes = Vec::new();
    file.by_ref().take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "file bound");
    Ok(bytes)
}
impl LaunchApproval {
    pub fn validate(&self, device: &str) -> Result<()> {
        ensure!(
            self.enabled && self.observe && self.input && self.capability_version > 0,
            "desktop authority unavailable"
        );
        ensure!(
            self.identity.device_id.to_string() == device && !self.identity.owner.is_empty(),
            "desktop authority identity"
        );
        let owner = uid()?;
        ensure!(
            self.desktop_uid == owner
                && self.identity.desktop_id
                    == format!("uid:{owner}:session:{}", self.desktop_session)
                && !self.desktop_session.is_empty()
                && self
                    .desktop_session
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric())
                && self.logical_size.iter().all(|n| *n > 0 && *n <= 8192)
                && self
                    .logical_position
                    .iter()
                    .all(|n| n.unsigned_abs() <= 1_000_000)
                && self.max_frame_bytes > 0
                && self.max_frame_bytes <= MAX_WORKER_PIXELS,
            "desktop capability geometry/identity"
        );
        use std::os::unix::fs::MetadataExt;
        let executable = std::env::current_exe()?;
        ensure!(
            digest(&owned_file(
                &executable,
                std::fs::metadata(&executable)?.mode() & 0o777,
                256 * 1024 * 1024,
                owner
            )?) == self.agent_sha256,
            "Agent hash changed"
        );
        let worker = std::fs::symlink_metadata(&self.worker_path)?;
        ensure!(
            self.worker_path.is_absolute()
                && worker.is_file()
                && !worker.file_type().is_symlink()
                && worker.uid() == owner
                && worker.mode() & 0o022 == 0
                && worker.mode() & 0o111 != 0,
            "worker executable authority"
        );
        ensure!(
            digest(&owned_file(
                &self.worker_path,
                worker.mode() & 0o777,
                256 * 1024 * 1024,
                owner
            )?) == self.worker_sha256,
            "worker hash changed"
        );
        Ok(())
    }
}
fn crate_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |v| v.as_millis() as u64)
}
struct Pipe {
    child: Child,
    worker_pid: Option<u32>,
    open_operation_id: Option<Uuid>,
    exit_evidence: Option<serde_json::Value>,
    input: Option<ChildStdin>,
    output: ChildStdout,
    stderr: Option<tokio::process::ChildStderr>,
    diagnostics: Option<tokio::task::JoinHandle<()>>,
    lifecycle: Option<Tracker>,
}
impl Pipe {
    fn spawn(approval: &LaunchApproval, config: &Path) -> Result<Self> {
        let mut command = Command::new(&approval.worker_path);
        command.args(["--approval"]).arg(config);
        Self::spawn_command(command)
    }
    fn spawn_command(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().context("worker stdin")?;
        let output = child.stdout.take().context("worker stdout")?;
        Ok(Self {
            worker_pid: child.id(),
            open_operation_id: None,
            exit_evidence: None,
            stderr: child.stderr.take(),
            diagnostics: None,
            lifecycle: None,
            child,
            input: Some(input),
            output,
        })
    }
    async fn track(&mut self, tracker: Tracker) -> bool {
        self.lifecycle = Some(tracker.clone());
        let persisted = tracker
            .phase(
                "spawned",
                tracker.snapshot()["operation_id"]
                    .as_str()
                    .and_then(|s| Uuid::parse_str(s).ok())
                    .expect("original operation"),
            )
            .await;
        if let Some(stderr) = self.stderr.take() {
            self.diagnostics = Some(tokio::spawn(async move {
                tracker.read(stderr).await;
            }));
        }
        persisted
    }
    async fn exchange(&mut self, id: Uuid, request: Request) -> Result<(WorkerReply, Vec<u8>)> {
        let is_open = matches!(request, Request::Open { .. });
        if let Some(tracker) = &self.lifecycle {
            tracker.admit(id);
            ensure!(
                tracker.phase("request_send_started", id).await,
                "lifecycle persistence failed"
            );
        }
        ensure!(self.child.try_wait()?.is_none(), "worker already exited");
        let encoded = serde_json::to_vec(&WorkerRequest {
            request_id: id,
            request,
        })?;
        ensure!(encoded.len() <= MAX_WORKER_HEADER, "request header bound");
        let input = self.input.as_mut().context("worker disconnected")?;
        input
            .write_all(&(encoded.len() as u32).to_be_bytes())
            .await?;
        input.write_all(&encoded).await?;
        input.flush().await?;
        if let Some(tracker) = &self.lifecycle {
            ensure!(
                tracker
                    .phase(if is_open { "open_sent" } else { "request_sent" }, id)
                    .await,
                "lifecycle persistence failed"
            );
        }
        let length = self.output.read_u32().await? as usize;
        ensure!(
            length > 0 && length <= MAX_WORKER_HEADER,
            "reply header bound"
        );
        let mut header = vec![0; length];
        self.output.read_exact(&mut header).await?;
        let reply: WorkerReply = serde_json::from_slice(&header)?;
        ensure!(reply.request_id == id, "reply operation conflict");
        let mut bytes = Vec::new();
        if let Some(pixels) = &reply.pixels {
            validate_pixels(pixels)?;
            let Response::Observed { frame } = reply
                .result
                .as_ref()
                .map_err(|_| anyhow::anyhow!("pixels on failed result"))?
            else {
                bail!("pixels without observation")
            };
            ensure!(
                pixels.size == frame.mapping.frame_size,
                "pixel geometry conflict"
            );
            for (length, hash) in pixels.plane_lengths.iter().zip(&pixels.plane_sha256) {
                let start = bytes.len();
                bytes.resize(start + length, 0);
                self.output.read_exact(&mut bytes[start..]).await?;
                ensure!(digest(&bytes[start..]) == *hash, "pixel hash conflict");
            }
        } else {
            ensure!(
                !matches!(reply.result, Ok(Response::Observed { .. })),
                "observation missing real bytes"
            );
        }
        if let Some(tracker) = &self.lifecycle {
            ensure!(
                tracker.phase("reply_received", id).await,
                "lifecycle persistence failed"
            );
        }
        Ok((reply, bytes))
    }
    async fn disconnect(&mut self) -> Result<()> {
        if let (Some(t), Some(id)) = (&self.lifecycle, self.open_operation_id) {
            t.phase("cleanup_started", id).await;
        }
        self.disconnect_child().await
    }
    async fn disconnect_child(&mut self) -> Result<()> {
        use std::os::unix::process::ExitStatusExt;
        self.input.take(); // EOF gives the worker its bounded release/Close path.
        match tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await {
            Ok(Ok(status)) => {
                self.exit_evidence = Some(serde_json::json!({
                    "protocol":1,"pid":self.worker_pid,"reaped":true,
                    "exit_code":status.code(),"signal":status.signal(),
                    "forced_stop":false,"exit_success":status.success()
                }));
                ensure!(status.success(), "worker cleanup outcome unknown");
                Ok(())
            }
            _ => {
                let kill_requested = self.child.start_kill().is_ok();
                let reaped = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
                self.exit_evidence = Some(match reaped {
                    Ok(Ok(status)) => serde_json::json!({
                        "protocol":1,"pid":self.worker_pid,"reaped":true,
                        "exit_code":status.code(),"signal":status.signal(),
                        "forced_stop":true,"kill_request_succeeded":kill_requested,"exit_success":status.success()
                    }),
                    _ => serde_json::json!({
                        "protocol":1,"pid":self.worker_pid,"reaped":false,
                        "exit_code":null,"signal":null,"forced_stop":true,"kill_request_succeeded":kill_requested,"exit_success":false
                    }),
                });
                bail!("worker cleanup outcome unknown")
            }
        }
    }
}
fn validate_pixels(p: &RawPixels) -> Result<()> {
    let [w, h] = p.size;
    ensure!(
        w > 0 && h > 0 && w <= 8192 && h <= 8192 && w % 2 == 0 && h % 2 == 0,
        "pixel dimensions"
    );
    let count = (w as usize)
        .checked_mul(h as usize)
        .context("pixel overflow")?;
    let expected = match p.encoding.as_str() {
        "nv12" => vec![count, count / 2],
        "i420" => vec![count, count / 4, count / 4],
        _ => bail!("unsupported raw pixels"),
    };
    ensure!(
        p.plane_lengths == expected
            && p.plane_sha256.len() == expected.len()
            && expected.iter().sum::<usize>() <= MAX_WORKER_PIXELS,
        "pixel layout/budget"
    );
    ensure!(
        p.plane_sha256
            .iter()
            .all(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit())),
        "pixel hashes"
    );
    Ok(())
}
struct State {
    approval: Option<LaunchApproval>,
    pipe: Option<Pipe>,
    binding: Option<Binding>,
    last: Option<FrameRef>,
    sequence: u64,
    poisoned: bool,
    guard_key: Option<String>,
    lifecycle: Option<Tracker>,
    expires_at_ms: u64,
    worker_config: Option<PathBuf>,
    artifacts: BTreeMap<Uuid, FrameArtifact>,
    lock: Option<std::fs::File>,
    source: Option<(PathBuf, String)>,
    #[cfg(test)]
    fixture: Option<String>,
}
fn desktop_lock_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    return std::env::temp_dir().join(format!(
        "octendra-desktop-{}.lock",
        uid().unwrap_or(u32::MAX)
    ));
    #[cfg(not(target_os = "macos"))]
    PathBuf::from(format!(
        "/run/user/{}/octendra-desktop.lock",
        uid().unwrap_or(u32::MAX)
    ))
}
#[derive(Clone)]
pub struct NativeWorkers {
    state: Arc<Mutex<State>>,
    cache: PathBuf,
    lock_path: PathBuf,
    store: Option<crate::store::Store>,
}
impl NativeWorkers {
    pub fn disabled(state_dir: &Path) -> Self {
        Self {
            store: None,
            cache: state_dir.join("desktop-frames"),
            lock_path: desktop_lock_path(),
            state: Arc::new(Mutex::new(State {
                approval: None,
                pipe: None,
                binding: None,
                last: None,
                sequence: 0,
                poisoned: false,
                guard_key: None,
                lifecycle: None,
                expires_at_ms: 0,
                worker_config: None,
                artifacts: BTreeMap::new(),
                lock: None,
                source: None,
                #[cfg(test)]
                fixture: None,
            })),
        }
    }
    pub fn with_store(mut self, store: crate::store::Store) -> Self {
        self.store = Some(store);
        self
    }
    pub fn load(state_dir: &Path, device: &str) -> Self {
        let result = Self::disabled(state_dir);
        let path = state_dir.join("desktop-launch.json");
        let approval = uid()
            .and_then(|u| owned_file(&path, 0o600, 16384, u))
            .and_then(|b| Ok(serde_json::from_slice::<LaunchApproval>(&b)?))
            .and_then(|a| {
                a.validate(device)?;
                Ok(a)
            })
            .ok();
        // Startup is synchronous, and no references have escaped this constructor.
        let source = approval
            .as_ref()
            .and_then(|_| std::fs::read(&path).ok())
            .map(|b| (path, digest(&b)));
        let mut state = result.state.try_lock().expect("new desktop state");
        state.approval = approval;
        state.source = source;
        drop(state);
        result
    }
    #[cfg(target_os = "macos")]
    pub async fn capabilities(&self) -> Capabilities {
        self.capabilities_with_probe(super::macos_worker::platform_status(), |a| {
            a.validate(&a.identity.device_id.to_string()).is_ok()
                && super::macos_worker::window_scope_ready(
                    &a.desktop_session,
                    a.logical_size,
                    a.logical_position,
                )
        })
        .await
    }
    #[cfg(any(target_os = "macos", test))]
    async fn capabilities_with_probe(
        &self,
        platform: &str,
        target_valid: impl FnOnce(&LaunchApproval) -> bool,
    ) -> Capabilities {
        let state = self.state.lock().await;
        let blocked = |reason: &str| Capabilities {
            backend: reason.into(),
            ..Capabilities::default()
        };
        let Some(a) = &state.approval else {
            return blocked(if platform == "screencapturekit" {
                "window_scoped_authority_required"
            } else {
                platform
            });
        };
        // Use the same current owned-file/hash boundary as dispatch. A readiness
        // read must not preserve a revoked grant or mutate its evidence.
        if state.source.as_ref().is_none_or(|(path, hash)| {
            !owned_file(path, 0o600, 16384, uid().unwrap_or(u32::MAX))
                .is_ok_and(|bytes| digest(&bytes) == *hash)
        }) {
            return blocked("desktop_approval_changed");
        }
        if state.poisoned
            || state
                .lifecycle
                .as_ref()
                .is_some_and(|t| t.snapshot()["persistence_failed"] == true)
        {
            return blocked("outcome_unknown");
        }
        if state.pipe.is_none()
            && let Some(store) = &self.store
        {
            let key =
                digest(format!("{}:{}", a.identity.device_id, a.identity.desktop_id).as_bytes());
            match store.get::<serde_json::Value>("desktop_guard", &key).await {
                Ok(Some(_)) => return blocked("desktop_recovery_required"),
                Err(_) => return blocked("desktop_state_unavailable"),
                Ok(None) => {}
            }
        }
        if !target_valid(a) {
            return blocked("window_target_unavailable");
        }
        if platform != "screencapturekit" {
            return blocked(platform);
        }
        Capabilities {
            available: true,
            backend: "macos_pid_private_window_poc".into(),
            ..Capabilities::default()
        }
    }
    pub async fn dispatch(
        &self,
        id: Uuid,
        identity: &Identity,
        version: u64,
        request: Request,
    ) -> Result<serde_json::Value> {
        let mut state = self.state.lock().await;
        let had_pipe = state.pipe.is_some();
        let result = self
            .dispatch_locked(&mut state, id, identity, version, request)
            .await;
        let result = if result.is_err() && !had_pipe && state.pipe.is_some() {
            state.poisoned = true;
            self.close_state(&mut state).await;
            Ok(serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false}))
        } else {
            result
        };
        let mut result = result?;
        if let Some(tracker) = &state.lifecycle {
            let snapshot = tracker.snapshot();
            if snapshot["persistence_failed"] == true {
                state.poisoned = true;
                self.close_state(&mut state).await;
                result =
                    serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false});
            }
            result["worker_lifecycle"] = state
                .lifecycle
                .as_ref()
                .expect("lifecycle retained")
                .snapshot();
        }
        Ok(result)
    }
    async fn dispatch_locked(
        &self,
        state: &mut State,
        id: Uuid,
        identity: &Identity,
        version: u64,
        request: Request,
    ) -> Result<serde_json::Value> {
        if state
            .lifecycle
            .as_ref()
            .is_some_and(|t| t.snapshot()["persistence_failed"] == true)
        {
            state.poisoned = true;
        }
        let Some(a) = state.approval.clone() else {
            return Ok(serde_json::json!({"error":"unavailable"}));
        };
        #[cfg(test)]
        let fixture = state.fixture.is_some();
        #[cfg(not(test))]
        let fixture = false;
        if !fixture
            && (a.validate(&identity.device_id.to_string()).is_err()
                || state.source.as_ref().is_none_or(|(path, hash)| {
                    owned_file(path, 0o600, 16384, uid().unwrap_or(u32::MAX))
                        .map_or(true, |bytes| digest(&bytes) != *hash)
                }))
        {
            let clean = self.close_state(state).await;
            state.approval = None;
            return Ok(
                serde_json::json!({"error":if clean {"unavailable"}else{"outcome_unknown"},"automatic_replay_safe":false}),
            );
        }
        ensure!(
            a.identity.allows(identity)
                && !identity.task_id.is_nil()
                && version == a.capability_version,
            "desktop authority conflict"
        );
        ensure!(
            request.identity().is_none_or(|i| i == identity),
            "desktop request identity"
        );
        if matches!(request, Request::Capabilities {}) {
            return Ok(serde_json::to_value(unavailable(&request).unwrap())?);
        }
        if let Request::Close { binding } = &request
            && state.pipe.is_none()
            && let Some(result) = self.reconcile_closed_guard(state, binding, version).await?
        {
            return Ok(result);
        }
        if state.poisoned {
            return Ok(
                serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false}),
            );
        }
        if let Request::Open { lease_ms, .. } = &request {
            ensure!(
                state.pipe.is_none() && *lease_ms > 0 && *lease_ms <= MAX_LEASE_MS,
                "desktop busy/lease"
            );
            state.lock = Some(self.acquire_lock()?);
            let key = digest(format!("{}:{}", identity.device_id, identity.desktop_id).as_bytes());
            if let Some(store) = &self.store {
                if let Some(guard) = store
                    .get::<serde_json::Value>("desktop_guard", &key)
                    .await?
                {
                    let evidence = if guard["worker_lifecycle"].is_object() {
                        &guard["worker_lifecycle"]
                    } else {
                        &guard
                    };
                    let parent_lost = interrupted(evidence, &process_identity(std::process::id()));
                    state.poisoned = true;
                    state.guard_key = Some(key);
                    return Ok(
                        serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false,"recovery_reason":if parent_lost {"parent_runtime_interrupted"} else {"unresolved_original_intent"},"original_operation_id":guard["operation_id"]}),
                    );
                }
                // Commit intent before spawning a child or touching the desktop.
                store
                    .put(
                        "desktop_guard",
                        &key,
                        &serde_json::json!({
                            "operation_id":id,"identity":identity,"capability_version":version,
                            "state":"opening_or_active","recorded_at_ms":crate_now_ms(),
                            "parent_identity":process_identity(std::process::id()),"worker_exit":null
                        }),
                        i64::MAX,
                    )
                    .await?;
            }
            state.guard_key = Some(key);
            state.expires_at_ms = crate_now_ms() + 25_000 + u64::from(*lease_ms);
            if !fixture {
                let config = self.write_worker_approval(&a, identity, id, state.expires_at_ms)?;
                state.worker_config = Some(config);
            }
            #[cfg(test)]
            if let Some(source) = &state.fixture {
                let mut command = Command::new("/usr/bin/python3");
                command.args(["-I", "-B", "-u", "-c", source]);
                state.pipe = Some(Pipe::spawn_command(command)?);
            } else {
                state.pipe = Some(Pipe::spawn(
                    &a,
                    state
                        .worker_config
                        .as_deref()
                        .context("worker execution config")?,
                )?);
            }
            #[cfg(not(test))]
            {
                state.pipe = Some(Pipe::spawn(
                    &a,
                    state
                        .worker_config
                        .as_deref()
                        .context("worker execution config")?,
                )?);
            }
            let pipe = state.pipe.as_mut().context("worker missing")?;
            pipe.open_operation_id = Some(id);
            let tracker = Tracker::new(
                id,
                pipe.worker_pid.context("worker PID")?,
                self.store.clone().zip(state.guard_key.clone()),
            );
            state.lifecycle = Some(tracker.clone());
            if !pipe.track(tracker).await {
                state.poisoned = true;
                self.close_state(state).await;
                return Ok(
                    serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false}),
                );
            }
        } else {
            let binding = match &request {
                Request::Observe { binding } | Request::Close { binding } => binding,
                Request::Act { frame, .. } => &frame.binding,
                _ => unreachable!(),
            };
            ensure!(
                state.binding.as_ref() == Some(binding),
                "desktop epoch conflict"
            );
            if let Request::Act {
                frame, sequence, ..
            } = &request
            {
                ensure!(
                    state.last.as_ref() == Some(frame) && *sequence == state.sequence + 1,
                    "stale frame or action sequence"
                );
            }
        }
        if matches!(request, Request::Observe { .. }) {
            self.expire_frames(state);
            ensure!(
                state.artifacts.len() < 2
                    && state.artifacts.values().map(|a| a.bytes).sum::<usize>() + MAX_WORKER_PIXELS
                        <= 2 * MAX_WORKER_PIXELS,
                "desktop frame cache budget"
            );
        }
        let is_close = matches!(request, Request::Close { .. });
        // Worker handling has a 25-second bound after startup. Leave time for
        // private approval validation and delivery of its terminal response.
        let budget = if matches!(request, Request::Open { .. }) {
            35
        } else {
            3
        };
        let response = tokio::time::timeout(
            Duration::from_secs(budget),
            state
                .pipe
                .as_mut()
                .context("worker missing")?
                .exchange(id, request.clone()),
        )
        .await;
        let (reply, bytes) = match response {
            Ok(Ok(v)) => v,
            failure => {
                if let Some(tracker) = &state.lifecycle {
                    tracker
                        .phase(
                            if failure.is_err() {
                                "operation_timeout"
                            } else {
                                "transport_failed"
                            },
                            id,
                        )
                        .await;
                }
                state.poisoned = true;
                self.close_state(state).await;
                return Ok(
                    serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false}),
                );
            }
        };
        let value = match reply.result {
            Ok(value) => value,
            Err(error) => {
                state.poisoned = matches!(error, ErrorCode::OutcomeUnknown);
                let clean = self.close_state(state).await;
                return Ok(
                    serde_json::json!({"error":if clean {error}else{ErrorCode::OutcomeUnknown},"automatic_replay_safe":false}),
                );
            }
        };
        let valid = match (&request, &value) {
            (
                Request::Open { lease_ms, .. },
                Response::Opened {
                    binding,
                    expires_at_ms,
                },
            ) => {
                binding.identity == *identity
                    && !binding.epoch.is_nil()
                    && *expires_at_ms > crate_now_ms()
                    && *expires_at_ms <= state.expires_at_ms
                    && *expires_at_ms <= crate_now_ms() + u64::from(*lease_ms)
            }
            (Request::Observe { binding }, Response::Observed { frame }) => {
                frame.binding == *binding
                    && frame.source_revision > 0
                    && frame.frame_sequence > 0
                    && frame.mapping.rotation == Rotation::R0
                    && frame.mapping.content
                        == ContentRect {
                            left: 0,
                            top: 0,
                            width: frame.mapping.frame_size[0],
                            height: frame.mapping.frame_size[1],
                        }
                    && frame
                        .mapping
                        .logical_size
                        .iter()
                        .all(|n| *n > 0 && *n <= 8192)
                    && state.last.as_ref().is_none_or(|last| {
                        frame.source_revision == last.source_revision
                            && frame.frame_sequence > last.frame_sequence
                            && frame.mapping == last.mapping
                    })
            }
            (
                Request::Act {
                    sequence, actions, ..
                },
                Response::Accepted {
                    sequence: accepted,
                    accepted_actions,
                    effect_confirmed,
                },
            ) => sequence == accepted && *accepted_actions == actions.len() && !effect_confirmed,
            (Request::Close { .. }, Response::Closed {}) => true,
            _ => false,
        };
        if !valid {
            state.poisoned = true;
            self.close_state(state).await;
            return Ok(serde_json::json!({"error":"outcome_unknown"}));
        }
        let mut result = serde_json::to_value(&value)?;
        match value {
            Response::Opened {
                binding,
                expires_at_ms,
            } => {
                if let Some(tracker) = &state.lifecycle
                    && !tracker.bind(&binding).await
                {
                    state.poisoned = true;
                    self.close_state(state).await;
                    return Ok(
                        serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false}),
                    );
                }
                state.binding = Some(binding.clone());
                state.expires_at_ms = expires_at_ms;
                state.sequence = 0;
                let this = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(
                        expires_at_ms.saturating_sub(crate_now_ms()),
                    ))
                    .await;
                    this.close_binding(&binding).await;
                });
            }
            Response::Observed { frame } => {
                let pixels = reply.pixels.context("pixels missing")?;
                let artifact = FrameArtifact {
                    operation_id: id,
                    binding: frame.binding.clone(),
                    capability_version: version,
                    expires_at_ms: state.expires_at_ms,
                    bytes: bytes.len(),
                    sha256: digest(&bytes),
                    pixels,
                };
                if self.write_frame(id, &bytes).is_err() {
                    state.poisoned = true;
                    self.close_state(state).await;
                    return Ok(
                        serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false}),
                    );
                }
                state.artifacts.insert(id, artifact.clone());
                state.last = Some(frame);
                result["frame_artifact"] = serde_json::to_value(artifact)?;
            }
            Response::Accepted { sequence, .. } => state.sequence = sequence,
            _ => {}
        }
        if is_close {
            let clean = self.close_state(state).await;
            if !clean {
                return Ok(
                    serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false}),
                );
            }
        }
        Ok(result)
    }
    // Existing Close only: reconcile durable proof of an already closed owned
    // child. No signal, Portal call, input, restart or original outcome change.
    async fn reconcile_closed_guard(
        &self,
        state: &mut State,
        binding: &Binding,
        version: u64,
    ) -> Result<Option<serde_json::Value>> {
        let Some(store) = &self.store else {
            return Ok(None);
        };
        let key = digest(
            format!(
                "{}:{}",
                binding.identity.device_id, binding.identity.desktop_id
            )
            .as_bytes(),
        );
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM kv WHERE kind='desktop_guard' AND key=?")
                .bind(&key)
                .fetch_optional(&store.pool)
                .await?;
        let Some((raw,)) = row else { return Ok(None) };
        let guard: serde_json::Value = serde_json::from_str(&raw)?;
        let evidence = &guard["worker_lifecycle"];
        let exit = &evidence["worker_exit"];
        let worker = &evidence["worker_identity"];
        let valid = evidence["protocol"] == 1
            && exit["protocol"] == 1
            && guard["identity"] == serde_json::to_value(&binding.identity)?
            && guard["capability_version"] == version
            && !binding.epoch.is_nil()
            && evidence["binding"] == serde_json::to_value(binding)?
            && guard["operation_id"] == evidence["operation_id"]
            && evidence["operation_id"]
                .as_str()
                .is_some_and(|s| Uuid::parse_str(s).is_ok_and(|id| !id.is_nil()))
            && evidence["persistence_failed"] == false
            && evidence["shutdown_status"] == "confirmed"
            && worker["identity_state"] == "observed"
            && worker["start_ticks"].as_u64().is_some_and(|n| n > 0)
            && worker["boot_id"]
                .as_str()
                .is_some_and(|s| Uuid::parse_str(s).is_ok())
            && guard["worker_pid"] == worker["pid"]
            && exit["pid"] == worker["pid"]
            && exit == &guard["worker_exit"]
            && exit["reaped"] == true
            && exit["exit_success"] == true
            && exit["exit_code"] == 0
            && exit["signal"].is_null()
            && exit["forced_stop"] == false;
        let pid = worker["pid"].as_u64().and_then(|n| u32::try_from(n).ok());
        if !valid || pid.is_none_or(|pid| process_identity(pid) == *worker) {
            return Ok(Some(
                serde_json::json!({"error":"outcome_unknown","automatic_replay_safe":false,
                "recovery_reason":"owned_terminal_close_evidence_required","original_operation_id":guard["operation_id"]}),
            ));
        }
        if state.lock.is_none() {
            state.lock = Some(self.acquire_lock()?);
        }
        let removed =
            sqlx::query("DELETE FROM kv WHERE kind='desktop_guard' AND key=? AND value=?")
                .bind(&key)
                .bind(&raw)
                .execute(&store.pool)
                .await?
                .rows_affected();
        ensure!(removed == 1, "desktop close evidence changed");
        state.guard_key = None;
        state.binding = None;
        state.last = None;
        state.poisoned = false;
        state.lifecycle = None;
        if let Some(lock) = state.lock.take() {
            fs2::FileExt::unlock(&lock)?;
        }
        Ok(Some(
            serde_json::json!({"state":"closed","recovered_from_guard":true,
            "original_operation_id":guard["operation_id"],"original_outcome_unchanged":true,
            "worker_lifecycle":evidence}),
        ))
    }
    fn acquire_lock(&self) -> Result<std::fs::File> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let parent =
            std::fs::symlink_metadata(self.lock_path.parent().context("desktop lock parent")?)?;
        ensure!(
            parent.is_dir()
                && !parent.file_type().is_symlink()
                && parent.uid() == uid()?
                && parent.mode() & 0o077 == 0,
            "desktop runtime authority"
        );
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&self.lock_path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == uid()? && meta.mode() & 0o777 == 0o600,
            "desktop lock authority"
        );
        fs2::FileExt::try_lock_exclusive(&file).context("desktop busy across Agents/tasks")?;
        Ok(file)
    }
    fn write_frame(&self, id: Uuid, bytes: &[u8]) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        if !self.cache.exists() {
            std::fs::DirBuilder::new().mode(0o700).create(&self.cache)?;
        }
        let meta = std::fs::symlink_metadata(&self.cache)?;
        ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == uid()?
                && meta.mode() & 0o777 == 0o700,
            "frame cache authority"
        );
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(self.cache.join(id.to_string()))?;
        use std::io::Write;
        if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            let _ = std::fs::remove_file(self.cache.join(id.to_string()));
            return Err(e.into());
        }
        Ok(())
    }
    fn write_worker_approval(
        &self,
        a: &LaunchApproval,
        identity: &Identity,
        id: Uuid,
        expires: u64,
    ) -> Result<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let config = WorkerApproval {
            enabled: true,
            app_id: APP_ID.into(),
            desktop_uid: a.desktop_uid,
            desktop_session: a.desktop_session.clone(),
            identity: identity.clone(),
            worker_sha256: a.worker_sha256.clone(),
            agent_sha256: a.agent_sha256.clone(),
            expires_at_ms: expires,
            logical_size: a.logical_size,
            logical_position: a.logical_position,
            max_frame_bytes: a.max_frame_bytes,
            allow_input: a.input,
        };
        let path = self
            .cache
            .parent()
            .context("state directory")?
            .join(format!("desktop-worker-{id}.json"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(&serde_json::to_vec(&config)?)?;
        file.sync_all()?;
        Ok(path)
    }
    fn expire_frames(&self, state: &mut State) {
        let ids: Vec<_> = state
            .artifacts
            .iter()
            .filter(|(_, a)| a.expires_at_ms <= crate_now_ms())
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            state.artifacts.remove(&id);
            let _ = std::fs::remove_file(self.cache.join(id.to_string()));
        }
    }
    pub async fn retire_artifact(&self, id: Uuid) -> Result<()> {
        let mut state = self.state.lock().await;
        state.artifacts.remove(&id);
        match std::fs::remove_file(self.cache.join(id.to_string())) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn artifact(&self, id: Uuid) -> Result<FrameArtifact> {
        let mut state = self.state.lock().await;
        self.expire_frames(&mut state);
        state
            .artifacts
            .get(&id)
            .cloned()
            .context("desktop frame missing or expired")
    }
    pub async fn read_frame(
        &self,
        artifact: &FrameArtifact,
        identity: &Identity,
        version: u64,
    ) -> Result<Vec<u8>> {
        let mut state = self.state.lock().await;
        self.expire_frames(&mut state);
        let a = state.approval.as_ref().context("desktop unavailable")?;
        ensure!(
            a.enabled
                && a.observe
                && a.identity.allows(identity)
                && a.capability_version == version
                && artifact.binding.identity == *identity
                && artifact.capability_version == version
                && artifact.expires_at_ms > crate_now_ms()
                && state.expires_at_ms > crate_now_ms()
                && state.artifacts.get(&artifact.operation_id) == Some(artifact),
            "frame authorization/expiry"
        );
        #[cfg(not(test))]
        {
            a.validate(&identity.device_id.to_string())?;
            ensure!(
                state.source.as_ref().is_some_and(|(path, hash)| owned_file(
                    path,
                    0o600,
                    16384,
                    uid().unwrap_or(u32::MAX)
                )
                .is_ok_and(|bytes| digest(&bytes) == *hash)),
                "desktop approval revoked"
            );
        }
        validate_pixels(&artifact.pixels)?;
        let bytes = owned_file(
            &self.cache.join(artifact.operation_id.to_string()),
            0o600,
            MAX_WORKER_PIXELS as u64,
            uid()?,
        )?;
        ensure!(
            bytes.len() == artifact.bytes && digest(&bytes) == artifact.sha256,
            "frame artifact changed"
        );
        Ok(bytes)
    }
    pub async fn revoke(&self, binding: Option<&Binding>) -> bool {
        let mut state = self.state.lock().await;
        if binding.is_some_and(|b| state.binding.as_ref() != Some(b)) {
            return false;
        }
        state.approval = None;
        self.close_state(&mut state).await
    }
    async fn close_binding(&self, binding: &Binding) -> bool {
        let mut state = self.state.lock().await;
        if state.binding.as_ref() != Some(binding) {
            return false;
        }
        self.close_state(&mut state).await
    }
    async fn close_state(&self, state: &mut State) -> bool {
        let cache = &self.cache;
        state.binding = None;
        state.last = None;
        let clean = match state.pipe.take() {
            Some(mut pipe) => {
                let clean = pipe.disconnect().await.is_ok();
                if let Some(mut task) = pipe.diagnostics.take()
                    && !matches!(
                        tokio::time::timeout(Duration::from_secs(1), &mut task).await,
                        Ok(Ok(()))
                    )
                {
                    task.abort();
                    let _ = task.await;
                    state.poisoned = true;
                }
                if let (Some(tracker), Some(exit)) = (&pipe.lifecycle, &pipe.exit_evidence)
                    && !tracker.exit(exit.clone()).await
                {
                    state.poisoned = true;
                }
                clean
            }
            None => true,
        };
        state.poisoned |= !clean;
        // Release the owned flock explicitly after confirmed cleanup. A concurrent
        // fork can temporarily inherit its CLOEXEC descriptor until exec; dropping
        // our descriptor alone need not release that open-file-description lock.
        if clean
            && !state.poisoned
            && state
                .lock
                .as_ref()
                .is_some_and(|lock| fs2::FileExt::unlock(lock).is_err())
        {
            state.poisoned = true;
        }
        // Unknown intent survives process loss. Only confirmed owned cleanup removes it.
        if clean && !state.poisoned {
            if let (Some(store), Some(key)) = (&self.store, &state.guard_key)
                && sqlx::query("DELETE FROM kv WHERE kind='desktop_guard' AND key=?")
                    .bind(key)
                    .execute(&store.pool)
                    .await
                    .is_err()
            {
                state.poisoned = true;
            }
            if !state.poisoned {
                state.guard_key = None;
            }
        }
        if let Some(path) = state.worker_config.take() {
            let _ = std::fs::remove_file(path);
        }
        if clean && !state.poisoned {
            state.lock = None;
        }
        for (id, _) in std::mem::take(&mut state.artifacts) {
            let _ = std::fs::remove_file(cache.join(id.to_string()));
        }
        clean && !state.poisoned
    }
}

#[cfg(test)]
#[path = "transport_tests.rs"]
pub(crate) mod tests;
