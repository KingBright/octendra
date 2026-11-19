#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{ClientConfig, GeminiClient};

#[tokio::test]
async fn usage_preserves_units_and_upstream_freshness_unknown() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    let mut config = ClientConfig::new(env!("CARGO_BIN_EXE_gemini-test-fixture"));
    config.state_dir = Some(state.path().to_owned());
    let client = GeminiClient::connect_with(config).await?;
    let usage = client.usage().await?;
    assert_eq!(usage["provider"], "google");
    assert_eq!(usage["product"], "gemini_desktop");
    assert_eq!(usage["percent_used"], 25);
    assert_eq!(usage["remaining_percent"].as_f64(), Some(75.0));
    assert_eq!(usage["remaining_credits"], 37);
    assert_eq!(usage["staleness"], "unknown");
    assert!(usage["account_scope"].is_null());
    assert!(usage["limit"].is_null());
    assert_eq!(usage["refresh_acknowledged"], false);
    let refreshed = client.usage_with_refresh(true).await?;
    assert_eq!(refreshed["refresh_acknowledged"], true);
    assert_eq!(refreshed["staleness"], "unknown");
    client.close().await?;
    Ok(())
}
