use super::*;
use crate::{DeviceRegistration, gateway::Job};

async fn fixture() -> (tempfile::TempDir, Gateway, Principal) {
    let dir = tempfile::tempdir().unwrap();
    let devices: Vec<_> = ["Studio", "Studio guest", "Other guest"]
        .into_iter()
        .map(|name| DeviceRegistration {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            token_hash: crate::hash(crate::random()),
            scopes: vec!["code:read".into(), "terminal:exec".into()],
        })
        .collect();
    let workspace = UiWorkspace {
        id: uuid::Uuid::new_v4().to_string(),
        name: "Studio AI workspace".into(),
        host_device_id: devices[0].id.clone(),
        execution_device_id: devices[1].id.clone(),
    };
    let gateway = Gateway::new(GatewayConfig {
        ui_workspaces: vec![workspace],
        forge_browser: None,
        public_url: "https://fixture.invalid".into(),
        public_url_aliases: vec![],
        bind: "127.0.0.1:0".into(),
        state_dir: dir.path().join("gateway"),
        owner: "test".into(),
        password_hash: "unused".into(),
        devices,
        redirect_uris: vec![],
        allowed_origins: vec![],
    })
    .await
    .unwrap();
    let principal = Principal {
        owner: "test".into(),
        scopes: vec!["code:read".into(), "terminal:exec".into()],
    };
    (dir, gateway, principal)
}

#[tokio::test]
async fn ui_defaults_to_distinct_guest_and_missing_workspace_never_selects_host() {
    let (_dir, mut gateway, principal) = fixture().await;
    for target in ["Studio", "Studio guest", "Studio AI workspace"] {
        let mut args = json!({"device":target});
        let context = resolve(&gateway, &principal, "ui_observe", &mut args)
            .await
            .unwrap();
        assert_eq!(args["device"], gateway.config.devices[1].id);
        assert_eq!(context.isolation, Isolation::FullVm);
        assert_ne!(context.host_device_id, context.execution_device_id);
    }
    std::sync::Arc::make_mut(&mut gateway.config)
        .ui_workspaces
        .clear();
    assert!(
        gateway
            .dispatch(&principal, "ui_observe", json!({"device":"Studio"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("ui_workspace_unavailable")
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
        .fetch_one(&gateway.store.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let mut args = json!({"device":"Studio","context":"host"});
    let context = resolve(&gateway, &principal, "ui_observe", &mut args)
        .await
        .unwrap();
    assert_eq!(context.isolation, Isolation::HostSession);
    assert_eq!(args["device"], gateway.config.devices[0].id);
    assert!(args.get("context").is_none());
}

#[tokio::test]
async fn ui_job_recovery_and_input_keep_original_context_without_remapping() {
    let (_dir, mut gateway, principal) = fixture().await;
    let guest = gateway.config.devices[1].id.clone();
    gateway.store.put("online", &guest, &json!({"last_seen":crate::now(),
        "hello":{"session":"fixture","roots":[],"allow_write":true,"allow_exec":true,"platform":"macos"},
        "runtime_features":{"protocol":1,"names":["unified_ui_v1"]}}), i64::MAX).await.unwrap();
    let pending = gateway
        .dispatch(&principal, "ui_observe", json!({"device":"Studio"}))
        .await
        .unwrap();
    assert_eq!(pending["execution_context"]["execution_device_id"], guest);
    assert_eq!(pending["execution_context"]["isolation"], "full_vm");
    let operation = pending["operation_id"].as_str().unwrap();
    let request: String = sqlx::query_scalar("SELECT request FROM jobs WHERE id=?")
        .bind(operation)
        .fetch_one(&gateway.store.pool)
        .await
        .unwrap();
    let job: Job = serde_json::from_str(&request).unwrap();
    assert_eq!(job.device_id, guest);
    assert_eq!(job.arguments["device"], guest);
    let observation = uuid::Uuid::new_v4().to_string();
    sqlx::query("UPDATE jobs SET result=?,state='completed' WHERE id=?")
        .bind(json!({"observation_id":observation,"device":guest}).to_string())
        .bind(operation)
        .execute(&gateway.store.pool)
        .await
        .unwrap();
    let recovered = gateway.result(&principal, operation).await.unwrap();
    assert_eq!(recovered["execution_context"], pending["execution_context"]);
    let args = json!({"device":"Studio","observation_id":observation,
        "action":{"type":"key","key":"Enter"},"idempotency_key":"same-input"});
    let input = gateway
        .dispatch(&principal, "ui_act", args.clone())
        .await
        .unwrap();
    assert_eq!(input["device_id"], guest);
    let retry = gateway
        .dispatch(&principal, "ui_act", args.clone())
        .await
        .unwrap();
    assert_eq!(retry["operation_id"], input["operation_id"]);
    let mut wrong_device = args.clone();
    wrong_device["device"] = json!("Other guest");
    assert!(
        resolve(&gateway, &principal, "ui_act", &mut wrong_device)
            .await
            .unwrap_err()
            .to_string()
            .contains("ui_observation_device_mismatch")
    );
    let other = Principal {
        owner: "other-owner".into(),
        scopes: principal.scopes.clone(),
    };
    assert!(
        resolve(&gateway, &other, "ui_act", &mut args.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("ui_observation_missing")
    );
    let replacement = gateway.config.devices[2].id.clone();
    std::sync::Arc::make_mut(&mut gateway.config).ui_workspaces[0].execution_device_id =
        replacement;
    let retry_after_change = gateway
        .dispatch(&principal, "ui_act", args.clone())
        .await
        .unwrap();
    assert_eq!(retry_after_change["operation_id"], input["operation_id"]);
    let mut new_request = args;
    new_request["idempotency_key"] = json!("new-request");
    assert!(
        gateway
            .dispatch(&principal, "ui_act", new_request)
            .await
            .unwrap_err()
            .to_string()
            .contains("ui_execution_context_changed")
    );
    let host_jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE device=?")
        .bind(&gateway.config.devices[0].id)
        .fetch_one(&gateway.store.pool)
        .await
        .unwrap();
    assert_eq!(host_jobs, 0);
}

#[tokio::test]
async fn full_vm_registration_rejects_self_aliases_and_missing_guests() {
    let (_dir, gateway, _principal) = fixture().await;
    let mut config = (*gateway.config).clone();
    config.ui_workspaces[0].execution_device_id = config.devices[0].id.clone();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("distinct guest")
    );
    config.ui_workspaces[0].execution_device_id = uuid::Uuid::new_v4().to_string();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("not registered")
    );
    config.ui_workspaces[0].execution_device_id = config.devices[1].id.clone();
    config.ui_workspaces[0].name = config.devices[0].name.clone();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
    );
}
