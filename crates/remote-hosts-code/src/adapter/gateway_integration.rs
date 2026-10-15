//! Actual Adapter bootstrap/HTTP forwarding against the real Gateway router/store.
//! Loopback HTTP is test-only. Production connect still requires verified HTTPS.
//! Client disconnect models abandoning consent; it does not prove a hosted OAuth UI.
use super::*;
use crate::{
    DeviceRegistration, GatewayConfig,
    auth::{Principal, oauth_state_kind},
    gateway::Gateway,
    hash, random,
};
use axum::{
    body::{Body, to_bytes},
    http::Request,
    middleware::{self, Next},
};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Fixture {
    _dir: tempfile::TempDir,
    g: Gateway,
    adapter: Adapter,
    raw_catalog: Value,
    access: Value,
    calls: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Fixture {
    async fn new(desktop_scopes: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let device = DeviceRegistration {
            id: uuid::Uuid::new_v4().to_string(),
            name: "fixture-device".into(),
            token_hash: hash(random()),
            scopes: vec!["code:read".into()],
        };
        let g = Gateway::new(GatewayConfig {
            forge_browser: None,
            public_url: "https://fixture.example".into(),
            bind: "127.0.0.1:0".into(),
            state_dir: dir.path().join("gateway"),
            owner: "fixture".into(),
            password_hash: "unused".into(),
            redirect_uris: vec![],
            allowed_origins: crate::default_mcp_client_origins(),
            devices: vec![device],
        })
        .await
        .unwrap();
        let mut p = Principal {
            owner: "fixture".into(),
            scopes: vec!["code:read".into()],
        };
        if desktop_scopes {
            p.scopes
                .extend(["desktop:observe".into(), "desktop:input".into()]);
        }
        let token = random();
        let access = json!({"principal":p,"client_id":"test-client","resource":"https://fixture.example/mcp","family":random()});
        g.store
            .put(
                &oauth_state_kind("access", &token),
                &hash(&token),
                &access,
                now() + 100,
            )
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let router = g.router().unwrap().layer(middleware::from_fn(
            move |request: Request<Body>, next: Next| {
                let counted = counted.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let bytes = to_bytes(body, MAX_RESPONSE).await.unwrap();
                    if serde_json::from_slice::<Value>(&bytes)
                        .ok()
                        .is_some_and(|v| v["method"] == "tools/call")
                    {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    next.run(Request::from_parts(parts, Body::from(bytes)))
                        .await
                }
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("fixture.example"));
        let state_dir = dir.path().join("adapter");
        std::fs::create_dir(&state_dir).unwrap();
        let mut adapter = Adapter {
            client: reqwest::Client::builder()
                .no_proxy()
                .default_headers(headers)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            origin,
            token: Arc::from(token),
            protocol: "2025-06-18".into(),
            session: None,
            catalog: Vec::new(),
            state_dir,
            host_report_file: None,
        };
        let raw_catalog = adapter.bootstrap().await.unwrap();
        // The same validation path used by production connect, not a preloaded fake catalog.
        adapter.accept_catalog(&raw_catalog).unwrap();
        Self {
            _dir: dir,
            g,
            adapter,
            raw_catalog,
            access,
            calls,
            server,
        }
    }
    fn params(&self) -> CallToolRequestParams {
        serde_json::from_value(json!({"name":"desktop_worker","arguments":{
            "device_id":self.g.config.devices[0].id,"task_id":"00000000-0000-4000-8000-000000000005",
            "capability_version":1,"idempotency_key":"fixture-consent-handoff",
            "request":serde_json::to_value(crate::computer_use::Request::Capabilities {}).unwrap()
        }})).unwrap()
    }
    async fn no_dispatch_or_grant_change(&self, expected_calls: usize) {
        assert_eq!(self.calls.load(Ordering::SeqCst), expected_calls);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&self.g.store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let access: Value = self
            .g
            .store
            .get(
                &oauth_state_kind("access", &self.adapter.token),
                &hash(&*self.adapter.token),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(access, self.access);
        assert!(
            !self.g.config.devices[0]
                .scopes
                .iter()
                .any(|s| s.starts_with("desktop:"))
        );
    }
}
struct Stdio {
    reader: BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    server: tokio::task::JoinHandle<()>,
}
impl Stdio {
    async fn connect(adapter: Adapter) -> Self {
        let (server_io, client_io) = tokio::io::duplex(128 * 1024);
        let server = tokio::spawn(async move {
            let (read, write) = tokio::io::split(server_io);
            crate::oauth_handoff::Wire(adapter)
                .serve((read, write))
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let (read, writer) = tokio::io::split(client_io);
        let mut s = Self {
            reader: BufReader::new(read),
            writer,
            server,
        };
        let init = s.request(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}})).await;
        assert!(init.get("result").is_some(), "{init}");
        s.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        s
    }
    async fn send(&mut self, value: Value) {
        self.writer
            .write_all((value.to_string() + "\n").as_bytes())
            .await
            .unwrap();
    }
    async fn request(&mut self, value: Value) -> Value {
        self.send(value).await;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.reader.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn close(self) {
        drop(self.writer);
        drop(self.reader);
        tokio::time::timeout(std::time::Duration::from_secs(5), self.server)
            .await
            .unwrap()
            .unwrap();
    }
}
fn assert_scope_challenge(value: &Value, _principal: &Principal) {
    assert_eq!(value["isError"], true);
    assert_eq!(value["structuredContent"]["error_code"], "device_offline");
    assert_eq!(value["structuredContent"]["execution_started"], false);
    assert!(value["_meta"]["mcp/www_authenticate"].is_null());
}
#[tokio::test]
async fn real_gateway_adapter_stdio_preserves_metadata_and_missing_scope_challenge() {
    let f = Fixture::new(false).await;
    let mut s = Stdio::connect(f.adapter.clone()).await;
    let listed = s
        .request(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    // Compare raw wire metadata from both actual transports, including every protected tool.
    assert_eq!(listed["result"]["tools"], f.raw_catalog["tools"]);
    for tool in listed["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["securitySchemes"], tool["_meta"]["securitySchemes"]);
        assert!(tool["securitySchemes"].is_array());
    }
    let called = s
        .request(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":f.params()}))
        .await;
    let principal: Principal = serde_json::from_value(f.access["principal"].clone()).unwrap();
    assert_scope_challenge(&called["result"], &principal);
    s.close().await;
    f.no_dispatch_or_grant_change(1).await;
}
#[tokio::test]
async fn real_gateway_adapter_client_abandons_consent_without_replay_or_dispatch() {
    let f = Fixture::new(false).await;
    let mut s = Stdio::connect(f.adapter.clone()).await;
    let called = s
        .request(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":f.params()}))
        .await;
    let principal: Principal = serde_json::from_value(f.access["principal"].clone()).unwrap();
    assert_scope_challenge(&called["result"], &principal);
    // Cancellation at this boundary means no new token and no further client request.
    s.close().await;
    f.no_dispatch_or_grant_change(1).await;
}
#[tokio::test]
async fn real_gateway_adapter_device_scope_refusal_never_becomes_oauth_or_dispatch() {
    let f = Fixture::new(true).await;
    let result = serde_json::to_value(f.adapter.forward(f.params()).await).unwrap();
    assert_eq!(result["isError"], true);
    assert!(
        result.to_string().contains("device_offline"),
        "{result}"
    );
    assert!(result["_meta"]["mcp/www_authenticate"].is_null());
    f.no_dispatch_or_grant_change(1).await;
}
#[tokio::test]
async fn real_gateway_adapter_revoked_access_is_refused_without_retry_or_dispatch() {
    let f = Fixture::new(false).await;
    f.g.store
        .put(
            "revoked",
            f.access["family"].as_str().unwrap(),
            &true,
            now() + 100,
        )
        .await
        .unwrap();
    let result = serde_json::to_value(f.adapter.forward(f.params()).await).unwrap();
    assert_eq!(
        result["structuredContent"]["error"],
        "upstream_authorization_rejected"
    );
    assert_eq!(
        result["structuredContent"]["execution_state"],
        "not_started"
    );
    assert!(result["_meta"]["mcp/www_authenticate"].is_null());
    f.no_dispatch_or_grant_change(1).await;
}
