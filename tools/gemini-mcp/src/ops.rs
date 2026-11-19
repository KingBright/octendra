use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use uuid::Uuid;
use wait_timeout::ChildExt;

const SWIFT: &str = include_str!("../native/bridge.swift");
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
    pub artifacts: Vec<crate::artifact::Artifact>,
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
                "argon" | "gemini-4-argon" => Some("argon".to_string()),
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
                        bail!("unsupported_model_selection")
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
            _ => bail!("unsupported_effort_selection"),
        },
    };
    Ok((model, effort))
}
pub(crate) fn root() -> Result<PathBuf> {
    let p = match std::env::var_os("GEMINI_DESKTOP_MCP_STATE_DIR") {
        Some(s) => PathBuf::from(s),
        None => PathBuf::from(std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME unset"))?)
            .join("Library/Application Support/GeminiDesktopMCP"),
    };
    if !p.is_absolute() {
        bail!("state directory must be absolute")
    };
    if !p.exists() {
        fs::create_dir_all(&p)?;
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700))?;
    }
    let m = fs::symlink_metadata(&p)?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        bail!("state directory must be private and owned")
    };
    Ok(p)
}
fn dir(id: &str) -> Result<PathBuf> {
    let p = root()?.join(uuid(id)?);
    match fs::symlink_metadata(&p) {
        Ok(m)
            if !m.is_dir()
                || m.file_type().is_symlink()
                || m.uid() != unsafe { libc::geteuid() }
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
fn lock(p: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
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
        .custom_flags(libc::O_NOFOLLOW)
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
    artifacts: &[crate::artifact::Artifact],
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
fn read(p: &Path) -> Result<Operation> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p.join("operation.json"))?;
    let mut o: Operation = serde_json::from_reader(f)?;
    if o.status == "completed" {
        let artifacts_valid = crate::artifact::verify_all(&o, &o.artifacts).is_ok();
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
    let Some(pid) = o.worker_pid else {
        return false;
    };
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
fn bridge() -> Result<PathBuf> {
    let root = root()?;
    let digest = format!("{:x}", Sha256::digest(SWIFT.as_bytes()));
    let path = root.join(format!("bridge-{digest}.swift"));
    let _guard = lock(&root)?;
    if !path.exists() {
        let temp = root.join(format!("bridge-{}.tmp", Uuid::new_v4()));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
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
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & 0o077 != 0
    {
        bail!("invalid bridge resource")
    };
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)?
        .read_to_end(&mut bytes)?;
    if format!("{:x}", Sha256::digest(bytes)) != digest {
        bail!("bridge resource mismatch")
    };
    Ok(path)
}
fn cached_bridge_runner() -> Result<PathBuf> {
    let swift_path = bridge()?;
    let state_root = root()?;
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
pub fn native(command: &str, req: Option<Value>) -> Result<Value> {
    #[cfg(feature = "test-fixture")]
    if crate::FIXTURE_NATIVE {
        return fixture_native(command, req);
    }
    #[cfg(not(feature = "test-fixture"))]
    let _ = crate::FIXTURE_NATIVE;
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
    };
    let root = root()?;
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
    let runner = cached_bridge_runner()?;
    let command_line = if runner.extension().is_some_and(|e| e == "swift") {
        let mut c = Command::new("/usr/bin/xcrun");
        c.args([
            "swift",
            runner.to_str().ok_or_else(|| anyhow!("bad bridge path"))?,
            command,
        ]);
        c
    } else {
        let mut c = Command::new(&runner);
        c.arg(command);
        c
    };
    if !run_bounded_with_stderr(
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
        .custom_flags(libc::O_NOFOLLOW)
        .open(&output)?
        .take(2_000_000)
        .read_to_string(&mut data)?;
    let _ = fs::remove_file(output);
    let mut err_data = String::new();
    if let Ok(ef) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
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
#[cfg(test)]
fn run_bounded(
    command: Command,
    request: Option<&Value>,
    out: File,
    budget: Duration,
) -> Result<bool> {
    run_bounded_with_stderr(command, request, out, None, budget)
}
fn run_bounded_with_stderr(
    mut command: Command,
    request: Option<&Value>,
    out: File,
    err: Option<File>,
    budget: Duration,
) -> Result<bool> {
    let start = Instant::now();
    let input = if let Some(request) = request {
        let path = root()?.join(format!("native-input-{}.tmp", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(&serde_json::to_vec(request)?)?;
        file.seek(SeekFrom::Start(0))?;
        fs::remove_file(&path)?;
        Stdio::from(file)
    } else {
        Stdio::null()
    };
    let err_stdio = err.map(Stdio::from).unwrap_or_else(Stdio::null);
    command.stdin(input).stdout(out).stderr(err_stdio);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    if child
        .wait_timeout(budget.saturating_sub(start.elapsed()))?
        .is_some()
    {
        return Ok(true);
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    Ok(false)
}
#[cfg(feature = "test-fixture")]
fn fixture_native(command: &str, req: Option<Value>) -> Result<Value> {
    let req = req.unwrap_or(json!({}));
    if command == "focus" {
        let mut result =
            json!({"version":1,"status":"ready","foreground":true,"reason":"fixture_focus"});
        for key in ["request_id", "session_id"] {
            if let Some(value) = req.get(key) {
                result[key] = value.clone();
            }
        }
        return Ok(result);
    }
    if matches!(command, "usage" | "usage_refresh") {
        return Ok(json!({"version":1,"status":"observed_unverified",
            "source":"gemini_desktop_usage_limits_current_view",
            "collected_at":"2026-11-07T00:00:00Z","scope":"unspecified",
            "remaining_credits":37,"percent_used":25,
            "reset_label":"Resets at 8:00 PM","updated_label":null,
            "refresh_acknowledged":command=="usage_refresh"}));
    }
    if command == "accessibility" {
        return Ok(json!({"version":1,"accessibility_trusted":true,"app_installed":true}));
    }
    if command == "prompt_accessibility" {
        return Ok(json!({"version":1,"accessibility_trusted":true,"prompted":false}));
    }
    if command == "discover_models" {
        return Ok(json!({"version":1,"accessibility_trusted":true,"app_installed":true,"discovered_models":[]}));
    }
    if command == "capabilities" {
        if root()?.join("fixture-capability-disabled").exists() {
            return Ok(json!({"version":1,"capabilities":{}}));
        }
        return Ok(
            json!({"version":1,"capabilities":{"chat":{"implementation":"implemented_unverified"},"image":{"implementation":"implemented_unverified"},"video":{"implementation":"implemented_unverified"},"deep_research":{"implementation":"implemented_unverified"},"canvas":{"implementation":"implemented_unverified"}}}),
        );
    }
    if command == "probe" {
        return Ok(json!({"version":1,"available":true,"reason":"fixture"}));
    }
    if command == "usage" {
        return Ok(
            json!({"version":1,"status":"unknown","source":"gemini_desktop_usage_limits_current_view","collected_at":"2026-01-01T00:00:00Z","scope":"unspecified","remaining_credits":null,"percent_used":null,"reset_label":null,"updated_label":null,"reason":"usage_surface_not_observed"}),
        );
    }
    let rid = req["request_id"]
        .as_str()
        .ok_or_else(|| anyhow!("request missing id"))?;
    let session = req["session_id"]
        .as_str()
        .ok_or_else(|| anyhow!("request missing session"))?;
    let root = root()?;
    let _guard = lock(&root)?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(root.join("fixture-calls.jsonl"))?;
    writeln!(
        log,
        "{}",
        json!({"command":command,"request_id":rid,"session_id":session,"state_dir":req["state_dir"],"prior_request_id":req["prior_request_id"],"model":req["model"],"effort":req["effort"],"action_id":req["action_id"],"action_version":req["action_version"]})
    )?;
    log.sync_all()?;
    drop(_guard);
    if command == "confirm" {
        if req["action_id"] != "deep_research_start" || req["action_version"] != "a".repeat(64) {
            return Err(anyhow!("wrong confirmation identity"));
        }
        return Ok(
            json!({"version":1,"request_id":rid,"session_id":session,"kind":"progress","reason":"research_confirmation_submitted","completion_observed":false}),
        );
    }
    if command == "run" {
        let d = PathBuf::from(
            req["state_dir"]
                .as_str()
                .ok_or_else(|| anyhow!("missing state dir"))?,
        );
        let hash = format!("{:x}", Sha256::digest(rid.as_bytes()));
        let mut binding = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(d.join(format!("gemini-{hash}.json")))?;
        binding.write_all(&serde_json::to_vec(
            &json!({"requestID":rid,"sessionID":session,"prompt":req["prompt"],"capability":req["capability"],"model":req["model"],"effort":req["effort"]}),
        )?)?;

        std::thread::sleep(Duration::from_millis(if req["prompt"] == "dead-worker" {
            5000
        } else {
            1100
        }));
        if req["prompt"] == "fixture-research-plan" {
            return Ok(
                json!({"version":1,"request_id":rid,"session_id":session,"kind":"waiting_human","reason":"research_plan_confirmation","completion_observed":false,"pending_action":{"action_id":"deep_research_start","action_version":"a".repeat(64),"kind":"research_start","scope":"owned_research_plan"}}),
            );
        }
        if let Some(prompt) = req["prompt"]
            .as_str()
            .filter(|p| p.starts_with("mode-proof-"))
        {
            let mut receipt = json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"owned_response_action_observed","completion_observed":true,"text":"untrusted"});
            match prompt {
                "mode-proof-completed-and-failed" => receipt["failure_observed"] = json!(true),
                "mode-proof-not-submitted-and-failed" => {
                    receipt["kind"] = json!("error");
                    receipt["completion_observed"] = json!(false);
                    receipt["failure_observed"] = json!(true);
                    receipt["not_submitted"] = json!(true);
                    receipt["text"] = Value::Null;
                }
                "mode-proof-cancelled-and-failed" => {
                    receipt["reason"] = json!("cancelled");
                    receipt["completion_observed"] = json!(false);
                    receipt["failure_observed"] = json!(true);
                    receipt["text"] = Value::Null;
                }
                "mode-proof-completed-and-not-submitted" => receipt["not_submitted"] = json!(true),
                "mode-proof-failed-and-not-submitted" => {
                    receipt["kind"] = json!("failed");
                    receipt["reason"] = json!("native_error_response");
                    receipt["completion_observed"] = json!(false);
                    receipt["failure_observed"] = json!(true);
                    receipt["not_submitted"] = json!(true);
                }
                "mode-proof-malformed-flag" => receipt["failure_observed"] = json!("false"),
                _ => return Err(anyhow!("unknown fixture proof case")),
            }
            return Ok(receipt);
        }
        if req["prompt"] == "mode-failure-contradictory" {
            return Ok(
                json!({"version":1,"request_id":rid,"session_id":session,"kind":"failed","reason":"native_error_response","failure_observed":true,"completion_observed":true,"text":"diagnostic"}),
            );
        }
        if req["prompt"] == "mode-failure-no-diagnostic" {
            return Ok(
                json!({"version":1,"request_id":rid,"session_id":session,"kind":"failed","reason":"native_error_response","failure_observed":true,"completion_observed":false}),
            );
        }
        if req["prompt"] == "mode-failed-start" {
            return fixture_failure(rid, session, true, true, "native_error_response");
        }
        if req["prompt"] == "mode-failure-unproved" {
            return fixture_failure(rid, session, false, true, "native_error_response");
        }
        if req["prompt"] == "mode-failure-wrong-reason" {
            return fixture_failure(rid, session, true, true, "other");
        }
        if req["prompt"] == "mode-failure-foreign" {
            return fixture_failure(rid, session, true, false, "native_error_response");
        }
        if let Some(name) = req["prompt"]
            .as_str()
            .filter(|p| p.starts_with("fixture-canvas-") || p.starts_with("fixture-research-"))
        {
            let research = name.starts_with("fixture-research-");
            let case = name
                .trim_start_matches("fixture-canvas-")
                .trim_start_matches("fixture-research-");
            let proof_kind = if research {
                "research_report_text"
            } else {
                "canvas_document_text"
            };
            let text = if case == "empty" {
                ""
            } else {
                "画布文档：三条建议\n一、明确目标\n二、验证假设\n三、记录结果"
            };
            let mut receipt = json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"canvas_document_text_observed","completion_observed":true,"text":text});
            if case != "absent" {
                receipt["completion_evidence"] = json!({"kind":if case=="wrong-kind" {"chat_reply"} else {proof_kind},"request_id":if case=="foreign-request" {Uuid::new_v4().to_string()} else {rid.to_owned()},"session_id":if case=="foreign-session" {Uuid::new_v4().to_string()} else {session.to_owned()},"text_sha256":if case=="wrong-hash" {"0".repeat(64)} else {format!("{:x}",Sha256::digest(text.as_bytes()))}});
            }
            return Ok(receipt);
        }
        if req["prompt"] == "mode-text-start" {
            return Ok(
                json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"generic_text","completion_observed":true,"text":"placeholder"}),
            );
        }
        if req["prompt"] == "mode-wrong-start" {
            return fixture_wrong_media(&d, rid, session, req["capability"].as_str().unwrap_or(""));
        }
        if req["prompt"] == "fixture-bound-window-lost" {
            return Ok(
                json!({"version":1,"request_id":rid,"session_id":session,"kind":"uncertain","reason":"bound_window_lost","recovery":"when_safe_quit_original_app_then_resume","completion_observed":false}),
            );
        }
        if req["prompt"]
            .as_str()
            .is_some_and(|x| x.starts_with("fixture-artifact-"))
        {
            let name = req["prompt"].as_str().unwrap();
            if name == "fixture-artifact-empty" {
                return Ok(
                    json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"native_export_complete","completion_observed":true,"artifacts":[]}),
                );
            }
            let artifacts = d.join("artifacts");
            fs::create_dir(&artifacts)?;
            fs::set_permissions(&artifacts, fs::Permissions::from_mode(0o700))?;
            let hex = "89504e470d0a1a0a0000000d4948445200000001000000010804000000b51c0c020000000b4944415478da6364f80f00010501012718e3660000000049454e44ae426082";
            let mut bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect();
            if name == "fixture-artifact-wrong-type" {
                bytes[0] = 0;
            }
            let path = artifacts.join("image.png");
            if name == "fixture-artifact-symlink" {
                let other = d.join("outside.png");
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&other)?;
                f.write_all(&bytes)?;
                std::os::unix::fs::symlink(&other, &path)?;
            } else if name != "fixture-artifact-missing" {
                let mut f = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                f.write_all(&bytes)?;
                f.sync_all()?;
                if name == "fixture-artifact-public" {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
                }
            }
            let reported_path = if name == "fixture-artifact-escape" {
                d.join("outside.png")
            } else {
                path
            };
            let artifact = json!({"artifact_id":format!("image-{rid}"),"kind":if name=="fixture-artifact-preview" {"preview"} else {"image"},"mime_type":"image/png","path":reported_path,"byte_length":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes)),"request_id":if name=="fixture-artifact-foreign" {Uuid::new_v4().to_string()} else {rid.to_owned()},"session_id":if name=="fixture-artifact-foreign-session" {Uuid::new_v4().to_string()} else {session.to_owned()}});
            let mut receipt = json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"native_export_complete","completion_observed":true,"artifacts":[artifact]});
            if name == "fixture-artifact-contradictory" {
                receipt["failure_observed"] = json!(true)
            }
            return Ok(receipt);
        }
        if req["prompt"] == "foreign-receipt" {
            return Ok(
                json!({"version":1,"request_id":rid,"session_id":Uuid::new_v4().to_string(),"kind":"completed","reason":"owned_response_action_observed","completion_observed":true,"text":"forged"}),
            );
        }
        return Ok(
            json!({"version":1,"request_id":rid,"session_id":session,"kind":"waiting_human","reason":"fixture_dialog","completion_observed":false}),
        );
    }
    if command == "resume" {
        let d = PathBuf::from(
            req["state_dir"]
                .as_str()
                .ok_or_else(|| anyhow!("missing state dir"))?,
        );
        let hash = format!("{:x}", Sha256::digest(rid.as_bytes()));
        let binding: Value =
            serde_json::from_slice(&fs::read(d.join(format!("gemini-{hash}.json")))?)?;
        if binding["prompt"] == "mode-failed-resume" {
            return fixture_failure(rid, session, true, true, "native_error_response");
        }
        if binding["prompt"] == "mode-text-resume" {
            return Ok(
                json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"generic_text","completion_observed":true,"text":"placeholder"}),
            );
        }
        if binding["prompt"] == "mode-wrong-resume" {
            return fixture_wrong_media(
                &d,
                rid,
                session,
                binding["capability"].as_str().unwrap_or(""),
            );
        }
    }
    if command == "cancel" {
        return Ok(
            json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"cancelled","completion_observed":false}),
        );
    }
    Ok(
        json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"owned_response_action_observed","completion_observed":true,"text":"fixture answer"}),
    )
}
#[cfg(feature = "test-fixture")]
fn fixture_failure(
    rid: &str,
    session: &str,
    proof: bool,
    identity: bool,
    reason: &str,
) -> Result<Value> {
    Ok(
        json!({"version":1,"request_id":if identity {rid.to_owned()} else {Uuid::new_v4().to_string()},"session_id":session,"kind":"failed","reason":reason,"failure_observed":proof,"completion_observed":false,"text":"The native image request failed."}),
    )
}
#[cfg(feature = "test-fixture")]
fn fixture_wrong_media(d: &Path, rid: &str, session: &str, requested: &str) -> Result<Value> {
    let folder = d.join("artifacts");
    fs::create_dir(&folder)?;
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700))?;
    let (kind, mime, bytes): (&str, &str, Vec<u8>) = if requested == "image" {
        ("video", "video/mp4", b"\0\0\0\x10ftypisom\0\0\0\0".to_vec())
    } else {
        let h = "89504e470d0a1a0a0000000d4948445200000001000000010804000000b51c0c020000000b4944415478da6364f80f00010501012718e3660000000049454e44ae426082";
        (
            "image",
            "image/png",
            (0..h.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
                .collect(),
        )
    };
    let path = folder.join("wrong-media");
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    f.write_all(&bytes)?;
    f.sync_all()?;
    Ok(
        json!({"version":1,"request_id":rid,"session_id":session,"kind":"completed","reason":"native_export_complete","completion_observed":true,"artifacts":[{"artifact_id":format!("wrong-{rid}"),"kind":kind,"mime_type":mime,"path":path,"byte_length":bytes.len(),"sha256":format!("{:x}",Sha256::digest(&bytes)),"request_id":rid,"session_id":session}]}),
    )
}
fn launch(p: &Path, o: &mut Operation, command: &str) -> Result<()> {
    let token = Uuid::new_v4().to_string();
    o.status = "unknown".into();
    o.reason = "launch_outcome_unknown".into();
    o.worker_token = Some(token.clone());
    o.worker_command = Some(command.into());
    o.worker_pid = None;
    save(p, o)?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["--worker", &o.operation_id, command, &token])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(child) => {
            o.worker_pid = Some(child.id());
            o.status = "running".into();
            o.reason = "native_worker_started".into();
            std::thread::spawn(move || {
                let mut child = child;
                let _ = child.wait();
            });
        }
        Err(_) => o.reason = "worker_launch_failed".into(),
    };
    save(p, o)
}
fn native_binding_matches(prior: &Operation) -> bool {
    let result = (|| -> Result<bool> {
        let native = dir(&prior.native_dir_id)?.join("native");
        let folder = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(native)?;
        let m = folder.metadata()?;
        if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
            return Ok(false);
        }
        let name = CString::new(format!(
            "gemini-{:x}.json",
            Sha256::digest(prior.request_id.as_bytes())
        ))?;
        let fd = unsafe {
            libc::openat(
                folder.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Ok(false);
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
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
    previous: &str,
    mode: RequestMode,
) -> Result<std::result::Result<Operation, Value>> {
    let p = dir(previous)?;
    if !p.is_dir() {
        return Ok(Err(
            json!({"status":"blocked","reason":"prior_operation_not_found","operation_id":previous}),
        ));
    }
    let _lock = lock(&p)?;
    let candidate = match read(&p) {
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
    if mode == RequestMode::FreshAfter && !native_binding_matches(&candidate) {
        return Ok(Err(
            json!({"status":"blocked","reason":"prior_native_binding_unverified","operation_id":previous}),
        ));
    }
    Ok(Ok(candidate))
}
pub fn precheck_fresh_after(previous: &str) -> Result<Option<Value>> {
    let previous = uuid(previous)?;
    let root = root()?;
    let _root_lock = lock(&root)?;
    Ok(resolve_predecessor(&previous, RequestMode::FreshAfter)?.err())
}
pub fn existing_start(
    prompt: &str,
    id: &str,
    timeout: u64,
    capability: &str,
    previous: Option<&str>,
) -> Result<Option<Value>> {
    existing_start_with_selection(prompt, id, timeout, capability, previous, None, None)
}
pub fn existing_start_with_selection(
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
    let root = root()?;
    let _root_lock = lock(&root)?;
    let path = dir(&id)?;
    if !path.exists() {
        return Ok(None);
    }
    let _lock = lock(&path)?;
    let existing = match read(&path) {
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
pub fn start(
    prompt: &str,
    id: Option<&str>,
    timeout: u64,
    capability: &str,
    prior: Option<&str>,
) -> Result<Value> {
    start_with_selection(prompt, id, timeout, capability, prior, None, None)
}
pub fn start_with_selection(
    prompt: &str,
    id: Option<&str>,
    timeout: u64,
    capability: &str,
    prior: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Value> {
    start_with(
        prompt,
        id,
        timeout,
        capability,
        prior,
        if prior.is_some() {
            RequestMode::Continue
        } else {
            RequestMode::Start
        },
        model,
        effort,
        launch,
    )
}
pub fn start_after(
    prompt: &str,
    id: Option<&str>,
    timeout: u64,
    capability: &str,
    prior: &str,
) -> Result<Value> {
    start_after_with_selection(prompt, id, timeout, capability, prior, None, None)
}
pub fn start_after_with_selection(
    prompt: &str,
    id: Option<&str>,
    timeout: u64,
    capability: &str,
    prior: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Value> {
    start_with(
        prompt,
        id,
        timeout,
        capability,
        Some(prior),
        RequestMode::FreshAfter,
        model,
        effort,
        launch,
    )
}
#[allow(clippy::too_many_arguments)]
fn start_with(
    prompt: &str,
    id: Option<&str>,
    timeout: u64,
    capability: &str,
    prior: Option<&str>,
    mode: RequestMode,
    model: Option<&str>,
    effort: Option<&str>,
    launcher: fn(&Path, &mut Operation, &str) -> Result<()>,
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
    let root = root()?;
    let _root_lock = lock(&root)?;
    let path = dir(&id)?;
    // Resolve an already-persisted UUID before checking current predecessor state.
    if path.exists() {
        let _lock = lock(&path)?;
        let existing = match read(&path) {
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
        match resolve_predecessor(previous, mode)? {
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
pub fn focus(id: Option<&str>) -> Result<Value> {
    let req = if let Some(id) = id {
        let p = dir(id)?;
        if !p.is_dir() {
            return Ok(json!({"status":"not_found","operation_id":id}));
        }
        let _guard = lock(&p)?;
        let operation = read(&p)?;
        Some(
            json!({"request_id":operation.request_id,"session_id":operation.session_id,
            "state_dir":dir(&operation.native_dir_id)?.join("native")}),
        )
    } else {
        None
    };
    let result = native("focus", req.clone())?;
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

fn read_progress_text(o: &Operation) -> Option<String> {
    let native = dir(&o.native_dir_id).ok()?.join("native");
    let folder = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(native)
        .ok()?;
    let m = folder.metadata().ok()?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return None;
    }
    let name = CString::new(format!(
        "progress-{:x}.json",
        Sha256::digest(o.request_id.as_bytes())
    ))
    .ok()?;
    let fd = unsafe {
        libc::openat(
            folder.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return None;
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata().ok()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
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
fn remove_progress_file(o: &Operation) {
    if let Ok(native) = dir(&o.native_dir_id).map(|d| d.join("native")) {
        let p = native.join(format!(
            "progress-{:x}.json",
            Sha256::digest(o.request_id.as_bytes())
        ));
        let _ = fs::remove_file(p);
    }
}
pub fn observe(id: &str) -> Result<Value> {
    let p = dir(id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    };
    let _l = lock(&p)?;
    let mut o = read(&p)?;
    if o.status == "running" && !alive(&o) {
        o.status = "unknown".into();
        o.reason = "worker_exited_without_receipt".into();
        save(&p, &o)?
    };
    if o.status != "running" {
        remove_progress_file(&o);
    }
    let mut out = o.public();
    if o.status == "running" && out.get("text").is_none_or(Value::is_null) {
        if let Some(partial) = read_progress_text(&o) {
            out["text"] = json!(partial);
        }
    }
    Ok(out)
}
pub fn collect(id: &str) -> Result<Value> {
    let p = dir(id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    }
    let _lock = lock(&p)?;
    let o = read(&p)?;
    if o.status != "completed" {
        return Ok(o.public());
    }
    if crate::artifact::verify_all(&o, &o.artifacts).is_err() {
        return Ok(
            json!({"status":"unknown","reason":"artifact_validation_failed","operation_id":id}),
        );
    }
    let mut value =
        json!({"status":"completed","operation_id":o.operation_id,"artifacts":o.artifacts});
    if let Some(evidence) = o.completion_evidence {
        value["completion_evidence"] = serde_json::to_value(evidence)?;
    }
    if let Some(text) = o.text {
        value["text"] = json!(text);
        value["format"] = json!("plain_text");
    }
    Ok(value)
}
pub fn action(id: &str, command: &str) -> Result<Value> {
    if !["resume", "cancel"].contains(&command) {
        bail!("invalid action")
    }
    let p = dir(id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    };
    let _l = lock(&p)?;
    let mut o = read(&p)?;
    if terminal(&o.status) {
        return Ok(o.public());
    };
    if o.status == "running" && alive(&o) {
        let hash = format!("{:x}", Sha256::digest(o.request_id.as_bytes()));
        let binding = dir(&o.native_dir_id)?
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
    launch(&p, &mut o, command)?;
    Ok(o.public())
}
/// Confirm a specific native-owned research plan or tool consent action at most once per version.
pub fn confirm(id: &str, action_id: &str, action_version: &str) -> Result<Value> {
    let p = dir(id)?;
    if !p.is_dir() {
        return Ok(json!({"status":"not_found","operation_id":id}));
    }
    let _l = lock(&p)?;
    let mut o = read(&p)?;
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
    // launch persists the attempted version and intent before spawning the worker.
    launch(&p, &mut o, "confirm")?;
    Ok(o.public())
}
pub fn worker(id: &str, command: &str, token: &str) -> Result<()> {
    if !["run", "continue", "resume", "cancel", "confirm"].contains(&command) {
        bail!("bad command")
    };
    let p = dir(id)?;
    let o = {
        let _l = lock(&p)?;
        let o = read(&p)?;
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
    let mut req = json!({"request_id":o.request_id,"session_id":o.session_id,"state_dir":dir(&o.native_dir_id)?.join("native"),"timeout_ms":effective_worker_timeout});
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
        req["prior_request_id"] = json!(o.prior_request_id)
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
    let result = match native(command, Some(req)) {
        Ok(v) => v,
        Err(e) => json!({
            "_transport_error": "native_worker_error",
            "_transport_detail": e.to_string(),
        }),
    };
    apply_receipt(&p, token, result)
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
fn apply_receipt(p: &Path, token: &str, result: Value) -> Result<()> {
    let _l = lock(p)?;
    let mut current = read(p)?;
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
            serde_json::from_value::<Vec<crate::artifact::Artifact>>(result["artifacts"].clone())
        };
        match parsed {
            Ok(items) if crate::artifact::verify_all(&current, &items).is_ok() => items,
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
#[cfg(test)]
mod tests {
    use super::*;
    fn fake(p: &Path, o: &mut Operation, c: &str) -> Result<()> {
        o.status = "running".into();
        o.worker_pid = Some(std::process::id());
        o.worker_token = Some(c.into());
        o.worker_command = Some(c.into());
        save(p, o)
    }
    #[test]
    fn valid_native_proof_flags_remain_accepted() {
        assert!(proof_flags_consistent(
            &json!({"completion_observed":false,"not_submitted":true}),
            "error",
            "timeout_before_submit"
        ));
        assert!(proof_flags_consistent(
            &json!({"completion_observed":false,"not_submitted":true}),
            "error",
            "busy:generation_in_progress"
        ));
        assert!(proof_flags_consistent(
            &json!({"completion_observed":false}),
            "completed",
            "cancelled"
        ));
        assert!(proof_flags_consistent(
            &json!({"completion_observed":false,"failure_observed":true}),
            "failed",
            "native_error_response"
        ));
        assert!(proof_flags_consistent(
            &json!({"completion_observed":true}),
            "completed",
            "owned_response_action_observed"
        ));
    }
    #[test]
    fn durable_idempotency_unknown_followup_and_fencing() -> Result<()> {
        let d = tempfile::tempdir()?;
        unsafe { std::env::set_var("GEMINI_DESKTOP_MCP_STATE_DIR", d.path()) };
        let id = Uuid::new_v4().to_string();
        let first = start_with(
            "first",
            Some(&id),
            60000,
            "chat",
            None,
            RequestMode::Start,
            Some("gemini-4-carbon"),
            Some("high"),
            fake,
        )?;
        assert_eq!(first["status"], "running");
        assert_eq!(first["model"], "carbon");
        assert_eq!(first["effort"], "high");
        assert_eq!(
            start_with(
                "first",
                Some(&id),
                60000,
                "chat",
                None,
                RequestMode::Start,
                Some("carbon"),
                Some("extended"),
                fake
            )?["operation_id"],
            id
        );
        assert_eq!(
            start_with(
                "first",
                Some(&id),
                60000,
                "chat",
                None,
                RequestMode::Start,
                Some("argon"),
                Some("high"),
                fake
            )?["status"],
            "conflict"
        );
        assert_eq!(
            start_with(
                "different",
                Some(&id),
                60000,
                "chat",
                None,
                RequestMode::Start,
                Some("carbon"),
                Some("high"),
                fake
            )?["status"],
            "conflict"
        );
        let p = dir(&id)?;
        apply_receipt(
            &p,
            "stale",
            json!({"kind":"completed","completion_observed":true,"text":"wrong"}),
        )?;
        assert_eq!(read(&p)?.status, "running");
        let worker = read(&p)?;
        assert!(!alive(&worker));
        assert!(worker_matches(
            &format!("S /tmp/server --worker {} run run", id),
            &worker
        ));
        assert!(!worker_matches(
            &format!("Z /tmp/server --worker {} run run", id),
            &worker
        ));
        assert!(!worker_matches(
            &format!("S /tmp/server --worker {} run wrong", id),
            &worker
        ));
        apply_receipt(
            &p,
            "run",
            json!({"version":1,"request_id":"wrong","session_id":first["session_id"],"kind":"completed","completion_observed":true,"text":"forged"}),
        )?;
        assert_eq!(read(&p)?.reason, "native_receipt_identity_mismatch");
        apply_receipt(
            &p,
            "run",
            json!({"version":1,"request_id":id,"session_id":first["session_id"],"kind":"uncertain","reason":"bound_window_lost","recovery":"when_safe_quit_original_app_then_resume"}),
        )?;
        let guidance = observe(&id)?;
        assert_eq!(guidance["status"], "unknown");
        assert_eq!(
            guidance["recovery"],
            "when_safe_quit_original_app_then_resume"
        );
        assert!(guidance.get("prompt").is_none());
        apply_receipt(
            &p,
            "run",
            json!({"version":1,"request_id":"foreign","session_id":first["session_id"],"kind":"uncertain","reason":"bound_window_lost","recovery":"forged"}),
        )?;
        assert_eq!(
            observe(&id)?["recovery"],
            "when_safe_quit_original_app_then_resume"
        );

        for reason in ["native_timeout", "native_no_receipt", "native_worker_error"] {
            apply_receipt(&p, "run", json!({"_transport_error":reason}))?;
            assert_eq!(read(&p)?.reason, reason);
        }
        let req = json!({"prompt":"\n".repeat(65536)});
        let sink = File::create(d.path().join("no-reader-output"))?;
        let mut child = Command::new("/bin/sleep");
        child.arg("3");
        let now = Instant::now();
        assert!(!run_bounded(
            child,
            Some(&req),
            sink,
            Duration::from_millis(200)
        )?);
        assert!(now.elapsed() < Duration::from_secs(2));

        apply_receipt(
            &p,
            "run",
            json!({"version":1,"request_id":id,"session_id":first["session_id"],"kind":"error","not_submitted":true,"completion_observed":false,"reason":"busy:generation_in_progress"}),
        )?;
        let busy_obs = observe(&id)?;
        assert_eq!(busy_obs["status"], "not_submitted");
        assert_eq!(busy_obs["reason"], "busy:generation_in_progress");
        assert_eq!(action(&id, "resume")?["status"], "not_submitted");
        apply_receipt(
            &p,
            "run",
            json!({"version":1,"request_id":id,"session_id":first["session_id"],"kind":"waiting_human","reason":"dialog"}),
        )?;
        assert_eq!(observe(&id)?["status"], "waiting_human");
        apply_receipt(
            &p,
            "run",
            json!({"version":1,"request_id":id,"session_id":first["session_id"],"kind":"uncertain","reason":"timeout"}),
        )?;
        assert_eq!(observe(&id)?["status"], "unknown");
        apply_receipt(
            &p,
            "run",
            json!({"version":1,"request_id":id,"session_id":first["session_id"],"kind":"completed","completion_observed":true,"reason":"owned_response_action_observed","text":"answer"}),
        )?;
        assert_eq!(observe(&id)?["status"], "completed");
        let next = Uuid::new_v4().to_string();
        let child = start_with(
            "next",
            Some(&next),
            60000,
            "chat",
            Some(&id),
            RequestMode::Continue,
            None,
            None,
            fake,
        )?;
        assert_eq!(child["session_id"], first["session_id"]);
        assert_eq!(child["native_dir_id"], id);
        assert_eq!(child["prior_request_id"], id);
        let cp = dir(&next)?;
        apply_receipt(
            &cp,
            "continue",
            json!({"version":1,"request_id":next,"session_id":child["session_id"],"kind":"completed","reason":"cancelled","completion_observed":false}),
        )?;
        assert_eq!(observe(&next)?["status"], "cancelled");
        assert_eq!(action(&next, "cancel")?["status"], "cancelled");
        let sid = Uuid::new_v4().to_string();
        std::os::unix::fs::symlink(d.path(), d.path().join(&sid))?;
        assert!(dir(&sid).is_err());
        assert!(action(&next, "run").is_err());
        unsafe { std::env::remove_var("GEMINI_DESKTOP_MCP_STATE_DIR") };
        Ok(())
    }
}
