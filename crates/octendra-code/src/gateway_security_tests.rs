//! Exercise the actual HTTP boundary without a live gateway or credentials.
use super::*;
use axum::body::{Body, to_bytes};
use tower::ServiceExt;

fn router() -> Router {
    let config = GatewayConfig {
        public_url_aliases: vec![],
        public_url: "https://fixture.invalid:8443".into(),
        bind: "127.0.0.1:0".into(),
        state_dir: PathBuf::new(),
        forge_browser: None,
        owner: "fixture".into(),
        password_hash: "unused".into(),
        devices: vec![],
        redirect_uris: vec![],
        allowed_origins: vec!["https://fixture.invalid".into()],
    };
    Router::new()
        .fallback(|| async { "accepted" })
        .layer(middleware::from_fn_with_state(
            Arc::new(config),
            security_headers,
        ))
}

#[tokio::test]
async fn host_rejections_explain_boundary_without_echoing_headers() {
    for host in [None, Some("proxy.internal"), Some("fixture.invalid")] {
        let mut request = Request::builder().uri("/mcp");
        if let Some(host) = host {
            request = request.header(header::HOST, host);
        }
        let response = router()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error":"gateway_host_mismatch"})
        );
    }
}

#[tokio::test]
async fn denied_or_invalid_origins_remain_denied_with_distinct_errors() {
    for (origin, code) in [
        (
            axum::http::HeaderValue::from_static("https://untrusted.invalid"),
            "browser_origin_denied",
        ),
        (
            axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
            "browser_origin_invalid",
        ),
    ] {
        let request = Request::builder()
            .uri("/oauth/approve")
            .header(header::HOST, "fixture.invalid:8443")
            .header(header::ORIGIN, origin)
            .body(Body::empty())
            .unwrap();
        let response = router().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"error":code})
        );
    }
}

#[tokio::test]
async fn exact_public_host_accepts_explicit_alias_origin_and_nonbrowser_clients() {
    for origin in [
        None,
        Some("https://fixture.invalid"),
        Some("https://fixture.invalid:8443"),
    ] {
        let mut request = Request::builder()
            .uri("/mcp")
            .header(header::HOST, "fixture.invalid:8443");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let response = router()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 1024).await.unwrap(),
            "accepted"
        );
    }
}
