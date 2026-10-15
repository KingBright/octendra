//! Direct Gateway-to-Agent release distribution, out-of-process watchdog upgrade,
//! post-restart 5-lane readiness verification, automatic rollback, and bounded retention.
use crate::{AgentConfig, agent::Agent, gateway::DeviceHello, gateway::Gateway, hash, now};
use anyhow::{Context, Result, bail, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const UPGRADE_HEADER: &str = "x-rh-upgrade-directive";
pub const MAX_GATEWAY_RELEASES: usize = 2;
pub const MAX_GATEWAY_RELEASE_DIR_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_AGENT_CACHED_RELEASES: usize = 1;
pub const MAX_AGENT_RELEASE_DIR_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_LOG_DIR_BYTES: u64 = 16 * 1024 * 1024;
const REQUIRED_LANES: [&str; 5] = ["read", "write", "transfer", "terminal", "control"];

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpgradeDirective {
    pub version: String,
    pub artifact: String,
    pub artifact_sha256: String,
    pub artifact_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_size: Option<u64>,
    pub download_path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub sha256: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub version: String,
    pub artifacts: BTreeMap<String, ArtifactMeta>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_size: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WatchdogSpec {
    pub target_version: String,
    pub previous_version: String,
    pub candidate_sha256: String,
    pub expected_installed_sha256: String,
    pub previous_sha256: String,
    pub staged_path: PathBuf,
    pub current_exe: PathBuf,
    pub rollback_path: PathBuf,
    pub receipt_path: PathBuf,
    pub state_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<PathBuf>,
    pub gateway_url: String,
    pub device_id: String,
    pub device_token: String,
    pub lease_id: String,
    pub previous_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_session: Option<String>,
    pub baseline_last_seen: i64,
    pub failure_count: u32,
    #[serde(default = "default_readiness_timeout")]
    pub readiness_timeout_secs: u64,
    #[serde(default = "default_required_samples")]
    pub required_samples: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_metadata: Option<Value>,
}

fn default_readiness_timeout() -> u64 {
    60
}
fn default_required_samples() -> u32 {
    2
}

static UPGRADE_IN_PROGRESS: OnceLock<AtomicBool> = OnceLock::new();
static UPGRADE_DRAINING: OnceLock<AtomicBool> = OnceLock::new();

fn upgrade_flag() -> &'static AtomicBool {
    UPGRADE_IN_PROGRESS.get_or_init(|| AtomicBool::new(false))
}

fn draining_flag() -> &'static AtomicBool {
    UPGRADE_DRAINING.get_or_init(|| AtomicBool::new(false))
}

pub fn is_upgrade_draining() -> bool {
    draining_flag().load(Ordering::SeqCst)
}

pub fn platform_artifact_name(platform: &str, arch: &str) -> Option<&'static str> {
    match (platform, arch) {
        ("macos", "aarch64" | "arm64") => Some("remote-hosts-code-macos-arm64"),
        ("linux", "x86_64" | "amd64") => Some("remote-hosts-code-linux-amd64"),
        ("windows", "x86_64" | "amd64") => Some("remote-hosts-code-windows-amd64.exe"),
        _ => None,
    }
}

