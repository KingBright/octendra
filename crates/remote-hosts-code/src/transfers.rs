//! Binary data plane. Tool JSON carries identifiers, never file bytes.
//! Inbound URL credentials are short-lived and kept out of operation journals.
use crate::{
    AgentConfig,
    files::{self, Workspace},
    gateway::{Gateway, Job},
    hash, now, random,
};
use anyhow::{Context, Result, anyhow, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as HttpPath, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use cap_std::fs::Dir;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const DISK_CAP: u64 = 1024 * 1024 * 1024;
pub(crate) const STORAGE_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
const BLOB_TTL: i64 = 3600;
pub(crate) const SOURCE_TTL: i64 = 900;
const LINK_TTL: i64 = 900;
const IDLE: Duration = crate::resumable::IDLE;

pub(crate) fn limit(v: &Value) -> Result<usize> {
    files::number(v, "max_bytes", DEFAULT_MAX_BYTES, 1, MAX_BYTES)
}
pub(crate) fn ensure_storage_capacity(path: &Path, requested: u64) -> Result<u64> {
    let available = fs2::available_space(path)?;
    ensure!(
        available >= requested.saturating_add(STORAGE_RESERVE_BYTES),
        "storage_capacity_insufficient: keep reserve space or request a smaller transfer"
    );
    Ok(available)
}
pub(crate) async fn source_authorization_status(g: &Gateway, id: &str) -> Result<Value> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT expires,value FROM kv WHERE kind='file_source' AND key=?")
            .bind(id)
            .fetch_optional(&g.store.pool)
            .await?;
    let control = crate::transfer_control::control(g, id).await?;
    let at = now();
    // The existing authorized source record supplies only its origin here.
    // Never return the signed path/query, even for an expired source.
    let endpoint = row.as_ref().and_then(|(_, value)| {
        let source: Value = serde_json::from_str(value).ok()?;
        let url = source_url(source["download_url"].as_str()?, &g.config.public_url).ok()?;
        Some(json!({"host":url.host_str()?,"port":url.port_or_known_default()?,"scheme":"https"}))
    });
    let (state, expires_at) = match row {
        Some((expires, _)) if expires > at => ("available", Some(expires)),
        Some((expires, _)) => ("expired", Some(expires)),
        None => ("required", None),
    };
    Ok(
        json!({"protocol":1,"state":state,"expires_at":expires_at,"source_endpoint":endpoint,
        "error_code":if state=="available" {Value::Null} else {json!("source_authorization_required")},
        "refresh_supported":true,"transfer_revision":control.revision,
        "next_action":if state=="available" {"observe_original_operation"} else {"transfer_resume_with_refreshed_file_authorization"}}),
    )
}
pub(crate) fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn verify_hash(expected: Option<&str>, actual: &str) -> Result<()> {
    if let Some(expected) = expected {
        ensure!(
            valid_hash(expected),
            "SHA-256 must be 64 lowercase hexadecimal characters"
        );
        ensure!(
            expected == actual,
            "sha256_mismatch: destination was not changed"
        );
    }
    Ok(())
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        // Full uploads are watched using sender demand and receiver progress.
        // read_timeout would also time out waiting for response headers.
        .build()?)
}
/// Only host-supplied file storage or this personal gateway may supply bytes.
/// DNS is additionally checked and pinned below; no redirects or forwarded bearer.
pub fn source_url(value: &str, gateway: &str) -> Result<reqwest::Url> {
    ensure!(value.len() <= 16384, "file URL too long");
    let u = reqwest::Url::parse(value).map_err(|_| anyhow!("invalid file URL"))?;
    let own = reqwest::Url::parse(gateway)?;
    ensure!(
        u.scheme() == "https"
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none(),
        "file source requires HTTPS without userinfo or fragment"
    );
    let host = u.host_str().context("file source needs host")?;
    let own_file = u.host_str() == own.host_str()
        && [Some(443), own.port_or_known_default()].contains(&u.port_or_known_default())
        && u.path().starts_with("/files/");
    let hosted = (host == "oaiusercontent.com"
        || host.ends_with(".oaiusercontent.com")
        || host.ends_with(".blob.core.windows.net"))
        && u.port_or_known_default() == Some(443);
    ensure!(
        own_file || hosted,
        "file source host not authorized; use a ChatGPT file parameter or a gateway download link"
    );
    Ok(u)
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_multicast()
                || ip.is_documentation()
                || o[0] == 0
                || o[0] >= 224
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            s[0] & 0xe000 == 0x2000 && !(s[0] == 0x2001 && (s[1] == 0x0db8 || s[1] < 0x0200))
        }
    }
}
/// Structured, URL-free pre-HTTP failures. These are not TCP/TLS failures.
#[derive(Debug)]
pub(crate) struct SourceClientFault {
    pub code: &'static str,
    host: String,
    os_error: Option<i32>,
}
impl SourceClientFault {
    fn new(u: &reqwest::Url, code: &'static str) -> Self {
        Self {
            code,
            host: u.host_str().unwrap_or_default().to_owned(),
            os_error: None,
        }
    }
    fn dns(u: &reqwest::Url, error: &std::io::Error) -> Self {
        let code = if error.kind() == std::io::ErrorKind::PermissionDenied {
            "source_network_access_denied"
        } else {
            "source_dns_lookup_failed"
        };
        Self {
            os_error: error.raw_os_error(),
            ..Self::new(u, code)
        }
    }
    pub fn diagnostic(&self) -> Value {
        json!({"code":self.code,"side":"device","stage":"source_client_initialization",
            "source_host":self.host,"os_error":self.os_error,
            "failure_boundary":match self.code {
                "source_network_access_denied"=>"device_network_permission",
                "source_address_policy_rejected"=>"source_address_policy",
                "source_client_initialization_failed"=>"device_client_initialization",
                _=>"source_dns"
            },"http_request_started":false,"authorization_failure_observed":false,
            "destination_changed":false})
    }
}
impl std::fmt::Display for SourceClientFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.code)
    }
}
impl std::error::Error for SourceClientFault {}

/// A response from the external file origin confirms that source HTTP began.
#[derive(Debug)]
pub(crate) struct SourceHttpFault {
    host: String,
    pub status: u16,
}
impl SourceHttpFault {
    pub(crate) fn new(url: &reqwest::Url, status: u16) -> Self {
        Self {
            host: url.host_str().unwrap_or_default().to_owned(),
            status,
        }
    }
    pub(crate) fn diagnostic(&self) -> Value {
        json!({"code":"source_http_authorization_rejected","side":"source",
            "stage":"source_http_response","source_host":self.host,"http_status":self.status,
            "failure_boundary":"source_http_authorization","http_request_started":true,
            "authorization_failure_observed":true,"destination_changed":false})
    }
}
impl std::fmt::Display for SourceHttpFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source_http_authorization_rejected")
    }
}
impl std::error::Error for SourceHttpFault {}

