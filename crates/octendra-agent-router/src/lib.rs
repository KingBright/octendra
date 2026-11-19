//! Shared per-machine Host Agent Router for Octendra, Telos, and Praxis.
//!
//! Unifies:
//! 1. Discovery and user customization of AI desktop/CLI applications across Octendra computers
//!    without forcing poetic aliases or categorizing general apps as coding-only workers.
//! 2. Session-sticky native multi-turn thread continuity (`Gemini.app`, `Codex App-Server`,
//!    `Antigravity ACP`, and custom registered apps).
//! 3. Structured `/` slash command and `@` internal tool / extension discovery and pass-through.
//! 4. Non-invasive live progress tracking and native permission confirmation relay.

#![allow(
    clippy::too_many_lines,
    clippy::struct_excessive_bools,
    clippy::similar_names,
    clippy::needless_pass_by_value
)]

use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{Path as AxumPath, Query as AxumQuery, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, ChildStdin, Command},
    sync::{Mutex as AsyncMutex, Notify, mpsc, oneshot, watch},
};
use uuid::Uuid;

pub mod discovery;
pub(crate) mod gemini_desktop;

/// Default loopback port for the local Octendra Host Agent Router daemon.
pub const DEFAULT_ROUTER_PORT: u16 = 18790;

/// Canonical `Mac-Studio` Octendra device identifier.
pub const MAC_STUDIO_DEVICE_ID: &str = "8e88f10a-36e3-4bea-bf9a-5bdc1fc0ab0d";

/// Supported `/` slash command descriptor exposed by a connected application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSlashCommand {
    /// Leading slash command token (for example `"/deep-research"` or `"/plan"`).
    pub command: String,
    /// Concise human-readable title.
    pub label: String,
    /// Description shown in autocomplete menus.
    pub description: String,
}

/// Supported `@` internal tool or extension descriptor inside a connected application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInternalTool {
    /// `@` handle (for example `"@Octendra"` or `"@Google Workspace"`).
    pub handle: String,
    /// Concise human-readable title.
    pub label: String,
    /// Description shown in autocomplete menus.
    pub description: String,
}

/// Supported model option dynamically discovered for a connected application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppModelOption {
    /// Canonical model identifier passed when starting a turn.
    pub id: String,
    /// Human-readable display label.
    pub label: String,
    /// Default thinking or reasoning effort for this model when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    /// Supported thinking or reasoning effort levels for this model.
    #[serde(default)]
    pub supported_efforts: Vec<String>,
    /// Discovery source tag (for example `"gemini_desktop_ax"`, `"antigravity_hub"`, `"codex_cache"`).
    #[serde(default)]
    pub source: String,
}

/// Connected AI application descriptor registered on an Octendra computer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredApp {
    /// Unique identifier (for example `"gemini_macos__mac-studio"` or `"custom__my-agent"`).
    pub app_id: String,
    /// Clean application base name (for example `"Gemini"`, `"Codex"`, `"Antigravity"`).
    pub name: String,
    /// Clean display label including host when appropriate (for example `"Gemini · Mac-Studio"`).
    pub display_name: String,
    /// Execution protocol (`"gemini_desktop"`, `"codex_app_server"`, `"antigravity_acp"`, `"praxis"`, `"custom_command"`).
    pub protocol: String,
    /// Category tag (`"connected_app"` or `"project_orchestrator"`).
    pub category: String,
    /// Host Octendra device UUID.
    pub device_id: String,
    /// Host computer display name.
    pub device_name: String,
    /// Host OS platform (`"macos"`, `"windows"`, `"linux"`).
    pub platform: String,
    /// True when hosted on the local computer running this router instance.
    pub is_local: bool,
    /// True when the host computer and adapter are currently reachable.
    pub online: bool,
    /// True when this entry was created or customized by the user.
    pub is_custom: bool,
    /// Default working directory on the host computer.
    pub default_cwd: String,
    /// Optional custom executable path or endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Concise user-facing description.
    pub summary: String,
    /// Capability tags.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Backward-compatible lookup aliases.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Supported `/` slash commands inside this application.
    #[serde(default)]
    pub slash_commands: Vec<AppSlashCommand>,
    /// Supported `@` internal tools / extensions inside this application.
    #[serde(default)]
    pub internal_tools: Vec<AppInternalTool>,
    /// Dynamically discovered selectable models for this application.
    #[serde(default)]
    pub available_models: Vec<AppModelOption>,
    /// Default model identifier reported by this application, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Fallback list of supported thinking or reasoning effort levels.
    #[serde(default)]
    pub supported_efforts: Vec<String>,
}

/// Request payload for registering or updating a custom connected application.
#[derive(Debug, Clone, Deserialize)]
pub struct UpsertCustomAppRequest {
    /// Optional stable custom `app_id` (generated when omitted).
    #[serde(default)]
    pub app_id: Option<String>,
    /// Clean application name.
    pub name: String,
    /// Optional explicit display name (defaults to `"<name> · <device_name>"`).
    #[serde(default)]
    pub display_name: Option<String>,
    /// Protocol (`"gemini_desktop"`, `"codex_app_server"`, `"antigravity_acp"`, `"custom_command"`).
    pub protocol: String,
    /// Target computer name or Octendra device UUID.
    #[serde(default)]
    pub device: Option<String>,
    /// Optional default working directory.
    #[serde(default)]
    pub default_cwd: Option<String>,
    /// Optional executable path or command template.
    #[serde(default)]
    pub executable: Option<String>,
    /// Optional description.
    #[serde(default)]
    pub summary: Option<String>,
    /// Optional capability tags.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Optional custom `/` commands.
    #[serde(default)]
    pub slash_commands: Vec<AppSlashCommand>,
    /// Optional custom `@` tools.
    #[serde(default)]
    pub internal_tools: Vec<AppInternalTool>,
}

/// Stored message item for stateless or OpenAI-compatible multi-turn protocols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessageItem {
    /// Role (`"user"` or `"assistant"`).
    pub role: String,
    /// Message text content.
    pub content: String,
}

/// Persisted native session binding per `(caller_session_id, app_id)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterSessionRecord {
    /// Upstream caller conversation/session identifier (e.g. Telos `session_id`).
    pub caller_session_id: String,
    /// Resolved connected `app_id`.
    pub app_id: String,
    /// Native thread/session identifier (`threadId` for Codex, `sessionId` for ACP, `session_id` for Gemini).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_thread_id: Option<String>,
    /// Last completed `operation_id` for `Gemini.app` `gemini_continue`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_operation_id: Option<String>,
    /// Completed turn count in this native session.
    pub turn_count: u64,
    /// Last selected model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Last selected reasoning effort override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Bound working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Optional rolling history for `custom_command` and `openai_compatible` multi-turn sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<SessionMessageItem>,
    /// Unix epoch milliseconds of last update.
    pub updated_at_ms: u64,
}

/// Selectable decision button for an intercepted permission prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOptionItem {
    /// Decision identifier (`"allow_once"`, `"allow_session"`, `"deny"`).
    pub option_id: String,
    /// User-facing button label.
    pub label: String,
    /// Semantic kind (`"approve_once"`, `"approve_session"`, `"deny"`).
    pub kind: String,
}

fn default_permission_options() -> Vec<PermissionOptionItem> {
    vec![
        PermissionOptionItem {
            option_id: "allow_once".into(),
            label: "允许本次".into(),
            kind: "approve_once".into(),
        },
        PermissionOptionItem {
            option_id: "allow_session".into(),
            label: "本会话始终允许".into(),
            kind: "approve_session".into(),
        },
        PermissionOptionItem {
            option_id: "deny".into(),
            label: "拒绝".into(),
            kind: "deny".into(),
        },
    ]
}

/// Selectable option proposed by an AI agent during clarification or elicitation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputOptionItem {
    /// Stable option identifier or value.
    pub option_id: String,
    /// Concise option label shown on chip/button.
    pub label: String,
    /// Optional explanation for the option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Pending structured question / multiple-choice elicitation requested by an AI application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingUserInputRecord {
    /// Unique router user-input request identifier.
    pub input_id: String,
    /// Associated router `turn_id`.
    pub turn_id: String,
    /// Upstream `caller_session_id`.
    pub caller_session_id: String,
    /// Connected `app_id`.
    pub app_id: String,
    /// Connected `app_display_name`.
    pub app_display_name: String,
    /// Question header title.
    pub title: String,
    /// Question prompt body.
    pub prompt: String,
    /// Optional native question ID (for `item/tool/requestUserInput`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_question_id: Option<String>,
    /// Candidate choices offered by the AI.
    #[serde(default)]
    pub options: Vec<UserInputOptionItem>,
    /// Whether the user may type a custom text answer.
    pub allow_free_text: bool,
    /// Whether multiple options can be selected simultaneously.
    pub multi_select: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Pending native permission confirmation intercepted from an application.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingPermissionRecord {
    /// Unique router permission request identifier.
    pub permission_id: String,
    /// Associated router `turn_id`.
    pub turn_id: String,
    /// Upstream `caller_session_id`.
    pub caller_session_id: String,
    /// Connected `app_id`.
    pub app_id: String,
    /// Connected `app_display_name`.
    pub app_display_name: String,
    /// Permission category (`"tool_consent"`, `"research_start"`, `"command_approval"`, `"acp_permission"`).
    pub kind: String,
    /// Concise title.
    pub title: String,
    /// Detailed context summary shown to the user.
    pub summary: String,
    /// Selectable decision options (`allow_once`, `allow_session`, `deny`).
    #[serde(default = "default_permission_options")]
    pub options: Vec<PermissionOptionItem>,
    /// Native operation identifier (for `Gemini.app`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_operation_id: Option<String>,
    /// Native action identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// Native 64-hex action version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_version: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Request payload for `POST /v1/sessions/turn`.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnRequest {
    /// Upstream conversation/session ID (sticky native session key).
    pub caller_session_id: String,
    /// Target `app_id` or alias.
    pub app_id: String,
    /// Raw user prompt (preserving leading `/` slash commands and `@` internal tool mentions).
    pub prompt: String,
    /// Optional working directory override.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional model ID override.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional thinking effort override.
    #[serde(default)]
    pub effort: Option<String>,
    /// Optional timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Optional caller-supplied idempotent turn UUID.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Whether to wait synchronously for completion or `waiting_permission` (defaults to `true`).
    #[serde(default)]
    pub wait: Option<bool>,
    /// Maximum milliseconds to queue waiting when a single-window desktop app is busy.
    #[serde(default)]
    pub wait_for_idle_ms: Option<u64>,
    /// If true, pause and return `waiting_permission` when the target app asks for confirmation.
    #[serde(default)]
    pub interactive_permissions: Option<bool>,
    /// Optional explicit permission policy override (`"auto_safe"` | `"interactive"` | `"full_auto"`).
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Prevents recursive peer-to-peer failover when a turn has already been forwarded.
    #[serde(default)]
    pub disable_peer_failover: Option<bool>,
}

fn default_true_bool() -> bool {
    true
}

/// Request payload for `POST /v1/sessions/reset`.
#[derive(Debug, Clone, Deserialize)]
pub struct ResetSessionRequest {
    /// Upstream conversation/session ID.
    pub caller_session_id: String,
    /// Optional specific `app_id` (if omitted, resets all app bindings for `caller_session_id`).
    #[serde(default)]
    pub app_id: Option<String>,
}

/// Request payload for `POST /v1/permissions/respond`.
#[derive(Debug, Clone, Deserialize)]
pub struct RespondPermissionRequest {
    /// Permission request ID.
    pub permission_id: String,
    /// True to approve/allow, false to deny/cancel.
    #[serde(default = "default_true_bool")]
    pub approved: bool,
    /// Optional granular decision (`"allow_once"`, `"allow_session"`, `"deny"`).
    #[serde(default)]
    pub decision: Option<String>,
    /// Optional user note when denying or steering.
    #[serde(default)]
    pub note: Option<String>,
    /// Whether to wait for the resumed turn to finish (`true` by default).
    #[serde(default)]
    pub wait_for_turn: Option<bool>,
}

/// Request payload for `POST /v1/inputs/respond`.
#[derive(Debug, Clone, Deserialize)]
pub struct RespondUserInputRequest {
    /// User-input request ID.
    pub input_id: String,
    /// Selected option IDs or labels.
    #[serde(default)]
    pub selected_options: Vec<String>,
    /// Optional free-text answer or supplement.
    #[serde(default)]
    pub text: Option<String>,
    /// Whether to wait for the resumed turn to finish (`true` by default).
    #[serde(default)]
    pub wait_for_turn: Option<bool>,
}

/// Request payload for `POST /v1/apps/{app_id}/policy`.
#[derive(Debug, Clone, Deserialize)]
pub struct SetAppPolicyRequest {
    /// Policy mode (`"auto_safe"`, `"interactive"`, `"full_auto"`).
    pub permission_mode: String,
}

/// Real-time incremental router event emitted for streaming UIs (`GET /v1/events`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterLiveEvent {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Unix epoch milliseconds.
    pub timestamp_ms: u64,
    /// Event kind (`"turn_started"`, `"turn_delta"`, `"turn_tool"`, `"turn_paused"`, `"turn_completed"`, `"turn_failed"`, `"app_status"`).
    pub kind: String,
    /// Optional router `turn_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// Optional upstream `caller_session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_session_id: Option<String>,
    /// Optional `app_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// Optional host computer name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// Optional turn or app status string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Optional incremental text chunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    /// Optional human-readable tool/step summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Enriched view returned by `GET /v1/apps` with live runtime status and active turn counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichedRegisteredApp {
    /// Base registered application descriptor.
    #[serde(flatten)]
    pub app: RegisteredApp,
    /// Live runtime status (`"idle"`, `"busy"`, `"waiting_permission"`, `"waiting_input"`, `"offline"`).
    pub runtime_status: String,
    /// Count of currently running turns on this application.
    pub active_turns: usize,
    /// Effective permission mode (`"auto_safe"`, `"interactive"`, `"full_auto"`).
    pub permission_mode: String,
}

/// Live turn state snapshot returned by `/v1/sessions/turn` and `/v1/turns/{turn_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnSnapshot {
    /// Router turn identifier.
    pub turn_id: String,
    /// Upstream caller session identifier.
    pub caller_session_id: String,
    /// Resolved `app_id`.
    pub app_id: String,
    /// Resolved `display_name`.
    pub app_display_name: String,
    /// Host computer name.
    pub device_name: String,
    /// Status: `"running"` | `"waiting_permission"` | `"waiting_input"` | `"completed"` | `"busy"` | `"failed"`.
    pub status: String,
    /// Detailed machine-readable reason.
    pub reason: String,
    /// Final or latest incremental text output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Native thread or operation ID used for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_thread_id: Option<String>,
    /// Turn index in this session (`1` = first turn, `2+` = native multi-turn continuation).
    pub turn_number: u64,
    /// Pending permission details when `status == "waiting_permission"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission: Option<PendingPermissionRecord>,
    /// Pending structured user-input / elicitation details when `status == "waiting_input"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_input: Option<PendingUserInputRecord>,
    /// Optional quick-reply choice chips extracted from the assistant's response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_options: Vec<UserInputOptionItem>,
    /// Progress breadcrumbs.
    #[serde(default)]
    pub events: Vec<String>,
    /// Last update timestamp in milliseconds.
    pub updated_at_ms: u64,
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub(crate) fn home_dir() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h);
    }
    if let Some(up) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(up);
    }
    if cfg!(windows) {
        PathBuf::from(r"C:\Users\Public")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/Users/jinliang")
    } else {
        PathBuf::from("/tmp")
    }
}

pub(crate) fn enriched_path() -> String {
    let home = home_dir().to_string_lossy().into_owned();
    let current = std::env::var("PATH").unwrap_or_default();
    if cfg!(windows) {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Roaming"));
        let localappdata =
            std::env::var("LOCALAPPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Local"));
        format!(
            r"{home}\.local\bin;{home}\.cargo\bin;{appdata}\npm;{localappdata}\Programs\Ollama;{localappdata}\Programs\Antigravity IDE\bin;{localappdata}\Programs\Antigravity\bin;C:\Program Files\nodejs;{current}"
        )
    } else {
        format!(
            "{home}/.local/bin:{home}/.cargo/bin:{home}/.gemini/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:{current}"
        )
    }
}

/// Returns the default storage directory for the Octendra agent router.
pub fn state_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os("OCTENDRA_AGENT_ROUTER_DIR") {
        return PathBuf::from(custom);
    }
    if cfg!(windows)
        && let Some(local) = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())
    {
        return PathBuf::from(local).join("octendra").join("agent-router");
    }
    home_dir().join(".local/share/octendra/agent-router")
}

/// Detects the Octendra identity `(device_name, device_id, platform, default_root)` of the local computer.
pub fn local_computer_identity() -> (String, String, String, String) {
    discovery::dynamic_local_computer_identity()
}

pub(crate) fn lookup_fleet_computer(raw: &str) -> Option<(String, String, String, String)> {
    let key = raw.trim().to_ascii_lowercase();
    match key.as_str() {
        "mac-studio" | "mac_studio" | "macstudio" | "8e88f10a-36e3-4bea-bf9a-5bdc1fc0ab0d" => Some((
            "Mac-Studio".into(),
            MAC_STUDIO_DEVICE_ID.into(),
            "macos".into(),
            "/Users/jinliang/workspace".into(),
        )),
        "macbook-m2-max" | "macbook" | "mbp" | "171b2696-bec2-4bf7-b95d-1b93b81c063a" => Some((
            "MacBook-M2-Max".into(),
            "171b2696-bec2-4bf7-b95d-1b93b81c063a".into(),
            "macos".into(),
            "/Users/jinliang/Workspace".into(),
        )),
        "cube" | "cubex" | "0d732697-14ab-4e4e-a5f7-97b255b87c0a" => Some((
            "cube".into(),
            "0d732697-14ab-4e4e-a5f7-97b255b87c0a".into(),
            "windows".into(),
            r"C:\Users\Liang\Workspace".into(),
        )),
        "ho5" | "ab43eeaa-5483-441a-9ad1-499a2d50237d" => Some((
            "HO5".into(),
            "ab43eeaa-5483-441a-9ad1-499a2d50237d".into(),
            "linux".into(),
            "/var/home/liang/workspace".into(),
        )),
        "matrix" | "4db01ef7-4e1e-4c6b-aa77-da0534e8b4a5" => Some((
            "matrix".into(),
            "4db01ef7-4e1e-4c6b-aa77-da0534e8b4a5".into(),
            "windows".into(),
            r"C:\Users\jlkin\Workspace".into(),
        )),
        "reachy-mini" | "reachy" | "3542f65d-2e7a-49c8-b997-21ecbe8254ba" => Some((
            "Reachy-Mini".into(),
            "3542f65d-2e7a-49c8-b997-21ecbe8254ba".into(),
            "linux".into(),
            "/home/pollen/workspace".into(),
        )),
        _ => None,
    }
}

/// Default `/` slash commands for `Gemini.app`.
pub fn default_gemini_slash_commands() -> Vec<AppSlashCommand> {
    vec![
        AppSlashCommand {
            command: "/deep-research".into(),
            label: "Deep Research".into(),
            description: "启动深度研究模式，生成研究计划并输出完整报告".into(),
        },
        AppSlashCommand {
            command: "/canvas".into(),
            label: "Canvas 画布".into(),
            description: "打开 Canvas 协同画布编写长文档或代码".into(),
        },
        AppSlashCommand {
            command: "/image".into(),
            label: "生成图片".into(),
            description: "切换到原生图片生成模式".into(),
        },
        AppSlashCommand {
            command: "/video".into(),
            label: "生成视频".into(),
            description: "切换到原生视频生成模式".into(),
        },
        AppSlashCommand {
            command: "/new".into(),
            label: "新原生对话".into(),
            description: "在保持当前应用绑定的前提下重置底层对话窗口上下文".into(),
        },
    ]
}

/// Default `@` internal tools / extensions for `Gemini.app`.
pub fn default_gemini_internal_tools() -> Vec<AppInternalTool> {
    vec![
        AppInternalTool {
            handle: "@Octendra".into(),
            label: "Octendra 跨机设备与工作区".into(),
            description: "调用 Octendra 访问多台电脑的代码仓库、终端与桌面观察能力".into(),
        },
        AppInternalTool {
            handle: "@Blender".into(),
            label: "Blender 3D 场景".into(),
            description: "调用 Blender MCP 检查场景、创建网格材质与视口快照".into(),
        },
        AppInternalTool {
            handle: "@Google Workspace".into(),
            label: "Google Workspace".into(),
            description: "综合检索 Gmail、Drive、Docs、Sheets、Slides 与 Calendar".into(),
        },
        AppInternalTool {
            handle: "@Gmail".into(),
            label: "Gmail 邮件".into(),
            description: "搜索、阅读邮件线程或创建邮件草稿".into(),
        },
        AppInternalTool {
            handle: "@Google Drive".into(),
            label: "Google Drive 云端硬盘".into(),
            description: "搜索与读取云端硬盘文档和表格".into(),
        },
        AppInternalTool {
            handle: "@Google Calendar".into(),
            label: "Google Calendar 日历".into(),
            description: "查看日程安排与空闲时段".into(),
        },
        AppInternalTool {
            handle: "@YouTube".into(),
            label: "YouTube 视频".into(),
            description: "搜索视频或读取视频字幕与元数据".into(),
        },
    ]
}

/// Default `/` slash commands for `Codex`.
pub fn default_codex_slash_commands() -> Vec<AppSlashCommand> {
    vec![
        AppSlashCommand {
            command: "/plan".into(),
            label: "方案规划".into(),
            description: "先梳理改动边界与实施计划，再推进修改".into(),
        },
        AppSlashCommand {
            command: "/review".into(),
            label: "代码审阅".into(),
            description: "对当前工作区未提交变更进行缺陷与回归风险审阅".into(),
        },
        AppSlashCommand {
            command: "/compact".into(),
            label: "压缩上下文".into(),
            description: "调用 Codex 原生线程压缩以释放长对话上下文窗口".into(),
        },
        AppSlashCommand {
            command: "/fast".into(),
            label: "快速模式".into(),
            description: "以轻量低延迟思考档位快速作答".into(),
        },
        AppSlashCommand {
            command: "/new".into(),
            label: "新原生线程".into(),
            description: "开启全新 Codex 线程并保留当前应用持久绑定".into(),
        },
    ]
}

/// Default `@` internal tools for `Codex` and `Antigravity`.
pub fn default_cli_app_internal_tools() -> Vec<AppInternalTool> {
    vec![
        AppInternalTool {
            handle: "@Octendra".into(),
            label: "Octendra 跨机执行".into(),
            description: "通过 Octendra 访问其他物理机的目录与终端".into(),
        },
        AppInternalTool {
            handle: "@workspace".into(),
            label: "当前工作区上下文".into(),
            description: "聚焦绑定工作区目录内的文件结构与变更".into(),
        },
    ]
}

/// Default `/` slash commands for `Antigravity`.
pub fn default_antigravity_slash_commands() -> Vec<AppSlashCommand> {
    vec![
        AppSlashCommand {
            command: "/plan".into(),
            label: "规划模式".into(),
            description: "切换 ACP 会话至规划分析模式".into(),
        },
        AppSlashCommand {
            command: "/review".into(),
            label: "审阅改动".into(),
            description: "检查当前工作区代码差异与潜在问题".into(),
        },
        AppSlashCommand {
            command: "/new".into(),
            label: "新原生会话".into(),
            description: "开启全新 Antigravity ACP 会话并保持当前绑定".into(),
        },
    ]
}

/// Resolves known Octendra-enrolled computers (strictly excluding registered remote SSH hosts).
pub fn resolve_octendra_computer(raw: Option<&str>) -> (String, String, String, String, bool) {
    let (local_name, local_id, local_platform, local_root) = local_computer_identity();
    let key = raw.unwrap_or("").trim();
    if key.is_empty() || key.eq_ignore_ascii_case("local") || key.eq_ignore_ascii_case("localhost") {
        return (local_name, local_id, local_platform, local_root, true);
    }
    if let Some((name, did, platform, root)) = lookup_fleet_computer(key) {
        let is_local = did == local_id;
        return (name, did, platform, root, is_local);
    }
    (local_name, local_id, local_platform, local_root, true)
}

fn is_electron_gui_symlink(path: &Path) -> bool {
    std::fs::read_link(path).ok().is_some_and(|target| {
        let s = target.to_string_lossy();
        s.contains("/opt/antigravity/antigravity") || s.contains("Antigravity.AppImage")
    })
}

