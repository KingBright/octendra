#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{Capability, ClientConfig, GeminiClient, StartOptions};
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn explicit_plan_confirmation_is_bound_and_never_replayed() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    let mut config = ClientConfig::new(env!("CARGO_BIN_EXE_gemini-test-fixture"));
    config.state_dir = Some(state.path().to_path_buf());
    let client = GeminiClient::connect_with(config.clone()).await?;
    let usage = client.usage().await?;
    assert_eq!(usage["status"], "unknown");
    assert!(usage["remaining_credits"].is_null());
    assert_eq!(usage["scope"], "unspecified");
    assert_eq!(
        client.usage_with_refresh(true).await?["status"],
        "unsupported"
    );
    let id = Uuid::new_v4().to_string();
    client
        .start_with_options(&StartOptions {
            prompt: "fixture-research-plan".into(),
            request_id: id.clone(),
            capability: Capability::DeepResearch,
            timeout_ms: 60000,
        })
        .await?;
    let mut ready = None;
    for _ in 0..250 {
        let value = client.observe(&id, false).await?;
        if value["status"] == "waiting_human" {
            ready = Some(value);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let value = ready.ok_or_else(|| anyhow::anyhow!("research plan missing"))?;
    let version = value["pending_action"]["action_version"].as_str().unwrap();
    assert_eq!(
        client.confirm(&id, "wrong", version).await?["status"],
        "blocked"
    );
    assert_eq!(
        client
            .confirm(&id, "deep_research_start", &"b".repeat(64))
            .await?["status"],
        "blocked"
    );
    client.confirm(&id, "deep_research_start", version).await?;
    client.close().await?;
    let client = GeminiClient::connect_with(config).await?;
    for _ in 0..250 {
        if client.observe(&id, false).await?["status"] != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        client.confirm(&id, "deep_research_start", version).await?["action"],
        "confirmation_already_attempted_no_replay"
    );
    let log = std::fs::read_to_string(state.path().join("fixture-calls.jsonl"))?;
    let calls: Vec<serde_json::Value> = log
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|v: &serde_json::Value| v["command"] == "confirm")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["action_version"], version);
    client.close().await?;
    Ok(())
}
