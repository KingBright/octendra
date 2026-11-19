//! Transparent Octendra Code Gateway bridge for `octendra-mcp`.
//!
//! Enables `octendra_code_execute` (and external-network device operations) to
//! automatically discover the configured Code Gateway URL, authenticate via
//! cached OAuth tokens / refresh rotation / owner password PKCE bootstrap, and
//! route remote device tools (`devices_list`, `fleet_status`, `<device>:/path`,
//! explicit `device_id`) without requiring a separate IDE MCP mount.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const DEFAULT_OAUTH_REDIRECT_URI: &str =
    "https://chatgpt.com/connector_platform_oauth_redirect";
const TOKEN_REFRESH_LEAD_SECONDS: u64 = 60;

/// Persistent OAuth token cache stored at `~/.config/octendra/code-gateway-auth.json` (mode 0600).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct GatewayAuthCache {
    pub gateway_url: String,
    pub client_id: String,
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at_unix: u64,
}

#[derive(Debug, Deserialize)]
struct LocalAgentConfigHint {
    #[serde(default)]
    gateway_url: Option<String>,
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn default_auth_cache_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("OCTENDRA_CODE_AUTH_CACHE") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    home_dir().map(|home| home.join(".config/octendra/code-gateway-auth.json"))
}

fn default_password_file_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("OCTENDRA_CODE_PASSWORD_FILE") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    home_dir().map(|home| home.join(".config/octendra/gateway-owner-password"))
}

