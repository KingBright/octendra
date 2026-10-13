use super::*;
use crate::{
    AgentConfig, DeviceRegistration, GatewayConfig,
    agent::Agent,
    gateway::{DeviceHello, Job},
    hash, now, receipts,
};
use axum::{body::Body, http::Request};
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    g: Gateway,
    p: Principal,
    agent: Agent,
    device: String,
    session: String,
    cookie: String,
    password: String,
}
async fn fixture() -> Fixture {
    fixture_with_flags(true, true).await
}
async fn fixture_with_flags(allow_write: bool, allow_exec: bool) -> Fixture {
    use argon2::{Argon2, PasswordHasher, password_hash::SaltString};
    let dir = tempfile::tempdir().unwrap();
    let password = "synthetic-owner-password".to_owned();
    let salt = SaltString::encode_b64(b"0123456789abcdef").unwrap();
    let password_hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .unwrap()
        .to_string();
    let device = uuid::Uuid::new_v4().to_string();
    let session = crate::random();
    let token = crate::random();
    let scopes: Vec<String> = crate::SCOPES
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let g = Gateway::new(GatewayConfig {
        forge_browser: None,
        public_url: "https://fixture.example".into(),
        bind: "127.0.0.1:0".into(),
        state_dir: dir.path().join("gateway"),
        owner: "owner".into(),
        password_hash,
        devices: vec![DeviceRegistration {
            id: device.clone(),
            name: "Fixture".into(),
            token_hash: hash(&token),
            scopes: scopes.clone(),
        }],
        redirect_uris: vec![],
        allowed_origins: crate::default_mcp_client_origins(),
    })
    .await
    .unwrap();
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let agent = Agent::new(AgentConfig {
        gateway_url: g.config.public_url.clone(),
        device_id: device.clone(),
        device_token: token,
        state_dir: dir.path().join("agent"),
        roots: vec![root.clone()],
        allow_write,
        allow_exec,
        shell: "/bin/sh".into(),
    })
    .await
    .unwrap();
    let hello: DeviceHello = serde_json::from_value(json!({
        "version":env!("CARGO_PKG_VERSION"),"session":session,"roots":[root],
        "platform":std::env::consts::OS,"allow_write":allow_write,"allow_exec":allow_exec,
    }))
    .unwrap();
    g.store
        .put(
            "online",
            &device,
            &json!({"hello":hello,"last_seen":now()}),
            i64::MAX,
        )
        .await
        .unwrap();
    let cookie = crate::random();
    g.store
        .put("status_session", &hash(&cookie), &true, now() + 3600)
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        g,
        p: Principal {
            owner: "owner".into(),
            scopes,
        },
        agent,
        device,
        session,
        cookie,
        password,
    }
}
fn request_id() -> String {
    format!("req_{}", uuid::Uuid::new_v4().simple())
}
async fn count(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(&f.g.store.pool)
        .await
        .unwrap()
}
async fn call(f: &Fixture, tool: &str, args: Value) -> Value {
    receipts::invoke(&f.g, &f.p, tool, args, &request_id())
        .await
        .unwrap()
}
fn selection(f: &Fixture) -> crate::job_dispatch::Selection<'_> {
    crate::job_dispatch::Selection {
        device: &f.device,
        session: &f.session,
        active: "[]",
        scopes: r#"["code:read","code:write","terminal:exec"]"#,
        lanes: r#"["read","write","terminal","control","transfer"]"#,
        defer_writes: false,
        write_workspaces: "[]",
        terminal_inputs: "[]",
    }
}
async fn run(f: &Fixture, tool: &str, args: Value) -> Value {
    let pending = call(f, tool, args).await;
    let id = pending["operation_id"].as_str().expect("queued operation");
    let (claimed, raw) = crate::job_dispatch::claim(&f.g.store, &selection(f), now())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed, id);
    let job: Job = serde_json::from_str(&raw).unwrap();
    assert!(job.arguments.get("authorization_version").is_none());
    assert!(job.arguments.get("task_id").is_none());
    let value = f.agent.execute(&job).await.unwrap();
    crate::job_receipts::commit(&f.g.store, &f.device, id, &value)
        .await
        .unwrap();
    f.g.result(&f.p, id).await.unwrap()
}
#[tokio::test]
async fn ordinary_mcp_access_reuses_existing_authority_and_automatic_audit() {
    let f = fixture().await;
    let ws=run(&f,"workspace_open",json!({"device_id":f.device,"root":f._dir.path().join("project"),"idempotency_key":"ordinary-open"})).await;
    let id = ws["workspace"]["id"].as_str().unwrap();
    let written=run(&f,"code_apply_edits",json!({"workspace_id":id,"idempotency_key":"ordinary-write","files":[{"path":"ordinary.txt","action":"create","expected_version":"absent","content":"ordinary"}]})).await;
    assert!(written.get("error").is_none(), "{written}");
    let read = run(
        &f,
        "code_read",
        json!({"workspace_id":id,"requests":[{"path":"ordinary.txt","start_line":1,"end_line":1}]}),
    )
    .await;
    assert!(read.get("error").is_none(), "{read}");
    assert!(
        read["ranges"][0]["text"]
            .as_str()
            .unwrap()
            .contains("ordinary")
    );
    let executed=run(&f,"terminal_exec",json!({"workspace_id":id,"idempotency_key":"ordinary-exec","command":"printf ordinary","wait_ms":1000})).await;
    assert!(executed.get("error").is_none(), "{executed}");
    assert_eq!(executed["terminal"]["exit_code"], 0, "{executed}");
    assert_eq!(count(&f).await, 4);
    let grant_rows:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kv WHERE kind IN ('task_authorization','operation_task_authorization')").fetch_one(&f.g.store.pool).await.unwrap();
    assert_eq!(grant_rows, 0);
    let receipts: Vec<String> =
        sqlx::query_scalar("SELECT value FROM kv WHERE kind='request_receipt'")
            .fetch_all(&f.g.store.pool)
            .await
            .unwrap();
    assert_eq!(receipts.len(), 4);
    for raw in receipts {
        let receipt: Value = serde_json::from_str(&raw).unwrap();
        assert!(receipt["operation_id"].is_string());
        assert!(receipt["task_id"].is_null());
    }
}
#[tokio::test]
async fn ordinary_mcp_status_is_read_only_and_browser_identity_is_not_execution_scope() {
    let f = fixture().await;
    let before = count(&f).await;
    let response =
        f.g.router()
            .unwrap()
            .oneshot(
                Request::builder()
                    .uri("/status")
                    .header("host", "fixture.example")
                    .header("cookie", format!("rh_status={}", f.cookie))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = String::from_utf8(
        axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(html.contains("连接与访问"));
    assert!(html.contains("无需额外限时授权或手填任务 ID"));
    assert!(html.contains("高级设置"));
    assert!(html.contains("不代表某个 MCP 连接"));
    assert!(!html.contains("name=task_id"));
    assert!(!html.contains(&f.cookie));
    assert!(!html.contains(&f.password));
    assert_eq!(count(&f).await, before);
    let grant_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM kv WHERE kind='task_authorization'")
            .fetch_one(&f.g.store.pool)
            .await
            .unwrap();
    assert_eq!(grant_rows, 0);
}
#[tokio::test]
async fn ordinary_mcp_agent_disabled_switches_keep_original_denials() {
    let f = fixture_with_flags(false, false).await;
    let ws=run(&f,"workspace_open",json!({"device_id":f.device,"root":f._dir.path().join("project"),"idempotency_key":"disabled-open"})).await;
    let id = ws["workspace"]["id"].as_str().unwrap();
    let write=run(&f,"code_apply_edits",json!({"workspace_id":id,"idempotency_key":"disabled-write","files":[{"path":"blocked.txt","action":"create","expected_version":"absent","content":"blocked"}]})).await;
    assert_eq!(write["error_code"], "local_write_disabled", "{write}");
    let exec=run(&f,"terminal_exec",json!({"workspace_id":id,"idempotency_key":"disabled-exec","command":"printf blocked","wait_ms":1000})).await;
    assert_eq!(exec["error_code"], "local_exec_disabled", "{exec}");
    assert!(!f._dir.path().join("project/blocked.txt").exists());
    let snapshot = crate::activity::status(&f.g, &f.p).await.unwrap();
    assert_eq!(
        snapshot["permissions"]["devices"][0]["capabilities"][1]["state"],
        "local_write_disabled"
    );
    assert_eq!(
        snapshot["permissions"]["devices"][0]["capabilities"][2]["state"],
        "local_exec_disabled"
    );
}

#[tokio::test]
async fn connection_authority_replaces_legacy_grant_for_new_work() {
    let f = fixture().await;
    f.g.store
        .put(
            "task_authorization",
            "legacy",
            &json!({
                "owner":"owner","task_id":"overnight","enabled":false,"version":1,
                "protocol":2,"expires_at":0,"devices":[],"scopes":[]
            }),
            i64::MAX,
        )
        .await
        .unwrap();
    let opened = run(
        &f,
        "workspace_open",
        json!({
            "device_id":f.device,"root":f._dir.path().join("project"),
            "idempotency_key":"connection-open","task_id":"overnight","authorization_version":999
        }),
    )
    .await;
    let ws = opened["workspace"]["id"].as_str().unwrap();
    let result = run(
        &f,
        "terminal_exec",
        json!({
            "workspace_id":ws,"idempotency_key":"connection-command","task_id":"overnight",
            "authorization_version":999,"command":"printf authorized","wait_ms":2000
        }),
    )
    .await;
    assert_eq!(result["terminal"]["exit_code"], 0, "{result}");
    let bindings: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM kv WHERE kind='operation_task_authorization'")
            .fetch_one(&f.g.store.pool)
            .await
            .unwrap();
    assert_eq!(bindings, 0);
    assert_eq!(
        view(&f.g, &f.p, "overnight").await.unwrap()["task_id_role"],
        "correlation_only"
    );
}

#[tokio::test]
async fn retired_task_form_cannot_create_authority() {
    let f = fixture().await;
    for method in ["GET", "POST"] {
        for authenticated in [false, true] {
            let mut request = Request::builder()
                .method(method)
                .uri("/status/task-authorization")
                .header("host", "fixture.example");
            if authenticated {
                request = request.header("cookie", format!("rh_status={}", f.cookie));
            }
            let response =
                f.g.router()
                    .unwrap()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap();
            assert_eq!(
                response.status(),
                if authenticated {
                    StatusCode::GONE
                } else {
                    StatusCode::UNAUTHORIZED
                }
            );
        }
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kv WHERE kind='task_authorization'")
        .fetch_one(&f.g.store.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    assert_eq!(count(&f).await, 0);
}

#[tokio::test]
async fn legacy_queued_binding_stays_held_and_resume_only_observes() {
    let f = fixture().await;
    let pending = call(
        &f,
        "terminal_exec",
        json!({
            "workspace_id":format!("{}:{}", f.device, uuid::Uuid::new_v4()),
            "command":"printf should-not-run","idempotency_key":"legacy-queued"
        }),
    )
    .await;
    let id = pending["operation_id"].as_str().unwrap();
    f.g.store
        .put(
            "operation_task_authorization",
            id,
            &json!({"owner":f.p.owner}),
            i64::MAX,
        )
        .await
        .unwrap();
    assert!(
        crate::job_dispatch::claim(&f.g.store, &selection(&f), now())
            .await
            .unwrap()
            .is_none()
    );
    let result = call(
        &f,
        "task_resume",
        json!({
            "task_id":"legacy","authorization_version":2,"operation_id":id
        }),
    )
    .await;
    assert_eq!(result["operation_id"], id);
    assert_eq!(result["task_authorization_retired"], true);
    assert_eq!(result["replayed"], false);
    assert_eq!(count(&f).await, 1);
    assert!(
        crate::job_dispatch::claim(&f.g.store, &selection(&f), now())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn connection_and_device_scope_revocation_still_block_execution() {
    let mut f = fixture().await;
    let args = json!({"workspace_id":format!("{}:{}",f.device,uuid::Uuid::new_v4()),
        "command":"printf rejected","idempotency_key":"scope-revoked"});
    let limited = Principal {
        owner: f.p.owner.clone(),
        scopes: vec!["code:read".into()],
    };
    let result = receipts::invoke(&f.g, &limited, "terminal_exec", args.clone(), &request_id())
        .await
        .unwrap();
    assert_eq!(result["error_code"], "account_scope_denied");
    std::sync::Arc::make_mut(&mut f.g.config).devices[0].scopes = vec!["code:read".into()];
    let result = call(&f, "terminal_exec", args).await;
    assert_eq!(result["error_code"], "device_scope_denied");
    assert_eq!(count(&f).await, 0);
}
