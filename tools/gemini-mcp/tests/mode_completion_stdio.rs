#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{Capability, GeminiClient, StartOptions};
use serde_json::Value;
use std::time::Duration;
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
#[tokio::test]
async fn mode_completion_requires_matching_native_proof() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    unsafe { std::env::set_var("GEMINI_DESKTOP_MCP_STATE_DIR", dir.path()) };
    let c = GeminiClient::connect(env!("CARGO_BIN_EXE_gemini-test-fixture")).await?;
    for cap in [Capability::Image, Capability::Video] {
        for prompt in [
            "mode-text-start",
            "mode-wrong-start",
            "mode-text-resume",
            "mode-wrong-resume",
        ] {
            let id = Uuid::new_v4().to_string();
            let options = StartOptions {
                prompt: prompt.into(),
                request_id: id.clone(),
                capability: cap.clone(),
                timeout_ms: 60000,
            };
            assert_eq!(c.start_with_options(&options).await?["operation_id"], id);
            if prompt.ends_with("resume") {
                until(&c, &id, "waiting_human").await?;
                c.observe(&id, true).await?;
            }
            let result = until(&c, &id, "unknown").await?;
            assert_eq!(
                result["reason"], "native_mode_completion_unverified",
                "{cap:?} {prompt}"
            );
            assert!(result.get("text").is_none());
            assert!(result.get("artifacts").is_none());
            assert_eq!(c.collect(&id).await?["status"], "unknown");
        }
    }
    for cap in [Capability::DeepResearch, Capability::Canvas] {
        let id = Uuid::new_v4().to_string();
        c.start_with_options(&StartOptions {
            prompt: "mode-text-start".into(),
            request_id: id.clone(),
            capability: cap,
            timeout_ms: 60000,
        })
        .await?;
        assert_eq!(
            until(&c, &id, "unknown").await?["reason"],
            "native_mode_completion_unverified"
        );
    }
    c.close().await?;
    unsafe { std::env::remove_var("GEMINI_DESKTOP_MCP_STATE_DIR") };
    Ok(())
}
