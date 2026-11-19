#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{Capability, GeminiClient, StartOptions};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use uuid::Uuid;

async fn until(c: &GeminiClient, id: &str, target: &str) -> anyhow::Result<Value> {
    for _ in 0..250 {
        let v = c.observe(id, false).await?;
        if v["status"] == target {
            return Ok(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("wanted {target}, got {}", c.observe(id, false).await?)
}
fn count(state: &Path, id: &str, command: &str) -> usize {
    std::fs::read_to_string(state.join("fixture-calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["request_id"] == id && v["command"] == command)
        .count()
}
fn options(id: &str, prompt: &str, capability: Capability) -> StartOptions {
    StartOptions {
        prompt: prompt.into(),
        request_id: id.into(),
        capability,
        timeout_ms: 60000,
    }
}
#[tokio::test]
async fn legacy_media_and_explicit_failure_are_fail_closed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    unsafe { std::env::set_var("GEMINI_DESKTOP_MCP_STATE_DIR", dir.path()) };
    let c = GeminiClient::connect(env!("CARGO_BIN_EXE_gemini-test-fixture")).await?;
    for capability in [Capability::Image, Capability::Video] {
        let id = Uuid::new_v4().to_string();
        let options = options(&id, "mode-text-start", capability);
        c.start_with_options(&options).await?;
        until(&c, &id, "unknown").await?;
        let path = dir.path().join(&id).join("operation.json");
        let mut legacy: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        legacy["status"] = json!("completed");
        legacy["text"] = json!("No image was produced");
        legacy.as_object_mut().unwrap().remove("artifacts");
        legacy.as_object_mut().unwrap().remove("failure_observed");
        std::fs::write(path, serde_json::to_vec(&legacy)?)?;
        for result in [
            c.observe(&id, false).await?,
            c.start_with_options(&options).await?,
            c.collect(&id).await?,
        ] {
            assert_eq!(result["status"], "unknown");
            assert_eq!(result["reason"], "native_mode_completion_unverified");
            assert_eq!(result["operation_id"], id);
            assert!(result.get("text").is_none());
        }
        assert_eq!(count(dir.path(), &id, "run"), 1);
        assert_eq!(count(dir.path(), &id, "resume"), 0);
    }
    for prompt in ["mode-failed-start", "mode-failed-resume"] {
        let id = Uuid::new_v4().to_string();
        let options = options(&id, prompt, Capability::Image);
        c.start_with_options(&options).await?;
        if prompt.ends_with("resume") {
            until(&c, &id, "waiting_human").await?;
            c.observe(&id, true).await?;
        }
        let failed = until(&c, &id, "failed").await?;
        assert_eq!(failed["reason"], "native_error_response");
        assert_eq!(failed["failure_observed"], true);
        assert_eq!(failed["text"], "The native image request failed.");
        assert_eq!(c.start_with_options(&options).await?["status"], "failed");
        assert_eq!(c.collect(&id).await?["status"], "failed");
        assert_eq!(c.cancel(&id).await?["status"], "failed");
        assert_eq!(c.observe(&id, true).await?["status"], "failed");
        let next = Uuid::new_v4().to_string();
        let blocked = c.continue_turn(&id, "new prompt", &next).await?;
        assert_eq!(blocked["status"], "blocked");
        assert_eq!(blocked["reason"], "prior_turn_not_completed");
        assert_eq!(count(dir.path(), &id, "run"), 1);
        assert_eq!(count(dir.path(), &id, "cancel"), 0);
        assert_eq!(
            count(dir.path(), &id, "resume"),
            usize::from(prompt.ends_with("resume"))
        );
    }
    for prompt in [
        "mode-failure-unproved",
        "mode-failure-wrong-reason",
        "mode-failure-foreign",
        "mode-failure-contradictory",
        "mode-failure-no-diagnostic",
    ] {
        let id = Uuid::new_v4().to_string();
        c.start_with_options(&options(&id, prompt, Capability::Image))
            .await?;
        let result = until(&c, &id, "unknown").await?;
        assert_ne!(result["status"], "failed");
        assert!(result.get("text").is_none());
    }
    for (prompt, capability) in [
        ("mode-proof-completed-and-failed", Capability::Text),
        ("mode-proof-not-submitted-and-failed", Capability::Text),
        ("mode-proof-cancelled-and-failed", Capability::Text),
        ("mode-proof-completed-and-not-submitted", Capability::Text),
        ("mode-proof-failed-and-not-submitted", Capability::Text),
        ("mode-proof-malformed-flag", Capability::Text),
        ("fixture-artifact-contradictory", Capability::Image),
    ] {
        let id = Uuid::new_v4().to_string();
        let request = options(&id, prompt, capability);
        c.start_with_options(&request).await?;
        let result = until(&c, &id, "unknown").await?;
        assert_eq!(result["reason"], "native_proof_contradiction", "{prompt}");
        assert!(result.get("text").is_none());
        assert!(result.get("artifacts").is_none());
        assert_eq!(c.start_with_options(&request).await?["status"], "unknown");
        assert_eq!(c.collect(&id).await?["status"], "unknown");
        assert_eq!(count(dir.path(), &id, "run"), 1);
    }
    c.close().await?;
    unsafe { std::env::remove_var("GEMINI_DESKTOP_MCP_STATE_DIR") };
    Ok(())
}