fn default_local_agent_config_path() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".local/share/octendra/agent/config/agent.json"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn non_empty_trimmed(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Returns true when `workspace_id` uses the remote `"<device>:/abs/path"` shorthand
/// (excluding `:default`, empty prefixes, and Windows drive letters like `C:\foo`).
pub(crate) fn workspace_id_targets_remote_device(workspace_id: &str) -> bool {
    let trimmed = workspace_id.trim();
    let Some((prefix, rest)) = trimmed.split_once(':') else {
        return false;
    };
    if prefix.is_empty() || rest.is_empty() {
        return false;
    }
    // Exclude Windows drive letter paths like `C:\...` or `C:/...`.
    if prefix.len() == 1
        && prefix.as_bytes()[0].is_ascii_alphabetic()
        && (rest.starts_with('\\') || rest.starts_with('/'))
    {
        return false;
    }
    // Exclude URLs like `https://...` or `file://...`.
    if rest.starts_with("//") {
        return false;
    }
    true
}

/// Determines whether a `octendra_code_execute` request should route through the
/// external Code Gateway rather than the embedded local-only agent.
pub(crate) fn should_route_to_gateway(
    tool: &str,
    explicit_gateway_url: Option<&str>,
    env_gateway_url: Option<&str>,
    device_id: Option<&str>,
    arguments: &Value,
) -> bool {
    if non_empty_trimmed(explicit_gateway_url).is_some()
        || non_empty_trimmed(env_gateway_url).is_some()
    {
        return true;
    }
    if matches!(
        tool,
        "devices_list"
            | "fleet_status"
            | "instance_upgrade"
            | "admin_status"
            | "operation_get"
            | "task_context"
            | "maintenance_lease"
            | "maintenance_status"
    ) {
        return true;
    }
    if non_empty_trimmed(device_id).is_some() {
        return true;
    }
    if arguments
        .get("device_id")
        .and_then(Value::as_str)
        .and_then(|s| non_empty_trimmed(Some(s)))
        .is_some()
    {
        return true;
    }
    if arguments
        .get("workspace_id")
        .and_then(Value::as_str)
        .is_some_and(workspace_id_targets_remote_device)
    {
        return true;
    }
    false
}

/// Discovers the Gateway base URL in priority order:
/// explicit arg -> `OCTENDRA_CODE_GATEWAY_URL` -> auth cache -> local `agent.json`.
pub(crate) fn discover_gateway_url(
    explicit_gateway_url: Option<&str>,
    env_gateway_url: Option<&str>,
    auth_cache_path: Option<&Path>,
    agent_config_path: Option<&Path>,
) -> Option<String> {
    if let Some(url) = non_empty_trimmed(explicit_gateway_url) {
        return Some(url.trim_end_matches('/').to_owned());
    }
    if let Some(url) = non_empty_trimmed(env_gateway_url) {
        return Some(url.trim_end_matches('/').to_owned());
    }
    if let Some(path) = auth_cache_path
        && let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(cache) = serde_json::from_str::<GatewayAuthCache>(&raw)
        && let Some(url) = non_empty_trimmed(Some(&cache.gateway_url))
    {
        return Some(url.trim_end_matches('/').to_owned());
    }
    if let Some(path) = agent_config_path
        && let Ok(raw) = std::fs::read_to_string(path)
        && let Ok(cfg) = serde_json::from_str::<LocalAgentConfigHint>(&raw)
        && let Some(url) = non_empty_trimmed(cfg.gateway_url.as_deref())
    {
        return Some(url.trim_end_matches('/').to_owned());
    }
    None
}

/// Normalizes remote `device_id` and `root` parameters into Gateway-compatible
/// `arguments` (synthesizing `workspace_id = "<device>:<root>"` when helpful).
pub(crate) fn normalize_remote_arguments(
    tool: &str,
    device_id: Option<&str>,
    root: Option<&str>,
    arguments: &mut Value,
) {
    let device = non_empty_trimmed(device_id);
    let root_dir = non_empty_trimmed(root);
    if tool == "workspace_open" {
        if let Some(dev) = device
            && arguments.get("device_id").is_none()
        {
            arguments["device_id"] = Value::String(dev);
        }
        if let Some(r) = root_dir
            && arguments.get("root").is_none()
        {
            arguments["root"] = Value::String(r);
        }
        return;
    }
    if arguments.get("workspace_id").is_none()
        && let (Some(dev), Some(r)) = (device.as_deref(), root_dir.as_deref())
    {
        arguments["workspace_id"] = Value::String(format!("{dev}:{r}"));
    }
    if let Some(dev) = device
        && arguments.get("device_id").is_none()
    {
        arguments["device_id"] = Value::String(dev);
    }
}

fn read_auth_cache(path: &Path) -> Option<GatewayAuthCache> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_auth_cache(path: &Path, cache: &GatewayAuthCache) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create auth cache directory: {err}"))?;
    }
    let serialized = serde_json::to_string_pretty(cache)
        .map_err(|err| format!("failed to serialize gateway auth cache: {err}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{serialized}\n"))
        .map_err(|err| format!("failed to write temporary gateway auth cache: {err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)
        .map_err(|err| format!("failed to atomically replace gateway auth cache: {err}"))?;
    Ok(())
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    fn encode_component(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        for b in input.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(char::from(b));
                }
                b' ' => out.push('+'),
                other => {
                    use std::fmt::Write as _;
                    let _ = write!(out, "%{other:02X}");
                }
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn extract_query_param(url: &str, key: &str) -> Option<String> {
    let (_, query) = url.split_once('?')?;
    for part in query.split('&') {
        let (k, v) = part.split_once('=')?;
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3])
                && let Ok(val) = u8::from_str_radix(hex_str, 16)
            {
                out.push(val);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            out.push(b' ');
            i += 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn refresh_oauth_token(
    client: &reqwest::Client,
    gateway_url: &str,
    cache: &GatewayAuthCache,
) -> Result<GatewayAuthCache, String> {
    let refresh_token = cache
        .refresh_token
        .as_deref()
        .ok_or_else(|| "cached gateway token has no refresh_token".to_owned())?;
    let resource = format!("{gateway_url}/mcp");
    let body = form_encode(&[
        ("grant_type", "refresh_token"),
        ("client_id", &cache.client_id),
        ("refresh_token", refresh_token),
        ("resource", &resource),
    ]);
    let resp = client
        .post(format!("{gateway_url}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|err| format!("gateway token refresh failed: {err}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|err| format!("gateway token refresh read failed: {err}"))?;
    if !status.is_success() {
        return Err(format!("gateway token refresh HTTP {status}: {text}"));
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|err| format!("gateway token refresh decode failed: {err}"))?;
    let access_token = parsed
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "gateway token refresh missing access_token".to_owned())?
        .to_owned();
    let new_refresh = parsed
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| cache.refresh_token.clone());
    let expires_in = parsed
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    Ok(GatewayAuthCache {
        gateway_url: gateway_url.to_owned(),
        client_id: cache.client_id.clone(),
        access_token,
        refresh_token: new_refresh,
        expires_at_unix: now_unix().saturating_add(expires_in),
    })
}

async fn bootstrap_oauth_with_password(
    gateway_url: &str,
    password: &str,
) -> Result<GatewayAuthCache, String> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| format!("failed to build OAuth HTTP client: {err}"))?;
    let resource = format!("{gateway_url}/mcp");

    let reg_resp = client
        .post(format!("{gateway_url}/oauth/register"))
        .header("Accept", "application/json")
        .json(&json!({
            "redirect_uris": [DEFAULT_OAUTH_REDIRECT_URI],
            "token_endpoint_auth_method": "none"
        }))
        .send()
        .await
        .map_err(|err| format!("gateway OAuth register failed: {err}"))?;
    if !reg_resp.status().is_success() {
        return Err(format!(
            "gateway OAuth register HTTP {}",
            reg_resp.status()
        ));
    }
    let reg_json: Value = reg_resp
        .json()
        .await
        .map_err(|err| format!("gateway OAuth register decode failed: {err}"))?;
    let client_id = reg_json
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "gateway OAuth register missing client_id".to_owned())?
        .to_owned();

    let verifier = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    let state = Uuid::new_v4().simple().to_string();

    let auth_query = form_encode(&[
        ("client_id", &client_id),
        ("redirect_uri", DEFAULT_OAUTH_REDIRECT_URI),
        ("response_type", "code"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("resource", &resource),
    ]);
    let auth_resp = client
        .get(format!("{gateway_url}/oauth/authorize?{auth_query}"))
        .send()
        .await
        .map_err(|err| format!("gateway OAuth authorize failed: {err}"))?;
    if !auth_resp.status().is_success() {
        return Err(format!(
            "gateway OAuth authorize HTTP {}",
            auth_resp.status()
        ));
    }
    let cookie_header = auth_resp
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "gateway OAuth authorize missing Set-Cookie nonce".to_owned())?
        .to_owned();
    let cookie_pair = cookie_header
        .split(';')
        .next()
        .unwrap_or(&cookie_header)
        .trim();
    let nonce = cookie_pair
        .split_once('=')
        .map(|(_, val)| val.trim())
        .ok_or_else(|| "gateway OAuth authorize invalid nonce cookie".to_owned())?;

    let approve_body = form_encode(&[("nonce", nonce), ("password", password.trim())]);
    let approve_resp = client
        .post(format!("{gateway_url}/oauth/approve"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header(reqwest::header::COOKIE, cookie_pair)
        .body(approve_body)
        .send()
        .await
        .map_err(|err| format!("gateway OAuth approve failed: {err}"))?;
    if approve_resp.status().as_u16() != 303 {
        return Err(format!(
            "gateway OAuth owner approval failed (HTTP {})",
            approve_resp.status()
        ));
    }
    let location = approve_resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "gateway OAuth approve missing Location header".to_owned())?;
    let returned_state = extract_query_param(location, "state");
    if returned_state.as_deref() != Some(state.as_str()) {
        return Err("gateway OAuth state mismatch".to_owned());
    }
    let code = extract_query_param(location, "code")
        .ok_or_else(|| "gateway OAuth approve missing authorization code".to_owned())?;

    let token_body = form_encode(&[
        ("grant_type", "authorization_code"),
        ("client_id", &client_id),
        ("code", &code),
        ("code_verifier", &verifier),
        ("redirect_uri", DEFAULT_OAUTH_REDIRECT_URI),
        ("resource", &resource),
    ]);
    let token_resp = client
        .post(format!("{gateway_url}/oauth/token"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .body(token_body)
        .send()
        .await
        .map_err(|err| format!("gateway OAuth token exchange failed: {err}"))?;
    if !token_resp.status().is_success() {
        return Err(format!(
            "gateway OAuth token exchange HTTP {}",
            token_resp.status()
        ));
    }
    let tokens: Value = token_resp
        .json()
        .await
        .map_err(|err| format!("gateway OAuth token decode failed: {err}"))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| "gateway OAuth token response missing access_token".to_owned())?
        .to_owned();
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let expires_in = tokens
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    Ok(GatewayAuthCache {
        gateway_url: gateway_url.to_owned(),
        client_id,
        access_token,
        refresh_token,
        expires_at_unix: now_unix().saturating_add(expires_in),
    })
}

async fn resolve_bearer_token(
    client: &reqwest::Client,
    gateway_url: &str,
    explicit_bearer: Option<&str>,
    auth_cache_path: Option<&Path>,
    password_file_path: Option<&Path>,
    force_refresh: bool,
) -> Result<Option<String>, String> {
    if !force_refresh {
        if let Some(tok) = non_empty_trimmed(explicit_bearer) {
            return Ok(Some(tok));
        }
        if let Ok(env_tok) = std::env::var("OCTENDRA_CODE_BEARER_TOKEN")
            && let Some(tok) = non_empty_trimmed(Some(&env_tok))
        {
            return Ok(Some(tok));
        }
    }

    if let Some(cache_path) = auth_cache_path
        && let Some(cached) = read_auth_cache(cache_path)
        && cached.gateway_url.trim_end_matches('/') == gateway_url.trim_end_matches('/')
    {
        let still_fresh = !force_refresh
            && cached.expires_at_unix > now_unix().saturating_add(TOKEN_REFRESH_LEAD_SECONDS);
        if still_fresh {
            return Ok(Some(cached.access_token));
        }
        if cached.refresh_token.is_some()
            && let Ok(refreshed) = refresh_oauth_token(client, gateway_url, &cached).await
        {
            let _ = write_auth_cache(cache_path, &refreshed);
            return Ok(Some(refreshed.access_token));
        }
    }

    if let Some(pw_path) = password_file_path
        && pw_path.is_file()
    {
        let password = std::fs::read_to_string(pw_path)
            .map_err(|err| format!("failed to read gateway owner password file: {err}"))?;
        let trimmed = password.trim();
        if !trimmed.is_empty() {
            let fresh = bootstrap_oauth_with_password(gateway_url, trimmed).await?;
            if let Some(cache_path) = auth_cache_path {
                let _ = write_auth_cache(cache_path, &fresh);
            }
            return Ok(Some(fresh.access_token));
        }
    }

    Ok(None)
}

/// Calls a tool on the remote Octendra Code Gateway via `/mcp`, with automatic
/// Gateway URL discovery, OAuth token caching/refresh, and 401 recovery.
pub(crate) async fn call_gateway_tool(
    gateway_url: &str,
    explicit_bearer: Option<&str>,
    tool: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let auth_cache_path = default_auth_cache_path();
    let password_file_path = default_password_file_path();
    call_gateway_tool_with_paths(
        gateway_url,
        explicit_bearer,
        tool,
        arguments,
        auth_cache_path.as_deref(),
        password_file_path.as_deref(),
    )
    .await
}

pub(crate) async fn call_gateway_tool_with_paths(
    gateway_url: &str,
    explicit_bearer: Option<&str>,
    tool: &str,
    arguments: &Value,
    auth_cache_path: Option<&Path>,
    password_file_path: Option<&Path>,
) -> Result<Value, String> {
    let base = gateway_url.trim_end_matches('/');
    let endpoint = format!("{base}/mcp");
    let client = reqwest::Client::new();

    for attempt in 0..2 {
        let force_refresh = attempt == 1;
        let bearer = resolve_bearer_token(
            &client,
            base,
            explicit_bearer,
            auth_cache_path,
            password_file_path,
            force_refresh,
        )
        .await?;

        let mut http_req = client
            .post(&endpoint)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": Uuid::new_v4().to_string(),
                "method": "tools/call",
                "params": {
                    "name": tool,
                    "arguments": arguments,
                }
            }));
        if let Some(token) = bearer.as_deref() {
            http_req = http_req.bearer_auth(token);
        }

        let response = http_req
            .send()
            .await
            .map_err(|error| format!("gateway request failed: {error}"))?;
        let status = response.status();
        let raw_text = response
            .text()
            .await
            .map_err(|error| format!("gateway response read failed ({status}): {error}"))?;

        if status.as_u16() == 401 && attempt == 0 {
            // Only retry once on 401 (auth rejected before execution) to refresh/re-bootstrap token.
            continue;
        }
        if !status.is_success() {
            if status.as_u16() == 401 {
                return Err(format!(
                    "gateway HTTP 401 (unauthorized) for {endpoint}: configure ~/.config/octendra/gateway-owner-password (0600) or OCTENDRA_CODE_BEARER_TOKEN. Detail: {raw_text}"
                ));
            }
            return Err(format!("gateway HTTP {status}: {raw_text}"));
        }

        let payload_str = raw_text
            .lines()
            .find_map(|line| line.strip_prefix("data:").map(str::trim))
            .filter(|data| !data.is_empty())
            .unwrap_or(raw_text.trim());
        let body: Value = serde_json::from_str(payload_str)
            .map_err(|error| format!("gateway response decode failed ({status}): {error}"))?;
        if let Some(rpc_err) = body.get("error") {
            return Err(format!("gateway rpc error: {rpc_err}"));
        }
        if let Some(result) = body.get("result") {
            if result.get("isError") == Some(&Value::Bool(true)) {
                return Err(format!("gateway tool error: {result}"));
            }
            if let Some(structured) = result.get("structuredContent") {
                return Ok(structured.clone());
            }
        }
        return Ok(body);
    }

    Err("gateway request exhausted authentication retries".to_owned())
}

