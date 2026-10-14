use super::*;
use crate::{DeviceRegistration, GatewayConfig};

async fn fixture() -> (
    tempfile::TempDir,
    Agent,
    crate::gateway::Gateway,
    Workspace,
    tokio::task::JoinHandle<()>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let device = uuid::Uuid::new_v4().to_string();
    let token = crate::random();
    let mut a = Agent::new(AgentConfig {
        gateway_url: "https://fixture.invalid".into(),
        device_id: device.clone(),
        device_token: token.clone(),
        state_dir: tmp.path().join("agent"),
        roots: vec![root.clone()],
        allow_write: true,
        allow_exec: false,
        shell: "/bin/sh".into(),
    })
    .await
    .unwrap();
    let g = crate::gateway::Gateway::new(GatewayConfig {
        forge_browser: None,
        public_url: "https://fixture.invalid".into(),
        bind: "127.0.0.1:0".into(),
        state_dir: tmp.path().join("gateway"),
        owner: "owner".into(),
        password_hash: crate::hash("unused"),
        devices: vec![DeviceRegistration {
            id: device.clone(),
            name: "fixture".into(),
            token_hash: crate::hash(&token),
            scopes: vec![
                "code:read".into(),
                "code:write".into(),
                "terminal:exec".into(),
            ],
        }],
        redirect_uris: vec![],
        allowed_origins: crate::default_mcp_client_origins(),
    })
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    Arc::make_mut(&mut a.config).gateway_url = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::code_payload::routes(g.clone())
        .merge(crate::gateway::payload_poll_test_route(g.clone()));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let ws = Workspace {
        id: format!("{}:{}", device, uuid::Uuid::new_v4()),
        device_id: device,
        root: root.canonicalize().unwrap(),
    };
    a.store
        .put("workspace", &ws.id, &ws, i64::MAX)
        .await
        .unwrap();
    (tmp, a, g, ws, server)
}
fn original(a: &Agent, ws: &Workspace, path: &str) -> Job {
    let source = include_str!("../../remote-hosts-connector/src/lib.rs");
    assert!(source.len() > 512 * 1024);
    eprintln!("real_source_size={}", source.len());
    Job {
        id: uuid::Uuid::new_v4().to_string(),
        device_id: a.config.device_id.clone(),
        owner: "owner".into(),
        tool: "code_apply_edits".into(),
        arguments: json!({"workspace_id":ws.id,"idempotency_key":"payload-fixture","files":[{"path":path,"action":"create","expected_version":"absent","content":source}]}),
    }
}
async fn save(g: &crate::gateway::Gateway, job: &Job) {
    sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,NULL,'dispatched',?)")
        .bind(&job.id).bind(&job.device_id).bind(&job.id).bind(crate::hash(serde_json::to_vec(&job.arguments).unwrap()))
        .bind(serde_json::to_string(job).unwrap()).bind(crate::now()).execute(&g.store.pool).await.unwrap();
}
fn wire(job: &Job, reconcile: bool) -> Job {
    serde_json::from_value(
        crate::code_payload::envelope(job, true, reconcile).unwrap()["job"].clone(),
    )
    .unwrap()
}
#[tokio::test]
async fn real_source_chunked_atomic_cas_and_duplicate_receipt() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "large.rs");
    save(&g, &job).await;
    let descriptor = wire(&job, false);
    assert!(crate::payload_budget::poll_bytes(&descriptor).unwrap() < 2048);
    let d = Delivery::new(a.store.clone(), a.config.clone())
        .await
        .unwrap();
    a.execute_report(&d, descriptor.clone()).await.unwrap();
    let data = std::fs::read(ws.root.join("large.rs")).unwrap();
    assert_eq!(
        crate::hash(&data),
        crate::hash(include_bytes!("../../remote-hosts-connector/src/lib.rs"))
    );
    let op = a
        .store
        .get::<LocalOperation>("local_operation", &job.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        op.result.as_ref().unwrap()["change_set"]["change_set_id"],
        job.id
    );
    let journal = a
        .store
        .get::<crate::transfer_journal::Journal>("transfer_local", &job.id)
        .await
        .unwrap()
        .unwrap();
    assert!(journal.data_cleaned);
    assert!(!journal.data_path(&a.config).exists());
    a.execute_report(&d, descriptor).await.unwrap();
    assert_eq!(std::fs::read(ws.root.join("large.rs")).unwrap(), data);
    server.abort();
}
#[tokio::test]
async fn real_source_verified_checkpoint_resumes_without_rewriting_destination() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "resume.rs");
    save(&g, &job).await;
    let descriptor = wire(&job, false);
    let bytes = serde_json::to_vec(&job.arguments).unwrap();
    let mut transfer = descriptor.clone();
    transfer.arguments = json!({"path":".remote-hosts-code-payload","sha256":crate::hash(&bytes),"max_bytes":bytes.len(),"workspace_id":ws.id});
    let mut j = crate::transfer_journal::Journal::load_or_create(
        &a.config,
        &ws,
        &transfer,
        &a.store,
        a.scheduler.writes.clone(),
    )
    .await
    .unwrap();
    let prefix = &bytes[..128 * 1024];
    crate::write_private(&j.data_path(&a.config), prefix).unwrap();
    j.offset = prefix.len() as u64;
    j.total = Some(bytes.len() as u64);
    j.prefix_sha256 = crate::hash(prefix);
    j.etag = Some(format!("\"{}\"", crate::hash(&bytes)));
    j.save(&a.store).await.unwrap();
    assert!(!ws.root.join("resume.rs").exists());
    let hydrated =
        crate::code_payload::hydrate(&a.config, &a.store, a.scheduler.writes.clone(), &descriptor)
            .await
            .unwrap();
    assert_eq!(
        serde_json::to_vec(&hydrated).unwrap(),
        serde_json::to_vec(&job).unwrap()
    );
    assert!(!ws.root.join("resume.rs").exists());
    let d = Delivery::new(a.store.clone(), a.config.clone())
        .await
        .unwrap();
    a.execute_report(&d, descriptor).await.unwrap();
    assert_eq!(
        std::fs::metadata(ws.root.join("resume.rs")).unwrap().len(),
        765943
    );
    server.abort();
}
#[tokio::test]
async fn legacy_oversize_converges_only_from_device_negative_acceptance() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "never.rs");
    save(&g, &job).await;
    sqlx::query("INSERT INTO semantic_guards VALUES('legacy-guard',?,'active',?)")
        .bind(&job.id)
        .bind(crate::now())
        .execute(&g.store.pool)
        .await
        .unwrap();
    let d = Delivery::new(a.store.clone(), a.config.clone())
        .await
        .unwrap();
    a.execute_report(&d, wire(&job, true)).await.unwrap();
    assert!(!ws.root.join("never.rs").exists());
    let op = a
        .store
        .get::<LocalOperation>("local_operation", &job.id)
        .await
        .unwrap()
        .unwrap();
    let result = op.result.unwrap();
    assert_eq!(result["device_acceptance"], "not_accepted");
    assert_eq!(result["execution_state"], "not_started");
    assert_eq!(
        crate::job_receipts::commit(&g.store, &job.device_id, &job.id, &result)
            .await
            .unwrap(),
        crate::job_receipts::Outcome::Accepted { duplicate: false }
    );
    let (state,guards):(String,i64)=sqlx::query_as("SELECT state,(SELECT COUNT(*) FROM semantic_guards WHERE operation_id=?) FROM jobs WHERE id=?")
        .bind(&job.id).bind(&job.id).fetch_one(&g.store.pool).await.unwrap();
    assert_eq!(state, "done");
    assert_eq!(guards, 0);
    server.abort();
}
#[tokio::test]
async fn accepted_and_started_jobs_never_become_negative_acceptance() {
    let (_tmp, a, _g, ws, server) = fixture().await;
    let d = Delivery::new(a.store.clone(), a.config.clone())
        .await
        .unwrap();
    for accepted in [false, true] {
        let job = original(&a, &ws, "accepted.rs");
        a.store
            .put(
                "local_operation",
                &job.id,
                &LocalOperation {
                    fingerprint: crate::hash(serde_json::to_vec(&job).unwrap()),
                    state: "running".into(),
                    result: None,
                    gateway_accepted: accepted,
                    resumable: false,
                    workspace_id: Some(ws.id.clone()),
                    tool: Some(job.tool.clone()),
                    updated_at: crate::now(),
                },
                i64::MAX,
            )
            .await
            .unwrap();
        a.reconcile_payload(&d, &job).await.unwrap();
        let op = a
            .store
            .get::<LocalOperation>("local_operation", &job.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(op.state, "running");
        assert!(op.result.is_none());
        if !accepted {
            let payload: String =
                sqlx::query_scalar("SELECT payload FROM receipt_outbox WHERE id=?")
                    .bind(&job.id)
                    .fetch_one(&a.store.pool)
                    .await
                    .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&payload).unwrap()["result"]["error"],
                "outcome_unknown"
            );
        }
    }
    server.abort();
}
#[tokio::test]
async fn payload_auth_scope_conflict_and_path_boundaries() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "conflict.rs");
    save(&g, &job).await;
    let url = format!("{}/device/code-payload/{}", a.config.gateway_url, job.id);
    assert_eq!(reqwest::get(&url).await.unwrap().status().as_u16(), 403);
    assert_eq!(
        reqwest::Client::new()
            .get(&url)
            .bearer_auth(crate::random())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    std::fs::write(ws.root.join("conflict.rs"), "preserve").unwrap();
    let d = Delivery::new(a.store.clone(), a.config.clone())
        .await
        .unwrap();
    a.execute_report(&d, wire(&job, false)).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(ws.root.join("conflict.rs")).unwrap(),
        "preserve"
    );
    let escaping = original(&a, &ws, "../escape.rs");
    save(&g, &escaping).await;
    a.execute_report(&d, wire(&escaping, false)).await.unwrap();
    assert!(!ws.root.parent().unwrap().join("escape.rs").exists());
    server.abort();
}

