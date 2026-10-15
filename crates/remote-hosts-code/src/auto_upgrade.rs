//! Direct Gateway-to-Agent release distribution, auto-upgrade trigger, and bounded package/log retention.
use crate::{AgentConfig, gateway::DeviceHello, gateway::Gateway, hash, now};
use anyhow::{Context, Result, ensure};
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
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
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

static UPGRADE_IN_PROGRESS: OnceLock<AtomicBool> = OnceLock::new();

fn upgrade_flag() -> &'static AtomicBool {
    UPGRADE_IN_PROGRESS.get_or_init(|| AtomicBool::new(false))
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
    let bundle_Filename = format!("remote-hosts-code-{version}-bundle.tgz");
    let bundle_path = dir.join(&bundle_Filename);
    let (bundle_name, bundle_sha256, bundle_size) = if bundle_path.is_file() {
        let data = fs::read(&bundle_path).ok()?;
        (
            Some(bundle_Filename),
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
    reclaimed = reclaimed.saturating_add(prune_release_tree(
        &state_dir.join("releases"),
        Some(current),
        MAX_AGENT_CACHED_RELEASES,
        MAX_AGENT_RELEASE_DIR_BYTES,
    ));
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let legacy_releases = PathBuf::from(home).join(".local/share/remote-hosts-code/releases");
        reclaimed = reclaimed.saturating_add(prune_release_tree(
            &legacy_releases,
            Some(current),
            MAX_AGENT_CACHED_RELEASES,
            MAX_AGENT_RELEASE_DIR_BYTES,
        ));
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
        if name.ends_with(".part") || name.ends_with(".download") || name.starts_with(".upload") {
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
            dirs.push((is_active, mtime, path));
        }
    }
    // Sort active first, then newest mtime first
    dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut retained_bytes = 0u64;
    for (idx, (is_active, _mtime, path)) in dirs.into_iter().enumerate() {
        // Also remove cached .tgz bundles inside non-active directories immediately
        if !is_active && let Ok(sub) = fs::read_dir(&path) {
            for item in sub.flatten() {
                let p = item.path();
                if p.extension().is_some_and(|ext| ext == "tgz") {
                    reclaimed = reclaimed.saturating_add(remove_entry(&p));
                }
            }
        }
        let dir_bytes = tree_size(&path);
        if idx >= max_dirs || (!is_active && retained_bytes.saturating_add(dir_bytes) > max_bytes) {
            reclaimed = reclaimed.saturating_add(remove_entry(&path));
        } else {
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
    config: Arc<AgentConfig>,
    active: Arc<crate::scheduler::ActiveJobs>,
    client: reqwest::Client,
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
    tokio::spawn(async move {
        let res = perform_agent_self_upgrade(&config, &active, &client, &directive).await;
        if let Err(err) = res {
            tracing::warn!(?err, target_version = %directive.version, "direct agent auto-upgrade deferred");
        }
        let _ = prune_agent_releases_and_logs(&config.state_dir);
        upgrade_flag().store(false, Ordering::SeqCst);
    });
}

async fn perform_agent_self_upgrade(
    config: &AgentConfig,
    active: &crate::scheduler::ActiveJobs,
    client: &reqwest::Client,
    directive: &UpgradeDirective,
) -> Result<()> {
    let releases_root = config.state_dir.join("releases");
    let version_dir = releases_root.join(&directive.version);
    tokio::fs::create_dir_all(&version_dir).await?;
    let receipt_path = version_dir.join("auto-upgrade-result.json");
    if receipt_path.is_file()
        && let Ok(bytes) = tokio::fs::read(&receipt_path).await
        && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
        && v.get("installed_sha256").and_then(Value::as_str) == Some(&directive.artifact_sha256)
    {
        return Ok(());
    }

    // Download verified platform binary directly from Gateway
    let candidate_path = version_dir.join(&directive.artifact);
    let need_download = !candidate_path.is_file()
        || tokio::fs::read(&candidate_path)
            .await
            .map(|b| hash(&b) != directive.artifact_sha256)
            .unwrap_or(true);
    if need_download {
        let url = format!("{}{}", config.gateway_url, directive.download_path);
        let resp = client
            .get(&url)
            .bearer_auth(&config.device_token)
            .timeout(Duration::from_secs(300))
            .send()
            .await?
            .error_for_status()?;
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
            ensure!(
                total <= directive.artifact_size.saturating_add(1024),
                "downloaded artifact exceeds expected size"
            );
            file.write_all(&chunk).await?;
            hasher.update(&chunk);
        }
        file.sync_all().await?;
        let got_sha = format!("{:x}", hasher.finalize());
        if got_sha != directive.artifact_sha256 || total != directive.artifact_size {
            let _ = tokio::fs::remove_file(&part_path).await;
            anyhow::bail!("downloaded artifact SHA-256 or size mismatch");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&part_path, fs::Permissions::from_mode(0o755)).await?;
        }
        tokio::fs::rename(&part_path, &candidate_path).await?;
    }

    // Acquire Gateway maintenance drain lease before cutover
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

    // Wait up to 60s for active local jobs to finish draining
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !active.list()?.is_empty() {
        if tokio::time::Instant::now() >= drain_deadline {
            let _ = client
                .post(format!("{}/device/maintenance", config.gateway_url))
                .bearer_auth(&config.device_token)
                .json(&json!({"action":"release","lease_id":lease_id}))
                .send()
                .await;
            anyhow::bail!("active work did not drain within budget");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Verify candidate binary executes `--version`
    let ver_out = tokio::process::Command::new(&candidate_path)
        .arg("--version")
        .output()
        .await?;
    let ver_str = String::from_utf8_lossy(&ver_out.stdout);
    ensure!(
        ver_out.status.success()
            && ver_str.trim() == format!("remote-hosts-code {}", directive.version),
        "candidate binary failed --version verification"
    );

    // Replace current binary with single rollback backup
    let current_exe = std::env::current_exe().context("cannot resolve current_exe")?;
    let rollback_dir = releases_root.join("rollback");
    tokio::fs::create_dir_all(&rollback_dir).await?;
    let rollback_bin = rollback_dir.join(
        current_exe
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("remote-hosts-code")),
    );
    let _ = tokio::fs::copy(&current_exe, &rollback_bin).await;

    #[cfg(unix)]
    {
        let staged_target = current_exe.with_extension("upgrade-new");
        tokio::fs::copy(&candidate_path, &staged_target).await?;
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&staged_target, fs::Permissions::from_mode(0o755)).await?;
        #[cfg(target_os = "macos")]
        {
            let _ = tokio::process::Command::new("codesign")
                .args(["-s", "-", "--force"])
                .arg(&staged_target)
                .status()
                .await;
        }
        tokio::fs::rename(&staged_target, &current_exe).await?;
    }
    #[cfg(windows)]
    {
        let old_exe = current_exe.with_extension("old.exe");
        let _ = tokio::fs::remove_file(&old_exe).await;
        tokio::fs::rename(&current_exe, &old_exe).await?;
        if let Err(err) = tokio::fs::copy(&candidate_path, &current_exe).await {
            let _ = tokio::fs::rename(&old_exe, &current_exe).await;
            return Err(err.into());
        }
    }

    let receipt = json!({
        "version": directive.version,
        "candidate_sha256": directive.artifact_sha256,
        "installed_sha256": directive.artifact_sha256,
        "state": "upgraded",
        "phase": "restarting",
        "service_changed": true,
        "pid": std::process::id(),
        "started_at": now()
    });
    let _ = crate::write_private(
        &receipt_path,
        serde_json::to_string_pretty(&receipt)?.as_bytes(),
    );
    let _ = client
        .post(format!("{}/device/maintenance", config.gateway_url))
        .bearer_auth(&config.device_token)
        .json(&json!({
            "action": "report",
            "lease_id": lease_id,
            "receipt": receipt
        }))
        .send()
        .await;
    let _ = client
        .post(format!("{}/device/maintenance", config.gateway_url))
        .bearer_auth(&config.device_token)
        .json(&json!({
            "action": "release",
            "lease_id": lease_id
        }))
        .send()
        .await;

    let _ = prune_agent_releases_and_logs(&config.state_dir);
    tracing::info!(version = %directive.version, "agent binary upgraded; exiting for service supervisor restart");
    #[cfg(not(test))]
    {
        tokio::time::sleep(Duration::from_millis(200)).await;
        std::process::exit(0);
    }
    #[cfg(test)]
    Ok(())
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
        fs::write(releases.join("0.10.36/bin"), vec![b'a'; 1024 * 100]).unwrap();
        fs::write(
            releases.join("0.10.39/remote-hosts-code-0.10.39-bundle.tgz"),
            vec![b'b'; 1024 * 100],
        )
        .unwrap();
        fs::write(releases.join("0.11.0/bin"), vec![b'c'; 1024 * 100]).unwrap();
        fs::write(releases.join(".upload.part"), b"partial").unwrap();

        let reclaimed = prune_release_tree(&releases, Some("0.11.0"), 1, 64 * 1024 * 1024);
        assert!(reclaimed > 0);
        assert!(releases.join("0.11.0").is_dir());
        assert!(!releases.join("0.10.36").exists());
        assert!(!releases.join("0.10.39").exists());
        assert!(!releases.join(".upload.part").exists());

        let logs = tmp.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(logs.join("agent.log"), vec![b'x'; 2048]).unwrap();
        fs::write(logs.join("old-upgrade.log"), vec![b'y'; 4096]).unwrap();
        let log_reclaimed = prune_log_dir(&logs, 2048);
        assert_eq!(log_reclaimed, 4096);
        assert!(logs.join("agent.log").exists());
        assert!(!logs.join("old-upgrade.log").exists());
    }
}
