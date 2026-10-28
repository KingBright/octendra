use super::*;

struct Fixture {
    _dir: tempfile::TempDir,
    gateway: Gateway,
    principal: Principal,
    old_device: String,
    new_device: String,
    operation: String,
    workspace: String,
    original_request: String,
    original_result: String,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let old_device = uuid::Uuid::new_v4().to_string();
        let new_device = uuid::Uuid::new_v4().to_string();
        let operation = uuid::Uuid::new_v4().to_string();
        let workspace = format!("{new_device}:{}", uuid::Uuid::new_v4());
        let principal = Principal {
            owner: "current-owner".into(),
            scopes: vec!["code:read".into(), "terminal:exec".into()],
        };
        let gateway = Gateway::new(GatewayConfig {
            public_url: "https://fixture.invalid".into(),
            bind: "127.0.0.1:0".into(),
            state_dir: dir.path().join("state"),
            owner: principal.owner.clone(),
            password_hash: "unused".into(),
            forge_browser: None,
            redirect_uris: vec![],
            allowed_origins: vec![],
            devices: vec![DeviceRegistration {
                id: new_device.clone(),
                name: "current device".into(),
                token_hash: hash("current-device-token"),
                scopes: principal.scopes.clone(),
            }],
        })
        .await
        .unwrap();
        let original_request = serde_json::to_string(&Job { id: operation.clone(), device_id: old_device.clone(),
            owner: "original-owner".into(), tool: "terminal_exec".into(),
            arguments: json!({"workspace_id":format!("{old_device}:{}",uuid::Uuid::new_v4()),"command":"printf historical"}) }).unwrap();
        let original_result = json!({"state":"exited","output":"historical output\n","exit_code":0,"output_complete":true}).to_string();
        sqlx::query("INSERT INTO jobs(id,device,idem,fingerprint,request,result,state,updated) VALUES(?,?,?,'original-fingerprint',?,?,'done',?)")
            .bind(&operation).bind(&old_device).bind(format!("original-{operation}"))
            .bind(&original_request).bind(&original_result).bind(now()).execute(&gateway.store.pool).await.unwrap();
        Self {
            _dir: dir,
            gateway,
            principal,
            old_device,
            new_device,
            operation,
            workspace,
            original_request,
            original_result,
        }
    }
    fn grant(&self) -> Value {
        json!({"protocol":1,"owner":self.principal.owner,"source_owner":"original-owner",
            "source_device_id":self.old_device,"recovery_device_id":self.new_device,
            "recovery_workspace_id":self.workspace,"scopes":["code:read","terminal:exec"]})
    }
    async fn put_grant(&self, grant: Value) {
        self.gateway
            .store
            .put("imported_history", &self.old_device, &grant, i64::MAX)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn imported_receipt_is_observable_without_rewriting_original_evidence() {
    let f = Fixture::new().await;
    assert!(f.gateway.result(&f.principal, &f.operation).await.is_err());
    f.put_grant(f.grant()).await;
    let observed = f
        .gateway
        .dispatch(
            &f.principal,
            "operation_get",
            json!({"operation_id":f.operation,"response_mode":"full"}),
        )
        .await
        .unwrap();
    assert_eq!(observed["output"], "historical output\n");
    assert_eq!(observed["device_id"], f.old_device);
    assert_eq!(observed["history_migration"]["read_only"], true);
    assert_eq!(
        observed["history_migration"]["recovery_workspace_id"],
        f.workspace
    );
    let saved: (String, String, String) =
        sqlx::query_as("SELECT request,result,fingerprint FROM jobs WHERE id=?")
            .bind(&f.operation)
            .fetch_one(&f.gateway.store.pool)
            .await
            .unwrap();
    assert_eq!(
        saved,
        (
            f.original_request,
            f.original_result,
            "original-fingerprint".into()
        )
    );
}

#[tokio::test]
async fn history_grant_does_not_authenticate_retired_device_or_dispatch_commands() {
    let f = Fixture::new().await;
    f.put_grant(f.grant()).await;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        "Bearer original-device-token".parse().unwrap(),
    );
    assert!(f.gateway.device(&headers).await.is_err());
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&f.gateway.store.pool)
        .await
        .unwrap();
    assert!(f.gateway.dispatch(&f.principal,"terminal_exec",json!({"workspace_id":format!("{}:historical",f.old_device),"command":"must not execute","idempotency_key":"forbidden-history-dispatch"})).await.is_err());
    assert_eq!(
        before,
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(&f.gateway.store.pool)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn owner_device_protocol_and_both_scope_boundaries_remain_required() {
    let f = Fixture::new().await;
    for (field, value) in [
        ("owner", json!("other-owner")),
        ("source_owner", json!("wrong-source-owner")),
        ("source_device_id", json!(uuid::Uuid::new_v4())),
        ("protocol", json!(2)),
        ("recovery_device_id", json!(uuid::Uuid::new_v4())),
        ("recovery_workspace_id", json!("wrong:workspace")),
        ("scopes", json!([])),
    ] {
        let mut grant = f.grant();
        grant[field] = value;
        f.put_grant(grant).await;
        assert!(
            f.gateway.result(&f.principal, &f.operation).await.is_err(),
            "{field}"
        );
    }
    f.put_grant(f.grant()).await;
    let readonly = Principal {
        owner: f.principal.owner.clone(),
        scopes: vec!["code:read".into()],
    };
    assert!(f.gateway.result(&readonly, &f.operation).await.is_ok());
    let unauthorized = Principal {
        owner: f.principal.owner.clone(),
        scopes: vec![],
    };
    assert!(f.gateway.result(&unauthorized, &f.operation).await.is_err());
    let mut reduced = f.gateway.clone();
    Arc::make_mut(&mut reduced.config).devices[0].scopes = vec![];
    assert!(reduced.result(&f.principal, &f.operation).await.is_err());
}
