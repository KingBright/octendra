//! Read-only compatibility for retired task grants. New work uses connection authority.
use crate::{auth::Principal, gateway::Gateway};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};

// Existing history stays readable; pre-upgrade queued bindings stay held.
pub(crate) const PROTOCOL: u32 = 2;

pub(crate) async fn view(_g: &Gateway, p: &Principal, task: &str) -> Result<Value> {
    Ok(
        json!({"state":"connection_authorized","task_id":task,"scopes":p.scopes,
        "grants_authority":false,"task_id_role":"correlation_only",
        "authority":"current_account_device_and_local_policy",
        "terminal_authority":"local_user; not confined by code roots",
        "legacy_task_grants":"retired; old queued bindings remain held"}),
    )
}

pub(crate) async fn blocked(g: &Gateway, p: &Principal, id: &str) -> Result<bool> {
    let Some(binding) = g
        .store
        .get::<Value>("operation_task_authorization", id)
        .await?
    else {
        return Ok(false);
    };
    ensure!(binding["owner"] == p.owner, "operation owner mismatch");
    Ok(true)
}

/// Compatibility observations never renew a grant or replay a former rejection.
pub(crate) async fn resume(g: &Gateway, p: &Principal, args: &Value) -> Result<Value> {
    ensure!(
        args.get("request_id").is_some() != args.get("operation_id").is_some(),
        "invalid_arguments: supply request_id or operation_id"
    );
    let mut value = if let Some(id) = args.get("operation_id").and_then(Value::as_str) {
        g.result(p, id).await?
    } else {
        let id = args
            .get("request_id")
            .and_then(Value::as_str)
            .context("invalid_arguments: request_id")?;
        crate::observations::observe(g, p, &json!({"request_id":id,"wait_ms":0})).await?
    };
    value["task_authorization_retired"] = json!(true);
    value["replayed"] = json!(false);
    value["guidance"] = json!(
        "Observe the original outcome. Legacy queued bindings are held; new operations use the connection's current authority."
    );
    Ok(value)
}

pub(crate) fn routes(g: Gateway) -> Router {
    Router::new()
        .route("/status/task-authorization", get(retired).post(retired))
        .with_state(g)
}
async fn retired(State(g): State<Gateway>, headers: HeaderMap) -> Response {
    if !crate::gateway::status_session_valid(&g, &headers).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (StatusCode::GONE, Html("<p>任务授权表单已移除。操作复用 MCP 连接的既有权限；请在客户端撤销连接以停止后续访问。历史回执保留，旧排队操作不会自动执行。</p><a href='/status'>返回连接与访问</a>")).into_response()
}

#[cfg(test)]
#[path = "task_authorization_tests.rs"]
mod tests;
