//! Public Octendra presentation metadata; independent from authorization.
use axum::{Json, Router, http::header, routing::get};
use rmcp::model::{Icon, Implementation};
use serde_json::json;

pub(crate) const NAME: &str = "Octendra";
pub(crate) const ICON_PATH: &str = "/branding/octendra-512.png";
pub(crate) const HEAD: &str = "<link rel=icon type='image/png' href='/branding/octendra-192.png'><link rel=apple-touch-icon href='/branding/octendra-192.png'><link rel=manifest href='/manifest.webmanifest'>";
const ICON_192: &[u8] = include_bytes!("../../../assets/brand/octendra/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../../../assets/brand/octendra/icon-512.png");

pub(crate) fn implementation(origin: &str) -> Implementation {
    Implementation::new("octendra-code", env!("CARGO_PKG_VERSION"))
        .with_title(NAME)
        .with_description("Octendra connects your authorized devices, workspaces and terminals.")
        .with_website_url(format!("{origin}/status"))
        .with_icons(vec![
            Icon::new(format!("{origin}{ICON_PATH}"))
                .with_mime_type("image/png")
                .with_sizes(vec!["512x512".into()]),
        ])
}

pub(crate) fn routes() -> Router {
    Router::new()
        .route("/branding/octendra-192.png", get(|| async { png(ICON_192) }))
        .route(ICON_PATH, get(|| async { png(ICON_512) }))
        .route("/favicon.ico", get(|| async { png(ICON_192) }))
        .route("/manifest.webmanifest", get(|| async {
            ([(header::CONTENT_TYPE, "application/manifest+json"),
              (header::CACHE_CONTROL, "public, max-age=3600")],
             Json(json!({
                 "id": "/status", "name": NAME, "short_name": NAME,
                 "start_url": "/status", "scope": "/", "display": "browser",
                 "background_color": "#11151c", "theme_color": "#11151c",
                 "icons": [
                     {"src": "/branding/octendra-192.png", "sizes": "192x192", "type": "image/png"},
                     {"src": ICON_PATH, "sizes": "512x512", "type": "image/png"}
                 ]
             })))
        }))
}

fn png(bytes: &'static [u8]) -> ([(header::HeaderName, &'static str); 2], &'static [u8]) {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        bytes,
    )
}
