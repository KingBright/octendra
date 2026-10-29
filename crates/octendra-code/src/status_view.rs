//! Server-rendered task cards. No JavaScript, execution controls or duplicated state store.
use serde_json::Value;
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn text(value: &Value) -> String {
    if value.is_null() {
        "unknown".into()
    } else if let Some(s) = value.as_str() {
        escape(s)
    } else {
        escape(&value.to_string())
    }
}
fn operation_status(item: &Value) -> (&'static str, &'static str) {
    match item["state"].as_str().unwrap_or("") {
        "completed" | "succeeded" => (
            "操作已返回",
            "结果以原操作回执为准；业务是否完成需结合实际结果判断。",
        ),
        "failed" | "operation_failed" | "process_failed" => (
            "操作失败",
            "查看原回执中的具体原因，修正后继续；保留原操作编号。",
        ),
        "cancelled" | "process_cancelled" => ("操作已取消", "清理是否完成以原操作回执为准。"),
        "process_timed_out" => ("执行超时", "查看原操作的退出状态和输出，再决定如何修正。"),
        "output_incomplete" => (
            "输出待补全",
            "通过 terminal_read 读取原终端保留的输出，无需重跑命令。",
        ),
        _ if item["stale"] == true => (
            "状态暂未更新",
            "连接恢复后查询原操作；执行结果尚未确认时不要重复提交命令。",
        ),
        "queued" => ("排队中", "操作已接收，等待设备处理；无需重复提交。"),
        "dispatched" | "awaiting_device_result" => {
            ("已发送到设备", "等待原操作回执；无需重复提交。")
        }
        "waiting_device" => (
            "等待设备恢复连接",
            "设备恢复连接后继续处理原操作，无需重复提交。",
        ),
        "paused" => (
            "传输已暂停",
            "连接恢复后通过 transfer_resume 继续同一操作。",
        ),
        "awaiting_source" => (
            "等待文件来源恢复",
            "通过 transfer_resume 为原操作补充有效文件来源。",
        ),
        "running" | "process_running" => (
            "执行中",
            "继续观察原操作。文件字节传输结束后仍需等待校验和最终结果。",
        ),
        _ => (
            "执行结果待确认",
            "查询原操作及进程回执，确认结果后再决定下一步；不要重复执行命令。",
        ),
    }
}
/// Freshness clocks are not task changes. State/stale/error/permission fields
/// stay in the validator, so a 304 never masks an actual outcome change.
pub(crate) fn revision(snapshot: &Value) -> String {
    fn stable(value: &mut Value) {
        match value {
            Value::Object(object) => {
                for key in ["observed_at", "heartbeat_at", "last_seen", "reported_at"] {
                    object.remove(key);
                }
                if let Some(event) = object
                    .get_mut("last_confirmed_event")
                    .and_then(Value::as_object_mut)
                {
                    event.remove("at");
                }
                for value in object.values_mut() {
                    stable(value);
                }
            }
            Value::Array(items) => {
                for value in items {
                    stable(value);
                }
            }
            _ => {}
        }
    }
    let mut value = snapshot.clone();
    stable(&mut value);
    crate::hash(serde_json::to_vec(&value).expect("JSON value"))
}