pub(crate) fn detect_local_binary(kind: &str) -> Option<String> {
    let home = home_dir().to_string_lossy().into_owned();
    let candidates: Vec<String> = match kind {
        "gemini_desktop" => {
            if cfg!(target_os = "macos") && Path::new("/Applications/Gemini.app").exists() {
                return Some("builtin:gemini_desktop".into());
            }
            return None;
        }
        "codex_app_server" => {
            if cfg!(windows) {
                let appdata =
                    std::env::var("APPDATA").unwrap_or_else(|_| format!(r"{home}\AppData\Roaming"));
                let localapp = std::env::var("LOCALAPPDATA")
                    .unwrap_or_else(|_| format!(r"{home}\AppData\Local"));
                vec![
                    format!(r"{appdata}\npm\codex.cmd"),
                    format!(r"{home}\.local\bin\codex.exe"),
                    format!(r"{home}\.local\bin\codex.cmd"),
                    format!(r"{home}\.cargo\bin\codex.exe"),
                    format!(r"{localapp}\Microsoft\WindowsApps\codex.exe"),
                ]
            } else {
                vec![
                    "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex".into(),
                    "/Applications/Codex.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex".into(),
                    "/opt/homebrew/bin/codex".into(),
                    format!("{home}/.local/bin/codex"),
                    format!("{home}/.cargo/bin/codex"),
                    "/usr/local/bin/codex".into(),
                    "/usr/bin/codex".into(),
                ]
            }
        }
        "antigravity_acp" => {
            if cfg!(windows) {
                vec![
                    format!(r"{home}\.local\bin\agy.exe"),
                    format!(r"{home}\.gemini\bin\agy.exe"),
                ]
            } else {
                vec![
                    format!("{home}/.local/bin/agy"),
                    format!("{home}/.gemini/bin/agy"),
                    "/opt/homebrew/bin/agy".into(),
                    "/usr/local/bin/agy".into(),
                ]
            }
        }
        "praxis" => {
            if cfg!(windows) {
                vec![format!(r"{home}\.local\bin\praxis.exe")]
            } else {
                vec![
                    format!("{home}/.local/bin/praxis"),
                    "/opt/homebrew/bin/praxis".into(),
                    "/usr/local/bin/praxis".into(),
                ]
            }
        }
        "ollama_openai" => {
            if cfg!(windows) {
                let localapp = std::env::var("LOCALAPPDATA")
                    .unwrap_or_else(|_| format!(r"{home}\AppData\Local"));
                if Path::new(&format!(r"{localapp}\Programs\Ollama\ollama.exe")).exists()
                    || Path::new(&format!(r"{home}\.local\bin\ollama.exe")).exists()
                {
                    return Some("http://127.0.0.1:11434/v1".into());
                }
                return None;
            }
            if Path::new("/opt/homebrew/bin/ollama").exists()
                || Path::new("/usr/local/bin/ollama").exists()
                || Path::new("/usr/bin/ollama").exists()
            {
                return Some("http://127.0.0.1:11434/v1".into());
            }
            return None;
        }
        _ => Vec::new(),
    };
    candidates.into_iter().find(|p| {
        let path = Path::new(p);
        path.exists() && (kind != "antigravity_acp" || !is_electron_gui_symlink(path))
    })
}

// ============================================================================
// Persistent Warm JSON-RPC Clients (`Codex App-Server` & `Antigravity ACP`)
// ============================================================================

struct WarmCodexClient {
    stdin: AsyncMutex<ChildStdin>,
    _child: AsyncMutex<Child>,
    alive: AtomicBool,
    next_id: AtomicU64,
    pending: StdMutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    thread_listeners: StdMutex<HashMap<String, mpsc::UnboundedSender<Value>>>,
}

impl WarmCodexClient {
    async fn connect(bin_path: &str) -> Result<Arc<Self>, String> {
        let lower = bin_path.to_ascii_lowercase();
        let mut cmd = if cfg!(windows) && (lower.ends_with(".cmd") || lower.ends_with(".bat")) {
            let mut c = Command::new("cmd.exe");
            c.args(["/C", bin_path]);
            c
        } else {
            Command::new(bin_path)
        };
        let has_octendra_mcp_in_config = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .ok()
            .and_then(|h| {
                std::fs::read_to_string(std::path::Path::new(&h).join(".codex").join("config.toml"))
                    .ok()
            })
            .is_some_and(|s| s.contains("[mcp_servers.octendra]"));
        cmd.args(["app-server", "-c", "features.hooks=false"]);
        if has_octendra_mcp_in_config {
            cmd.args(["-c", "mcp_servers.octendra.enabled=false"]);
        }
        cmd.env("PATH", enriched_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("无法启动 codex app-server：{e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "无法获取 codex stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "无法获取 codex stdout".to_string())?;

        let client = Arc::new(Self {
            stdin: AsyncMutex::new(stdin),
            _child: AsyncMutex::new(child),
            alive: AtomicBool::new(true),
            next_id: AtomicU64::new(1),
            pending: StdMutex::new(HashMap::new()),
            thread_listeners: StdMutex::new(HashMap::new()),
        });

        let reader_ref = Arc::clone(&client);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.starts_with('{') {
                    continue;
                }
                if let Ok(msg) = serde_json::from_str::<Value>(trimmed) {
                    reader_ref.on_message(msg).await;
                }
            }
            reader_ref.alive.store(false, Ordering::SeqCst);
            if let Ok(mut guard) = reader_ref.pending.lock() {
                for (_, tx) in guard.drain() {
                    let _ = tx.send(Err("codex app-server 连接已关闭".into()));
                }
            }
            if let Ok(mut listeners) = reader_ref.thread_listeners.lock() {
                listeners.clear();
            }
        });

        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "octendra-agent-router",
                        "title": "Octendra Host Agent Router",
                        "version": "0.1.0"
                    },
                    "capabilities": { "experimentalApi": true }
                }),
                Duration::from_secs(15),
            )
            .await?;
        client
            .write_json(&json!({ "method": "initialized", "params": {} }))
            .await?;
        Ok(client)
    }

    async fn on_message(self: &Arc<Self>, msg: Value) {
        let id_opt = msg.get("id").and_then(Value::as_u64);
        let method_opt = msg.get("method").and_then(Value::as_str);
        match (id_opt, method_opt) {
            (Some(id), None) => {
                if let Some(tx) = self.pending.lock().ok().and_then(|mut g| g.remove(&id)) {
                    if let Some(err) = msg.get("error") {
                        let emsg = err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("app-server error");
                        let _ = tx.send(Err(emsg.to_string()));
                    } else {
                        let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                    }
                }
            }
            (Some(req_id), Some(method)) => {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let is_interactive_method = matches!(
                    method,
                    "item/tool/requestUserInput"
                        | "mcpServer/elicitation/request"
                        | "item/commandExecution/requestApproval"
                        | "item/fileChange/requestApproval"
                        | "item/permissions/requestApproval"
                );
                if is_interactive_method {
                    let forwarded = {
                        let tid_hint = params
                            .get("threadId")
                            .or_else(|| params.pointer("/thread/id"))
                            .and_then(Value::as_str);
                        if let Ok(map) = self.thread_listeners.lock() {
                            let target_tx = tid_hint
                                .and_then(|t| map.get(t))
                                .or_else(|| (map.len() == 1).then(|| map.values().next()).flatten());
                            if let Some(tx) = target_tx {
                                tx.send(json!({
                                    "id": req_id,
                                    "method": method,
                                    "params": params,
                                    "_server_request": true,
                                }))
                                .is_ok()
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    };
                    if forwarded {
                        return;
                    }
                }
                let reply = match method {
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                        json!({
                            "id": req_id,
                            "result": { "decision": "acceptForSession" }
                        })
                    }
                    "item/permissions/requestApproval" => json!({
                        "id": req_id,
                        "result": {
                            "permissions": params.get("permissions").cloned().unwrap_or_else(|| json!({})),
                            "scope": "session"
                        }
                    }),
                    _ => json!({ "id": req_id, "result": {} }),
                };
                let _ = self.write_json(&reply).await;
            }
            (None, Some(_)) => {
                if let Some(tid) = msg
                    .pointer("/params/threadId")
                    .or_else(|| msg.pointer("/params/thread/id"))
                    .and_then(Value::as_str)
                    && let Ok(map) = self.thread_listeners.lock()
                    && let Some(tx) = map.get(tid)
                {
                    let _ = tx.send(msg);
                }
            }
            _ => {}
        }
    }

    async fn write_json(&self, val: &Value) -> Result<(), String> {
        let mut s = serde_json::to_string(val).map_err(|e| e.to_string())?;
        s.push('\n');
        let mut guard = self.stdin.lock().await;
        if let Err(e) = guard.write_all(s.as_bytes()).await {
            self.alive.store(false, Ordering::SeqCst);
            return Err(format!("写入 codex app-server 失败：{e}"));
        }
        if let Err(e) = guard.flush().await {
            self.alive.store(false, Ordering::SeqCst);
            return Err(format!("刷新 codex app-server 失败：{e}"));
        }
        Ok(())
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout_dur: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut g) = self.pending.lock() {
            g.insert(id, tx);
        }
        if let Err(e) = self
            .write_json(&json!({ "id": id, "method": method, "params": params }))
            .await
        {
            if let Ok(mut g) = self.pending.lock() {
                g.remove(&id);
            }
            return Err(e);
        }
        match tokio::time::timeout(timeout_dur, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(format!("codex 请求 {method} 连接中断")),
            Err(_) => {
                if let Ok(mut g) = self.pending.lock() {
                    g.remove(&id);
                }
                Err(format!("codex 请求 {method} 超时"))
            }
        }
    }
}

// ============================================================================
// Shared Host Agent Router State & Service
// ============================================================================

/// Cloud upstream gateway connection for unified edge `octendra-code agent` instances.
#[derive(Debug, Clone)]
pub struct CloudUpstreamConfig {
    /// Octendra Cloud Gateway base URL (for example `https://octendra.hackerlife.fun:8443`).
    pub gateway_url: String,
    /// Authenticated device bearer token for Gateway APIs.
    pub bearer_token: String,
    /// Local device UUID owned by this edge agent.
    pub local_device_id: String,
}

/// Shared runtime state for the Octendra Host Agent Router.
#[derive(Clone)]
pub struct AgentRouterState {
    inner: Arc<AgentRouterInner>,
}

const PEER_SPOKE_ONLINE_TTL_MS: u64 = 90_000;
const PEER_KEY_HEADER: &str = "x-octendra-router-key";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PeerHubKeyFile {
    peer_key: String,
}

/// Connection configuration exported by the Hub and imported on remote Spoke computers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSpokeBundle {
    /// Hub relay endpoint URL (`https://.../control` or `http://127.0.0.1:18790`).
    pub hub_url: String,
    /// Shared 256-bit authentication key.
    pub peer_key: String,
}

fn default_peer_poll_wait_ms() -> u64 {
    20_000
}

/// Long-poll registration & heartbeat payload sent by a remote Spoke router to the Hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerPollRegistration {
    /// Spoke Octendra `device_id`.
    pub device_id: String,
    /// Spoke display name (`MacBook-M2-Max`, `cube`, `HO5`).
    pub device_name: String,
    /// Spoke OS platform (`macos`, `windows`, `linux`).
    pub platform: String,
    /// Default workspace root on the Spoke.
    pub default_cwd: String,
    /// Live local applications discovered or customized on the Spoke.
    #[serde(default)]
    pub local_apps: Vec<RegisteredApp>,
    /// Live incremental events emitted on the Spoke since its last poll.
    #[serde(default)]
    pub spoke_events: Vec<RouterLiveEvent>,
    /// Long-poll wait duration in milliseconds.
    #[serde(default = "default_peer_poll_wait_ms")]
    pub wait_ms: u64,
}

/// RPC envelope dispatched from Hub to a connected Spoke over the long-poll channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRpcRequest {
    /// Unique correlation ID.
    pub request_id: String,
    /// HTTP method (`GET`, `POST`, `DELETE`).
    pub method: String,
    /// Loopback router path (`/v1/sessions/turn`, `/v1/sessions/reset`, `/v1/permissions/respond`, etc.).
    pub path: String,
    /// Optional JSON request body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
    /// Execution timeout in milliseconds.
    pub timeout_ms: u64,
}

/// RPC reply posted by a Spoke back to the Hub after executing a `PeerRpcRequest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRpcReply {
    /// Spoke Octendra `device_id`.
    pub device_id: String,
    /// Correlation ID from `PeerRpcRequest`.
    pub request_id: String,
    /// Whether execution succeeded.
    pub ok: bool,
    /// JSON response body on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error message on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Public diagnostic summary of a live connected Spoke computer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerSpokeSnapshot {
    /// Spoke Octendra `device_id`.
    pub device_id: String,
    /// Spoke display name.
    pub device_name: String,
    /// Spoke OS platform.
    pub platform: String,
    /// Count of online apps reported by this Spoke.
    pub apps_online: usize,
    /// Last heartbeat Unix epoch milliseconds.
    pub last_seen_ms: u64,
}

struct ConnectedPeerSpoke {
    device_id: String,
    device_name: String,
    platform: String,
    local_apps: Vec<RegisteredApp>,
    last_seen_ms: u64,
    pending_queue: VecDeque<PeerRpcRequest>,
    wake_notify: Arc<Notify>,
}