/// A destination permission failure. Unknown means publication may have committed.
#[derive(Debug)]
pub(crate) struct DestinationPermissionFault {
    stage: &'static str,
    os_error: Option<i32>,
    destination_changed: Option<bool>,
}
impl DestinationPermissionFault {
    fn new(stage: &'static str, os_error: Option<i32>, destination_changed: Option<bool>) -> Self {
        Self {
            stage,
            os_error,
            destination_changed,
        }
    }
    pub(crate) fn diagnostic(&self) -> Value {
        json!({"code":"destination_file_permission_denied","side":"destination",
            "stage":self.stage,"os_error":self.os_error,
            "failure_boundary":"destination_file_permission","http_request_started":true,
            "authorization_failure_observed":false,"destination_changed":self.destination_changed})
    }
}
impl std::fmt::Display for DestinationPermissionFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("destination_file_permission_denied")
    }
}
impl std::error::Error for DestinationPermissionFault {}

/// Convert only an actual PermissionDenied I/O cause; leave other errors intact.
pub(crate) fn classify_destination_error(
    error: anyhow::Error,
    stage: &'static str,
    destination_changed: Option<bool>,
) -> anyhow::Error {
    let os_error = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .filter(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
            .and_then(std::io::Error::raw_os_error)
    });
    let denied = error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
    });
    if denied {
        DestinationPermissionFault::new(stage, os_error, destination_changed).into()
    } else {
        error
    }
}

fn source_client_with_addresses(
    u: &reqwest::Url,
    addresses: &[SocketAddr],
) -> Result<reqwest::Client> {
    if addresses.is_empty() {
        return Err(SourceClientFault::new(u, "source_dns_no_addresses").into());
    }
    if addresses.iter().any(|a| !public_ip(a.ip())) {
        return Err(SourceClientFault::new(u, "source_address_policy_rejected").into());
    }
    let host = u.host_str().context("missing source host")?;
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(IDLE)
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|_| SourceClientFault::new(u, "source_client_initialization_failed").into())
}
pub(crate) async fn source_client(u: &reqwest::Url) -> Result<reqwest::Client> {
    let host = u.host_str().context("missing source host")?;
    let addresses: Vec<SocketAddr> = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::net::lookup_host((host, u.port_or_known_default().unwrap_or(443))),
    )
    .await
    .map_err(|_| SourceClientFault::new(u, "source_dns_timeout"))?
    .map_err(|error| SourceClientFault::dns(u, &error))?
    .collect();
    source_client_with_addresses(u, &addresses)
}
pub(crate) fn root(ws: &Workspace) -> Result<Dir> {
    Ok(Dir::open_ambient_dir(
        &ws.root,
        cap_std::ambient_authority(),
    )?)
}
pub(crate) fn digest_reader(
    mut file: impl Read,
    max: usize,
    mut output: Option<&mut std::fs::File>,
) -> Result<(usize, String)> {
    let mut digest = Sha256::new();
    let mut size = 0usize;
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size = size.checked_add(n).context("file size overflow")?;
        ensure!(size <= max, "file exceeds max_bytes");
        digest.update(&buf[..n]);
        if let Some(output) = output.as_mut() {
            output.write_all(&buf[..n]).map_err(|error| {
                classify_destination_error(error.into(), "destination_staging", Some(false))
            })?;
        }
    }
    Ok((size, format!("{:x}", digest.finalize())))
}
pub(crate) fn existing(dir: &Dir, name: &Path) -> Result<String> {
    match dir.symlink_metadata(name) {
        Ok(m) => {
            ensure!(
                m.is_file() && !m.is_symlink(),
                "destination must be a regular non-symlink file"
            );
            ensure!(
                m.len() <= MAX_BYTES as u64,
                "existing destination exceeds transfer limit"
            );
            Ok(digest_reader(dir.open(name)?, MAX_BYTES, None)?.1)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("absent".into()),
        Err(e) => Err(e.into()),
    }
}
/// Capability-relative staging; pins the destination parent directory.
pub(crate) struct Staged {
    dir: Dir,
    name: PathBuf,
    pub(crate) temporary: String,
    expected: Option<String>,
}
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = self.dir.remove_file(&self.temporary);
    }
}
impl Staged {
    pub(crate) fn new(ws: &Workspace, v: &Value) -> Result<(Self, std::fs::File)> {
        let path = files::relative(files::text(v, "path")?)?;
        let root = root(ws)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        root.create_dir_all(parent)?;
        let dir = root.open_dir(parent)?;
        let name = PathBuf::from(path.file_name().context("missing file name")?);
        let expected = v
            .get("expected_version")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(expected) = &expected {
            ensure!(
                expected == "absent" || valid_hash(expected),
                "expected_version must be absent or a SHA-256"
            );
            ensure!(
                existing(&dir, &name)? == *expected,
                "version_conflict: destination was not changed"
            );
        }
        let temporary = format!(".remote-hosts-transfer-{}.tmp", random());
        let mut opts = cap_std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        let file = dir.open_with(&temporary, &opts)?.into_std();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok((
            Self {
                dir,
                name,
                temporary,
                expected,
            },
            file,
        ))
    }
    pub(crate) fn publish(self) -> Result<()> {
        let current = existing(&self.dir, &self.name)?;
        if let Some(expected) = &self.expected {
            ensure!(
                current == *expected,
                "version_conflict_during_transfer: destination was not changed"
            );
        }
        if current == "absent" {
            // Atomic create-only publication, including a concurrent creator race.
            self.dir.hard_link(&self.temporary, &self.dir, &self.name)?;
            self.dir.remove_file(&self.temporary)?;
        } else {
            let permissions = self.dir.metadata(&self.name)?.permissions();
            self.dir.set_permissions(&self.temporary, permissions)?;
            self.dir.rename(&self.temporary, &self.dir, &self.name)?;
        }
        #[cfg(unix)]
        // Linux capability directories may be O_PATH handles, which cannot be
        // fsynced. Open the pinned directory itself for I/O without returning to
        // an ambient path or dropping the durability requirement.
        self.dir.open(".")?.sync_all()?;
        Ok(())
    }
}