#[tokio::test]
async fn payload_receive_integrity_failure_finishes_before_execution() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "integrity.rs");
    save(&g, &job).await;
    let mut descriptor = wire(&job, false);
    descriptor.arguments["code_payload"]["sha256"] = json!("0".repeat(64));
    let d = Delivery::new(a.store.clone(), a.config.clone())
        .await
        .unwrap();
    a.execute_report(&d, descriptor).await.unwrap();
    let op = a
        .store
        .get::<LocalOperation>("local_operation", &job.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(op.result.unwrap()["execution_state"], "not_started");
    assert!(!ws.root.join("integrity.rs").exists());
    server.abort();
}
#[tokio::test]
async fn payload_admission_rejection_has_durable_request_without_remote_job() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let principal = crate::auth::Principal {
        owner: "owner".into(),
        scopes: vec!["code:write".into(), "terminal:exec".into()],
    };
    let job = original(&a, &ws, "oversized.rs");
    let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
    let result = crate::receipts::invoke(
        &g,
        &principal,
        "code_apply_edits",
        job.arguments,
        &request_id,
    )
    .await
    .unwrap();
    assert_eq!(result["error_code"], "payload_too_large");
    assert_eq!(result["execution_state"], "not_started");
    let saved: Value = g
        .store
        .get("request_receipt", &request_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["state"], "gateway_rejected");
    assert!(saved["operation_id"].is_null());
    let terminal_request = format!("req_{}", uuid::Uuid::new_v4().simple());
    let result=crate::receipts::invoke(&g,&principal,"terminal_exec",json!({"workspace_id":ws.id,"idempotency_key":"oversized-command","command":"x".repeat(65537)}),&terminal_request).await.unwrap();
    assert_eq!(result["error_code"], "payload_too_large");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(&g.store.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    server.abort();
}