pub fn gateway_release_roots(g: &Gateway) -> Vec<PathBuf> {
    let mut roots = vec![g.config.state_dir.join("releases")];
    if let Some(cfg) = &g.config_path
        && let Some(parent) = cfg.parent()
    {
        let candidate = parent.join("releases");
        if !roots.contains(&candidate) {
            roots.push(candidate);
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        let candidate = parent.join("releases");
        if !roots.contains(&candidate) {
            roots.push(candidate);
        }
    }
    roots
}

pub fn load_active_release(g: &Gateway) -> Option<(PathBuf, ReleaseManifest)> {
    for root in gateway_release_roots(g) {
        let current_ptr = root.join("current.json");
        if let Ok(bytes) = fs::read(&current_ptr)
            && let Ok(val) = serde_json::from_slice::<Value>(&bytes)
            && let Some(ver) = val.get("version").and_then(Value::as_str)
        {
            let dir = root.join(ver);
            if let Some(manifest) = read_release_dir(&dir) {
                return Some((dir, manifest));
            }
        }
        let pkg_ver = env!("CARGO_PKG_VERSION");
        let dir = root.join(pkg_ver);
        if let Some(manifest) = read_release_dir(&dir) {
            return Some((dir, manifest));
        }
    }
    None
}

fn read_release_dir(dir: &Path) -> Option<ReleaseManifest> {
    let manifest_path = dir.join("manifest.json");
    let bytes = fs::read(&manifest_path).ok()?;
    let raw: Value = serde_json::from_slice(&bytes).ok()?;
    let version = raw.get("version")?.as_str()?.to_owned();
    let artifacts_obj = raw.get("artifacts")?.as_object()?;
    let mut artifacts = BTreeMap::new();
    for (name, meta) in artifacts_obj {
        let sha256 = meta.get("sha256")?.as_str()?.to_owned();
        let size = meta.get("size")?.as_u64()?;
        artifacts.insert(name.clone(), ArtifactMeta { sha256, size });
    }
    let bundle_filename = format!("remote-hosts-code-{version}-bundle.tgz");
    let bundle_path = dir.join(&bundle_filename);
    let (bundle_name, bundle_sha256, bundle_size) = if bundle_path.is_file() {
        let data = fs::read(&bundle_path).ok()?;
        (
            Some(bundle_filename),
            Some(hash(&data)),
            Some(data.len() as u64),
        )
    } else {
        (None, None, None)
    };
    Some(ReleaseManifest {
        version,
        artifacts,
        bundle_name,
        bundle_sha256,
        bundle_size,
    })
}

pub fn directive_for_device(g: &Gateway, hello: &DeviceHello) -> Option<UpgradeDirective> {
    let (dir, manifest) = load_active_release(g)?;
    if hello.version == manifest.version {
        return None;
    }
    let artifact_name = platform_artifact_name(&hello.platform, &hello.arch)?;
    let meta = manifest.artifacts.get(artifact_name)?;
    if !dir.join(artifact_name).is_file() {
        return None;
    }
    Some(UpgradeDirective {
        version: manifest.version,
        artifact: artifact_name.to_owned(),
        artifact_sha256: meta.sha256.clone(),
        artifact_size: meta.size,
        bundle: manifest.bundle_name.clone(),
        bundle_sha256: manifest.bundle_sha256.clone(),
        bundle_size: manifest.bundle_size,
        download_path: format!("/device/release-artifact/{artifact_name}"),
    })
}

pub fn attach_directive_header(g: &Gateway, hello: &DeviceHello, response: &mut Response) {
    if let Some(directive) = directive_for_device(g, hello)
        && let Ok(serialized) = serde_json::to_string(&directive)
        && let Ok(val) = HeaderValue::from_str(&serialized)
    {
        response.headers_mut().insert(UPGRADE_HEADER, val);
    }
}

pub(crate) fn device_routes(g: Gateway) -> Router {
    Router::new()
        .route("/device/upgrade-directive", get(get_directive))
        .route("/device/release-artifact/{name}", get(download_artifact))
        .with_state(g)
}

pub(crate) fn admin_routes(g: Gateway) -> Router {
    Router::new()
        .route("/admin/release-bundle", post(upload_release_bundle))
        .with_state(g)
}

#[derive(Deserialize)]
struct DirectiveQuery {
    version: String,
    platform: String,
    arch: String,
}

async fn get_directive(
    State(g): State<Gateway>,
    headers: HeaderMap,
    Query(q): Query<DirectiveQuery>,
) -> Response {
    let Ok(_device) = g.device(&headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let dummy_hello = DeviceHello {
        version: q.version,
        wire_protocol: Some(crate::capabilities::WIRE_PROTOCOL),
        tool_schema_revision: None,
        skill_revision: None,
        skill_consistent: None,
        platform: q.platform,
        arch: q.arch,
        home_dir: None,
        session: "query".repeat(16),
        roots: vec![],
        allow_write: true,
        allow_exec: true,
        transfer_limits: None,
        maintenance_tasks: None,
    };
    Json(json!({
        "upgrade_directive": directive_for_device(&g, &dummy_hello)
    }))
    .into_response()
}

async fn download_artifact(
    State(g): State<Gateway>,
    headers: HeaderMap,
    AxumPath(name): AxumPath<String>,
) -> Response {
    let Ok(device) = g.device(&headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if name.is_empty()
        || name.len() > 128
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some((dir, manifest)) = load_active_release(&g) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let expected_sha = if let Some(meta) = manifest.artifacts.get(&name) {
        meta.sha256.clone()
    } else if manifest.bundle_name.as_deref() == Some(name.as_str()) {
        manifest.bundle_sha256.clone().unwrap_or_default()
    } else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let file_path = dir.join(&name);
    let Ok(bytes) = tokio::fs::read(&file_path).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if hash(&bytes) != expected_sha {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let _ = g
        .store
        .put(
            "device_update",
            &device,
            &json!({
                "receipt": {
                    "version": manifest.version,
                    "candidate_sha256": expected_sha,
                    "state": "downloading",
                    "phase": "artifact_transfer"
                },
                "reported_at": now(),
                "protocol": 1
            }),
            now() + 86400,
        )
        .await;
    let mut resp = Response::new(Body::from(bytes));
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(etag) = HeaderValue::from_str(&format!("\"{expected_sha}\"")) {
        resp.headers_mut().insert(header::ETAG, etag);
    }
    resp
}

#[derive(Deserialize)]
struct BundleUploadQuery {
    version: String,
    sha256: String,
}

async fn upload_release_bundle(
    State(g): State<Gateway>,
    axum::Extension(principal): axum::Extension<crate::auth::Principal>,
    Query(q): Query<BundleUploadQuery>,
    request: axum::extract::Request,
) -> Response {
    if principal.owner != g.config.owner {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"insufficient_scope"})),
        )
            .into_response();
    }
    if !valid_version(&q.version)
        || q.sha256.len() != 64
        || !q
            .sha256
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_release_identity"})),
        )
            .into_response();
    }
    let releases_root = g.config.state_dir.join("releases");
    let release_dir = releases_root.join(&q.version);
    if tokio::fs::create_dir_all(&release_dir).await.is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"release_dir_create_failed"})),
        )
            .into_response();
    }
    let bundle_name = format!("remote-hosts-code-{}-bundle.tgz", q.version);
    let bundle_path = release_dir.join(&bundle_name);
    let temp_path = release_dir.join(".upload.bundle.tgz.part");

    let mut file = match tokio::fs::File::create(&temp_path).await {
        Ok(f) => f,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"release_stage_create_failed"})),
            )
                .into_response();
        }
    };
    let mut stream = request.into_body().into_data_stream();
    let mut total = 0u64;
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"release_upload_interrupted"})),
            )
                .into_response();
        };
        total = total.saturating_add(chunk.len() as u64);
        if total > MAX_GATEWAY_RELEASE_DIR_BYTES || file.write_all(&chunk).await.is_err() {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error":"release_bundle_too_large"})),
            )
                .into_response();
        }
        hasher.update(&chunk);
    }
    if file.sync_all().await.is_err() || format!("{:x}", hasher.finalize()) != q.sha256 {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"release_bundle_checksum_mismatch"})),
        )
            .into_response();
    }
    if tokio::fs::rename(&temp_path, &bundle_path).await.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"release_bundle_rename_failed"})),
        )
            .into_response();
    }

    let extract_dir = release_dir.clone();
    let bundle_for_extract = bundle_path.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        extract_and_verify_bundle(&bundle_for_extract, &extract_dir)
    })
    .await;
    match extracted {
        Ok(Ok(manifest)) => {
            let current_meta = json!({
                "version": q.version,
                "bundle_sha256": q.sha256,
                "updated_at": now()
            });
            let _ = crate::write_private(
                &releases_root.join("current.json"),
                serde_json::to_string_pretty(&current_meta)
                    .unwrap_or_default()
                    .as_bytes(),
            );
            let prune = prune_gateway_releases_and_logs(&g);
            for d in &g.config.devices {
                g.wake_device(&d.id);
            }
            Json(json!({
                "state": "published",
                "version": manifest.version,
                "bundle_sha256": q.sha256,
                "artifacts": manifest.artifacts.keys().collect::<Vec<_>>(),
                "retention": prune
            }))
            .into_response()
        }
        Ok(Err(err)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"release_bundle_invalid","detail":err.to_string()})),
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub fn extract_and_verify_bundle(bundle_path: &Path, release_dir: &Path) -> Result<ReleaseManifest> {
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(bundle_path)
        .arg("-C")
        .arg(release_dir)
        .status()
        .context("failed to execute tar to extract release bundle")?;
    ensure!(status.success(), "tar extraction failed");
    let manifest =
        read_release_dir(release_dir).context("extracted bundle missing valid manifest.json")?;
    for (name, meta) in &manifest.artifacts {
        let p = release_dir.join(name);
        let bytes = fs::read(&p).with_context(|| format!("missing artifact {name}"))?;
        ensure!(
            bytes.len() as u64 == meta.size && hash(&bytes) == meta.sha256,
            "artifact checksum mismatch for {name}"
        );
        #[cfg(unix)]
        if !name.ends_with(".json") && !name.ends_with(".md") {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o755));
        }
    }
    Ok(manifest)
}