pub async fn upload(
    config: &AgentConfig,
    ws: &Workspace,
    v: &Value,
    operation: &str,
) -> Result<Value> {
    upload_managed(
        config,
        ws,
        v,
        operation,
        &crate::progress::Progress::new(operation),
        None,
    )
    .await
}
pub(crate) async fn upload_managed(
    config: &AgentConfig,
    ws: &Workspace,
    v: &Value,
    operation: &str,
    progress: &crate::progress::Progress,
    scopes: Option<std::sync::Arc<crate::scheduler::WriteScopes>>,
) -> Result<Value> {
    let max = limit(v)?;
    let expected_sha = v.get("sha256").and_then(Value::as_str);
    if let Some(s) = expected_sha {
        ensure!(valid_hash(s), "invalid incoming sha256");
    }
    progress.phase("connecting");
    let http_control = client()?;
    let mut source = None;
    for attempt in 0..3 {
        let response = http_control
            .get(format!(
                "{}/device/file-source/{operation}",
                config.gateway_url
            ))
            .bearer_auth(&config.device_token)
            .timeout(Duration::from_secs(15))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                source = Some(
                    response
                        .json::<Value>()
                        .await
                        .map_err(|_| anyhow!("invalid file source response"))?,
                );
                break;
            }
            Ok(response) if response.status().as_u16() == 410 => anyhow::bail!(
                "source_authorization_required: refresh the original file authorization with transfer_resume; destination unchanged"
            ),
            Ok(response) if response.status().is_client_error() => anyhow::bail!(
                "file_source_rejected: authorization or operation identity rejected; destination unchanged"
            ),
            _ if attempt < 2 => {
                progress.retry(0);
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            _ => anyhow::bail!(
                "file source lookup failed after three attempts; destination unchanged"
            ),
        }
    }
    let source = source.context("file source lookup failed")?;
    let u = source_url(files::text(&source, "download_url")?, &config.gateway_url)?;
    let http = source_client(&u).await?;
    let (stage, raw) = Staged::new(ws, v)?;
    let mut output = tokio::fs::File::from_std(raw);
    let result = crate::resumable::fetch(
        &http,
        &u,
        &mut output,
        max,
        expected_sha,
        progress,
        crate::resumable::Policy::default(),
    )
    .await?;
    drop(output);
    progress.phase("waiting_resource");
    let guard = match scopes {
        Some(scopes) => Some(scopes.acquire(&ws.root).await?),
        None => None,
    };
    // Network I/O does not hold a workspace write lock. The actual publication
    // retains its guard through the blocking call and rechecks the target version.
    progress.phase("publishing");
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        stage.publish()
    })
    .await??;
    Ok(
        json!({"path":v["path"],"size":result.0,"sha256":result.1,"version":result.1,"state":"completed","atomic":true,"source_file_id":v["file"]["file_id"]}),
    )
}

pub async fn download(
    config: &AgentConfig,
    ws: &Workspace,
    v: &Value,
    operation: &str,
) -> Result<Value> {
    download_managed(
        config,
        ws,
        v,
        operation,
        &crate::progress::Progress::new(operation),
    )
    .await
}
pub(crate) async fn download_managed(
    config: &AgentConfig,
    ws: &Workspace,
    v: &Value,
    operation: &str,
    progress: &crate::progress::Progress,
) -> Result<Value> {
    progress.phase("snapshot");
    let ws = ws.clone();
    let args = v.clone();
    let dir = config.state_dir.join("transfer-staging");
    std::fs::create_dir_all(&dir)?;
    let (snapshot, size, sha, name) = tokio::task::spawn_blocking(move || -> Result<_> {
        let path = files::relative(files::text(&args, "path")?)?;
        let directory = root(&ws)?;
        let meta = directory.symlink_metadata(path)?;
        ensure!(
            meta.is_file() && !meta.is_symlink(),
            "source must be a regular non-symlink file"
        );
        let file = directory.open(path)?;
        ensure!(file.metadata()?.is_file(), "source is not a regular file");
        ensure!(meta.len() <= limit(&args)? as u64, "file exceeds max_bytes");
        let mut snapshot = tempfile::NamedTempFile::new_in(dir)?;
        let (size, sha) = digest_reader(file, limit(&args)?, Some(snapshot.as_file_mut()))?;
        verify_hash(args.get("expected_version").and_then(Value::as_str), &sha)?;
        snapshot.as_file().sync_all()?;
        let name = path
            .file_name()
            .context("missing source filename")?
            .to_string_lossy()
            .into_owned();
        Ok((snapshot, size, sha, name))
    })
    .await??;
    let http = client()?;
    progress.advance(0, Some(size as u64));
    let mut accepted = false;
    for attempt in 0..3 {
        progress.phase("transferring");
        let mut input = tokio::fs::File::from_std(snapshot.reopen()?);
        input.seek(std::io::SeekFrom::Start(0)).await?;
        let consumed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = consumed.clone();
        let stream = ReaderStream::new(input).map(move |chunk| {
            if let Ok(bytes) = &chunk {
                counter.fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
            }
            chunk
        });
        let request = http
            .post(format!("{}/device/files/{operation}", config.gateway_url))
            .bearer_auth(&config.device_token)
            .header("x-file-size", size)
            .header("x-file-sha256", &sha)
            .header(header::CONTENT_LENGTH, size)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(reqwest::Body::wrap_stream(stream));
        let receiver_progress = http
            .get(format!(
                "{}/device/file-progress/{operation}",
                config.gateway_url
            ))
            .bearer_auth(&config.device_token);
        let response =
            crate::transfer_watch::send(request, receiver_progress, consumed, IDLE).await;
        match response {
            Ok(r) if r.status().is_success() => {
                let manifest: Value = tokio::time::timeout(IDLE, r.json())
                    .await
                    .map_err(|_| anyhow!("gateway transfer receipt timed out"))?
                    .map_err(|_| anyhow!("invalid gateway transfer receipt"))?;
                ensure!(
                    manifest["sha256"] == sha && manifest["size"] == size,
                    "gateway transfer receipt mismatch"
                );
                accepted = true;
                break;
            }
            Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => {
                anyhow::bail!("gateway rejected file transfer ({})", r.status().as_u16());
            }
            _ if attempt < 2 => {
                progress.retry(0);
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
            _ => {}
        }
    }
    ensure!(accepted, "file upload to gateway failed; source unchanged");
    progress.advance(size as u64, Some(size as u64));
    Ok(
        json!({"artifact_id":operation,"path":v["path"],"file_name":name,"size":size,"sha256":sha,"state":"completed"}),
    )
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Blob {
    pub(crate) operation: String,
    pub(crate) device: String,
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) size: usize,
    pub(crate) sha256: String,
    pub(crate) expires: i64,
}
#[derive(Serialize, Deserialize)]
struct Link {
    operation: String,
    device: String,
    owner: String,
}

pub(crate) async fn desktop_artifact(
    g: &Gateway,
    job: &Job,
) -> Result<remote_hosts_desktop_protocol::FrameArtifact> {
    let a: crate::agent::DesktopFrameJob = serde_json::from_value(job.arguments.clone())?;
    ensure!(
        job.tool == "desktop_frame"
            && job.owner == g.config.owner
            && a.device_id.to_string() == job.device_id
            && a.capability_version > 0,
        "desktop artifact owner/device"
    );
    let (request, result): (String, Option<String>) =
        sqlx::query_as("SELECT request,result FROM jobs WHERE id=? AND device=?")
            .bind(a.observed_operation_id.to_string())
            .bind(&job.device_id)
            .fetch_one(&g.store.pool)
            .await?;
    let original: Job = serde_json::from_str(&request)?;
    let original_args: crate::agent::DesktopJob =
        serde_json::from_value(original.arguments.clone())?;
    let original_identity = original_args.identity(&original)?;
    ensure!(
        original.tool == "desktop_worker"
            && original.owner == job.owner
            && matches!(
                original_args.request,
                remote_hosts_desktop_protocol::Request::Observe { .. }
            )
            && original_args.task_id == a.task_id
            && original_args.capability_version == a.capability_version,
        "desktop artifact task/capability"
    );
    let result: Value =
        serde_json::from_str(&result.context("desktop observation not delivered")?)?;
    let artifact: remote_hosts_desktop_protocol::FrameArtifact =
        serde_json::from_value(result["frame_artifact"].clone())?;
    ensure!(
        artifact.operation_id == a.observed_operation_id
            && artifact.binding.identity == original_identity
            && artifact.capability_version == a.capability_version
            && artifact.expires_at_ms > crate::now_ms() as u64
            && artifact.bytes > 0
            && artifact.bytes <= remote_hosts_desktop_protocol::MAX_WORKER_PIXELS
            && valid_hash(&artifact.sha256),
        "desktop artifact expired/identity"
    );
    let blocked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE device=? AND json_extract(request,'$.owner')=? AND json_extract(request,'$.tool')='desktop_worker' AND COALESCE(json_extract(request,'$.arguments.request.binding.epoch'),json_extract(request,'$.arguments.request.frame.binding.epoch'))=? AND (json_extract(request,'$.arguments.request.operation')='close' OR json_extract(result,'$.error')='outcome_unknown'))")
        .bind(&job.device_id).bind(&job.owner).bind(artifact.binding.epoch.to_string())
        .fetch_one(&g.store.pool).await?;
    ensure!(!blocked, "desktop artifact epoch closed or unknown");
    let failed_export: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM jobs WHERE device=? AND json_extract(request,'$.owner')=? AND json_extract(request,'$.tool')='desktop_frame' AND json_extract(request,'$.arguments.observed_operation_id')=? AND json_extract(result,'$.error_code')='desktop_export_interrupted')")
        .bind(&job.device_id).bind(&job.owner).bind(a.observed_operation_id.to_string())
        .fetch_one(&g.store.pool).await?;
    ensure!(
        !failed_export,
        "desktop artifact export already interrupted"
    );
    Ok(artifact)
}

