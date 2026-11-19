//! Dynamic per-host agent client discovery and native desktop/CLI adapters.
//!
//! Enables every Octendra computer (macOS, Windows, Linux) to dynamically discover
//! and natively dock all locally installed or running AI agent clients — including
//! `Gemini.app`, `Antigravity` Desktop App (`language_server` ConnectRPC Hub + `agy` ACP),
//! `Codex` Desktop App (`ChatGPT.app` / `Codex` `app-server` + `~/.codex/ipc`),
//! `Claude Code`, `Gemini CLI`, `Ollama`, `LM Studio`, and `Praxis` — without
//! hardcoded per-machine tables.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex as StdMutex, OnceLock},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};

use crate::{
    AgentRouterState, AppModelOption, AppSlashCommand, MAC_STUDIO_DEVICE_ID, RegisteredApp,
    RouterSessionRecord, SessionMessageItem, TurnRequest, TurnSnapshot,
    default_antigravity_slash_commands, default_cli_app_internal_tools,
    default_codex_slash_commands, default_gemini_internal_tools, default_gemini_slash_commands,
    enriched_path, home_dir, lookup_fleet_computer, now_ms,
};

const DISCOVERY_CACHE_TTL_MS: u64 = 4_000;
const MODEL_CATALOG_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const ANTIGRAVITY_RPC_PREFIX: &str = "/exa.language_server_pb.LanguageServerService/";
const MAX_CONNECT_RPC_BYTES: usize = 8 * 1024 * 1024;

struct LocalDiscoveryCache {
    captured_at_ms: u64,
    apps: Vec<RegisteredApp>,
}

static DISCOVERY_CACHE: OnceLock<StdMutex<Option<LocalDiscoveryCache>>> = OnceLock::new();

struct CliModelCacheEntry {
    captured_at_ms: u64,
    models: Vec<AppModelOption>,
}

static GEMINI_CLI_MODEL_CACHE: OnceLock<StdMutex<Option<CliModelCacheEntry>>> = OnceLock::new();

/// Clears the short-lived local discovery cache so the next query performs an immediate OS scan.
pub fn invalidate_discovery_cache() {
    if let Some(lock) = DISCOVERY_CACHE.get()
        && let Ok(mut guard) = lock.lock()
    {
        *guard = None;
    }
    if let Some(lock) = GEMINI_CLI_MODEL_CACHE.get()
        && let Ok(mut guard) = lock.lock()
    {
        *guard = None;
    }
}

/// Returns the most recently cached local applications immediately without blocking on OS discovery.
pub fn peek_cached_local_apps() -> Vec<RegisteredApp> {
    DISCOVERY_CACHE
        .get()
        .and_then(|lock| lock.lock().ok())
        .and_then(|guard| guard.as_ref().map(|entry| entry.apps.clone()))
        .unwrap_or_default()
}

/// Normalizes a device display name into a clean lowercase `app_id` host suffix.
pub fn host_slug(device_name: &str) -> String {
    let mut out = String::with_capacity(device_name.len());
    let mut prev_dash = false;
    for ch in device_name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "host".into()
    } else {
        out
    }
}

