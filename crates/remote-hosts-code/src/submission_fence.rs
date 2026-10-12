//! Close a never-enqueued source submission under the same writer boundary as admission.
//! This preserves existing operations and prevents a delayed old request from racing recovery.
use crate::{auth::Principal, files, gateway::Gateway, hash, now};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::{Sqlite, Transaction};

pub(crate) fn identity(owner: &str, workspace: &str, key: &str) -> Result<String> {
    let (device, local) = workspace.split_once(':').context("invalid workspace id")?;
    uuid::Uuid::parse_str(device)?;
    uuid::Uuid::parse_str(local)?;
    ensure!(
        !key.is_empty() && key.len() <= 128,
        "invalid_arguments: original idempotency key"
    );
    Ok(hash(format!(
        "{owner}:{device}:code_apply_edits:{workspace}:{key}"
    )))
}
pub(crate) async fn require_open(tx: &mut Transaction<'_, Sqlite>, idem: &str) -> Result<()> {
    let closed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM kv WHERE kind='submission_fence' AND key=?)",
    )
    .bind(idem)
    .fetch_one(&mut **tx)
    .await?;
    ensure!(!closed, "submission_closed_not_enqueued");
    Ok(())
}
pub(crate) async fn close(g: &Gateway, p: &Principal, args: &Value) -> Result<Value> {
    let workspace = files::text(args, "workspace_id")?;
    let key = files::text(args, "original_idempotency_key")?;
    let resolution_key = files::text(args, "idempotency_key")?;
    ensure!(
        !resolution_key.is_empty() && resolution_key.len() <= 128,
        "invalid_arguments: resolution idempotency key"
    );
    let idem = identity(&p.owner, workspace, key)?;
    let resolution_id = hash(format!("{}:submission_close:{resolution_key}", p.owner));
    let fingerprint = hash(serde_json::to_vec(
        &json!({"owner":p.owner,"workspace":workspace,"original_key":key}),
    )?);
    let device = workspace.split_once(':').context("invalid workspace id")?.0;
    ensure!(
        g.config
            .devices
            .iter()
            .any(|d| d.id == device && d.scopes.iter().any(|s| s == "code:write")),
        "device_scope_denied: code:write"
    );
    let binding =
        crate::task_authorization::requested(g, p, "code_apply_edits", args, device).await?;
    let mut tx = g.store.pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(binding) = &binding {
        crate::task_authorization::check_in_transaction(&mut tx, binding).await?;
    }
    let saved: Option<String> =
        sqlx::query_scalar("SELECT value FROM kv WHERE kind='submission_close_dedup' AND key=?")
            .bind(&resolution_id)
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(saved) = saved {
        let saved: Value = serde_json::from_str(&saved)?;
        ensure!(
            saved["fingerprint"] == fingerprint,
            "idempotency_conflict: resolution target changed"
        );
        let mut result = saved["result"].clone();
        result["duplicate"] = json!(true);
        tx.rollback().await?;
        return Ok(result);
    }
    // A racing admission that already owns the writer commits first. Existing
    // operations retain their complete state, result, timing and semantic guard.
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM jobs WHERE idem=?")
        .bind(&idem)
        .fetch_optional(&mut *tx)
        .await?;
    let result = if let Some(id) = existing {
        json!({"state":"existing_operation","operation_id":id,"submission_closed":false,"replayed":false,"next_action":"operation_get"})
    } else {
        let old: Option<String> =
            sqlx::query_scalar("SELECT value FROM kv WHERE kind='submission_fence' AND key=?")
                .bind(&idem)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(old) = old {
            let mut result: Value = serde_json::from_str(&old)?;
            result["duplicate"] = json!(true);
            result
        } else {
            let result = json!({"state":"closed_not_enqueued","operation_id":null,"workspace_id":workspace,
                "original_submission_id":idem,"original_tool":"code_apply_edits","replayed":false,"duplicate":false,
                "submission_receipt":{"protocol":1,"durable":true,"operation_id":null,"execution_state":"not_started",
                    "acceptance":"not_enqueued","key_sealed":true,"closed_at":now()}});
            sqlx::query("INSERT INTO kv VALUES('submission_fence',?,?,?)")
                .bind(&idem)
                .bind(result.to_string())
                .bind(i64::MAX)
                .execute(&mut *tx)
                .await?;
            result
        }
    };
    let anchor = json!({"fingerprint":fingerprint,"result":result});
    sqlx::query("INSERT INTO kv VALUES('submission_close_dedup',?,?,?)")
        .bind(&resolution_id)
        .bind(anchor.to_string())
        .bind(i64::MAX)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(result)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_matches_original_gateway_idempotency_contract() {
        assert_eq!(
            identity(
                "jinliang",
                "02f29fa0-48c1-4e31-a345-90aa88467323:1fe3c280-d0bc-497c-af9a-ba0b2e08634f",
                "ho5-r6-source-sync-batch-26-20261012"
            )
            .unwrap(),
            "a0295fceaa7f35a555ceda274cb78a1925b6562d1536e94a0df29e83e02518fd"
        );
        assert!(identity("owner", "not-a-workspace", "key").is_err());
    }
    #[tokio::test]
    async fn a_committed_fence_blocks_delayed_admission_without_creating_a_job() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(tmp.path()).await.unwrap();
        store.install_gateway_schema().await.unwrap();
        store
            .put(
                "submission_fence",
                "idem",
                &json!({"sealed":true}),
                i64::MAX,
            )
            .await
            .unwrap();
        let mut tx = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        assert!(require_open(&mut tx, "idem").await.is_err());
        require_open(&mut tx, "another-idem").await.unwrap();
        tx.rollback().await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
