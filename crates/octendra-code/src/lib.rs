//! Authenticated personal MCP gateway and outbound local code agent.
mod activity;
pub mod agent;
pub mod agent_log;
pub mod auth;
pub mod auto_upgrade;
mod branding;
mod capabilities;
mod code_payload;
mod payload_budget;
mod permission_view;
mod poll_health;
mod status_view;
mod submission_fence;
pub use capabilities::release_manifest;
pub mod adapter;
mod change_review;
pub mod computer_use;
pub mod contract;
mod delivery;
mod diagnostics;
mod durable_transfer;
pub mod files;
mod files_sync;
pub mod forge_browser;
pub mod gateway;
mod history_artifacts;
mod history_gateway;
mod history_retention;
mod job_dispatch;
mod job_receipts;
pub mod local_sudo;
mod maintenance;
pub mod maintenance_tasks;
mod oauth_handoff;
mod observations;
mod outcome_resolution;
mod progress;
mod readiness;
mod reads;
pub mod receipts;
mod resumable;
mod scheduler;
mod storage_gc;
pub mod store;
mod task_authorization;
pub mod terminal;
#[cfg(unix)]
mod terminal_io;
mod terminal_output;
mod terminal_sync;
mod token_output;
pub mod tools;
mod transfer_control;
mod transfer_journal;
mod transfer_receiver;
mod transfer_watch;
pub mod transfers;
mod upgrade_identity;
mod work_events;
mod workspace_context;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const SCOPES: &str = "code:read code:write terminal:exec desktop:observe desktop:input";
/// Supported desktop scopes require explicit OAuth consent and device registration.
pub const SUPPORTED_SCOPES: &str =
    "code:read code:write terminal:exec desktop:observe desktop:input";
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
pub fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
pub fn hash(value: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(value.as_ref()))
}
pub fn secret_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    bool::from(hash(a).as_bytes().ct_eq(hash(b).as_bytes()))
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceRegistration {
    pub id: String,
    pub name: String,
    pub token_hash: String,
    pub scopes: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge_browser: Option<forge_browser::ForgeBrowserConfig>,
    pub public_url: String,
    /// Other exact HTTPS origins serving this same Gateway and OAuth state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_url_aliases: Vec<String>,
    pub bind: String,
    pub state_dir: PathBuf,
    pub owner: String,
    pub password_hash: String,
    pub devices: Vec<DeviceRegistration>,
    pub redirect_uris: Vec<String>,
    /// Exact browser origins. OAuth callback URIs are a separate allowlist.
    #[serde(default = "default_mcp_client_origins")]
    pub allowed_origins: Vec<String>,
}
/// Preserve the browser policy of existing configurations on upgrade.
pub fn default_mcp_client_origins() -> Vec<String> {
    vec![
        "https://chatgpt.com".into(),
        "https://gemini.google.com".into(),
    ]
}
impl GatewayConfig {
    /// Check client-facing policy without starting a service or opening its database.
    pub fn validate_oauth_policy(&self) -> Result<()> {
        validate_url(&self.public_url)?;
        ensure!(
            self.public_url_aliases.len() <= 8,
            "too many public URL aliases"
        );
        for alias in &self.public_url_aliases {
            validate_url(alias).context("public URL alias must be an exact HTTPS origin")?;
            ensure!(
                !alias.contains('*'),
                "wildcard public URL aliases are not allowed"
            );
        }
        ensure!(self.allowed_origins.len() <= 32, "too many browser origins");
        ensure!(self.redirect_uris.len() <= 64, "too many OAuth callbacks");
        for origin in &self.allowed_origins {
            validate_url(origin).context("browser origin must be an exact HTTPS origin")?;
            ensure!(!origin.contains('*'), "wildcard origins are not allowed");
        }
        for uri in &self.redirect_uris {
            let u = reqwest::Url::parse(uri)?;
            ensure!(
                uri.len() <= 4096
                    && !uri.contains('*')
                    && !uri
                        .bytes()
                        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
                    && (u.scheme() == "https"
                        || native_oauth_callback(uri)
                        || installed_oauth_callback(uri))
                    && u.host_str().is_some()
                    && u.username().is_empty()
                    && u.password().is_none()
                    && u.fragment().is_none(),
                "OAuth callback must be exact HTTPS or a supported native callback without userinfo or fragment"
            );
        }
        Ok(())
    }
    pub(crate) fn browser_origins(&self) -> Vec<String> {
        let mut origins = self.allowed_origins.clone();
        origins.push(self.public_url.clone());
        origins.extend(self.public_url_aliases.iter().cloned());
        origins.sort();
        origins.dedup();
        origins
    }
    pub(crate) fn public_urls(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.public_url.as_str())
            .chain(self.public_url_aliases.iter().map(String::as_str))
    }
    /// Only an explicitly configured Host selects an alias. Never trust forwarded headers.
    pub(crate) fn public_url_for_request(&self, headers: &axum::http::HeaderMap) -> Option<&str> {
        let hosts = headers.get_all(axum::http::header::HOST);
        if hosts.iter().count() != 1 {
            return None;
        }
        let authority = hosts
            .iter()
            .next()
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.contains('@'))
            .and_then(|value| value.parse::<axum::http::uri::Authority>().ok())?;
        if authority.port().is_some() && authority.port_u16().is_none() {
            return None;
        }
        self.public_urls().find(|origin| {
            reqwest::Url::parse(origin).is_ok_and(|url| {
                url.host_str()
                    .is_some_and(|host| host.eq_ignore_ascii_case(authority.host()))
                    && url.port_or_known_default() == Some(authority.port_u16().unwrap_or(443))
            })
        })
    }
    pub(crate) fn authorization_csp(&self) -> String {
        self.authorization_csp_for_callback(None)
    }
    pub(crate) fn authorization_csp_for_callback(&self, callback: Option<&str>) -> String {
        // Only configured callbacks contribute redirect destinations. Browser
        // request origins must never implicitly authorize an OAuth callback.
        let mut origins: Vec<_> = self
            .redirect_uris
            .iter()
            .filter_map(|uri| reqwest::Url::parse(uri).ok())
            .filter(|uri| matches!(uri.scheme(), "https" | "http"))
            .map(|uri| uri.origin().ascii_serialization())
            .collect();
        // A validated grant contributes its concrete callback origin, never
        // a wildcard host/port or an unrelated browser origin.
        if let Some(callback) =
            callback.filter(|uri| native_oauth_callback(uri) || google_oauth_callback(uri))
            && let Ok(uri) = reqwest::Url::parse(callback)
        {
            origins.push(uri.origin().ascii_serialization());
            if google_oauth_callback(callback) {
                // Chrome also checks form-action on subsequent redirects.
                // Google Account Linking returns to Gemini after its callback.
                origins.push("https://gemini.google.com".into());
            }
        }
        if self
            .allowed_origins
            .iter()
            .any(|origin| origin == "https://gemini.google.com")
        {
            origins.push("https://oauth-redirect.googleusercontent.com".into());
        }
        if callback == Some("cursor://anysphere.cursor-mcp/oauth/callback")
            && self
                .redirect_uris
                .iter()
                .any(|uri| Some(uri.as_str()) == callback)
        {
            origins.push("cursor:".into());
        }
        origins.sort();
        origins.dedup();
        format!(
            "default-src 'none'; img-src 'self'; manifest-src 'self'; form-action 'self' {}; frame-ancestors 'none'; base-uri 'none'",
            origins.join(" ")
        )
    }
}