/// Convenience helper for callers that only need default system paths.
pub(crate) fn discover_default_gateway_url(
    explicit_gateway_url: Option<&str>,
) -> Option<String> {
    let env_url = std::env::var("OCTENDRA_CODE_GATEWAY_URL").ok();
    let auth_cache = default_auth_cache_path();
    let agent_cfg = default_local_agent_config_path();
    discover_gateway_url(
        explicit_gateway_url,
        env_url.as_deref(),
        auth_cache.as_deref(),
        agent_cfg.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn remote_workspace_id_detection_distinguishes_devices_from_windows_drives() {
        assert!(workspace_id_targets_remote_device("MacBook-M2-Max:/Users/jinliang/workspace/weavelo"));
        assert!(workspace_id_targets_remote_device("HO5:/var/home/liang/workspace/remote_play"));
        assert!(!workspace_id_targets_remote_device("/Users/jinliang/workspace/octendra"));
        assert!(!workspace_id_targets_remote_device(":default"));
        assert!(!workspace_id_targets_remote_device("C:\\Users\\liang\\work"));
        assert!(!workspace_id_targets_remote_device("D:/repo"));
        assert!(!workspace_id_targets_remote_device("https://octendra.hackerlife.fun:8443"));
    }

    #[test]
    fn should_route_to_gateway_handles_fleet_tools_and_remote_devices() {
        assert!(should_route_to_gateway("devices_list", None, None, None, &json!({})));
        assert!(should_route_to_gateway("fleet_status", None, None, None, &json!({})));
        assert!(should_route_to_gateway(
            "terminal_exec",
            None,
            None,
            Some("MacBook-M2-Max"),
            &json!({"command": "uname -a"})
        ));
        assert!(should_route_to_gateway(
            "terminal_exec",
            None,
            None,
            None,
            &json!({"workspace_id": "HO5:/var/home/liang/workspace/remote_play", "command": "pwd"})
        ));
        assert!(!should_route_to_gateway(
            "code_read",
            None,
            None,
            None,
            &json!({"workspace_id": "/Users/jinliang/workspace/octendra"})
        ));
    }

    #[test]
    fn discover_gateway_url_respects_priority_and_falls_back_to_agent_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("code-gateway-auth.json");
        let agent_path = dir.path().join("agent.json");
        std::fs::write(
            &agent_path,
            r#"{"gateway_url": "https://octendra.hackerlife.fun:8443/"}"#,
        )
        .expect("write agent.json");

        assert_eq!(
            discover_gateway_url(None, None, Some(&auth_path), Some(&agent_path)).as_deref(),
            Some("https://octendra.hackerlife.fun:8443")
        );

        std::fs::write(
            &auth_path,
            r#"{"gateway_url":"https://cache.example.com:8443","client_id":"c1","access_token":"t1"}"#,
        )
        .expect("write auth cache");
        assert_eq!(
            discover_gateway_url(None, None, Some(&auth_path), Some(&agent_path)).as_deref(),
            Some("https://cache.example.com:8443")
        );
        assert_eq!(
            discover_gateway_url(
                Some("https://explicit.example.com/"),
                Some("https://env.example.com/"),
                Some(&auth_path),
                Some(&agent_path)
            )
            .as_deref(),
            Some("https://explicit.example.com")
        );
    }

    #[test]
    fn normalize_remote_arguments_builds_device_workspace_shorthand() {
        let mut args = json!({"command": "git status"});
        normalize_remote_arguments(
            "terminal_exec",
            Some("HO5"),
            Some("/var/home/liang/workspace/remote_play"),
            &mut args,
        );
        assert_eq!(
            args["workspace_id"],
            json!("HO5:/var/home/liang/workspace/remote_play")
        );
        assert_eq!(args["device_id"], json!("HO5"));
    }

    #[tokio::test]
    async fn call_gateway_tool_refreshes_expired_token_on_401() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).await.expect("read");
                let req = String::from_utf8_lossy(&buf[..n]);
                let lower = req.to_ascii_lowercase();
                let (status_line, ctype, body) = if req.starts_with("POST /oauth/token ") {
                    (
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"access_token":"fresh-tok","refresh_token":"refresh-2","expires_in":3600}"#,
                    )
                } else if lower.contains("authorization: bearer fresh-tok") {
                    (
                        "HTTP/1.1 200 OK",
                        "application/json",
                        r#"{"jsonrpc":"2.0","id":"1","result":{"structuredContent":{"ok":true}}}"#,
                    )
                } else {
                    ("HTTP/1.1 401 Unauthorized", "text/plain", "expired")
                };
                let response = format!(
                    "{status_line}\r\nConnection: close\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.expect("write");
            }
        });

        let dir = tempfile::tempdir().expect("tempdir");
        let cache_path = dir.path().join("code-gateway-auth.json");
        let gateway_url = format!("http://{addr}");
        write_auth_cache(
            &cache_path,
            &GatewayAuthCache {
                gateway_url: gateway_url.clone(),
                client_id: "cli-test".to_owned(),
                access_token: "stale-tok".to_owned(),
                refresh_token: Some("refresh-1".to_owned()),
                expires_at_unix: now_unix().saturating_add(3600),
            },
        )
        .expect("write cache");

        let res = call_gateway_tool_with_paths(
            &gateway_url,
            None,
            "devices_list",
            &json!({}),
            Some(&cache_path),
            None,
        )
        .await
        .expect("gateway call succeeds after refresh");
        assert_eq!(res, json!({"ok": true}));

        let updated = read_auth_cache(&cache_path).expect("updated cache");
        assert_eq!(updated.access_token, "fresh-tok");
        assert_eq!(updated.refresh_token.as_deref(), Some("refresh-2"));
        server.await.expect("server task");
    }
}