#[tokio::test]
async fn source_submission_close_is_durable_and_preserves_existing_operation() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let p = crate::auth::Principal {
        owner: "owner".into(),
        scopes: vec!["code:write".into()],
    };
    let args = json!({"resolution":"close_not_enqueued","workspace_id":ws.id,"original_idempotency_key":"missing-batch","idempotency_key":"close-missing-batch"});
    let result = crate::receipts::invoke(
        &g,
        &p,
        "outcome_resolve",
        args.clone(),
        &format!("req_{}", uuid::Uuid::new_v4().simple()),
    )
    .await
    .unwrap();
    assert_eq!(result["state"], "closed_not_enqueued");
    assert_eq!(result["submission_receipt"]["acceptance"], "not_enqueued");
    assert_eq!(result["submission_receipt"]["key_sealed"], true);
    let idem = crate::submission_fence::identity("owner", &ws.id, "missing-batch").unwrap();
    let mut tx = g.store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(
        crate::submission_fence::require_open(&mut tx, &idem)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut conflicting = args.clone();
    conflicting["original_idempotency_key"] = json!("different-target");
    assert!(
        crate::submission_fence::close(&g, &p, &conflicting)
            .await
            .is_err()
    );
    let duplicate = crate::submission_fence::close(&g, &p, &args).await.unwrap();
    assert_eq!(duplicate["duplicate"], true);
    let job = original(&a, &ws, "existing.rs");
    save(&g, &job).await;
    let active_key = "active-batch";
    let active_idem = crate::submission_fence::identity("owner", &ws.id, active_key).unwrap();
    sqlx::query("UPDATE jobs SET idem=? WHERE id=?")
        .bind(&active_idem)
        .bind(&job.id)
        .execute(&g.store.pool)
        .await
        .unwrap();
    let result = crate::submission_fence::close(
        &g,
        &p,
        &json!({"workspace_id":ws.id,"original_idempotency_key":active_key,"idempotency_key":"close-active"}),
    )
    .await
    .unwrap();
    assert_eq!(result["state"], "existing_operation");
    assert_eq!(result["operation_id"], job.id);
    assert_eq!(result["submission_closed"], false);
    let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE id=?")
        .bind(&job.id)
        .fetch_one(&g.store.pool)
        .await
        .unwrap();
    assert_eq!(state, "dispatched");
    server.abort();
}
#[tokio::test]
async fn source_submission_racing_admission_wins_without_false_nonacceptance() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "racing.rs");
    let key = "racing-admission";
    let idem = crate::submission_fence::identity("owner", &ws.id, key).unwrap();
    let mut tx = g.store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    crate::submission_fence::require_open(&mut tx, &idem)
        .await
        .unwrap();
    sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,?,?,NULL,'queued',?)")
        .bind(&job.id).bind(&job.device_id).bind(&idem).bind("fp").bind(serde_json::to_string(&job).unwrap()).bind(crate::now()).execute(&mut *tx).await.unwrap();
    let copy = g.clone();
    let args = json!({"workspace_id":ws.id,"original_idempotency_key":key,"idempotency_key":"close-racing"});
    let closing = tokio::spawn(async move {
        crate::submission_fence::close(
            &copy,
            &crate::auth::Principal {
                owner: "owner".into(),
                scopes: vec!["code:write".into()],
            },
            &args,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    tx.commit().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result["state"], "existing_operation");
    assert_eq!(result["operation_id"], job.id);
    assert!(
        g.store
            .get::<Value>("submission_fence", &idem)
            .await
            .unwrap()
            .is_none()
    );
    server.abort();
}

fn payload_poll_hello(
    a: &Agent,
    ws: &Workspace,
    features: Option<crate::capabilities::RuntimeFeatures>,
) -> serde_json::Value {
    json!({
        "version":"0.10.32",
        "wire_protocol":crate::capabilities::WIRE_PROTOCOL,
        "session":"f".repeat(64),
        "platform":"linux",
        "arch":"x86_64",
        "roots":[ws.root],
        "allow_write":a.config.allow_write,
        "allow_exec":a.config.allow_exec,
        "runtime_features":features,
        "poll_wait_ms":100
    })
}

async fn payload_poll_http(a: &Agent, request: &serde_json::Value) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("{}/device/poll", a.config.gateway_url))
        .bearer_auth(&a.config.device_token)
        .json(request)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn payload_current_agent_features_poll_all_lanes_and_legacy_shape() {
    let (_tmp, a, g, ws, server) = fixture().await;
    for lane in Lane::ALL {
        let mut request = payload_poll_hello(
            &a,
            &ws,
            Some(crate::capabilities::RuntimeFeatures::current()),
        );
        request["lanes"] = json!([lane]);
        assert_eq!(
            payload_poll_http(&a, &request).await,
            reqwest::StatusCode::OK,
            "current agent lane {lane:?}"
        );
    }
    let online = g
        .store
        .get::<serde_json::Value>("online", &a.config.device_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(online["hello"]["version"], "0.10.32");
    assert!(crate::code_payload::supported(&online["runtime_features"]));
    let request = payload_poll_hello(&a, &ws, None);
    assert_eq!(
        payload_poll_http(&a, &request).await,
        reqwest::StatusCode::OK
    );
    server.abort();
}

#[tokio::test]
async fn payload_feature_registration_uses_bytes_not_feature_count() {
    let (_tmp, a, g, ws, server) = fixture().await;
    // Unknown optional names survive discovery and do not grant a desktop scope.
    let features = crate::capabilities::RuntimeFeatures {
        protocol: 1,
        names: (0..300).map(|i| format!("future_feature_{i}")).collect(),
    };
    let mut request = payload_poll_hello(&a, &ws, Some(features));
    assert_eq!(
        payload_poll_http(&a, &request).await,
        reqwest::StatusCode::OK
    );
    let online = g
        .store
        .get::<Value>("online", &a.config.device_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        online["runtime_features"]["names"]
            .as_array()
            .unwrap()
            .len(),
        300
    );
    assert!(
        !g.config.devices[0]
            .scopes
            .iter()
            .any(|v| v.starts_with("desktop:"))
    );
    request["session"] = json!("b".repeat(64));
    // Actual encoded bytes, not the number of features, define the resource boundary.
    request["runtime_features"]["names"] = json!(vec!["x".repeat(64); 8192]);
    assert!(serde_json::to_vec(&request).unwrap().len() > crate::payload_budget::POLL_BYTES);
    assert_eq!(
        payload_poll_http(&a, &request).await,
        reqwest::StatusCode::PAYLOAD_TOO_LARGE
    );
    request["runtime_features"]["names"] = json!(["Invalid-feature"]);
    assert_eq!(
        payload_poll_http(&a, &request).await,
        reqwest::StatusCode::BAD_REQUEST
    );
    request["runtime_features"]["names"] = json!(["x".repeat(65)]);
    assert_eq!(
        payload_poll_http(&a, &request).await,
        reqwest::StatusCode::BAD_REQUEST
    );
    let online = g
        .store
        .get::<Value>("online", &a.config.device_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(online["hello"]["session"], "f".repeat(64));
    server.abort();
}

#[tokio::test]
async fn payload_structured_unknown_keeps_guard_and_official_resolution_is_atomic() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "unwritten.rs");
    save(&g, &job).await;
    sqlx::query("INSERT INTO semantic_guards VALUES('unknown-guard',?,'active',?)")
        .bind(&job.id)
        .bind(crate::now())
        .execute(&g.store.pool)
        .await
        .unwrap();
    let value = crate::diagnostics::error(
        "code_apply_edits",
        "unspecified device failure",
        Some(&job.id),
        "agent_execute",
    );
    crate::job_receipts::commit(&g.store, &job.device_id, &job.id, &value)
        .await
        .unwrap();
    let guard: String =
        sqlx::query_scalar("SELECT state FROM semantic_guards WHERE operation_id=?")
            .bind(&job.id)
            .fetch_one(&g.store.pool)
            .await
            .unwrap();
    assert_eq!(guard, "outcome_unknown");
    let p = crate::auth::Principal {
        owner: "owner".into(),
        scopes: vec!["code:write".into()],
    };
    let args = json!({"operation_id":job.id,"resolution":"verified_not_applied","idempotency_key":"resolve-test"});
    sqlx::query("CREATE TRIGGER reject_resolution BEFORE INSERT ON kv WHEN NEW.kind='outcome_resolution' BEGIN SELECT RAISE(ABORT,'injected'); END")
        .execute(&g.store.pool).await.unwrap();
    assert!(
        crate::outcome_resolution::resolve(&g, &p, &args)
            .await
            .is_err()
    );
    let guards: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM semantic_guards WHERE operation_id=?")
            .bind(&job.id)
            .fetch_one(&g.store.pool)
            .await
            .unwrap();
    assert_eq!(guards, 1);
    sqlx::query("DROP TRIGGER reject_resolution")
        .execute(&g.store.pool)
        .await
        .unwrap();
    let result = crate::receipts::invoke(
        &g,
        &p,
        "outcome_resolve",
        args.clone(),
        &format!("req_{}", uuid::Uuid::new_v4().simple()),
    )
    .await
    .unwrap();
    assert_eq!(result["state"], "resolved");
    assert_eq!(result["guard_was_present"], true);
    assert_eq!(result["replayed"], false);
    assert_eq!(
        crate::outcome_resolution::resolve(&g, &p, &args)
            .await
            .unwrap()["duplicate"],
        true
    );
    let mut conflicting = args.clone();
    conflicting["resolution"] = json!("verified_applied");
    assert!(
        crate::outcome_resolution::resolve(&g, &p, &conflicting)
            .await
            .is_err()
    );
    let foreign = crate::auth::Principal {
        owner: "foreign".into(),
        scopes: vec!["code:write".into()],
    };
    assert!(
        crate::outcome_resolution::resolve(&g, &foreign, &args)
            .await
            .is_err()
    );
    let raw: String = sqlx::query_scalar("SELECT result FROM jobs WHERE id=?")
        .bind(&job.id)
        .fetch_one(&g.store.pool)
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), value);
    assert!(!ws.root.join("unwritten.rs").exists());
    server.abort();
}
#[tokio::test]
async fn payload_legacy_missing_guard_resolves_without_clearing_another_operation() {
    let (_tmp, a, g, ws, server) = fixture().await;
    let job = original(&a, &ws, "never-written.rs");
    save(&g, &job).await;
    let value = crate::diagnostics::error(
        "code_apply_edits",
        "unspecified device failure",
        Some(&job.id),
        "agent_execute",
    );
    crate::job_receipts::commit(&g.store, &job.device_id, &job.id, &value)
        .await
        .unwrap();
    sqlx::query("INSERT INTO semantic_guards VALUES('other-guard','other-operation','active',0)")
        .execute(&g.store.pool)
        .await
        .unwrap();
    let p = crate::auth::Principal {
        owner: "owner".into(),
        scopes: vec!["code:write".into()],
    };
    let args = json!({"operation_id":job.id,"resolution":"verified_not_applied","idempotency_key":"legacy-resolve"});
    let r = crate::outcome_resolution::resolve(&g, &p, &args)
        .await
        .unwrap();
    assert_eq!(r["guard_was_present"], false);
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM semantic_guards WHERE operation_id='other-operation'",
    )
    .fetch_one(&g.store.pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
    let known = original(&a, &ws, "known.rs");
    save(&g, &known).await;
    crate::job_receipts::commit(
        &g.store,
        &known.device_id,
        &known.id,
        &json!({"error":"file_not_found"}),
    )
    .await
    .unwrap();
    let mut wrong = args.clone();
    wrong["operation_id"] = json!(known.id);
    let e = crate::outcome_resolution::resolve(&g, &p, &wrong)
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "outcome_not_unknown");
    let d = crate::diagnostics::error("outcome_resolve", &e.to_string(), None, "gateway_dispatch");
    assert_eq!(d["outcome"], "not_executed");
    assert_eq!(d["error_code"], "outcome_not_unknown");
    assert!(!ws.root.join("never-written.rs").exists());
    server.abort();
}