/// The six callback variants registered together by the real Spark client.
/// This is an exact host/path family, never a googleusercontent wildcard.
pub(crate) fn google_oauth_callback(candidate: &str) -> bool {
    let Ok(uri) = reqwest::Url::parse(candidate) else {
        return false;
    };
    let Some(suffix) = uri
        .path()
        .strip_prefix("/r/user_bound_custom-mcp-")
        .or_else(|| uri.path().strip_prefix("/a/user_bound_custom-mcp-"))
    else {
        return false;
    };
    uri.as_str() == candidate
        && uri.scheme() == "https"
        && matches!(
            uri.host_str(),
            Some(
                "oauth-redirect.googleusercontent.com"
                    | "oauth-redirect-sandbox.googleusercontent.com"
                    | "oauth-redirect-test.googleusercontent.com"
            )
        )
        && uri.port().is_none()
        && uri.username().is_empty()
        && uri.password().is_none()
        && uri.query().is_none()
        && uri.fragment().is_none()
        && !suffix.is_empty()
        && suffix.len() <= 512
        && suffix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.~".contains(&b))
}

/// Installed Cursor/Grok Bot callbacks are opt-in through exact configuration,
/// unlike the automatically accepted IP-loopback callback family below.
fn installed_oauth_callback(candidate: &str) -> bool {
    matches!(
        candidate,
        "cursor://anysphere.cursor-mcp/oauth/callback" | "http://localhost:8787/callback"
    )
}
/// Bounded native OAuth redirects. The concrete URI is still bound to the
/// registered client and every authorization/code exchange; PKCE is mandatory.
pub(crate) fn native_oauth_callback(candidate: &str) -> bool {
    let Ok(uri) = reqwest::Url::parse(candidate) else {
        return false;
    };
    let path_allowed = uri.path() == "/callback"
        || uri.path().strip_prefix("/callback/").is_some_and(|suffix| {
            !suffix.is_empty()
                && suffix.len() <= 128
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        });
    candidate.len() <= 512
        && uri.as_str() == candidate
        && uri.scheme() == "http"
        && matches!(uri.host_str(), Some("127.0.0.1" | "[::1]"))
        && uri.port().is_some_and(|port| port != 0)
        && uri.username().is_empty()
        && uri.password().is_none()
        && uri.query().is_none()
        && uri.fragment().is_none()
        && path_allowed
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub gateway_url: String,
    pub device_id: String,
    pub device_token: String,
    pub state_dir: PathBuf,
    pub roots: Vec<PathBuf>,
    pub allow_write: bool,
    pub allow_exec: bool,
    pub shell: PathBuf,
}
pub fn read_config<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(path)?.permissions().mode() & 0o077 == 0,
            "config must have mode 0600"
        );
    }
    serde_json::from_slice(&std::fs::read(path)?).context("invalid config")
}
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    write_private_with_owner_target(path, bytes, None)
}