fn detect_os_hostname() -> String {
    for env_key in ["OCTENDRA_DEVICE_NAME", "COMPUTERNAME", "HOSTNAME"] {
        if let Ok(val) = std::env::var(env_key) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if let Ok(raw) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output()
        && out.status.success()
    {
        let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !raw.is_empty() {
            return raw
                .strip_suffix(".local")
                .unwrap_or(&raw)
                .trim()
                .to_string();
        }
    }
    "Local-Host".into()
}

fn current_platform_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// Dynamically resolves `(device_name, device_id, platform, default_root)` for any Octendra host.
pub fn dynamic_local_computer_identity() -> (String, String, String, String) {
    let home = home_dir();
    let mut detected_id = std::env::var("OCTENDRA_DEVICE_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut cfg_name: Option<String> = None;
    let mut cfg_root: Option<String> = None;

    let mut agent_cfg_candidates = vec![
        home.join(".local/share/octendra/agent/config/agent.json"),
        home.join("Octendra/agent/config/agent.json"),
        PathBuf::from("/var/home/liang/.local/share/octendra/agent/config/agent.json"),
    ];
    if let Some(local_app) = std::env::var_os("LOCALAPPDATA") {
        agent_cfg_candidates.push(
            PathBuf::from(local_app)
                .join("octendra")
                .join("agent")
                .join("config")
                .join("agent.json"),
        );
    }
    for path in agent_cfg_candidates {
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(val) = serde_json::from_str::<Value>(&raw)
        {
            if detected_id.is_none()
                && let Some(did) = val.get("device_id").and_then(Value::as_str)
                && !did.trim().is_empty()
            {
                detected_id = Some(did.trim().to_string());
            }
            if cfg_name.is_none() {
                cfg_name = val
                    .get("device_name")
                    .or_else(|| val.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
            }
            if cfg_root.is_none() {
                cfg_root = val
                    .get("roots")
                    .and_then(Value::as_array)
                    .and_then(|arr| arr.first())
                    .and_then(Value::as_str)
                    .map(|s| s.trim().trim_start_matches(r"\\?\").to_string())
                    .filter(|s| !s.is_empty());
            }
            if detected_id.is_some() {
                break;
            }
        }
    }

    if let Some(ref did) = detected_id
        && let Some(known) = lookup_fleet_computer(did)
    {
        return known;
    }

    let host_name = cfg_name.unwrap_or_else(detect_os_hostname);
    if let Some(known_by_name) = lookup_fleet_computer(&host_name) {
        let resolved_id = detected_id.unwrap_or(known_by_name.1);
        return (known_by_name.0, resolved_id, known_by_name.2, known_by_name.3);
    }

    let platform = current_platform_name().to_string();
    let fallback_root = cfg_root.unwrap_or_else(|| {
        let upper = home.join("Workspace");
        let lower = home.join("workspace");
        if upper.exists() {
            upper.to_string_lossy().into_owned()
        } else if lower.exists() {
            lower.to_string_lossy().into_owned()
        } else if cfg!(windows) {
            upper.to_string_lossy().into_owned()
        } else {
            lower.to_string_lossy().into_owned()
        }
    });
    let final_id = detected_id.unwrap_or_else(|| format!("local-{}", host_slug(&host_name)));
    (host_name, final_id, platform, fallback_root)
}

/// Discovered live Antigravity `LanguageServerService` Hub endpoint on `127.0.0.1`.
#[derive(Debug, Clone)]
pub struct AntigravityHubEndpoint {
    /// Host process ID of the running `language_server` Hub.
    pub pid: u32,
    /// Loopback ConnectRPC HTTP port.
    pub http_port: u16,
    /// Loopback `X-Codeium-Csrf-Token` value.
    pub csrf_token: String,
    /// Reported `lsVersion` string.
    pub ls_version: String,
    /// Preferred default model identifier.
    pub default_model_id: String,
    /// Preferred default protobuf model enum string.
    pub default_model_enum: String,
    /// Count of available agent models.
    pub model_count: usize,
    /// Count of existing Cascade trajectories on this host.
    pub trajectory_count: usize,
    /// Dynamically discovered selectable models reported by this Hub.
    pub available_models: Vec<AppModelOption>,
    /// Native `/` slash commands exposed by this Hub.
    pub slash_commands: Vec<AppSlashCommand>,
}

fn parse_raw_http_body(wire: &[u8]) -> Option<(u16, Vec<u8>)> {
    let sep = wire.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head_str = std::str::from_utf8(&wire[..sep]).ok()?;
    let mut lines = head_str.lines();
    let status_line = lines.next()?;
    let status_code: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    let body_raw = &wire[sep + 4..];
    if !chunked {
        return Some((status_code, body_raw.to_vec()));
    }
    let mut decoded = Vec::new();
    let mut cursor = body_raw;
    while !cursor.is_empty() {
        let crlf = cursor.windows(2).position(|w| w == b"\r\n")?;
        let len_hex = std::str::from_utf8(&cursor[..crlf]).ok()?.trim();
        let len_clean = len_hex.split(';').next().unwrap_or(len_hex).trim();
        let chunk_len = usize::from_str_radix(len_clean, 16).ok()?;
        cursor = &cursor[crlf + 2..];
        if chunk_len == 0 {
            break;
        }
        if cursor.len() < chunk_len {
            return None;
        }
        decoded.extend_from_slice(&cursor[..chunk_len]);
        cursor = &cursor[chunk_len..];
        if cursor.starts_with(b"\r\n") {
            cursor = &cursor[2..];
        }
    }
    Some((status_code, decoded))
}

/// Calls a loopback Antigravity ConnectRPC JSON method synchronously with bounded timeouts.
pub fn call_antigravity_connect_rpc_blocking(
    port: u16,
    csrf: &str,
    method: &str,
    body: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let payload = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("连接 Antigravity Hub 127.0.0.1:{port} 失败：{e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let header = format!(
        "POST {ANTIGRAVITY_RPC_PREFIX}{method} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nConnect-Protocol-Version: 1\r\nX-Codeium-Csrf-Token: {csrf}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(header.as_bytes())
        .and_then(|()| stream.write_all(&payload))
        .map_err(|e| format!("发送 Antigravity {method} 请求失败：{e}"))?;
    let mut wire = Vec::new();
    stream
        .take((MAX_CONNECT_RPC_BYTES + 65_536) as u64)
        .read_to_end(&mut wire)
        .map_err(|e| format!("读取 Antigravity {method} 响应失败：{e}"))?;
    let (status, decoded) =
        parse_raw_http_body(&wire).ok_or_else(|| format!("解析 Antigravity {method} HTTP 响应失败"))?;
    if status != 200 {
        let preview = String::from_utf8_lossy(&decoded);
        return Err(format!(
            "Antigravity {method} 返回 HTTP {status}：{}",
            preview.chars().take(240).collect::<String>()
        ));
    }
    serde_json::from_slice(&decoded).map_err(|e| format!("解析 Antigravity {method} JSON 失败：{e}"))
}

fn split_model_effort_suffix(slug: &str) -> Option<(&str, &str)> {
    for eff in ["minimal", "low", "medium", "high", "xhigh", "max"] {
        if let Some(base) = slug
            .strip_suffix(eff)
            .and_then(|s| s.strip_suffix('-'))
            .filter(|b| !b.is_empty())
        {
            return Some((base, eff));
        }
    }
    None
}

fn push_unique_app_model(out: &mut Vec<AppModelOption>, candidate: AppModelOption) {
    let trimmed_id = candidate.id.trim();
    if trimmed_id.is_empty() {
        return;
    }
    if let Some(existing) = out
        .iter_mut()
        .find(|m| m.id.eq_ignore_ascii_case(trimmed_id))
    {
        if existing.label.is_empty() && !candidate.label.is_empty() {
            existing.label = candidate.label;
        }
        if existing.default_effort.is_none() && candidate.default_effort.is_some() {
            existing.default_effort = candidate.default_effort;
        }
        for eff in candidate.supported_efforts {
            if !existing.supported_efforts.iter().any(|e| e == &eff) {
                existing.supported_efforts.push(eff);
            }
        }
        return;
    }
    out.push(candidate);
}

fn effort_sort_rank(e: &str) -> u8 {
    match e {
        "minimal" | "none" | "off" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" => 4,
        "max" => 5,
        _ => 9,
    }
}

/// Parses Antigravity `GetAvailableModels` response JSON into grouped `AppModelOption` items.
pub fn parse_antigravity_hub_models(models_resp: &Value) -> (Option<String>, Vec<AppModelOption>) {
    let Some(resp) = models_resp.get("response") else {
        return (None, Vec::new());
    };
    let Some(models_map) = resp.get("models").and_then(Value::as_object) else {
        return (None, Vec::new());
    };
    let raw_default = resp
        .get("defaultAgentModelId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut ordered_keys: Vec<String> = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    if let Some(def_id) = raw_default
        && seen_keys.insert(def_id.to_string())
    {
        ordered_keys.push(def_id.to_string());
    }
    if let Some(sorts) = resp.get("agentModelSorts").and_then(Value::as_array) {
        for s in sorts {
            if let Some(groups) = s.get("groups").and_then(Value::as_array) {
                for g in groups {
                    if let Some(ids) = g.get("modelIds").and_then(Value::as_array) {
                        for id_val in ids {
                            if let Some(id_str) = id_val.as_str()
                                && seen_keys.insert(id_str.to_string())
                            {
                                ordered_keys.push(id_str.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    let mut remaining: Vec<String> = models_map.keys().cloned().collect();
    remaining.sort();
    for k in remaining {
        if seen_keys.insert(k.clone()) {
            ordered_keys.push(k);
        }
    }

    let mut base_order: Vec<String> = Vec::new();
    let mut grouped: HashMap<String, (String, Vec<String>)> = HashMap::new();
    for key in ordered_keys {
        let Some(entry) = models_map.get(&key) else {
            continue;
        };
        let display_raw = entry
            .get("displayName")
            .or_else(|| entry.get("label"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(display) = display_raw else {
            // Skip internal/unlabeled slots (for example `chat_20706`).
            continue;
        };
        if let Some((base, eff)) = split_model_effort_suffix(&key) {
            let clean_title = display
                .split_once(" (")
                .map(|(prefix, _)| prefix.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| base.to_string());
            if !grouped.contains_key(base) {
                base_order.push(base.to_string());
            }
            let item = grouped
                .entry(base.to_string())
                .or_insert_with(|| (clean_title, Vec::new()));
            if !item.1.iter().any(|e| e == eff) {
                item.1.push(eff.to_string());
            }
        } else {
            if !grouped.contains_key(&key) {
                base_order.push(key.clone());
            }
            grouped
                .entry(key)
                .or_insert_with(|| (display.to_string(), Vec::new()));
        }
    }

    let default_base_and_eff = raw_default.map(|d| match split_model_effort_suffix(d) {
        Some((b, e)) => (b.to_string(), Some(e.to_string())),
        None => (d.to_string(), None),
    });

    let mut out = Vec::new();
    for base_id in base_order {
        if let Some((title, mut efforts)) = grouped.remove(&base_id) {
            efforts.sort_by_key(|e| effort_sort_rank(e));
            let def_effort = default_base_and_eff
                .as_ref()
                .filter(|(b, _)| b.eq_ignore_ascii_case(&base_id))
                .and_then(|(_, e)| e.clone())
                .or_else(|| {
                    if efforts.iter().any(|e| e == "medium") {
                        Some("medium".to_string())
                    } else {
                        efforts.first().cloned()
                    }
                });
            let label = if title.is_empty() || title.eq_ignore_ascii_case(&base_id) {
                base_id.clone()
            } else {
                format!("{title} ({base_id})")
            };
            push_unique_app_model(
                &mut out,
                AppModelOption {
                    id: base_id,
                    label,
                    default_effort: def_effort,
                    supported_efforts: efforts,
                    source: "antigravity_hub".into(),
                },
            );
        }
    }
    let default_model = default_base_and_eff
        .map(|(b, _)| b)
        .or_else(|| out.first().map(|m| m.id.clone()));
    (default_model, out)
}

fn select_default_antigravity_model(models_resp: &Value) -> Option<(String, String, usize)> {
    let resp = models_resp.get("response")?;
    let models_map = resp.get("models")?.as_object()?;
    if models_map.is_empty() {
        return None;
    }
    let mut ordered_candidates: Vec<String> = Vec::new();
    if let Some(def_id) = resp.get("defaultAgentModelId").and_then(Value::as_str)
        && !def_id.trim().is_empty()
    {
        ordered_candidates.push(def_id.trim().to_string());
    }
    if let Some(sorts) = resp.get("agentModelSorts").and_then(Value::as_array) {
        for s in sorts {
            if let Some(groups) = s.get("groups").and_then(Value::as_array) {
                for g in groups {
                    if let Some(ids) = g.get("modelIds").and_then(Value::as_array) {
                        for id in ids {
                            if let Some(id_str) = id.as_str() {
                                ordered_candidates.push(id_str.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    for k in models_map.keys() {
        ordered_candidates.push(k.clone());
    }
    for cid in ordered_candidates {
        if let Some(entry) = models_map.get(&cid)
            && let Some(enum_str) = entry.get("model").and_then(Value::as_str)
            && !enum_str.trim().is_empty()
        {
            return Some((cid, enum_str.trim().to_string(), models_map.len()));
        }
    }
    None
}

fn resolve_requested_antigravity_model(
    models_resp: &Value,
    requested_model: Option<&str>,
    requested_effort: Option<&str>,
    default_enum: &str,
) -> Result<String, String> {
    let Some(raw_model) = requested_model
        .map(str::trim)
        .filter(|s| !s.is_empty() && !matches!(*s, "auto" | "default"))
    else {
        return Ok(default_enum.to_string());
    };
    let models_map = models_resp
        .pointer("/response/models")
        .and_then(Value::as_object)
        .ok_or_else(|| "Antigravity 桌面端未返回可用模型列表".to_string())?;
    let mut candidates = Vec::new();
    if let Some(eff) = requested_effort
        .map(str::trim)
        .filter(|s| !s.is_empty() && !matches!(*s, "auto" | "default"))
    {
        candidates.push(format!("{raw_model}-{eff}"));
    }
    candidates.push(raw_model.to_string());
    candidates.push(format!("{raw_model}-medium"));
    candidates.push(format!("{raw_model}-high"));
    for cand in candidates {
        if let Some(enum_str) = models_map
            .get(&cand)
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            && !enum_str.trim().is_empty()
        {
            return Ok(enum_str.trim().to_string());
        }
    }
    for (k, v) in models_map {
        if k.eq_ignore_ascii_case(raw_model)
            || k.to_ascii_lowercase()
                .starts_with(&format!("{}-", raw_model.to_ascii_lowercase()))
        {
            if let Some(enum_str) = v.get("model").and_then(Value::as_str)
                && !enum_str.trim().is_empty()
            {
                return Ok(enum_str.trim().to_string());
            }
        }
    }
    Err(format!(
        "Antigravity 当前未开放或不支持指定模型 `{raw_model}`，请切换其他可用模型"
    ))
}

/// Scans `~/.gemini/antigravity/daemon/hub_*.json` for a live running Antigravity Hub.
pub fn discover_live_antigravity_hub() -> Option<AntigravityHubEndpoint> {
    let home = home_dir();
    let mut daemon_dirs = vec![home.join(".gemini").join("antigravity").join("daemon")];
    if cfg!(target_os = "linux")
        && let Some(user) = home.file_name().and_then(|n| n.to_str())
    {
        let alt = PathBuf::from(format!("/var/home/{user}/.gemini/antigravity/daemon"));
        if !daemon_dirs.contains(&alt) {
            daemon_dirs.push(alt);
        }
    }

    let mut hub_files: Vec<(u64, PathBuf)> = Vec::new();
    for dir in daemon_dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if fname.starts_with("hub_") && fname.ends_with(".json") {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since( std::time::UNIX_EPOCH ).ok())
                    .map_or(0, |d| d.as_secs());
                hub_files.push((mtime, path));
            }
        }
    }
    hub_files.sort_by(|a, b| b.0.cmp(&a.0));

    let mut best: Option<AntigravityHubEndpoint> = None;
    for (_, path) in hub_files.into_iter().take(6) {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let Some(port_u64) = val.get("httpPort").and_then(Value::as_u64) else {
            continue;
        };
        let Ok(http_port) = u16::try_from(port_u64) else {
            continue;
        };
        let Some(csrf) = val
            .get("csrfToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let pid = val
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);
        let ls_version = val
            .get("lsVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let probe_timeout = Duration::from_millis(320);
        let Ok(models_val) = call_antigravity_connect_rpc_blocking(
            http_port,
            csrf,
            "GetAvailableModels",
            &json!({}),
            probe_timeout,
        ) else {
            continue;
        };
        let Some((default_model_id, default_model_enum, model_count)) =
            select_default_antigravity_model(&models_val)
        else {
            continue;
        };
        let (_, hub_models) = parse_antigravity_hub_models(&models_val);
        let trajectory_count = call_antigravity_connect_rpc_blocking(
            http_port,
            csrf,
            "GetAllCascadeTrajectories",
            &json!({}),
            probe_timeout,
        )
        .ok()
        .and_then(|t| {
            t.get("trajectorySummaries")
                .and_then(Value::as_object)
                .map(serde_json::Map::len)
        })
        .unwrap_or(0);

        let mut slash_commands = default_antigravity_slash_commands();
        if let Ok(sc_val) = call_antigravity_connect_rpc_blocking(
            http_port,
            csrf,
            "GetSlashCommands",
            &json!({
                "cascadeConfig": { "plannerConfig": { "planModel": default_model_enum } }
            }),
            probe_timeout,
        ) && let Some(cmds) = sc_val.get("commands").and_then(Value::as_array)
        {
            for item in cmds.iter().take(16) {
                if let Some(name) = item
                    .pointer("/info/name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let cmd_tok = format!("/{name}");
                    if !slash_commands.iter().any(|c| c.command == cmd_tok) {
                        let desc = item
                            .pointer("/info/description")
                            .and_then(Value::as_str)
                            .unwrap_or("Antigravity 原生斜杠命令")
                            .to_string();
                        slash_commands.push(AppSlashCommand {
                            command: cmd_tok,
                            label: name.to_string(),
                            description: desc,
                        });
                    }
                }
            }
        }

        let candidate = AntigravityHubEndpoint {
            pid,
            http_port,
            csrf_token: csrf.to_string(),
            ls_version,
            default_model_id,
            default_model_enum,
            model_count,
            trajectory_count,
            available_models: hub_models,
            slash_commands,
        };
        let replace = match &best {
            None => true,
            Some(prev) => {
                (candidate.trajectory_count, candidate.model_count)
                    > (prev.trajectory_count, prev.model_count)
            }
        };
        if replace {
            best = Some(candidate);
        }
    }
    best
}

/// Finds the installed Antigravity Desktop `language_server` binary on macOS, Windows, or Linux.
pub fn detect_antigravity_language_server_bin() -> Option<String> {
    let home = home_dir().to_string_lossy().into_owned();
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        let localapp =
            std::env::var("LOCALAPPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Local"));
        let progfiles =
            std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
        candidates.push(PathBuf::from(format!(
            r"{localapp}\Programs\Antigravity\resources\bin\language_server.exe"
        )));
        candidates.push(PathBuf::from(format!(
            r"{progfiles}\Antigravity\resources\bin\language_server.exe"
        )));
    } else if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(
            "/Applications/Antigravity.app/Contents/Resources/bin/language_server",
        ));
        candidates.push(PathBuf::from(format!(
            "{home}/Applications/Antigravity.app/Contents/Resources/bin/language_server"
        )));
    } else {
        candidates.push(PathBuf::from(
            "/opt/antigravity/resources/bin/language_server",
        ));
        candidates.push(PathBuf::from(
            "/var/opt/antigravity/resources/bin/language_server",
        ));
        candidates.push(PathBuf::from(format!(
            "{home}/.local/opt/antigravity/resources/bin/language_server"
        )));
        if let Ok(entries) = std::fs::read_dir("/tmp") {
            for e in entries.flatten() {
                let p = e.path();
                if p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.starts_with("appimage_extracted_"))
                {
                    let ls = p.join("resources/bin/language_server");
                    if ls.is_file() {
                        candidates.push(ls);
                    }
                }
            }
        }
    }
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

fn search_executable_in_path(names: &[&str]) -> Option<String> {
    let path_str = enriched_path();
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in path_str.split(sep).filter(|s| !s.trim().is_empty()) {
        for name in names {
            let candidate = Path::new(dir.trim()).join(name);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}


/// Detects Codex runtime `(executable_spec, is_desktop_app_bundle, desktop_ipc_live)`.
pub fn detect_codex_runtime() -> Option<(String, bool, bool)> {
    let home_path = home_dir();
    let home = home_path.to_string_lossy().into_owned();
    let desktop_ipc_live = home_path.join(".codex/ipc/ipc.sock").exists();

    if cfg!(windows) {
        let appdata =
            std::env::var("APPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Roaming"));
        let localapp =
            std::env::var("LOCALAPPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Local"));
        let desktop_candidates = [
            format!(r"{localapp}\Programs\Codex\resources\codex-cli\codex.exe"),
            format!(r"{localapp}\Programs\ChatGPT\resources\codex-cli\codex.exe"),
            format!(r"{localapp}\Microsoft\WindowsApps\codex.exe"),
        ];
        for dc in desktop_candidates {
            if Path::new(&dc).is_file() {
                return Some((dc, true, desktop_ipc_live));
            }
        }
        let cli_candidates = [
            format!(r"{home}\.local\bin\codex.exe"),
            format!(r"{appdata}\npm\codex.cmd"),
            format!(r"{home}\.local\bin\codex.cmd"),
            format!(r"{home}\.cargo\bin\codex.exe"),
        ];
        for cc in cli_candidates {
            if Path::new(&cc).is_file() {
                return Some((cc, false, desktop_ipc_live));
            }
        }
        if let Some(found) = search_executable_in_path(&["codex.exe", "codex.cmd"]) {
            return Some((found, false, desktop_ipc_live));
        }
        return None;
    }

    let mac_desktop_bundles = [
        "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
        "/Applications/Codex.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    ];
    for mb in mac_desktop_bundles {
        if Path::new(mb).is_file() {
            return Some((mb.to_string(), true, desktop_ipc_live));
        }
    }

    let mut unix_candidates = vec![
        "/opt/homebrew/bin/codex".to_string(),
        format!("{home}/.local/bin/codex"),
        format!("{home}/.cargo/bin/codex"),
        format!("{home}/.npm-global/bin/codex"),
        format!("{home}/.volta/bin/codex"),
        "/usr/local/bin/codex".to_string(),
        "/usr/bin/codex".to_string(),
    ];
    let nvm_vers = home_path.join(".nvm/versions/node");
    if let Ok(entries) = std::fs::read_dir(&nvm_vers) {
        let mut node_bins: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path().join("bin/codex"))
            .filter(|p| p.is_file())
            .collect();
        node_bins.sort();
        node_bins.reverse();
        for nb in node_bins {
            unix_candidates.push(nb.to_string_lossy().into_owned());
        }
    }
    for uc in unix_candidates {
        if Path::new(&uc).is_file() {
            return Some((uc, false, desktop_ipc_live));
        }
    }
    search_executable_in_path(&["codex"]).map(|p| (p, false, desktop_ipc_live))
}

fn detect_claude_cli() -> Option<String> {
    let home = home_dir().to_string_lossy().into_owned();
    if cfg!(windows) {
        let appdata =
            std::env::var("APPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Roaming"));
        let localapp =
            std::env::var("LOCALAPPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Local"));
        for c in [
            format!(r"{home}\.local\bin\claude.exe"),
            format!(r"{appdata}\npm\claude.cmd"),
            format!(r"{localapp}\Programs\claude\claude.exe"),
        ] {
            if Path::new(&c).is_file() {
                return Some(c);
            }
        }
        return search_executable_in_path(&["claude.exe", "claude.cmd"]);
    }
    for c in [
        format!("{home}/.local/bin/claude"),
        "/opt/homebrew/bin/claude".into(),
        format!("{home}/.npm-global/bin/claude"),
        "/usr/local/bin/claude".into(),
        "/usr/bin/claude".into(),
    ] {
        if Path::new(&c).is_file() {
            return Some(c);
        }
    }
    search_executable_in_path(&["claude"])
}

fn detect_gemini_cli() -> Option<String> {
    let home = home_dir().to_string_lossy().into_owned();
    if cfg!(windows) {
        let appdata =
            std::env::var("APPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Roaming"));
        for c in [
            format!(r"{appdata}\npm\gemini.cmd"),
            format!(r"{home}\.local\bin\gemini.exe"),
            format!(r"{home}\.local\bin\gemini.cmd"),
        ] {
            if Path::new(&c).is_file() {
                return Some(c);
            }
        }
        return search_executable_in_path(&["gemini.cmd", "gemini.exe"]);
    }
    for c in [
        "/opt/homebrew/bin/gemini".into(),
        format!("{home}/.local/bin/gemini"),
        format!("{home}/.npm-global/bin/gemini"),
        "/usr/local/bin/gemini".into(),
        "/usr/bin/gemini".into(),
    ] {
        if Path::new(&c).is_file() && !c.ends_with("gemini-desktop-mcp") {
            return Some(c);
        }
    }
    search_executable_in_path(&["gemini"])
}

fn probe_http_json_loopback(port: u16, path: &str, timeout: Duration) -> Option<Value> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout).ok()?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut wire = Vec::new();
    stream.take(512 * 1024).read_to_end(&mut wire).ok()?;
    let (status, body) = parse_raw_http_body(&wire)?;
    if status != 200 {
        return None;
    }
    serde_json::from_slice(&body).ok()
}

fn probe_ollama_models() -> Option<Vec<String>> {
    let val = probe_http_json_loopback(11434, "/api/tags", Duration::from_millis(220))?;
    let models: Vec<String> = val
        .get("models")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|m| m.get("name").and_then(Value::as_str).map(str::to_string))
        .collect();
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

fn probe_lmstudio_models() -> Option<Vec<String>> {
    let val = probe_http_json_loopback(1234, "/v1/models", Duration::from_millis(180))?;
    let models: Vec<String> = val
        .get("data")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if models.is_empty() {
        None
    } else {
        Some(models)
    }
}

fn probe_praxis_live() -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 17420));
    TcpStream::connect_timeout(&addr, Duration::from_millis(150)).is_ok()
}

/// Returns dynamically discovered local agent clients on this computer (4s TTL cache).
pub fn discover_local_apps_cached() -> Vec<RegisteredApp> {
    let cache_mutex = DISCOVERY_CACHE.get_or_init(|| StdMutex::new(None));
    let now = now_ms();
    if let Ok(guard) = cache_mutex.lock()
        && let Some(ref entry) = *guard
        && now.saturating_sub(entry.captured_at_ms) <= DISCOVERY_CACHE_TTL_MS
    {
        return entry.apps.clone();
    }
    let fresh = discover_local_apps_uncached();
    if let Ok(mut guard) = cache_mutex.lock() {
        *guard = Some(LocalDiscoveryCache {
            captured_at_ms: now_ms(),
            apps: fresh.clone(),
        });
    }
    fresh
}

fn default_gemini_desktop_efforts() -> Vec<String> {
    ["low", "medium", "high", "xhigh", "max"]
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

fn discover_gemini_desktop_models(home: &str) -> (Option<String>, Vec<AppModelOption>, Vec<String>) {
    let default_efforts = default_gemini_desktop_efforts();
    let cache_path = Path::new(home).join("Library/Caches/GeminiDesktopMCP/discovered-models.json");
    let mut models = Vec::new();
    if let Ok(raw) = std::fs::read_to_string(cache_path)
        && let Ok(val) = serde_json::from_str::<Value>(&raw)
        && let Some(arr) = val.get("models").and_then(Value::as_array)
    {
        for item in arr {
            let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let label = item
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(id)
                .to_string();
            let def_eff = item
                .get("default_effort")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty());
            let mut effs: Vec<String> = item
                .get("supported_efforts")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            if effs.is_empty() && id != "gemini-desktop-auto" {
                effs = default_efforts.clone();
            }
            push_unique_app_model(
                &mut models,
                AppModelOption {
                    id: id.to_string(),
                    label,
                    default_effort: def_eff,
                    supported_efforts: effs,
                    source: "gemini_desktop_ax".into(),
                },
            );
        }
    }
    push_unique_app_model(
        &mut models,
        AppModelOption {
            id: "gemini-desktop-task".into(),
            label: "跟随 Gemini.app 当前会话设置 (gemini-desktop-task)".into(),
            default_effort: None,
            supported_efforts: default_efforts.clone(),
            source: "gemini_desktop_ax".into(),
        },
    );
    let def_model = models
        .iter()
        .find(|m| m.id != "gemini-desktop-auto" && m.id != "gemini-desktop-task")
        .or_else(|| models.first())
        .map(|m| m.id.clone());
    (def_model, models, default_efforts)
}

fn run_bounded_cli_stdout(bin: &str, args: &[&str], timeout: Duration) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .env("PATH", enriched_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() {
                return None;
            }
            let out = child.wait_with_output().ok()?;
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn discover_codex_models(home: &str) -> (Option<String>, Vec<AppModelOption>, Vec<String>) {
    let mut default_model: Option<String> = None;
    let cfg_path = Path::new(home).join(".codex/config.toml");
    if let Ok(raw) = std::fs::read_to_string(cfg_path) {
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') && default_model.is_some() {
                break;
            }
            if trimmed.starts_with('#') || trimmed.starts_with('[') {
                continue;
            }
            if let Some((k, v)) = trimmed.split_once('=')
                && k.trim() == "model"
            {
                let clean = v.trim().trim_matches(['"', '\'']).trim();
                if !clean.is_empty() {
                    default_model = Some(clean.to_string());
                    break;
                }
            }
        }
    }

    let mut out = Vec::new();
    let cache_path = Path::new(home).join(".codex/models_cache.json");
    if let Ok(raw) = std::fs::read_to_string(cache_path)
        && let Ok(val) = serde_json::from_str::<Value>(&raw)
        && let Some(models) = val.get("models").and_then(Value::as_array)
    {
        for m in models {
            let vis = m.get("visibility").and_then(Value::as_str).unwrap_or("list");
            if vis.eq_ignore_ascii_case("hide") {
                continue;
            }
            let Some(slug) = m
                .get("slug")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let display = m
                .get("display_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(slug);
            let label = if display.eq_ignore_ascii_case(slug) {
                slug.to_string()
            } else {
                format!("{display} ({slug})")
            };
            let def_eff = m
                .get("default_reasoning_level")
                .and_then(Value::as_str)
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty());
            let mut effs = Vec::new();
            if let Some(levels) = m.get("supported_reasoning_levels").and_then(Value::as_array) {
                for lv in levels {
                    if let Some(eff) = lv
                        .get("effort")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_ascii_lowercase())
                        .filter(|s| !s.is_empty())
                        && !effs.contains(&eff)
                    {
                        effs.push(eff);
                    }
                }
            }
            if effs.is_empty() {
                effs = vec!["low".into(), "medium".into(), "high".into()];
            }
            push_unique_app_model(
                &mut out,
                AppModelOption {
                    id: slug.to_string(),
                    label,
                    default_effort: def_eff,
                    supported_efforts: effs,
                    source: "codex_cache".into(),
                },
            );
        }
    }
    if default_model.is_none() {
        default_model = out.first().map(|m| m.id.clone());
    }
    let fallback_efforts = default_model
        .as_deref()
        .and_then(|dm| out.iter().find(|o| o.id.eq_ignore_ascii_case(dm)))
        .map(|o| o.supported_efforts.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["low".into(), "medium".into(), "high".into(), "xhigh".into()]);
    (default_model, out, fallback_efforts)
}

fn discover_gemini_cli_models(gcli_bin: &str) -> (Option<String>, Vec<AppModelOption>) {
    let cache = GEMINI_CLI_MODEL_CACHE.get_or_init(|| StdMutex::new(None));
    let now = now_ms();
    if let Ok(guard) = cache.lock()
        && let Some(ref entry) = *guard
        && now.saturating_sub(entry.captured_at_ms) <= MODEL_CATALOG_CACHE_TTL_MS
    {
        let def = entry.models.first().map(|m| m.id.clone());
        return (def, entry.models.clone());
    }
    let mut out = Vec::new();
    if let Some(txt) =
        run_bounded_cli_stdout(gcli_bin, &["--list-models", "-o", "json"], Duration::from_millis(3500))
        && let Ok(Value::Array(items)) = serde_json::from_str::<Value>(txt.trim())
    {
        for item in items {
            let Some(id) = item
                .get("modelId")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let display = item
                .get("displayName")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(id);
            let label = if display.eq_ignore_ascii_case(id) {
                id.to_string()
            } else {
                format!("{display} ({id})")
            };
            push_unique_app_model(
                &mut out,
                AppModelOption {
                    id: id.to_string(),
                    label,
                    default_effort: None,
                    supported_efforts: Vec::new(),
                    source: "gemini_cli".into(),
                },
            );
        }
    }
    if !out.is_empty()
        && let Ok(mut guard) = cache.lock()
    {
        *guard = Some(CliModelCacheEntry {
            captured_at_ms: now_ms(),
            models: out.clone(),
        });
    }
    let def = out.first().map(|m| m.id.clone());
    (def, out)
}

fn discover_claude_code_models(home: &str) -> (Option<String>, Vec<AppModelOption>, Vec<String>) {
    let mut out = Vec::new();
    let mut default_model: Option<String> = None;
    let efforts: Vec<String> = ["low", "medium", "high", "max"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let settings_path = Path::new(home).join(".claude/settings.json");
    if let Ok(raw) = std::fs::read_to_string(settings_path)
        && let Ok(val) = serde_json::from_str::<Value>(&raw)
    {
        let active_provider = val
            .get("activeProvider")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(ap) = active_provider
            && let Some(prov_model) = val
                .pointer(&format!("/providers/{ap}/model"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        {
            default_model = Some(prov_model.to_string());
            push_unique_app_model(
                &mut out,
                AppModelOption {
                    id: prov_model.to_string(),
                    label: format!("{prov_model} ({ap})"),
                    default_effort: None,
                    supported_efforts: efforts.clone(),
                    source: "claude_settings".into(),
                },
            );
        }
        if let Some(provs) = val.get("providers").and_then(Value::as_object) {
            for (pname, pobj) in provs {
                if let Some(m) = pobj
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    push_unique_app_model(
                        &mut out,
                        AppModelOption {
                            id: m.to_string(),
                            label: format!("{m} ({pname})"),
                            default_effort: None,
                            supported_efforts: efforts.clone(),
                            source: "claude_settings".into(),
                        },
                    );
                }
            }
        }
        if let Some(main_model) = val
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if default_model.is_none() {
                default_model = Some(main_model.to_string());
            }
            push_unique_app_model(
                &mut out,
                AppModelOption {
                    id: main_model.to_string(),
                    label: format!("Configured ({main_model})"),
                    default_effort: None,
                    supported_efforts: efforts.clone(),
                    source: "claude_settings".into(),
                },
            );
        }
    }
    for (alias, desc) in [
        ("opus", "Claude Opus Alias (opus)"),
        ("sonnet", "Claude Sonnet Alias (sonnet)"),
        ("haiku", "Claude Haiku Alias (haiku)"),
    ] {
        push_unique_app_model(
            &mut out,
            AppModelOption {
                id: alias.into(),
                label: desc.into(),
                default_effort: None,
                supported_efforts: efforts.clone(),
                source: "claude_alias".into(),
            },
        );
    }
    (default_model, out, efforts)
}

/// Performs a full non-invasive local OS scan for all installed and running AI agent clients.
pub fn discover_local_apps_uncached() -> Vec<RegisteredApp> {
    let (device_name, device_id, platform, default_cwd) = dynamic_local_computer_identity();
    let slug = host_slug(&device_name);
    let is_primary_hub = device_id == MAC_STUDIO_DEVICE_ID;
    let home = home_dir().to_string_lossy().into_owned();
    let mut apps = Vec::new();

    // 1. Gemini.app (macOS Native Desktop App, embedded in-process AX driver)
    let has_macos_gemini_app = cfg!(target_os = "macos")
        && (Path::new("/Applications/Gemini.app").exists()
            || Path::new(&format!("{home}/Applications/Gemini.app")).exists());
    if has_macos_gemini_app {
        let (default_model, available_models, supported_efforts) =
            discover_gemini_desktop_models(&home);
        let mut aliases = vec![
            format!("gemini@{slug}"),
            format!("gemini-{slug}"),
            format!("gemini.app@{slug}"),
            format!("Gemini · {device_name}"),
        ];
        if slug == "macbook-m2-max" {
            aliases.push("gemini@macbook".into());
            aliases.push("gemini-mbp".into());
        }
        if is_primary_hub {
            aliases.extend([
                "gemini".into(),
                "gemini.app".into(),
                "gemini_app".into(),
                "gemini-app".into(),
                "gemini_worker".into(),
                "灵犀".into(),
                "灵犀 · Gemini.app".into(),
            ]);
        }
        apps.push(RegisteredApp {
            app_id: format!("gemini_macos__{slug}"),
            name: "Gemini App".into(),
            display_name: format!("Gemini · {device_name}"),
            protocol: "gemini_desktop".into(),
            category: "connected_app".into(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            platform: platform.clone(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: default_cwd.clone(),
            executable: Some("builtin:gemini_desktop".into()),
            summary: format!(
                "{device_name} 本机 Gemini 桌面端应用，支持新建任务、多轮续接、/ 模式切换与 @ 扩展调用。"
            ),
            capabilities: vec![
                "桌面端原生".into(),
                "多轮对话".into(),
                "Deep Research".into(),
                "Canvas".into(),
                "@扩展工具".into(),
            ],
            aliases,
            slash_commands: default_gemini_slash_commands(),
            internal_tools: default_gemini_internal_tools(),
            available_models,
            default_model,
            supported_efforts,
        });
    }

    // 2. Antigravity Desktop App (Live ConnectRPC Hub ONLY — zero CLI/headless fallback)
    let live_agy_hub = discover_live_antigravity_hub();
    let agy_ls_bin = detect_antigravity_language_server_bin();
    if live_agy_hub.is_some() || agy_ls_bin.is_some() {
        let mut aliases = vec![
            format!("antigravity@{slug}"),
            format!("agy@{slug}"),
            format!("antigravity-{slug}"),
            format!("Antigravity · {device_name}"),
        ];
        if slug == "macbook-m2-max" {
            aliases.push("antigravity@macbook".into());
            aliases.push("agy@macbook".into());
        }
        if is_primary_hub {
            aliases.extend([
                "antigravity".into(),
                "antigravity.app".into(),
                "antigravity_worker".into(),
                "agy".into(),
                "凌空".into(),
                "凌空 · Antigravity".into(),
            ]);
        }
        let (online, exe, summary, slash_commands, caps, available_models, default_model) =
            if let Some(hub) = live_agy_hub {
                let def_base = split_model_effort_suffix(&hub.default_model_id)
                    .map(|(b, _)| b.to_string())
                    .unwrap_or_else(|| hub.default_model_id.clone());
                (
                    true,
                    agy_ls_bin,
                    format!(
                        "{device_name} 本机 Antigravity 桌面端（在线直连 Hub v{}，{} 个可用模型），实时同步桌面会话。",
                        hub.ls_version, hub.model_count
                    ),
                    hub.slash_commands,
                    vec![
                        "桌面端实时直连".into(),
                        "Cascade 多轮续接".into(),
                        "多模型切换".into(),
                        "原生斜杠命令".into(),
                    ],
                    hub.available_models,
                    Some(def_base),
                )
            } else {
                (
                    false,
                    agy_ls_bin,
                    format!(
                        "{device_name} 本机 Antigravity 桌面端当前未启动或未登录，请先打开 Antigravity 桌面应用。"
                    ),
                    default_antigravity_slash_commands(),
                    vec![
                        "需先打开桌面端".into(),
                        "Cascade 多轮续接".into(),
                        "多模型切换".into(),
                    ],
                    Vec::new(),
                    None,
                )
            };
        let agy_efforts = available_models
            .first()
            .map(|m| m.supported_efforts.clone())
            .filter(|v| !v.is_empty())
            .unwrap_or_default();
        apps.push(RegisteredApp {
            app_id: format!("antigravity__{slug}"),
            name: "Antigravity".into(),
            display_name: format!("Antigravity · {device_name}"),
            protocol: "antigravity_desktop".into(),
            category: "connected_app".into(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            platform: platform.clone(),
            is_local: true,
            online,
            is_custom: false,
            default_cwd: default_cwd.clone(),
            executable: exe,
            summary,
            capabilities: caps,
            aliases,
            slash_commands,
            internal_tools: default_cli_app_internal_tools(),
            available_models,
            default_model,
            supported_efforts: agy_efforts,
        });
    }

    // 3. Codex Desktop App (`ChatGPT.app` / `Codex.app`) & `codex app-server`
    if let Some((codex_bin, is_desktop_bundle, ipc_live)) = detect_codex_runtime() {
        let (default_model, available_models, supported_efforts) = discover_codex_models(&home);
        let mut aliases = vec![
            format!("codex@{slug}"),
            format!("codex-{slug}"),
            format!("codex.app@{slug}"),
            format!("Codex · {device_name}"),
        ];
        if slug == "macbook-m2-max" {
            aliases.push("codex@macbook".into());
            aliases.push("codex-mbp".into());
        }
        if is_primary_hub {
            aliases.extend([
                "codex".into(),
                "codex.app".into(),
                "codex_app".into(),
                "codex_worker".into(),
                "openai_codex".into(),
                "驭码".into(),
                "驭码 · Codex".into(),
            ]);
        }
        let mode_desc = if is_desktop_bundle && ipc_live {
            "桌面端运行中，共享本机 ~/.codex 会话与内部工具"
        } else if is_desktop_bundle {
            "桌面端内置 Codex App-Server，共享 ~/.codex 状态"
        } else {
            "本机 Codex App-Server 持久多轮线程"
        };
        apps.push(RegisteredApp {
            app_id: format!("codex__{slug}"),
            name: if is_desktop_bundle {
                "Codex App".into()
            } else {
                "Codex".into()
            },
            display_name: format!("Codex · {device_name}"),
            protocol: "codex_app_server".into(),
            category: "connected_app".into(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            platform: platform.clone(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: default_cwd.clone(),
            executable: Some(codex_bin),
            summary: format!("{device_name} 上的 Codex 客户端（{mode_desc}）。"),
            capabilities: vec![
                "持久线程".into(),
                "多轮续接".into(),
                "@内部工具与插件".into(),
                "授权与选项回调".into(),
            ],
            aliases,
            slash_commands: default_codex_slash_commands(),
            internal_tools: default_cli_app_internal_tools(),
            available_models,
            default_model,
            supported_efforts,
        });
    }

    // 4. Claude Code (`claude` CLI)
    if let Some(claude_bin) = detect_claude_cli() {
        let (default_model, available_models, supported_efforts) = discover_claude_code_models(&home);
        let mut aliases = vec![
            format!("claude@{slug}"),
            format!("claude-{slug}"),
            format!("Claude · {device_name}"),
        ];
        if slug == "macbook-m2-max" {
            aliases.push("claude@macbook".into());
        }
        if is_primary_hub {
            aliases.extend(["claude".into(), "claude_code".into(), "claude-code".into()]);
        }
        apps.push(RegisteredApp {
            app_id: format!("claude__{slug}"),
            name: "Claude Code".into(),
            display_name: format!("Claude · {device_name}"),
            protocol: "claude_code".into(),
            category: "connected_app".into(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            platform: platform.clone(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: default_cwd.clone(),
            executable: Some(claude_bin),
            summary: format!(
                "{device_name} 本机 Claude Code 智能体客户端，支持原生 `--resume` 多轮会话续接。"
            ),
            capabilities: vec!["流式输出".into(), "原生多轮续接".into(), "本机工具调用".into()],
            aliases,
            slash_commands: vec![
                AppSlashCommand {
                    command: "/new".into(),
                    label: "新建会话".into(),
                    description: "开启全新 Claude 会话".into(),
                },
                AppSlashCommand {
                    command: "/review".into(),
                    label: "代码审阅".into(),
                    description: "审阅当前工作区改动与潜在问题".into(),
                },
            ],
            internal_tools: default_cli_app_internal_tools(),
            available_models,
            default_model,
            supported_efforts,
        });
    }

    // 5. Gemini CLI (`gemini` CLI — dynamically exposed alongside or when macOS Gemini.app is absent)
    if let Some(gcli_bin) = detect_gemini_cli() {
        let (default_model, available_models) = discover_gemini_cli_models(&gcli_bin);
        let mut aliases = vec![
            format!("gemini-cli@{slug}"),
            format!("gemini_cli@{slug}"),
        ];
        if !has_macos_gemini_app {
            aliases.push(format!("gemini@{slug}"));
            aliases.push(format!("gemini-{slug}"));
        }
        if is_primary_hub {
            aliases.extend(["gemini-cli".into(), "gemini_cli".into()]);
        }
        apps.push(RegisteredApp {
            app_id: format!("gemini_cli__{slug}"),
            name: "Gemini CLI".into(),
            display_name: format!("Gemini CLI · {device_name}"),
            protocol: "gemini_cli".into(),
            category: "connected_app".into(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            platform: platform.clone(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: default_cwd.clone(),
            executable: Some(gcli_bin),
            summary: format!("{device_name} 本机 Gemini CLI 终端智能体客户端。"),
            capabilities: vec!["本机终端执行".into(), "多轮上下文".into(), "MCP 扩展".into()],
            aliases,
            slash_commands: vec![AppSlashCommand {
                command: "/new".into(),
                label: "新建会话".into(),
                description: "清空多轮上下文历史".into(),
            }],
            internal_tools: default_cli_app_internal_tools(),
            available_models,
            default_model,
            supported_efforts: Vec::new(),
        });
    }

    // 6. Ollama (only registered online when running with >= 1 pulled local model)
    if let Some(ollama_models) = probe_ollama_models() {
        let preview = ollama_models
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let default_model = ollama_models.first().cloned();
        let available_models: Vec<AppModelOption> = ollama_models
            .into_iter()
            .map(|m| AppModelOption {
                label: m.clone(),
                id: m,
                default_effort: None,
                supported_efforts: Vec::new(),
                source: "ollama".into(),
            })
            .collect();
        let mut aliases = vec![format!("ollama@{slug}"), format!("ollama-{slug}")];
        if is_primary_hub {
            aliases.push("ollama".into());
        }
        apps.push(RegisteredApp {
            app_id: format!("ollama__{slug}"),
            name: "Ollama".into(),
            display_name: format!("Ollama · {device_name}"),
            protocol: "openai_compatible".into(),
            category: "connected_app".into(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            platform: platform.clone(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: default_cwd.clone(),
            executable: Some("http://127.0.0.1:11434/v1".into()),
            summary: format!(
                "{device_name} 本机 Ollama 本地模型推理（已就绪模型：{preview}）。"
            ),
            capabilities: vec!["本地推理".into(), "多轮对话".into()],
            aliases,
            slash_commands: vec![AppSlashCommand {
                command: "/new".into(),
                label: "重置会话".into(),
                description: "清空多轮上下文历史".into(),
            }],
            internal_tools: Vec::new(),
            available_models,
            default_model,
            supported_efforts: Vec::new(),
        });
    }

    // 7. LM Studio (registered when local OpenAI server on :1234 is active)
    if let Some(lms_models) = probe_lmstudio_models() {
        let preview = lms_models
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let default_model = lms_models.first().cloned();
        let available_models: Vec<AppModelOption> = lms_models
            .into_iter()
            .map(|m| AppModelOption {
                label: m.clone(),
                id: m,
                default_effort: None,
                supported_efforts: Vec::new(),
                source: "lmstudio".into(),
            })
            .collect();
        apps.push(RegisteredApp {
            app_id: format!("lmstudio__{slug}"),
            name: "LM Studio".into(),
            display_name: format!("LM Studio · {device_name}"),
            protocol: "openai_compatible".into(),
            category: "connected_app".into(),
            device_id: device_id.clone(),
            device_name: device_name.clone(),
            platform: platform.clone(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: default_cwd.clone(),
            executable: Some("http://127.0.0.1:1234/v1".into()),
            summary: format!("{device_name} 本机 LM Studio 推理服务（在线模型：{preview}）。"),
            capabilities: vec!["本地推理".into(), "多轮对话".into()],
            aliases: vec![format!("lmstudio@{slug}"), format!("lms@{slug}")],
            slash_commands: vec![AppSlashCommand {
                command: "/new".into(),
                label: "重置会话".into(),
                description: "清空多轮上下文历史".into(),
            }],
            internal_tools: Vec::new(),
            available_models,
            default_model,
            supported_efforts: Vec::new(),
        });
    }

    // 8. Praxis Project Orchestrator (registered on any host running Praxis :17420 or binary)
    let praxis_bin = crate::detect_local_binary("praxis");
    if probe_praxis_live() || praxis_bin.is_some() {
        let mut aliases = vec![format!("praxis@{slug}")];
        if is_primary_hub {
            aliases.extend([
                "praxis".into(),
                "praxis_pipeline".into(),
                "领航".into(),
                "领航 · Praxis".into(),
            ]);
        }
        apps.push(RegisteredApp {
            app_id: format!("praxis__{slug}"),
            name: "Praxis".into(),
            display_name: if is_primary_hub {
                "Praxis".into()
            } else {
                format!("Praxis · {device_name}")
            },
            protocol: "praxis".into(),
            category: "project_orchestrator".into(),
            device_id,
            device_name,
            platform,
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd,
            executable: praxis_bin,
            summary: "复杂工程项目总控（需求分析 -> 架构设计 -> 编码实现 -> 独立验收）；简单对话请直接选择对应设备客户端。"
                .into(),
            capabilities: vec!["复杂项目编排".into(), "多阶段状态机".into(), "跨机调度".into()],
            aliases,
            slash_commands: vec![
                AppSlashCommand {
                    command: "/direct".into(),
                    label: "轻量直连档位".into(),
                    description: "跳过多角色流水线，使用单步直连执行".into(),
                },
                AppSlashCommand {
                    command: "/full".into(),
                    label: "完整研发流水线".into(),
                    description: "按需求、架构、开发、测试四阶段推进复杂项目".into(),
                },
            ],
            internal_tools: default_cli_app_internal_tools(),
            available_models: Vec::new(),
            default_model: None,
            supported_efforts: Vec::new(),
        });
    }

    apps
}

async fn call_hub_rpc(
    hub: &AntigravityHubEndpoint,
    method: &str,
    body: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let port = hub.http_port;
    let csrf = hub.csrf_token.clone();
    let m = method.to_string();
    tokio::task::spawn_blocking(move || {
        call_antigravity_connect_rpc_blocking(port, &csrf, &m, &body, timeout)
    })
    .await
    .map_err(|e| format!("Antigravity RPC join 错误：{e}"))?
}

impl AgentRouterState {
    /// Auto- detects the first available model from an OpenAI-compatible `/models` endpoint.
    pub async fn detect_first_openai_model(base_url: &str) -> Option<String> {
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?;
        let resp = client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let val: Value = resp.json().await.ok()?;
        val.get("data")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|m| m.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn project_workspace_file_uri(cwd: &str, default_root: &str) -> Option<String> {
        let trimmed = cwd.trim();
        if trimmed.is_empty() {
            return None;
        }
        let path = std::fs::canonicalize(trimmed).unwrap_or_else(|_| PathBuf::from(trimmed));
        if !path.is_dir() {
            return None;
        }
        let norm = |p: &Path| {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .to_string()
        };
        let norm_cwd = norm(&path);
        let norm_home = norm(&home_dir());
        let norm_def = norm(Path::new(default_root.trim()));
        if path.parent().is_none()
            || norm_cwd.is_empty()
            || norm_cwd == "/"
            || (!norm_home.is_empty() && norm_cwd == norm_home)
            || (!norm_def.is_empty() && norm_cwd == norm_def)
        {
            return None;
        }
        reqwest::Url::from_file_path(&path).ok().map(Into::into)
    }

    /// Executes a turn strictly against a live logged-in Antigravity Desktop App (`LanguageServerService` Hub),
    /// returning an explicit error when the desktop app is not running (zero CLI/headless fallback).
    pub async fn run_local_antigravity_desktop_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        raw_prompt: &str,
        cwd: &str,
        turn_id: &str,
        prior_session: Option<RouterSessionRecord>,
    ) -> Result<TurnSnapshot, String> {
        let hub = tokio::task::spawn_blocking(discover_live_antigravity_hub)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| {
                format!(
                    "{} 桌面端未启动或未登录，请先打开并登录 Antigravity 桌面应用",
                    app.display_name
                )
            })?;

        let models_val = call_hub_rpc(&hub, "GetAvailableModels", json!({}), Duration::from_secs(5))
            .await
            .map_err(|e| format!("{} 查询可用模型列表失败：{e}", app.display_name))?;
        let model_enum = resolve_requested_antigravity_model(
            &models_val,
            req.model.as_deref(),
            req.effort.as_deref(),
            &hub.default_model_enum,
        )?;

        let explicit_cwd_switched = req
            .cwd
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
            && prior_session
                .as_ref()
                .and_then(|s| s.cwd.as_deref())
                .is_some_and(|prev| {
                    crate::AgentRouterState::normalize_cwd_for_compare(prev)
                        != crate::AgentRouterState::normalize_cwd_for_compare(cwd)
                });

        let mut reuse_cid: Option<String> = None;
        let mut baseline_steps: usize = 0;
        if !explicit_cwd_switched
            && let Some(existing_cid) = prior_session
                .as_ref()
                .and_then(|s| s.native_thread_id.as_deref())
                .filter(|s| !s.is_empty() && !s.starts_with("acp:"))
        {
            for attempt in 0..8 {
                let Ok(hist) = call_hub_rpc(
                    &hub,
                    "GetCascadeTrajectory",
                    json!({ "cascadeId": existing_cid }),
                    Duration::from_secs(5),
                )
                .await
                else {
                    break;
                };
                let status = hist
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("CASCADE_RUN_STATUS_IDLE");
                if status == "CASCADE_RUN_STATUS_RUNNING" && attempt < 7 {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    continue;
                }
                if !matches!(
                    status,
                    "CASCADE_RUN_STATUS_ERROR"
                        | "CASCADE_RUN_STATUS_FAILED"
                        | "CASCADE_RUN_STATUS_CANCELED"
                        | "CASCADE_RUN_STATUS_CANCELLED"
                ) {
                    baseline_steps = hist
                        .pointer("/trajectory/steps")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    reuse_cid = Some(existing_cid.to_string());
                }
                break;
            }
        }

        let cascade_id = if let Some(cid) = reuse_cid {
            cid
        } else {
            let mut start_body = json!({
                "source": "CORTEX_TRAJECTORY_SOURCE_CASCADE_CLIENT",
                "trajectoryType": "CORTEX_TRAJECTORY_TYPE_CASCADE"
            });
            let workspace_uri = Self::project_workspace_file_uri(cwd, &app.default_cwd);
            if let Some(ref uri) = workspace_uri {
                start_body["workspaceUris"] = json!([uri]);
            }
            let created = call_hub_rpc(&hub, "StartCascade", start_body, Duration::from_secs(8))
                .await
                .map_err(|e| format!("Antigravity 创建工作区对话失败：{e}"))?;
            baseline_steps = 0;
            created
                .get("cascadeId")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| "Antigravity StartCascade 未返回有效 cascadeId".to_string())?
                .to_string()
        };

        // Check if leading `/slash` matches a native Antigravity slash command
        let trimmed = raw_prompt.trim();
        let mut items = vec![json!({ "text": trimmed })];
        if let Some(after_slash) = trimmed.strip_prefix('/') {
            let (cmd_name, rest_text) = match after_slash.split_once(char::is_whitespace) {
                Some((n, r)) => (n.trim(), r.trim()),
                None => (after_slash.trim(), ""),
            };
            if !cmd_name.is_empty()
                && let Ok(cat) = call_hub_rpc(
                    &hub,
                    "GetSlashCommands",
                    json!({
                        "cascadeId": cascade_id,
                        "cascadeConfig": { "plannerConfig": { "planModel": model_enum } }
                    }),
                    Duration::from_secs(4),
                )
                .await
                && let Some(def) = cat
                    .get("commands")
                    .and_then(Value::as_array)
                    .and_then(|arr| {
                        arr.iter().find(|d| {
                            d.pointer("/info/name").and_then(Value::as_str) == Some(cmd_name)
                        })
                    })
                && let Some(info) = def.get("info").filter(|v| v.is_object())
            {
                let empty_args: BTreeMap<String, String> = BTreeMap::new();
                let mut slash_vec = vec![json!({
                    "item": {
                        "slashCommand": {
                            "info": info,
                            "argumentValues": empty_args,
                        }
                    }
                })];
                if !rest_text.is_empty() {
                    slash_vec.push(json!({ "text": rest_text }));
                }
                items = slash_vec;
            }
        }

        call_hub_rpc(
            &hub,
            "SendUserCascadeMessage",
            json!({
                "cascadeId": cascade_id,
                "clientMessageId": turn_id,
                "items": items,
                "media": [],
                "cascadeConfig": {
                    "plannerConfig": {
                        "planModel": model_enum
                    }
                }
            }),
            Duration::from_secs(12),
        )
        .await?;

        let mut streamed_so_far = String::new();
        let mut seen_tools: HashSet<String> = HashSet::new();
        let mut saw_activity = false;
        let mut consecutive_rpc_errors: u32 = 0;

        loop {
            tokio::time::sleep(Duration::from_millis(320)).await;
            let hist = match call_hub_rpc(
                &hub,
                "GetCascadeTrajectory",
                json!({ "cascadeId": cascade_id }),
                Duration::from_secs(8),
            )
            .await
            {
                Ok(v) => {
                    consecutive_rpc_errors = 0;
                    v
                }
                Err(e) => {
                    consecutive_rpc_errors += 1;
                    if consecutive_rpc_errors >= 10 {
                        return Err(format!(
                            "{} 状态同步中断（客户端可能已关闭）：{e}",
                            app.display_name
                        ));
                    }
                    continue;
                }
            };
            let run_status = hist
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN");
            if matches!(
                run_status,
                "CASCADE_RUN_STATUS_ERROR"
                    | "CASCADE_RUN_STATUS_FAILED"
                    | "CASCADE_RUN_STATUS_CANCELED"
                    | "CASCADE_RUN_STATUS_CANCELLED"
            ) {
                return Err(format!(
                    "{} 客户端返回终止状态：{run_status}",
                    app.display_name
                ));
            }
            let empty_steps = Vec::new();
            let steps = hist
                .pointer("/trajectory/steps")
                .and_then(Value::as_array)
                .unwrap_or(&empty_steps);

            if run_status != "CASCADE_RUN_STATUS_IDLE" || steps.len() > baseline_steps {
                saw_activity = true;
            }

            let mut latest_response = String::new();
            let mut has_done_planner_reply = false;
            let mut step_error_msg: Option<String> = None;
            for (offset, step) in steps.iter().enumerate().skip(baseline_steps) {
                let stype = step.get("type").and_then(Value::as_str).unwrap_or("");
                let sstatus = step.get("status").and_then(Value::as_str).unwrap_or("");
                if matches!(
                    sstatus,
                    "CORTEX_STEP_STATUS_ERROR"
                        | "CORTEX_STEP_STATUS_FAILED"
                        | "CORTEX_STEP_STATUS_CANCELED"
                        | "CORTEX_STEP_STATUS_CANCELLED"
                ) {
                    let detail = step
                        .pointer("/error/message")
                        .or_else(|| step.pointer("/error/userErrorMessage"))
                        .and_then(Value::as_str)
                        .unwrap_or(sstatus);
                    step_error_msg = Some(detail.to_string());
                }
                if stype == "CORTEX_STEP_TYPE_RUN_COMMAND" {
                    let key = format!("cmd_{offset}");
                    if seen_tools.insert(key) {
                        let cmd_preview = step
                            .pointer("/runCommand/commandLine")
                            .or_else(|| step.pointer("/runCommand/command"))
                            .and_then(Value::as_str)
                            .unwrap_or("shell command");
                        let short: String = cmd_preview.chars().take(64).collect();
                        self.emit_turn_tool(
                            app,
                            &req.caller_session_id,
                            turn_id,
                            &format!("执行命令：{short}"),
                        );
                    }
                } else if stype == "CORTEX_STEP_TYPE_PLANNER_RESPONSE"
                    && let Some(pr) = step.get("plannerResponse")
                {
                    if let Some(tcalls) = pr.get("toolCalls").and_then(Value::as_array) {
                        for tc in tcalls {
                            let tc_id = tc
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            if seen_tools.insert(tc_id)
                                && let Some(tname) = tc.get("name").and_then(Value::as_str)
                            {
                                self.emit_turn_tool(
                                    app,
                                    &req.caller_session_id,
                                    turn_id,
                                    &format!("调用工具：{tname}"),
                                );
                            }
                        }
                    }
                    let txt = pr
                        .get("modifiedResponse")
                        .or_else(|| pr.get("response"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if !txt.trim().is_empty() {
                        latest_response = txt.to_string();
                        if sstatus == "CORTEX_STEP_STATUS_DONE" {
                            has_done_planner_reply = true;
                        }
                    }
                }
            }

            if latest_response.len() > streamed_so_far.len()
                && latest_response.starts_with(&streamed_so_far)
            {
                let delta = &latest_response[streamed_so_far.len()..];
                self.emit_turn_delta(
                    app,
                    &req.caller_session_id,
                    turn_id,
                    delta,
                    &latest_response,
                );
                streamed_so_far = latest_response.clone();
            } else if !latest_response.is_empty() && latest_response != streamed_so_far {
                self.emit_turn_delta(
                    app,
                    &req.caller_session_id,
                    turn_id,
                    &latest_response,
                    &latest_response,
                );
                streamed_so_far = latest_response.clone();
            }

            if saw_activity && run_status == "CASCADE_RUN_STATUS_IDLE" {
                if latest_response.trim().is_empty()
                    && let Some(err_detail) = step_error_msg
                {
                    return Err(format!("{} 内部执行异常：{err_detail}", app.display_name));
                }
                if has_done_planner_reply || steps.len() > baseline_steps + 1 {
                    let new_turns = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
                    self.upsert_session(RouterSessionRecord {
                        caller_session_id: req.caller_session_id.clone(),
                        app_id: app.app_id.clone(),
                        native_thread_id: Some(cascade_id.clone()),
                        last_operation_id: None,
                        turn_count: new_turns,
                        model: req.model.clone(),
                        effort: req.effort.clone(),
                        cwd: Some(cwd.to_string()),
                        history: Vec::new(),
                        updated_at_ms: now_ms(),
                    });
                    let final_text = if latest_response.trim().is_empty() {
                        format!("{} 已完成执行。", app.display_name)
                    } else {
                        latest_response.trim().to_string()
                    };
                    let snap = TurnSnapshot {
                        turn_id: turn_id.to_string(),
                        caller_session_id: req.caller_session_id.clone(),
                        app_id: app.app_id.clone(),
                        app_display_name: app.display_name.clone(),
                        device_name: app.device_name.clone(),
                        status: "completed".into(),
                        reason: "completed".into(),
                        text: Some(final_text),
                        native_thread_id: Some(cascade_id),
                        turn_number: new_turns,
                        pending_permission: None,
                        pending_input: None,
                        suggested_options: Vec::new(),
                        events: vec!["completed".into()],
                        updated_at_ms: now_ms(),
                    };
                    self.update_turn(snap.clone());
                    return Ok(snap);
                }
            }
        }
    }

    /// Executes a multi-turn streaming session against local `Claude Code` (`claude` CLI).
    pub async fn run_local_claude_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        raw_prompt: &str,
        cwd: &str,
        turn_id: &str,
        prior_session: Option<RouterSessionRecord>,
    ) -> Result<TurnSnapshot, String> {
        let bin = app
            .executable
            .clone()
            .or_else(detect_claude_cli)
            .ok_or_else(|| "本机未检测到 claude 可执行文件".to_string())?;
        let prompt = if let Some(rest) = raw_prompt.strip_prefix("/review")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            format!("请审阅当前工作区代码改动与潜在风险：\n{}", rest.trim())
        } else {
            raw_prompt.to_string()
        };

        let mut args = vec![
            "-p".to_string(),
            prompt,
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--dangerously-skip-permissions".to_string(),
        ];
        let explicit_cwd_switched = req
            .cwd
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
            && prior_session
                .as_ref()
                .and_then(|s| s.cwd.as_deref())
                .is_some_and(|prev| {
                    crate::AgentRouterState::normalize_cwd_for_compare(prev)
                        != crate::AgentRouterState::normalize_cwd_for_compare(cwd)
                });
        if !explicit_cwd_switched
            && let Some(ref sid) = prior_session.as_ref().and_then(|s| s.native_thread_id.clone())
            && !sid.trim().is_empty()
        {
            args.push("--resume".to_string());
            args.push(sid.clone());
        }
        if let Some(m) = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !matches!(*s, "default" | "auto"))
        {
            args.push("--model".to_string());
            args.push(m.to_string());
        }

        let mut cmd = if cfg!(windows) && bin.to_ascii_lowercase().ends_with(".cmd") {
            let mut c = Command::new("cmd.exe");
            c.arg("/C").arg(&bin);
            c
        } else {
            Command::new(&bin)
        };
        cmd.args(&args)
            .env("PATH", enriched_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if Path::new(cwd).is_dir() {
            cmd.current_dir(cwd);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动 Claude Code 失败：{e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "无法读取 Claude stdout".to_string())?;
        let mut lines = BufReader::new(stdout).lines();

        let mut full_text = String::new();
        let mut claude_reported_error: Option<String> = None;
        let mut resolved_sid = prior_session
            .as_ref()
            .and_then(|s| s.native_thread_id.clone());

        while let Ok(Some(line)) = lines.next_line().await {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if let Some(sid) = ev.get("session_id").and_then(Value::as_str)
                && !sid.is_empty()
            {
                resolved_sid = Some(sid.to_string());
            }
            match ev.get("type").and_then(Value::as_str).unwrap_or("") {
                "assistant" => {
                    if let Some(arr) = ev.pointer("/message/content").and_then(Value::as_array) {
                        for item in arr {
                            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                                "text" => {
                                    if let Some(t) = item.get("text").and_then(Value::as_str)
                                        && !t.is_empty()
                                    {
                                        if !full_text.is_empty() && !t.starts_with(&full_text) {
                                            full_text.push('\n');
                                        }
                                        full_text.push_str(t);
                                        self.emit_turn_delta(
                                            app,
                                            &req.caller_session_id,
                                            turn_id,
                                            t,
                                            &full_text,
                                        );
                                    }
                                }
                                "tool_use" => {
                                    if let Some(name) = item.get("name").and_then(Value::as_str) {
                                        self.emit_turn_tool(
                                            app,
                                            &req.caller_session_id,
                                            turn_id,
                                            &format!("调用工具：{name}"),
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "result" => {
                    let is_err = ev.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                    if let Some(res_txt) = ev.get("result").and_then(Value::as_str)
                        && !res_txt.trim().is_empty()
                    {
                        if is_err {
                            claude_reported_error = Some(res_txt.trim().to_string());
                        } else {
                            full_text = res_txt.to_string();
                        }
                    }
                }
                _ => {}
            }
        }
        let status = child.wait().await.ok();
        if let Some(err_msg) = claude_reported_error {
            return Err(format!("{} 返回错误：{err_msg}", app.display_name));
        }
        if status.is_some_and(|s| !s.success()) && full_text.trim().is_empty() {
            return Err(format!("{} 进程异常退出", app.display_name));
        }

        let new_turns = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
        self.upsert_session(RouterSessionRecord {
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            native_thread_id: resolved_sid.clone(),
            last_operation_id: None,
            turn_count: new_turns,
            model: req.model.clone(),
            effort: req.effort.clone(),
            cwd: Some(cwd.to_string()),
            history: Vec::new(),
            updated_at_ms: now_ms(),
        });
        let snap = TurnSnapshot {
            turn_id: turn_id.to_string(),
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            app_display_name: app.display_name.clone(),
            device_name: app.device_name.clone(),
            status: "completed".into(),
            reason: "completed".into(),
            text: Some(if full_text.trim().is_empty() {
                format!("{} 已完成执行。", app.display_name)
            } else {
                full_text.trim().to_string()
            }),
            native_thread_id: resolved_sid,
            turn_number: new_turns,
            pending_permission: None,
            pending_input: None,
            suggested_options: Vec::new(),
            events: vec!["completed".into()],
            updated_at_ms: now_ms(),
        };
        self.update_turn(snap.clone());
        Ok(snap)
    }

    /// Executes a turn against local `Gemini CLI` (`gemini` CLI) with rolling context continuity.
    pub async fn run_local_gemini_cli_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        raw_prompt: &str,
        cwd: &str,
        turn_id: &str,
        prior_session: Option<RouterSessionRecord>,
    ) -> Result<TurnSnapshot, String> {
        let bin = app
            .executable
            .clone()
            .or_else(detect_gemini_cli)
            .ok_or_else(|| "本机未检测到 gemini CLI 可执行文件".to_string())?;
        let mut history = prior_session
            .as_ref()
            .map_or_else(Vec::new, |s| s.history.clone());

        let effective_prompt = if history.is_empty() {
            raw_prompt.to_string()
        } else {
            let mut ctx = String::from("以下是本会话前序对话记录：\n");
            for item in history.iter().rev().take(8).rev() {
                let role_label = if item.role == "user" { "用户" } else { "助手" };
                ctx.push_str(&format!("{role_label}: {}\n", item.content));
            }
            ctx.push_str(&format!("\n请继续回应用户的最新消息：\n{raw_prompt}"));
            ctx
        };

        let mut cmd = if cfg!(windows) && bin.to_ascii_lowercase().ends_with(".cmd") {
            let mut c = Command::new("cmd.exe");
            c.arg("/C").arg(&bin);
            c
        } else {
            Command::new(&bin)
        };
        cmd.args(["-p", &effective_prompt, "-y"])
            .env("PATH", enriched_path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(m) = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !matches!(*s, "default" | "auto"))
        {
            cmd.args(["-m", m]);
        }
        if Path::new(cwd).is_dir() {
            cmd.current_dir(cwd);
        }

        let child = cmd
            .spawn()
            .map_err(|e| format!("启动 Gemini CLI 失败：{e}"))?;
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("Gemini CLI 进程读取失败：{e}"))?;

        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() && stdout.is_empty() {
            return Err(format!("Gemini CLI 执行异常：{stderr}"));
        }
        let reply = if stdout.is_empty() { stderr } else { stdout };
        history.push(SessionMessageItem {
            role: "user".into(),
            content: raw_prompt.to_string(),
        });
        history.push(SessionMessageItem {
            role: "assistant".into(),
            content: reply.clone(),
        });
        if history.len() > 20 {
            let drain = history.len().saturating_sub(20);
            history.drain(0..drain);
        }
        let new_turns = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
        let tid = format!("gcli:{}", req.caller_session_id);
        self.upsert_session(RouterSessionRecord {
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            native_thread_id: Some(tid.clone()),
            last_operation_id: None,
            turn_count: new_turns,
            model: req.model.clone(),
            effort: req.effort.clone(),
            cwd: Some(cwd.to_string()),
            history,
            updated_at_ms: now_ms(),
        });
        let snap = TurnSnapshot {
            turn_id: turn_id.to_string(),
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            app_display_name: app.display_name.clone(),
            device_name: app.device_name.clone(),
            status: "completed".into(),
            reason: "completed".into(),
            text: Some(reply),
            native_thread_id: Some(tid),
            turn_number: new_turns,
            pending_permission: None,
            pending_input: None,
            suggested_options: Vec::new(),
            events: vec!["completed".into()],
            updated_at_ms: now_ms(),
        };
        self.update_turn(snap.clone());
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_antigravity_hub_models_with_dynamic_neon_and_efforts() {
        let resp = json!({
            "response": {
                "defaultAgentModelId": "gemini-4-neon-high",
                "agentModelSorts": [{
                    "displayName": "Recommended",
                    "groups": [{
                        "modelIds": [
                            "gemini-4-neon-high",
                            "gemini-4-neon-xhigh",
                            "gemini-4-neon-low",
                            "gemini-4-carbon-high"
                        ]
                    }]
                }],
                "models": {
                    "chat_20706": { "model": "MODEL_CHAT_20706" },
                    "gemini-4-neon-high": {
                        "displayName": "Gemini 4 Neon (High)",
                        "model": "MODEL_NEON"
                    },
                    "gemini-4-neon-xhigh": {
                        "displayName": "Gemini 4 Neon (XHigh)",
                        "model": "MODEL_NEON"
                    },
                    "gemini-4-neon-low": {
                        "displayName": "Gemini 4 Neon (Low)",
                        "model": "MODEL_NEON"
                    },
                    "gemini-4-carbon-high": {
                        "displayName": "Gemini 4 Carbon (High)",
                        "model": "MODEL_CARBON"
                    }
                }
            }
        });
        let (def, models) = parse_antigravity_hub_models(&resp);
        assert_eq!(def.as_deref(), Some("gemini-4-neon"));
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gemini-4-neon");
        assert_eq!(models[0].default_effort.as_deref(), Some("high"));
        assert_eq!(models[0].supported_efforts, vec!["low", "high", "xhigh"]);
        assert_eq!(models[1].id, "gemini-4-carbon");
    }

    #[test]
    fn registered_app_deserializes_legacy_payload_without_model_fields() {
        let legacy = json!({
            "app_id": "gemini_macos__old",
            "name": "Gemini App",
            "display_name": "Gemini · Old",
            "protocol": "gemini_desktop",
            "category": "connected_app",
            "device_id": "id-1",
            "device_name": "Old",
            "platform": "macos",
            "is_local": false,
            "online": true,
            "is_custom": false,
            "default_cwd": "/tmp",
            "summary": "legacy"
        });
        let parsed: RegisteredApp = serde_json::from_value(legacy).unwrap_or_else(|e| {
            panic!("legacy RegisteredApp should deserialize safely: {e}")
        });
        assert!(parsed.available_models.is_empty());
        assert!(parsed.default_model.is_none());
        assert!(parsed.supported_efforts.is_empty());
    }

    #[test]
    fn project_workspace_file_uri_excludes_root_and_generates_project_uri() {
        let root = std::env::temp_dir().join(format!("octendra-ws-uri-{}", uuid::Uuid::new_v4()));
        let proj = root.join("telos");
        std::fs::create_dir_all(&proj).unwrap();

        let root_str = root.to_string_lossy().into_owned();
        let proj_str = proj.to_string_lossy().into_owned();
        assert_eq!(
            AgentRouterState::project_workspace_file_uri(&root_str, &root_str),
            None
        );
        assert_eq!(AgentRouterState::project_workspace_file_uri("/", &root_str), None);
        let uri = AgentRouterState::project_workspace_file_uri(&proj_str, &root_str);
        assert!(
            uri.as_deref().is_some_and(|u| u.starts_with("file:///") && u.ends_with("/telos")),
            "unexpected uri: {uri:?}"
        );

        let _ = std::fs::remove_dir_all(root);
    }
}