fn constant_time_str_eq(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() || ab.is_empty() {
        return false;
    }
    ab.iter()
        .zip(bb.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Boxed future helper used by [`HostUiBridge`] to avoid adding new crate dependencies.
pub type UiBoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Callback bridge injected by `octendra-code agent` so the embedded edge router
/// (`127.0.0.1:18790`) can execute local host `ui_observe` / `ui_act` / `bind_window`
/// directly on this computer without round-tripping through Cloud Gateway.
pub trait HostUiBridge: Send + Sync + 'static {
    /// Returns local UI capability readiness and onscreen window inventory.
    fn local_ui_status(&self) -> UiBoxFuture<'_, Value>;
    /// Explicitly writes and adopts a window-scoped `desktop-launch.json` grant on the local host.
    fn bind_local_window(&self, req: Value) -> UiBoxFuture<'_, Result<Value, String>>;
    /// Executes local `ui_observe` or `ui_act`, or returns `None` if no local bridge is attached yet.
    fn execute_local_ui(
        &self,
        tool: String,
        args: Value,
    ) -> UiBoxFuture<'_, Option<Result<Value, String>>>;
}

struct AgentRouterInner {
    storage_dir: PathBuf,
    cloud_hub_mode: bool,
    cloud_upstream: Option<CloudUpstreamConfig>,
    host_ui: Option<Arc<dyn HostUiBridge>>,
    upstream_peer_apps: StdMutex<Option<(u64, Vec<RegisteredApp>)>>,
    custom_apps: StdMutex<HashMap<String, RegisteredApp>>,
    app_policies: StdMutex<HashMap<String, String>>,
    sessions: StdMutex<HashMap<String, RouterSessionRecord>>,
    turns: StdMutex<HashMap<String, TurnSnapshot>>,
    permissions: StdMutex<HashMap<String, PendingPermissionRecord>>,
    inputs: StdMutex<HashMap<String, PendingUserInputRecord>>,
    rpc_waiters: StdMutex<HashMap<String, oneshot::Sender<Value>>>,
    turn_pause_notifiers: StdMutex<HashMap<String, mpsc::UnboundedSender<TurnSnapshot>>>,
    turn_done_watchers:
        StdMutex<HashMap<String, watch::Receiver<Option<Result<TurnSnapshot, String>>>>>,
    event_seq: AtomicU64,
    event_log: StdMutex<VecDeque<RouterLiveEvent>>,
    event_notify: Arc<Notify>,
    spoke_cursor_pushed: AtomicU64,
    gemini_window_locks: AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>,
    peer_spokes: StdMutex<HashMap<String, ConnectedPeerSpoke>>,
    known_peer_apps: StdMutex<HashMap<String, Vec<RegisteredApp>>>,
    peer_in_flight: StdMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>,
    codex_hub: AsyncMutex<Option<Arc<WarmCodexClient>>>,
    accessibility_cache: StdMutex<Option<(u64, bool)>>,
    accessibility_refresh_in_flight: AtomicBool,
    last_accessibility_prompt_ms: AtomicU64,
}

impl Default for AgentRouterState {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRouterState {
    /// Creates or loads router state from the default user storage directory.
    pub fn new() -> Self {
        Self::with_storage_dir(state_dir())
    }

    /// Creates or loads router state from an explicit storage directory.
    pub fn with_storage_dir(storage_dir: PathBuf) -> Self {
        Self::build_state(storage_dir, false, None, None)
    }

    /// Creates a cloud Hub router state for `octendra-code gateway` (aggregates online edge agents only).
    pub fn new_cloud_hub(storage_dir: PathBuf) -> Self {
        Self::build_state(storage_dir, true, None, None)
    }

    /// Creates an edge router state wired upstream to Cloud `octendra-gateway`.
    pub fn with_cloud_upstream(storage_dir: PathBuf, upstream: CloudUpstreamConfig) -> Self {
        Self::build_state(storage_dir, false, Some(upstream), None)
    }

    /// Creates an edge router state wired upstream to Cloud `octendra-gateway` and a local host UI bridge.
    pub fn with_cloud_upstream_and_host_ui(
        storage_dir: PathBuf,
        upstream: CloudUpstreamConfig,
        host_ui: Arc<dyn HostUiBridge>,
    ) -> Self {
        Self::build_state(storage_dir, false, Some(upstream), Some(host_ui))
    }

    fn build_state(
        storage_dir: PathBuf,
        cloud_hub_mode: bool,
        cloud_upstream: Option<CloudUpstreamConfig>,
        host_ui: Option<Arc<dyn HostUiBridge>>,
    ) -> Self {
        let _ = std::fs::create_dir_all(&storage_dir);
        let custom_apps = load_json_map(&storage_dir.join("custom_apps.json"));
        let app_policies = load_json_map(&storage_dir.join("app_policies.json"));
        let sessions = load_json_map(&storage_dir.join("sessions.json"));
        let known_peer_apps = load_json_map(&storage_dir.join("known_peer_apps.json"));
        Self {
            inner: Arc::new(AgentRouterInner {
                storage_dir,
                cloud_hub_mode,
                cloud_upstream,
                host_ui,
                upstream_peer_apps: StdMutex::new(None),
                custom_apps: StdMutex::new(custom_apps),
                app_policies: StdMutex::new(app_policies),
                sessions: StdMutex::new(sessions),
                turns: StdMutex::new(HashMap::new()),
                permissions: StdMutex::new(HashMap::new()),
                inputs: StdMutex::new(HashMap::new()),
                rpc_waiters: StdMutex::new(HashMap::new()),
                turn_pause_notifiers: StdMutex::new(HashMap::new()),
                turn_done_watchers: StdMutex::new(HashMap::new()),
                event_seq: AtomicU64::new(1),
                event_log: StdMutex::new(VecDeque::with_capacity(512)),
                event_notify: Arc::new(Notify::new()),
                spoke_cursor_pushed: AtomicU64::new(0),
                gemini_window_locks: AsyncMutex::new(HashMap::new()),
                peer_spokes: StdMutex::new(HashMap::new()),
                known_peer_apps: StdMutex::new(known_peer_apps),
                peer_in_flight: StdMutex::new(HashMap::new()),
                codex_hub: AsyncMutex::new(None),
                accessibility_cache: StdMutex::new(None),
                accessibility_refresh_in_flight: AtomicBool::new(false),
                last_accessibility_prompt_ms: AtomicU64::new(0),
            }),
        }
    }

    /// Returns true when running inside Cloud `octendra-gateway` Hub mode.
    pub fn is_cloud_hub(&self) -> bool {
        self.inner.cloud_hub_mode
    }

    fn effective_local_identity(&self) -> (String, String, String) {
        let (device_name, local_device_id, platform, _) = local_computer_identity();
        let device_id = self
            .inner
            .cloud_upstream
            .as_ref()
            .map(|u| u.local_device_id.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or(local_device_id);
        (device_id, device_name, platform)
    }

    fn is_local_ui_target(&self, raw_device: Option<&str>) -> bool {
        if self.inner.host_ui.is_none() {
            return false;
        }
        let Some(trimmed) = raw_device.map(str::trim).filter(|s| !s.is_empty()) else {
            return true;
        };
        if trimmed.eq_ignore_ascii_case("local") {
            return true;
        }
        let (local_id, local_name, _) = self.effective_local_identity();
        trimmed.eq_ignore_ascii_case(&local_id) || trimmed.eq_ignore_ascii_case(&local_name)
    }

    /// Forwards a UI management RPC from Cloud `octendra-gateway` to an online connected spoke.
    ///
    /// # Errors
    ///
    /// Returns an error if the spoke daemon is offline or if the remote RPC fails/times out.
    pub async fn forward_spoke_ui_rpc(
        &self,
        device_id: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout_dur: Duration,
    ) -> Result<Value, String> {
        if !self.is_peer_spoke_online(device_id) {
            return Err(format!(
                "远端节点 {device_id} 的路由守护进程当前离线或未连接到 Cloud Hub"
            ));
        }
        self.call_spoke_peer_rpc(device_id, method, path, body, timeout_dur)
            .await
    }

    /// Initializes (or reuses) the Hub's 256-bit peer key and exports a spoke bundle JSON file.
    ///
    /// # Errors
    ///
    /// Returns an error if `hub_url` is empty or writing key/bundle files fails.
    pub fn init_or_export_hub_bundle(&self, hub_url: &str, output: &Path) -> Result<(), String> {
        let clean_url = hub_url.trim().to_string();
        if clean_url.is_empty() {
            return Err("hub_url 不能为空".into());
        }
        let hub_key_path = self.inner.storage_dir.join("peer_hub.json");
        let peer_key = if let Ok(raw) = std::fs::read_to_string(&hub_key_path)
            && let Ok(parsed) = serde_json::from_str::<PeerHubKeyFile>(&raw)
            && parsed.peer_key.trim().len() >= 32
        {
            parsed.peer_key.trim().to_string()
        } else {
            let generated = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            let bytes = serde_json::to_vec_pretty(&PeerHubKeyFile {
                peer_key: generated.clone(),
            })
            .map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&self.inner.storage_dir).map_err(|e| e.to_string())?;
            std::fs::write(&hub_key_path, bytes).map_err(|e| e.to_string())?;
            generated
        };

        if let Some(parent) = output.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let bundle = PeerSpokeBundle {
            hub_url: clean_url,
            peer_key,
        };
        let bundle_bytes = serde_json::to_vec_pretty(&bundle).map_err(|e| e.to_string())?;
        std::fs::write(output, bundle_bytes).map_err(|e| e.to_string())
    }

    /// Imports a spoke connection bundle into `<storage_dir>/peer_spoke.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if the input bundle cannot be read, parsed, or persisted.
    pub fn import_spoke_bundle(&self, input: &Path, delete_input: bool) -> Result<String, String> {
        let raw = std::fs::read_to_string(input).map_err(|e| format!("读取 spoke bundle 失败：{e}"))?;
        let mut bundle: PeerSpokeBundle =
            serde_json::from_str(&raw).map_err(|e| format!("解析 spoke bundle 失败：{e}"))?;
        bundle.hub_url = bundle.hub_url.trim().to_string();
        bundle.peer_key = bundle.peer_key.trim().to_string();
        if bundle.hub_url.is_empty() || bundle.peer_key.len() < 32 {
            return Err("spoke bundle 缺少有效的 hub_url 或 peer_key".into());
        }
        std::fs::create_dir_all(&self.inner.storage_dir).map_err(|e| e.to_string())?;
        let dest = self.inner.storage_dir.join("peer_spoke.json");
        let tmp = dest.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(&bundle).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        if delete_input {
            let _ = std::fs::remove_file(input);
        }
        Ok(bundle.hub_url)
    }

    fn verify_hub_peer_key(&self, provided: Option<&str>) -> bool {
        let Some(candidate) = provided.map(str::trim).filter(|s| s.len() >= 32) else {
            return false;
        };
        if let Ok(env_key) = std::env::var("OCTENDRA_ROUTER_PEER_KEY")
            && env_key.trim().len() >= 32
            && constant_time_str_eq(candidate, env_key.trim())
        {
            return true;
        }
        let hub_key_path = self.inner.storage_dir.join("peer_hub.json");
        let Ok(raw) = std::fs::read_to_string(hub_key_path) else {
            return false;
        };
        let Ok(parsed) = serde_json::from_str::<PeerHubKeyFile>(&raw) else {
            return false;
        };
        constant_time_str_eq(candidate, parsed.peer_key.trim())
    }

    fn load_peer_spoke_bundle(&self) -> Option<PeerSpokeBundle> {
        if let (Ok(url), Ok(key)) = (
            std::env::var("OCTENDRA_ROUTER_HUB_URL"),
            std::env::var("OCTENDRA_ROUTER_PEER_KEY"),
        ) && !url.trim().is_empty()
            && key.trim().len() >= 32
        {
            return Some(PeerSpokeBundle {
                hub_url: url.trim().to_string(),
                peer_key: key.trim().to_string(),
            });
        }
        let raw = std::fs::read_to_string(self.inner.storage_dir.join("peer_spoke.json")).ok()?;
        let parsed: PeerSpokeBundle = serde_json::from_str(&raw).ok()?;
        if parsed.hub_url.trim().is_empty() || parsed.peer_key.trim().len() < 32 {
            return None;
        }
        Some(parsed)
    }

    /// Lists only locally hosted applications on this computer (empty in Cloud Hub mode).
    pub fn list_local_only_apps(&self) -> Vec<RegisteredApp> {
        if self.inner.cloud_hub_mode {
            return Vec::new();
        }
        let (_, local_device_id, _, _) = local_computer_identity();
        let mut list = discovery::discover_local_apps_cached();
        if let Ok(guard) = self.inner.custom_apps.lock() {
            for custom in guard.values() {
                if custom.device_id == local_device_id {
                    let mut normalized = custom.clone();
                    normalized.is_local = true;
                    if let Some(pos) = list.iter().position(|x| x.app_id == normalized.app_id) {
                        list[pos] = normalized;
                    } else {
                        list.push(normalized);
                    }
                }
            }
        }
        list
    }

    fn persist_known_peer_apps(&self, peer_key: &str, apps: &[RegisteredApp]) {
        if peer_key.is_empty() || apps.is_empty() {
            return;
        }
        let snapshot = self.inner.known_peer_apps.lock().ok().and_then(|mut guard| {
            let changed = guard.get(peer_key).is_none_or(|prev| {
                prev.len() != apps.len()
                    || prev.iter().zip(apps.iter()).any(|(a, b)| {
                        a.app_id != b.app_id || a.protocol != b.protocol || a.online != b.online
                    })
            });
            if changed {
                guard.insert(peer_key.to_string(), apps.to_vec());
                Some(guard.clone())
            } else {
                None
            }
        });
        if let Some(snap) = snapshot {
            let _ = save_json_map(&self.inner.storage_dir.join("known_peer_apps.json"), &snap);
        }
    }

    /// Returns snapshots of all currently online Spoke computers connected to this Hub.
    pub fn connected_peer_snapshots(&self) -> Vec<PeerSpokeSnapshot> {
        let now = now_ms();
        let Ok(guard) = self.inner.peer_spokes.lock() else {
            return Vec::new();
        };
        let mut out: Vec<PeerSpokeSnapshot> = guard
            .values()
            .filter(|p| now.saturating_sub(p.last_seen_ms) <= PEER_SPOKE_ONLINE_TTL_MS)
            .map(|p| PeerSpokeSnapshot {
                device_id: p.device_id.clone(),
                device_name: p.device_name.clone(),
                platform: p.platform.clone(),
                apps_online: p.local_apps.iter().filter(|a| a.online).count(),
                last_seen_ms: p.last_seen_ms,
            })
            .collect();
        out.sort_by(|a, b| a.device_name.cmp(&b.device_name));
        out
    }

    fn is_peer_spoke_online(&self, device_id: &str) -> bool {
        let now = now_ms();
        let key = device_id.trim().to_ascii_lowercase();
        let Ok(guard) = self.inner.peer_spokes.lock() else {
            return false;
        };
        guard
            .get(&key)
            .is_some_and(|p| now.saturating_sub(p.last_seen_ms) <= PEER_SPOKE_ONLINE_TTL_MS)
    }

    async fn poll_spoke_next(&self, reg: PeerPollRegistration) -> Option<PeerRpcRequest> {
        let key = reg.device_id.trim().to_ascii_lowercase();
        if key.is_empty() {
            return None;
        }
        let wait_ms = reg.wait_ms.clamp(500, 25_000);
        for ev in reg.spoke_events {
            self.push_live_event_raw(
                ev.kind,
                ev.turn_id,
                ev.caller_session_id,
                ev.app_id,
                ev.device_name,
                ev.status,
                ev.delta,
                ev.summary,
            );
        }
        self.persist_known_peer_apps(&key, &reg.local_apps);
        let wake_notify = {
            let mut guard = self.inner.peer_spokes.lock().ok()?;
            let now = now_ms();
            let entry = guard.entry(key.clone()).or_insert_with(|| ConnectedPeerSpoke {
                device_id: reg.device_id.trim().to_string(),
                device_name: reg.device_name.trim().to_string(),
                platform: reg.platform.trim().to_string(),
                local_apps: Vec::new(),
                last_seen_ms: now,
                pending_queue: VecDeque::new(),
                wake_notify: Arc::new(Notify::new()),
            });
            entry.device_id = reg.device_id.trim().to_string();
            entry.device_name = reg.device_name.trim().to_string();
            entry.platform = reg.platform.trim().to_string();
            entry.local_apps = reg.local_apps;
            entry.last_seen_ms = now;
            if let Some(req) = entry.pending_queue.pop_front() {
                return Some(req);
            }
            entry.wake_notify.clone()
        };

        let _ = tokio::time::timeout(Duration::from_millis(wait_ms), wake_notify.notified()).await;
        let mut guard = self.inner.peer_spokes.lock().ok()?;
        let peer = guard.get_mut(&key)?;
        peer.last_seen_ms = now_ms();
        peer.pending_queue.pop_front()
    }

    fn resolve_spoke_reply(&self, reply: PeerRpcReply) {
        let key = reply.device_id.trim().to_ascii_lowercase();
        if let Ok(mut guard) = self.inner.peer_spokes.lock()
            && let Some(peer) = guard.get_mut(&key)
        {
            peer.last_seen_ms = now_ms();
        }
        let tx = self
            .inner
            .peer_in_flight
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&reply.request_id));
        if let Some(sender) = tx {
            if reply.ok {
                let _ = sender.send(Ok(reply.result.unwrap_or(Value::Null)));
            } else {
                let msg = reply
                    .error
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "远端路由节点执行失败".into());
                let _ = sender.send(Err(msg));
            }
        }
    }

    async fn call_spoke_peer_rpc(
        &self,
        device_id: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
        timeout_dur: Duration,
    ) -> Result<Value, String> {
        let key = device_id.trim().to_ascii_lowercase();
        let req_id = format!("prpc_{}", Uuid::new_v4().simple());
        let timeout_ms = u64::try_from(timeout_dur.as_millis()).unwrap_or(240_000);
        let (tx, rx) = oneshot::channel();
        let wake_notify = {
            let mut guard = self
                .inner
                .peer_spokes
                .lock()
                .map_err(|_| "peer_spokes 锁异常".to_string())?;
            let peer = guard
                .get_mut(&key)
                .ok_or_else(|| format!("远端节点 {device_id} 的路由守护进程尚未连接"))?;
            if now_ms().saturating_sub(peer.last_seen_ms) > PEER_SPOKE_ONLINE_TTL_MS {
                return Err(format!("远端节点 {} 心跳已过期离线", peer.device_name));
            }
            if let Ok(mut flight) = self.inner.peer_in_flight.lock() {
                flight.insert(req_id.clone(), tx);
            }
            peer.pending_queue.push_back(PeerRpcRequest {
                request_id: req_id.clone(),
                method: method.to_string(),
                path: path.to_string(),
                body,
                timeout_ms,
            });
            peer.wake_notify.clone()
        };
        wake_notify.notify_one();

        match tokio::time::timeout(timeout_dur + Duration::from_secs(5), rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => {
                if let Ok(mut flight) = self.inner.peer_in_flight.lock() {
                    flight.remove(&req_id);
                }
                Err(format!("远端节点 {device_id} 路由请求通道中断"))
            }
            Err(_) => {
                if let Ok(mut flight) = self.inner.peer_in_flight.lock() {
                    flight.remove(&req_id);
                }
                Err(format!(
                    "远端节点 {device_id} 执行 {method} {path} 超时 ({}s)",
                    timeout_dur.as_secs()
                ))
            }
        }
    }

    /// Authenticated spoke poll called by `octendra-code gateway` after verifying `device_token`.
    pub async fn poll_authenticated_spoke(
        &self,
        authenticated_device_id: &str,
        registered_device_name: &str,
        mut reg: PeerPollRegistration,
    ) -> Option<PeerRpcRequest> {
        reg.device_id = authenticated_device_id.trim().to_string();
        if !registered_device_name.trim().is_empty() {
            reg.device_name = registered_device_name.trim().to_string();
        }
        self.poll_spoke_next(reg).await
    }

    /// Authenticated spoke reply called by `octendra-code gateway` after verifying `device_token`.
    pub fn resolve_authenticated_spoke_reply(
        &self,
        authenticated_device_id: &str,
        mut reply: PeerRpcReply,
    ) {
        reply.device_id = authenticated_device_id.trim().to_string();
        self.resolve_spoke_reply(reply);
    }

    async fn sync_upstream_apps(&self, force_discover: bool) {
        let Some(ref upstream) = self.inner.cloud_upstream else {
            return;
        };
        let timeout_dur = Duration::from_secs(if force_discover { 10 } else { 4 });
        let Ok(client) = Self::build_cloud_http_client(&upstream.gateway_url, timeout_dur) else {
            return;
        };
        let base = upstream.gateway_url.trim_end_matches('/');
        let resp = if force_discover {
            client
                .get(format!("{base}/v1/models?refresh=true"))
                .bearer_auth(&upstream.bearer_token)
                .send()
                .await
        } else {
            client
                .get(format!("{base}/v1/apps"))
                .bearer_auth(&upstream.bearer_token)
                .send()
                .await
        };
        let Ok(resp) = resp else {
            return;
        };
        if !resp.status().is_success() {
            return;
        }
        let Ok(val) = resp.json::<Value>().await else {
            return;
        };
        let Some(arr) = val.get("apps").and_then(Value::as_array) else {
            return;
        };
        let local_did = upstream.local_device_id.trim();
        let mut parsed = Vec::new();
        for item in arr {
            if let Ok(mut app) = serde_json::from_value::<RegisteredApp>(item.clone()) {
                if !local_did.is_empty() && app.device_id.eq_ignore_ascii_case(local_did) {
                    continue;
                }
                app.is_local = false;
                parsed.push(app);
            }
        }
        if let Ok(mut guard) = self.inner.upstream_peer_apps.lock() {
            *guard = Some((now_ms(), parsed));
        }
    }

    async fn maybe_sync_upstream_apps(&self, max_age_ms: u64) {
        if self.inner.cloud_upstream.is_none() {
            return;
        }
        let fresh = self
            .inner
            .upstream_peer_apps
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|(ts, _)| *ts))
            .is_some_and(|ts| now_ms().saturating_sub(ts) <= max_age_ms);
        if !fresh {
            self.sync_upstream_apps(false).await;
        }
    }

    /// Lists all connected applications (dynamic local clients + cloud-synced peers + live connected spokes + cached offline peers + user custom registrations).
    pub fn list_apps(&self) -> Vec<RegisteredApp> {
        let local_device_id = if self.inner.cloud_hub_mode {
            String::new()
        } else {
            self.inner
                .cloud_upstream
                .as_ref()
                .map(|u| u.local_device_id.clone())
                .unwrap_or_else(|| local_computer_identity().1)
        };
        let local_key = local_device_id.to_ascii_lowercase();
        let mut list = self.list_local_only_apps();
        let now = now_ms();
        let mut live_peer_keys = std::collections::HashSet::new();
        let mut upstream_seen_keys = std::collections::HashSet::new();

        if !self.inner.cloud_hub_mode
            && let Ok(guard) = self.inner.upstream_peer_apps.lock()
            && let Some((_, ref peer_apps)) = *guard
        {
            for remote_app in peer_apps {
                if !local_device_id.is_empty()
                    && remote_app.device_id.eq_ignore_ascii_case(&local_device_id)
                {
                    continue;
                }
                upstream_seen_keys.insert(remote_app.device_id.to_ascii_lowercase());
                if remote_app.online {
                    live_peer_keys.insert(remote_app.device_id.to_ascii_lowercase());
                }
                let mut synced = remote_app.clone();
                synced.is_local = false;
                if let Some(pos) = list.iter().position(|x| x.app_id == synced.app_id) {
                    list[pos] = synced;
                } else {
                    list.push(synced);
                }
            }
        }

        if let Ok(spokes) = self.inner.peer_spokes.lock() {
            for (peer_key, spoke) in spokes.iter() {
                if (!local_device_id.is_empty()
                    && spoke.device_id.eq_ignore_ascii_case(&local_device_id))
                    || now.saturating_sub(spoke.last_seen_ms) > PEER_SPOKE_ONLINE_TTL_MS
                {
                    continue;
                }
                live_peer_keys.insert(peer_key.clone());
                for remote_app in &spoke.local_apps {
                    let mut synced = remote_app.clone();
                    synced.is_local = false;
                    if let Some(pos) = list.iter().position(|x| x.app_id == synced.app_id) {
                        list[pos] = synced;
                    } else {
                        list.push(synced);
                    }
                }
            }
        }

        if let Ok(known_peers) = self.inner.known_peer_apps.lock() {
            for (peer_key, cached_apps) in known_peers.iter() {
                if (!local_key.is_empty() && peer_key == &local_key)
                    || live_peer_keys.contains(peer_key)
                    || upstream_seen_keys.contains(peer_key)
                {
                    continue;
                }
                for cached_app in cached_apps {
                    let mut offline_app = cached_app.clone();
                    offline_app.is_local = false;
                    offline_app.online = false;
                    if !list.iter().any(|x| x.app_id == offline_app.app_id) {
                        list.push(offline_app);
                    }
                }
            }
        }

        if let Ok(guard) = self.inner.custom_apps.lock() {
            for custom in guard.values() {
                let mut normalized = custom.clone();
                normalized.is_local = !self.inner.cloud_hub_mode
                    && !local_device_id.is_empty()
                    && normalized.device_id == local_device_id;
                if !normalized.is_local
                    && !live_peer_keys.contains(&normalized.device_id.to_ascii_lowercase())
                {
                    normalized.online = false;
                }
                if let Some(pos) = list.iter().position(|x| x.app_id == normalized.app_id) {
                    list[pos] = normalized;
                } else {
                    list.push(normalized);
                }
            }
        }
        list
    }

    /// Resolves an application by `app_id`, `display_name`, `name`, or alias (preferring online instances).
    pub fn resolve_app(&self, raw: &str) -> Option<RegisteredApp> {
        let cleaned = raw.trim().trim_start_matches('@');
        if cleaned.is_empty() {
            return None;
        }
        let all = self.list_apps();
        all.iter()
            .find(|a| a.app_id.eq_ignore_ascii_case(cleaned))
            .or_else(|| {
                all.iter().find(|a| {
                    a.online
                        && (a.display_name.eq_ignore_ascii_case(cleaned)
                            || a.aliases.iter().any(|al| al.eq_ignore_ascii_case(cleaned)))
                })
            })
            .or_else(|| {
                all.iter().find(|a| {
                    a.display_name.eq_ignore_ascii_case(cleaned)
                        || a.aliases.iter().any(|al| al.eq_ignore_ascii_case(cleaned))
                })
            })
            .or_else(|| all.iter().find(|a| a.online && a.name.eq_ignore_ascii_case(cleaned)))
            .or_else(|| all.iter().find(|a| a.name.eq_ignore_ascii_case(cleaned)))
            .cloned()
    }

    /// Registers or updates a custom application.
    ///
    /// # Errors
    ///
    /// Returns an error message when required fields are empty or persistence fails.
    pub fn upsert_custom_app(&self, req: UpsertCustomAppRequest) -> Result<RegisteredApp, String> {
        let clean_name = req.name.trim().to_string();
        if clean_name.is_empty() {
            return Err("应用名称不能为空".into());
        }
        let protocol = req.protocol.trim().to_ascii_lowercase();
        if !matches!(
            protocol.as_str(),
            "gemini_desktop"
                | "gemini_cli"
                | "codex_app_server"
                | "antigravity_desktop"
                | "antigravity_acp"
                | "claude_code"
                | "praxis"
                | "custom_command"
                | "openai_compatible"
        ) {
            return Err(format!("不支持的协议类型：{protocol}"));
        }
        let (device_name, device_id, platform, default_root, is_local) =
            resolve_octendra_computer(req.device.as_deref());
        let slug: String = clean_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        let app_id = req
            .app_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || {
                    let suffix = if slug.is_empty() {
                        Uuid::new_v4().simple().to_string()
                    } else {
                        format!("{slug}__{}", device_name.to_ascii_lowercase())
                    };
                    format!("custom__{suffix}")
                },
                str::to_string,
            );
        let display_name = req
            .display_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || format!("{clean_name} · {device_name}"),
                str::to_string,
            );
        let slash_commands = if req.slash_commands.is_empty() {
            match protocol.as_str() {
                "gemini_desktop" => default_gemini_slash_commands(),
                "codex_app_server" => default_codex_slash_commands(),
                "antigravity_desktop" | "antigravity_acp" => default_antigravity_slash_commands(),
                _ => vec![AppSlashCommand {
                    command: "/new".into(),
                    label: "重置会话".into(),
                    description: "开启全新会话上下文".into(),
                }],
            }
        } else {
            req.slash_commands
        };
        let internal_tools = if req.internal_tools.is_empty() {
            match protocol.as_str() {
                "gemini_desktop" => default_gemini_internal_tools(),
                _ => default_cli_app_internal_tools(),
            }
        } else {
            req.internal_tools
        };

        let record = RegisteredApp {
            app_id: app_id.clone(),
            name: clean_name.clone(),
            display_name,
            protocol,
            category: "connected_app".into(),
            device_id,
            device_name: device_name.clone(),
            platform,
            is_local,
            online: true,
            is_custom: true,
            default_cwd: req
                .default_cwd
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map_or(default_root, str::to_string),
            executable: req
                .executable
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            summary: req
                .summary
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map_or_else(
                    || format!("{device_name} 上的自定义应用 {clean_name}"),
                    str::to_string,
                ),
            capabilities: if req.capabilities.is_empty() {
                vec!["自定义应用".into(), "多轮会话".into()]
            } else {
                req.capabilities
            },
            aliases: vec![clean_name],
            slash_commands,
            internal_tools,
            available_models: Vec::new(),
            default_model: None,
            supported_efforts: Vec::new(),
        };

        let snapshot = {
            let mut guard = self
                .inner
                .custom_apps
                .lock()
                .map_err(|_| "custom_apps lock poisoned".to_string())?;
            guard.insert(app_id, record.clone());
            guard.clone()
        };
        save_json_map(&self.inner.storage_dir.join("custom_apps.json"), &snapshot)?;
        Ok(record)
    }

    /// Removes a custom application registration.
    ///
    /// # Errors
    ///
    /// Returns an error if persistence fails.
    pub fn delete_custom_app(&self, app_id: &str) -> Result<bool, String> {
        let (removed, snapshot) = {
            let mut guard = self
                .inner
                .custom_apps
                .lock()
                .map_err(|_| "custom_apps lock poisoned".to_string())?;
            let existed = guard.remove(app_id).is_some();
            (existed, guard.clone())
        };
        if removed {
            save_json_map(&self.inner.storage_dir.join("custom_apps.json"), &snapshot)?;
        }
        Ok(removed)
    }

    /// Lists persisted native session bindings.
    pub fn list_sessions(&self) -> Vec<RouterSessionRecord> {
        self.inner
            .sessions
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.values().cloned().collect())
    }

    /// Resets stored native thread continuity for `caller_session_id` (optionally scoped to `app_id`).
    pub fn reset_session(&self, caller_session_id: &str, app_id: Option<&str>) -> usize {
        let resolved_app_id = app_id.and_then(|raw| self.resolve_app(raw).map(|a| a.app_id));
        let (removed, snapshot) = match self.inner.sessions.lock() {
            Ok(mut guard) => {
                let before = guard.len();
                guard.retain(|_, rec| {
                    if rec.caller_session_id != caller_session_id {
                        return true;
                    }
                    if let Some(ref target_app) = resolved_app_id {
                        return &rec.app_id != target_app;
                    }
                    false
                });
                let count = before.saturating_sub(guard.len());
                (count, Some(guard.clone()))
            }
            Err(_) => (0, None),
        };
        if let Some(snap) = snapshot {
            let _ = save_json_map(&self.inner.storage_dir.join("sessions.json"), &snap);
        }
        removed
    }

    fn session_key(caller_session_id: &str, app_id: &str) -> String {
        format!("{caller_session_id}::{app_id}")
    }

    fn get_session(&self, caller_session_id: &str, app_id: &str) -> Option<RouterSessionRecord> {
        let key = Self::session_key(caller_session_id, app_id);
        self.inner
            .sessions
            .lock()
            .ok()
            .and_then(|g| g.get(&key).cloned())
    }

    pub(crate) fn upsert_session(&self, record: RouterSessionRecord) {
        let key = Self::session_key(&record.caller_session_id, &record.app_id);
        let snapshot = self.inner.sessions.lock().ok().map(|mut g| {
            g.insert(key, record);
            g.clone()
        });
        if let Some(snap) = snapshot {
            let _ = save_json_map(&self.inner.storage_dir.join("sessions.json"), &snap);
        }
    }

    fn push_live_event_raw(
        &self,
        kind: String,
        turn_id: Option<String>,
        caller_session_id: Option<String>,
        app_id: Option<String>,
        device_name: Option<String>,
        status: Option<String>,
        delta: Option<String>,
        summary: Option<String>,
    ) {
        let seq = self.inner.event_seq.fetch_add(1, Ordering::SeqCst);
        let ev = RouterLiveEvent {
            seq,
            timestamp_ms: now_ms(),
            kind,
            turn_id,
            caller_session_id,
            app_id,
            device_name,
            status,
            delta,
            summary,
        };
        if let Ok(mut log) = self.inner.event_log.lock() {
            if log.len() >= 512 {
                log.pop_front();
            }
            log.push_back(ev);
        }
        self.inner.event_notify.notify_waiters();
    }

    fn drain_spoke_events_for_hub(&self) -> Vec<RouterLiveEvent> {
        let prev = self.inner.spoke_cursor_pushed.load(Ordering::SeqCst);
        let Ok(log) = self.inner.event_log.lock() else {
            return Vec::new();
        };
        let events: Vec<RouterLiveEvent> = log.iter().filter(|e| e.seq > prev).cloned().collect();
        if let Some(last) = events.last() {
            self.inner.spoke_cursor_pushed.store(last.seq, Ordering::SeqCst);
        }
        events
    }

    /// Queries buffered live events with `seq > since_seq`, optionally long-polling up to `wait_ms`.
    pub async fn poll_events(
        &self,
        since_seq: u64,
        turn_filter: Option<&str>,
        wait_ms: u64,
    ) -> (u64, Vec<RouterLiveEvent>) {
        let collect_now = || -> (u64, Vec<RouterLiveEvent>) {
            let latest = self.inner.event_seq.load(Ordering::SeqCst).saturating_sub(1);
            let Ok(log) = self.inner.event_log.lock() else {
                return (latest, Vec::new());
            };
            let items: Vec<RouterLiveEvent> = log
                .iter()
                .filter(|e| {
                    e.seq > since_seq
                        && turn_filter.is_none_or(|tf| e.turn_id.as_deref() == Some(tf))
                })
                .cloned()
                .collect();
            (latest, items)
        };

        let (latest, initial) = collect_now();
        if !initial.is_empty() || wait_ms == 0 {
            return (latest, initial);
        }
        let clamped = wait_ms.min(20_000);
        let notified = self.inner.event_notify.notified();
        let _ = tokio::time::timeout(Duration::from_millis(clamped), notified).await;
        collect_now()
    }

    /// Gets the configured permission mode for `app_id` (`"auto_safe"`, `"interactive"`, or `"full_auto"`).
    pub fn app_permission_mode(&self, app_id: &str) -> String {
        self.inner
            .app_policies
            .lock()
            .ok()
            .and_then(|g| g.get(app_id).cloned())
            .unwrap_or_else(|| "auto_safe".into())
    }

    /// Updates the permission mode for `app_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if `mode` is invalid or persistence fails.
    pub fn set_app_permission_mode(&self, app_id: &str, mode: &str) -> Result<String, String> {
        let norm = mode.trim().to_ascii_lowercase();
        if !matches!(norm.as_str(), "auto_safe" | "interactive" | "full_auto") {
            return Err(format!("不支持的授权策略：{mode}（可选：auto_safe / interactive / full_auto）"));
        }
        let snapshot = {
            let mut guard = self
                .inner
                .app_policies
                .lock()
                .map_err(|_| "app_policies 锁异常".to_string())?;
            guard.insert(app_id.to_string(), norm.clone());
            guard.clone()
        };
        save_json_map(&self.inner.storage_dir.join("app_policies.json"), &snapshot)?;
        Ok(norm)
    }

    /// Lists all connected applications enriched with live runtime status (`idle` / `busy` / `waiting_permission` / `waiting_input` / `offline`).
    pub fn list_enriched_apps(&self) -> Vec<EnrichedRegisteredApp> {
        let apps = self.list_apps();
        let running_counts: HashMap<String, usize> = self
            .inner
            .turns
            .lock()
            .map(|g| {
                let mut m = HashMap::new();
                for t in g.values() {
                    if t.status == "running" {
                        *m.entry(t.app_id.clone()).or_insert(0) += 1;
                    }
                }
                m
            })
            .unwrap_or_default();
        let perm_apps: HashMap<String, bool> = self
            .inner
            .permissions
            .lock()
            .map(|g| g.values().map(|p| (p.app_id.clone(), true)).collect())
            .unwrap_or_default();
        let input_apps: HashMap<String, bool> = self
            .inner
            .inputs
            .lock()
            .map(|g| g.values().map(|i| (i.app_id.clone(), true)).collect())
            .unwrap_or_default();

        apps.into_iter()
            .map(|app| {
                let active_turns = running_counts.get(&app.app_id).copied().unwrap_or(0);
                let runtime_status = if !app.online {
                    "offline"
                } else if perm_apps.contains_key(&app.app_id) {
                    "waiting_permission"
                } else if input_apps.contains_key(&app.app_id) {
                    "waiting_input"
                } else if app.is_local
                    && app.protocol == "gemini_desktop"
                    && self.cached_local_macos_accessibility() == Some(false)
                {
                    "needs_accessibility"
                } else if active_turns > 0 {
                    "busy"
                } else {
                    "idle"
                }
                .to_string();
                let permission_mode = self.app_permission_mode(&app.app_id);
                EnrichedRegisteredApp {
                    app,
                    runtime_status,
                    active_turns,
                    permission_mode,
                }
            })
            .collect()
    }

    /// Lists pending structured user-input / elicitation requests.
    pub fn list_inputs(&self) -> Vec<PendingUserInputRecord> {
        self.inner
            .inputs
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.values().cloned().collect())
    }

    pub(crate) fn update_turn(&self, mut snap: TurnSnapshot) {
        if snap.status == "completed"
            && snap.suggested_options.is_empty()
            && let Some(ref txt) = snap.text
        {
            snap.suggested_options = extract_structured_choice_options(txt);
        }
        let kind = match snap.status.as_str() {
            "running" => "turn_running",
            "waiting_permission" | "waiting_input" => "turn_paused",
            "completed" => "turn_completed",
            _ => "turn_failed",
        }
        .to_string();
        self.push_live_event_raw(
            kind,
            Some(snap.turn_id.clone()),
            Some(snap.caller_session_id.clone()),
            Some(snap.app_id.clone()),
            Some(snap.device_name.clone()),
            Some(snap.status.clone()),
            None,
            Some(snap.reason.clone()),
        );
        let is_paused = matches!(snap.status.as_str(), "waiting_permission" | "waiting_input");
        if is_paused
            && let Ok(map) = self.inner.turn_pause_notifiers.lock()
            && let Some(tx) = map.get(&snap.turn_id)
        {
            let _ = tx.send(snap.clone());
        }
        if let Ok(mut g) = self.inner.turns.lock() {
            g.insert(snap.turn_id.clone(), snap);
        }
    }

    pub(crate) fn emit_turn_delta(&self, app: &RegisteredApp, caller_session_id: &str, turn_id: &str, delta: &str, full_so_far: &str) {
        if delta.is_empty() {
            return;
        }
        if let Ok(mut g) = self.inner.turns.lock()
            && let Some(t) = g.get_mut(turn_id)
        {
            t.text = Some(full_so_far.to_string());
            t.updated_at_ms = now_ms();
        }
        self.push_live_event_raw(
            "turn_delta".into(),
            Some(turn_id.to_string()),
            Some(caller_session_id.to_string()),
            Some(app.app_id.clone()),
            Some(app.device_name.clone()),
            Some("running".into()),
            Some(delta.to_string()),
            None,
        );
    }

    pub(crate) fn emit_turn_tool(&self, app: &RegisteredApp, caller_session_id: &str, turn_id: &str, summary: &str) {
        if summary.trim().is_empty() {
            return;
        }
        if let Ok(mut g) = self.inner.turns.lock()
            && let Some(t) = g.get_mut(turn_id)
        {
            t.events.push(summary.to_string());
            t.updated_at_ms = now_ms();
        }
        self.push_live_event_raw(
            "turn_tool".into(),
            Some(turn_id.to_string()),
            Some(caller_session_id.to_string()),
            Some(app.app_id.clone()),
            Some(app.device_name.clone()),
            Some("running".into()),
            None,
            Some(summary.to_string()),
        );
    }

    /// Reads a live or completed turn snapshot.
    pub fn get_turn(&self, turn_id: &str) -> Option<TurnSnapshot> {
        self.inner
            .turns
            .lock()
            .ok()
            .and_then(|g| g.get(turn_id).cloned())
    }

    fn has_running_turns(&self) -> bool {
        self.inner
            .turns
            .lock()
            .is_ok_and(|g| g.values().any(|t| t.status == "running"))
    }

    /// Lists pending permission requests.
    pub fn list_permissions(&self) -> Vec<PendingPermissionRecord> {
        self.inner
            .permissions
            .lock()
            .map_or_else(|_| Vec::new(), |g| g.values().cloned().collect())
    }

    async fn device_gemini_lock(&self, device_id: &str) -> Arc<AsyncMutex<()>> {
        let mut guard = self.inner.gemini_window_locks.lock().await;
        Arc::clone(
            guard
                .entry(device_id.to_string())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    async fn get_codex_client(&self, bin_path: &str) -> Result<Arc<WarmCodexClient>, String> {
        let mut guard = self.inner.codex_hub.lock().await;
        if let Some(existing) = guard.as_ref()
            && existing.alive.load(Ordering::SeqCst)
        {
            return Ok(Arc::clone(existing));
        }
        let fresh = WarmCodexClient::connect(bin_path).await?;
        *guard = Some(Arc::clone(&fresh));
        Ok(fresh)
    }

    /// Executes a session-bound turn on the target application.
    pub async fn execute_turn(&self, req: TurnRequest) -> Result<TurnSnapshot, String> {
        let app = self
            .resolve_app(&req.app_id)
            .ok_or_else(|| format!("未找到已注册的应用：{}", req.app_id))?;
        let trimmed_prompt = req.prompt.trim();
        if trimmed_prompt.is_empty() {
            return Err("发送内容不能为空".into());
        }
        let turn_id = req
            .turn_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_else(Uuid::new_v4)
            .to_string();

        if trimmed_prompt == "/new" || trimmed_prompt.eq_ignore_ascii_case("/reset") {
            self.reset_session(&req.caller_session_id, Some(&app.app_id));
            if !app.is_local {
                let _ = self
                    .call_remote_peer_router(
                        &app.device_id,
                        "POST",
                        "/v1/sessions/reset",
                        Some(&json!({
                            "caller_session_id": req.caller_session_id,
                            "app_id": app.app_id,
                        })),
                        Duration::from_secs(20),
                    )
                    .await;
            }
            let snap = TurnSnapshot {
                turn_id,
                caller_session_id: req.caller_session_id,
                app_id: app.app_id,
                app_display_name: app.display_name.clone(),
                device_name: app.device_name,
                status: "completed".into(),
                reason: "session_reset".into(),
                text: Some(format!(
                    "已为「{}」开启全新原生对话上下文（当前会话绑定保持不变）。",
                    app.display_name
                )),
                native_thread_id: None,
                turn_number: 0,
                pending_permission: None,
                pending_input: None,
                suggested_options: Vec::new(),
                events: vec!["session_reset".into()],
                updated_at_ms: now_ms(),
            };
            self.update_turn(snap.clone());
            return Ok(snap);
        }

        let effective_prompt = if let Some(after_new) = trimmed_prompt.strip_prefix("/new ") {
            self.reset_session(&req.caller_session_id, Some(&app.app_id));
            after_new.trim().to_string()
        } else {
            trimmed_prompt.to_string()
        };

        let prior_session = self.get_session(&req.caller_session_id, &app.app_id);
        let turn_number = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
        let initial = TurnSnapshot {
            turn_id: turn_id.clone(),
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            app_display_name: app.display_name.clone(),
            device_name: app.device_name.clone(),
            status: "running".into(),
            reason: "dispatched".into(),
            text: None,
            native_thread_id: prior_session
                .as_ref()
                .and_then(|s| s.native_thread_id.clone()),
            turn_number,
            pending_permission: None,
            pending_input: None,
            suggested_options: Vec::new(),
            events: vec![format!("dispatch:{}:turn_{turn_number}", app.app_id)],
            updated_at_ms: now_ms(),
        };
        self.update_turn(initial.clone());

        let wait_sync = req.wait.unwrap_or(true);
        let (pause_tx, mut pause_rx) = mpsc::unbounded_channel::<TurnSnapshot>();
        let (done_tx, mut done_rx) = watch::channel::<Option<Result<TurnSnapshot, String>>>(None);
        if let Ok(mut pmap) = self.inner.turn_pause_notifiers.lock() {
            pmap.insert(turn_id.clone(), pause_tx);
        }
        if let Ok(mut dmap) = self.inner.turn_done_watchers.lock() {
            dmap.insert(turn_id.clone(), done_rx.clone());
        }

        let router = self.clone();
        let tid_bg = turn_id.clone();
        tokio::spawn(async move {
            let res = router
                .run_turn_inner(app, req, effective_prompt, tid_bg.clone(), prior_session)
                .await;
            if let Err(ref err_msg) = res
                && let Some(mut cur) = router.get_turn(&tid_bg)
                && (cur.status != "failed" || cur.reason != *err_msg)
            {
                cur.status = "failed".into();
                cur.reason = err_msg.clone();
                cur.updated_at_ms = now_ms();
                router.update_turn(cur);
            }
            let _ = done_tx.send(Some(res));
            if let Ok(mut pmap) = router.inner.turn_pause_notifiers.lock() {
                pmap.remove(&tid_bg);
            }
        });

        if !wait_sync {
            return Ok(initial);
        }

        loop {
            tokio::select! {
                maybe_paused = pause_rx.recv() => {
                    if let Some(paused_snap) = maybe_paused {
                        return Ok(paused_snap);
                    }
                }
                changed = done_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    if let Some(final_res) = done_rx.borrow().clone() {
                        return final_res;
                    }
                }
            }
        }
        self.get_turn(&turn_id)
            .ok_or_else(|| "回合执行通道异常关闭".into())
    }

    fn normalize_cwd_for_compare(cwd: &str) -> String {
        let trimmed = cwd.trim().trim_end_matches(['/', '\\']);
        std::fs::canonicalize(trimmed)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| trimmed.to_string())
    }

    fn matches_identifier_boundary(haystack_lower: &str, needle_lower: &str) -> bool {
        let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'-';
        let bytes = haystack_lower.as_bytes();
        let mut start = 0usize;
        while let Some(pos) = haystack_lower[start..].find(needle_lower) {
            let abs = start + pos;
            let before_ok = abs == 0 || !is_ident(bytes[abs - 1]);
            let end = abs + needle_lower.len();
            let after_ok = end >= bytes.len() || !is_ident(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
            start = abs + needle_lower.len();
        }
        false
    }

    fn infer_project_cwd_from_prompt(workspace_root: &str, prompt: &str) -> Option<String> {
        let root_path = Path::new(workspace_root.trim());
        if !root_path.is_dir() {
            return None;
        }
        let lower_prompt = prompt.to_ascii_lowercase();
        if lower_prompt.trim().is_empty() {
            return None;
        }
        let entries = std::fs::read_dir(root_path).ok()?;
        let mut matches: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with('.')
                || name.len() < 3
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "target" | "node_modules" | "dist" | "build" | "cache" | "tmp" | "temp" | "src" | "docs"
                )
            {
                continue;
            }
            let lower_name = name.to_ascii_lowercase();
            if Self::matches_identifier_boundary(&lower_prompt, &lower_name) {
                matches.push(p.to_string_lossy().into_owned());
                if matches.len() > 1 {
                    return None;
                }
            }
        }
        if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        }
    }

    fn resolve_effective_turn_cwd(
        app: &RegisteredApp,
        req: &TurnRequest,
        prompt: &str,
        prior_session: Option<&RouterSessionRecord>,
    ) -> String {
        let strip_dev_prefix = |raw: &str| -> String {
            let t = raw.trim();
            t.split_once(':')
                .filter(|(prefix, _)| prefix.len() > 1)
                .map_or(t, |(_, path_part)| path_part)
                .to_string()
        };
        if let Some(explicit) = req
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return strip_dev_prefix(explicit);
        }
        if let Some(prev_cwd) = prior_session
            .and_then(|s| s.cwd.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let clean_prev = strip_dev_prefix(prev_cwd);
            if Path::new(&clean_prev).exists() {
                return clean_prev;
            }
        }
        let clean_default = strip_dev_prefix(&app.default_cwd);
        if matches!(
            app.protocol.as_str(),
            "codex_app_server" | "antigravity_desktop" | "antigravity_acp" | "claude_code" | "gemini_cli"
        ) && let Some(inferred) = Self::infer_project_cwd_from_prompt(&clean_default, prompt)
        {
            return inferred;
        }
        clean_default
    }

    async fn run_turn_inner(
        &self,
        app: RegisteredApp,
        req: TurnRequest,
        prompt: String,
        turn_id: String,
        prior_session: Option<RouterSessionRecord>,
    ) -> Result<TurnSnapshot, String> {
        let effective_cwd =
            Self::resolve_effective_turn_cwd(&app, &req, &prompt, prior_session.as_ref());

        if !app.is_local {
            return self
                .run_remote_octendra_turn(&app, &req, &prompt, &effective_cwd, &turn_id)
                .await;
        }

        match app.protocol.as_str() {
            "gemini_desktop" => {
                self.run_local_gemini_desktop_turn(
                    &app,
                    &req,
                    &prompt,
                    &turn_id,
                    prior_session,
                )
                .await
            }
            "codex_app_server" => {
                self.run_local_codex_turn(
                    &app,
                    &req,
                    &prompt,
                    &effective_cwd,
                    &turn_id,
                    prior_session,
                )
                .await
            }
            "antigravity_desktop" | "antigravity_acp" => {
                self.run_local_antigravity_desktop_turn(
                    &app,
                    &req,
                    &prompt,
                    &effective_cwd,
                    &turn_id,
                    prior_session,
                )
                .await
            }
            "claude_code" => {
                self.run_local_claude_turn(
                    &app,
                    &req,
                    &prompt,
                    &effective_cwd,
                    &turn_id,
                    prior_session,
                )
                .await
            }
            "gemini_cli" => {
                self.run_local_gemini_cli_turn(
                    &app,
                    &req,
                    &prompt,
                    &effective_cwd,
                    &turn_id,
                    prior_session,
                )
                .await
            }
            "praxis" => {
                self.run_local_praxis_turn(&app, &req, &prompt, &effective_cwd, &turn_id)
                    .await
            }
            "custom_command" => {
                self.run_local_custom_command_turn(
                    &app,
                    &req,
                    &prompt,
                    &effective_cwd,
                    &turn_id,
                    prior_session,
                )
                .await
            }
            "openai_compatible" => {
                self.run_local_openai_compatible_turn(
                    &app,
                    &req,
                    &prompt,
                    &turn_id,
                    prior_session,
                )
                .await
            }
            other => Err(format!("未知的协议适配器：{other}")),
        }
    }

    fn normalize_gemini_reason_code(reason: &str) -> &str {
        let trimmed = reason.trim();
        trimmed.strip_prefix("not_submitted:").unwrap_or(trimmed).trim()
    }

    fn is_gemini_model_selection_failure(reason: &str) -> bool {
        let bare = Self::normalize_gemini_reason_code(reason).to_ascii_lowercase();
        [
            "model_option_not_found",
            "model_selection_not_verified",
            "model_selector_",
            "effort_option_not_found",
            "effort_selection_not_verified",
            "effort_selector_",
            "post_effort_context_not_verified",
            "argon_limit",
            "arg_limit",
        ]
        .iter()
        .any(|needle| bare.contains(needle))
    }

    fn is_gemini_pre_submit_race(reason: &str) -> bool {
        let bare = Self::normalize_gemini_reason_code(reason).to_ascii_lowercase();
        [
            "composer_not_found",
            "composer_set_unverified",
            "focus_verification_failed",
            "window_not_ready",
            "ax_transient",
            "new_chat_not_verified",
        ]
        .iter()
        .any(|needle| bare.contains(needle))
    }

    fn is_gemini_window_busy_reason(reason: &str) -> bool {
        let bare = Self::normalize_gemini_reason_code(reason).to_ascii_lowercase();
        bare.starts_with("busy")
            || bare.starts_with("active_binding_exists")
            || bare.starts_with("helper_busy")
    }

    fn should_clear_prior_op_on_not_submitted(reason: &str) -> bool {
        let bare = Self::normalize_gemini_reason_code(reason).to_ascii_lowercase();
        (bare.contains("prior_") || bare.contains("not_found"))
            && !Self::is_gemini_model_selection_failure(&bare)
            && !bare.contains("composer_not_found")
    }

    fn extract_stream_text_delta<'a>(previous: &str, partial: &'a str) -> Option<&'a str> {
        if partial.is_empty() || partial == previous {
            return None;
        }
        if let Some(delta) = partial.strip_prefix(previous) {
            if delta.is_empty() {
                None
            } else {
                Some(delta)
            }
        } else if partial.chars().count() > previous.chars().count() {
            Some(partial)
        } else {
            None
        }
    }

    fn local_gemini_app_installed() -> bool {
        let home = home_dir();
        Path::new("/Applications/Gemini.app").exists()
            || home.join("Applications/Gemini.app").exists()
    }

    fn record_local_accessibility_trusted(&self, trusted: bool) {
        if let Ok(mut guard) = self.inner.accessibility_cache.lock() {
            *guard = Some((now_ms(), trusted));
        }
    }

    fn cached_local_macos_accessibility(&self) -> Option<bool> {
        if !cfg!(target_os = "macos") {
            return Some(true);
        }
        let now = now_ms();
        let (cached_val, expired) = match self.inner.accessibility_cache.lock() {
            Ok(guard) => match *guard {
                Some((ts, val)) => {
                    let ttl_ms = if val { 15_000 } else { 3_000 };
                    (Some(val), now.saturating_sub(ts) > ttl_ms)
                }
                None => (None, true),
            },
            Err(_) => (None, false),
        };
        if expired
            && tokio::runtime::Handle::try_current().is_ok()
            && !self
                .inner
                .accessibility_refresh_in_flight
                .swap(true, Ordering::AcqRel)
        {
            let router = self.clone();
            tokio::spawn(async move {
                let _ = router.refresh_local_macos_accessibility().await;
                router
                    .inner
                    .accessibility_refresh_in_flight
                    .store(false, Ordering::Release);
            });
        }
        cached_val
    }

    /// Probes macOS Accessibility (`AXIsProcessTrusted`) via the embedded `gemini_desktop`
    /// native runner so the check shares the process's own TCC responsible-process identity.
    pub async fn refresh_local_macos_accessibility(&self) -> bool {
        if !cfg!(target_os = "macos") || self.inner.cloud_hub_mode {
            return true;
        }
        let Ok(Ok(parsed)) =
            tokio::task::spawn_blocking(|| crate::gemini_desktop::dispatch("accessibility", &json!({})))
                .await
        else {
            return false;
        };
        if let Some(trusted) = parsed.get("accessibility_trusted").and_then(Value::as_bool) {
            self.record_local_accessibility_trusted(trusted);
            return trusted;
        }
        false
    }

    /// Triggers the official macOS Accessibility trust prompt and opens Privacy & Security settings
    /// when `Octendra Agent` is not yet trusted (debounced unless `force` is set).
    pub async fn prompt_local_macos_accessibility(&self, force: bool) -> Value {
        let exe = std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        if !cfg!(target_os = "macos") {
            return json!({
                "status": "ok",
                "accessibility_trusted": true,
                "prompted": false,
                "router_executable": exe,
            });
        }
        if self.refresh_local_macos_accessibility().await {
            return json!({
                "status": "ok",
                "accessibility_trusted": true,
                "prompted": false,
                "router_executable": exe,
            });
        }
        let now = now_ms();
        let prev = self.inner.last_accessibility_prompt_ms.load(Ordering::Relaxed);
        let should_prompt = force || prev == 0 || now.saturating_sub(prev) > 60_000;
        let mut prompted = false;
        if should_prompt {
            self.inner
                .last_accessibility_prompt_ms
                .store(now, Ordering::Relaxed);
            let _ = tokio::task::spawn_blocking(|| {
                crate::gemini_desktop::dispatch("prompt_accessibility", &json!({}))
            })
            .await;
            prompted = true;
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                Command::new("/usr/bin/open")
                    .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            )
            .await;
        }
        let trusted = self.refresh_local_macos_accessibility().await;
        json!({
            "status": if trusted { "ok" } else { "needs_accessibility" },
            "accessibility_trusted": trusted,
            "prompted": prompted,
            "router_executable": exe,
        })
    }

    async fn call_gemini_desktop(&self, action: &str, payload: &Value) -> Result<Value, String> {
        if !Self::local_gemini_app_installed() {
            return Err("未检测到本机 /Applications/Gemini.app，请先安装并登录 Gemini 桌面端".into());
        }
        let action_owned = action.to_string();
        let payload_owned = payload.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::gemini_desktop::dispatch(&action_owned, &payload_owned)
        })
        .await
        .map_err(|e| format!("Gemini.app 驱动任务异常：{e}"))?
        .map_err(|e| format!("Gemini.app 驱动执行失败：{e}"));

        if action == "capabilities"
            && let Ok(ref val) = result
            && let Some(trusted) = val
                .pointer("/readiness/accessibility_trusted")
                .or_else(|| val.pointer("/native/accessibility_trusted"))
                .and_then(Value::as_bool)
        {
            self.record_local_accessibility_trusted(trusted);
        }
        result
    }

    async fn run_local_gemini_desktop_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        prompt: &str,
        turn_id: &str,
        prior_session: Option<RouterSessionRecord>,
    ) -> Result<TurnSnapshot, String> {
        if cfg!(target_os = "macos")
            && self.cached_local_macos_accessibility() != Some(true)
            && !self.refresh_local_macos_accessibility().await
        {
            let _ = self.prompt_local_macos_accessibility(false).await;
            return Err(format!(
                "not_submitted:app_accessibility_unavailable: {} 所在主机上的 Octendra Agent (octendra-code) 尚未获得 macOS「辅助功能 (Accessibility)」权限；已自动打开「系统设置 → 隐私与安全性 → 辅助功能」，请开启后重试",
                app.display_name
            ));
        }

        let dev_lock = self.device_gemini_lock(&app.device_id).await;
        let mut window_guard = Some(dev_lock.lock().await);

        let native_budget_ms = req
            .timeout_ms
            .filter(|&ms| ms > 0)
            .unwrap_or(86_400_000);
        let wait_idle_budget = Duration::from_millis(req.wait_for_idle_ms.unwrap_or(20_000));
        let idle_deadline = tokio::time::Instant::now() + wait_idle_budget;

        let is_slash_mode = ["/deep-research", "/deep_research", "/research", "/canvas", "/image", "/video"]
            .iter()
            .any(|p| prompt.trim_start().starts_with(p));
        let mut prior_op = if is_slash_mode {
            None
        } else {
            prior_session.as_ref().and_then(|s| s.last_operation_id.clone())
        };

        let effective_model = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !matches!(*s, "default" | "auto" | "gemini-desktop-task"))
            .map(str::to_string);
        let effective_effort = req
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !matches!(*s, "default" | "auto"))
            .map(str::to_string);
        let mut transient_pre_submit_retries: u32 = 0;

        loop {
            let attempt_op_id = Uuid::new_v4().to_string();
            let (action_name, mut start_payload) = if let Some(ref prev_op) = prior_op {
                (
                    "continue",
                    json!({
                        "operation_id": prev_op,
                        "prompt": prompt,
                        "request_id": attempt_op_id,
                        "timeout_ms": native_budget_ms,
                    }),
                )
            } else {
                (
                    "start",
                    json!({
                        "prompt": prompt,
                        "request_id": attempt_op_id,
                        "capability": "text",
                        "timeout_ms": native_budget_ms,
                    }),
                )
            };
            if let Some(ref m) = effective_model {
                start_payload["model"] = json!(m);
            }
            if let Some(ref e) = effective_effort {
                start_payload["effort"] = json!(e);
            }

            let start_val = self.call_gemini_desktop(action_name, &start_payload).await?;
            let st = start_val
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("running");
            let reason = start_val
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or(st);

            if st == "blocked"
                && prior_op.is_some()
                && Self::should_clear_prior_op_on_not_submitted(reason)
            {
                prior_op = None;
                continue;
            }

            let raw_ns_reason: String = if st == "not_submitted" {
                reason.to_string()
            } else if matches!(st, "blocked" | "conflict") {
                return Err(format!("Gemini.app 启动前安全拦截（{st}）：{reason}"));
            } else {
                let returned_id = start_val
                    .get("operation_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&attempt_op_id)
                    .to_string();

                // Quick 450ms check in case worker immediately hit pre-submit race/model mismatch or prior mismatch
                tokio::time::sleep(Duration::from_millis(450)).await;
                let first_obs = self
                    .call_gemini_desktop(
                        "observe",
                        &json!({ "operation_id": returned_id, "resume": false }),
                    )
                    .await?;
                let obs_st = first_obs
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("running");
                let obs_reason = first_obs
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or(obs_st);
                if obs_st == "unknown" && prior_op.is_some() && obs_reason.contains("prior_") {
                    prior_op = None;
                    continue;
                }
                if obs_st == "not_submitted" {
                    obs_reason.to_string()
                } else {
                    let poll_res = self
                        .poll_gemini_operation_until_pause_or_done(
                            app,
                            &req.caller_session_id,
                            turn_id,
                            &returned_id,
                            effective_model.clone(),
                            effective_effort.clone(),
                            req.cwd.clone(),
                            prior_session.as_ref().map_or(0, |s| s.turn_count),
                        )
                        .await;
                    let poll_snap = poll_res?;
                    if poll_snap.status == "busy"
                        && poll_snap.reason.starts_with("not_submitted:")
                    {
                        poll_snap.reason.clone()
                    } else {
                        drop(window_guard.take());
                        if poll_snap.status == "busy" {
                            self.update_turn(poll_snap.clone());
                        }
                        return Ok(poll_snap);
                    }
                }
            };

            let inner_reason = Self::normalize_gemini_reason_code(&raw_ns_reason).to_string();
            if prior_op.is_some()
                && Self::should_clear_prior_op_on_not_submitted(&inner_reason)
            {
                prior_op = None;
                continue;
            }
            if (effective_model.is_some() || effective_effort.is_some())
                && Self::is_gemini_model_selection_failure(&inner_reason)
            {
                drop(window_guard.take());
                return Err(format!(
                    "{}：指定的模型（{}）或思考度不可用（{inner_reason}），请检查并在模型列表中选择当前可用模型",
                    app.display_name,
                    effective_model.as_deref().unwrap_or("默认")
                ));
            }
            if transient_pre_submit_retries < 2
                && Self::is_gemini_pre_submit_race(&inner_reason)
            {
                transient_pre_submit_retries += 1;
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            if Self::is_gemini_window_busy_reason(&inner_reason)
                && tokio::time::Instant::now() < idle_deadline
            {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                continue;
            }

            drop(window_guard.take());
            let inner_lower = inner_reason.to_ascii_lowercase();
            if inner_lower.contains("accessibility") {
                self.record_local_accessibility_trusted(false);
                let _ = self.prompt_local_macos_accessibility(false).await;
                return Err(format!(
                    "not_submitted:app_accessibility_unavailable: {} 所在主机上的 Octendra Agent (octendra-code) 尚未获得 macOS「辅助功能 (Accessibility)」权限；已自动打开「系统设置 → 隐私与安全性 → 辅助功能」，请开启后重试",
                    app.display_name
                ));
            }
            if Self::is_gemini_window_busy_reason(&inner_reason) {
                let snap = TurnSnapshot {
                    turn_id: turn_id.to_string(),
                    caller_session_id: req.caller_session_id.clone(),
                    app_id: app.app_id.clone(),
                    app_display_name: app.display_name.clone(),
                    device_name: app.device_name.clone(),
                    status: "busy".into(),
                    reason: format!("not_submitted:{inner_reason}"),
                    text: None,
                    native_thread_id: prior_op,
                    turn_number: prior_session.as_ref().map_or(1, |s| s.turn_count + 1),
                    pending_permission: None,
                    pending_input: None,
                    suggested_options: Vec::new(),
                    events: vec![format!("busy:{inner_reason}")],
                    updated_at_ms: now_ms(),
                };
                self.update_turn(snap.clone());
                return Ok(snap);
            }
            return Err(format!("Gemini.app 提交前未就绪（{inner_reason}）"));
        }
    }

    fn format_gemini_obs_recovery(obs: &Value) -> String {
        obs.get("recovery")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let snippet: String = s.chars().take(300).collect();
                format!("（诊断：{snippet}）")
            })
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    async fn poll_gemini_operation_until_pause_or_done(
        &self,
        app: &RegisteredApp,
        caller_session_id: &str,
        turn_id: &str,
        op_id: &str,
        model: Option<String>,
        effort: Option<String>,
        cwd: Option<String>,
        prior_turns: u64,
    ) -> Result<TurnSnapshot, String> {
        let mut resume_attempts: u32 = 0;
        let mut last_streamed_text = String::new();

        loop {
            let obs = self
                .call_gemini_desktop(
                    "observe",
                    &json!({ "operation_id": op_id, "resume": false }),
                )
                .await?;
            let status = obs
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("running");
            let reason = obs
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or(status);
            if status == "running"
                && let Some(partial) = obs.get("text").and_then(Value::as_str)
                && let Some(delta) = Self::extract_stream_text_delta(&last_streamed_text, partial)
            {
                self.emit_turn_delta(app, caller_session_id, turn_id, delta, partial);
                last_streamed_text = partial.to_string();
            }

            if status == "completed" {
                let collected = self
                    .call_gemini_desktop(
                        "collect",
                        &json!({ "operation_id": op_id }),
                    )
                    .await
                .unwrap_or_else(|_| obs.clone());
                let reply = collected
                    .get("text")
                    .or_else(|| obs.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let session_uuid = obs
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let new_turn_count = prior_turns + 1;
                self.upsert_session(RouterSessionRecord {
                    caller_session_id: caller_session_id.to_string(),
                    app_id: app.app_id.clone(),
                    native_thread_id: session_uuid.clone(),
                    last_operation_id: Some(op_id.to_string()),
                    turn_count: new_turn_count,
                    model,
                    effort,
                    cwd,
                    history: Vec::new(),
                    updated_at_ms: now_ms(),
                });
                let snap = TurnSnapshot {
                    turn_id: turn_id.to_string(),
                    caller_session_id: caller_session_id.to_string(),
                    app_id: app.app_id.clone(),
                    app_display_name: app.display_name.clone(),
                    device_name: app.device_name.clone(),
                    status: "completed".into(),
                    reason: reason.to_string(),
                    text: Some(if reply.is_empty() {
                        format!("{} 已完成执行。", app.display_name)
                    } else {
                        reply
                    }),
                    native_thread_id: Some(op_id.to_string()),
                    turn_number: new_turn_count,
                    pending_permission: None,
                    pending_input: None,
                    suggested_options: Vec::new(),
                    events: vec!["completed".into()],
                    updated_at_ms: now_ms(),
                };
                self.update_turn(snap.clone());
                return Ok(snap);
            }

            if status == "waiting_human" {
                let pending = obs.get("pending_action").cloned().unwrap_or(Value::Null);
                let action_id = pending
                    .get("action_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let action_version = pending
                    .get("action_version")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let perm_kind = pending
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("desktop_confirmation")
                    .to_string();
                let summary = pending
                    .get("summary")
                    .and_then(Value::as_str)
                    .map_or_else(
                        || {
                            if reason == "research_plan_confirmation" {
                                "Gemini Deep Research 已生成研究方案，确认后将开始深度研究。".into()
                            } else {
                                format!("{} 请求执行确认（{reason}）", app.display_name)
                            }
                        },
                        str::to_string,
                    );
                let perm_id = Uuid::new_v4().to_string();
                let perm = PendingPermissionRecord {
                    permission_id: perm_id.clone(),
                    turn_id: turn_id.to_string(),
                    caller_session_id: caller_session_id.to_string(),
                    app_id: app.app_id.clone(),
                    app_display_name: app.display_name.clone(),
                    kind: perm_kind,
                    title: format!("{} 请求权限确认", app.display_name),
                    summary,
                    options: vec![
                        PermissionOptionItem {
                            option_id: "allow_once".into(),
                            label: "允许并继续".into(),
                            kind: "approve_once".into(),
                        },
                        PermissionOptionItem {
                            option_id: "deny".into(),
                            label: "拒绝执行".into(),
                            kind: "deny".into(),
                        },
                    ],
                    native_operation_id: Some(op_id.to_string()),
                    action_id,
                    action_version,
                    created_at_ms: now_ms(),
                };
                if let Ok(mut g) = self.inner.permissions.lock() {
                    g.insert(perm_id, perm.clone());
                }
                let snap = TurnSnapshot {
                    turn_id: turn_id.to_string(),
                    caller_session_id: caller_session_id.to_string(),
                    app_id: app.app_id.clone(),
                    app_display_name: app.display_name.clone(),
                    device_name: app.device_name.clone(),
                    status: "waiting_permission".into(),
                    reason: reason.to_string(),
                    text: None,
                    native_thread_id: Some(op_id.to_string()),
                    turn_number: prior_turns + 1,
                    pending_permission: Some(perm),
                    pending_input: None,
                    suggested_options: Vec::new(),
                    events: vec![format!("waiting_permission:{reason}")],
                    updated_at_ms: now_ms(),
                };
                self.update_turn(snap.clone());
                return Ok(snap);
            }

            if status == "not_submitted" {
                return Ok(TurnSnapshot {
                    turn_id: turn_id.to_string(),
                    caller_session_id: caller_session_id.to_string(),
                    app_id: app.app_id.clone(),
                    app_display_name: app.display_name.clone(),
                    device_name: app.device_name.clone(),
                    status: "busy".into(),
                    reason: format!("not_submitted:{reason}"),
                    text: None,
                    native_thread_id: None,
                    turn_number: prior_turns + 1,
                    pending_permission: None,
                    pending_input: None,
                    suggested_options: Vec::new(),
                    events: vec![format!("busy:{reason}")],
                    updated_at_ms: now_ms(),
                });
            }

            if matches!(status, "failed" | "cancelled" | "blocked" | "conflict") {
                let detail = obs
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(reason);
                let recovery_suffix = Self::format_gemini_obs_recovery(&obs);
                return Err(format!(
                    "Gemini.app 执行结束（{status}）：{detail}{recovery_suffix}"
                ));
            }

            if matches!(status, "unknown" | "uncertain") {
                if matches!(
                    reason,
                    "worker_exited_without_receipt" | "timeout_generation_may_continue"
                ) && resume_attempts < 24
                {
                    resume_attempts += 1;
                    let _ = self
                        .call_gemini_desktop(
                            "observe",
                            &json!({ "operation_id": op_id, "resume": true }),
                        )
                        .await;
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    continue;
                }
                let recovery_suffix = Self::format_gemini_obs_recovery(&obs);
                return Err(format!(
                    "Gemini.app 状态结束（{status}）：{reason}{recovery_suffix}"
                ));
            }

            tokio::time::sleep(Duration::from_millis(450)).await;
        }
    }

    /// Responds to an intercepted native permission request and optionally resumes waiting.
    pub async fn respond_permission(
        &self,
        req: RespondPermissionRequest,
    ) -> Result<TurnSnapshot, String> {
        let perm = self
            .inner
            .permissions
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&req.permission_id))
            .ok_or_else(|| format!("未找到待处理的权限请求：{}", req.permission_id))?;

        let app = self
            .resolve_app(&perm.app_id)
            .ok_or_else(|| format!("未找到应用：{}", perm.app_id))?;

        let decision_norm = req
            .decision
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| {
                if req.approved {
                    "allow_once".into()
                } else {
                    "deny".into()
                }
            });
        let effective_approved = !matches!(decision_norm.as_str(), "deny" | "decline" | "cancel" | "reject");

        if !app.is_local {
            let remote_perm_id = perm
                .action_id
                .clone()
                .unwrap_or_else(|| perm.permission_id.clone());
            let remote_val = self
                .call_remote_peer_router(
                    &app.device_id,
                    "POST",
                    "/v1/permissions/respond",
                    Some(&json!({
                        "permission_id": remote_perm_id,
                        "approved": effective_approved,
                        "decision": decision_norm,
                        "note": req.note,
                        "wait_for_turn": false,
                    })),
                    Duration::from_secs(20),
                )
                .await?;
            let mut snap: TurnSnapshot =
                serde_json::from_value(remote_val).map_err(|e| format!("解析远端权限响应失败：{e}"))?;
            snap.turn_id = perm.turn_id.clone();
            self.update_turn(snap.clone());
            if !req.wait_for_turn.unwrap_or(true) || snap.status != "running" {
                return Ok(snap);
            }
            return self
                .sync_remote_turn_until_pause_or_done(
                    &app,
                    &perm.caller_session_id,
                    None,
                    None,
                    None,
                    &perm.turn_id,
                )
                .await;
        }

        if let Some(waiter_tx) = self
            .inner
            .rpc_waiters
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&perm.permission_id))
        {
            if decision_norm == "allow_session" {
                let _ = self.set_app_permission_mode(&app.app_id, "full_auto");
            }
            let _ = waiter_tx.send(json!({
                "decision": decision_norm,
                "approved": effective_approved,
                "note": req.note,
            }));
            if !req.wait_for_turn.unwrap_or(true) {
                return self
                    .get_turn(&perm.turn_id)
                    .ok_or_else(|| "未找到关联的会话轮次记录".into());
            }
            return self.wait_for_resumed_turn(&perm.turn_id).await;
        }

        if app.protocol == "gemini_desktop"
            && let (Some(op_id), Some(ver)) =
                (perm.native_operation_id.as_deref(), perm.action_version.as_deref())
        {
            let chosen_action = if perm.kind == "research_start" {
                if !effective_approved {
                    let _ = self
                        .call_gemini_desktop("cancel", &json!({ "operation_id": op_id }))
                        .await;
                    let snap = TurnSnapshot {
                        turn_id: perm.turn_id,
                        caller_session_id: perm.caller_session_id,
                        app_id: app.app_id,
                        app_display_name: app.display_name,
                        device_name: app.device_name,
                        status: "failed".into(),
                        reason: "permission_denied".into(),
                        text: Some("已取消 Deep Research 研究计划执行。".into()),
                        native_thread_id: Some(op_id.to_string()),
                        turn_number: 1,
                        pending_permission: None,
                        pending_input: None,
                        suggested_options: Vec::new(),
                        events: vec!["permission_denied".into()],
                        updated_at_ms: now_ms(),
                    };
                    self.update_turn(snap.clone());
                    return Ok(snap);
                }
                "deep_research_start"
            } else if effective_approved {
                "tool_consent_allow"
            } else {
                "tool_consent_deny"
            };

            let _ = self
                .call_gemini_desktop(
                    "confirm",
                    &json!({
                        "operation_id": op_id,
                        "action_id": chosen_action,
                        "action_version": ver,
                    }),
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(600)).await;
            let _ = self
                .call_gemini_desktop(
                    "observe",
                    &json!({ "operation_id": op_id, "resume": true }),
                )
                .await;

            if req.wait_for_turn.unwrap_or(true) {
                let prior_rec = self.get_session(&perm.caller_session_id, &app.app_id);
                let prior_turns = prior_rec.as_ref().map_or(0, |s| s.turn_count);
                let resumed = self
                    .poll_gemini_operation_until_pause_or_done(
                        &app,
                        &perm.caller_session_id,
                        &perm.turn_id,
                        op_id,
                        prior_rec.as_ref().and_then(|s| s.model.clone()),
                        prior_rec.as_ref().and_then(|s| s.effort.clone()),
                        prior_rec.as_ref().and_then(|s| s.cwd.clone()),
                        prior_turns,
                    )
                    .await?;
                if resumed.status == "busy" {
                    self.update_turn(resumed.clone());
                }
                return Ok(resumed);
            }
        }

        self.get_turn(&perm.turn_id)
            .ok_or_else(|| "未找到关联的会话轮次记录".into())
    }

    async fn wait_for_resumed_turn(&self, turn_id: &str) -> Result<TurnSnapshot, String> {
        let maybe_rx = self
            .inner
            .turn_done_watchers
            .lock()
            .ok()
            .and_then(|m| m.get(turn_id).cloned());
        let Some(mut done_rx) = maybe_rx else {
            return self
                .get_turn(turn_id)
                .ok_or_else(|| "未找到关联的会话轮次记录".into());
        };
        if let Some(done_res) = done_rx.borrow().clone() {
            return done_res;
        }
        let (pause_tx, mut pause_rx) = mpsc::unbounded_channel::<TurnSnapshot>();
        if let Ok(mut pmap) = self.inner.turn_pause_notifiers.lock() {
            pmap.insert(turn_id.to_string(), pause_tx);
        }
        loop {
            tokio::select! {
                maybe_paused = pause_rx.recv() => {
                    if let Some(paused_snap) = maybe_paused {
                        return Ok(paused_snap);
                    }
                }
                changed = done_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    if let Some(final_res) = done_rx.borrow().clone() {
                        return final_res;
                    }
                }
            }
        }
        self.get_turn(turn_id)
            .ok_or_else(|| "回合执行通道已结束".into())
    }

    /// Responds to a structured AI user-input / option selection request and resumes the paused turn.
    pub async fn respond_user_input(
        &self,
        req: RespondUserInputRequest,
    ) -> Result<TurnSnapshot, String> {
        let input_rec = self
            .inner
            .inputs
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&req.input_id))
            .ok_or_else(|| format!("未找到待处理的选项提问：{}", req.input_id))?;

        let app = self
            .resolve_app(&input_rec.app_id)
            .ok_or_else(|| format!("未找到应用：{}", input_rec.app_id))?;

        if !app.is_local {
            let remote_val = self
                .call_remote_peer_router(
                    &app.device_id,
                    "POST",
                    "/v1/inputs/respond",
                    Some(&json!({
                        "input_id": input_rec.native_question_id.clone().unwrap_or(req.input_id),
                        "selected_options": req.selected_options,
                        "text": req.text,
                        "wait_for_turn": false,
                    })),
                    Duration::from_secs(20),
                )
                .await?;
            let mut snap: TurnSnapshot =
                serde_json::from_value(remote_val).map_err(|e| format!("解析远端选项回复失败：{e}"))?;
            snap.turn_id = input_rec.turn_id.clone();
            self.update_turn(snap.clone());
            if !req.wait_for_turn.unwrap_or(true) || snap.status != "running" {
                return Ok(snap);
            }
            return self
                .sync_remote_turn_until_pause_or_done(
                    &app,
                    &input_rec.caller_session_id,
                    None,
                    None,
                    None,
                    &input_rec.turn_id,
                )
                .await;
        }

        if let Some(waiter_tx) = self
            .inner
            .rpc_waiters
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&input_rec.input_id))
        {
            let mut answers = req.selected_options.clone();
            if let Some(t) = req.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                answers.push(t.to_string());
            }
            if answers.is_empty() {
                answers.push("确认继续".into());
            }
            let _ = waiter_tx.send(json!({
                "answers": answers,
                "question_id": input_rec.native_question_id,
            }));
            if let Some(mut cur) = self.get_turn(&input_rec.turn_id) {
                cur.status = "running".into();
                cur.reason = "input_resumed".into();
                cur.pending_input = None;
                cur.updated_at_ms = now_ms();
                self.update_turn(cur);
            }
            if !req.wait_for_turn.unwrap_or(true) {
                return self
                    .get_turn(&input_rec.turn_id)
                    .ok_or_else(|| "未找到关联的会话轮次记录".into());
            }
            return self.wait_for_resumed_turn(&input_rec.turn_id).await;
        }

        self.get_turn(&input_rec.turn_id)
            .ok_or_else(|| "未找到关联的会话轮次记录".into())
    }

    async fn run_local_codex_turn(
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
            .or_else(|| discovery::detect_codex_runtime().map(|(p, _, _)| p))
            .or_else(|| detect_local_binary("codex_app_server"))
            .ok_or_else(|| "本机未检测到 codex 可执行文件".to_string())?;
        let client = self.get_codex_client(&bin).await?;

        let cwd_clean = if Path::new(cwd).exists() {
            cwd.to_string()
        } else {
            let (_, _, _, default_root) = local_computer_identity();
            if Path::new(&default_root).exists() {
                default_root
            } else {
                home_dir().to_string_lossy().into_owned()
            }
        };

        let mut effective_effort = req
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !matches!(*s, "default" | "auto"))
            .map(str::to_string);
        let mut prompt = raw_prompt.to_string();

        if let Some(rest) = raw_prompt.strip_prefix("/fast")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            effective_effort = Some("low".into());
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                prompt = trimmed.to_string();
            }
        } else if let Some(rest) = raw_prompt.strip_prefix("/plan")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let trimmed = rest.trim();
            prompt = format!(
                "[Planning Mode] 请先给出清晰、可验证的分析与实施计划，在未获确认前不要直接改动无关文件：\n{}",
                if trimmed.is_empty() { "分析当前工作区状态" } else { trimmed }
            );
        } else if let Some(rest) = raw_prompt.strip_prefix("/review")
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        {
            let trimmed = rest.trim();
            prompt = format!(
                "[Code Review] 请审阅当前工作区的代码变更，指出潜在缺陷、边界情况与优化建议：\n{trimmed}"
            );
        }

        let model_clean = req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !matches!(*s, "default" | "auto"));

        let explicit_cwd_switched = req
            .cwd
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty())
            && prior_session
                .as_ref()
                .and_then(|s| s.cwd.as_deref())
                .is_some_and(|prev| {
                    Self::normalize_cwd_for_compare(prev)
                        != Self::normalize_cwd_for_compare(&cwd_clean)
                });
        let mut tid_opt = if explicit_cwd_switched {
            None
        } else {
            prior_session.as_ref().and_then(|s| s.native_thread_id.clone())
        };
        if let Some(ref existing_tid) = tid_opt {
            let resume_res = client
                .request(
                    "thread/resume",
                    json!({ "threadId": existing_tid }),
                    Duration::from_secs(10),
                )
                .await;
            if resume_res.is_err() {
                tid_opt = None;
            }
        }

        let tid = if let Some(existing) = tid_opt {
            existing
        } else {
            let mut start_params = json!({
                "cwd": cwd_clean,
                "approvalPolicy": "never",
                "sandbox": "workspace-write",
                "ephemeral": false,
                "serviceName": "octendra_router",
            });
            if let Some(m) = model_clean {
                if !app.available_models.is_empty()
                    && !app
                        .available_models
                        .iter()
                        .any(|opt| opt.id.eq_ignore_ascii_case(m))
                    && app
                        .default_model
                        .as_deref()
                        .is_none_or(|d| !d.eq_ignore_ascii_case(m))
                {
                    return Err(format!(
                        "{} 当前不支持或未开放指定模型 `{m}`",
                        app.display_name
                    ));
                }
                start_params["model"] = json!(m);
            }
            let thread_resp = client
                .request("thread/start", start_params, Duration::from_secs(15))
                .await
                .map_err(|e| format!("{} 启动对话或应用指定模型失败：{e}", app.display_name))?;
            let new_tid = thread_resp
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .ok_or_else(|| "Codex 未返回 thread.id".to_string())?
                .to_string();
            let title_line: String = prompt
                .lines()
                .next()
                .unwrap_or("新对话")
                .trim()
                .chars()
                .take(48)
                .collect();
            if !title_line.is_empty() {
                let _ = client
                    .request(
                        "thread/name/set",
                        json!({ "threadId": new_tid, "name": title_line }),
                        Duration::from_secs(4),
                    )
                    .await;
            }
            new_tid
        };

        if raw_prompt.trim() == "/compact" {
            let _ = client
                .request(
                    "thread/compact/start",
                    json!({ "threadId": tid }),
                    Duration::from_secs(20),
                )
                .await;
            let new_turns = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
            self.upsert_session(RouterSessionRecord {
                caller_session_id: req.caller_session_id.clone(),
                app_id: app.app_id.clone(),
                native_thread_id: Some(tid.clone()),
                last_operation_id: None,
                turn_count: new_turns,
                model: req.model.clone(),
                effort: effective_effort,
                cwd: Some(cwd_clean),
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
                reason: "thread_compacted".into(),
                text: Some(format!("已压缩 Codex 原生线程 `{tid}` 的上下文历史。")),
                native_thread_id: Some(tid),
                turn_number: new_turns,
                pending_permission: None,
                pending_input: None,
                suggested_options: Vec::new(),
                events: vec!["compacted".into()],
                updated_at_ms: now_ms(),
            };
            self.update_turn(snap.clone());
            return Ok(snap);
        }

        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        if let Ok(mut map) = client.thread_listeners.lock() {
            map.insert(tid.clone(), ev_tx);
        }

        let effective_policy = req
            .permission_mode
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| self.app_permission_mode(&app.app_id));
        let codex_approval_policy = if effective_policy == "interactive" {
            "untrusted"
        } else {
            "never"
        };

        let eff = effective_effort.as_deref().unwrap_or("medium");
        let mut turn_params = json!({
            "threadId": tid,
            "input": [{ "type": "text", "text": prompt }],
            "cwd": cwd_clean,
            "approvalPolicy": codex_approval_policy,
            "sandboxPolicy": {
                "type": "workspaceWrite",
                "writableRoots": [cwd_clean],
                "networkAccess": true
            },
            "effort": eff,
        });
        if let Some(m) = model_clean {
            if !app.available_models.is_empty()
                && !app
                    .available_models
                    .iter()
                    .any(|opt| opt.id.eq_ignore_ascii_case(m))
                && app
                    .default_model
                    .as_deref()
                    .is_none_or(|d| !d.eq_ignore_ascii_case(m))
            {
                if let Ok(mut map) = client.thread_listeners.lock() {
                    map.remove(&tid);
                }
                return Err(format!(
                    "{} 当前不支持或未开放指定模型 `{m}`",
                    app.display_name
                ));
            }
            turn_params["model"] = json!(m);
        }

        let turn_resp = client
            .request("turn/start", turn_params, Duration::from_secs(20))
            .await
            .inspect_err(|_| {
                if let Ok(mut map) = client.thread_listeners.lock() {
                    map.remove(&tid);
                }
            })
            .map_err(|e| format!("{} 发送消息失败：{e}", app.display_name))?;

        let expected_turn_id = turn_resp
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_string);

        let mut delta_buf = String::new();
        let mut completed_msg = String::new();
        let mut pending_delta_chunk = String::new();
        let mut last_delta_flush = tokio::time::Instant::now();

        let end_ev = loop {
            let Some(ev) = ev_rx.recv().await else {
                break Err("Codex 客户端连接已断开".to_string());
            };
            if ev.get("_server_request") == Some(&Value::Bool(true))
                && let Some(req_id) = ev.get("id").and_then(Value::as_u64)
            {
                let srv_method = ev.get("method").and_then(Value::as_str).unwrap_or("");
                let srv_params = ev.get("params").cloned().unwrap_or(Value::Null);
                if matches!(srv_method, "item/tool/requestUserInput" | "mcpServer/elicitation/request") {
                    let (q_id, q_title, q_prompt, q_options, multi) = parse_codex_user_input_request(&srv_params);
                    let input_id = Uuid::new_v4().to_string();
                    let input_rec = PendingUserInputRecord {
                        input_id: input_id.clone(),
                        turn_id: turn_id.to_string(),
                        caller_session_id: req.caller_session_id.clone(),
                        app_id: app.app_id.clone(),
                        app_display_name: app.display_name.clone(),
                        title: q_title,
                        prompt: q_prompt,
                        native_question_id: q_id.clone(),
                        options: q_options,
                        allow_free_text: true,
                        multi_select: multi,
                        created_at_ms: now_ms(),
                    };
                    let (w_tx, w_rx) = oneshot::channel();
                    if let Ok(mut g) = self.inner.inputs.lock() {
                        g.insert(input_id.clone(), input_rec.clone());
                    }
                    if let Ok(mut w) = self.inner.rpc_waiters.lock() {
                        w.insert(input_id, w_tx);
                    }
                    let paused = TurnSnapshot {
                        turn_id: turn_id.to_string(),
                        caller_session_id: req.caller_session_id.clone(),
                        app_id: app.app_id.clone(),
                        app_display_name: app.display_name.clone(),
                        device_name: app.device_name.clone(),
                        status: "waiting_input".into(),
                        reason: "user_input_required".into(),
                        text: (!delta_buf.is_empty()).then(|| delta_buf.clone()),
                        native_thread_id: Some(tid.clone()),
                        turn_number: prior_session.as_ref().map_or(1, |s| s.turn_count + 1),
                        pending_permission: None,
                        pending_input: Some(input_rec),
                        suggested_options: Vec::new(),
                        events: vec!["waiting_input".into()],
                        updated_at_ms: now_ms(),
                    };
                    self.update_turn(paused);
                    let user_val = w_rx
                        .await
                        .unwrap_or_else(|_| json!({ "answers": ["继续"] }));
                    let answers = user_val.get("answers").cloned().unwrap_or_else(|| json!(["继续"]));
                    let rpc_reply = if srv_method == "item/tool/requestUserInput" {
                        let key = q_id.unwrap_or_else(|| "default".into());
                        json!({
                            "id": req_id,
                            "result": {
                                "answers": {
                                    key: { "answers": answers }
                                }
                            }
                        })
                    } else {
                        json!({
                            "id": req_id,
                            "result": { "action": "accept", "content": { "selection": answers } }
                        })
                    };
                    let _ = client.write_json(&rpc_reply).await;
                    continue;
                }

                let cmd_preview = srv_params
                    .get("command")
                    .or_else(|| srv_params.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("执行工作区命令或文件变更");
                let should_prompt = effective_policy == "interactive"
                    || (effective_policy == "auto_safe" && is_sensitive_command_text(cmd_preview));
                if !should_prompt {
                    let auto_reply = if srv_method == "item/permissions/requestApproval" {
                        json!({
                            "id": req_id,
                            "result": {
                                "permissions": srv_params.get("permissions").cloned().unwrap_or_else(|| json!({})),
                                "scope": "session"
                            }
                        })
                    } else {
                        json!({ "id": req_id, "result": { "decision": "acceptForSession" } })
                    };
                    let _ = client.write_json(&auto_reply).await;
                    continue;
                }

                let perm_id = Uuid::new_v4().to_string();
                let perm = PendingPermissionRecord {
                    permission_id: perm_id.clone(),
                    turn_id: turn_id.to_string(),
                    caller_session_id: req.caller_session_id.clone(),
                    app_id: app.app_id.clone(),
                    app_display_name: app.display_name.clone(),
                    kind: "command_approval".into(),
                    title: format!("{} 请求命令/文件执行授权", app.display_name),
                    summary: format!("待确认操作：`{cmd_preview}`"),
                    options: default_permission_options(),
                    native_operation_id: None,
                    action_id: None,
                    action_version: None,
                    created_at_ms: now_ms(),
                };
                let (w_tx, w_rx) = oneshot::channel();
                if let Ok(mut g) = self.inner.permissions.lock() {
                    g.insert(perm_id.clone(), perm.clone());
                }
                if let Ok(mut w) = self.inner.rpc_waiters.lock() {
                    w.insert(perm_id, w_tx);
                }
                let paused = TurnSnapshot {
                    turn_id: turn_id.to_string(),
                    caller_session_id: req.caller_session_id.clone(),
                    app_id: app.app_id.clone(),
                    app_display_name: app.display_name.clone(),
                    device_name: app.device_name.clone(),
                    status: "waiting_permission".into(),
                    reason: "command_approval".into(),
                    text: (!delta_buf.is_empty()).then(|| delta_buf.clone()),
                    native_thread_id: Some(tid.clone()),
                    turn_number: prior_session.as_ref().map_or(1, |s| s.turn_count + 1),
                    pending_permission: Some(perm),
                    pending_input: None,
                    suggested_options: Vec::new(),
                    events: vec!["waiting_permission:command_approval".into()],
                    updated_at_ms: now_ms(),
                };
                self.update_turn(paused);
                let user_decision = w_rx
                    .await
                    .unwrap_or_else(|_| json!({ "decision": "deny", "approved": false }));
                let dec = user_decision
                    .get("decision")
                    .and_then(Value::as_str)
                    .unwrap_or("allow_once");
                let codex_dec = match dec {
                    "allow_session" => "acceptForSession",
                    "deny" | "decline" => "decline",
                    _ => "accept",
                };
                let _ = client
                    .write_json(&json!({ "id": req_id, "result": { "decision": codex_dec } }))
                    .await;
                if let Some(mut cur) = self.get_turn(turn_id) {
                    cur.status = "running".into();
                    cur.pending_permission = None;
                    cur.updated_at_ms = now_ms();
                    self.update_turn(cur);
                }
                continue;
            }
            match ev.get("method").and_then(Value::as_str).unwrap_or("") {
                "item/agentMessage/delta" => {
                    if let Some(d) = ev.pointer("/params/delta").and_then(Value::as_str) {
                        delta_buf.push_str(d);
                        pending_delta_chunk.push_str(d);
                        if pending_delta_chunk.len() >= 24
                            || last_delta_flush.elapsed() >= Duration::from_millis(160)
                        {
                            self.emit_turn_delta(
                                app,
                                &req.caller_session_id,
                                turn_id,
                                &pending_delta_chunk,
                                &delta_buf,
                            );
                            pending_delta_chunk.clear();
                            last_delta_flush = tokio::time::Instant::now();
                        }
                    }
                }
                "item/started" => {
                    if let Some(item) = ev.pointer("/params/item") {
                        let itype = item.get("type").and_then(Value::as_str).unwrap_or("");
                        if itype == "commandExecution"
                            && let Some(c) = item.get("command").and_then(Value::as_str)
                        {
                            let short: String = c.chars().take(64).collect();
                            self.emit_turn_tool(
                                app,
                                &req.caller_session_id,
                                turn_id,
                                &format!("执行命令：{short}"),
                            );
                        } else if itype == "mcpToolCall"
                            && let Some(t) = item.get("tool").and_then(Value::as_str)
                        {
                            self.emit_turn_tool(
                                app,
                                &req.caller_session_id,
                                turn_id,
                                &format!("调用工具：{t}"),
                            );
                        }
                    }
                }
                "item/completed" => {
                    if let Some(item) = ev.pointer("/params/item")
                        && item.get("type").and_then(Value::as_str) == Some("agentMessage")
                        && let Some(txt) = item.get("text").and_then(Value::as_str)
                        && !txt.trim().is_empty()
                    {
                        completed_msg = txt.to_string();
                    }
                }
                "turn/completed" => {
                    let tid_ev = ev.pointer("/params/turn/id").and_then(Value::as_str);
                    if expected_turn_id.is_none() || tid_ev == expected_turn_id.as_deref() {
                        break Ok(ev);
                    }
                }
                _ => {}
            }
        };
        if !pending_delta_chunk.is_empty() {
            self.emit_turn_delta(app, &req.caller_session_id, turn_id, &pending_delta_chunk, &delta_buf);
        }

        if let Ok(mut map) = client.thread_listeners.lock() {
            map.remove(&tid);
        }

        let done = end_ev?;
        if done.pointer("/params/turn/status").and_then(Value::as_str) == Some("failed") {
            let emsg = done
                .pointer("/params/turn/error/message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(format!("Codex 执行失败：{emsg}"));
        }

        let final_reply = if !completed_msg.trim().is_empty() {
            completed_msg
        } else {
            delta_buf
        };
        let new_turns = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
        self.upsert_session(RouterSessionRecord {
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            native_thread_id: Some(tid.clone()),
            last_operation_id: None,
            turn_count: new_turns,
            model: req.model.clone(),
            effort: effective_effort,
            cwd: Some(cwd_clean),
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
            text: Some(if final_reply.trim().is_empty() {
                format!("{} 已完成执行。", app.display_name)
            } else {
                final_reply.trim().to_string()
            }),
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

    async fn run_local_praxis_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        raw_prompt: &str,
        cwd: &str,
        turn_id: &str,
    ) -> Result<TurnSnapshot, String> {
        let mut tier = "auto";
        let mut goal = raw_prompt.to_string();
        if let Some(rest) = raw_prompt.strip_prefix("/direct ") {
            tier = "direct";
            goal = rest.trim().to_string();
        } else if let Some(rest) = raw_prompt.strip_prefix("/full ") {
            tier = "full";
            goal = rest.trim().to_string();
        }
        let scheme = "http";
        let url = format!("{scheme}://127.0.0.1:17420/api/v1/telos/dispatch");
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(4))
            .build()
            .map_err(|e| e.to_string())?;
        let mut payload = json!({
            "goal": goal,
            "workspace_target": cwd,
            "tier": tier,
            "wait_for_completion": tier == "direct",
        });
        if let Some(ref m) = req.model {
            payload["model"] = json!(m);
        }
        if let Some(ref e) = req.effort {
            payload["effort"] = json!(e);
        }
        let resp = client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("调用 Praxis 失败：{e}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Praxis 返回 {st}：{body}"));
        }
        let val: Value = resp.json().await.map_err(|e| e.to_string())?;
        let text = val
            .get("latest_output")
            .or_else(|| val.get("summary"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(
                || {
                    let tid = val.get("task_id").and_then(Value::as_str).unwrap_or("已受理");
                    format!("已交由 Praxis 项目编排推进（任务 `{tid}`，工作区 `{cwd}`）。")
                },
                str::to_string,
            );
        let snap = TurnSnapshot {
            turn_id: turn_id.to_string(),
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            app_display_name: app.display_name.clone(),
            device_name: app.device_name.clone(),
            status: "completed".into(),
            reason: "completed".into(),
            text: Some(text),
            native_thread_id: val.get("task_id").and_then(Value::as_str).map(str::to_string),
            turn_number: 1,
            pending_permission: None,
            pending_input: None,
            suggested_options: Vec::new(),
            events: vec!["praxis_dispatched".into()],
            updated_at_ms: now_ms(),
        };
        self.update_turn(snap.clone());
        Ok(snap)
    }

    async fn run_local_custom_command_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        prompt: &str,
        cwd: &str,
        turn_id: &str,
        prior_session: Option<RouterSessionRecord>,
    ) -> Result<TurnSnapshot, String> {
        let exe = app
            .executable
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("自定义应用「{}」未配置可执行命令", app.display_name))?;
        let mut history = prior_session
            .as_ref()
            .map_or_else(Vec::new, |s| s.history.clone());
        let new_turns = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
        let history_json = serde_json::to_string(&history).unwrap_or_else(|_| "[]".into());

        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd.exe");
            c.args(["/C", exe]);
            c
        } else {
            let mut c = Command::new("/bin/sh");
            c.args(["-lc", exe]);
            c
        };
        cmd.env("PATH", enriched_path())
            .env("OCTENDRA_PROMPT", prompt)
            .env("OCTENDRA_CALLER_SESSION_ID", &req.caller_session_id)
            .env("OCTENDRA_SESSION_TURN", new_turns.to_string())
            .env("OCTENDRA_HISTORY_JSON", &history_json)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if Path::new(cwd).exists() {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().map_err(|e| format!("启动自定义应用失败：{e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| format!("自定义应用读取失败：{e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if !out.status.success() && stdout.is_empty() {
            return Err(format!("自定义应用退出异常：{stderr}"));
        }
        let reply = if stdout.is_empty() { stderr } else { stdout };
        history.push(SessionMessageItem {
            role: "user".into(),
            content: prompt.to_string(),
        });
        history.push(SessionMessageItem {
            role: "assistant".into(),
            content: reply.clone(),
        });
        if history.len() > 24 {
            let drain_count = history.len().saturating_sub(24);
            history.drain(0..drain_count);
        }
        self.upsert_session(RouterSessionRecord {
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            native_thread_id: Some(format!("custom:{}", req.caller_session_id)),
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
            native_thread_id: Some(format!("custom:{}", req.caller_session_id)),
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

    async fn run_local_openai_compatible_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        prompt: &str,
        turn_id: &str,
        prior_session: Option<RouterSessionRecord>,
    ) -> Result<TurnSnapshot, String> {
        let base_url = app
            .executable
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("http://127.0.0.1:11434/v1");
        let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let model = match req
            .model
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !matches!(*s, "default" | "auto"))
        {
            Some(explicit) => explicit.to_string(),
            None => Self::detect_first_openai_model(base_url)
                .await
                .unwrap_or_else(|| "qwen2.5:latest".to_string()),
        };
        let mut history = prior_session
            .as_ref()
            .map_or_else(Vec::new, |s| s.history.clone());
        history.push(SessionMessageItem {
            role: "user".into(),
            content: prompt.to_string(),
        });
        let messages: Vec<Value> = history
            .iter()
            .map(|m| json!({ "role": m.role, "content": m.content }))
            .collect();

        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(4))
            .build()
            .map_err(|e| e.to_string())?;
        let mut http_req = client.post(&endpoint).json(&json!({
            "model": model,
            "messages": messages,
            "stream": false,
        }));
        if let Ok(api_key) = std::env::var("OCTENDRA_OPENAI_API_KEY")
            && !api_key.trim().is_empty()
        {
            http_req = http_req.bearer_auth(api_key.trim());
        }
        let resp = http_req
            .send()
            .await
            .map_err(|e| format!("OpenAI 兼容端点请求失败：{e}"))?;
        if !resp.status().is_success() {
            let st = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("OpenAI 兼容端点返回 {st}：{body}"));
        }
        let val: Value = resp.json().await.map_err(|e| e.to_string())?;
        let reply = val
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        history.push(SessionMessageItem {
            role: "assistant".into(),
            content: reply.clone(),
        });
        if history.len() > 24 {
            let drain_count = history.len().saturating_sub(24);
            history.drain(0..drain_count);
        }
        let new_turns = prior_session.as_ref().map_or(1, |s| s.turn_count + 1);
        self.upsert_session(RouterSessionRecord {
            caller_session_id: req.caller_session_id.clone(),
            app_id: app.app_id.clone(),
            native_thread_id: Some(format!("openai:{}", req.caller_session_id)),
            last_operation_id: None,
            turn_count: new_turns,
            model: Some(model),
            effort: req.effort.clone(),
            cwd: None,
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
            native_thread_id: Some(format!("openai:{}", req.caller_session_id)),
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

    fn build_cloud_http_client(base_url: &str, timeout_dur: Duration) -> Result<reqwest::Client, String> {
        let lower = base_url.trim().to_ascii_lowercase();
        let is_loopback = lower.starts_with("http://127.0.0.1")
            || lower.starts_with("http://localhost")
            || lower.starts_with("http://[::1]")
            || lower.starts_with("https://127.0.0.1")
            || lower.starts_with("https://localhost");
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .tcp_keepalive(Duration::from_secs(15))
            .http2_keep_alive_interval(Duration::from_secs(15))
            .http2_keep_alive_timeout(Duration::from_secs(5))
            .timeout(timeout_dur);
        if is_loopback {
            builder = builder.no_proxy();
        }
        builder.build().map_err(|e| e.to_string())
    }

    /// Forwards an HTTP-equivalent JSON RPC to a remote edge computer:
    /// - When running on Cloud Hub (or directly connected as Hub), dispatches downlink over `/v1/peer/{poll,reply}`.
    /// - When running on an Edge computer with `cloud_upstream`, forwards via HTTPS Bearer REST to Cloud Gateway.
    /// Zero fallback: if the target device is offline or unreachable, returns an explicit error immediately.
    async fn call_remote_peer_router(
        &self,
        device_id: &str,
        method: &str,
        path: &str,
        body: Option<&Value>,
        timeout_dur: Duration,
    ) -> Result<Value, String> {
        if self.is_peer_spoke_online(device_id) {
            return self
                .call_spoke_peer_rpc(device_id, method, path, body.cloned(), timeout_dur)
                .await;
        }
        if !self.inner.cloud_hub_mode
            && let Some(ref upstream) = self.inner.cloud_upstream
        {
            let base = upstream.gateway_url.trim_end_matches('/');
            let clean_path = if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            };
            let url = format!("{base}{clean_path}");
            let parsed_method = Method::from_bytes(method.trim().as_bytes())
                .map_err(|e| format!("无效 HTTP 方法 {method}：{e}"))?;
            let client = Self::build_cloud_http_client(base, timeout_dur)?;
            let mut req_builder = client
                .request(parsed_method, &url)
                .bearer_auth(upstream.bearer_token.trim());
            if let Some(payload) = body {
                req_builder = req_builder.json(payload);
            }
            let resp = req_builder
                .send()
                .await
                .map_err(|e| format!("请求 Octendra Cloud Gateway ({url}) 失败：{e}"))?;
            let status = resp.status();
            let raw = resp
                .text()
                .await
                .map_err(|e| format!("读取 Cloud Gateway 响应失败：{e}"))?;
            let val: Value = serde_json::from_str(&raw)
                .unwrap_or_else(|_| json!({ "status": status.as_u16(), "message": raw }));
            if !status.is_success() {
                let msg = val
                    .get("message")
                    .or_else(|| val.get("reason"))
                    .or_else(|| val.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or(&raw);
                return Err(format!("Cloud Gateway HTTP {status}：{msg}"));
            }
            return Ok(val);
        }
        Err(format!(
            "目标设备 {device_id} 当前离线（未连接到 Octendra Cloud Gateway）"
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn sync_remote_turn_until_pause_or_done(
        &self,
        app: &RegisteredApp,
        caller_session_id: &str,
        model: Option<String>,
        effort: Option<String>,
        cwd: Option<String>,
        turn_id: &str,
    ) -> Result<TurnSnapshot, String> {
        let mut consecutive_poll_errors: u32 = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let remote_res = self
                .call_remote_peer_router(
                    &app.device_id,
                    "GET",
                    &format!("/v1/turns/{turn_id}"),
                    None,
                    Duration::from_secs(12),
                )
                .await;
            let remote_val = match remote_res {
                Ok(v) => {
                    consecutive_poll_errors = 0;
                    v
                }
                Err(err_msg) => {
                    if err_msg.contains("404") || err_msg.contains("not_found") {
                        return Err(format!(
                            "远端节点 {} 上的回合 `{turn_id}` 不存在（服务可能已重启）",
                            app.device_name
                        ));
                    }
                    consecutive_poll_errors += 1;
                    let direct_spoke_alive = self.is_peer_spoke_online(&app.device_id);
                    let has_upstream = self.inner.cloud_upstream.is_some();
                    if (!direct_spoke_alive && !has_upstream && consecutive_poll_errors >= 12)
                        || consecutive_poll_errors >= 30
                    {
                        return Err(format!(
                            "远端节点 {} 连接已断开：{err_msg}",
                            app.device_name
                        ));
                    }
                    continue;
                }
            };

            let Ok(mut snap) = serde_json::from_value::<TurnSnapshot>(remote_val) else {
                continue;
            };
            snap.turn_id = turn_id.to_string();

            if snap.status == "running" {
                if let Ok(mut g) = self.inner.turns.lock()
                    && let Some(cur) = g.get_mut(turn_id)
                {
                    if snap.text.is_some() {
                        cur.text.clone_from(&snap.text);
                    }
                    if !snap.events.is_empty() {
                        cur.events.clone_from(&snap.events);
                    }
                    cur.updated_at_ms = now_ms();
                }
                continue;
            }

            if snap.status == "failed" {
                self.update_turn(snap.clone());
                return Err(snap.reason);
            }

            if let Some(ref remote_perm) = snap.pending_permission {
                let local_perm_id = Uuid::new_v4().to_string();
                let mut proxied = remote_perm.clone();
                proxied.action_id = Some(remote_perm.permission_id.clone());
                proxied.permission_id = local_perm_id.clone();
                proxied.turn_id = turn_id.to_string();
                if let Ok(mut g) = self.inner.permissions.lock() {
                    g.insert(local_perm_id, proxied.clone());
                }
                snap.pending_permission = Some(proxied);
            } else if let Some(ref remote_input) = snap.pending_input {
                let local_input_id = Uuid::new_v4().to_string();
                let mut proxied = remote_input.clone();
                proxied.native_question_id = Some(remote_input.input_id.clone());
                proxied.input_id = local_input_id.clone();
                proxied.turn_id = turn_id.to_string();
                if let Ok(mut g) = self.inner.inputs.lock() {
                    g.insert(local_input_id, proxied.clone());
                }
                snap.pending_input = Some(proxied);
            } else if snap.status == "completed" {
                self.upsert_session(RouterSessionRecord {
                    caller_session_id: caller_session_id.to_string(),
                    app_id: app.app_id.clone(),
                    native_thread_id: snap.native_thread_id.clone(),
                    last_operation_id: None,
                    turn_count: snap.turn_number,
                    model,
                    effort,
                    cwd,
                    history: Vec::new(),
                    updated_at_ms: now_ms(),
                });
            }
            self.update_turn(snap.clone());
            return Ok(snap);
        }
    }

    async fn run_remote_octendra_turn(
        &self,
        app: &RegisteredApp,
        req: &TurnRequest,
        prompt: &str,
        cwd: &str,
        turn_id: &str,
    ) -> Result<TurnSnapshot, String> {
        if self.inner.cloud_hub_mode {
            if !self.is_peer_spoke_online(&app.device_id) {
                return Err(format!(
                    "目标设备 {} ({}) 当前离线，请确认该设备上的 octendra-code agent 已运行并在线",
                    app.device_name, app.device_id
                ));
            }
            if !app.online {
                return Err(format!(
                    "目标设备 {} 上的应用「{}」当前未运行或离线，请先在目标设备启动并登录桌面端（禁用隐式回退）",
                    app.device_name, app.display_name
                ));
            }
        }
        if !self.is_peer_spoke_online(&app.device_id) && self.inner.cloud_upstream.is_none() {
            return Err(format!(
                "目标设备 {} ({}) 当前离线，且未配置 Octendra Cloud Gateway 上游连接",
                app.device_name, app.device_id
            ));
        }

        let remote_payload = json!({
            "caller_session_id": req.caller_session_id,
            "app_id": app.app_id,
            "prompt": prompt,
            "cwd": cwd,
            "model": req.model,
            "effort": req.effort,
            "turn_id": turn_id,
            "wait": false,
            "wait_for_idle_ms": req.wait_for_idle_ms,
            "interactive_permissions": req.interactive_permissions,
            "permission_mode": req.permission_mode,
            "disable_peer_failover": true,
        });
        let remote_val = self
            .call_remote_peer_router(
                &app.device_id,
                "POST",
                "/v1/sessions/turn",
                Some(&remote_payload),
                Duration::from_secs(20),
            )
            .await?;
        let mut initial_snap: TurnSnapshot = serde_json::from_value(remote_val)
            .map_err(|e| format!("解析远端 TurnSnapshot 失败：{e}"))?;
        initial_snap.turn_id = turn_id.to_string();
        if initial_snap.status != "running" {
            if initial_snap.status == "failed" {
                self.update_turn(initial_snap.clone());
                return Err(initial_snap.reason);
            }
            if initial_snap.status == "completed" {
                self.upsert_session(RouterSessionRecord {
                    caller_session_id: req.caller_session_id.clone(),
                    app_id: app.app_id.clone(),
                    native_thread_id: initial_snap.native_thread_id.clone(),
                    last_operation_id: None,
                    turn_count: initial_snap.turn_number,
                    model: req.model.clone(),
                    effort: req.effort.clone(),
                    cwd: Some(cwd.to_string()),
                    history: Vec::new(),
                    updated_at_ms: now_ms(),
                });
            }
            self.update_turn(initial_snap.clone());
            return Ok(initial_snap);
        }
        self.sync_remote_turn_until_pause_or_done(
            app,
            &req.caller_session_id,
            req.model.clone(),
            req.effort.clone(),
            Some(cwd.to_string()),
            turn_id,
        )
        .await
    }
}

fn load_json_map<T: for<'de> Deserialize<'de>>(path: &Path) -> HashMap<String, T> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_json_map<T: Serialize>(path: &Path, map: &HashMap<String, T>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(map).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ============================================================================
// HTTP Axum Router & Standalone Loopback Daemon
// ============================================================================

/// Builds the Axum `/v1/*` client HTTP routes (excluding `/healthz` and `/v1/peer/*`),
/// suitable for mounting inside Cloud `octendra-gateway` alongside Gateway auth & peer endpoints.
pub fn app_router_client_routes(state: AgentRouterState) -> Router {
    Router::new()
        .route("/v1/health", get(handle_health))
        .route("/v1/events", get(handle_poll_events))
        .route("/v1/apps", get(handle_list_apps).post(handle_upsert_app))
        .route("/v1/apps/discover", post(handle_discover_apps))
        .route("/v1/apps/{app_id}", delete(handle_delete_app))
        .route("/v1/apps/{app_id}/policy", post(handle_set_app_policy))
        .route("/v1/models", get(handle_list_models))
        .route("/v1/sessions", get(handle_list_sessions))
        .route("/v1/sessions/reset", post(handle_reset_session))
        .route("/v1/sessions/turn", post(handle_session_turn))
        .route("/v1/turns/{turn_id}", get(handle_get_turn))
        .route("/v1/permissions", get(handle_list_permissions))
        .route("/v1/permissions/respond", post(handle_respond_permission))
        .route("/v1/inputs", get(handle_list_inputs))
        .route("/v1/inputs/respond", post(handle_respond_input))
        .route("/v1/gemini/readiness", get(handle_gemini_readiness))
        .route(
            "/v1/gemini/prompt-accessibility",
            post(handle_gemini_prompt_accessibility),
        )
        .with_state(state)
}

/// Builds the complete Axum HTTP router for the Octendra Host Agent Router.
pub fn router_with_state(state: AgentRouterState) -> Router {
    app_router_client_routes(state.clone()).merge(
        Router::new()
            .route("/healthz", get(handle_health))
            .route("/v1/peer/poll", post(handle_peer_poll))
            .route("/v1/peer/reply", post(handle_peer_reply))
            .route("/v1/ui/devices", get(handle_ui_devices))
            .route("/v1/ui/bind_window", post(handle_ui_bind_window))
            .route("/v1/ui/observe", post(handle_ui_observe))
            .route("/v1/ui/act", post(handle_ui_act))
            .with_state(state),
    )
}

async fn warm_local_desktop_accessibility(state: AgentRouterState) {
    if !cfg!(target_os = "macos") || state.is_cloud_hub() {
        return;
    }
    if state.refresh_local_macos_accessibility().await
        && AgentRouterState::local_gemini_app_installed()
    {
        let _ = state
            .call_gemini_desktop("capabilities", &json!({ "refresh": true }))
            .await;
        discovery::invalidate_discovery_cache();
    }
}

/// Serves the unified in-process Edge App Router embedded inside `octendra-code agent`:
/// binds loopback HTTP (`127.0.0.1:<port>`) for local callers while running the persistent
/// outbound long-poll worker against Cloud `octendra-gateway` (`/v1/peer/{poll,reply}`).
///
/// # Errors
///
/// Returns an error if binding a non-loopback address or serving HTTP fails.
pub async fn serve_embedded_edge_agent(
    addr: SocketAddr,
    router_storage_dir: PathBuf,
    upstream: CloudUpstreamConfig,
) -> std::io::Result<()> {
    serve_embedded_edge_agent_with_host_ui(addr, router_storage_dir, upstream, None).await
}

/// Serves the unified in-process Edge App Router embedded inside `octendra-code agent`
/// with an optional local [`HostUiBridge`] for locality-first desktop UI automation.
///
/// # Errors
///
/// Returns an error if binding a non-loopback address or serving HTTP fails.
pub async fn serve_embedded_edge_agent_with_host_ui(
    addr: SocketAddr,
    router_storage_dir: PathBuf,
    upstream: CloudUpstreamConfig,
    host_ui: Option<Arc<dyn HostUiBridge>>,
) -> std::io::Result<()> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "octendra embedded edge router requires a loopback bind address",
        ));
    }
    let state = match host_ui {
        Some(bridge) => AgentRouterState::with_cloud_upstream_and_host_ui(
            router_storage_dir,
            upstream.clone(),
            bridge,
        ),
        None => AgentRouterState::with_cloud_upstream(router_storage_dir, upstream.clone()),
    };
    let listener = TcpListener::bind(addr).await?;
    let local_bound = listener.local_addr().unwrap_or(addr);

    tokio::spawn(warm_local_desktop_accessibility(state.clone()));

    let spoke_state = state.clone();
    tokio::spawn(async move {
        spoke_state.sync_upstream_apps(false).await;
        run_cloud_spoke_worker(local_bound, spoke_state, upstream).await;
    });

    axum::serve(listener, router_with_state(state)).await
}

/// Serves the standalone loopback Host Agent Router HTTP daemon (and starts the background Spoke
/// long-poll worker when `cloud_upstream` or `peer_spoke.json` is configured).
///
/// # Errors
///
/// Returns an error if binding a non-loopback address or serving HTTP fails.
pub async fn serve(addr: SocketAddr, state: AgentRouterState) -> std::io::Result<()> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "octendra-agent-router requires a loopback bind address",
        ));
    }
    let listener = TcpListener::bind(addr).await?;
    let local_bound = listener.local_addr().unwrap_or(addr);
    if let Some(upstream) = state.inner.cloud_upstream.clone() {
        let spoke_state = state.clone();
        tokio::spawn(async move {
            spoke_state.sync_upstream_apps(false).await;
            run_cloud_spoke_worker(local_bound, spoke_state, upstream).await;
        });
    } else {
        let (_, local_device_id, _, _) = local_computer_identity();
        let force_spoke = std::env::var("OCTENDRA_FORCE_SPOKE")
            .ok()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        if (local_device_id != MAC_STUDIO_DEVICE_ID || force_spoke)
            && let Some(bundle) = state.load_peer_spoke_bundle()
        {
            let spoke_state = state.clone();
            tokio::spawn(async move {
                run_peer_spoke_worker(local_bound, spoke_state, bundle).await;
            });
        }
    }
    tokio::spawn(warm_local_desktop_accessibility(state.clone()));
    axum::serve(listener, router_with_state(state)).await
}

