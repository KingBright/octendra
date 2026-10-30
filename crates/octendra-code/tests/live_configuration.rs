use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use octendra_code::{GatewayConfig, gateway::Gateway, hash, random};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn request(
    router: &Router,
    path: &str,
    token: Option<&str>,
    origin: Option<&str>,
) -> StatusCode {
    let mut request = Request::builder()
        .uri(path)
        .header("host", "fixture.example");
    let body = if let Some(token) = token {
        request = request
            .method("POST")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json");
        json!({"version":"0.12.4","session":hash("fixture-session"),"roots":["/tmp"],"allow_write":true,"allow_exec":true,"poll_wait_ms":100}).to_string()
    } else {
        String::new()
    };
    if let Some(origin) = origin {
        request = request.header("origin", origin);
    }
    router
        .clone()
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn existing_router_loads_devices_and_clients_and_keeps_last_valid_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.json");
    let token = random();
    let mut document = json!({"public_url":"https://fixture.example","bind":"127.0.0.1:0","state_dir":dir.path().join("state"),
        "owner":"fixture","password_hash":"unused","devices":[],"redirect_uris":[],"allowed_origins":[]});
    std::fs::write(&path, document.to_string()).unwrap();
    let config: GatewayConfig = serde_json::from_value(document.clone()).unwrap();
    let gateway = Gateway::new_with_config_path(config, path.clone())
        .await
        .unwrap();
    let router = gateway.router().unwrap(); // Deliberately never rebuild this router.
    assert_eq!(
        request(&router, "/device/poll", Some(&token), None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        request(
            &router,
            "/.well-known/oauth-authorization-server",
            None,
            Some("https://grok.com")
        )
        .await,
        StatusCode::FORBIDDEN
    );
    document["devices"] = json!([{"id":uuid::Uuid::new_v4(),"name":"new device","token_hash":hash(&token),"scopes":["code:read","code:write","terminal:exec"]}]);
    document["allowed_origins"] = json!(["https://grok.com"]);
    document["redirect_uris"] = json!(["https://grok.com/connectors-oauth-exchange-code/"]);
    std::fs::write(&path, document.to_string()).unwrap();
    assert_eq!(
        request(&router, "/device/poll", Some(&token), None).await,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &router,
            "/.well-known/oauth-authorization-server",
            None,
            Some("https://grok.com")
        )
        .await,
        StatusCode::OK
    );
    // A partial editor save, or an unapproved listener change, leaves active clients usable.
    std::fs::write(&path, "{").unwrap();
    assert_eq!(
        request(&router, "/device/heartbeat", Some(&token), None).await,
        StatusCode::NO_CONTENT
    );
    document["bind"] = Value::String("0.0.0.0:19999".into());
    document["devices"] = json!([]);
    std::fs::write(&path, document.to_string()).unwrap();
    assert_eq!(
        request(&router, "/device/heartbeat", Some(&token), None).await,
        StatusCode::NO_CONTENT
    );
}
