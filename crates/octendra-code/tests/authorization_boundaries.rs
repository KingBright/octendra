//! Synthetic Gateway/Agent policy fixtures. No production credentials or devices.
use octendra_code::{
    AgentConfig, DeviceRegistration, GatewayConfig,
    agent::Agent,
    auth::Principal,
    files::Workspace,
    gateway::{Gateway, Job},
    hash, now, random,
};
use serde_json::{Value, json};
use std::path::PathBuf;

async fn gateway(scopes: &[&str]) -> (tempfile::TempDir, Gateway, String) {
    let dir = tempfile::tempdir().unwrap();
    let device = uuid::Uuid::new_v4().to_string();
    let g = Gateway::new(GatewayConfig {
        public_url: "https://authorization-fixture.invalid".into(),
        bind: "127.0.0.1:0".into(),
        state_dir: dir.path().join("state"),
        owner: "fixture".into(),
        password_hash: "unused-artificial-hash".into(),
        forge_browser: None,
        devices: vec![DeviceRegistration {
            id: device.clone(),
            name: "fixture".into(),
            token_hash: hash("artificial-device-token"),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        }],
        redirect_uris: vec![],
        allowed_origins: vec![],
    })
    .await
    .unwrap();
    (dir, g, device)
}

fn principal(scopes: &[&str]) -> Principal {
    Principal {
        owner: "fixture".into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
    }
}

async fn job_count(g: &Gateway) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&g.store.pool)
        .await
        .unwrap()
}

async fn saved_job(g: &Gateway, device: &str, tool: &str, result: Value) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let job = Job {
        id: id.clone(),
        device_id: device.into(),
        owner: "fixture".into(),
        tool: tool.into(),
        arguments: json!({"workspace_id":format!("{device}:default")}),
    };
    sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,?,'done',?)")
        .bind(&id).bind(device).bind(random()).bind(hash(b"fixture"))
        .bind(serde_json::to_string(&job).unwrap()).bind(result.to_string()).bind(now())
        .execute(&g.store.pool).await.unwrap();
    id
}

#[tokio::test]
async fn authorization_read_only_account_rejects_every_mutation_and_control_before_queueing() {
    let (_dir, g, _) = gateway(&[
        "code:read",
        "code:write",
        "terminal:exec",
        "desktop:observe",
        "desktop:input",
    ])
    .await;
    let p = principal(&["code:read"]);
    for name in [
        "code_apply_edits",
        "change_resume",
        "workspace_gc",
        "files_sync",
        "file_upload",
        "transfer_cancel",
        "transfer_resume",
        "outcome_resolve",
        "terminal_exec",
        "terminal_input",
        "terminal_cancel",
        "desktop_worker",
    ] {
        let error = g.dispatch(&p, name, json!({})).await.unwrap_err();
        assert!(
            error.to_string().contains("insufficient_scope"),
            "{name}: {error}"
        );
    }
    assert_eq!(job_count(&g).await, 0);
    assert!(g.dispatch(&p, "devices_list", json!({})).await.is_ok());
}

