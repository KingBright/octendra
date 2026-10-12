//! Oversized text edits use the existing transfer journal and Range receiver.
//! The stored Job, idempotency identity, CAS and edit journal remain authoritative.
use crate::{
    AgentConfig,
    files::{self, Workspace},
    gateway::{Gateway, Job},
    hash, payload_budget,
    progress::Progress,
    scheduler::WriteScopes,
    store::Store,
    transfer_journal::Journal,
};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub(crate) const FEATURE: &str = "code_payload_v1";
// Match the existing edit-journal and ordinary transfer budget. Per-file text limits remain unchanged.
const MAX: usize = 64 * 1024 * 1024;
const WIRE_CHUNK: usize = crate::transfer_receiver::CHUNK;

pub(crate) fn supported(features: &Value) -> bool {
    features["names"]
        .as_array()
        .is_some_and(|names| names.iter().any(|v| v == FEATURE))
}
pub(crate) fn admit(job: &Job, enabled: bool) -> Result<()> {
    ensure!(
        job.arguments.get("code_payload").is_none(),
        "invalid_arguments: reserved transport field"
    );
    if payload_budget::poll_bytes(job)? <= payload_budget::POLL_BYTES {
        return Ok(());
    }
    ensure!(
        enabled && job.tool == "code_apply_edits",
        "payload_too_large: selected Agent cannot receive encoded request"
    );
    ensure!(
        serde_json::to_vec(&job.arguments)?.len() <= MAX,
        "payload_too_large: text edit exceeds transport budget"
    );
    Ok(())
}
pub(crate) fn envelope(job: &Job, enabled: bool, reconcile_only: bool) -> Result<Value> {
    if payload_budget::poll_bytes(job)? <= payload_budget::POLL_BYTES {
        return Ok(json!({"job":job}));
    }
    // Old dispatched jobs remain unknown until an upgraded Agent supplies its
    // own durable negative acceptance receipt. Never infer it at the Gateway.
    if !enabled || job.tool != "code_apply_edits" {
        return Ok(
            json!({"job":null,"blocked_operation_id":job.id,"error_code":"payload_too_large","execution_state":"unknown"}),
        );
    }
    let data = serde_json::to_vec(&job.arguments)?;
    ensure!(
        data.len() <= MAX,
        "payload_too_large: text edit exceeds transport budget"
    );
    let mut wire = job.clone();
    wire.arguments = json!({"workspace_id":job.arguments["workspace_id"],"code_payload":{
        "protocol":1,"size":data.len(),"sha256":hash(&data),"job_sha256":hash(serde_json::to_vec(job)?),"reconcile_only":reconcile_only}});
    payload_budget::admit(&wire)?;
    Ok(json!({"job":wire}))
}
pub(crate) fn routes(g: Gateway) -> Router {
    Router::new()
        .route("/device/code-payload/{id}", get(serve))
        .with_state(g)
}
async fn serve(State(g): State<Gateway>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let result = async {
        uuid::Uuid::parse_str(&id)?;
        let device = g.device(&headers).await?;
        ensure!(
            g.config
                .devices
                .iter()
                .any(|d| d.id == device && d.scopes.iter().any(|s| s == "code:write")),
            "code scope revoked"
        );
        let request: String =
            sqlx::query_scalar("SELECT request FROM jobs WHERE id=? AND device=?")
                .bind(&id)
                .bind(&device)
                .fetch_optional(&g.store.pool)
                .await?
                .context("payload unavailable")?;
        let job: Job = serde_json::from_str(&request)?;
        ensure!(
            job.tool == "code_apply_edits" && job.owner == g.config.owner,
            "payload scope mismatch"
        );
        let principal = crate::auth::Principal {
            owner: job.owner.clone(),
            scopes: vec!["code:write".into()],
        };
        ensure!(
            !crate::task_authorization::blocked(&g, &principal, &id).await?,
            "task authorization revoked"
        );
        let bytes = serde_json::to_vec(&job.arguments)?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX,
            "payload size rejected"
        );
        let etag = format!("\"{}\"", hash(&bytes));
        let offset = match headers.get(header::RANGE) {
            None => 0,
            Some(value) => value
                .to_str()?
                .strip_prefix("bytes=")
                .and_then(|s| s.strip_suffix('-'))
                .context("invalid range")?
                .parse::<usize>()?,
        };
        ensure!(offset < bytes.len(), "invalid offset");
        if let Some(value) = headers.get(header::IF_RANGE) {
            ensure!(value == etag.as_str(), "payload identity changed");
        }
        let end = offset.saturating_add(WIRE_CHUNK).min(bytes.len());
        Ok::<_, anyhow::Error>(
            (
                StatusCode::PARTIAL_CONTENT,
                [
                    (header::CONTENT_TYPE, "application/json".to_owned()),
                    (
                        header::CONTENT_RANGE,
                        format!("bytes {}-{}/{}", offset, end - 1, bytes.len()),
                    ),
                    (header::CONTENT_LENGTH, (end - offset).to_string()),
                    (header::ETAG, etag),
                    (header::CACHE_CONTROL, "no-store".to_owned()),
                ],
                Body::from(bytes[offset..end].to_vec()),
            )
                .into_response(),
        )
    }
    .await;
    result.unwrap_or_else(|error| {
        let status = if error.is::<sqlx::Error>() {
            StatusCode::SERVICE_UNAVAILABLE
        } else if error.is::<std::io::Error>() {
            StatusCode::INTERNAL_SERVER_ERROR
        } else {
            StatusCode::FORBIDDEN
        };
        status.into_response()
    })
}
pub(crate) async fn hydrate(
    config: &AgentConfig,
    store: &Store,
    scopes: Arc<WriteScopes>,
    wire: &Job,
) -> Result<Job> {
    let metadata = &wire.arguments["code_payload"];
    let size = metadata["size"].as_u64().context("invalid payload size")?;
    let sha = files::text(metadata, "sha256")?;
    ensure!(
        metadata["protocol"] == 1
            && size > 0
            && size <= MAX as u64
            && crate::transfers::valid_hash(sha),
        "invalid payload metadata"
    );
    let ws_id = files::text(&wire.arguments, "workspace_id")?;
    let ws: Workspace = store
        .get("workspace", ws_id)
        .await?
        .context("workspace not found")?;
    ensure!(
        ws.device_id == config.device_id && wire.device_id == config.device_id,
        "wrong device"
    );
    let mut transfer = wire.clone();
    transfer.arguments = json!({"path":".remote-hosts-code-payload","sha256":sha,"max_bytes":size,"workspace_id":ws_id});
    let mut journal = Journal::load_or_create(config, &ws, &transfer, store, scopes).await?;
    ensure!(
        journal.expires_at > crate::now() && !journal.data_cleaned,
        "payload checkpoint expired"
    );
    let raw = crate::transfer_journal::open_data(&journal.data_path(config), true)?;
    let (raw, digest) =
        crate::transfer_journal::restore_prefix(raw, journal.offset, &journal.prefix_sha256)?;
    let mut output = tokio::fs::File::from_std(raw);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {}", config.device_token).parse()?,
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let url = reqwest::Url::parse(&format!(
        "{}/device/code-payload/{}",
        config.gateway_url, wire.id
    ))?;
    let progress = Progress::new(&wire.id);
    crate::durable_transfer::fetch(
        &client,
        &url,
        &mut output,
        &transfer,
        &progress,
        store,
        &mut journal,
        digest,
    )
    .await?;
    drop(output);
    ensure!(
        journal.offset == size && journal.content_sha256.as_deref() == Some(sha),
        "payload length or hash mismatch"
    );
    let data = tokio::fs::read(journal.data_path(config)).await?;
    ensure!(
        data.len() as u64 == size && hash(&data) == sha,
        "payload changed after verification"
    );
    let mut job = wire.clone();
    job.arguments = serde_json::from_slice(&data)?;
    ensure!(
        job.arguments["workspace_id"] == ws_id && job.arguments.get("code_payload").is_none(),
        "payload workspace identity mismatch"
    );
    ensure!(
        hash(serde_json::to_vec(&job)?) == metadata["job_sha256"],
        "payload operation identity mismatch"
    );
    Ok(job)
}
pub(crate) async fn cleanup(config: &AgentConfig, store: &Store, id: &str) -> Result<()> {
    if let Some(mut journal) = store.get::<Journal>("transfer_local", id).await? {
        let path = journal.data_path(config);
        match tokio::fs::remove_file(path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        journal.phase = "completed".into();
        journal.data_cleaned = true;
        journal.save(store).await?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    fn job(content: String) -> Job {
        Job {
            id: uuid::Uuid::new_v4().to_string(),
            device_id: uuid::Uuid::new_v4().to_string(),
            owner: "test".into(),
            tool: "code_apply_edits".into(),
            arguments: json!({"workspace_id":"device:workspace","files":[{"path":"source.rs","action":"create","expected_version":"absent","content":content}]}),
        }
    }
    #[test]
    fn large_text_uses_small_envelope_with_exact_identity() {
        let original = job("文\n\"".repeat(160000));
        assert!(admit(&original, false).is_err());
        admit(&original, true).unwrap();
        let data = serde_json::to_vec(&original.arguments).unwrap();
        let wire = envelope(&original, true, false).unwrap();
        assert!(serde_json::to_vec(&wire).unwrap().len() < 2048);
        assert_eq!(wire["job"]["id"], original.id);
        assert_eq!(
            wire["job"]["arguments"]["code_payload"]["sha256"],
            hash(data)
        );
        assert_eq!(
            wire["job"]["arguments"]["code_payload"]["reconcile_only"],
            false
        );
        assert_eq!(
            envelope(&original, true, true).unwrap()["job"]["arguments"]["code_payload"]["reconcile_only"],
            true
        );
        assert!(envelope(&original, false, true).unwrap()["job"].is_null());
    }
    #[test]
    fn small_delta_is_unchanged_and_reserved_fields_are_refused() {
        let mut original = job("small".into());
        assert_eq!(
            envelope(&original, true, false).unwrap(),
            json!({"job":original})
        );
        original.arguments["code_payload"] = json!({"protocol":1});
        assert!(admit(&original, true).is_err());
    }
}