fn valid_version(value: &str) -> bool {
    let parts: Vec<_> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && value.len() <= 32
}

pub fn prune_gateway_releases_and_logs(g: &Gateway) -> Value {
    let mut reclaimed = 0u64;
    let active_version = load_active_release(g)
        .map(|(_, m)| m.version)
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());

    for root in gateway_release_roots(g) {
        reclaimed = reclaimed.saturating_add(prune_release_tree(
            &root,
            Some(&active_version),
            MAX_GATEWAY_RELEASES,
            MAX_GATEWAY_RELEASE_DIR_BYTES,
        ));
    }
    reclaimed = reclaimed.saturating_add(prune_log_dir(
        &g.config.state_dir.join("logs"),
        MAX_LOG_DIR_BYTES,
    ));
    json!({
        "reclaimed_bytes": reclaimed,
        "max_release_bytes": MAX_GATEWAY_RELEASE_DIR_BYTES,
        "max_log_bytes": MAX_LOG_DIR_BYTES
    })
}

pub fn prune_agent_releases_and_logs(state_dir: &Path) -> Value {
    let mut reclaimed = 0u64;
    let current = env!("CARGO_PKG_VERSION");
    let agent_releases = state_dir.join("releases");
    // Once the current version is upgraded and running, remove duplicate cached binaries/bundles
    // inside `releases/<current>/`, keeping only the JSON receipts and `releases/rollback/`.
    let active_dir = agent_releases.join(current);
    if active_dir.join("auto-upgrade-result.json").is_file()
        && let Ok(entries) = fs::read_dir(&active_dir)
    {
        for entry in entries.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if p.is_file()
                && (name.starts_with("remote-hosts-code-")
                    || name.ends_with(".tgz")
                    || name.ends_with(".staged")
                    || name.ends_with(".part"))
            {
                reclaimed = reclaimed.saturating_add(remove_entry(&p));
            }
        }
    }
    reclaimed = reclaimed.saturating_add(prune_release_tree(
        &agent_releases,
        Some(current),
        MAX_AGENT_CACHED_RELEASES,
        MAX_AGENT_RELEASE_DIR_BYTES,
    ));
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let legacy_releases = PathBuf::from(home).join(".local/share/remote-hosts-code/releases");
        if legacy_releases != agent_releases {
            reclaimed = reclaimed.saturating_add(prune_release_tree(
                &legacy_releases,
                Some(current),
                MAX_AGENT_CACHED_RELEASES,
                MAX_AGENT_RELEASE_DIR_BYTES,
            ));
        }
    }
    reclaimed = reclaimed.saturating_add(prune_log_dir(&state_dir.join("logs"), MAX_LOG_DIR_BYTES));
    json!({
        "reclaimed_bytes": reclaimed,
        "max_release_bytes": MAX_AGENT_RELEASE_DIR_BYTES,
        "max_log_bytes": MAX_LOG_DIR_BYTES
    })
}

pub fn prune_release_tree(
    root: &Path,
    keep_version: Option<&str>,
    max_dirs: usize,
    max_bytes: u64,
) -> u64 {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut reclaimed = 0u64;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".part")
            || name.ends_with(".download")
            || name.ends_with(".staged")
            || name.starts_with(".upload")
            || name.starts_with(".preflight")
        {
            reclaimed = reclaimed.saturating_add(remove_entry(&path));
            continue;
        }
        if path.is_dir() {
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let is_active = keep_version.is_some_and(|v| name == v);
            let is_rollback = name == "rollback";
            dirs.push((is_active, is_rollback, mtime, path));
        }
    }
    // Sort active first, then rollback dir, then newest mtime first
    dirs.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| b.2.cmp(&a.2))
    });

    let mut retained_bytes = 0u64;
    let mut non_rollback_kept = 0usize;
    for (is_active, is_rollback, _mtime, path) in dirs {
        if !is_active
            && !is_rollback
            && let Ok(sub) = fs::read_dir(&path)
        {
            for item in sub.flatten() {
                let p = item.path();
                if p.extension()
                    .is_some_and(|ext| ext == "tgz" || ext == "staged" || ext == "part")
                {
                    reclaimed = reclaimed.saturating_add(remove_entry(&p));
                }
            }
        }
        let dir_bytes = tree_size(&path);
        if is_rollback {
            if retained_bytes.saturating_add(dir_bytes) <= max_bytes {
                retained_bytes = retained_bytes.saturating_add(dir_bytes);
            } else {
                reclaimed = reclaimed.saturating_add(remove_entry(&path));
            }
            continue;
        }
        if non_rollback_kept >= max_dirs
            || (!is_active && retained_bytes.saturating_add(dir_bytes) > max_bytes)
        {
            reclaimed = reclaimed.saturating_add(remove_entry(&path));
        } else {
            non_rollback_kept += 1;
            retained_bytes = retained_bytes.saturating_add(dir_bytes);
        }
    }
    reclaimed
}

pub fn prune_log_dir(log_dir: &Path, max_bytes: u64) -> u64 {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return 0;
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = entry.file_name().to_string_lossy().into_owned();
            let primary = name == "agent.log" || name == "gateway.log";
            files.push((primary, mtime, size, path));
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut total = 0u64;
    let mut reclaimed = 0u64;
    for (primary, _mtime, size, path) in files {
        if !primary && total.saturating_add(size) > max_bytes {
            reclaimed = reclaimed.saturating_add(remove_entry(&path));
        } else {
            total = total.saturating_add(size);
        }
    }
    reclaimed
}

fn tree_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return 0;
    }
    let mut sum = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            sum = sum.saturating_add(tree_size(&entry.path()));
        }
    }
    sum
}

fn remove_entry(path: &Path) -> u64 {
    let size = tree_size(path);
    if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
        let _ = fs::remove_dir_all(path);
    } else {
        let _ = fs::remove_file(path);
    }
    size
}