async fn build_poll_registration(
    state: &AgentRouterState,
    device_id_override: Option<&str>,
) -> (String, PeerPollRegistration) {
    let st_scan = state.clone();
    let (device_name, auto_device_id, platform, default_cwd, local_apps) =
        tokio::task::spawn_blocking(move || {
            let (dn, did, pf, cwd) = local_computer_identity();
            let apps = st_scan.list_local_only_apps();
            (dn, did, pf, cwd, apps)
        })
        .await
        .unwrap_or_else(|_| {
            let (dn, did, pf, cwd) = local_computer_identity();
            (dn, did, pf, cwd, Vec::new())
        });
    let effective_id = device_id_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or(auto_device_id, str::to_string);
    let wait_ms = if state.has_running_turns() {
        1_200
    } else {
        20_000
    };
    let reg = PeerPollRegistration {
        device_id: effective_id.clone(),
        device_name,
        platform,
        default_cwd,
        local_apps,
        spoke_events: state.drain_spoke_events_for_hub(),
        wait_ms,
    };
    (effective_id, reg)
}

async fn execute_inbound_peer_rpc(
    local_addr: SocketAddr,
    device_id: String,
    rpc_req: PeerRpcRequest,
) -> PeerRpcReply {
    let b64_body = rpc_req
        .body
        .as_ref()
        .and_then(|v| serde_json::to_vec(v).ok())
        .map(|bytes| BASE64_STANDARD.encode(bytes));
    let exec = rpc_loopback_json(
        local_addr,
        &rpc_req.method,
        &rpc_req.path,
        b64_body.as_deref(),
        rpc_req.timeout_ms,
    )
    .await;
    match exec {
        Ok(result) => PeerRpcReply {
            device_id,
            request_id: rpc_req.request_id,
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(err) => PeerRpcReply {
            device_id,
            request_id: rpc_req.request_id,
            ok: false,
            result: None,
            error: Some(err),
        },
    }
}

async fn run_cloud_spoke_worker(
    local_addr: SocketAddr,
    state: AgentRouterState,
    upstream: CloudUpstreamConfig,
) {
    let base = upstream.gateway_url.trim_end_matches('/').to_string();
    let Ok(client) = AgentRouterState::build_cloud_http_client(&base, Duration::from_secs(34))
    else {
        return;
    };
    let token = upstream.bearer_token.trim().to_string();
    let poll_url = format!("{base}/v1/peer/poll");
    let reply_url = format!("{base}/v1/peer/reply");

    let mut backoff_secs = 2u64;
    loop {
        let (device_id, reg) =
            build_poll_registration(&state, Some(&upstream.local_device_id)).await;
        let poll_res = client
            .post(&poll_url)
            .bearer_auth(&token)
            .json(&reg)
            .send()
            .await;

        match poll_res {
            Ok(resp) => {
                let status = resp.status();
                let raw = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    tracing::warn!(
                        %status,
                        gateway = %base,
                        "cloud peer poll rejected"
                    );
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(15);
                    continue;
                }
                let Ok(val) = serde_json::from_str::<Value>(&raw) else {
                    tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    backoff_secs = (backoff_secs * 2).min(15);
                    continue;
                };
                backoff_secs = 2;
                if let Some(req_val) = val.get("request").filter(|v| !v.is_null())
                    && let Ok(rpc_req) = serde_json::from_value::<PeerRpcRequest>(req_val.clone())
                {
                    let cli = client.clone();
                    let rep_url = reply_url.clone();
                    let tok = token.clone();
                    tokio::spawn(async move {
                        let reply = execute_inbound_peer_rpc(local_addr, device_id, rpc_req).await;
                        for delay_secs in [0u64, 1, 2, 4] {
                            if delay_secs > 0 {
                                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                            }
                            if let Ok(r) = cli
                                .post(&rep_url)
                                .bearer_auth(&tok)
                                .json(&reply)
                                .send()
                                .await
                                && r.status().is_success()
                            {
                                break;
                            }
                        }
                    });
                }
            }
            Err(err) => {
                tracing::debug!(error = %err, gateway = %base, "cloud peer poll transport error");
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(15);
            }
        }
    }
}