pub(crate) fn render(snapshot: &Value) -> String {
    let mut html = String::from(
        "<!doctype html><html lang=zh-CN><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><noscript><meta http-equiv=refresh content=30></noscript><script src='/status/live.js' defer></script><title>Octendra 连接与访问</title><style>body{font-family:system-ui;margin:0;background:#11151c;color:#e9edf5}main{max-width:1200px;margin:auto;padding:24px}.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,350px),1fr));gap:16px}.card{border:1px solid #354153;border-radius:12px;padding:16px;background:#19222e;overflow-wrap:anywhere}h1,h2,h3{line-height:1.3}.muted{color:#adb8c7}.state{font-weight:700}dl{display:grid;grid-template-columns:112px 1fr;gap:6px;margin:12px 0}dt{color:#adb8c7}dd{margin:0}pre{white-space:pre-wrap;overflow-wrap:anywhere;font-size:12px}button{padding:8px 16px;font:inherit}code{font-size:12px}</style></head><body><p id='octendra-observation' role='status' style='margin:16px 24px'>正在显示已确认快照</p><main id='octendra-status'><h1>Octendra 连接与访问</h1><p class=muted>状态随设备报告更新。</p>",
    );
    html = html.replacen("</head>", &format!("{}</head>", crate::branding::HEAD), 1);
    html = html.replacen(
        "id='octendra-status'",
        &format!(
            "id='octendra-status' data-revision='{}'",
            revision(snapshot)
        ),
        1,
    );
    if snapshot["forge_browser_enabled"].as_bool() == Some(true) {
        html.push_str("<p><a href='/status/forge/' style='display:inline-block;padding:12px 16px;color:#9ecbff;overflow-wrap:anywhere'>Forge 项目 · 问题与实验</a></p>");
    }
    html.push_str(&crate::permission_view::render(&snapshot["permissions"]));
    html.push_str("<details id=operation-audit><summary>查看操作与审计</summary>");
    html.push_str(&format!("<section class=card><h2>{}</h2><p>观察时间：{} · 活跃操作：{} · 待核对：{} · 当前页完整：{}</p></section>",
        text(&snapshot["summary"]["state"]),text(&snapshot["observed_at"]),text(&snapshot["summary"]["active_operations"]),
        text(&snapshot["summary"]["uncertain_operations"]),text(&snapshot["summary"]["scope_complete"])));
    html.push_str("<h2>任务与真实操作</h2><p class=muted>请求及操作回执自动关联；已有任务限制保持原绑定，状态页不会启动或续接操作。</p><div class=grid>");
    if let Some(items) = snapshot["operations"].as_array() {
        for item in items {
            let (state, next) = operation_status(item);
            html.push_str(&format!(
                "<section class=card data-operation='{}' data-view='{}'><h3>{} · {}</h3><p class=state>{state}</p><p>{next}</p><dl>",
                text(&item["operation_id"]), revision(item),
                text(&item["device_name"]),
                text(&item["tool"])
            ));
            for (label, value) in [
                ("原始状态", &item["state"]),
                ("操作", &item["operation_id"]),
                ("工作区", &item["workspace_id"]),
                ("目录", &item["working_directory"]),
                ("通信阶段", &item["transport_state"]),
                ("进程 PID", &item["process"]["pid"]),
                ("进程状态", &item["process"]["state"]),
                ("退出码", &item["process"]["exit_code"]),
                ("最后确认", &item["last_confirmed_event"]),
                ("进度变化时间", &item["last_progress_at"]),
                ("设备心跳", &item["heartbeat_at"]),
                ("状态过期", &item["stale"]),
                ("阻塞原因", &item["blocked_reason"]),
                ("所需操作", &item["receipt"]["user_action"]),
                ("下一步", &item["receipt"]["next_action"]),
                ("业务验收", &item["receipt"]["business_state"]),
                ("证据完整", &item["receipt"]["evidence_complete"]),
            ] {
                html.push_str(&format!("<dt>{label}</dt><dd>{}</dd>", text(value)));
            }
            if let Some(reason) = crate::permission_view::rejection(&item["receipt"]) {
                html.push_str(&format!("<dt>拒绝说明</dt><dd>{reason}</dd>"));
            }
            html.push_str(&format!(
                "</dl><details><summary>查看脱敏回执</summary><pre>{}</pre></details></section>",
                escape(&serde_json::to_string_pretty(&item["receipt"]).unwrap_or_default())
            ));
        }
    }
    html.push_str("</div><h2>尚无远端操作的请求</h2><div class=grid>");
    if let Some(items) = snapshot["requests_without_operation"].as_array() {
        for item in items {
            let guidance = crate::permission_view::rejection(item)
                .or_else(|| crate::permission_view::rejection(&item["receipt"]));
            if let Some(reason) = guidance {
                html.push_str(&format!("<p role=note>{reason}</p>"));
            }
            html.push_str(&format!("<section class=card><h3>{}</h3><p>{}</p><code>{}</code><details><summary>脱敏回执</summary><pre>{}</pre></details></section>",text(&item["tool"]),text(&item["state"]),text(&item["request_id"]),escape(&serde_json::to_string_pretty(item).unwrap_or_default())));
        }
    }
    html.push_str("</div><p class=muted>只展示已授权、仍在保留期内的记录。历史分页或缺失的证据不会被标为整体完成。</p></details><form method=post action=/status/logout><button type=submit>退出管理页</button></form></main></body></html>");
    html
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn recovery_guidance_preserves_unknown_outcomes_and_confirmed_results() {
        assert_eq!(
            operation_status(&json!({"state":"running","stale":true})).0,
            "状态暂未更新"
        );
        assert_eq!(
            operation_status(&json!({"state":"completed","stale":true})).0,
            "操作已返回"
        );
        assert!(
            operation_status(&json!({"state":"paused"}))
                .1
                .contains("同一操作")
        );
        assert!(
            operation_status(&json!({"state":"stale_or_unknown"}))
                .1
                .contains("不要重复执行命令")
        );
    }
    #[test]
    fn conditional_revision_preserves_stale_errors_and_real_progress() {
        let first = json!({"observed_at":10,"operations":[{"state":"running","stale":false,"heartbeat_at":10,"last_confirmed_event":{"stage":"started","at":10},"last_progress_at":8}]});
        let mut heartbeat = first.clone();
        heartbeat["observed_at"] = json!(20);
        heartbeat["operations"][0]["heartbeat_at"] = json!(20);
        heartbeat["operations"][0]["last_confirmed_event"]["at"] = json!(20);
        assert_eq!(revision(&first), revision(&heartbeat));
        for key in ["stale", "state", "last_progress_at"] {
            let mut changed = heartbeat.clone();
            changed["operations"][0][key] = json!("changed");
            assert_ne!(revision(&first), revision(&changed));
        }
    }

    #[test]
    fn task_cards_escape_data_and_show_unknown_instead_of_fake_progress() {
        let html = render(
            &json!({"summary":{"state":"no_active_work","active_operations":0,"uncertain_operations":0,"scope_complete":true},"operations":[{"device_name":"<script>bad()</script>","tool":"terminal_exec","state":"stale_or_unknown","receipt":{"business_state":"not_evaluated","evidence_complete":false}}]}),
        );
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("unknown"));
        assert!(html.contains("not_evaluated"));
        // Responsive CSS may legitimately use 100% width. Reject progress UI,
        // not layout percentages, when the snapshot has no measured denominator.
        assert!(!html.contains("<progress"));
        assert!(!html.contains("aria-valuenow"));
        assert!(!html.contains(">100%<"));
        assert!(html.contains("脱敏回执"));
    }
}
