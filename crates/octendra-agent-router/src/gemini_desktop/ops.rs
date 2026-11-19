//! Embedded `Gemini.app` macOS Accessibility operation state machine and `bridge.swift` supervisor.

#![allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::expect_used
)]

use super::artifact::{Artifact, verify_all_with_root};
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use nix::fcntl::{OFlag, openat};
use nix::sys::signal::{Signal, killpg};
use nix::sys::stat::Mode;
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use uuid::Uuid;

const SWIFT: &str = include_str!("../../../../tools/gemini-mcp/native/bridge.swift");

#[derive(Clone)]
struct ActiveWorkerEntry {
    token: String,
    command: String,
    pid: u32,
}

static ACTIVE_WORKERS: OnceLock<Mutex<HashMap<String, ActiveWorkerEntry>>> = OnceLock::new();

fn active_workers_map() -> &'static Mutex<HashMap<String, ActiveWorkerEntry>> {
    ACTIVE_WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

struct ActiveWorkerGuard {
    op_id: String,
    token: String,
}

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        if let Ok(mut guard) = active_workers_map().lock()
            && guard
                .get(&self.op_id)
                .is_some_and(|entry| entry.token == self.token)
        {
            guard.remove(&self.op_id);
        }
    }
}

fn current_euid() -> u32 {
    nix::unistd::geteuid().as_raw()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionEvidence {
    pub kind: String,
    pub request_id: String,
    pub session_id: String,
    pub text_sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestMode {
    Start,
    FreshAfter,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingAction {
    pub action_id: String,
    pub action_version: String,
    pub kind: String,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl PendingAction {
    fn valid(&self) -> bool {
        let valid_hex = self.action_version.len() == 64
            && self
                .action_version
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
        let valid_research = self.action_id == "deep_research_start"
            && self.kind == "research_start"
            && self.scope == "owned_research_plan";
        let valid_tool_consent = matches!(
            self.action_id.as_str(),
            "tool_consent_allow" | "tool_consent_deny"
        ) && self.kind == "tool_consent"
            && self.scope == "owned_tool_consent";
        valid_hex && (valid_research || valid_tool_consent)
    }
}

pub fn normalize_slash_capability(
    raw_prompt: &str,
    default_capability: &str,
) -> (String, String) {
    let trimmed = raw_prompt.trim_start();
    let mappings: &[(&str, &str)] = &[
        ("/deep-research", "deep_research"),
        ("/deep_research", "deep_research"),
        ("/research", "deep_research"),
        ("/canvas", "canvas"),
        ("/image", "image"),
        ("/video", "video"),
    ];
    for &(prefix, cap) in mappings {
        if let Some(after) = trimmed.strip_prefix(prefix) {
            if after.is_empty() || after.starts_with(char::is_whitespace) {
                let rest = after.trim_start();
                if !rest.is_empty() {
                    return (cap.to_string(), rest.to_string());
                }
            }
        }
    }
    (default_capability.to_string(), raw_prompt.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub operation_id: String,
    pub request_id: String,
    pub session_id: String,
    pub status: String,
    pub reason: String,
    pub capability: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    pub timeout_ms: u64,
    pub prior_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_mode: Option<RequestMode>,
    pub native_dir_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_observed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_evidence: Option<CompletionEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_action: Option<PendingAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_attempted_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmation_attempted_action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    pub prompt: String,
    pub worker_pid: Option<u32>,
    pub worker_token: Option<String>,
    pub worker_command: Option<String>,
}

impl Operation {
    fn mode(&self) -> RequestMode {
        self.request_mode
            .unwrap_or(if self.prior_request_id.is_some() {
                RequestMode::Continue
            } else {
                RequestMode::Start
            })
    }

    pub fn public(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap();
        let o = v.as_object_mut().unwrap();
        for k in ["prompt", "worker_pid", "worker_token", "worker_command"] {
            o.remove(k);
        }
        v
    }
}

fn uuid(s: &str) -> Result<String> {
    Ok(Uuid::parse_str(s)?.to_string())
}

fn matches_cached_desktop_model(slug: &str) -> bool {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return false;
    };
    let path = home.join("Library/Caches/GeminiDesktopMCP/discovered-models.json");
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    val.get("models")
        .and_then(Value::as_array)
        .is_some_and(|arr| {
            arr.iter().any(|item| {
                let id_match = item
                    .get("id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id.eq_ignore_ascii_case(slug));
                let tok_match = item
                    .get("picker_token")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t.eq_ignore_ascii_case(slug));
                id_match || tok_match
            })
        })
}

pub fn normalize_model_and_effort(
    raw_model: Option<&str>,
    raw_effort: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    let mut inferred_effort: Option<&str> = None;
    let model = match raw_model.map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(m) => {
            let mut slug = m.to_ascii_lowercase();
            for prefix in ["google/", "gemini_app/", "gemini-app/", "gemini/"] {
                if let Some(rest) = slug.strip_prefix(prefix) {
                    slug = rest.to_string();
                    break;
                }
            }
            if raw_effort.map(str::trim).filter(|s| !s.is_empty()).is_none() {
                for suffix in ["minimal", "low", "medium", "high", "xhigh", "max"] {
                    if let Some(base) = slug
                        .strip_suffix(suffix)
                        .and_then(|p| p.strip_suffix('-'))
                        .filter(|b| !b.is_empty())
                    {
                        inferred_effort = Some(suffix);
                        slug = base.to_string();
                        break;
                    }
                }
            }
            match slug.as_str() {
                "auto" | "default" | "gemini-desktop-task" => None,
                "gemini-desktop-auto" => Some("gemini-desktop-auto".to_string()),
                "flash" | "hydrogen" | "gemini-4-hydrogen" | "gemini-4-flash" | "gemini-flash" => {
                    Some("flash".to_string())
                }
                "carbon" | "gemini-4-carbon" | "pro" | "gemini-pro" => Some("carbon".to_string()),
                other => {
                    let is_valid_gemini_slug = other
                        .strip_prefix("gemini-")
                        .is_some_and(|rest| {
                            !rest.is_empty()
                                && !rest.contains("unknown")
                                && !rest.contains("bogus")
                                && rest.chars().all(|c| {
                                    c.is_ascii_lowercase()
                                        || c.is_ascii_digit()
                                        || matches!(c, '.' | '-')
                                })
                        });
                    if is_valid_gemini_slug || matches_cached_desktop_model(other) {
                        Some(other.to_string())
                    } else {
                        bail!("unsupported_model_selection: 指定模型 `{m}` 不受支持")
                    }
                }
            }
        }
    };
    let effective_effort = raw_effort
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(inferred_effort);
    let effort = match effective_effort {
        None => None,
        Some(e) => match e.to_ascii_lowercase().as_str() {
            "auto" | "default" => None,
            "minimal" | "none" | "off" => Some("minimal".to_string()),
            "low" | "standard" => Some("low".to_string()),
            "medium" | "med" => Some("medium".to_string()),
            "high" | "extended" => Some("high".to_string()),
            "xhigh" | "extra_high" | "x-high" => Some("xhigh".to_string()),
            "max" | "deep" | "deep_think" | "deep-think" => Some("max".to_string()),
            _ => bail!("unsupported_effort_selection: 指定思考等级 `{e}` 不受支持"),
        },
    };
    Ok((model, effort))
}

pub(crate) fn root_for(custom_root: Option<&Path>) -> Result<PathBuf> {
    let p = if let Some(explicit) = custom_root {
        explicit.to_path_buf()
    } else {
        match std::env::var_os("GEMINI_DESKTOP_MCP_STATE_DIR") {
            Some(s) => PathBuf::from(s),
            None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME unset"))?)
                .join("Library/Application Support/GeminiDesktopMCP"),
        }
    };
    if !p.is_absolute() {
        bail!("state directory must be absolute")
    }
    if !p.exists() {
        fs::create_dir_all(&p)?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700))?;
    }
    let m = fs::symlink_metadata(&p)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != current_euid()
        || m.mode() & 0o077 != 0
    {
        bail!("state directory must be private and owned")
    }
    Ok(p)
}

fn dir(custom_root: Option<&Path>, id: &str) -> Result<PathBuf> {
    let p = root_for(custom_root)?.join(uuid(id)?);
    match fs::symlink_metadata(&p) {
        Ok(m)
            if !m.is_dir()
                || m.file_type().is_symlink()
                || m.uid() != current_euid()
                || m.mode() & 0o077 != 0 =>
        {
            bail!("invalid operation directory")
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(p)
}

fn nofollow_flag() -> i32 {
    OFlag::O_NOFOLLOW.bits()
}

fn dir_read_flags() -> OFlag {
    OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

fn lock(p: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nofollow_flag())
        .open(p.join("lock"))?;
    f.lock_exclusive()?;
    Ok(f)
}

fn save(p: &Path, o: &Operation) -> Result<()> {
    let tmp = p.join(format!("{}.tmp", Uuid::new_v4()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(nofollow_flag())
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec(o)?)?;
    f.sync_all()?;
    fs::rename(&tmp, p.join("operation.json"))?;
    File::open(p)?.sync_all()?;
    Ok(())
}

fn mode_complete(
    operation: &Operation,
    text: Option<&str>,
    artifacts: &[Artifact],
    evidence: Option<&CompletionEvidence>,
) -> bool {
    let text_present = text.is_some_and(|s| !s.is_empty());
    match operation.capability.as_str() {
        "chat" => text_present || !artifacts.is_empty(),
        "image" => artifacts.iter().any(|item| item.kind == "image"),
        "video" => artifacts.iter().any(|item| item.kind == "video"),
        "canvas" | "deep_research" => match (text.filter(|s| !s.is_empty()), evidence) {
            (Some(text), Some(proof)) => {
                let expected_kind = if operation.capability == "canvas" {
                    "canvas_document_text"
                } else {
                    "research_report_text"
                };
                proof.kind == expected_kind
                    && proof.request_id == operation.request_id
                    && proof.session_id == operation.session_id
                    && proof.text_sha256 == format!("{:x}", Sha256::digest(text.as_bytes()))
            }
            _ => false,
        },
        _ => false,
    }
}

fn read(custom_root: Option<&Path>, p: &Path) -> Result<Operation> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_flag())
        .open(p.join("operation.json"))?;
    let mut o: Operation = serde_json::from_reader(f)?;
    if o.status == "completed" {
        let artifacts_valid = verify_all_with_root(custom_root, &o, &o.artifacts).is_ok();
        if !artifacts_valid
            || !mode_complete(
                &o,
                o.text.as_deref(),
                &o.artifacts,
                o.completion_evidence.as_ref(),
            )
        {
            o.status = "unknown".into();
            o.reason = if artifacts_valid {
                "native_mode_completion_unverified"
            } else {
                "artifact_validation_failed"
            }
            .into();
            o.text = None;
            o.artifacts.clear();
            o.completion_evidence = None;
        }
    } else if o.status == "failed"
        && !(o.failure_observed == Some(true)
            && o.native_kind.as_deref() == Some("failed")
            && o.reason == "native_error_response"
            && o.artifacts.is_empty()
            && o.text
                .as_ref()
                .is_some_and(|t| !t.is_empty() && t.len() <= 65536))
    {
        o.status = "unknown".into();
        o.reason = "native_failure_unverified".into();
        o.text = None;
        o.artifacts.clear();
        o.failure_observed = None;
    }
    Ok(o)
}

fn terminal(s: &str) -> bool {
    matches!(s, "completed" | "failed" | "cancelled" | "not_submitted")
}

fn worker_matches(line: &str, o: &Operation) -> bool {
    let (Some(token), Some(command)) = (o.worker_token.as_deref(), o.worker_command.as_deref())
    else {
        return false;
    };
    let mut pieces = line.split_whitespace();
    let Some(state) = pieces.next() else {
        return false;
    };
    if state.starts_with('Z') {
        return false;
    }
    let words: Vec<_> = pieces.collect();
    words.ends_with(&["--worker", o.operation_id.as_str(), command, token])
}

fn alive(o: &Operation) -> bool {
    let (Some(pid), Some(token), Some(command)) = (
        o.worker_pid,
        o.worker_token.as_deref(),
        o.worker_command.as_deref(),
    ) else {
        return false;
    };
    if pid == std::process::id() {
        if let Ok(guard) = active_workers_map().lock()
            && let Some(entry) = guard.get(&o.operation_id)
            && entry.pid == pid
            && entry.token == token
            && entry.command == command
        {
            return true;
        }
    }
    let output = match Command::new("/bin/ps")
        .args([
            "-ww",
            "-p",
            &pid.to_string(),
            "-o",
            "stat=",
            "-o",
            "command=",
        ])
        .output()
    {
        Ok(x) if x.status.success() => x,
        _ => return false,
    };
    worker_matches(&String::from_utf8_lossy(&output.stdout), o)
}

fn bridge(custom_root: Option<&Path>) -> Result<PathBuf> {
    let root = root_for(custom_root)?;
    let digest = format!("{:x}", Sha256::digest(SWIFT.as_bytes()));
    let path = root.join(format!("bridge-{digest}.swift"));
    let _guard = lock(&root)?;
    if !path.exists() {
        let temp = root.join(format!("bridge-{}.tmp", Uuid::new_v4()));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nofollow_flag())
            .open(&temp)?;
        f.write_all(SWIFT.as_bytes())?;
        f.sync_all()?;
        let linked = fs::hard_link(&temp, &path);
        fs::remove_file(&temp)?;
        if let Err(e) = linked {
            if e.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(e.into());
            }
        }
        File::open(&root)?.sync_all()?;
    }
    let m = fs::symlink_metadata(&path)?;
    if !m.is_file()
        || m.file_type().is_symlink()
        || m.uid() != current_euid()
        || m.mode() & 0o077 != 0
    {
        bail!("invalid bridge resource")
    }
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_flag())
        .open(&path)?
        .read_to_end(&mut bytes)?;
    if format!("{:x}", Sha256::digest(bytes)) != digest {
        bail!("bridge resource mismatch")
    }
    Ok(path)
}