async fn post_spoke_to_hub(
    direct_client: &reqwest::Client,
    proxy_client: &reqwest::Client,
    prefer_proxy: &AtomicBool,
    bundle: &PeerSpokeBundle,
    action_header: &str,
    rpc_method: &str,
    params: Value,
) -> Result<Value, String> {
    let hub_url = bundle.hub_url.trim_end_matches('/');
    let is_direct_peer = hub_url.ends_with("/v1/peer/poll") || hub_url.ends_with("/v1/peer");
    let (target_url, body) = if is_direct_peer {
        let base = hub_url
            .trim_end_matches("/poll")
            .trim_end_matches("/reply");
        let suffix = if rpc_method.ends_with(".poll") {
            "poll"
        } else {
            "reply"
        };
        (format!("{base}/{suffix}"), params)
    } else {
        (
            bundle.hub_url.clone(),
            json!({
                "method": rpc_method,
                "params": params,
            }),
        )
    };

    let send_once = |client: &reqwest::Client| {
        client
            .post(&target_url)
            .header("X-Praxis-Action", action_header)
            .header(PEER_KEY_HEADER, bundle.peer_key.trim())
            .json(&body)
            .send()
    };

    let try_proxy_first = prefer_proxy.load(Ordering::Relaxed);
    let (first_cli, second_cli) = if try_proxy_first {
        (proxy_client, direct_client)
    } else {
        (direct_client, proxy_client)
    };

    let resp = if let Ok(r) = send_once(first_cli).await {
        r
    } else {
        let fallback_resp = send_once(second_cli)
            .await
            .map_err(|e| format!("连接 Hub ({target_url}) 失败：{e}"))?;
        prefer_proxy.store(!try_proxy_first, Ordering::Relaxed);
        fallback_resp
    };
    let status = resp.status();
    let raw = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("Hub HTTP {status}: {raw}"));
    }
    let val: Value = serde_json::from_str(&raw).map_err(|e| format!("解析 Hub JSON 失败：{e}"))?;
    if val.get("ok") == Some(&Value::Bool(false)) {
        let err = val
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("hub error");
        return Err(err.to_string());
    }
    if let Some(inner) = val.get("result") {
        Ok(inner.clone())
    } else {
        Ok(val)
    }
}

