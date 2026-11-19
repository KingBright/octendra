use anyhow::{Result, anyhow};
use rmcp::{
    RoleClient, ServiceExt, model::CallToolRequestParams, service::RunningService,
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};
use tokio::process::Command;

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Text,
    Image,
    Video,
    DeepResearch,
    Canvas,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct StartOptions {
    pub prompt: String,
    pub request_id: String,
    pub capability: Capability,
    pub timeout_ms: u64,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct StartSelectionOptions {
    pub prompt: String,
    pub request_id: String,
    pub capability: Capability,
    pub timeout_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Launch configuration for an independently hosted stdio server.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub env: BTreeMap<OsString, OsString>,
    pub state_dir: Option<PathBuf>,
}
impl ClientConfig {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            state_dir: None,
        }
    }
}

/// An initialized MCP stdio connection. Explicitly call `close` for graceful shutdown.
pub struct GeminiClient {
    service: RunningService<RoleClient, ()>,
}
impl GeminiClient {
    pub async fn connect(executable: impl AsRef<Path>) -> Result<Self> {
        Self::connect_with(ClientConfig::new(executable.as_ref())).await
    }
    pub async fn connect_with(config: ClientConfig) -> Result<Self> {
        if config
            .state_dir
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(anyhow!("state_dir must be absolute"));
        }
        let mut command = Command::new(&config.executable);
        command.args(&config.args).envs(&config.env);
        if let Some(path) = &config.state_dir {
            command.env("GEMINI_DESKTOP_MCP_STATE_DIR", path);
        }
        let transport = TokioChildProcess::new(command)?;
        Ok(Self {
            service: ().serve(transport).await?,
        })
    }
    pub async fn tools(&self) -> Result<Vec<String>> {
        Ok(self
            .service
            .list_tools(None)
            .await?
            .tools
            .into_iter()
            .map(|t| t.name.to_string())
            .collect())
    }
    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        if ![
            "capabilities",
            "usage",
            "focus",
            "start",
            "observe",
            "confirm",
            "continue",
            "collect",
            "cancel",
        ]
        .contains(&name)
        {
            return Err(anyhow!("unknown operation"));
        }
        let arguments = args
            .as_object()
            .ok_or_else(|| anyhow!("arguments must be an object"))?
            .clone();
        let r = self
            .service
            .call_tool(
                CallToolRequestParams::new(format!("gemini_{name}")).with_arguments(arguments),
            )
            .await?;
        if r.is_error == Some(true) {
            return Err(anyhow!("tool returned error: {:?}", r.content));
        }
        r.structured_content
            .ok_or_else(|| anyhow!("no structured content"))
    }
    pub async fn capabilities(&self) -> Result<Value> {
        self.call("capabilities", json!({})).await
    }
    /// Read usage only when Gemini Desktop exposes it; unknown values stay unknown.
    pub async fn focus(&self, operation_id: Option<&str>) -> Result<Value> {
        self.call("focus", json!({"operation_id":operation_id}))
            .await
    }
    pub async fn usage(&self) -> Result<Value> {
        self.call("usage", json!({})).await
    }
    pub async fn usage_with_refresh(&self, refresh: bool) -> Result<Value> {
        self.call("usage", json!({"refresh":refresh})).await
    }
    pub async fn start(&self, prompt: &str, request_id: &str) -> Result<Value> {
        self.call("start", json!({"prompt":prompt,"request_id":request_id}))
            .await
    }
    pub async fn start_with_options(&self, options: &StartOptions) -> Result<Value> {
        self.call("start", serde_json::to_value(options)?).await
    }
    pub async fn start_with_selection(&self, options: &StartSelectionOptions) -> Result<Value> {
        self.call("start", serde_json::to_value(options)?).await
    }
    /// Start a fresh chat after an explicitly selected, verified predecessor.
    pub async fn start_after(
        &self,
        previous_operation_id: &str,
        options: &StartOptions,
    ) -> Result<Value> {
        let mut arguments = serde_json::to_value(options)?;
        arguments["previous_operation_id"] = json!(previous_operation_id);
        self.call("start", arguments).await
    }
    pub async fn observe(&self, id: &str, resume: bool) -> Result<Value> {
        self.call("observe", json!({"operation_id":id,"resume":resume}))
            .await
    }
    pub async fn continue_turn(&self, id: &str, prompt: &str, request_id: &str) -> Result<Value> {
        self.call(
            "continue",
            json!({"operation_id":id,"prompt":prompt,"request_id":request_id}),
        )
        .await
    }
    pub async fn collect(&self, id: &str) -> Result<Value> {
        self.call("collect", json!({"operation_id":id})).await
    }
    pub async fn cancel(&self, id: &str) -> Result<Value> {
        self.call("cancel", json!({"operation_id":id})).await
    }
    pub async fn confirm(&self, id: &str, action_id: &str, action_version: &str) -> Result<Value> {
        self.call(
            "confirm",
            json!({"operation_id":id,"action_id":action_id,"action_version":action_version}),
        )
        .await
    }
    pub async fn close(mut self) -> Result<()> {
        self.service.close().await?;
        Ok(())
    }
}
