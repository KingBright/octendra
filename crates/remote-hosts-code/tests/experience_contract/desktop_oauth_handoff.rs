//! Scratch fixtures only; no production credentials, network, desktop or deployment.
use super::*;

fn rpc(method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
}
fn desktop_args(f: &Fixture) -> Value {
    json!({"device_id":f.g.config.devices[0].id,"task_id":"00000000-0000-4000-8000-000000000005",
        "capability_version":1,"idempotency_key":"stable-desktop-handoff",
        "request":serde_json::to_value(remote_hosts_code::computer_use::Request::Capabilities {}).unwrap()})
}
async fn call(f: &Fixture, bearer: &str, name: &str, args: Value) -> (StatusCode, Value) {
    f.request(
        "POST",
        "/mcp",
        rpc("tools/call", json!({"name":name,"arguments":args})),
        bearer,
        &[],
    )
    .await
}
fn challenge(v: &Value) -> &Value {
    &v["result"]["_meta"]["mcp/www_authenticate"]
}

#[tokio::test]
async fn desktop_oauth_handoff_discovery_has_top_level_and_mirrored_schemes_without_grants() {
    let f = Fixture::new().await;
    let bearer = f.bearer().await;
    let (status, v) = f
        .request("POST", "/mcp", rpc("tools/list", json!({})), &bearer, &[])
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let tools = v["result"]["tools"].as_array().unwrap();
    for (name, scopes) in [
        (
            "desktop_worker",
            json!(["desktop:observe", "desktop:input"]),
        ),
        ("desktop_frame", json!(["desktop:observe"])),
        ("devices_list", json!(["code:read"])),
    ] {
        let tool = tools.iter().find(|t| t["name"] == name).unwrap();
        assert_eq!(
            tool["securitySchemes"],
            json!([{"type":"oauth2","scopes":scopes}])
        );
        assert_eq!(tool["securitySchemes"], tool["_meta"]["securitySchemes"]);
    }
    assert!(
        !contract::authorized_tools(&f.p)
            .iter()
            .any(|t| t.name.starts_with("desktop_"))
    );
    assert!(
        f.g.config
            .devices
            .iter()
            .all(|d| !d.scopes.iter().any(|s| s.starts_with("desktop:")))
    );
    assert_eq!(f.count().await, 0);
}

#[tokio::test]
async fn desktop_oauth_handoff_missing_scope_is_stable_and_never_replays_or_changes_grants() {
    let f = Fixture::new().await;
    let bearer = f.bearer().await;
    let kind = oauth_state_kind("access", &bearer);
    let before: Value = f.g.store.get(&kind, &hash(&bearer)).await.unwrap().unwrap();
    let mut previous = Value::Null;
    for _ in 0..5 {
        let (status, v) = call(&f, &bearer, "desktop_worker", desktop_args(&f)).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["result"]["isError"], true);
        let auth = challenge(&v);
        assert_eq!(auth.as_array().unwrap().len(), 1);
        let auth = auth[0].as_str().unwrap();
        assert!(auth.contains("error=\"insufficient_scope\""));
        assert!(auth.contains("error_description="));
        assert!(auth.contains(
            "resource_metadata=\"https://fixture.example/.well-known/oauth-protected-resource\""
        ));
        assert!(auth.contains(
            "scope=\"code:read code:write terminal:exec desktop:observe desktop:input\""
        ));
        assert_eq!(v["result"]["structuredContent"]["automatic_replay"], false);
        assert_eq!(v["result"]["structuredContent"]["execution_started"], false);
        if !previous.is_null() {
            assert_eq!(&previous, challenge(&v));
        }
        previous = challenge(&v).clone();
    }
    let after: Value = f.g.store.get(&kind, &hash(&bearer)).await.unwrap().unwrap();
    assert_eq!(before, after);
    assert_eq!(f.count().await, 0);
}

#[tokio::test]
async fn desktop_oauth_handoff_observe_and_input_are_requested_in_one_consent() {
    let mut f = Fixture::new().await;
    f.p.scopes.push("desktop:input".into());
    let bearer = f.bearer().await;
    let (_, v) = call(&f, &bearer, "desktop_worker", desktop_args(&f)).await;
    assert!(
        challenge(&v)[0]
            .as_str()
            .unwrap()
            .contains("desktop:observe")
    );
    assert_eq!(
        v["result"]["structuredContent"]["required_scopes"],
        json!(["desktop:observe", "desktop:input"])
    );
    assert_eq!(f.count().await, 0);
}

#[tokio::test]
async fn desktop_oauth_handoff_existing_scope_device_policy_and_unknown_tool_do_not_relink() {
    let mut f = Fixture::new().await;
    f.p.scopes
        .extend(["desktop:observe".into(), "desktop:input".into()]);
    let bearer = f.bearer().await;
    for _ in 0..3 {
        let (status, v) = call(&f, &bearer, "desktop_worker", desktop_args(&f)).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["result"]["isError"], true);
        assert!(v.to_string().contains("device_scope_denied"), "{v}");
        assert!(challenge(&v).is_null(), "{v}");
    }
    let (_, v) = call(&f, &bearer, "unknown_tool", json!({})).await;
    assert!(challenge(&v).is_null());
    assert_eq!(f.count().await, 0);
}

#[tokio::test]
async fn desktop_oauth_handoff_wrong_resource_owner_and_revocation_stay_transport_denials() {
    for boundary in ["resource", "owner", "revoked"] {
        let f = Fixture::new().await;
        let bearer = f.bearer().await;
        let kind = oauth_state_kind("access", &bearer);
        let key = hash(&bearer);
        let mut access: Value = f.g.store.get(&kind, &key).await.unwrap().unwrap();
        match boundary {
            "resource" => access["resource"] = json!("https://wrong.example/mcp"),
            "owner" => access["principal"]["owner"] = json!("another-owner"),
            _ => {
                f.g.store
                    .put(
                        "revoked",
                        access["family"].as_str().unwrap(),
                        &true,
                        now() + 100,
                    )
                    .await
                    .unwrap()
            }
        }
        f.g.store
            .put(&kind, &key, &access, now() + 100)
            .await
            .unwrap();
        let (status, v) = call(&f, &bearer, "desktop_worker", desktop_args(&f)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{boundary}: {v}");
        assert!(challenge(&v).is_null(), "{v}");
        assert_eq!(f.count().await, 0);
    }
}