async fn run_peer_spoke_worker(
    local_addr: SocketAddr,
    state: AgentRouterState,
    bundle: PeerSpokeBundle,
) {
    let Ok(direct_client) = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(4))
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(34))
        .build()
    else {
        return;
    };
    let Ok(proxy_client) = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(0)
        .timeout(Duration::from_secs(34))
        .build()
    else {
        return;
    };
    let prefer_proxy = Arc::new(AtomicBool::new(false));

    let mut backoff_secs = 2u64;
    loop {
        let st_scan = state.clone();
        let (device_name, device_id, platform, default_cwd, local_apps) =
            tokio::task::spawn_blocking(move || {
                let (dn, did, pf, cwd) = local_computer_identity();
                let apps = st_scan.list_local_only_apps();
                (dn, did, pf, cwd, apps)
            })
            .await
            .unwrap_or_else(|_| {
                let (dn, did, pf, cwd) = local_computer_identity();
                (dn, did, pf, cwd, Vec::new())
            });
        let poll_wait_ms = if state.has_running_turns() {
            1_200
        } else {
            20_000
        };
        let reg = PeerPollRegistration {
            device_id: device_id.clone(),
            device_name,
            platform,
            default_cwd,
            local_apps,
            spoke_events: state.drain_spoke_events_for_hub(),
            wait_ms: poll_wait_ms,
        };
        let poll_res = post_spoke_to_hub(
            &direct_client,
            &proxy_client,
            &prefer_proxy,
            &bundle,
            "octendra_router_poll",
            "octendra_router.poll",
            serde_json::to_value(&reg).unwrap_or(Value::Null),
        )
        .await;

        match poll_res {
            Ok(val) => {
                backoff_secs = 2;
                if let Some(req_val) = val.get("request").filter(|v| !v.is_null())
                    && let Ok(rpc_req) = serde_json::from_value::<PeerRpcRequest>(req_val.clone())
                {
                    let d_cli = direct_client.clone();
                    let p_cli = proxy_client.clone();
                    let pref = Arc::clone(&prefer_proxy);
                    let bnd = bundle.clone();
                    let dev_id = device_id.clone();
                    tokio::spawn(async move {
                        let b64_body = rpc_req
                            .body
                            .as_ref()
                            .and_then(|v| serde_json::to_vec(v).ok())
                            .map(|bytes| BASE64_STANDARD.encode(bytes));
                        let exec = rpc_loopback_json(
                            local_addr,
                            &rpc_req.method,
                            &rpc_req.path,
                            b64_body.as_deref(),
                            rpc_req.timeout_ms,
                        )
                        .await;
                        let reply = match exec {
                            Ok(result) => PeerRpcReply {
                                device_id: dev_id,
                                request_id: rpc_req.request_id,
                                ok: true,
                                result: Some(result),
                                error: None,
                            },
                            Err(err) => PeerRpcReply {
                                device_id: dev_id,
                                request_id: rpc_req.request_id,
                                ok: false,
                                result: None,
                                error: Some(err),
                            },
                        };
                        let reply_payload = serde_json::to_value(&reply).unwrap_or(Value::Null);
                        for delay_secs in [0u64, 1, 2, 4] {
                            if delay_secs > 0 {
                                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                            }
                            if post_spoke_to_hub(
                                &d_cli,
                                &p_cli,
                                &pref,
                                &bnd,
                                "octendra_router_reply",
                                "octendra_router.reply",
                                reply_payload.clone(),
                            )
                            .await
                            .is_ok()
                            {
                                break;
                            }
                        }
                    });
                }
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(12);
            }
        }
    }
}

