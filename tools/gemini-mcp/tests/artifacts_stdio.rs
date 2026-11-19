#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{Capability, GeminiClient, StartOptions};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

async fn done(c: &GeminiClient, id: &str) -> anyhow::Result<Value> {
    for _ in 0..200 {
        let v = c.observe(id, false).await?;
        if v["status"] != "running" {
            return Ok(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("operation timed out")
}
async fn start(c: &GeminiClient, prompt: &str) -> anyhow::Result<String> {
    let id = Uuid::new_v4().to_string();
    let result = c
        .start_with_options(&StartOptions {
            prompt: prompt.into(),
            request_id: id.clone(),
            capability: Capability::Image,
            timeout_ms: 60000,
        })
        .await?;
    assert_eq!(result["operation_id"], id);
    Ok(id)
}
#[tokio::test]
async fn native_artifact_completion_and_revalidation() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    unsafe { std::env::set_var("GEMINI_DESKTOP_MCP_STATE_DIR", state.path()) };
    let c = GeminiClient::connect(env!("CARGO_BIN_EXE_gemini-test-fixture")).await?;
    let id = start(&c, "fixture-artifact-good").await?;
    let completed = done(&c, &id).await?;
    assert_eq!(completed["status"], "completed");
    assert!(completed.get("prompt").is_none());
    // Existing requests must be recoverable even when capability probing is unavailable.
    std::fs::write(
        state.path().join("fixture-capability-disabled"),
        b"disabled",
    )?;
    let replay = c
        .start_with_options(&StartOptions {
            prompt: "fixture-artifact-good".into(),
            request_id: id.clone(),
            capability: Capability::Image,
            timeout_ms: 60000,
        })
        .await?;
    assert_eq!(replay["status"], "completed");
    assert_eq!(replay["session_id"], completed["session_id"]);
    let log = std::fs::read_to_string(state.path().join("fixture-calls.jsonl"))?;
    let starts = log
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|item| item["command"] == "run" && item["request_id"] == id)
        .count();
    assert_eq!(starts, 1);

    let conflict = c
        .start_with_options(&StartOptions {
            prompt: "different".into(),
            request_id: id.clone(),
            capability: Capability::Image,
            timeout_ms: 60000,
        })
        .await?;
    assert_eq!(conflict["status"], "conflict");
    let new_id = Uuid::new_v4().to_string();
    let blocked = c
        .start_with_options(&StartOptions {
            prompt: "new request".into(),
            request_id: new_id,
            capability: Capability::Image,
            timeout_ms: 60000,
        })
        .await?;
    assert_eq!(blocked["status"], "blocked");
    std::fs::remove_file(state.path().join("fixture-capability-disabled"))?;
    let collected = c.collect(&id).await?;
    assert_eq!(collected["status"], "completed");
    assert!(collected["text"].is_null());
    assert_eq!(collected["artifacts"][0]["kind"], "image");
    let path = collected["artifacts"][0]["path"].as_str().unwrap();
    let mut bytes = std::fs::read(path)?;
    bytes[30] ^= 1;
    std::fs::write(path, &bytes)?;
    let tampered = c.collect(&id).await?;
    assert_eq!(tampered["status"], "unknown");
    assert_eq!(tampered["reason"], "artifact_validation_failed");
    let removed_id = start(&c, "fixture-artifact-good").await?;
    assert_eq!(done(&c, &removed_id).await?["status"], "completed");
    let removed = c.collect(&removed_id).await?;
    std::fs::remove_file(removed["artifacts"][0]["path"].as_str().unwrap())?;
    assert_eq!(
        c.collect(&removed_id).await?["reason"],
        "artifact_validation_failed"
    );
    for prompt in [
        "fixture-artifact-missing",
        "fixture-artifact-foreign",
        "fixture-artifact-foreign-session",
        "fixture-artifact-wrong-type",
        "fixture-artifact-public",
        "fixture-artifact-escape",
        "fixture-artifact-symlink",
        "fixture-artifact-preview",
    ] {
        let id = start(&c, prompt).await?;
        let result = done(&c, &id).await?;
        assert_eq!(result["status"], "unknown", "{prompt}");
        assert_eq!(result["reason"], "artifact_validation_failed", "{prompt}");
        assert!(result.get("artifacts").is_none());
    }
    let empty = start(&c, "fixture-artifact-empty").await?;
    assert_eq!(done(&c, &empty).await?["status"], "unknown");
    c.close().await?;
    unsafe { std::env::remove_var("GEMINI_DESKTOP_MCP_STATE_DIR") };
    Ok(())
}