#[tokio::test]
async fn authorization_read_only_device_rejects_full_account_writes_execution_and_resume() {
    let (_dir, g, device) = gateway(&["code:read"]).await;
    let p = principal(&["code:read", "code:write", "terminal:exec"]);
    for (name, args) in [
        (
            "code_apply_edits",
            json!({"workspace_id":format!("{device}:default"),"files":[{"path":"sentinel","content":"never","action":"create"}]}),
        ),
        (
            "terminal_exec",
            json!({"workspace_id":format!("{device}:default"),"command":"printf never","wait_ms":0}),
        ),
    ] {
        assert!(
            g.dispatch(&p, name, args)
                .await
                .unwrap_err()
                .to_string()
                .contains("device_scope_denied")
        );
    }
    assert_eq!(job_count(&g).await, 0);
    let id = saved_job(&g, &device, "file_upload", json!({"state":"paused"})).await;
    let error = g
        .dispatch(&p, "transfer_resume", json!({"operation_id":id}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("device_scope_denied"));
    assert_eq!(job_count(&g).await, 1);
}

#[tokio::test]
async fn authorization_workspace_capabilities_are_current_account_device_local_intersection() {
    let (_dir, g, device) = gateway(&["code:read", "code:write", "terminal:exec"]).await;
    g.store
        .put(
            "online",
            &device,
            &json!({"hello":{
        "session":"fixture", "roots":[], "allow_write":true, "allow_exec":true
    },"last_seen":now()}),
            now() + 60,
        )
        .await
        .unwrap();
    let id = saved_job(
        &g,
        &device,
        "workspace_open",
        json!({"allow_write":true,"allow_exec":true}),
    )
    .await;
    let result = g
        .dispatch(
            &principal(&["code:read"]),
            "operation_get",
            json!({"operation_id":id,"wait_ms":0}),
        )
        .await
        .unwrap();
    assert_eq!(result["allow_write"], false);
    assert_eq!(result["allow_exec"], false);
    let full = g
        .dispatch(
            &principal(&["code:read", "code:write", "terminal:exec"]),
            "operation_get",
            json!({"operation_id":id,"wait_ms":0}),
        )
        .await
        .unwrap();
    assert_eq!(full["allow_write"], true);
    assert_eq!(full["allow_exec"], true);
    g.store
        .put(
            "online",
            &device,
            &json!({"hello":{
        "session":"fixture", "roots":[], "allow_write":false, "allow_exec":false
    },"last_seen":now()}),
            now() + 60,
        )
        .await
        .unwrap();
    let changed = g
        .dispatch(
            &principal(&["code:read", "code:write", "terminal:exec"]),
            "operation_get",
            json!({"operation_id":id,"wait_ms":0}),
        )
        .await
        .unwrap();
    assert_eq!(changed["allow_write"], false);
    assert_eq!(changed["allow_exec"], false);
    assert_eq!(changed["local_permissions_currently_observed"], true);
    sqlx::query("DELETE FROM kv WHERE kind='online'")
        .execute(&g.store.pool)
        .await
        .unwrap();
    let absent = g
        .dispatch(
            &principal(&["code:read", "code:write", "terminal:exec"]),
            "operation_get",
            json!({"operation_id":id,"wait_ms":0}),
        )
        .await
        .unwrap();
    assert_eq!(absent["allow_write"], false);
    assert_eq!(absent["local_permissions_currently_observed"], false);
}

#[tokio::test]
async fn authorization_outcome_reconciliation_cannot_borrow_an_execution_scope() {
    let (_dir, g, device) = gateway(&["code:read", "code:write", "terminal:exec"]).await;
    let id = saved_job(
        &g,
        &device,
        "terminal_exec",
        json!({"error":"outcome_unknown"}),
    )
    .await;
    let error = g
        .dispatch(
            &principal(&["code:read", "code:write"]),
            "outcome_resolve",
            json!({"operation_id":id,"resolution":"verified_not_applied"}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("insufficient_scope"));
    assert!(
        g.store
            .get::<Value>("outcome_resolution", &id)
            .await
            .unwrap()
            .is_none()
    );
}

async fn agent(write: bool, exec: bool) -> (tempfile::TempDir, Agent, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("allowed");
    let outside = dir.path().join("allowed-sibling");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(root.join("visible.txt"), "VISIBLE-FIXTURE").unwrap();
    std::fs::write(outside.join("outside.txt"), "OUTSIDE-FIXTURE").unwrap();
    let a = Agent::new(AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: uuid::Uuid::new_v4().to_string(),
        device_token: random(),
        state_dir: dir.path().join("state"),
        roots: vec![root.clone()],
        allow_write: write,
        allow_exec: exec,
        shell: "/bin/sh".into(),
    })
    .await
    .unwrap();
    (dir, a, root, outside)
}

async fn execute(a: &Agent, tool: &str, args: Value) -> Value {
    a.execute(&Job {
        id: uuid::Uuid::new_v4().to_string(),
        device_id: a.config.device_id.clone(),
        owner: "fixture".into(),
        tool: tool.into(),
        arguments: args,
    })
    .await
    .unwrap()
}

fn read_args(workspace: String, path: &str) -> Value {
    json!({"workspace_id":workspace,"requests":[{"path":path}]})
}

#[tokio::test]
async fn authorization_local_flags_allow_reads_but_deny_writes_shell_and_controls() {
    let (_dir, a, root, _) = agent(false, false).await;
    let opened = execute(
        &a,
        "workspace_open",
        json!({"device_id":a.config.device_id,"root":root}),
    )
    .await;
    assert_eq!(opened["allow_write"], false);
    assert_eq!(opened["allow_exec"], false);
    let ws = opened["workspace"]["id"].as_str().unwrap();
    let read = execute(&a, "code_read", read_args(ws.into(), "visible.txt")).await;
    assert!(read.to_string().contains("VISIBLE-FIXTURE"), "{read}");
    for name in [
        "code_apply_edits",
        "files_sync",
        "file_upload",
        "change_resume",
        "workspace_gc",
    ] {
        let denied = execute(&a, name, json!({"workspace_id":ws})).await;
        assert_eq!(
            denied["error_code"], "local_write_disabled",
            "{name}: {denied}"
        );
        assert_eq!(denied["outcome"], "not_executed");
    }
    for name in ["terminal_exec", "terminal_input", "terminal_cancel"] {
        let denied = execute(
            &a,
            name,
            json!({"workspace_id":ws,"command":"printf status; touch forbidden"}),
        )
        .await;
        assert_eq!(
            denied["error_code"], "local_exec_disabled",
            "{name}: {denied}"
        );
    }
    assert!(!root.join("forbidden").exists());
}

#[tokio::test]
async fn authorization_configured_roots_cover_open_alias_default_and_old_workspace_ids() {
    let (_dir, a, root, outside) = agent(false, false).await;
    let denied = execute(
        &a,
        "workspace_open",
        json!({"device_id":a.config.device_id,"root":outside}),
    )
    .await;
    assert_eq!(denied["error_code"], "workspace_root_denied");
    for alias in [
        format!("{}:{}", a.config.device_id, outside.display()),
        outside.to_string_lossy().into_owned(),
    ] {
        let denied = execute(&a, "code_read", read_args(alias, "outside.txt")).await;
        assert_eq!(denied["error_code"], "workspace_root_denied");
        assert!(!denied.to_string().contains("OUTSIDE-FIXTURE"));
    }
    let id = format!("{}:{}", a.config.device_id, uuid::Uuid::new_v4());
    a.store
        .put(
            "workspace",
            &id,
            &Workspace {
                directory: None,
                id: id.clone(),
                device_id: a.config.device_id.clone(),
                root: outside.canonicalize().unwrap(),
            },
            i64::MAX,
        )
        .await
        .unwrap();
    let denied = execute(&a, "code_read", read_args(id, "outside.txt")).await;
    assert_eq!(denied["error_code"], "workspace_root_denied");
    for alias in [
        format!("{}:default", a.config.device_id),
        format!("{}:{}", a.config.device_id, root.display()),
    ] {
        assert!(
            execute(&a, "code_read", read_args(alias, "visible.txt"))
                .await
                .to_string()
                .contains("VISIBLE-FIXTURE")
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn authorization_symlink_roots_files_and_moved_workspaces_cannot_escape() {
    use std::os::unix::fs::symlink;
    let (_dir, a, root, outside) = agent(false, false).await;
    symlink(&outside, root.join("escape")).unwrap();
    let denied = execute(
        &a,
        "workspace_open",
        json!({"device_id":a.config.device_id,"root":root.join("escape")}),
    )
    .await;
    assert_eq!(denied["error_code"], "workspace_root_denied");
    symlink(outside.join("outside.txt"), root.join("escape.txt")).unwrap();
    let denied = execute(
        &a,
        "code_read",
        read_args(format!("{}:default", a.config.device_id), "escape.txt"),
    )
    .await;
    assert!(denied.get("error").is_some());
    assert!(!denied.to_string().contains("OUTSIDE-FIXTURE"));
    let sub = root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let opened = execute(
        &a,
        "workspace_open",
        json!({"device_id":a.config.device_id,"root":sub}),
    )
    .await;
    std::fs::remove_dir(&sub).unwrap();
    symlink(&outside, &sub).unwrap();
    let denied = execute(
        &a,
        "code_read",
        read_args(
            opened["workspace"]["id"].as_str().unwrap().into(),
            "outside.txt",
        ),
    )
    .await;
    assert_eq!(denied["error_code"], "workspace_root_denied");
}

#[tokio::test]
async fn authorization_empty_roots_do_not_default_to_filesystem_root() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let result = Agent::new(AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: uuid::Uuid::new_v4().to_string(),
        device_token: random(),
        state_dir: state.clone(),
        roots: vec![],
        allow_write: false,
        allow_exec: false,
        shell: "/bin/sh".into(),
    })
    .await;
    assert!(result.is_err());
    assert!(!state.exists());
}

#[tokio::test]
async fn authorization_invalid_workspace_result_is_rejected_without_panicking() {
    let (_dir, g, device) = gateway(&["code:read"]).await;
    let id = saved_job(&g, &device, "workspace_open", json!([])).await;
    assert!(
        g.dispatch(
            &principal(&["code:read"]),
            "operation_get",
            json!({"operation_id":id,"wait_ms":0})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn authorization_old_request_receipt_cannot_bypass_current_account_scopes() {
    let (_dir, g, device) = gateway(&["code:read", "code:write"]).await;
    let args = json!({"workspace_id":format!("{device}:default"),"files":[{"path":"sentinel","action":"create","content":"never"}]});
    let request = format!("req_{}", uuid::Uuid::new_v4().simple());
    octendra_code::receipts::invoke_durable(
        &g,
        &principal(&["code:read", "code:write"]),
        "code_apply_edits",
        args.clone(),
        &request,
    )
    .await
    .unwrap();
    let error = octendra_code::receipts::invoke_durable(
        &g,
        &principal(&["code:read"]),
        "code_apply_edits",
        args,
        &request,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("insufficient_scope"));
    assert_eq!(job_count(&g).await, 0);
}

#[tokio::test]
async fn authorization_explicit_write_permission_still_works_without_exec_permission() {
    let (_dir, a, root, _) = agent(true, false).await;
    let result=execute(&a,"code_apply_edits",json!({"workspace_id":format!("{}:default",a.config.device_id),
        "files":[{"path":"created.txt","action":"create","expected_version":"absent","content":"AUTHORIZED-FIXTURE"}]})).await;
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(
        std::fs::read_to_string(root.join("created.txt")).unwrap(),
        "AUTHORIZED-FIXTURE"
    );
    let denied = execute(
        &a,
        "terminal_exec",
        json!({"workspace_id":format!("{}:default",a.config.device_id),"command":"printf never"}),
    )
    .await;
    assert_eq!(denied["error_code"], "local_exec_disabled");
}

#[cfg(unix)]
#[tokio::test]
async fn authorization_explicit_exec_permission_still_runs_a_bounded_synthetic_command() {
    let (_dir, a, _, _) = agent(false, true).await;
    let result = execute(
        &a,
        "terminal_exec",
        json!({"workspace_id":format!("{}:default",a.config.device_id),
        "command":"printf AUTHORIZATION-ALLOWED","wait_ms":2000,"timeout_seconds":5}),
    )
    .await;
    assert!(result.get("error").is_none(), "{result}");
    assert_eq!(result["output"], "AUTHORIZATION-ALLOWED", "{result}");
}

#[tokio::test]
async fn authorization_all_transfer_http_entries_recheck_scope_on_original_jobs() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (_dir, g, device) = gateway(&[]).await;
    for tool in ["file_upload", "file_download"] {
        let id = saved_job(&g, &device, tool, json!({"state":"paused"})).await;
        sqlx::query("UPDATE jobs SET state='dispatched' WHERE id=?")
            .bind(&id)
            .execute(&g.store.pool)
            .await
            .unwrap();
        let routes = if tool == "file_upload" {
            vec![("GET", format!("/device/file-source/{id}"))]
        } else {
            vec![
                ("POST", format!("/device/files/{id}")),
                ("GET", format!("/device/file-progress/{id}")),
                ("GET", format!("/device/transfers/{id}")),
                ("POST", format!("/device/transfers/{id}")),
                ("POST", format!("/device/transfers/{id}/chunk")),
                ("POST", format!("/device/transfers/{id}/complete")),
                ("POST", format!("/device/transfers/{id}/abort")),
            ]
        };
        for (method, path) in routes.into_iter().chain([
            ("GET", format!("/device/transfer-control/{id}")),
            ("POST", format!("/device/transfer-status/{id}")),
        ]) {
            let response = g
                .router()
                .unwrap()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(&path)
                        .header("host", "authorization-fixture.invalid")
                        .header("authorization", "Bearer artificial-device-token")
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
}

#[tokio::test]
async fn authorization_resuming_persisted_upload_rechecks_current_local_write_flag() {
    let (_dir, a, _root, _outside) = agent(false, false).await;
    let job = Job {
        id: uuid::Uuid::new_v4().to_string(),
        device_id: a.config.device_id.clone(),
        owner: "fixture".into(),
        tool: "file_upload".into(),
        arguments: json!({
            "workspace_id":format!("{}:default",a.config.device_id),"path":"never.txt",
            "file":{"file_id":"fixture","download_url":"https://files.oaiusercontent.com/fixture"}
        }),
    };
    a.store
        .put(
            "local_operation",
            &job.id,
            &json!({
                "fingerprint":hash(serde_json::to_vec(&job).unwrap()),"state":"paused",
                "result":null,"resumable":true,"gateway_accepted":false,
                "workspace_id":job.arguments["workspace_id"],"tool":"file_upload","updated_at":now()
            }),
            i64::MAX,
        )
        .await
        .unwrap();
    let result = a.execute(&job).await.unwrap();
    assert_eq!(result["error_code"], "local_write_disabled");
    assert_eq!(result["outcome"], "not_executed");
}

#[tokio::test]
async fn authorization_existing_download_link_is_revoked_by_current_device_scope() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let (_dir, mut g, device) = gateway(&["code:read"]).await;
    let bytes = b"ARTIFICIAL-EXPORT";
    let sha = hash(bytes);
    let id = saved_job(
        &g,
        &device,
        "file_download",
        json!({"state":"completed","size":bytes.len(),"sha256":sha}),
    )
    .await;
    sqlx::query("UPDATE jobs SET state='dispatched', request=json_set(request,'$.arguments.path','export.bin') WHERE id=?")
        .bind(&id)
        .execute(&g.store.pool)
        .await
        .unwrap();
    let response = g
        .router()
        .unwrap()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/device/files/{id}"))
                .header("host", "authorization-fixture.invalid")
                .header("authorization", "Bearer artificial-device-token")
                .header("x-file-size", bytes.len())
                .header("x-file-sha256", &sha)
                .body(Body::from(bytes.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let request: String = sqlx::query_scalar("SELECT request FROM jobs WHERE id=?")
        .bind(&id)
        .fetch_one(&g.store.pool)
        .await
        .unwrap();
    let original: Job = serde_json::from_str(&request).unwrap();
    let mut result = json!({"state":"completed","size":bytes.len(),"sha256":sha});
    octendra_code::transfers::decorate(&g, &original, &mut result)
        .await
        .unwrap();
    let url = reqwest::Url::parse(result["download_url"].as_str().unwrap()).unwrap();
    let request = || {
        Request::builder()
            .uri(url.path())
            .header("host", "authorization-fixture.invalid")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        g.router()
            .unwrap()
            .oneshot(request())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    // Synthetic configuration replacement against the same durable store.
    std::sync::Arc::make_mut(&mut g.config).devices[0]
        .scopes
        .clear();
    assert_eq!(
        g.router()
            .unwrap()
            .oneshot(request())
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn authorization_cached_local_workspace_response_clamps_current_agent_flags() {
    let (_dir, a, root, _outside) = agent(false, false).await;
    let job = Job {
        id: uuid::Uuid::new_v4().to_string(),
        device_id: a.config.device_id.clone(),
        owner: "fixture".into(),
        tool: "workspace_open".into(),
        arguments: json!({
            "device_id":a.config.device_id,"root":root,
        }),
    };
    a.store.put("local_operation", &job.id, &json!({
        "fingerprint":hash(serde_json::to_vec(&job).unwrap()),"state":"done",
        "result":{"allow_write":true,"allow_exec":true},"resumable":false,"gateway_accepted":false,
        "tool":"workspace_open","updated_at":now()
    }),i64::MAX).await.unwrap();
    let result = a.execute(&job).await.unwrap();
    assert_eq!(result["allow_write"], false);
    assert_eq!(result["allow_exec"], false);
}

fn state_only_config(base: &std::path::Path) -> AgentConfig {
    let root = base.join("published-state");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("health.json"), "{\"ready\":true}\n").unwrap();
    AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: uuid::Uuid::new_v4().to_string(),
        device_token: random(),
        state_dir: base.join("private-agent"),
        roots: vec![root],
        allow_write: false,
        allow_exec: false,
        shell: "/bin/sh".into(),
    }
}

#[tokio::test]
async fn authorization_state_only_allows_only_explicit_state_directory_reads() {
    let dir = tempfile::tempdir().unwrap();
    let config = state_only_config(dir.path());
    let a = Agent::new_state_only(config.clone()).await.unwrap();
    let opened = execute(
        &a,
        "workspace_open",
        json!({"device_id":config.device_id,"root":config.roots[0]}),
    )
    .await;
    assert_eq!(opened["access_policy"], "state_only");
    assert_eq!(opened["allow_write"], false);
    assert_eq!(opened["allow_exec"], false);
    assert_eq!(opened["file_transfer"], false);
    assert_eq!(opened["terminal_authority"], "disabled");
    let workspace = opened["workspace"]["id"].as_str().unwrap();
    let listed = execute(&a, "code_list", json!({"workspace_id":workspace})).await;
    assert_eq!(listed["files"], json!(["health.json"]));
    let value = execute(&a, "code_read", read_args(workspace.into(), "health.json")).await;
    assert!(value.get("error").is_none(), "{value}");
    assert!(value.to_string().contains("ready"));
    for tool in octendra_code::tools::catalog() {
        if matches!(
            tool.name.as_ref(),
            "workspace_open" | "code_list" | "code_read"
        ) {
            continue;
        }
        let denied = execute(&a, tool.name.as_ref(), json!({})).await;
        assert_eq!(
            denied["error_code"], "state_only_tool_denied",
            "{}: {denied}",
            tool.name
        );
        assert_eq!(denied["outcome"], "not_executed");
    }
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("private.json"), "OUTSIDE-FIXTURE").unwrap();
    let denied = execute(
        &a,
        "code_read",
        read_args(outside.to_string_lossy().into(), "private.json"),
    )
    .await;
    assert_eq!(denied["error_code"], "workspace_root_denied");
    assert!(!denied.to_string().contains("OUTSIDE-FIXTURE"));
    assert_eq!(
        std::fs::read_to_string(config.roots[0].join("health.json")).unwrap(),
        "{\"ready\":true}\n"
    );
}

#[tokio::test]
async fn authorization_state_only_rejects_conflicting_flags_roots_and_runtime_exposure() {
    let dir = tempfile::tempdir().unwrap();
    let config = state_only_config(dir.path());
    for (write, exec) in [(true, false), (false, true), (true, true)] {
        let mut bad = config.clone();
        bad.allow_write = write;
        bad.allow_exec = exec;
        let error = Agent::new_state_only(bad).await.err().unwrap();
        assert!(error.to_string().starts_with("state_only_config_denied"));
        assert!(!config.state_dir.exists());
    }
    let mut bad = config.clone();
    bad.roots.push(config.roots[0].clone());
    assert!(Agent::new_state_only(bad).await.is_err());
    let mut bad = config.clone();
    bad.state_dir = config.roots[0].join("private-agent");
    assert!(Agent::new_state_only(bad).await.is_err());
    let mut bad = config.clone();
    bad.state_dir = dir.path().into();
    assert!(Agent::new_state_only(bad).await.is_err());
    assert!(!config.state_dir.exists());
    let exposed_config = config.roots[0].join("agent-fixture.json");
    std::fs::write(&exposed_config, "ARTIFICIAL-CONFIG-FIXTURE").unwrap();
    assert!(Agent::validate_state_only_config_path(&config, &exposed_config).is_err());
    let private_config = dir.path().join("private-config.json");
    std::fs::write(&private_config, "ARTIFICIAL-CONFIG-FIXTURE").unwrap();
    assert!(Agent::validate_state_only_config_path(&config, &private_config).is_ok());
}

#[tokio::test]
async fn authorization_state_only_cannot_return_old_git_or_hydrate_payload_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let config = state_only_config(dir.path());
    let a = Agent::new_state_only(config).await.unwrap();
    let job = Job {
        id: uuid::Uuid::new_v4().to_string(),
        device_id: a.config.device_id.clone(),
        owner: "fixture".into(),
        tool: "code_diff".into(),
        arguments: json!({}),
    };
    a.store
        .put(
            "local_operation",
            &job.id,
            &json!({
                "fingerprint":hash(serde_json::to_vec(&job).unwrap()),"state":"done",
                "result":{"diff":"OLD-UNAUTHORIZED-FIXTURE"},"resumable":false,
                "gateway_accepted":false,"tool":"code_diff","updated_at":now()
            }),
            i64::MAX,
        )
        .await
        .unwrap();
    let result = a.execute(&job).await.unwrap();
    assert_eq!(result["error_code"], "state_only_tool_denied");
    assert!(!result.to_string().contains("OLD-UNAUTHORIZED-FIXTURE"));
    let denied = execute(
        &a,
        "code_read",
        json!({"code_payload":{"file_id":"fixture"}}),
    )
    .await;
    assert_eq!(denied["error_code"], "state_only_tool_denied");
    let frame = Job {
        tool: "desktop_frame".into(),
        ..job
    };
    assert!(
        a.read_desktop_frame(&frame)
            .await
            .unwrap_err()
            .to_string()
            .starts_with("state_only_tool_denied")
    );
}

#[tokio::test]
async fn authorization_state_only_restart_preserves_policy_and_refuses_general_history_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let config = state_only_config(dir.path());
    let a = Agent::new_state_only(config.clone()).await.unwrap();
    drop(a);
    let error = Agent::new(config.clone()).await.err().unwrap();
    assert!(error.to_string().starts_with("state_only_start_required"));
    let a = Agent::new_state_only(config.clone()).await.unwrap();
    drop(a);
    let mut changed = config.clone();
    let other = dir.path().join("other-state");
    std::fs::create_dir(&other).unwrap();
    changed.roots = vec![other];
    assert!(
        Agent::new_state_only(changed)
            .await
            .err()
            .unwrap()
            .to_string()
            .starts_with("state_only_state_conflict")
    );
    let general_dir = tempfile::tempdir().unwrap();
    let general = state_only_config(general_dir.path());
    let a = Agent::new(general.clone()).await.unwrap();
    let before: Value = a.store.get("runtime", "build").await.unwrap().unwrap();
    drop(a);
    assert!(
        Agent::new_state_only(general.clone())
            .await
            .err()
            .unwrap()
            .to_string()
            .starts_with("state_only_state_conflict")
    );
    let a = Agent::new(general).await.unwrap();
    assert!(
        a.store
            .get::<Value>("runtime", "local_access_policy")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        a.store
            .get::<Value>("runtime", "build")
            .await
            .unwrap()
            .unwrap()["version"],
        before["version"]
    );
}