fn write_private_with_owner_target(
    path: &Path,
    bytes: &[u8],
    owner_override: Option<(u32, u32)>,
) -> Result<()> {
    use std::io::Write;
    let existing = match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            ensure!(
                meta.is_file() && !meta.file_type().is_symlink(),
                "refusing to replace non-regular private file"
            );
            Some(meta)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let parent = path.parent().context("file needs parent")?;
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, fchown};
        let mode = existing
            .as_ref()
            .map(|meta| meta.permissions().mode() & 0o700)
            .filter(|mode| *mode != 0)
            .unwrap_or(0o600);
        if let Some((uid, gid)) =
            owner_override.or_else(|| existing.as_ref().map(|meta| (meta.uid(), meta.gid())))
        {
            fchown(file.as_file(), Some(uid), Some(gid))
                .context("preserve existing private file ownership")?;
        }
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn enroll_device(
    gateway_config: &Path,
    agent_config: &Path,
    name: String,
    state_dir: PathBuf,
    root: Vec<PathBuf>,
    allow_write: bool,
    allow_exec: bool,
    shell: PathBuf,
) -> Result<String> {
    ensure!(
        !agent_config.exists(),
        "refusing to overwrite device identity"
    );
    let mut g: GatewayConfig = read_config(gateway_config)?;
    ensure!(
        !g.devices.iter().any(|d| d.name == name),
        "device name already enrolled"
    );
    let id = uuid::Uuid::new_v4().to_string();
    let token = random();
    let mut scopes = vec!["code:read".into()];
    if allow_write {
        scopes.push("code:write".into());
    }
    if allow_exec {
        scopes.push("terminal:exec".into());
    }
    g.devices.push(DeviceRegistration {
        id: id.clone(),
        name,
        token_hash: hash(&token),
        scopes,
    });
    let a = AgentConfig {
        gateway_url: g.public_url.clone(),
        device_id: id.clone(),
        device_token: token,
        state_dir,
        roots: root,
        allow_write,
        allow_exec,
        shell,
    };
    write_private(agent_config, &serde_json::to_vec_pretty(&a)?)?;
    write_private(gateway_config, &serde_json::to_vec_pretty(&g)?)?;
    Ok(id)
}
pub fn validate_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "gateway URL must be an HTTPS origin, including public port if required"
    );
    ensure!(!value.ends_with('/'), "omit trailing slash from public URL");
    Ok(url)
}

