use gemini_desktop_mcp::client::GeminiClient;
use serde_json::json;
#[tokio::test]
async fn real_sdk_stdio_handshake_list_and_structured_call() -> anyhow::Result<()> {
    let client = GeminiClient::connect(env!("CARGO_BIN_EXE_gemini-desktop-mcp")).await?;
    let tools = client.tools().await?;
    assert_eq!(tools.len(), 9);
    assert!(tools.contains(&"gemini_usage".to_owned()));
    assert!(tools.contains(&"gemini_focus".to_owned()));
    assert!(tools.contains(&"gemini_start".to_owned()));
    let r = client
        .call("observe", json!({"operation_id":"not-a-uuid"}))
        .await?;
    assert_eq!(r["status"], "invalid_argument_or_unavailable");
    let r = client
        .call("start", json!({"prompt":"hello","capability":"bogus"}))
        .await?;
    assert_eq!(r["status"], "invalid_argument_or_unavailable");
    let missing = client.call("start", json!({"prompt":"hello"})).await?;
    assert_eq!(missing["status"], "invalid_argument_or_unavailable");
    client.close().await?;
    Ok(())
}