/// Executes a single loopback HTTP JSON RPC against a local `octendra-agent-router` daemon.
///
/// # Errors
///
/// Returns an error string if the bind address is non-loopback, payload decoding fails,
/// or the daemon returns a non-success HTTP status.
pub async fn rpc_loopback_json(
    addr: SocketAddr,
    method: &str,
    path: &str,
    payload_b64: Option<&str>,
    timeout_ms: u64,
) -> Result<Value, String> {
    if !addr.ip().is_loopback() {
        return Err("rpc_loopback_json requires a loopback address".into());
    }
    let parsed_method = Method::from_bytes(method.trim().as_bytes())
        .map_err(|e| format!("invalid HTTP method {method}: {e}"))?;
    let clean_path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let url = format!("http://{addr}{clean_path}");
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(3));
    if timeout_ms > 0 {
        builder = builder.timeout(Duration::from_millis(timeout_ms));
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let mut req = client.request(parsed_method, &url);
    if let Some(b64) = payload_b64.map(str::trim).filter(|s| !s.is_empty()) {
        let decoded = BASE64_STANDARD
            .decode(b64)
            .map_err(|e| format!("invalid base64 payload: {e}"))?;
        let json_val: Value =
            serde_json::from_slice(&decoded).map_err(|e| format!("invalid json payload: {e}"))?;
        req = req.json(&json_val);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("connect loopback router {addr} failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let val: Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| json!({ "status": status.as_u16(), "raw": text }));
    if !status.is_success() {
        let msg = val
            .get("message")
            .or_else(|| val.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or(&text);
        return Err(format!("router HTTP {status}: {msg}"));
    }
    Ok(val)
}

async fn handle_health(State(state): State<AgentRouterState>) -> Json<Value> {
    let (device_name, local_device_id, platform, _) = local_computer_identity();
    let device_id = state
        .inner
        .cloud_upstream
        .as_ref()
        .map(|u| u.local_device_id.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or(local_device_id);
    let local_online = if state.is_cloud_hub() {
        0
    } else {
        discovery::peek_cached_local_apps()
            .into_iter()
            .filter(|a| a.online)
            .count()
    };
    let peers = state.connected_peer_snapshots();
    let remote_online: usize = if state.is_cloud_hub() {
        peers.iter().map(|p| p.apps_online).sum()
    } else {
        state
            .inner
            .upstream_peer_apps
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|(_, list)| list.iter().filter(|a| a.online).count()))
            .unwrap_or(0)
    };
    let apps_online = local_online + remote_online;
    Json(json!({
        "status": "ok",
        "service": "octendra-agent-router",
        "cloud_hub": state.is_cloud_hub(),
        "cloud_upstream": state.inner.cloud_upstream.as_ref().map(|u| u.gateway_url.as_str()),
        "port": DEFAULT_ROUTER_PORT,
        "device_id": device_id,
        "device_name": device_name,
        "platform": platform,
        "local_apps_online": local_online,
        "apps_online": apps_online,
        "connected_peers": peers,
        "timestamp_ms": now_ms(),
    }))
}

async fn handle_list_apps(State(state): State<AgentRouterState>) -> Json<Value> {
    state.maybe_sync_upstream_apps(8_000).await;
    let apps = state.list_enriched_apps();
    Json(json!({
        "status": "ok",
        "apps": apps,
    }))
}

#[derive(Debug, Default, Deserialize)]
struct RouterModelsQuery {
    #[serde(default)]
    refresh: bool,
}

async fn refresh_local_apps_and_peers(
    state: &AgentRouterState,
    refresh_gemini_picker: bool,
    include_peers: bool,
) -> (Vec<RegisteredApp>, Vec<EnrichedRegisteredApp>) {
    let local_apps = if state.is_cloud_hub() {
        Vec::new()
    } else {
        if refresh_gemini_picker
            && AgentRouterState::local_gemini_app_installed()
            && state.refresh_local_macos_accessibility().await
        {
            let _ = state
                .call_gemini_desktop("capabilities", &json!({ "refresh": true }))
                .await;
        }
        discovery::invalidate_discovery_cache();
        state.list_local_only_apps()
    };
    if include_peers {
        let live_peer_ids: Vec<String> = state
            .connected_peer_snapshots()
            .into_iter()
            .map(|p| p.device_id)
            .collect();
        let mut futs = Vec::new();
        for pid in live_peer_ids {
            let st = state.clone();
            futs.push(tokio::spawn(async move {
                let res = st
                    .call_spoke_peer_rpc(
                        &pid,
                        "POST",
                        "/v1/apps/discover",
                        Some(json!({
                            "local_only": true,
                            "refresh_gemini": refresh_gemini_picker
                        })),
                        Duration::from_secs(10),
                    )
                    .await;
                (pid, res)
            }));
        }
        for f in futs {
            if let Ok((pid, Ok(val))) = f.await
                && let Some(arr) = val.get("local_apps")
                && let Ok(parsed) = serde_json::from_value::<Vec<RegisteredApp>>(arr.clone())
            {
                let key = pid.trim().to_ascii_lowercase();
                state.persist_known_peer_apps(&key, &parsed);
                if let Ok(mut guard) = state.inner.peer_spokes.lock()
                    && let Some(spoke) = guard.get_mut(&key)
                {
                    spoke.local_apps = parsed;
                    spoke.last_seen_ms = now_ms();
                }
            }
        }
        if !state.is_cloud_hub() && state.inner.cloud_upstream.is_some() {
            state.sync_upstream_apps(refresh_gemini_picker).await;
        }
    }
    let apps = state.list_enriched_apps();
    (local_apps, apps)
}

async fn handle_discover_apps(
    State(state): State<AgentRouterState>,
    body: axum::body::Bytes,
) -> Json<Value> {
    let parsed_body = serde_json::from_slice::<Value>(&body).ok();
    let local_only = parsed_body
        .as_ref()
        .and_then(|v| v.get("local_only").and_then(Value::as_bool))
        .unwrap_or(false);
    let refresh_gemini = parsed_body
        .as_ref()
        .and_then(|v| v.get("refresh_gemini").and_then(Value::as_bool))
        .unwrap_or(!local_only);
    let (local_apps, apps) =
        refresh_local_apps_and_peers(&state, refresh_gemini, !local_only).await;
    Json(json!({
        "status": "ok",
        "local_apps": local_apps,
        "apps": apps,
    }))
}

async fn handle_list_models(
    State(state): State<AgentRouterState>,
    AxumQuery(q): AxumQuery<RouterModelsQuery>,
) -> Json<Value> {
    let apps = if q.refresh {
        refresh_local_apps_and_peers(&state, true, true).await.1
    } else {
        state.maybe_sync_upstream_apps(8_000).await;
        state.list_enriched_apps()
    };
    let mut models_by_app = serde_json::Map::new();
    for item in &apps {
        models_by_app.insert(
            item.app.app_id.clone(),
            json!({
                "display_name": item.app.display_name,
                "protocol": item.app.protocol,
                "online": item.app.online,
                "runtime_status": item.runtime_status,
                "default_model": item.app.default_model,
                "available_models": item.app.available_models,
                "supported_efforts": item.app.supported_efforts,
            }),
        );
    }
    Json(json!({
        "status": "ok",
        "apps": apps,
        "models_by_app": Value::Object(models_by_app),
    }))
}

#[derive(Debug, Default, Deserialize)]
struct EventsQuery {
    #[serde(default)]
    since_seq: u64,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    wait_ms: u64,
}

async fn handle_poll_events(
    State(state): State<AgentRouterState>,
    AxumQuery(q): AxumQuery<EventsQuery>,
) -> Json<Value> {
    let (latest_seq, events) = state
        .poll_events(q.since_seq, q.turn_id.as_deref(), q.wait_ms)
        .await;
    Json(json!({
        "status": "ok",
        "latest_seq": latest_seq,
        "events": events,
    }))
}

async fn handle_set_app_policy(
    State(state): State<AgentRouterState>,
    AxumPath(app_id): AxumPath<String>,
    Json(req): Json<SetAppPolicyRequest>,
) -> Response {
    let resolved_id = state
        .resolve_app(&app_id)
        .map_or_else(|| app_id.clone(), |a| a.app_id);
    match state.set_app_permission_mode(&resolved_id, &req.permission_mode) {
        Ok(applied) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "app_id": resolved_id,
                "permission_mode": applied,
            })),
        )
            .into_response(),
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "message": msg })),
        )
            .into_response(),
    }
}

