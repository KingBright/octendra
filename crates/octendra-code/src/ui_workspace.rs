//! Resolve UI work to one persistent execution environment. The saved observation,
//! not a mutable device selection, owns the destination of subsequent input.
use crate::{GatewayConfig, auth::Principal, gateway::Gateway};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiWorkspace {
    /// New UUID when the persistent guest is replaced, not when it reconnects.
    pub id: String,
    pub name: String,
    pub host_device_id: String,
    pub execution_device_id: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Selection {
    #[default]
    Isolated,
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Isolation {
    FullVm,
    HostSession,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionContext {
    pub id: String,
    pub name: String,
    pub host_device_id: String,
    pub execution_device_id: String,
    isolation: Isolation,
}
impl ExecutionContext {
    pub(crate) fn is_full_vm(&self) -> bool {
        self.isolation == Isolation::FullVm
    }
}

impl UiWorkspace {
    fn context(&self) -> ExecutionContext {
        ExecutionContext {
            id: self.id.clone(),
            name: self.name.clone(),
            host_device_id: self.host_device_id.clone(),
            execution_device_id: self.execution_device_id.clone(),
            isolation: Isolation::FullVm,
        }
    }
}

pub(crate) fn validate(config: &GatewayConfig) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    let mut names = std::collections::HashSet::new();
    let mut devices = std::collections::HashSet::new();
    for workspace in &config.ui_workspaces {
        uuid::Uuid::parse_str(&workspace.id)?;
        ensure!(ids.insert(&workspace.id), "duplicate UI workspace identity");
        ensure!(
            !config.devices.iter().any(|d| d.id == workspace.id),
            "ambiguous UI workspace identity"
        );
        ensure!(
            !workspace.name.trim().is_empty() && workspace.name.len() <= 256
                && names.insert(workspace.name.to_lowercase())
                && !config.devices.iter().any(|d| d.id == workspace.name || d.name.eq_ignore_ascii_case(&workspace.name)),
            "invalid or ambiguous UI workspace name"
        );
        ensure!(
            workspace.host_device_id != workspace.execution_device_id,
            "a full VM workspace requires a distinct guest device"
        );
        for id in [&workspace.host_device_id, &workspace.execution_device_id] {
            ensure!(
                config.devices.iter().any(|d| &d.id == id),
                "UI workspace device not registered"
            );
            ensure!(devices.insert(id), "ambiguous UI workspace device binding");
        }
    }
    Ok(())
}

fn device<'a>(config: &'a GatewayConfig, raw: &str) -> Option<&'a crate::DeviceRegistration> {
    config
        .devices
        .iter()
        .find(|d| d.id == raw || d.name.eq_ignore_ascii_case(raw))
}

fn observation_key(owner: &str, observation: &str) -> String {
    format!("{}:{observation}", crate::hash(owner))
}

pub(crate) async fn resolve(
    gateway: &Gateway,
    principal: &Principal,
    tool: &str,
    args: &mut Value,
) -> Result<ExecutionContext> {
    let raw = crate::files::text(args, "device")?;
    let context = if tool == "ui_act" {
        let id = crate::files::text(args, "observation_id")?;
        let saved: ExecutionContext = gateway
            .store
            .get(
                "ui_observation_route",
                &observation_key(&principal.owner, id),
            )
            .await?
            .context("ui_observation_missing: observe the selected workspace again")?;
        let selected = device(&gateway.config, raw);
        ensure!(
            raw == saved.id
                || raw.eq_ignore_ascii_case(&saved.name)
                || selected.is_some_and(
                    |d| d.id == saved.host_device_id || d.id == saved.execution_device_id
                ),
            "ui_observation_device_mismatch"
        );
        ensure!(
            device(&gateway.config, &saved.execution_device_id).is_some(),
            "UI execution device no longer registered"
        );
        saved
    } else {
        let selection: Selection = args
            .get("context")
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()?
            .unwrap_or_default();
        let selected = device(&gateway.config, raw);
        match selection {
            Selection::Isolated => gateway.config.ui_workspaces.iter().find(|w|
                w.id == raw || w.name.eq_ignore_ascii_case(raw)
                    || selected.is_some_and(|d| d.id == w.host_device_id || d.id == w.execution_device_id))
                .map(UiWorkspace::context)
                .context("ui_workspace_unavailable: no isolated workspace is configured for this device; host input was not selected")?,
            Selection::Host => {
                let d = selected.context("unknown device")?;
                ExecutionContext { id: format!("host:{}", d.id), name: d.name.clone(),
                    host_device_id: d.id.clone(), execution_device_id: d.id.clone(), isolation: Isolation::HostSession }
            }
        }
    };
    let object = args.as_object_mut().context("invalid UI arguments")?;
    // Selection is resolved here. Older guest agents need only their real device ID.
    object.remove("context");
    object.insert("device".into(), json!(context.execution_device_id));
    Ok(context)
}

