#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{Capability, GeminiClient, StartOptions};
use serde_json::Value;
use sha2::{Digest, Sha256};
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
fn opts(id: &str, prompt: &str) -> StartOptions {
    StartOptions {
        prompt: prompt.into(),
        request_id: id.into(),
        capability: Capability::DeepResearch,
        timeout_ms: 60000,
    }
}
fn run_count(state: &Path, id: &str) -> usize {
    std::fs::read_to_string(state.join("fixture-calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["command"] == "run" && v["request_id"] == id)
        .count()
}
#[tokio::test]
async fn research_report_requires_matching_owned_proof_and_revalidates() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    unsafe { std::env::set_var("GEMINI_DESKTOP_MCP_STATE_DIR", state.path()) };
    let exe = env!("CARGO_BIN_EXE_gemini-test-fixture");
    let c = GeminiClient::connect(exe).await?;
    let id = Uuid::new_v4().to_string();
    let request = opts(&id, "fixture-research-valid");
    c.start_with_options(&request).await?;
    let result = until(&c, &id, "completed").await?;
    let text = result["text"].as_str().unwrap();
    let sha = format!("{:x}", Sha256::digest(text.as_bytes()));
    assert_eq!(
        result["completion_evidence"]["kind"],
        "research_report_text"
    );
    assert_eq!(result["completion_evidence"]["request_id"], id);
    assert_eq!(
        result["completion_evidence"]["session_id"],
        result["session_id"]
    );
    assert_eq!(result["completion_evidence"]["text_sha256"], sha);
    let collected = c.collect(&id).await?;
    assert_eq!(collected["text"], text);
    assert_eq!(collected["completion_evidence"]["text_sha256"], sha);
    assert!(collected["artifacts"].as_array().unwrap().is_empty());
    c.close().await?;
    let c = GeminiClient::connect(exe).await?;
    assert_eq!(c.start_with_options(&request).await?["status"], "completed");
    assert_eq!(
        c.collect(&id).await?["completion_evidence"]["text_sha256"],
        sha
    );
    assert_eq!(run_count(state.path(), &id), 1);
    let path = state.path().join(&id).join("operation.json");
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    record["text"] = Value::String("篡改文本".into());
    std::fs::write(&path, serde_json::to_vec(&record)?)?;
    for value in [
        c.observe(&id, false).await?,
        c.start_with_options(&request).await?,
        c.collect(&id).await?,
    ] {
        assert_eq!(value["status"], "unknown");
        assert_eq!(value["reason"], "native_mode_completion_unverified");
        assert!(value.get("text").is_none());
        assert!(value.get("completion_evidence").is_none());
    }
    assert_eq!(run_count(state.path(), &id), 1);
    let old = Uuid::new_v4().to_string();
    let request = opts(&old, "fixture-research-valid");
    c.start_with_options(&request).await?;
    until(&c, &old, "completed").await?;
    let path = state.path().join(&old).join("operation.json");
    let mut record: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    record
        .as_object_mut()
        .unwrap()
        .remove("completion_evidence");
    std::fs::write(path, serde_json::to_vec(&record)?)?;
    assert_eq!(c.observe(&old, false).await?["status"], "unknown");
    assert_eq!(c.start_with_options(&request).await?["status"], "unknown");
    assert_eq!(c.collect(&old).await?["status"], "unknown");
    assert_eq!(run_count(state.path(), &old), 1);
    for prompt in [
        "fixture-research-absent",
        "fixture-research-foreign-request",
        "fixture-research-foreign-session",
        "fixture-research-wrong-hash",
        "fixture-research-wrong-kind",
        "fixture-research-empty",
    ] {
        let id = Uuid::new_v4().to_string();
        c.start_with_options(&opts(&id, prompt)).await?;
        let value = until(&c, &id, "unknown").await?;
        assert_eq!(
            value["reason"], "native_mode_completion_unverified",
            "{prompt}"
        );
        assert!(value.get("text").is_none());
        assert!(value.get("completion_evidence").is_none());
        assert_eq!(run_count(state.path(), &id), 1);
    }
    c.close().await?;
    unsafe { std::env::remove_var("GEMINI_DESKTOP_MCP_STATE_DIR") };
    Ok(())
}