async fn handle_list_inputs(State(state): State<AgentRouterState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "inputs": state.list_inputs(),
    }))
}

async fn handle_gemini_readiness(State(state): State<AgentRouterState>) -> Response {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());
    match state.call_gemini_desktop("capabilities", &json!({})).await {
        Ok(mut val) => {
            if let Some(obj) = val.as_object_mut() {
                obj.insert(
                    "router_executable".into(),
                    exe.map_or(Value::Null, Value::String),
                );
            }
            (StatusCode::OK, Json(val)).into_response()
        }
        Err(msg) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "error",
                "message": msg,
                "router_executable": exe,
            })),
        )
            .into_response(),
    }
}

async fn handle_gemini_prompt_accessibility(
    State(state): State<AgentRouterState>,
    body: axum::body::Bytes,
) -> Response {
    let force = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v.get("force").and_then(Value::as_bool))
        .unwrap_or(true);
    let payload = state.prompt_local_macos_accessibility(force).await;
    (StatusCode::OK, Json(payload)).into_response()
}

async fn handle_respond_input(
    State(state): State<AgentRouterState>,
    Json(req): Json<RespondUserInputRequest>,
) -> Response {
    match state.respond_user_input(req).await {
        Ok(snap) => (StatusCode::OK, Json(snap)).into_response(),
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "message": msg })),
        )
            .into_response(),
    }
}

fn is_sensitive_command_text(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    [
        "rm -rf", "sudo ", "git push", "git reset --hard", "mkfs", "dd if=", "shutdown", "reboot",
        "launchctl bootout", "systemctl disable",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
}

fn parse_codex_user_input_request(
    params: &Value,
) -> (
    Option<String>,
    String,
    String,
    Vec<UserInputOptionItem>,
    bool,
) {
    if let Some(first_q) = params
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
    {
        let qid = first_q
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let header = first_q
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("请选择执行方案")
            .to_string();
        let prompt = first_q
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("请选择以下选项或输入补充说明：")
            .to_string();
        let mut opts = Vec::new();
        if let Some(arr) = first_q.get("options").and_then(Value::as_array) {
            for (idx, item) in arr.iter().enumerate() {
                let label = item
                    .get("label")
                    .or_else(|| item.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if !label.is_empty() {
                    opts.push(UserInputOptionItem {
                        option_id: item
                            .get("id")
                            .and_then(Value::as_str)
                            .map_or_else(|| format!("opt_{}", idx + 1), str::to_string),
                        label: label.to_string(),
                        description: item
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
            }
        }
        let multi = first_q
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return (qid, header, prompt, opts, multi);
    }

    let msg = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("应用需要你的补充输入或选项确认：");
    (None, "需要你的确认或选择".into(), msg.to_string(), Vec::new(), false)
}

fn extract_structured_choice_options(reply_text: &str) -> Vec<UserInputOptionItem> {
    let trimmed = reply_text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let tail_lines: Vec<&str> = trimmed.lines().rev().take(18).collect();
    let asks_choice = tail_lines.iter().take(6).any(|l| {
        l.contains('？')
            || l.contains('?')
            || l.contains("请选择")
            || l.contains("你想")
            || l.contains("哪一")
            || l.contains("哪个")
    });
    if !asks_choice {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for raw_line in tail_lines.into_iter().rev() {
        let line = raw_line.trim();
        let stripped = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .unwrap_or(line);
        let maybe_item = ["1. ", "2. ", "3. ", "4. ", "A. ", "B. ", "C. ", "D. ", "A：", "B：", "C："]
            .iter()
            .find_map(|p| stripped.strip_prefix(p));
        if let Some(rest) = maybe_item {
            let clean = rest.trim().trim_matches('*').trim();
            let short_label: String = clean
                .split(['：', ':', '（', '('])
                .next()
                .unwrap_or(clean)
                .trim()
                .chars()
                .take(36)
                .collect();
            if short_label.chars().count() >= 2 {
                candidates.push(UserInputOptionItem {
                    option_id: format!("choice_{}", candidates.len() + 1),
                    label: short_label,
                    description: None,
                });
            }
        }
    }
    if (2..=5).contains(&candidates.len()) {
        candidates
    } else {
        Vec::new()
    }
}

async fn handle_upsert_app(
    State(state): State<AgentRouterState>,
    Json(req): Json<UpsertCustomAppRequest>,
) -> Response {
    let req_clone = req.clone();
    match state.upsert_custom_app(req) {
        Ok(app) => {
            if !app.is_local && state.is_peer_spoke_online(&app.device_id) {
                let mut remote_req = serde_json::to_value(&app).unwrap_or(Value::Null);
                if let Value::Object(ref mut m) = remote_req {
                    m.insert("device".into(), json!(app.device_id));
                    if let Some(ref ex) = req_clone.executable {
                        m.insert("executable".into(), json!(ex));
                    }
                }
                let _ = state
                    .call_spoke_peer_rpc(
                        &app.device_id,
                        "POST",
                        "/v1/apps",
                        Some(remote_req),
                        Duration::from_secs(15),
                    )
                    .await;
            }
            (StatusCode::OK, Json(json!({ "status": "ok", "app": app }))).into_response()
        }
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "message": msg })),
        )
            .into_response(),
    }
}

async fn handle_delete_app(
    State(state): State<AgentRouterState>,
    AxumPath(app_id): AxumPath<String>,
) -> Response {
    let maybe_target = state.resolve_app(&app_id);
    match state.delete_custom_app(&app_id) {
        Ok(removed) => {
            if let Some(app) = maybe_target
                && !app.is_local
                && state.is_peer_spoke_online(&app.device_id)
            {
                let _ = state
                    .call_spoke_peer_rpc(
                        &app.device_id,
                        "DELETE",
                        &format!("/v1/apps/{app_id}"),
                        None,
                        Duration::from_secs(15),
                    )
                    .await;
            }
            (
                StatusCode::OK,
                Json(json!({ "status": "ok", "app_id": app_id, "removed": removed })),
            )
                .into_response()
        }
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "message": msg })),
        )
            .into_response(),
    }
}

async fn handle_list_sessions(State(state): State<AgentRouterState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "sessions": state.list_sessions(),
    }))
}

async fn handle_reset_session(
    State(state): State<AgentRouterState>,
    Json(req): Json<ResetSessionRequest>,
) -> Json<Value> {
    let cleared = state.reset_session(&req.caller_session_id, req.app_id.as_deref());
    Json(json!({
        "status": "ok",
        "caller_session_id": req.caller_session_id,
        "cleared": cleared,
    }))
}

async fn handle_session_turn(
    State(state): State<AgentRouterState>,
    Json(req): Json<TurnRequest>,
) -> Response {
    match state.execute_turn(req).await {
        Ok(snap) => (StatusCode::OK, Json(snap)).into_response(),
        Err(msg) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "status": "failed", "reason": msg, "message": msg })),
        )
            .into_response(),
    }
}

async fn handle_get_turn(
    State(state): State<AgentRouterState>,
    AxumPath(turn_id): AxumPath<String>,
) -> Response {
    match state.get_turn(&turn_id) {
        Some(snap) => (StatusCode::OK, Json(snap)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "status": "not_found", "turn_id": turn_id })),
        )
            .into_response(),
    }
}

async fn handle_list_permissions(State(state): State<AgentRouterState>) -> Json<Value> {
    Json(json!({
        "status": "ok",
        "permissions": state.list_permissions(),
    }))
}

async fn handle_respond_permission(
    State(state): State<AgentRouterState>,
    Json(req): Json<RespondPermissionRequest>,
) -> Response {
    match state.respond_permission(req).await {
        Ok(snap) => (StatusCode::OK, Json(snap)).into_response(),
        Err(msg) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "message": msg })),
        )
            .into_response(),
    }
}

async fn handle_peer_poll(
    State(state): State<AgentRouterState>,
    headers: HeaderMap,
    Json(reg): Json<PeerPollRegistration>,
) -> Response {
    let key = headers
        .get(PEER_KEY_HEADER)
        .and_then(|v| v.to_str().ok());
    if !state.verify_hub_peer_key(key) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "status": "unauthorized", "message": "invalid peer router key" })),
        )
            .into_response();
    }
    let req = state.poll_spoke_next(reg).await;
    (StatusCode::OK, Json(json!({ "status": "ok", "request": req }))).into_response()
}

async fn handle_peer_reply(
    State(state): State<AgentRouterState>,
    headers: HeaderMap,
    Json(reply): Json<PeerRpcReply>,
) -> Response {
    let key = headers
        .get(PEER_KEY_HEADER)
        .and_then(|v| v.to_str().ok());
    if !state.verify_hub_peer_key(key) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "status": "unauthorized", "message": "invalid peer router key" })),
        )
            .into_response();
    }
    state.resolve_spoke_reply(reply);
    (StatusCode::OK, Json(json!({ "status": "ok", "ok": true }))).into_response()
}

async fn handle_ui_devices(State(state): State<AgentRouterState>) -> Response {
    let (local_id, local_name, platform) = state.effective_local_identity();
    let local_status = match state.inner.host_ui {
        Some(ref bridge) => Some(bridge.local_ui_status().await),
        None => None,
    };
    let mut remote_devices: Option<Value> = None;
    let mut remote_workspaces: Option<Value> = None;
    let mut remote_error: Option<String> = None;
    if !state.is_cloud_hub() && state.inner.cloud_upstream.is_some() {
        match state
            .call_remote_peer_router("", "GET", "/v1/ui/devices", None, Duration::from_secs(10))
            .await
        {
            Ok(val) => {
                remote_devices = val.get("devices").cloned();
                remote_workspaces = val.get("ui_workspaces").cloned();
            }
            Err(err) => {
                remote_error = Some(err);
            }
        }
    }
    let devices = remote_devices.unwrap_or_else(|| {
        json!([{
            "device_id": local_id,
            "name": local_name,
            "platform": platform,
            "online": true,
            "is_local": true,
        }])
    });
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "local_device": {
                "device_id": local_id,
                "device_name": local_name,
                "platform": platform,
                "bridge_attached": local_status.is_some(),
                "ui_status": local_status,
            },
            "devices": devices,
            "ui_workspaces": remote_workspaces.unwrap_or_else(|| json!([])),
            "remote_error": remote_error,
        })),
    )
        .into_response()
}

async fn handle_ui_bind_window(
    State(state): State<AgentRouterState>,
    Json(req): Json<Value>,
) -> Response {
    let dev = req.get("device").and_then(Value::as_str);
    if state.is_local_ui_target(dev) {
        let Some(ref bridge) = state.inner.host_ui else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "error", "message": "local host UI bridge unavailable" })),
            )
                .into_response();
        };
        return match bridge.bind_local_window(req).await {
            Ok(val) => (StatusCode::OK, Json(json!({ "status": "ok", "binding": val }))).into_response(),
            Err(msg) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "message": msg })),
            )
                .into_response(),
        };
    }
    let target = dev.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("");
    if target.is_empty() || target.eq_ignore_ascii_case("local") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "error", "message": "local host UI bridge unavailable" })),
        )
            .into_response();
    }
    match state
        .call_remote_peer_router(
            target,
            "POST",
            "/v1/ui/bind_window",
            Some(&req),
            Duration::from_secs(20),
        )
        .await
    {
        Ok(val) => (StatusCode::OK, Json(val)).into_response(),
        Err(msg) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "status": "error", "message": msg })),
        )
            .into_response(),
    }
}

async fn handle_ui_tool(
    state: AgentRouterState,
    tool: &str,
    path: &str,
    mut req: Value,
) -> Response {
    let dev = req.get("device").and_then(Value::as_str).map(str::to_owned);
    if state.is_local_ui_target(dev.as_deref()) {
        let (local_id, _, _) = state.effective_local_identity();
        let Some(ref bridge) = state.inner.host_ui else {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "error", "message": "local host UI bridge unavailable" })),
            )
                .into_response();
        };
        if let Some(obj) = req.as_object_mut() {
            obj.insert("device".into(), json!(local_id));
            if tool == "ui_act" {
                obj.remove("context");
            } else if obj.get("context").is_none() && obj.get("observation_id").is_none() {
                obj.insert("context".into(), json!("host"));
            }
        }
        return match bridge.execute_local_ui(tool.to_string(), req).await {
            Some(Ok(val)) => (StatusCode::OK, Json(val)).into_response(),
            Some(Err(msg)) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "status": "error", "message": msg })),
            )
                .into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "status": "error", "message": "local host agent is still initializing" })),
            )
                .into_response(),
        };
    }
    let target = dev
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    if target.is_empty() || target.eq_ignore_ascii_case("local") {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "error", "message": "local host UI bridge unavailable" })),
        )
            .into_response();
    }
    match state
        .call_remote_peer_router(target, "POST", path, Some(&req), Duration::from_secs(45))
        .await
    {
        Ok(val) => (StatusCode::OK, Json(val)).into_response(),
        Err(msg) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "status": "error", "message": msg })),
        )
            .into_response(),
    }
}

async fn handle_ui_observe(
    State(state): State<AgentRouterState>,
    Json(req): Json<Value>,
) -> Response {
    handle_ui_tool(state, "ui_observe", "/v1/ui/observe", req).await
}

async fn handle_ui_act(
    State(state): State<AgentRouterState>,
    Json(req): Json<Value>,
) -> Response {
    handle_ui_tool(state, "ui_act", "/v1/ui/act", req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_dynamic_local_and_custom_apps_without_ssh_hosts() {
        let dir = std::env::temp_dir().join(format!("octendra-router-test-{}", Uuid::new_v4()));
        let state = AgentRouterState::with_storage_dir(dir.clone());
        let apps = state.list_apps();
        assert!(!apps.is_empty());
        assert!(apps.iter().all(|a| !a.device_name.contains("ssh")));

        let created = state.upsert_custom_app(UpsertCustomAppRequest {
            app_id: None,
            name: "ResearchBot".into(),
            display_name: None,
            protocol: "antigravity_desktop".into(),
            device: Some("HO5".into()),
            default_cwd: Some("/var/home/liang/workspace".into()),
            executable: None,
            summary: Some("HO5 自定义研究助手".into()),
            capabilities: vec!["文献整理".into()],
            slash_commands: vec![],
            internal_tools: vec![],
        });
        assert!(created.is_ok());
        if let Ok(app) = created {
            assert_eq!(app.display_name, "ResearchBot · HO5");
            assert_eq!(state.delete_custom_app(&app.app_id), Ok(true));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn gemini_pre_submit_error_classifiers_and_prior_op_policy() {
        assert!(AgentRouterState::is_gemini_model_selection_failure(
            "not_submitted:model_option_not_found"
        ));
        assert!(AgentRouterState::is_gemini_model_selection_failure(
            "argon_limit_reached"
        ));
        assert!(AgentRouterState::is_gemini_model_selection_failure(
            "post_effort_context_not_verified"
        ));
        assert!(!AgentRouterState::is_gemini_model_selection_failure(
            "busy:generation_in_progress"
        ));

        assert!(AgentRouterState::is_gemini_pre_submit_race(
            "not_submitted:composer_not_found"
        ));
        assert!(AgentRouterState::is_gemini_pre_submit_race(
            "focus_verification_failed"
        ));
        assert!(AgentRouterState::is_gemini_window_busy_reason(
            "not_submitted:busy:generation_in_progress"
        ));
        assert!(AgentRouterState::is_gemini_window_busy_reason(
            "active_binding_exists"
        ));
        assert!(!AgentRouterState::is_gemini_window_busy_reason(
            "not_submitted:accessibility_untrusted"
        ));
        assert!(!AgentRouterState::is_gemini_window_busy_reason(
            "not_submitted:app_accessibility_unavailable"
        ));

        let dir = std::env::temp_dir().join(format!("octendra-ax-state-{}", Uuid::new_v4()));
        let state = AgentRouterState::with_storage_dir(dir.clone());
        state.record_local_accessibility_trusted(false);
        for item in state.list_enriched_apps() {
            if item.app.is_local && item.app.protocol == "gemini_desktop" && item.app.online {
                assert_eq!(item.runtime_status, "needs_accessibility");
            }
        }
        let _ = std::fs::remove_dir_all(dir);

        // Do NOT wipe conversation prior_op on model or composer failures
        assert!(!AgentRouterState::should_clear_prior_op_on_not_submitted(
            "model_option_not_found"
        ));
        assert!(!AgentRouterState::should_clear_prior_op_on_not_submitted(
            "composer_not_found"
        ));
        assert!(AgentRouterState::should_clear_prior_op_on_not_submitted(
            "prior_operation_not_found"
        ));
    }

    #[test]
    fn gemini_stream_delta_handles_utf8_multibyte_safely() {
        assert_eq!(
            AgentRouterState::extract_stream_text_delta("", "你好"),
            Some("你好")
        );
        assert_eq!(
            AgentRouterState::extract_stream_text_delta("你好", "你好，世界"),
            Some("，世界")
        );
        assert_eq!(
            AgentRouterState::extract_stream_text_delta("你好", "你好"),
            None
        );
        assert_eq!(
            AgentRouterState::extract_stream_text_delta("你好世界", "你好"),
            None
        );
        assert_eq!(
            AgentRouterState::extract_stream_text_delta("你好", "全新回复内容"),
            Some("全新回复内容")
        );
    }

    #[test]
    fn resolve_effective_turn_cwd_infers_unique_project_and_inherits_prior_session() {
        let root = std::env::temp_dir().join(format!("octendra-cwd-infer-{}", Uuid::new_v4()));
        let alpha = root.join("alpha-proj");
        let beta = root.join("beta-service");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();

        let app = RegisteredApp {
            app_id: "codex__mac-studio".into(),
            name: "Codex".into(),
            display_name: "Codex · Mac-Studio".into(),
            protocol: "codex_app_server".into(),
            category: "connected_app".into(),
            device_id: "dev".into(),
            device_name: "Mac-Studio".into(),
            platform: "macos".into(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: root.to_string_lossy().into_owned(),
            executable: None,
            summary: String::new(),
            capabilities: Vec::new(),
            aliases: Vec::new(),
            slash_commands: Vec::new(),
            internal_tools: Vec::new(),
            available_models: Vec::new(),
            default_model: None,
            supported_efforts: Vec::new(),
        };
        let base_req = TurnRequest {
            caller_session_id: "s1".into(),
            app_id: "codex__mac-studio".into(),
            prompt: String::new(),
            cwd: None,
            model: None,
            effort: None,
            timeout_ms: None,
            turn_id: None,
            wait: None,
            wait_for_idle_ms: None,
            interactive_permissions: None,
            permission_mode: None,
            disable_peer_failover: None,
        };

        // First turn mentioning one unique repo routes into that project directory
        let inferred = AgentRouterState::resolve_effective_turn_cwd(
            &app,
            &base_req,
            "帮我检查 alpha-proj 的编译问题",
            None,
        );
        assert_eq!(inferred, alpha.to_string_lossy().into_owned());

        // Ambiguous prompt mentioning multiple repos stays at root
        let ambiguous = AgentRouterState::resolve_effective_turn_cwd(
            &app,
            &base_req,
            "对比 alpha-proj 和 beta-service",
            None,
        );
        assert_eq!(ambiguous, root.to_string_lossy().into_owned());

        // Second turn with short follow-up inherits prior session's project cwd
        let prior = RouterSessionRecord {
            caller_session_id: "s1".into(),
            app_id: "codex__mac-studio".into(),
            native_thread_id: Some("thr-1".into()),
            last_operation_id: None,
            turn_count: 1,
            model: None,
            effort: None,
            cwd: Some(beta.to_string_lossy().into_owned()),
            history: Vec::new(),
            updated_at_ms: 1,
        };
        let inherited =
            AgentRouterState::resolve_effective_turn_cwd(&app, &base_req, "继续", Some(&prior));
        assert_eq!(inherited, beta.to_string_lossy().into_owned());

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cloud_hub_aggregates_spokes_and_fails_without_fallback() {
        let dir = std::env::temp_dir().join(format!("octendra-cloud-hub-test-{}", Uuid::new_v4()));
        let hub = AgentRouterState::new_cloud_hub(dir.clone());
        assert!(hub.is_cloud_hub());
        assert!(hub.list_apps().is_empty());

        let remote_app = RegisteredApp {
            app_id: "gemini_macos__remote-mac".into(),
            name: "Gemini".into(),
            display_name: "Gemini · RemoteMac".into(),
            protocol: "gemini_desktop".into(),
            category: "connected_app".into(),
            device_id: "remote-mac-id".into(),
            device_name: "RemoteMac".into(),
            platform: "macos".into(),
            is_local: true,
            online: true,
            is_custom: false,
            default_cwd: "/Users/test/workspace".into(),
            executable: Some("builtin:gemini_desktop".into()),
            summary: String::new(),
            capabilities: Vec::new(),
            aliases: Vec::new(),
            slash_commands: Vec::new(),
            internal_tools: Vec::new(),
            available_models: Vec::new(),
            default_model: None,
            supported_efforts: Vec::new(),
        };

        let next_req = hub
            .poll_authenticated_spoke(
                "remote-mac-id",
                "RemoteMac",
                PeerPollRegistration {
                    device_id: "spoofed-id".into(),
                    device_name: "SpoofedName".into(),
                    platform: "macos".into(),
                    default_cwd: "/Users/test/workspace".into(),
                    local_apps: vec![remote_app],
                    spoke_events: Vec::new(),
                    wait_ms: 500,
                },
            )
            .await;
        assert!(next_req.is_none());

        let apps = hub.list_apps();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].app_id, "gemini_macos__remote-mac");
        assert!(!apps[0].is_local);

        let offline_err = hub
            .execute_turn(TurnRequest {
                caller_session_id: "s-offline".into(),
                app_id: "nonexistent__offline".into(),
                prompt: "hello".into(),
                cwd: None,
                model: None,
                effort: None,
                timeout_ms: None,
                turn_id: None,
                wait: Some(true),
                wait_for_idle_ms: None,
                interactive_permissions: None,
                permission_mode: None,
                disable_peer_failover: Some(true),
            })
            .await;
        assert!(offline_err.is_err());

        let _ = std::fs::remove_dir_all(dir);
    }
}