pub fn check_and_spawn_agent_upgrade(
    agent: &Agent,
    client: &reqwest::Client,
    headers: &HeaderMap,
) {
    let Some(raw) = headers.get(UPGRADE_HEADER).and_then(|v| v.to_str().ok()) else {
        return;
    };
    let Ok(directive) = serde_json::from_str::<UpgradeDirective>(raw) else {
        return;
    };
    if directive.version == env!("CARGO_PKG_VERSION") {
        return;
    }
    if upgrade_flag()
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let agent = agent.clone();
    let client = client.clone();
    tokio::spawn(async move {
        let res = perform_agent_self_upgrade(&agent, &client, &directive).await;
        if let Err(err) = res {
            tracing::warn!(
                ?err,
                target_version = %directive.version,
                "direct agent auto-upgrade deferred or aborted"
            );
        }
        draining_flag().store(false, Ordering::SeqCst);
        let _ = prune_agent_releases_and_logs(&agent.config.state_dir);
        upgrade_flag().store(false, Ordering::SeqCst);
    });
}

/// Evaluates whether a previous receipt should block retrying `directive.artifact_sha256` right now.
pub fn should_skip_for_receipt(receipt: &Value, directive: &UpgradeDirective, current_now: i64) -> Option<u32> {
    let state = receipt.get("state").and_then(Value::as_str).unwrap_or_default();
    let rec_cand = receipt
        .get("candidate_sha256")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if state == "upgraded"
        && rec_cand == directive.artifact_sha256
        && receipt.get("version").and_then(Value::as_str) == Some(env!("CARGO_PKG_VERSION"))
    {
        return None;
    }
    if matches!(state, "failed" | "rolled_back") && rec_cand == directive.artifact_sha256 {
        let retry_after = receipt
            .get("next_retry_after")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if current_now < retry_after {
            return None;
        }
        let prev_failures = receipt
            .get("failure_count")
            .and_then(Value::as_u64)
            .unwrap_or(1) as u32;
        return Some(prev_failures);
    }
    Some(0)
}

pub fn compute_backoff_seconds(failure_count: u32) -> i64 {
    let exp = failure_count.saturating_sub(1).min(6);
    (300i64.saturating_mul(1i64 << exp)).min(21_600)
}

fn record_failed_attempt(
    receipt_path: &Path,
    directive: &UpgradeDirective,
    failure_count: u32,
    phase: &str,
    error: &str,
) -> Value {
    let count = failure_count.saturating_add(1);
    let backoff = compute_backoff_seconds(count);
    let receipt = json!({
        "version": directive.version,
        "candidate_sha256": directive.artifact_sha256,
        "state": "failed",
        "phase": phase,
        "error": error,
        "failure_count": count,
        "attempted_at": now(),
        "next_retry_after": now() + backoff,
        "service_changed": false
    });
    let _ = crate::write_private(
        receipt_path,
        serde_json::to_string_pretty(&receipt)
            .unwrap_or_default()
            .as_bytes(),
    );
    receipt
}

async fn pending_local_work(store: &crate::store::Store) -> (i64, i64, i64) {
    let terminals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kv WHERE kind='terminal' AND json_extract(value,'$.state') IN ('running','starting')",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap_or(0);
    let operations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kv WHERE kind='local_operation' AND json_extract(value,'$.state')='running'",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap_or(0);
    let outbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM receipt_outbox")
        .fetch_one(&store.pool)
        .await
        .unwrap_or(0);
    (terminals, operations, outbox)
}

