#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{Capability, GeminiClient, StartOptions};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use uuid::Uuid;

async fn until(c: &GeminiClient, id: &str, wanted: &str) -> anyhow::Result<Value> {
    for _ in 0..200 {
        let v = c.observe(id, false).await?;
        if v["status"] == wanted {
            return Ok(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("did not reach {wanted}: {}", c.observe(id, false).await?)
}
async fn binding(state: &Path, id: &str) -> anyhow::Result<()> {
    let hash = format!("{:x}", Sha256::digest(id.as_bytes()));
    let path = state
        .join(id)
        .join("native")
        .join(format!("gemini-{hash}.json"));
    for _ in 0..200 {
        if path.is_file() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("no fixture binding")
}
fn count(state: &Path, id: &str, command: &str) -> usize {
    let log = std::fs::read_to_string(state.join("fixture-calls.jsonl")).unwrap_or_default();
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["request_id"] == id && v["command"] == command)
        .count()
}
#[tokio::test]
async fn detached_worker_survives_restart_and_safe_lifecycle() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    unsafe { std::env::set_var("GEMINI_DESKTOP_MCP_STATE_DIR", state.path()) };
    let exe = env!("CARGO_BIN_EXE_gemini-test-fixture");
    let id = Uuid::new_v4().to_string();
    let options = StartOptions {
        prompt: "first".into(),
        request_id: id.clone(),
        capability: Capability::Text,
        timeout_ms: 54321,
    };
    let c = GeminiClient::connect(exe).await?;
    let first = c.start_with_options(&options).await?;
    assert_eq!(first["operation_id"], id);
    assert_eq!(first["timeout_ms"], 54321);
    c.close().await?;
    let c = GeminiClient::connect(exe).await?;
    let duplicate = c.start_with_options(&options).await?;
    assert_eq!(duplicate["session_id"], first["session_id"]);
    until(&c, &id, "waiting_human").await?;
    assert_eq!(count(state.path(), &id, "run"), 1);
    c.observe(&id, true).await?;
    until(&c, &id, "completed").await?;
    let next = Uuid::new_v4().to_string();
    let follow = c.continue_turn(&id, "follow up", &next).await?;
    assert_eq!(follow["session_id"], first["session_id"]);
    until(&c, &next, "completed").await?;
    assert_eq!(c.collect(&next).await?["text"], "fixture answer");
    let cancel_id = Uuid::new_v4().to_string();
    c.start("cancel example", &cancel_id).await?;
    binding(state.path(), &cancel_id).await?;
    c.cancel(&cancel_id).await?;
    until(&c, &cancel_id, "cancelled").await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(c.observe(&cancel_id, false).await?["status"], "cancelled");
    let foreign = Uuid::new_v4().to_string();
    c.start("foreign-receipt", &foreign).await?;
    let bad = until(&c, &foreign, "unknown").await?;
    assert_eq!(bad["reason"], "native_receipt_identity_mismatch");
    let lost = Uuid::new_v4().to_string();
    c.start("fixture-bound-window-lost", &lost).await?;
    let recovery = until(&c, &lost, "unknown").await?;
    assert_eq!(recovery["reason"], "bound_window_lost");
    assert_eq!(
        recovery["recovery"],
        "when_safe_quit_original_app_then_resume"
    );
    assert!(recovery.get("prompt").is_none());
    c.close().await?;
    let c = GeminiClient::connect(exe).await?;
    let recovery = c.observe(&lost, false).await?;
    assert_eq!(
        recovery["recovery"],
        "when_safe_quit_original_app_then_resume"
    );
    assert!(recovery.get("prompt").is_none());
    let dead = Uuid::new_v4().to_string();
    c.start("dead-worker", &dead).await?;
    binding(state.path(), &dead).await?;
    let record: Value = serde_json::from_slice(&std::fs::read(
        state.path().join(&dead).join("operation.json"),
    )?)?;
    let pid = record["worker_pid"].as_i64().unwrap() as i32;
    let token = record["worker_token"].as_str().unwrap();
    let process = std::process::Command::new("/bin/ps")
        .args([
            "-ww",
            "-p",
            &pid.to_string(),
            "-o",
            "stat=",
            "-o",
            "command=",
        ])
        .output()?;
    let line = String::from_utf8_lossy(&process.stdout);
    assert!(
        process.status.success()
            && !line.trim_start().starts_with('Z')
            && line
                .trim_end()
                .ends_with(&format!("--worker {dead} run {token}"))
    );
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    until(&c, &dead, "unknown").await?;
    c.observe(&dead, true).await?;
    until(&c, &dead, "completed").await?;
    assert_eq!(count(state.path(), &dead, "run"), 1);
    c.close().await?;
    unsafe { std::env::remove_var("GEMINI_DESKTOP_MCP_STATE_DIR") };
    Ok(())
}
