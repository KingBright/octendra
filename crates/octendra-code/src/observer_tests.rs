//! Permission fixtures only: no desktop, Portal or production device access.
use super::*;

async fn fixture() -> (tempfile::TempDir, Gateway, Principal, Value) {
    let dir = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let g = Gateway::new(GatewayConfig {
        public_url_aliases: vec![],
        public_url: "https://fixture.invalid".into(),
        bind: "127.0.0.1:0".into(),
        state_dir: dir.path().join("gateway"),
        forge_browser: None,
        owner: "fixture".into(),
        password_hash: "unused".into(),
        devices: vec![DeviceRegistration {
            id: id.clone(),
            name: "fixture".into(),
            token_hash: hash("fixture"),
            scopes: vec!["desktop:observe".into()],
        }],
        redirect_uris: vec![],
        allowed_origins: vec![],
    })
    .await
    .unwrap();
    g.store.put("online", &id, &json!({
        "hello": {"session":"fixture", "roots":[], "allow_write":false,
                  "allow_exec":false,"platform":"linux"},
        "last_seen":now(), "runtime_features":crate::capabilities::RuntimeFeatures::current()
    }), i64::MAX).await.unwrap();
    let p = Principal {
        owner: "fixture".into(),
        scopes: vec!["desktop:observe".into()],
    };
    let args = json!({"device_id":id,"task_id":uuid::Uuid::new_v4(),"capability_version":7,"request":{"operation":"capabilities"},"idempotency_key":"observer-fixture"});
    (dir, g, p, args)
}

#[tokio::test]
async fn observer_gateway_admission_and_results_require_observe_but_not_input() {
    let (_dir, mut g, p, args) = fixture().await;
    let denied = Principal {
        scopes: vec!["desktop:input".into()],
        ..p.clone()
    };
    assert!(
        g.dispatch(&denied, "desktop_observer", args.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("insufficient_scope")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(&g.store.pool)
            .await
            .unwrap(),
        0
    );
    let result = g
        .dispatch(&p, "desktop_observer", args.clone())
        .await
        .unwrap();
    let op = result["operation_id"].as_str().unwrap();
    assert!(g.result(&p, op).await.is_ok());
    assert!(
        g.result(&denied, op)
            .await
            .unwrap_err()
            .to_string()
            .contains("insufficient_scope")
    );
    assert!(
        g.dispatch(&denied, "operation_get", json!({"operation_id":op}))
            .await
            .is_err()
    );
    assert!(
        g.dispatch(
            &denied,
            "operation_get",
            json!({"operation_ids":[op],"wait_ms":0})
        )
        .await
        .is_err()
    );
    g.store
        .put(
            "task_operation",
            op,
            &json!({"owner":p.owner,"task_id":args["task_id"]}),
            i64::MAX,
        )
        .await
        .unwrap();
    assert!(
        crate::activity::task(&g, &denied, &json!({"task_id":args["task_id"]}))
            .await
            .is_err()
    );
    sqlx::query("UPDATE jobs SET result=?,state='done' WHERE id=?")
        .bind(json!({"state":"observed","fixture_marker":"private"}).to_string())
        .bind(op)
        .execute(&g.store.pool)
        .await
        .unwrap();
    assert_eq!(g.result(&p, op).await.unwrap()["fixture_marker"], "private");
    assert!(g.result(&denied, op).await.is_err());
    Arc::make_mut(&mut g.config).devices[0].scopes = vec!["desktop:input".into()];
    assert!(
        g.result(&p, op)
            .await
            .unwrap_err()
            .to_string()
            .contains("device_scope_denied")
    );
    assert!(
        g.dispatch(&p, "desktop_observer", args)
            .await
            .unwrap_err()
            .to_string()
            .contains("device_scope_denied")
    );
}

#[tokio::test]
async fn observer_gateway_rejects_old_agents_before_queueing() {
    let (_dir, g, p, args) = fixture().await;
    let device = args["device_id"].as_str().unwrap();
    let mut online: Value = g.store.get("online", device).await.unwrap().unwrap();
    online["runtime_features"]["names"] = json!([]);
    g.store
        .put("online", device, &online, i64::MAX)
        .await
        .unwrap();
    assert!(
        g.dispatch(&p, "desktop_observer", args.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("device_feature_unavailable")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs")
            .fetch_one(&g.store.pool)
            .await
            .unwrap(),
        0
    );
}