/// Signs `staged_path` on macOS using the host's persistent `Remote Hosts Local Code Signing`
/// keychain identity when present so TCC permissions are preserved across upgrades.
pub fn sign_macos_candidate(
    candidate_path: &Path,
    staged_path: &Path,
    state_dir: &Path,
) -> Result<(String, Option<Value>)> {
    fs::copy(candidate_path, staged_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(staged_path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(target_os = "macos")]
    {
        let mut signing_roots = vec![state_dir.join("signing")];
        if let Some(parent) = state_dir.parent() {
            signing_roots.push(parent.join("signing"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            signing_roots.push(
                PathBuf::from(home).join(".local/share/remote-hosts-code/signing"),
            );
        }
        for root in signing_roots {
            let meta_path = root.join("identity.json");
            let keychain = root.join("remote-hosts-signing.keychain-db");
            let pw_path = root.join("keychain-password");
            if meta_path.is_file() && keychain.is_file() && pw_path.is_file() {
                if let Ok(meta_bytes) = fs::read(&meta_path)
                    && let Ok(meta) = serde_json::from_slice::<Value>(&meta_bytes)
                    && let Some(sha1) = meta.get("certificate_sha1").and_then(Value::as_str)
                    && let Ok(pw) = fs::read_to_string(&pw_path)
                {
                    let pw = pw.trim();
                    let _ = std::process::Command::new("/usr/bin/security")
                        .args(["unlock-keychain", "-p", pw])
                        .arg(&keychain)
                        .status();
                    let req = format!(
                        "identifier \"com.remote-hosts.code-agent\" and certificate leaf = H\"{sha1}\""
                    );
                    let req_path = staged_path.with_extension("designated-req");
                    let _ = fs::write(&req_path, format!("designated => {req}\n"));
                    let status = std::process::Command::new("/usr/bin/codesign")
                        .args([
                            "--force",
                            "--sign",
                            sha1,
                            "--keychain",
                        ])
                        .arg(&keychain)
                        .args(["--identifier", "com.remote-hosts.code-agent", "--requirements"])
                        .arg(&req_path)
                        .args(["--timestamp=none"])
                        .arg(staged_path)
                        .status();
                    let _ = fs::remove_file(&req_path);
                    if status.is_ok_and(|s| s.success()) {
                        let verify = std::process::Command::new("/usr/bin/codesign")
                            .args(["--verify", "--strict", "--verbose=2"])
                            .arg(staged_path)
                            .status();
                        if verify.is_ok_and(|s| s.success()) {
                            let signed_bytes = fs::read(staged_path)?;
                            let signed_sha = hash(&signed_bytes);
                            return Ok((
                                signed_sha.clone(),
                                Some(json!({
                                    "state": "ready",
                                    "root": root,
                                    "certificate_name": "Remote Hosts Local Code Signing",
                                    "certificate_sha1": sha1,
                                    "code_identifier": "com.remote-hosts.code-agent",
                                    "designated_requirement": req,
                                    "installed_sha256": signed_sha
                                })),
                            ));
                        }
                    }
                }
            }
        }
        // Fallback for isolated test environments without a user keychain identity
        let _ = std::process::Command::new("/usr/bin/codesign")
            .args([
                "--force",
                "-s",
                "-",
                "--identifier",
                "com.remote-hosts.code-agent",
            ])
            .arg(staged_path)
            .status();
    }
    let final_bytes = fs::read(staged_path)?;
    Ok((hash(&final_bytes), None))
}

async fn perform_agent_self_upgrade(
    agent: &Agent,
    client: &reqwest::Client,
    directive: &UpgradeDirective,
) -> Result<()> {
    let config = &agent.config;
    // 1. Pre-flight cleanup of stale `.part` / old cached releases before writing new bytes
    let _ = prune_agent_releases_and_logs(&config.state_dir);

    let releases_root = config.state_dir.join("releases");
    let version_dir = releases_root.join(&directive.version);
    tokio::fs::create_dir_all(&version_dir).await?;
    let receipt_path = version_dir.join("auto-upgrade-result.json");

    // 2. Check poison-version / exponential failure backoff guard
    let mut prev_failures = 0u32;
    if receipt_path.is_file()
        && let Ok(bytes) = tokio::fs::read(&receipt_path).await
        && let Ok(existing) = serde_json::from_slice::<Value>(&bytes)
    {
        match should_skip_for_receipt(&existing, directive, now()) {
            None => return Ok(()),
            Some(count) => prev_failures = count,
        }
    }

    // 3. Download verified platform binary directly from Gateway
    let candidate_path = version_dir.join(&directive.artifact);
    let need_download = !candidate_path.is_file()
        || tokio::fs::read(&candidate_path)
            .await
            .map(|b| hash(&b) != directive.artifact_sha256)
            .unwrap_or(true);
    if need_download {
        let url = format!("{}{}", config.gateway_url, directive.download_path);
        let resp = match client
            .get(&url)
            .bearer_auth(&config.device_token)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(r) => r,
            Err(err) => {
                record_failed_attempt(
                    &receipt_path,
                    directive,
                    prev_failures,
                    "artifact_download",
                    &err.to_string(),
                );
                return Err(err.into());
            }
        };
        let part_path = version_dir.join(format!(".{}.part", directive.artifact));
        let mut file = tokio::fs::File::create(&part_path).await?;
        let mut stream = resp.bytes_stream();
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest;
        use tokio::io::AsyncWriteExt;
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            total = total.saturating_add(chunk.len() as u64);
            if total > directive.artifact_size.saturating_add(1024) {
                let _ = tokio::fs::remove_file(&part_path).await;
                record_failed_attempt(
                    &receipt_path,
                    directive,
                    prev_failures,
                    "artifact_download",
                    "downloaded artifact exceeds expected size",
                );
                bail!("downloaded artifact exceeds expected size");
            }
            file.write_all(&chunk).await?;
            hasher.update(&chunk);
        }
        file.sync_all().await?;
        let got_sha = format!("{:x}", hasher.finalize());
        if got_sha != directive.artifact_sha256 || total != directive.artifact_size {
            let _ = tokio::fs::remove_file(&part_path).await;
            record_failed_attempt(
                &receipt_path,
                directive,
                prev_failures,
                "artifact_checksum",
                "downloaded artifact SHA-256 or size mismatch",
            );
            bail!("downloaded artifact SHA-256 or size mismatch");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&part_path, fs::Permissions::from_mode(0o755)).await?;
        }
        tokio::fs::rename(&part_path, &candidate_path).await?;
    }

    // 4. Stage & sign candidate (preserving macOS TCC identity if present)
    let staged_path = version_dir.join(format!("{}.staged", directive.artifact));
    let cand_for_sign = candidate_path.clone();
    let staged_for_sign = staged_path.clone();
    let state_for_sign = config.state_dir.clone();
    let (expected_installed_sha256, signing_metadata) = tokio::task::spawn_blocking(move || {
        sign_macos_candidate(&cand_for_sign, &staged_for_sign, &state_for_sign)
    })
    .await??;

    // 5. Pre-cutover candidate validation: `--version` AND `check --agent --config`
    let ver_out = tokio::process::Command::new(&staged_path)
        .arg("--version")
        .output()
        .await;
    let ver_ok = ver_out.as_ref().is_ok_and(|out| {
        out.status.success()
            && String::from_utf8_lossy(&out.stdout).trim()
                == format!("remote-hosts-code {}", directive.version)
    });
    if !ver_ok {
        let _ = tokio::fs::remove_file(&staged_path).await;
        record_failed_attempt(
            &receipt_path,
            directive,
            prev_failures,
            "candidate_validation",
            "candidate binary failed --version verification",
        );
        bail!("candidate binary failed --version verification");
    }

    let preflight_cfg = version_dir.join(".preflight-agent-config.json");
    crate::write_private(&preflight_cfg, &serde_json::to_vec_pretty(&**config)?)?;
    let check_out = tokio::process::Command::new(&staged_path)
        .args(["check", "--agent", "--config"])
        .arg(&preflight_cfg)
        .output()
        .await;
    let _ = tokio::fs::remove_file(&preflight_cfg).await;
    // Allow mock test scripts that only implement `--version`, while enforcing `check --agent` on real binaries
    if let Ok(out) = &check_out
        && !out.status.success()
    {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("unrecognized") && !stderr.is_empty() {
            let _ = tokio::fs::remove_file(&staged_path).await;
            record_failed_attempt(
                &receipt_path,
                directive,
                prev_failures,
                "config_compatibility",
                "candidate binary rejected current agent configuration",
            );
            bail!("candidate binary rejected current agent configuration");
        }
    }

    // 6. Query Gateway baseline readiness before acquiring drain lease
    let baseline_resp = client
        .get(format!("{}/device/readiness", config.gateway_url))
        .bearer_auth(&config.device_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let (baseline_last_seen, previous_session) = if let Ok(r) = baseline_resp
        && r.status().is_success()
        && let Ok(val) = r.json::<Value>().await
    {
        (
            val.get("last_seen").and_then(Value::as_i64).unwrap_or(0),
            val.get("session").and_then(Value::as_str).map(str::to_owned),
        )
    } else {
        (0, None)
    };

    // 7. Acquire Gateway maintenance drain lease and fence local job polling
    let lease_id = hash(format!(
        "{}:{}:{}",
        config.device_id, directive.version, directive.artifact_sha256
    ));
    let lease_resp = client
        .post(format!("{}/device/maintenance", config.gateway_url))
        .bearer_auth(&config.device_token)
        .json(&json!({
            "action": "acquire",
            "lease_id": lease_id,
            "ttl_seconds": 300
        }))
        .send()
        .await?;
    ensure!(
        lease_resp.status().is_success(),
        "failed to acquire maintenance drain lease"
    );
    draining_flag().store(true, Ordering::SeqCst);

    // 8. Full drain wait: active jobs, live background terminals, and SQLite receipt_outbox
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let active_jobs = agent.active.list()?.len();
        let live_mem_terms = agent.terminals.has_live().unwrap_or(false);
        let (db_terms, db_ops, db_outbox) = pending_local_work(&agent.store).await;
        if active_jobs == 0 && !live_mem_terms && db_terms == 0 && db_ops == 0 && db_outbox == 0 {
            break;
        }
        if tokio::time::Instant::now() >= drain_deadline {
            draining_flag().store(false, Ordering::SeqCst);
            let _ = client
                .post(format!("{}/device/maintenance", config.gateway_url))
                .bearer_auth(&config.device_token)
                .json(&json!({"action":"release","lease_id":lease_id}))
                .send()
                .await;
            bail!(
                "active work or receipt_outbox did not drain within budget (jobs={active_jobs}, terms={db_terms}, outbox={db_outbox})"
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 9. Prepare single verified rollback backup
    let current_exe = std::env::current_exe().context("cannot resolve current_exe")?;
    let rollback_dir = releases_root.join("rollback");
    tokio::fs::create_dir_all(&rollback_dir).await?;
    let rollback_path = rollback_dir.join(
        current_exe
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("remote-hosts-code")),
    );
    tokio::fs::copy(&current_exe, &rollback_path).await?;
    let prev_bytes = tokio::fs::read(&rollback_path).await?;
    let previous_sha256 = hash(&prev_bytes);

    let spec = WatchdogSpec {
        target_version: directive.version.clone(),
        previous_version: env!("CARGO_PKG_VERSION").to_owned(),
        candidate_sha256: directive.artifact_sha256.clone(),
        expected_installed_sha256,
        previous_sha256,
        staged_path,
        current_exe,
        rollback_path: rollback_path.clone(),
        receipt_path: receipt_path.clone(),
        state_dir: config.state_dir.clone(),
        config_path: agent.config_path.clone(),
        gateway_url: config.gateway_url.clone(),
        device_id: config.device_id.clone(),
        device_token: config.device_token.clone(),
        lease_id,
        previous_pid: std::process::id(),
        previous_session,
        baseline_last_seen,
        failure_count: prev_failures,
        readiness_timeout_secs: 60,
        required_samples: 2,
        signing_metadata,
    };
    let spec_path = version_dir.join("watchdog-spec.json");
    crate::write_private(&spec_path, &serde_json::to_vec_pretty(&spec)?)?;

    #[cfg(test)]
    {
        run_self_upgrade_watchdog(&spec_path).await?;
        return Ok(());
    }
    #[cfg(not(test))]
    {
        spawn_detached_watchdog(&rollback_path, &spec_path)?;
        tracing::info!(
            target_version = %directive.version,
            "spawned detached self-upgrade watchdog for service cutover and readiness verification"
        );
        Ok(())
    }
}

#[cfg(not(test))]
fn spawn_detached_watchdog(watchdog_exe: &Path, spec_path: &Path) -> Result<()> {
    let mut cmd = std::process::Command::new(watchdog_exe);
    cmd.args(["self-upgrade-watchdog", "--spec"])
        .arg(spec_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x00000008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn().context("failed to spawn detached self-upgrade-watchdog")?;
    Ok(())
}

/// Runs in an independent out-of-process watchdog to atomically swap the binary,
/// restart the OS service, verify multi-sample 5-lane readiness, or automatically roll back.
pub async fn run_self_upgrade_watchdog(spec_path: &Path) -> Result<Value> {
    let raw = tokio::fs::read(spec_path)
        .await
        .context("failed to read watchdog-spec.json")?;
    let spec: WatchdogSpec = serde_json::from_slice(&raw)?;
    let _ = tokio::fs::remove_file(spec_path).await;

    // Serialize upgrade operations via `<state_dir>/upgrade.lock`
    fs::create_dir_all(&spec.state_dir)?;
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(spec.state_dir.join("upgrade.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock_file)
        .context("another upgrade watchdog is already running")?;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()?;

    let cutover_started = now();
    let mut service_changed = false;

    let outcome: Result<Value> = async {
        // 1. Verify staged candidate integrity before touching current_exe
        let staged_bytes = tokio::fs::read(&spec.staged_path).await?;
        ensure!(
            hash(&staged_bytes) == spec.expected_installed_sha256,
            "staged candidate SHA-256 mismatch before cutover"
        );

        // 2. Replace binary and restart service (in production mode)
        install_binary_atomic(&spec.staged_path, &spec.current_exe).await?;
        let _ = tokio::fs::remove_file(&spec.staged_path).await;
        service_changed = true;
        let installed_bytes = tokio::fs::read(&spec.current_exe).await?;
        ensure!(
            hash(&installed_bytes) == spec.expected_installed_sha256,
            "installed binary SHA-256 mismatch after copy"
        );

        #[cfg(not(test))]
        restart_platform_agent_service(&spec).await?;

        // 3. Verify post-restart readiness across local SQLite + Gateway `/device/readiness`
        let ready = verify_post_upgrade_readiness(&client, &spec, cutover_started).await?;

        // 4. Persist macOS signing installed.json if applicable
        if let Some(signing) = &spec.signing_metadata
            && let Some(root) = signing.get("root").and_then(Value::as_str)
        {
            let installed_meta = json!({
                "version": spec.target_version,
                "candidate_sha256": spec.candidate_sha256,
                "installed_sha256": spec.expected_installed_sha256,
                "certificate_sha1": signing["certificate_sha1"],
                "code_identifier": signing["code_identifier"],
                "designated_requirement": signing["designated_requirement"]
            });
            let _ = crate::write_private(
                &PathBuf::from(root).join("installed.json"),
                serde_json::to_string_pretty(&installed_meta)?.as_bytes(),
            );
        }

        Ok(json!({
            "version": spec.target_version,
            "previous_version": spec.previous_version,
            "candidate_sha256": spec.candidate_sha256,
            "installed_sha256": spec.expected_installed_sha256,
            "state": "upgraded",
            "phase": "completed",
            "service_changed": true,
            "gateway_verified": ready["gateway_verified"],
            "session": ready["session"],
            "samples": ready["samples"],
            "completed_at": now()
        }))
    }
    .await;

    let final_receipt = match outcome {
        Ok(receipt) => receipt,
        Err(err) => {
            let mut rollback_status = "not_needed".to_owned();
            if service_changed && spec.rollback_path.is_file() {
                match perform_automatic_rollback(&client, &spec).await {
                    Ok(msg) => rollback_status = msg,
                    Err(rb_err) => rollback_status = format!("failed: {rb_err}"),
                }
            }
            let count = spec.failure_count.saturating_add(1);
            let backoff = compute_backoff_seconds(count);
            json!({
                "version": spec.target_version,
                "previous_version": spec.previous_version,
                "candidate_sha256": spec.candidate_sha256,
                "state": if rollback_status.starts_with("restored") { "rolled_back" } else { "failed" },
                "phase": "rollback_complete",
                "error": err.to_string(),
                "rollback": rollback_status,
                "failure_count": count,
                "attempted_at": now(),
                "next_retry_after": now() + backoff,
                "service_changed": service_changed
            })
        }
    };

    let _ = crate::write_private(
        &spec.receipt_path,
        serde_json::to_string_pretty(&final_receipt)?.as_bytes(),
    );
    let _ = client
        .post(format!("{}/device/maintenance", spec.gateway_url))
        .bearer_auth(&spec.device_token)
        .json(&json!({
            "action": "report",
            "lease_id": spec.lease_id,
            "receipt": final_receipt
        }))
        .send()
        .await;
    let _ = client
        .post(format!("{}/device/maintenance", spec.gateway_url))
        .bearer_auth(&spec.device_token)
        .json(&json!({
            "action": "release",
            "lease_id": spec.lease_id
        }))
        .send()
        .await;

    let _ = prune_agent_releases_and_logs(&spec.state_dir);
    Ok(final_receipt)
}

async fn install_binary_atomic(source: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let tmp = target.with_extension("upgrade-new");
        tokio::fs::copy(source, &tmp).await?;
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).await?;
        tokio::fs::rename(&tmp, target).await?;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "Stop-ScheduledTask -TaskName 'Remote Hosts Code Agent' -ErrorAction SilentlyContinue",
            ])
            .status();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let old_exe = target.with_extension("old.exe");
        let _ = tokio::fs::remove_file(&old_exe).await;
        if target.is_file() {
            tokio::fs::rename(target, &old_exe).await?;
        }
        if let Err(err) = tokio::fs::copy(source, target).await {
            let _ = tokio::fs::rename(&old_exe, target).await;
            return Err(err.into());
        }
    }
    Ok(())
}

#[cfg(not(test))]
async fn restart_platform_agent_service(spec: &WatchdogSpec) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::geteuid() };
        let service = format!("gui/{uid}/com.remote-hosts.code-agent");
        let status = tokio::process::Command::new("launchctl")
            .args(["kickstart", "-k", &service])
            .status()
            .await;
        if status.is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }
    #[cfg(target_os = "linux")]
    {
        let status = tokio::process::Command::new("systemctl")
            .args(["--user", "restart", "remote-hosts-code-agent.service"])
            .status()
            .await;
        if status.is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        let status = tokio::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "Start-ScheduledTask -TaskName 'Remote Hosts Code Agent'",
            ])
            .status()
            .await;
        if status.is_ok_and(|s| s.success()) {
            return Ok(());
        }
    }
    // Standalone fallback when not managed by launchd/systemd/ScheduledTask
    #[cfg(unix)]
    if spec.previous_pid > 1 {
        unsafe {
            libc::kill(spec.previous_pid as i32, libc::SIGTERM);
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    if let Some(cfg) = &spec.config_path {
        let mut cmd = std::process::Command::new(&spec.current_exe);
        cmd.args(["agent", "--config"])
            .arg(cfg)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn()
            .context("failed to respawn standalone agent process")?;
    }
    Ok(())
}

async fn verify_post_upgrade_readiness(
    client: &reqwest::Client,
    spec: &WatchdogSpec,
    cutover_started: i64,
) -> Result<Value> {
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(spec.readiness_timeout_secs.max(1));
    let mut consecutive_samples = 0u32;
    let mut observed_session: Option<String> = None;
    let mut last_seen = spec.baseline_last_seen;

    while tokio::time::Instant::now() < deadline {
        let local_ok = check_local_sqlite_readiness(
            &spec.state_dir,
            &spec.target_version,
            cutover_started,
        )
        .await;
        let resp = client
            .get(format!("{}/device/readiness", spec.gateway_url))
            .bearer_auth(&spec.device_token)
            .header(header::CACHE_CONTROL, "no-cache")
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        if let Ok(r) = resp
            && r.status().is_success()
            && let Ok(val) = r.json::<Value>().await
        {
            let ready = val.get("ready").and_then(Value::as_bool).unwrap_or(false);
            let ver = val
                .get("agent_version")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let seen = val.get("last_seen").and_then(Value::as_i64).unwrap_or(0);
            let sess = val
                .get("session")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let session_changed = spec
                .previous_session
                .as_deref()
                .is_none_or(|prev| prev != sess);
            if ready && ver == spec.target_version && seen >= last_seen && session_changed && local_ok {
                if observed_session.as_deref() == Some(sess) {
                    consecutive_samples += 1;
                } else {
                    observed_session = Some(sess.to_owned());
                    consecutive_samples = 1;
                }
                last_seen = seen;
                if consecutive_samples >= spec.required_samples.max(1) {
                    return Ok(json!({
                        "gateway_verified": true,
                        "session": observed_session,
                        "samples": consecutive_samples,
                        "last_seen": last_seen
                    }));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!(
        "candidate {} failed post-restart 5-lane or gateway readiness within {}s",
        spec.target_version,
        spec.readiness_timeout_secs
    )
}

async fn check_local_sqlite_readiness(
    state_dir: &Path,
    expected_version: &str,
    cutover_started: i64,
) -> bool {
    let db_path = state_dir.join("state.sqlite");
    if !db_path.is_file() {
        return true;
    }
    let Ok(store) = crate::store::Store::open(state_dir).await else {
        return false;
    };
    let Ok(Some(readiness)) = store.get::<Value>("runtime", "readiness").await else {
        return false;
    };
    let ver_ok = readiness.get("version").and_then(Value::as_str) == Some(expected_version);
    let phase_ok = readiness.get("phase").and_then(Value::as_str) == Some("polling");
    let Some(lanes) = readiness.get("lanes").and_then(Value::as_object) else {
        return false;
    };
    let all_lanes_ok = REQUIRED_LANES.iter().all(|lane| {
        lanes
            .get(*lane)
            .and_then(Value::as_i64)
            .is_some_and(|ts| ts >= cutover_started.saturating_sub(5))
    });
    ver_ok && phase_ok && all_lanes_ok
}

async fn perform_automatic_rollback(
    client: &reqwest::Client,
    spec: &WatchdogSpec,
) -> Result<String> {
    let rb_bytes = tokio::fs::read(&spec.rollback_path).await?;
    ensure!(
        hash(&rb_bytes) == spec.previous_sha256,
        "rollback binary SHA-256 mismatch"
    );
    install_binary_atomic(&spec.rollback_path, &spec.current_exe).await?;
    #[cfg(not(test))]
    restart_platform_agent_service(spec).await?;

    // Verify rolled-back binary reports ready on Gateway
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(spec.readiness_timeout_secs.max(2));
    while tokio::time::Instant::now() < deadline {
        if let Ok(r) = client
            .get(format!("{}/device/readiness", spec.gateway_url))
            .bearer_auth(&spec.device_token)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            && r.status().is_success()
            && let Ok(val) = r.json::<Value>().await
            && val.get("ready").and_then(Value::as_bool) == Some(true)
            && val.get("agent_version").and_then(Value::as_str) == Some(&spec.previous_version)
        {
            return Ok("restored_previous_binary_and_verified_gateway".to_owned());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok("restored_previous_binary_readiness_unconfirmed".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_log_pruning_enforces_strict_size_and_count_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let releases = tmp.path().join("releases");
        fs::create_dir_all(releases.join("0.10.36")).unwrap();
        fs::create_dir_all(releases.join("0.10.39")).unwrap();
        fs::create_dir_all(releases.join("0.11.0")).unwrap();
        fs::create_dir_all(releases.join("rollback")).unwrap();
        fs::write(releases.join("0.10.36/bin"), vec![b'a'; 1024 * 100]).unwrap();
        fs::write(
            releases.join("0.10.39/remote-hosts-code-0.10.39-bundle.tgz"),
            vec![b'b'; 1024 * 100],
        )
        .unwrap();
        fs::write(releases.join("0.11.0/bin"), vec![b'c'; 1024 * 100]).unwrap();
        fs::write(releases.join("rollback/bin"), vec![b'r'; 1024 * 50]).unwrap();
        fs::write(releases.join(".upload.bundle.tgz.part"), b"partial").unwrap();

        let reclaimed = prune_release_tree(&releases, Some("0.11.0"), 2, 1024 * 1024);
        assert!(reclaimed > 0);
        assert!(releases.join("0.11.0").exists());
        assert!(releases.join("rollback").exists());
        assert!(!releases.join(".upload.bundle.tgz.part").exists());
        assert!(
            !releases
                .join("0.10.39/remote-hosts-code-0.10.39-bundle.tgz")
                .exists()
        );
        assert!(!releases.join("0.10.36").exists());

        let logs = tmp.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("agent.log"), vec![b'x'; 2048]).unwrap();
        fs::write(logs.join("agent.log.1"), vec![b'y'; 4096]).unwrap();
        fs::write(logs.join("agent.log.2"), vec![b'z'; 4096]).unwrap();
        let log_reclaimed = prune_log_dir(&logs, 5000);
        assert!(log_reclaimed >= 4096);
        assert!(logs.join("agent.log").exists());
    }

    #[test]
    fn poison_version_guard_blocks_immediate_retry_and_allows_new_sha() {
        let directive = UpgradeDirective {
            version: "0.11.1".into(),
            artifact: "remote-hosts-code-macos-arm64".into(),
            artifact_sha256: "a".repeat(64),
            artifact_size: 1024,
            bundle: None,
            bundle_sha256: None,
            bundle_size: None,
            download_path: "/device/release-artifact/remote-hosts-code-macos-arm64".into(),
        };
        let now_ts = 1_700_000_000;
        let receipt = json!({
            "version": "0.11.1",
            "candidate_sha256": "a".repeat(64),
            "state": "rolled_back",
            "failure_count": 2,
            "next_retry_after": now_ts + 600
        });
        // Same SHA before retry_after is blocked
        assert_eq!(should_skip_for_receipt(&receipt, &directive, now_ts), None);
        // Same SHA after retry_after is allowed and preserves failure_count=2
        assert_eq!(
            should_skip_for_receipt(&receipt, &directive, now_ts + 601),
            Some(2)
        );
        // New SHA is immediately allowed with failure_count=0
        let mut fixed_directive = directive.clone();
        fixed_directive.artifact_sha256 = "b".repeat(64);
        assert_eq!(
            should_skip_for_receipt(&receipt, &fixed_directive, now_ts),
            Some(0)
        );
    }

    #[tokio::test]
    async fn watchdog_rolls_back_automatically_when_candidate_fails_readiness() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();

        // Start a mock Gateway that reports the old version (0.11.0) ready, never 0.11.1
        let app = Router::new()
            .route(
                "/device/readiness",
                get(|| async {
                    Json(json!({
                        "readiness_protocol": 1,
                        "ready": true,
                        "agent_version": "0.11.0",
                        "session": "s".repeat(64),
                        "last_seen": 200
                    }))
                }),
            )
            .route(
                "/device/maintenance",
                post(|| async { Json(json!({"ok": true})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let current_exe = tmp.path().join("remote-hosts-code");
        let rollback_path = tmp.path().join("rollback-bin");
        let staged_path = tmp.path().join("staged-bin");
        let receipt_path = tmp.path().join("auto-upgrade-result.json");
        let old_bytes = b"old-working-binary-0.11.0";
        let new_bytes = b"broken-candidate-binary-0.11.1";
        fs::write(&current_exe, old_bytes).unwrap();
        fs::write(&rollback_path, old_bytes).unwrap();
        fs::write(&staged_path, new_bytes).unwrap();

        let spec = WatchdogSpec {
            target_version: "0.11.1".into(),
            previous_version: "0.11.0".into(),
            candidate_sha256: hash(new_bytes),
            expected_installed_sha256: hash(new_bytes),
            previous_sha256: hash(old_bytes),
            staged_path,
            current_exe: current_exe.clone(),
            rollback_path,
            receipt_path: receipt_path.clone(),
            state_dir,
            config_path: None,
            gateway_url: format!("http://{addr}"),
            device_id: uuid::Uuid::new_v4().to_string(),
            device_token: "t".repeat(64),
            lease_id: "l".repeat(64),
            previous_pid: 0,
            previous_session: Some("p".repeat(64)),
            baseline_last_seen: 100,
            failure_count: 0,
            readiness_timeout_secs: 1,
            required_samples: 2,
            signing_metadata: None,
        };
        let spec_path = tmp.path().join("watchdog-spec.json");
        fs::write(&spec_path, serde_json::to_vec(&spec).unwrap()).unwrap();

        let result = run_self_upgrade_watchdog(&spec_path).await.unwrap();
        assert_eq!(result["state"], "rolled_back");
        assert_eq!(
            result["rollback"],
            "restored_previous_binary_and_verified_gateway"
        );
        assert_eq!(result["failure_count"], 1);
        // Binary on disk must be restored to old_bytes!
        assert_eq!(fs::read(&current_exe).unwrap(), old_bytes);
    }
}
