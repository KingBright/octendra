//! Explicit owner reconciliation of a durable unknown outcome. Never dispatches work.
use crate::{
    auth::Principal,
    diagnostics, files,
    gateway::{Gateway, Job},
    now,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

pub(crate) async fn resolve(g: &Gateway, p: &Principal, args: &Value) -> Result<Value> {
    ensure!(p.owner == g.config.owner, "unknown owner");
    crate::tools::require_account_scopes(p, "outcome_resolve")?;
    let id = files::text(args, "operation_id")?;
    uuid::Uuid::parse_str(id).context("invalid_arguments: operation id")?;
    let resolution = files::text(args, "resolution")?;
    ensure!(
        matches!(resolution, "verified_applied" | "verified_not_applied"),
        "invalid_arguments: outcome resolution"
    );
    let mut tx = g.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT request,result FROM jobs WHERE id=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let (request, result) = row.context("operation_unavailable")?;
    let original: Job = serde_json::from_str(&request)?;
    ensure!(
        original.owner == p.owner,
        "access_denied: operation owner mismatch"
    );
    let device = g
        .config
        .devices
        .iter()
        .find(|d| d.id == original.device_id)
        .context("device permission revoked for original operation")?;
    crate::tools::require_account_scopes(p, &original.tool)?;
    crate::tools::require_device_scopes(device, "outcome_resolve")?;
    crate::tools::require_device_scopes(device, &original.tool)?;
    let saved: Option<String> =
        sqlx::query_scalar("SELECT value FROM kv WHERE kind='outcome_resolution' AND key=?")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(saved) = saved {
        let saved: Value = serde_json::from_str(&saved)?;
        ensure!(
            saved["owner"] == p.owner,
            "access_denied: resolution owner mismatch"
        );
        ensure!(
            saved["resolution"] == resolution,
            "outcome_resolution_conflict"
        );
        tx.rollback().await?;
        return Ok(
            json!({"state":"resolved","operation_id":id,"resolution":resolution,
            "replayed":false,"duplicate":true}),
        );
    }
    let result: Value = result
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .context("outcome_evidence_unavailable")?;
    ensure!(diagnostics::outcome_unknown(&result), "outcome_not_unknown");
    // Older receipt commits incorrectly removed guards for structured unknowns.
    // Reconcile that original result without reconstructing or clearing another job's guard.
    let released = sqlx::query("DELETE FROM semantic_guards WHERE operation_id=?")
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    sqlx::query("DELETE FROM kv WHERE kind='operation_semantic' AND key=?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let evidence = json!({"resolution":resolution,"resolved_at":now(),"owner":p.owner,
        "guard_was_present":released > 0,"original_result_preserved":true});
    sqlx::query("INSERT INTO kv VALUES('outcome_resolution',?,?,?)")
        .bind(id)
        .bind(evidence.to_string())
        .bind(i64::MAX)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(
        json!({"state":"resolved","operation_id":id,"resolution":resolution,"replayed":false,
        "duplicate":false,"guard_was_present":released > 0,"original_result_preserved":true}),
    )
}