pub(crate) fn require_current(config: &GatewayConfig, context: &ExecutionContext) -> Result<()> {
    if context.isolation == Isolation::FullVm {
        ensure!(
            config.ui_workspaces.iter().any(|w| w.id == context.id
                && w.host_device_id == context.host_device_id
                && w.execution_device_id == context.execution_device_id),
            "ui_execution_context_changed: observe the selected workspace again"
        );
    }
    Ok(())
}

pub(crate) async fn discovery(gateway: &Gateway) -> Result<Value> {
    let mut workspaces = Vec::new();
    for workspace in &gateway.config.ui_workspaces {
        let online: Option<Value> = gateway
            .store
            .get("online", &workspace.execution_device_id)
            .await?;
        let connected = online
            .as_ref()
            .and_then(|v| v["last_seen"].as_i64())
            .is_some_and(|t| (0..45).contains(&(crate::now() - t)));
        workspaces.push(json!({"execution_context":workspace.context(),
            "status":if connected {"agent_connected"} else {"agent_offline"},
            "isolation_evidence":"operator_configuration; not guest graphical acceptance",
            "host_fallback":false}));
    }
    Ok(json!(workspaces))
}

/// Called on every durable result read, including operation_get after reconnect.
pub(crate) async fn decorate(
    gateway: &Gateway,
    job: &crate::gateway::Job,
    value: &mut Value,
) -> Result<()> {
    let Some(context) = &job.ui_context else {
        return Ok(());
    };
    ensure!(
        context.execution_device_id == job.device_id,
        "UI operation context mismatch"
    );
    if let Some(object) = value.as_object_mut() {
        let mut description = json!(context);
        description["isolation_evidence"] = json!("gateway_configuration; not runtime attestation");
        object.insert("execution_context".into(), description);
    }
    let mut inserted = false;
    for observation in [
        value.get("observation_id"),
        value.pointer("/observation/observation_id"),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(id) = observation
            .as_str()
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        {
            let key = observation_key(&job.owner, id);
            let changed =
                sqlx::query("INSERT OR IGNORE INTO kv VALUES('ui_observation_route',?,?,?)")
                    .bind(&key)
                    .bind(serde_json::to_string(&context)?)
                    .bind(i64::MAX)
                    .execute(&gateway.store.pool)
                    .await?
                    .rows_affected();
            inserted |= changed > 0;
            let original: ExecutionContext = gateway
                .store
                .get("ui_observation_route", &key)
                .await?
                .context("UI observation route missing")?;
            ensure!(
                original.id == context.id
                    && original.execution_device_id == context.execution_device_id
                    && original.host_device_id == context.host_device_id
                    && original.isolation == context.isolation,
                "UI observation identity collision"
            );
        }
    }
    // Bounded observation cache, not a task deadline. Durable operation evidence
    // remains available when a caller needs a fresh observation after eviction.
    if inserted {
        let prefix = format!("{}:%", crate::hash(&job.owner));
        sqlx::query("DELETE FROM kv WHERE kind='ui_observation_route' AND key IN (SELECT key FROM kv WHERE kind='ui_observation_route' AND key LIKE ? ORDER BY rowid DESC LIMIT -1 OFFSET 256)")
            .bind(prefix).execute(&gateway.store.pool).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