fn cached_bridge_runner(custom_root: Option<&Path>) -> Result<PathBuf> {
    let swift_path = bridge(custom_root)?;
    let state_root = root_for(custom_root)?;
    let digest = format!("{:x}", Sha256::digest(SWIFT.as_bytes()));
    let compiled_path = state_root.join(format!("bridge-{digest}.runner"));
    let _guard = lock(&state_root)?;
    if !compiled_path.exists() {
        let staged = state_root.join(format!("bridge-{}.runner.tmp", Uuid::new_v4()));
        let output = Command::new("/usr/bin/xcrun")
            .args([
                "swiftc",
                "-Onone",
                "-o",
                staged.to_str().ok_or_else(|| anyhow!("invalid staged path"))?,
                swift_path
                    .to_str()
                    .ok_or_else(|| anyhow!("invalid swift path"))?,
            ])
            .stdin(Stdio::null())
            .output()
            .context("failed to invoke xcrun swiftc")?;
        if output.status.success() && staged.exists() {
            fs::set_permissions(&staged, fs::Permissions::from_mode(0o700))?;
            if let Err(err) = fs::rename(&staged, &compiled_path) {
                let _ = fs::remove_file(&staged);
                if !compiled_path.exists() {
                    return Err(err.into());
                }
            }
        } else {
            let _ = fs::remove_file(&staged);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let snippet: String = stderr.trim().chars().take(1200).collect();
            bail!("bridge_compile_failed: {snippet}");
        }
    }
    let meta = fs::symlink_metadata(&compiled_path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.mode() & 0o077 != 0 {
        bail!("invalid cached bridge runner")
    }
    Ok(compiled_path)
}

fn native_with_root(custom_root: Option<&Path>, command: &str, req: Option<Value>) -> Result<Value> {
    if ![
        "probe",
        "accessibility",
        "prompt_accessibility",
        "discover_models",
        "capabilities",
        "run",
        "continue",
        "resume",
        "cancel",
        "confirm",
        "usage",
        "usage_refresh",
        "focus",
    ]
    .contains(&command)
    {
        bail!("invalid command")
    }
    let root = root_for(custom_root)?;
    let output = root.join(format!("native-{}.tmp", Uuid::new_v4()));
    let out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&output)?;
    let err_output = root.join(format!("native-err-{}.tmp", Uuid::new_v4()));
    let err_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&err_output)?;
    let timeout = req
        .as_ref()
        .and_then(|r| r["timeout_ms"].as_u64())
        .unwrap_or(10000);
    let runner = cached_bridge_runner(custom_root)?;
    let mut command_line = Command::new(&runner);
    command_line.arg(command);
    if !run_bounded_with_stderr(
        custom_root,
        command_line,
        req.as_ref(),
        out,
        Some(err_file),
        Duration::from_millis(timeout + 30000),
    )? {
        let _ = fs::remove_file(output);
        let _ = fs::remove_file(err_output);
        return Ok(json!({"_transport_error":"native_timeout"}));
    }
    let mut data = String::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_flag())
        .open(&output)?
        .take(2_000_000)
        .read_to_string(&mut data)?;
    let _ = fs::remove_file(output);
    let mut err_data = String::new();
    if let Ok(ef) = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow_flag())
        .open(&err_output)
    {
        let _ = ef.take(4096).read_to_string(&mut err_data);
    }
    let _ = fs::remove_file(err_output);
    if let Some(parsed) = data
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .last()
    {
        return Ok(parsed);
    }
    let mut err_obj = json!({"_transport_error":"native_no_receipt"});
    let clean_err: String = err_data.trim().chars().take(1200).collect();
    if !clean_err.is_empty() {
        err_obj["_transport_detail"] = json!(clean_err);
    }
    Ok(err_obj)
}