#[tokio::test]
async fn payload_new_agent_legacy_gateway_omits_only_optional_features_once() {
    let (_tmp, mut a, _g, ws, original_server) = fixture().await;
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let observed = seen.clone();
    let route = axum::Router::new().route(
        "/device/poll",
        axum::routing::post(move |axum::Json(value): axum::Json<Value>| {
            let observed = observed.clone();
            async move {
                observed.lock().unwrap().push(value.clone());
                if value["runtime_features"]["names"]
                    .as_array()
                    .is_some_and(|v| v.len() > 32)
                {
                    return (
                        reqwest::StatusCode::BAD_REQUEST,
                        axum::Json(json!({"legacy_validation":true})),
                    );
                }
                (reqwest::StatusCode::OK, axum::Json(json!({"job":null})))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    Arc::make_mut(&mut a.config).gateway_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, route).await.unwrap();
    });
    let mut hello: DeviceHello = serde_json::from_value(payload_poll_hello(&a, &ws, None)).unwrap();
    hello.version = env!("CARGO_PKG_VERSION").into();
    let client = reqwest::Client::new();
    assert!(
        a.poll_job(&client, &hello, Lane::Read)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        a.poll_job(&client, &hello, Lane::Read)
            .await
            .unwrap()
            .is_none()
    );
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0]["runtime_features"]["names"]
            .as_array()
            .unwrap()
            .len()
            > 32
    );
    assert!(requests[1].get("runtime_features").is_none());
    assert!(requests[2].get("runtime_features").is_none());
    let mut first = requests[0].clone();
    first.as_object_mut().unwrap().remove("runtime_features");
    assert_eq!(first, requests[1]);
    assert_eq!(requests[1]["allow_exec"], false);
    assert_eq!(requests[1]["roots"], json!([ws.root]));
    drop(requests);
    server.abort();
    original_server.abort();
}