#[cfg(all(test, unix))]
mod private_file_tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};

    #[test]
    fn write_private_preserves_existing_private_mode_and_owner_on_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.json");
        write_private(&path, b"{\"v\":1}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = std::fs::symlink_metadata(&path).unwrap();
        write_private(&path, b"{\"v\":2}").unwrap();
        let after = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(after.permissions().mode() & 0o777, 0o600);
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.gid(), before.gid());
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"v\":2}");
    }

    #[test]
    fn enroll_device_preserves_gateway_owner_group_and_private_mode() {
        let dir = tempfile::tempdir().unwrap();
        let gw_path = dir.path().join("gateway.json");
        let ag_path = dir.path().join("agent.json");
        let project_root = dir.path().join("workspace");
        std::fs::create_dir_all(&project_root).unwrap();
        let gw = GatewayConfig {
            public_url_aliases: vec![],
            forge_browser: None,
            public_url: "https://mcp.example.com:8443".into(),
            bind: "127.0.0.1:18787".into(),
            state_dir: dir.path().join("gw-state"),
            owner: "owner".into(),
            password_hash: "argon2-fixture".into(),
            devices: vec![],
            redirect_uris: vec!["https://chatgpt.com/connector_platform_oauth_redirect".into()],
            allowed_origins: default_mcp_client_origins(),
        };
        write_private(&gw_path, &serde_json::to_vec_pretty(&gw).unwrap()).unwrap();
        let initial_meta = std::fs::symlink_metadata(&gw_path).unwrap();
        let mut gids = [0 as nix::libc::gid_t; 64];
        let count = unsafe { nix::libc::getgroups(gids.len() as i32, gids.as_mut_ptr()) };
        if count > 0 {
            for &gid in &gids[..count as usize] {
                if gid != initial_meta.gid()
                    && chown(&gw_path, Some(initial_meta.uid()), Some(gid)).is_ok()
                {
                    break;
                }
            }
        }
        let before = std::fs::symlink_metadata(&gw_path).unwrap();
        let enrolled_id = enroll_device(
            &gw_path,
            &ag_path,
            "prod-workstation".into(),
            dir.path().join("ag-state"),
            vec![project_root],
            true,
            true,
            PathBuf::from("/bin/zsh"),
        )
        .unwrap();
        let after = std::fs::symlink_metadata(&gw_path).unwrap();
        let ag_meta = std::fs::symlink_metadata(&ag_path).unwrap();
        assert_eq!(after.permissions().mode() & 0o777, 0o600);
        assert_eq!(ag_meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.gid(), before.gid());
        let updated_gw: GatewayConfig = read_config(&gw_path).unwrap();
        let created_ag: AgentConfig = read_config(&ag_path).unwrap();
        assert_eq!(created_ag.device_id, enrolled_id);
        assert_eq!(updated_gw.devices.len(), 1);
        assert_eq!(updated_gw.devices[0].id, enrolled_id);
        assert_eq!(
            updated_gw.devices[0].token_hash,
            hash(&created_ag.device_token)
        );
    }

    #[test]
    fn write_private_cross_user_chown_failure_aborts_without_clobbering_gateway_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.json");
        write_private(&path, b"{\"original\":true}").unwrap();
        let before = std::fs::symlink_metadata(&path).unwrap();
        if unsafe { nix::libc::geteuid() } != 0 {
            let foreign_uid = before.uid().saturating_add(10_000);
            let err = write_private_with_owner_target(
                &path,
                b"{\"clobbered\":true}",
                Some((foreign_uid, before.gid())),
            )
            .unwrap_err();
            assert!(
                err.to_string()
                    .contains("preserve existing private file ownership")
            );
            let after = std::fs::symlink_metadata(&path).unwrap();
            assert_eq!(after.uid(), before.uid());
            assert_eq!(after.gid(), before.gid());
            assert_eq!(after.permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::read(&path).unwrap(), b"{\"original\":true}");
        }
    }
}