fn run_bounded_with_stderr(
    custom_root: Option<&Path>,
    mut command: Command,
    request: Option<&Value>,
    out: File,
    err: Option<File>,
    budget: Duration,
) -> Result<bool> {
    let start = Instant::now();
    let input = if let Some(request) = request {
        let path = root_for(custom_root)?.join(format!("native-input-{}.tmp", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nofollow_flag())
            .open(&path)?;
        file.write_all(&serde_json::to_vec(request)?)?;
        file.seek(SeekFrom::Start(0))?;
        fs::remove_file(&path)?;
        Stdio::from(file)
    } else {
        Stdio::null()
    };
    let err_stdio = err.map(Stdio::from).unwrap_or_else(Stdio::null);
    command
        .stdin(input)
        .stdout(out)
        .stderr(err_stdio)
        .process_group(0);
    let mut child = command.spawn()?;
    let pgid = i32::try_from(child.id()).ok();
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if start.elapsed() >= budget {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if let Some(pg) = pgid {
        let _ = killpg(Pid::from_raw(pg), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(false)
}

fn launch(custom_root: Option<&Path>, p: &Path, o: &mut Operation, command: &str) -> Result<()> {
    let token = Uuid::new_v4().to_string();
    o.status = "unknown".into();
    o.reason = "launch_outcome_unknown".into();
    o.worker_token = Some(token.clone());
    o.worker_command = Some(command.into());
    o.worker_pid = None;
    save(p, o)?;

    let op_id = o.operation_id.clone();
    let cmd_owned = command.to_string();
    let tok_owned = token.clone();
    let root_owned = custom_root.map(Path::to_path_buf);
    let self_pid = std::process::id();

    if let Ok(mut guard) = active_workers_map().lock() {
        guard.insert(
            op_id.clone(),
            ActiveWorkerEntry {
                token: tok_owned.clone(),
                command: cmd_owned.clone(),
                pid: self_pid,
            },
        );
    }

    let spawn_res = std::thread::Builder::new()
        .name(format!("gemini-worker-{}", &op_id[..op_id.len().min(8)]))
        .spawn(move || {
            let _guard = ActiveWorkerGuard {
                op_id: op_id.clone(),
                token: tok_owned.clone(),
            };
            let _ = worker_inner(root_owned.as_deref(), &op_id, &cmd_owned, &tok_owned);
        });

    match spawn_res {
        Ok(_) => {
            o.worker_pid = Some(self_pid);
            o.status = "running".into();
            o.reason = "native_worker_started".into();
        }
        Err(_) => {
            if let Ok(mut guard) = active_workers_map().lock() {
                guard.remove(&o.operation_id);
            }
            o.reason = "worker_launch_failed".into();
        }
    }
    save(p, o)
}

fn native_binding_matches(custom_root: Option<&Path>, prior: &Operation) -> bool {
    let result = (|| -> Result<bool> {
        let native = dir(custom_root, &prior.native_dir_id)?.join("native");
        let folder_fd = nix::fcntl::open(&native, dir_read_flags(), Mode::empty())?;
        let folder = File::from(folder_fd);
        let m = folder.metadata()?;
        if !m.is_dir() || m.uid() != current_euid() || m.mode() & 0o077 != 0 {
            return Ok(false);
        }
        let name = format!("gemini-{:x}.json", Sha256::digest(prior.request_id.as_bytes()));
        let file_fd = openat(
            &folder,
            Path::new(&name),
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        )?;
        let file = File::from(file_fd);
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != current_euid()
            || meta.mode() & 0o077 != 0
            || meta.len() == 0
            || meta.len() > 1_000_000
        {
            return Ok(false);
        }
        let binding: Value = serde_json::from_reader(file)?;
        Ok(binding["requestID"].as_str() == Some(&prior.request_id)
            && binding["sessionID"].as_str() == Some(&prior.session_id))
    })();
    result.unwrap_or(false)
}

fn request_matches(
    existing: &Operation,
    prompt: &str,
    timeout: u64,
    capability: &str,
    mode: RequestMode,
    previous: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> bool {
    existing.prompt == prompt
        && existing.timeout_ms == timeout
        && existing.capability == capability
        && existing.mode() == mode
        && existing.prior_request_id.as_deref() == previous
        && existing.model.as_deref() == model
        && existing.effort.as_deref() == effort
}

fn resolve_predecessor(
    custom_root: Option<&Path>,
    previous: &str,
    mode: RequestMode,
) -> Result<std::result::Result<Operation, Value>> {
    let p = dir(custom_root, previous)?;
    if !p.is_dir() {
        return Ok(Err(
            json!({"status":"blocked","reason":"prior_operation_not_found","operation_id":previous}),
        ));
    }
    let _lock = lock(&p)?;
    let candidate = match read(custom_root, &p) {
        Ok(x) => x,
        Err(_) => {
            return Ok(Err(
                json!({"status":"blocked","reason":"prior_state_unavailable","operation_id":previous}),
            ));
        }
    };
    if candidate.operation_id != previous
        || candidate.request_id != previous
        || Uuid::parse_str(&candidate.session_id).is_err()
    {
        return Ok(Err(
            json!({"status":"blocked","reason":"prior_identity_mismatch","operation_id":previous}),
        ));
    }
    let allowed = match mode {
        RequestMode::Continue => candidate.status == "completed",
        RequestMode::FreshAfter => {
            candidate.status == "completed"
                || (candidate.status == "failed" && candidate.failure_observed == Some(true))
        }
        RequestMode::Start => false,
    };
    if !allowed {
        return Ok(Err(
            json!({"status":"blocked","reason":"prior_turn_not_completed","operation_id":previous}),
        ));
    }
    if mode == RequestMode::FreshAfter && !native_binding_matches(custom_root, &candidate) {
        return Ok(Err(
            json!({"status":"blocked","reason":"prior_native_binding_unverified","operation_id":previous}),
        ));
    }
    Ok(Ok(candidate))
}

fn precheck_fresh_after(custom_root: Option<&Path>, previous: &str) -> Result<Option<Value>> {
    let previous = uuid(previous)?;
    let root = root_for(custom_root)?;
    let _root_lock = lock(&root)?;
    Ok(resolve_predecessor(custom_root, &previous, RequestMode::FreshAfter)?.err())
}

fn existing_start_with_selection(
    custom_root: Option<&Path>,
    prompt: &str,
    id: &str,
    timeout: u64,
    capability: &str,
    previous: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Option<Value>> {
    let timeout = if timeout == 0 { 86_400_000 } else { timeout };
    if prompt.is_empty() || prompt.len() > 65536 || !(100..=86_400_000).contains(&timeout) {
        bail!("invalid prompt or timeout")
    }
    let (norm_model, norm_effort) = normalize_model_and_effort(model, effort)?;
    let id = uuid(id)?;
    let previous = previous.map(uuid).transpose()?;
    let mode = if previous.is_some() {
        RequestMode::FreshAfter
    } else {
        RequestMode::Start
    };
    let root = root_for(custom_root)?;
    let _root_lock = lock(&root)?;
    let path = dir(custom_root, &id)?;
    if !path.exists() {
        return Ok(None);
    }
    let _lock = lock(&path)?;
    let existing = match read(custom_root, &path) {
        Ok(x) => x,
        Err(_) => {
            return Ok(Some(
                json!({"status":"unknown","reason":"incomplete_operation_record","operation_id":id}),
            ));
        }
    };
    if !request_matches(
        &existing,
        prompt,
        timeout,
        capability,
        mode,
        previous.as_deref(),
        norm_model.as_deref(),
        norm_effort.as_deref(),
    ) {
        return Ok(Some(json!({"status":"conflict","operation_id":id})));
    }
    Ok(Some(existing.public()))
}

#[allow(clippy::too_many_arguments)]
fn start_with(
    custom_root: Option<&Path>,
    prompt: &str,
    id: Option<&str>,
    timeout: u64,
    capability: &str,
    prior: Option<&str>,
    mode: RequestMode,
    model: Option<&str>,
    effort: Option<&str>,
    launcher: fn(Option<&Path>, &Path, &mut Operation, &str) -> Result<()>,
) -> Result<Value> {
    let timeout = if timeout == 0 { 86_400_000 } else { timeout };
    if prompt.is_empty() || prompt.len() > 65536 || !(100..=86_400_000).contains(&timeout) {
        bail!("invalid prompt or timeout")
    }
    if !["chat", "image", "video", "deep_research", "canvas"].contains(&capability) {
        bail!("invalid capability")
    }
    if (mode == RequestMode::Start) != prior.is_none() {
        bail!("invalid request mode")
    }
    let (norm_model, norm_effort) = normalize_model_and_effort(model, effort)?;
    let id = uuid(id.ok_or_else(|| anyhow!("request_id is required"))?)?;
    let previous = prior.map(uuid).transpose()?;
    let root = root_for(custom_root)?;
    let _root_lock = lock(&root)?;
    let path = dir(custom_root, &id)?;
    if path.exists() {
        let _lock = lock(&path)?;
        let existing = match read(custom_root, &path) {
            Ok(x) => x,
            Err(_) => {
                return Ok(
                    json!({"status":"unknown","reason":"incomplete_operation_record","operation_id":id}),
                );
            }
        };
        if !request_matches(
            &existing,
            prompt,
            timeout,
            capability,
            mode,
            previous.as_deref(),
            norm_model.as_deref(),
            norm_effort.as_deref(),
        ) {
            return Ok(json!({"status":"conflict","operation_id":id}));
        }
        return Ok(existing.public());
    }
    let predecessor = if let Some(previous) = previous.as_deref() {
        match resolve_predecessor(custom_root, previous, mode)? {
            Ok(candidate) => Some(candidate),
            Err(blocked) => return Ok(blocked),
        }
    } else {
        None
    };
    fs::create_dir(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    let _lock = lock(&path)?;
    let mut o = Operation {
        operation_id: id.clone(),
        request_id: id.clone(),
        session_id: if mode == RequestMode::Continue {
            predecessor.as_ref().unwrap().session_id.clone()
        } else {
            Uuid::new_v4().to_string()
        },
        status: "unknown".into(),
        reason: "persisted_before_launch".into(),
        capability: capability.into(),
        model: norm_model,
        effort: norm_effort,
        timeout_ms: timeout,
        prior_request_id: previous,
        request_mode: Some(mode),
        native_dir_id: predecessor
            .as_ref()
            .map(|x| x.native_dir_id.clone())
            .unwrap_or(id),
        native_kind: None,
        recovery: None,
        failure_observed: None,
        completion_evidence: None,
        pending_action: None,
        confirmation_attempted_version: None,
        confirmation_attempted_action: None,
        text: None,
        artifacts: Vec::new(),
        prompt: prompt.into(),
        worker_pid: None,
        worker_token: None,
        worker_command: None,
    };
    save(&path, &o)?;
    if predecessor.is_none() {
        fs::create_dir(path.join("native"))?;
        fs::set_permissions(path.join("native"), fs::Permissions::from_mode(0o700))?;
    }
    launcher(
        custom_root,
        &path,
        &mut o,
        if mode == RequestMode::Continue {
            "continue"
        } else {
            "run"
        },
    )?;
    Ok(o.public())
}

fn focus_with_root(custom_root: Option<&Path>, id: Option<&str>) -> Result<Value> {
    let req = if let Some(id) = id {
        let p = dir(custom_root, id)?;
        if !p.is_dir() {
            return Ok(json!({"status":"not_found","operation_id":id}));
        }
        let _guard = lock(&p)?;
        let operation = read(custom_root, &p)?;
        Some(json!({
            "request_id": operation.request_id,
            "session_id": operation.session_id,
            "state_dir": dir(custom_root, &operation.native_dir_id)?.join("native")
        }))
    } else {
        None
    };
    let result = native_with_root(custom_root, "focus", req.clone())?;
    if result["version"] != 1
        || !matches!(result["status"].as_str(), Some("ready" | "unknown"))
        || (result["status"] == "ready"
            && (result["foreground"] != true
                || req.as_ref().is_some_and(|r| {
                    result["request_id"] != r["request_id"]
                        || result["session_id"] != r["session_id"]
                })))
    {
        return Ok(json!({"status":"unknown","reason":"native_focus_receipt_unverified"}));
    }
    Ok(result)
}

fn read_progress_text(custom_root: Option<&Path>, o: &Operation) -> Option<String> {
    let native = dir(custom_root, &o.native_dir_id).ok()?.join("native");
    let folder_fd = nix::fcntl::open(&native, dir_read_flags(), Mode::empty()).ok()?;
    let folder = File::from(folder_fd);
    let m = folder.metadata().ok()?;
    if !m.is_dir() || m.uid() != current_euid() || m.mode() & 0o077 != 0 {
        return None;
    }
    let name = format!("progress-{:x}.json", Sha256::digest(o.request_id.as_bytes()));
    let file_fd = openat(
        &folder,
        Path::new(&name),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(file_fd);
    let meta = file.metadata().ok()?;
    if !meta.is_file()
        || meta.uid() != current_euid()
        || meta.mode() & 0o077 != 0
        || meta.len() == 0
        || meta.len() > 1_000_000
    {
        return None;
    }
    let val: Value = serde_json::from_reader(file).ok()?;
    if val["request_id"].as_str() != Some(&o.request_id)
        || val["session_id"].as_str() != Some(&o.session_id)
    {
        return None;
    }
    val["text"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn remove_progress_file(custom_root: Option<&Path>, o: &Operation) {
    if let Ok(native) = dir(custom_root, &o.native_dir_id).map(|d| d.join("native")) {
        let p = native.join(format!(
            "progress-{:x}.json",
            Sha256::digest(o.request_id.as_bytes())
        ));
        let _ = fs::remove_file(p);
    }
}

pub fn observe_with_root(custom_root: Option<&Path>, id: &str) -> Result<Value> {
    let p = dir(custom_root, id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    }
    let _l = lock(&p)?;
    let mut o = read(custom_root, &p)?;
    if o.status == "running" && !alive(&o) {
        o.status = "unknown".into();
        o.reason = "worker_exited_without_receipt".into();
        save(&p, &o)?;
    }
    if o.status != "running" {
        remove_progress_file(custom_root, &o);
    }
    let mut out = o.public();
    if o.status == "running" && out.get("text").is_none_or(Value::is_null) {
        if let Some(partial) = read_progress_text(custom_root, &o) {
            out["text"] = json!(partial);
        }
    }
    Ok(out)
}

pub fn collect_with_root(custom_root: Option<&Path>, id: &str) -> Result<Value> {
    let p = dir(custom_root, id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    }
    let _lock = lock(&p)?;
    let o = read(custom_root, &p)?;
    if o.status != "completed" {
        return Ok(o.public());
    }
    if verify_all_with_root(custom_root, &o, &o.artifacts).is_err() {
        return Ok(
            json!({"status":"unknown","reason":"artifact_validation_failed","operation_id":id}),
        );
    }
    let mut value = json!({"status":"completed","operation_id":o.operation_id,"artifacts":o.artifacts});
    if let Some(evidence) = o.completion_evidence {
        value["completion_evidence"] = serde_json::to_value(evidence)?;
    }
    if let Some(text) = o.text {
        value["text"] = json!(text);
        value["format"] = json!("plain_text");
    }
    Ok(value)
}

pub fn action_with_root(custom_root: Option<&Path>, id: &str, command: &str) -> Result<Value> {
    if !["resume", "cancel"].contains(&command) {
        bail!("invalid action")
    }
    let p = dir(custom_root, id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    }
    let _l = lock(&p)?;
    let mut o = read(custom_root, &p)?;
    if terminal(&o.status) {
        return Ok(o.public());
    }
    if o.status == "running" && alive(&o) {
        let hash = format!("{:x}", Sha256::digest(o.request_id.as_bytes()));
        let binding = dir(custom_root, &o.native_dir_id)?
            .join("native")
            .join(format!("gemini-{hash}.json"));
        if command != "cancel"
            || o.worker_command.as_deref() == Some("cancel")
            || !binding.is_file()
        {
            let mut v = o.public();
            v["action"] = json!("worker_already_running");
            return Ok(v);
        }
    }
    launch(custom_root, &p, &mut o, command)?;
    Ok(o.public())
}

pub fn confirm_with_root(
    custom_root: Option<&Path>,
    id: &str,
    action_id: &str,
    action_version: &str,
) -> Result<Value> {
    let p = dir(custom_root, id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    }
    let _l = lock(&p)?;
    let mut o = read(custom_root, &p)?;
    if (action_id == "deep_research_start"
        || action_id == "tool_consent_allow"
        || action_id == "tool_consent_deny")
        && o.confirmation_attempted_version.as_deref() == Some(action_version)
    {
        let mut result = o.public();
        result["action"] = json!("confirmation_already_attempted_no_replay");
        return Ok(result);
    }
    let eligible_research = o.status == "waiting_human"
        && o.reason == "research_plan_confirmation"
        && o.capability == "deep_research"
        && o.confirmation_attempted_version.is_none()
        && o.pending_action.as_ref().is_some_and(|a| {
            a.valid() && a.action_id == action_id && a.action_version == action_version
        });
    let eligible_tool_consent = o.status == "waiting_human"
        && o.reason == "tool_consent_confirmation"
        && matches!(action_id, "tool_consent_allow" | "tool_consent_deny")
        && o.confirmation_attempted_version.as_deref() != Some(action_version)
        && o.pending_action.as_ref().is_some_and(|a| {
            a.valid() && a.kind == "tool_consent" && a.action_version == action_version
        });
    if !(eligible_research || eligible_tool_consent) {
        return Ok(
            json!({"status":"blocked","operation_id":id,"reason":"confirmation_action_or_version_mismatch"}),
        );
    }
    o.confirmation_attempted_version = Some(action_version.to_owned());
    o.confirmation_attempted_action = Some(action_id.to_owned());
    launch(custom_root, &p, &mut o, "confirm")?;
    Ok(o.public())
}

fn worker_inner(custom_root: Option<&Path>, id: &str, command: &str, token: &str) -> Result<()> {
    if !["run", "continue", "resume", "cancel", "confirm"].contains(&command) {
        bail!("bad command")
    }
    let p = dir(custom_root, id)?;
    let o = {
        let _l = lock(&p)?;
        let o = read(custom_root, &p)?;
        if o.worker_token.as_deref() != Some(token) {
            return Ok(());
        }
        o
    };
    let effective_worker_timeout = if command == "cancel" {
        o.timeout_ms.min(15_000)
    } else {
        o.timeout_ms
    };
    let mut req = json!({
        "request_id": o.request_id,
        "session_id": o.session_id,
        "state_dir": dir(custom_root, &o.native_dir_id)?.join("native"),
        "timeout_ms": effective_worker_timeout
    });
    if ["run", "continue"].contains(&command) {
        req["prompt"] = json!(o.prompt);
        req["capability"] = json!(o.capability);
        if let Some(m) = o.model.as_deref() {
            req["model"] = json!(m);
        }
        if let Some(e) = o.effort.as_deref() {
            req["effort"] = json!(e);
        }
    }
    if ["run", "continue"].contains(&command) && o.prior_request_id.is_some() {
        req["prior_request_id"] = json!(o.prior_request_id);
    }
    if command == "confirm" {
        let action = o
            .pending_action
            .as_ref()
            .ok_or_else(|| anyhow!("missing pending action"))?;
        if !action.valid()
            || o.confirmation_attempted_version.as_deref() != Some(&action.action_version)
        {
            bail!("invalid persisted confirmation")
        }
        let requested_action = o
            .confirmation_attempted_action
            .as_deref()
            .unwrap_or(&action.action_id);
        req["action_id"] = json!(requested_action);
        req["action_version"] = json!(action.action_version);
    }
    let result = match native_with_root(custom_root, command, Some(req)) {
        Ok(v) => v,
        Err(e) => json!({
            "_transport_error": "native_worker_error",
            "_transport_detail": e.to_string(),
        }),
    };
    apply_receipt(custom_root, &p, token, result)
}

fn proof_flags_consistent(result: &Value, kind: &str, reason: &str) -> bool {
    fn flag(result: &Value, key: &str) -> Option<bool> {
        match result.get(key) {
            None | Some(Value::Null) => Some(false),
            Some(Value::Bool(value)) => Some(*value),
            _ => None,
        }
    }
    let (Some(completed), Some(failed), Some(not_submitted)) = (
        flag(result, "completion_observed"),
        flag(result, "failure_observed"),
        flag(result, "not_submitted"),
    ) else {
        return false;
    };
    if u8::from(completed) + u8::from(failed) + u8::from(not_submitted) > 1 {
        return false;
    }
    if completed && (kind != "completed" || reason == "cancelled") {
        return false;
    }
    if failed && (kind != "failed" || reason != "native_error_response") {
        return false;
    }
    if not_submitted && (kind != "error" || reason == "cancelled") {
        return false;
    }
    if kind == "failed" && (completed || not_submitted) {
        return false;
    }
    if kind == "completed" && reason == "cancelled" && (completed || failed || not_submitted) {
        return false;
    }
    true
}

fn apply_receipt(custom_root: Option<&Path>, p: &Path, token: &str, result: Value) -> Result<()> {
    let _l = lock(p)?;
    let mut current = read(custom_root, p)?;
    if current.worker_token.as_deref() != Some(token) {
        return Ok(());
    }
    if let Some(reason) = result["_transport_error"]
        .as_str()
        .filter(|x| ["native_timeout", "native_no_receipt", "native_worker_error"].contains(x))
    {
        current.status = "unknown".into();
        current.reason = reason.into();
        current.native_kind = None;
        current.worker_pid = None;
        if let Some(detail) = result["_transport_detail"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            current.recovery = Some(detail.to_string());
        }
        return save(p, &current).context("write transport error");
    }
    let valid = result["version"] == 1
        && result["request_id"].as_str() == Some(&current.request_id)
        && result["session_id"].as_str() == Some(&current.session_id);
    let kind = if valid {
        result["kind"].as_str().unwrap_or("unknown")
    } else {
        "uncertain"
    };
    let reason = if valid {
        result["reason"].as_str().unwrap_or("unknown")
    } else {
        "native_receipt_identity_mismatch"
    };
    if valid && !proof_flags_consistent(&result, kind, reason) {
        current.status = "unknown".into();
        current.reason = "native_proof_contradiction".into();
        current.native_kind = Some(kind.into());
        current.failure_observed = None;
        current.worker_pid = None;
        current.text = None;
        current.artifacts.clear();
        return save(p, &current).context("write contradictory native proof");
    }
    let artifacts = if valid && kind == "completed" && result["completion_observed"] == true {
        let parsed = if result.get("artifacts").is_none() || result["artifacts"].is_null() {
            Ok(Vec::new())
        } else {
            serde_json::from_value::<Vec<Artifact>>(result["artifacts"].clone())
        };
        match parsed {
            Ok(items) if verify_all_with_root(custom_root, &current, &items).is_ok() => items,
            _ => {
                current.status = "unknown".into();
                current.reason = "artifact_validation_failed".into();
                current.native_kind = Some(kind.into());
                current.worker_pid = None;
                current.artifacts.clear();
                current.text = None;
                return save(p, &current).context("write artifact verification failure");
            }
        }
    } else {
        Vec::new()
    };
    let evidence = result
        .get("completion_evidence")
        .filter(|v| !v.is_null())
        .and_then(|v| serde_json::from_value::<CompletionEvidence>(v.clone()).ok());
    let native_mode_complete = mode_complete(
        &current,
        result["text"].as_str(),
        &artifacts,
        evidence.as_ref(),
    );

    let failure_text = result["text"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 65536);
    let no_failure_artifacts = result
        .get("artifacts")
        .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty));
    let verified_failure = valid
        && kind == "failed"
        && reason == "native_error_response"
        && result["failure_observed"] == true
        && result["completion_observed"] == false
        && failure_text.is_some()
        && no_failure_artifacts;
    let status = if !valid {
        "unknown"
    } else if verified_failure {
        "failed"
    } else if kind == "waiting_human" {
        "waiting_human"
    } else if kind == "error"
        && result["not_submitted"] == true
        && result["completion_observed"] == false
        && result["text"].is_null()
    {
        "not_submitted"
    } else if kind == "completed"
        && reason == "cancelled"
        && result["completion_observed"] == false
        && result["text"].is_null()
    {
        "cancelled"
    } else if kind == "completed" && result["completion_observed"] == true && native_mode_complete {
        "completed"
    } else {
        "unknown"
    };
    current.status = status.into();
    current.reason = if valid
        && kind == "completed"
        && result["completion_observed"] == true
        && !native_mode_complete
    {
        "native_mode_completion_unverified".into()
    } else {
        reason.into()
    };
    current.native_kind = Some(kind.into());
    current.pending_action = if valid
        && status == "waiting_human"
        && ((reason == "research_plan_confirmation" && current.capability == "deep_research")
            || reason == "tool_consent_confirmation")
    {
        result
            .get("pending_action")
            .and_then(|v| serde_json::from_value::<PendingAction>(v.clone()).ok())
            .filter(PendingAction::valid)
    } else {
        None
    };
    if valid {
        current.recovery = result["recovery"].as_str().map(str::to_owned);
    }
    current.worker_pid = None;
    current.failure_observed = if verified_failure { Some(true) } else { None };
    if verified_failure {
        current.text = failure_text.map(str::to_owned);
        current.artifacts.clear();
        current.completion_evidence = None;
    }
    if status == "completed" {
        current.text = result["text"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        current.artifacts = artifacts;
        current.completion_evidence =
            if matches!(current.capability.as_str(), "canvas" | "deep_research") {
                evidence
            } else {
                None
            };
    }
    save(p, &current).context("write worker result")
}

/// Embedded dispatcher replacing the external `gemini-desktop-mcp` / `gemini-desktop-client` processes.
pub fn dispatch(action: &str, payload: &Value) -> Result<Value> {
    dispatch_with_root(None, action, payload)
}

pub fn dispatch_with_root(
    custom_root: Option<&Path>,
    action: &str,
    payload: &Value,
) -> Result<Value> {
    match action {
        "capabilities" => {
            let refresh = payload
                .get("refresh")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut native = native_with_root(custom_root, "capabilities", None)?;
            if refresh
                && let Ok(fresh) = native_with_root(custom_root, "discover_models", None)
                && let Some(models) = fresh.get("discovered_models")
                && let Some(obj) = native.as_object_mut()
            {
                obj.insert("discovered_models".into(), models.clone());
            }
            let available_models: Vec<Value> = native
                .get("discovered_models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let supported_models: Vec<String> = available_models
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            Ok(json!({
                "native": native,
                "readiness": native_with_root(custom_root, "probe", None)?,
                "public_capability_mapping": {"text":"chat","image":"image","video":"video","deep_research":"deep_research","canvas":"canvas"},
                "supported_models": supported_models,
                "available_models": available_models,
                "supported_efforts": ["minimal","low","medium","high","xhigh","max"],
            }))
        }
        "probe" => native_with_root(custom_root, "probe", None),
        "accessibility" => native_with_root(custom_root, "accessibility", None),
        "prompt_accessibility" => native_with_root(custom_root, "prompt_accessibility", None),
        "discover_models" => native_with_root(custom_root, "discover_models", None),
        "focus" => {
            let op_id = payload.get("operation_id").and_then(Value::as_str);
            focus_with_root(custom_root, op_id)
        }
        "start" => {
            let prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing prompt"))?;
            let request_id = payload
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing request_id"))?;
            let timeout = payload
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .filter(|&ms| ms > 0)
                .unwrap_or(86_400_000);
            let raw_cap = payload
                .get("capability")
                .and_then(Value::as_str)
                .unwrap_or("text");
            let base_cap = if raw_cap == "text" { "chat" } else { raw_cap };
            let (promoted_cap, effective_prompt) = normalize_slash_capability(prompt, base_cap);
            let native_cap = promoted_cap.as_str();
            let cap = if native_cap == "chat" {
                "text"
            } else {
                native_cap
            };
            if !["chat", "image", "video", "deep_research", "canvas"].contains(&native_cap) {
                bail!("invalid capability")
            }
            let model_arg = payload.get("model").and_then(Value::as_str);
            let effort_arg = payload.get("effort").and_then(Value::as_str);
            let previous_op = payload.get("previous_operation_id").and_then(Value::as_str);
            let (norm_model, norm_effort) = normalize_model_and_effort(model_arg, effort_arg)?;

            if native_cap != "chat" {
                if let Some(existing) = existing_start_with_selection(
                    custom_root,
                    &effective_prompt,
                    request_id,
                    timeout,
                    native_cap,
                    previous_op,
                    norm_model.as_deref(),
                    norm_effort.as_deref(),
                )? {
                    return Ok(existing);
                }
                if let Some(prev) = previous_op
                    && let Some(blocked) = precheck_fresh_after(custom_root, prev)?
                {
                    if let Some(existing) = existing_start_with_selection(
                        custom_root,
                        &effective_prompt,
                        request_id,
                        timeout,
                        native_cap,
                        previous_op,
                        norm_model.as_deref(),
                        norm_effort.as_deref(),
                    )? {
                        return Ok(existing);
                    }
                    return Ok(blocked);
                }
                let caps = native_with_root(custom_root, "capabilities", None).ok();
                let entry = caps.as_ref().map(|c| &c["capabilities"][native_cap]);
                let denial = match entry {
                    None | Some(Value::Null) => Some(
                        json!({"status":"blocked","reason":"native_capability_unavailable","capability":cap}),
                    ),
                    Some(entry)
                        if !matches!(
                            entry["implementation"].as_str(),
                            Some(
                                "implemented"
                                    | "implemented_unverified"
                                    | "experimental_unverified"
                            )
                        ) =>
                    {
                        Some(json!({"status":"unsupported","capability":cap,"native":entry}))
                    }
                    _ => None,
                };
                if let Some(denial) = denial {
                    if let Some(existing) = existing_start_with_selection(
                        custom_root,
                        &effective_prompt,
                        request_id,
                        timeout,
                        native_cap,
                        previous_op,
                        norm_model.as_deref(),
                        norm_effort.as_deref(),
                    )? {
                        return Ok(existing);
                    }
                    return Ok(denial);
                }
            }
            if let Some(prev) = previous_op {
                start_with(
                    custom_root,
                    &effective_prompt,
                    Some(request_id),
                    timeout,
                    native_cap,
                    Some(prev),
                    RequestMode::FreshAfter,
                    norm_model.as_deref(),
                    norm_effort.as_deref(),
                    launch,
                )
            } else {
                start_with(
                    custom_root,
                    &effective_prompt,
                    Some(request_id),
                    timeout,
                    native_cap,
                    None,
                    RequestMode::Start,
                    norm_model.as_deref(),
                    norm_effort.as_deref(),
                    launch,
                )
            }
        }
        "continue" => {
            let operation_id = payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing operation_id"))?;
            let prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing prompt"))?;
            let request_id = payload
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing request_id"))?;
            let timeout = payload
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .filter(|&ms| ms > 0)
                .unwrap_or(86_400_000);
            let model_arg = payload.get("model").and_then(Value::as_str);
            let effort_arg = payload.get("effort").and_then(Value::as_str);
            let (norm_model, norm_effort) = normalize_model_and_effort(model_arg, effort_arg)?;
            let old = observe_with_root(custom_root, operation_id)?;
            let cap = old["capability"].as_str().unwrap_or("chat");
            start_with(
                custom_root,
                prompt,
                Some(request_id),
                timeout,
                cap,
                Some(operation_id),
                RequestMode::Continue,
                norm_model.as_deref(),
                norm_effort.as_deref(),
                launch,
            )
        }
        "observe" => {
            let operation_id = payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing operation_id"))?;
            if payload
                .get("resume")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                action_with_root(custom_root, operation_id, "resume")
            } else {
                observe_with_root(custom_root, operation_id)
            }
        }
        "confirm" => {
            let operation_id = payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing operation_id"))?;
            let action_id = payload
                .get("action_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing action_id"))?;
            let action_version = payload
                .get("action_version")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing action_version"))?;
            confirm_with_root(custom_root, operation_id, action_id, action_version)
        }
        "collect" => {
            let operation_id = payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing operation_id"))?;
            collect_with_root(custom_root, operation_id)
        }
        "cancel" => {
            let operation_id = payload
                .get("operation_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("missing operation_id"))?;
            action_with_root(custom_root, operation_id, "cancel")
        }
        other => bail!("unsupported gemini desktop action: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_launch(_custom_root: Option<&Path>, p: &Path, o: &mut Operation, c: &str) -> Result<()> {
        o.status = "running".into();
        o.worker_pid = Some(std::process::id());
        o.worker_token = Some(c.into());
        o.worker_command = Some(c.into());
        save(p, o)
    }

    #[test]
    fn embedded_gemini_ops_zero_unsafe_and_rejects_unknown_models() -> Result<()> {
        assert!(normalize_model_and_effort(Some("nonexistent-model-xyz"), None).is_err());
        let (m, e) = normalize_model_and_effort(Some("gemini-4-carbon-high"), None)?;
        assert_eq!(m.as_deref(), Some("carbon"));
        assert_eq!(e.as_deref(), Some("high"));

        let d = tempfile::tempdir()?;
        let root_path_buf = d.path().join("state");
        let root_path = root_path_buf.as_path();
        let id = Uuid::new_v4().to_string();
        let first = start_with(
            Some(root_path),
            "hello",
            Some(&id),
            60000,
            "chat",
            None,
            RequestMode::Start,
            Some("gemini-4-carbon"),
            Some("high"),
            fake_launch,
        )?;
        assert_eq!(first["status"], "running");
        assert_eq!(first["model"], "carbon");
        assert_eq!(first["effort"], "high");

        let p = dir(Some(root_path), &id)?;
        apply_receipt(
            Some(root_path),
            &p,
            "run",
            json!({
                "version": 1,
                "request_id": id,
                "session_id": first["session_id"],
                "kind": "completed",
                "completion_observed": true,
                "reason": "owned_response_action_observed",
                "text": "answer"
            }),
        )?;
        assert_eq!(observe_with_root(Some(root_path), &id)?["status"], "completed");
        Ok(())
    }
}