pub(crate) async fn retire_desktop_epoch(
    g: &Gateway,
    operation_id: &str,
    result: &Value,
) -> Result<()> {
    let Some(text): Option<String> = sqlx::query_scalar("SELECT request FROM jobs WHERE id=?")
        .bind(operation_id)
        .fetch_optional(&g.store.pool)
        .await?
    else {
        return Ok(());
    };
    let job: Job = serde_json::from_str(&text)?;
    if job.tool != "desktop_worker" {
        return Ok(());
    }
    let a: crate::agent::DesktopJob = serde_json::from_value(job.arguments.clone())?;
    let epoch = match a.request {
        remote_hosts_desktop_protocol::Request::Close { binding }
            if result["state"] == "closed" || result["error"] == "outcome_unknown" =>
        {
            binding.epoch
        }
        remote_hosts_desktop_protocol::Request::Act { frame, .. }
            if result["error"] == "outcome_unknown" =>
        {
            frame.binding.epoch
        }
        remote_hosts_desktop_protocol::Request::Observe { binding }
            if result["error"] == "outcome_unknown" =>
        {
            binding.epoch
        }
        _ => return Ok(()),
    };
    let _allocation = g.transfer_limits.allocation.lock().await;
    let ids:Vec<String>=sqlx::query_scalar(
        "SELECT DISTINCT f.id FROM jobs f JOIN jobs o ON o.id=json_extract(f.request,'$.arguments.observed_operation_id') JOIN kv k ON k.key=f.id AND k.kind IN ('file_blob','transfer_receiver') WHERE f.device=? AND json_extract(f.request,'$.owner')=? AND json_extract(f.request,'$.tool')='desktop_frame' AND json_extract(o.request,'$.arguments.request.binding.epoch')=? LIMIT 3")
        .bind(&job.device_id).bind(&job.owner).bind(epoch.to_string()).fetch_all(&g.store.pool).await?;
    for id in ids {
        uuid::Uuid::parse_str(&id)?;
        let Ok(_permit) = g.transfer_limits.try_acquire(&job.device_id, &id) else {
            continue;
        };
        for path in [
            blob_path(g, &id),
            g.config
                .state_dir
                .join("file-objects")
                .join(format!("{id}.part040")),
        ] {
            match tokio::fs::remove_file(path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        sqlx::query("DELETE FROM kv WHERE kind IN ('file_blob','transfer_receiver','receive_progress') AND key=?")
            .bind(&id).execute(&g.store.pool).await?;
    }
    Ok(())
}

pub(crate) async fn desktop_export_job(g: &Gateway, mut job: Job) -> Result<Job> {
    if job.tool == "desktop_frame" {
        let artifact = desktop_artifact(g, &job).await?;
        job.arguments["path"] = json!(format!("{}.raw", artifact.operation_id));
        job.arguments["max_bytes"] = json!(artifact.bytes);
        job.arguments["expected_version"] = json!(artifact.sha256);
        job.arguments["artifact_expires_at"] = json!(artifact.expires_at_ms / 1000);
    }
    Ok(job)
}

pub(crate) fn blob_path(g: &Gateway, id: &str) -> PathBuf {
    g.config
        .state_dir
        .join("file-objects")
        .join(format!("{id}.blob"))
}

pub fn routes(g: Gateway) -> Router {
    Router::new()
        .route("/device/file-source/{id}", get(get_source))
        .route("/device/files/{id}", post(receive_file))
        .route("/device/file-progress/{id}", get(get_received_progress))
        .route("/files/{key}/{name}", get(serve_file))
        .with_state(g)
}
async fn authorized_job(g: &Gateway, h: &HeaderMap, id: &str, tool: &str) -> Result<Job> {
    uuid::Uuid::parse_str(id).context("invalid operation id")?;
    let device = g.device(h).await?;
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT request,state FROM jobs WHERE id=? AND device=?")
            .bind(id)
            .bind(&device)
            .fetch_optional(&g.store.pool)
            .await?;
    let (request, state) = row.context("transfer operation not found")?;
    let job: Job = serde_json::from_str(&request)?;
    ensure!(
        (job.tool == tool || (tool == "file_download" && job.tool == "desktop_frame"))
            && job.owner == g.config.owner
            && (state == "dispatched" || state == "done"),
        "transfer operation is not authorized"
    );
    let _scope = crate::tools::scope(&job.tool).context("unknown transfer scope")?;
    ensure!(
        g.config.devices.iter().any(|d| d.id == device),
        "transfer scope revoked"
    );
    desktop_export_job(g, job).await
}
async fn get_source(
    State(g): State<Gateway>,
    HttpPath(id): HttpPath<String>,
    headers: HeaderMap,
) -> Response {
    if authorized_job(&g, &headers, &id, "file_upload")
        .await
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    match g.store.get::<Value>("file_source", &id).await {
        Ok(Some(value)) => Json(value).into_response(),
        Ok(None) => match source_authorization_status(&g, &id).await {
            Ok(status) => (StatusCode::GONE, Json(status)).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn get_received_progress(
    State(g): State<Gateway>,
    HttpPath(id): HttpPath<String>,
    headers: HeaderMap,
) -> Response {
    if authorized_job(&g, &headers, &id, "file_download")
        .await
        .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    match g.store.get::<Value>("receive_progress", &id).await {
        Ok(Some(value)) => Json(value).into_response(),
        Ok(None) => Json(json!({"snapshot":{"bytes_done":0,"phase":"queued"}})).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
pub(crate) async fn cleanup(g: &Gateway) -> Result<u64> {
    for b in g.store.list::<Blob>("file_blob").await? {
        if b.expires <= now() {
            let _ = std::fs::remove_file(blob_path(g, &b.operation));
            sqlx::query("DELETE FROM kv WHERE kind='file_blob' AND key=?")
                .bind(&b.operation)
                .execute(&g.store.pool)
                .await?;
        }
    }
    g.store.prune().await?;
    let dir = g.config.state_dir.join("file-objects");
    std::fs::create_dir_all(&dir)?;
    let mut used = 0u64;
    let mut count = 0usize;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = match entry.metadata() {
            Ok(meta) => meta,
            // A failed/cancelled stream may drop its temporary file concurrently.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !meta.is_file() {
            continue;
        }
        if meta.modified()?.elapsed().unwrap_or_default()
            > Duration::from_secs(BLOB_TTL as u64 + 300)
            && entry.path().extension().is_none_or(|e| e != "part040")
        {
            std::fs::remove_file(entry.path())?;
        } else {
            used = used.saturating_add(meta.len());
            count += 1;
        }
    }
    ensure!(count < 512, "gateway artifact count limit reached");
    Ok(used)
}
async fn receive_file(
    State(g): State<Gateway>,
    HttpPath(id): HttpPath<String>,
    request: Request,
) -> Response {
    let headers = request.headers();
    let job = match authorized_job(&g, headers, &id, "file_download").await {
        Ok(j) => j,
        Err(_) => return StatusCode::FORBIDDEN.into_response(),
    };
    let size = headers
        .get("x-file-size")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok());
    let sha = headers
        .get("x-file-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let Some(size) = size.filter(|n| *n <= limit(&job.arguments).unwrap_or(0)) else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    if !valid_hash(&sha) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(_permit) = g.transfer_limits.try_acquire(&job.device_id, &id) else {
        return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "2")]).into_response();
    };
    let saved = match g.store.get::<Blob>("file_blob", &id).await {
        Ok(saved) => saved,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if let Some(saved) = saved {
        if saved.sha256 != sha || saved.size != size || saved.expires <= now() {
            return StatusCode::CONFLICT.into_response();
        }
        // Drain and verify a repeated streaming request before acknowledging it.
        // Returning early can reset a sender that still has body bytes in flight.
        let verified = async {
            let mut stream = request.into_body().into_data_stream();
            let mut count = 0usize;
            let mut digest = Sha256::new();
            while let Some(bytes) = tokio::time::timeout(IDLE, stream.next()).await? {
                let bytes = bytes.context("duplicate stream failed")?;
                count = count
                    .checked_add(bytes.len())
                    .context("file size overflow")?;
                ensure!(count <= size, "duplicate exceeds declared size");
                digest.update(&bytes);
            }
            ensure!(
                count == size && format!("{:x}", digest.finalize()) == sha,
                "duplicate checksum mismatch"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        return match verified {
            Ok(()) => Json(json!({"size":size,"sha256":sha,"duplicate":true})).into_response(),
            Err(error) if error.is::<tokio::time::error::Elapsed>() => {
                StatusCode::REQUEST_TIMEOUT.into_response()
            }
            Err(_) => StatusCode::BAD_REQUEST.into_response(),
        };
    }
    let temporary = {
        let _allocation = g.transfer_limits.allocation.lock().await;
        let used = match cleanup(&g).await {
            Ok(n) => n,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        if used.saturating_add(size as u64) > DISK_CAP
            || ensure_storage_capacity(&g.config.state_dir, size as u64).is_err()
        {
            return StatusCode::INSUFFICIENT_STORAGE.into_response();
        }
        let temporary =
            match tempfile::NamedTempFile::new_in(g.config.state_dir.join("file-objects")) {
                Ok(file) => file,
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
        // Reserve the full logical size before releasing quota coordination. Other
        // concurrent streams count this reservation, not its current partial length.
        if temporary.as_file().set_len(size as u64).is_err() {
            return StatusCode::INSUFFICIENT_STORAGE.into_response();
        }
        temporary
    };
    let progress = crate::progress::Progress::new(&id);
    progress.phase("transferring");
    progress.advance(0, Some(size as u64));
    record_received_progress(&g, &progress).await;
    let result = async {
        let mut file = tokio::fs::File::from_std(temporary.reopen()?);
        let mut stream = request.into_body().into_data_stream();
        let mut count = 0usize;
        let mut digest = Sha256::new();
        let mut last_report = std::time::Instant::now();
        while let Some(bytes) = tokio::time::timeout(IDLE, stream.next()).await? {
            let bytes = bytes.context("incoming file stream failed")?;
            count = count
                .checked_add(bytes.len())
                .context("file size overflow")?;
            ensure!(count <= size, "incoming file exceeds declared size");
            digest.update(&bytes);
            file.write_all(&bytes).await?;
            progress.advance(count as u64, Some(size as u64));
            if count == bytes.len() || last_report.elapsed() >= Duration::from_millis(500) {
                record_received_progress(&g, &progress).await;
                last_report = std::time::Instant::now();
            }
        }
        progress.phase("verifying");
        record_received_progress(&g, &progress).await;
        ensure!(
            count == size && format!("{:x}", digest.finalize()) == sha,
            "incoming file checksum or size mismatch"
        );
        file.sync_all().await?;
        drop(file);
        let name = files::relative(files::text(&job.arguments, "path")?)?
            .file_name()
            .context("missing file name")?
            .to_string_lossy()
            .into_owned();
        ensure!(name.len() <= 255, "file name too long");
        let blob = Blob {
            operation: id.clone(),
            device: job.device_id,
            owner: job.owner,
            name,
            size,
            sha256: sha.clone(),
            expires: now() + BLOB_TTL,
        };
        // Coordinate only publication metadata, not the network/body lifetime.
        let _allocation = g.transfer_limits.allocation.lock().await;
        temporary.persist(blob_path(&g, &id))?;
        g.store.put("file_blob", &id, &blob, i64::MAX).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    progress.phase(if result.is_ok() {
        "completed"
    } else {
        "failed"
    });
    record_received_progress(&g, &progress).await;
    match result {
        Ok(()) => Json(json!({"size":size,"sha256":sha})).into_response(),
        Err(error) if error.is::<tokio::time::error::Elapsed>() => {
            StatusCode::REQUEST_TIMEOUT.into_response()
        }
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
async fn record_received_progress(g: &Gateway, progress: &crate::progress::Progress) {
    if let Some(snapshot) = progress.snapshot() {
        let _ = g
            .store
            .put(
                "receive_progress",
                &snapshot.operation_id,
                &json!({"snapshot":snapshot,"reported_at":now(),"origin":"gateway_receiver"}),
                now() + 86400,
            )
            .await;
    }
}
fn unavailable_download(result: &mut Value) -> Result<()> {
    let receipt = result.as_object_mut().context("invalid download receipt")?;
    receipt.remove("download_url");
    receipt.remove("expires_at");
    receipt.insert("download_available".into(), json!(false));
    receipt.insert(
        "download_unavailable_reason".into(),
        json!("artifact_expired_or_removed"),
    );
    receipt.insert(
        "recovery_action".into(),
        json!("export_again_with_expected_source_version"),
    );
    Ok(())
}

/// Called only after the original operation and principal have been authorized.
pub async fn decorate(g: &Gateway, job: &Job, result: &mut Value) -> Result<()> {
    if !matches!(job.tool.as_str(), "file_download" | "desktop_frame")
        || result.get("error").is_some()
        || result.get("state").is_some_and(|s| s != "completed")
    {
        return Ok(());
    }
    if job.tool == "desktop_frame" && desktop_artifact(g, job).await.is_err() {
        return unavailable_download(result);
    }
    // The durable operation receipt outlives its temporary downloadable blob.
    // Expiration affects availability, never whether the operation completed.
    // Database, scope and receipt-integrity failures must still propagate.
    let Some(blob): Option<Blob> = g.store.get("file_blob", &job.id).await? else {
        return unavailable_download(result);
    };
    ensure!(
        blob.device == job.device_id && blob.owner == job.owner,
        "download artifact scope mismatch"
    );
    ensure!(
        result["sha256"] == blob.sha256 && result["size"] == blob.size,
        "download artifact receipt mismatch"
    );
    if blob.expires <= now() {
        return unavailable_download(result);
    }
    let key = random();
    let expires = (now() + LINK_TTL).min(blob.expires);
    g.store
        .put(
            "file_link",
            &hash(&key),
            &Link {
                operation: job.id.clone(),
                device: blob.device.clone(),
                owner: blob.owner.clone(),
            },
            expires,
        )
        .await?;
    let name: String = url::form_urlencoded::byte_serialize(blob.name.as_bytes()).collect();
    result["download_available"] = json!(true);
    result["download_url"] = json!(format!("{}/files/{key}/{name}", g.config.public_url));
    result["expires_at"] = json!(expires);
    result["artifact_expires_at"] = json!(blob.expires);
    result["mime_type"] = json!("application/octet-stream");
    Ok(())
}
// Desktop bodies recheck the epoch and their original deadline for every bounded
// read. A removed path does not revoke an already-open file descriptor.
fn desktop_body(g: Gateway, job: Job, file: tokio::fs::File, bytes: u64, deadline: u64) -> Body {
    let reader = ReaderStream::with_capacity(file.take(bytes), 64 * 1024);
    let stream = futures_util::stream::try_unfold(
        (reader, g, job),
        move |(mut reader, g, job)| async move {
            let denied = || {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "desktop artifact revoked or expired",
                )
            };
            if crate::now_ms() as u64 >= deadline {
                return Err(denied());
            }
            desktop_artifact(&g, &job).await.map_err(|_| denied())?;
            let Some(chunk) = reader.next().await else {
                return Ok(None);
            };
            let chunk = chunk?;
            if crate::now_ms() as u64 >= deadline {
                return Err(denied());
            }
            desktop_artifact(&g, &job).await.map_err(|_| denied())?;
            Ok::<_, std::io::Error>(Some((chunk, (reader, g, job))))
        },
    );
    Body::from_stream(stream)
}

async fn serve_file(
    State(g): State<Gateway>,
    HttpPath((key, _name)): HttpPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if key.len() != 64 || !key.bytes().all(|c| c.is_ascii_hexdigit()) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let link = match g.store.get::<Link>("file_link", &hash(&key)).await {
        Ok(Some(link)) => link,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let desktop_job: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
        "SELECT request FROM jobs WHERE id=? AND json_extract(request,'$.tool')='desktop_frame'",
    )
    .bind(&link.operation)
    .fetch_optional(&g.store.pool)
    .await;
    let desktop_job = match desktop_job {
        Ok(v) => v,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let desktop = if let Some(text) = desktop_job {
        let Ok(job) = serde_json::from_str::<Job>(&text) else {
            return StatusCode::GONE.into_response();
        };
        let Ok(artifact) = desktop_artifact(&g, &job).await else {
            return StatusCode::GONE.into_response();
        };
        Some((job, artifact.expires_at_ms))
    } else {
        None
    };
    let _required_scope = if desktop.is_some() {
        "desktop:observe"
    } else {
        "code:read"
    };
    if link.owner != g.config.owner || !g.config.devices.iter().any(|d| d.id == link.device) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let blob = match g.store.get::<Blob>("file_blob", &link.operation).await {
        Ok(Some(b)) if b.expires > now() && b.device == link.device && b.owner == link.owner => b,
        _ => return StatusCode::GONE.into_response(),
    };
    let mut file = match tokio::fs::File::open(blob_path(&g, &blob.operation)).await {
        Ok(f) => f,
        Err(_) => return StatusCode::GONE.into_response(),
    };
    if !matches!(file.metadata().await, Ok(meta) if meta.is_file() && meta.len() == blob.size as u64)
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut start = 0usize;
    let mut end = blob.size;
    let mut status = StatusCode::OK;
    // RFC 9110: mismatched/weak/date If-Range uses the full immutable body.
    let etag = format!("\"{}\"", blob.sha256);
    if let Some(range) = headers.get(header::RANGE)
        && headers
            .get(header::IF_RANGE)
            .is_none_or(|value| value == etag.as_str())
    {
        let parsed = (|| -> Option<(usize, usize)> {
            let (a, b) = range
                .to_str()
                .ok()?
                .strip_prefix("bytes=")?
                .split_once('-')?;
            if a.is_empty() {
                let suffix = b.parse::<usize>().ok()?;
                return (suffix > 0 && blob.size > 0)
                    .then_some((blob.size.saturating_sub(suffix), blob.size));
            }
            let a = a.parse::<usize>().ok()?;
            let b = if b.is_empty() {
                blob.size
            } else {
                b.parse::<usize>().ok()?.checked_add(1)?.min(blob.size)
            };
            (a < b && b <= blob.size).then_some((a, b))
        })();
        let Some((a, b)) = parsed else {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{}", blob.size))],
            )
                .into_response();
        };
        start = a;
        end = b;
        status = StatusCode::PARTIAL_CONTENT;
    }
    if file
        .seek(std::io::SeekFrom::Start(start as u64))
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let name: String = url::form_urlencoded::byte_serialize(blob.name.as_bytes()).collect();
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, end - start)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"download.bin\"; filename*=UTF-8''{name}"),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, format!("\"{}\"", blob.sha256));
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{}", end - 1, blob.size),
        );
    }
    let body = if let Some((job, deadline)) = desktop {
        desktop_body(
            g,
            job,
            file,
            (end - start) as u64,
            deadline.min(blob.expires as u64 * 1000),
        )
    } else {
        Body::from_stream(ReaderStream::new(file.take((end - start) as u64)))
    };
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn desktop_stream_fixture() -> (tempfile::TempDir, Gateway, Job, String) {
        use remote_hosts_desktop_protocol::*;
        let dir = tempfile::tempdir().unwrap();
        let identity = Identity {
            owner: "offline-stream".into(),
            task_id: uuid::Uuid::new_v4(),
            device_id: uuid::Uuid::new_v4(),
            desktop_id: "offline-private".into(),
        };
        let binding = Binding {
            identity: identity.clone(),
            epoch: uuid::Uuid::new_v4(),
        };
        let g = Gateway::new(crate::GatewayConfig {
            public_url: "https://offline.invalid".into(),
            bind: "127.0.0.1:0".into(),
            state_dir: dir.path().join("gateway"),
            forge_browser: None,
            owner: identity.owner.clone(),
            password_hash: "test-unused".into(),
            devices: vec![crate::DeviceRegistration {
                id: identity.device_id.to_string(),
                name: "offline".into(),
                token_hash: hash("test-device"),
                scopes: vec!["desktop:observe".into(), "desktop:input".into()],
            }],
            redirect_uris: vec![],
            allowed_origins: vec![],
        })
        .await
        .unwrap();
        let observe = Job {
            id: uuid::Uuid::new_v4().to_string(),
            owner: identity.owner.clone(),
            device_id: identity.device_id.to_string(),
            tool: "desktop_worker".into(),
            arguments: json!({"task_id":identity.task_id,"device_id":identity.device_id,"capability_version":7,
                "idempotency_key":"observe","request":Request::Observe { binding:binding.clone() }}),
        };
        let data = vec![1u8; 196608];
        let artifact = FrameArtifact {
            operation_id: uuid::Uuid::parse_str(&observe.id).unwrap(),
            binding,
            capability_version: 7,
            expires_at_ms: crate::now_ms() as u64 + 60000,
            bytes: data.len(),
            sha256: hash(&data),
            pixels: RawPixels {
                encoding: "nv12".into(),
                size: [512, 256],
                color: [1; 4],
                plane_lengths: vec![131072, 65536],
                plane_sha256: vec![hash(&data[..131072]), hash(&data[131072..])],
            },
        };
        sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,?,'done',?)")
            .bind(&observe.id).bind(&observe.device_id).bind(&observe.id).bind(hash(serde_json::to_vec(&observe).unwrap()))
            .bind(serde_json::to_string(&observe).unwrap()).bind(json!({"frame_artifact":artifact}).to_string())
            .bind(now()).execute(&g.store.pool).await.unwrap();
        let export = Job {
            id: uuid::Uuid::new_v4().to_string(),
            owner: identity.owner.clone(),
            device_id: identity.device_id.to_string(),
            tool: "desktop_frame".into(),
            arguments: json!({"task_id":identity.task_id,"device_id":identity.device_id,"capability_version":7,
                "observed_operation_id":observe.id,"idempotency_key":"frame"}),
        };
        sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,NULL,'done',?)")
            .bind(&export.id).bind(&export.device_id).bind(&export.id).bind(hash(serde_json::to_vec(&export).unwrap()))
            .bind(serde_json::to_string(&export).unwrap()).bind(now()).execute(&g.store.pool).await.unwrap();
        tokio::fs::create_dir_all(g.config.state_dir.join("file-objects"))
            .await
            .unwrap();
        tokio::fs::write(blob_path(&g, &export.id), &data)
            .await
            .unwrap();
        let blob = Blob {
            operation: export.id.clone(),
            device: export.device_id.clone(),
            owner: export.owner.clone(),
            name: "frame.raw".into(),
            size: data.len(),
            sha256: hash(&data),
            expires: now() + 60,
        };
        g.store
            .put("file_blob", &export.id, &blob, i64::MAX)
            .await
            .unwrap();
        let key = random();
        g.store
            .put(
                "file_link",
                &hash(&key),
                &Link {
                    operation: export.id.clone(),
                    device: export.device_id.clone(),
                    owner: export.owner.clone(),
                },
                now() + 60,
            )
            .await
            .unwrap();
        (dir, g, export, key)
    }

    #[tokio::test]
    async fn desktop_slow_stream_close_revokes_open_descriptor() {
        let (_dir, g, export, key) = desktop_stream_fixture().await;
        let response = serve_file(
            State(g.clone()),
            HttpPath((key, "frame.raw".into())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 64 * 1024);
        let observed = export.arguments["observed_operation_id"].as_str().unwrap();
        let request: String = sqlx::query_scalar("SELECT request FROM jobs WHERE id=?")
            .bind(observed)
            .fetch_one(&g.store.pool)
            .await
            .unwrap();
        let mut close: Job = serde_json::from_str(&request).unwrap();
        close.id = uuid::Uuid::new_v4().to_string();
        close.arguments["request"]["operation"] = json!("close");
        sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,NULL,'queued',?)")
            .bind(&close.id).bind(&close.device_id).bind(&close.id).bind(hash(serde_json::to_vec(&close).unwrap()))
            .bind(serde_json::to_string(&close).unwrap()).bind(now()).execute(&g.store.pool).await.unwrap();
        tokio::fs::remove_file(blob_path(&g, &export.id))
            .await
            .unwrap();
        sqlx::query("DELETE FROM kv WHERE kind='file_blob' AND key=?")
            .bind(&export.id)
            .execute(&g.store.pool)
            .await
            .unwrap();
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn desktop_slow_stream_deadline_stops_existing_descriptor() {
        let (_dir, g, export, key) = desktop_stream_fixture().await;
        let observed = export.arguments["observed_operation_id"].as_str().unwrap();
        let deadline = crate::now_ms() as u64 + 250;
        sqlx::query(
            "UPDATE jobs SET result=json_set(result,'$.frame_artifact.expires_at_ms',?) WHERE id=?",
        )
        .bind(deadline as i64)
        .bind(observed)
        .execute(&g.store.pool)
        .await
        .unwrap();
        let response = serve_file(
            State(g.clone()),
            HttpPath((key, "frame.raw".into())),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(stream.next().await.unwrap().unwrap().len(), 64 * 1024);
        tokio::time::sleep(Duration::from_millis(
            deadline.saturating_sub(crate::now_ms() as u64) + 20,
        ))
        .await;
        assert!(blob_path(&g, &export.id).exists());
        assert!(stream.next().await.unwrap().is_err());
        assert!(stream.next().await.is_none());
    }
    #[test]
    fn source_and_destination_faults_report_boundaries_without_paths_or_secrets() {
        let u = source_url(
            "https://files.oaiusercontent.com/private-path?signature=secret-token",
            "https://gateway.example",
        )
        .unwrap();
        let source = SourceHttpFault::new(&u, 403);
        let source_json = source.diagnostic();
        assert_eq!(source_json["failure_boundary"], "source_http_authorization");
        assert_eq!(source_json["http_request_started"], true);
        assert_eq!(source_json["http_status"], 403);
        assert_eq!(source_json["destination_changed"], false);
        assert_eq!(source.to_string(), "source_http_authorization_rejected");

        let denied = classify_destination_error(
            anyhow!(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private destination path",
            )),
            "destination_staging",
            Some(false),
        );
        let destination = denied.downcast_ref::<DestinationPermissionFault>().unwrap();
        let destination_json = destination.diagnostic();
        assert_eq!(
            destination_json["failure_boundary"],
            "destination_file_permission"
        );
        assert_eq!(destination_json["side"], "destination");
        assert_eq!(destination_json["destination_changed"], false);
        assert_eq!(denied.to_string(), "destination_file_permission_denied");

        let publication = classify_destination_error(
            anyhow!(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private destination path",
            )),
            "destination_publication",
            None,
        );
        assert_eq!(
            publication
                .downcast_ref::<DestinationPermissionFault>()
                .unwrap()
                .diagnostic()["destination_changed"],
            Value::Null
        );

        let unrelated = classify_destination_error(
            anyhow!(std::io::Error::new(std::io::ErrorKind::NotFound, "missing")),
            "destination_staging",
            Some(false),
        );
        assert!(
            unrelated
                .downcast_ref::<DestinationPermissionFault>()
                .is_none()
        );

        for diagnostic in [source_json, destination_json] {
            let text = diagnostic.to_string();
            for private in [
                "private-path",
                "signature",
                "secret-token",
                "private destination path",
            ] {
                assert!(!text.contains(private));
            }
        }
    }

    #[test]
    fn source_diagnostics_distinguish_dns_permission_and_address_policy_without_urls() {
        let u = source_url(
            "https://files.oaiusercontent.com/private-path?signature=secret",
            "https://gateway.example",
        )
        .unwrap();
        for (kind, code) in [
            (std::io::ErrorKind::NotFound, "source_dns_lookup_failed"),
            (
                std::io::ErrorKind::PermissionDenied,
                "source_network_access_denied",
            ),
        ] {
            let error =
                SourceClientFault::dns(&u, &std::io::Error::new(kind, "sensitive OS error"));
            assert_eq!(error.code, code);
            let text = error.diagnostic().to_string();
            assert!(text.contains("files.oaiusercontent.com"));
            for private in ["private-path", "signature", "secret", "sensitive OS error"] {
                assert!(!text.contains(private));
                assert!(!error.to_string().contains(private));
            }
        }
        for (addresses, code) in [
            (vec![], "source_dns_no_addresses"),
            (
                vec!["127.0.0.1:443".parse().unwrap()],
                "source_address_policy_rejected",
            ),
            (
                vec![
                    "1.1.1.1:443".parse().unwrap(),
                    "198.18.0.1:443".parse().unwrap(),
                ],
                "source_address_policy_rejected",
            ),
        ] {
            let error = source_client_with_addresses(&u, &addresses).err().unwrap();
            assert_eq!(
                error.downcast_ref::<SourceClientFault>().unwrap().code,
                code
            );
        }
        assert!(source_client_with_addresses(&u, &["1.1.1.1:443".parse().unwrap()]).is_ok());
        assert_eq!(
            SourceClientFault::new(&u, "source_dns_timeout").diagnostic()["http_request_started"],
            false
        );
    }
    #[test]
    fn source_allowlist_and_private_addresses() {
        let own = "https://mcp.example:8443";
        for bad in [
            "http://files.oaiusercontent.com/f",
            "https://files.oaiusercontent.com.evil.test/f",
            "https://127.0.0.1/f",
            "https://files.oaiusercontent.com@evil.test/f",
            "https://files.oaiusercontent.com:22/f",
            "https://mcp.example/oauth/token",
        ] {
            assert!(source_url(bad, own).is_err(), "{bad}");
        }
        for good in [
            "https://files.oaiusercontent.com/f?signature=synthetic",
            "https://account.blob.core.windows.net/files/a",
            "https://mcp.example/files/a/b",
            "https://mcp.example:8443/files/a/b",
        ] {
            assert!(source_url(good, own).is_ok(), "{good}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("1.1.1.1".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
    #[test]
    fn binary_publish_default_no_clobber_and_hash_conflicts() {
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace {
            id: random(),
            device_id: random(),
            root: d.path().canonicalize().unwrap(),
        };
        let bytes = b"\0\xff\xfe\x01binary\0";
        let (stage, mut file) = Staged::new(&ws, &json!({"path":"assets/a.bin"})).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        stage.publish().unwrap();
        assert_eq!(std::fs::read(ws.root.join("assets/a.bin")).unwrap(), bytes);
        assert!(Staged::new(&ws, &json!({"path":"assets/a.bin"})).is_err());
        assert!(Staged::new(&ws, &json!({"path":"../outside"})).is_err());
        let (stage, mut file) = Staged::new(&ws, &json!({"path":"new.bin"})).unwrap();
        file.write_all(bytes).unwrap();
        drop(file);
        std::fs::write(ws.root.join("new.bin"), b"concurrent").unwrap();
        assert!(stage.publish().is_err());
        assert_eq!(
            std::fs::read(ws.root.join("new.bin")).unwrap(),
            b"concurrent"
        );
        assert!(verify_hash(Some(&hash(b"wrong")), &hash(bytes)).is_err());
    }
    #[test]
    fn bounded_snapshot_and_failed_stage_cleanup() {
        assert!(digest_reader(&b"0123456789"[..], 3, None).is_err());
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace {
            id: random(),
            device_id: random(),
            root: d.path().canonicalize().unwrap(),
        };
        let (stage, file) = Staged::new(&ws, &json!({"path":"a.bin"})).unwrap();
        drop(file);
        drop(stage);
        assert_eq!(std::fs::read_dir(&ws.root).unwrap().count(), 0);
    }
    #[cfg(unix)]
    #[test]
    fn capability_transfer_rejects_symlink_escape() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = Workspace {
            id: random(),
            device_id: random(),
            root: d.path().canonicalize().unwrap(),
        };
        std::os::unix::fs::symlink(outside.path(), ws.root.join("escape")).unwrap();
        assert!(Staged::new(&ws, &json!({"path":"escape/a.bin"})).is_err());
        std::fs::write(outside.path().join("target"), b"untouched").unwrap();
        std::os::unix::fs::symlink(outside.path().join("target"), ws.root.join("link")).unwrap();
        assert!(
            Staged::new(
                &ws,
                &json!({"path":"link","expected_version":hash(b"untouched")})
            )
            .is_err()
        );
    }
}
