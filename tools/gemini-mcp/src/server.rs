use crate::ops;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
#[derive(Clone, Default)]
pub struct GeminiServer;
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Capabilities {
    #[serde(default)]
    refresh: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Usage {
    refresh: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Focus {
    operation_id: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Start {
    prompt: String,
    request_id: String,
    timeout_ms: Option<u64>,
    capability: Option<String>,
    previous_operation_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Observe {
    operation_id: String,
    resume: Option<bool>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Id {
    operation_id: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Confirm {
    operation_id: String,
    action_id: String,
    action_version: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Continue {
    operation_id: String,
    prompt: String,
    request_id: String,
    timeout_ms: Option<u64>,
    model: Option<String>,
    effort: Option<String>,
}
fn schema<T: JsonSchema>() -> Arc<serde_json::Map<String, Value>> {
    Arc::new(
        serde_json::to_value(schemars::schema_for!(T))
            .unwrap()
            .as_object()
            .unwrap()
            .clone(),
    )
}
fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "gemini_capabilities",
            "Read native capabilities, discovered models, and readiness",
            schema::<Capabilities>(),
        ),
        Tool::new(
            "gemini_focus",
            "Explicitly bring Gemini to the foreground; an active lease requires its matching operation_id. Does not submit or resume generation.",
            schema::<Focus>(),
        ),
        Tool::new(
            "gemini_usage",
            "Read Gemini Desktop usage limits; refresh=true may navigate settings only when the device is idle",
            schema::<Usage>(),
        ),
        Tool::new(
            "gemini_start",
            "Start a text or supported native capability with a stable UUID",
            schema::<Start>(),
        ),
        Tool::new(
            "gemini_observe",
            "Read state; resume=true observes the original turn",
            schema::<Observe>(),
        ),
        Tool::new(
            "gemini_confirm",
            "Confirm the exact pending owned research plan action and version",
            schema::<Confirm>(),
        ),
        Tool::new(
            "gemini_continue",
            "Start a new turn following a completed bound turn",
            schema::<Continue>(),
        ),
        Tool::new(
            "gemini_collect",
            "Collect the completed text of an operation",
            schema::<Id>(),
        ),
        Tool::new(
            "gemini_cancel",
            "Request native cancellation of the owned turn",
            schema::<Id>(),
        ),
    ]
}
fn decode<T: for<'de> Deserialize<'de>>(v: Value) -> anyhow::Result<T> {
    Ok(serde_json::from_value(v)?)
}
fn dispatch(name: &str, v: Value) -> anyhow::Result<Value> {
    match name {
        "gemini_capabilities" => {
            let a: Capabilities = decode(v)?;
            let mut native = ops::native("capabilities", None)?;
            if a.refresh.unwrap_or(false)
                && let Ok(fresh) = ops::native("discover_models", None)
                && let Some(models) = fresh.get("discovered_models")
                && let Some(obj) = native.as_object_mut()
            {
                obj.insert("discovered_models".into(), models.clone());
            }
            let mut available_models: Vec<Value> = native
                .get("discovered_models")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if available_models.is_empty() {
                let default_efforts = json!(["low", "medium", "high", "xhigh", "max"]);
                available_models = vec![
                    json!({"id":"gemini-4-carbon","label":"Gemini 4 Carbon (gemini-4-carbon)","default_effort":"high","supported_efforts":default_efforts}),
                    json!({"id":"gemini-4-argon","label":"Gemini 4 Argon (gemini-4-argon)","default_effort":"medium","supported_efforts":default_efforts}),
                    json!({"id":"gemini-4-hydrogen","label":"Gemini 4 Hydrogen (gemini-4-hydrogen)","default_effort":"medium","supported_efforts":default_efforts}),
                ];
            }
            if !available_models
                .iter()
                .any(|m| m.get("id").and_then(Value::as_str) == Some("gemini-desktop-task"))
            {
                available_models.push(json!({
                    "id": "gemini-desktop-task",
                    "label": "跟随 Gemini.app 当前会话设置 (gemini-desktop-task)",
                    "default_effort": Value::Null,
                    "supported_efforts": ["low", "medium", "high", "xhigh", "max"],
                }));
            }
            let supported_models: Vec<String> = available_models
                .iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect();
            Ok(json!({
                "native": native,
                "readiness": ops::native("probe", None)?,
                "public_capability_mapping": {"text":"chat","image":"image","video":"video","deep_research":"deep_research","canvas":"canvas"},
                "supported_models": supported_models,
                "available_models": available_models,
                "supported_efforts": ["minimal","low","medium","high","xhigh","max"],
                "human_confirmation": {"status":"conditional","reason":"exact_owned_research_plan_only; account_and_permission_controls_require_human"}
            }))
        }
        "gemini_focus" => {
            let a: Focus = decode(v)?;
            ops::focus(a.operation_id.as_deref())
        }
        "gemini_usage" => {
            let a: Usage = decode(v)?;
            let command = if a.refresh.unwrap_or(false) {
                "usage_refresh"
            } else {
                "usage"
            };
            let native = ops::native(command, None)?;
            let status = native["status"].as_str().unwrap_or("unknown");
            if native["version"] != 1
                || !matches!(status, "unknown" | "observed_unverified")
                || native["source"] != "gemini_desktop_usage_limits_current_view"
            {
                return Ok(
                    json!({"status":"unknown","reason":"usage_receipt_unavailable","source":"gemini_desktop","captured_at":null,"staleness":"unknown","scope":"unspecified","remaining_credits":null,"limit":null,"reset_label":null}),
                );
            }
            let mut result = native;
            result["captured_at"] = result["collected_at"].clone();
            result["staleness"] = json!("unknown");
            result["limit"] = Value::Null;
            result["provider"] = json!("google");
            result["product"] = json!("gemini_desktop");
            result["account_scope"] = Value::Null;
            result["remaining_percent"] = result["percent_used"]
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
                .map(|v| json!(100.0 - v))
                .unwrap_or(Value::Null);
            if let Some(buckets) = result["buckets"].as_array_mut() {
                for bucket in buckets {
                    bucket["remaining_percent"] = bucket["percent_used"]
                        .as_f64()
                        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
                        .map(|v| json!(100.0 - v))
                        .unwrap_or(Value::Null);
                    bucket["account_scope"] = Value::Null;
                    bucket["limit_id"] = Value::Null;
                }
            }
            Ok(result)
        }
        "gemini_start" => {
            let a: Start = decode(v)?;
            let (norm_model, norm_effort) =
                ops::normalize_model_and_effort(a.model.as_deref(), a.effort.as_deref())?;
            let raw_cap = a.capability.as_deref().unwrap_or("text");
            let base_cap = if raw_cap == "text" { "chat" } else { raw_cap };
            let (promoted_cap, effective_prompt) =
                ops::normalize_slash_capability(&a.prompt, base_cap);
            let native = promoted_cap.as_str();
            let cap = if native == "chat" { "text" } else { native };
            if !["chat", "image", "video", "deep_research", "canvas"].contains(&native) {
                anyhow::bail!("invalid capability")
            };
            if native != "chat" {
                let timeout = a.timeout_ms.filter(|&ms| ms > 0).unwrap_or(86_400_000);
                if let Some(existing) = ops::existing_start_with_selection(
                    &effective_prompt,
                    &a.request_id,
                    timeout,
                    native,
                    a.previous_operation_id.as_deref(),
                    norm_model.as_deref(),
                    norm_effort.as_deref(),
                )? {
                    return Ok(existing);
                }
                if let Some(previous) = a.previous_operation_id.as_deref() {
                    if let Some(blocked) = ops::precheck_fresh_after(previous)? {
                        if let Some(existing) = ops::existing_start_with_selection(
                            &effective_prompt,
                            &a.request_id,
                            timeout,
                            native,
                            a.previous_operation_id.as_deref(),
                            norm_model.as_deref(),
                            norm_effort.as_deref(),
                        )? {
                            return Ok(existing);
                        }
                        return Ok(blocked);
                    }
                }
                let caps = ops::native("capabilities", None).ok();
                let entry = caps.as_ref().map(|c| &c["capabilities"][native]);
                let denial = match entry {
                    None | Some(Value::Null) => Some(
                        json!({"status":"blocked","reason":"native_capability_unavailable","capability":cap}),
                    ),
                    Some(entry)
                        if !matches!(
                            entry["implementation"].as_str(),
                            Some(
                                "implemented"
                                    | "implemented_unverified"
                                    | "experimental_unverified"
                            )
                        ) =>
                    {
                        Some(json!({"status":"unsupported","capability":cap,"native":entry}))
                    }
                    _ => None,
                };
                if let Some(denial) = denial {
                    // Another caller may have persisted this UUID during the probe.
                    if let Some(existing) = ops::existing_start_with_selection(
                        &effective_prompt,
                        &a.request_id,
                        timeout,
                        native,
                        a.previous_operation_id.as_deref(),
                        norm_model.as_deref(),
                        norm_effort.as_deref(),
                    )? {
                        return Ok(existing);
                    }
                    return Ok(denial);
                }
            }
            if let Some(previous) = a.previous_operation_id.as_deref() {
                ops::start_after_with_selection(
                    &effective_prompt,
                    Some(&a.request_id),
                    a.timeout_ms.filter(|&ms| ms > 0).unwrap_or(86_400_000),
                    native,
                    previous,
                    norm_model.as_deref(),
                    norm_effort.as_deref(),
                )
            } else {
                ops::start_with_selection(
                    &effective_prompt,
                    Some(&a.request_id),
                    a.timeout_ms.filter(|&ms| ms > 0).unwrap_or(86_400_000),
                    native,
                    None,
                    norm_model.as_deref(),
                    norm_effort.as_deref(),
                )
            }
        }
        "gemini_observe" => {
            let a: Observe = decode(v)?;
            if a.resume.unwrap_or(false) {
                ops::action(&a.operation_id, "resume")
            } else {
                ops::observe(&a.operation_id)
            }
        }
        "gemini_confirm" => {
            let a: Confirm = decode(v)?;
            ops::confirm(&a.operation_id, &a.action_id, &a.action_version)
        }
        "gemini_continue" => {
            let a: Continue = decode(v)?;
            let (norm_model, norm_effort) =
                ops::normalize_model_and_effort(a.model.as_deref(), a.effort.as_deref())?;
            let old = ops::observe(&a.operation_id)?;
            let cap = old["capability"].as_str().unwrap_or("chat");
            ops::start_with_selection(
                &a.prompt,
                Some(&a.request_id),
                a.timeout_ms.filter(|&ms| ms > 0).unwrap_or(86_400_000),
                cap,
                Some(&a.operation_id),
                norm_model.as_deref(),
                norm_effort.as_deref(),
            )
        }
        "gemini_collect" => {
            let a: Id = decode(v)?;
            ops::collect(&a.operation_id)
        }
        "gemini_cancel" => {
            let a: Id = decode(v)?;
            ops::action(&a.operation_id, "cancel")
        }
        _ => Ok(json!({"status":"unsupported","reason":"unknown_tool"})),
    }
}
impl ServerHandler for GeminiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: tools(),
            ..Default::default()
        })
    }
    async fn call_tool(
        &self,
        req: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let name = req.name.to_string();
        let args = Value::Object(req.arguments.unwrap_or_default());
        let r = tokio::task::spawn_blocking(move || dispatch(&name, args))
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::structured(match r {
            Ok(v) => v,
            Err(e) => json!({"status":"invalid_argument_or_unavailable","reason":e.to_string()}),
        })
        .into())
    }
}
