#![cfg(feature = "test-fixture")]
use gemini_desktop_mcp::client::{Capability, GeminiClient, StartOptions};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use uuid::Uuid;
fn options(id: &str, prompt: &str, capability: Capability) -> StartOptions {
    StartOptions {
        prompt: prompt.into(),
        request_id: id.into(),
        capability,
        timeout_ms: 60000,
    }
}
async fn until(c: &GeminiClient, id: &str, status: &str) -> anyhow::Result<Value> {
    for _ in 0..250 {
        let v = c.observe(id, false).await?;
        if v["status"] == status {
            return Ok(v);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    anyhow::bail!("wanted {status}, got {}", c.observe(id, false).await?)
}
async fn log_for(dir: &Path, id: &str, command: &str) -> anyhow::Result<Value> {
    for _ in 0..250 {
        if let Ok(log) = std::fs::read_to_string(dir.join("fixture-calls.jsonl")) {
            if let Some(v) = log
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| v["request_id"] == id && v["command"] == command)
            {
                return Ok(v);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    anyhow::bail!("missing fixture call")
}
fn count(dir: &Path, id: &str) -> usize {
    std::fs::read_to_string(dir.join("fixture-calls.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["request_id"] == id
                && ["run", "continue"].contains(&v["command"].as_str().unwrap_or(""))
        })
        .count()
}
async fn completed_chat(c: &GeminiClient) -> anyhow::Result<Value> {
    let id = Uuid::new_v4().to_string();
    c.start("prior owned chat", &id).await?;
    until(c, &id, "waiting_human").await?;
    c.observe(&id, true).await?;
    until(c, &id, "completed").await
}
#[tokio::test]
async fn fresh_chat_uses_explicit_owned_predecessor_and_distinct_session() -> anyhow::Result<()> {
    let state = tempfile::tempdir()?;
    unsafe { std::env::set_var("GEMINI_DESKTOP_MCP_STATE_DIR", state.path()) };
    let c = GeminiClient::connect(env!("CARGO_BIN_EXE_gemini-test-fixture")).await?;
    let old = completed_chat(&c).await?;
    let old_id = old["operation_id"].as_str().unwrap();
    let fresh_id = Uuid::new_v4().to_string();
    let request = options(&fresh_id, "fresh prompt", Capability::Text);
    let fresh = c.start_after(old_id, &request).await?;
    assert_eq!(fresh["request_mode"], "fresh_after");
    assert_ne!(fresh["session_id"], old["session_id"]);
    assert_eq!(fresh["native_dir_id"], old["native_dir_id"]);
    assert_eq!(fresh["prior_request_id"], old_id);
    let call = log_for(state.path(), &fresh_id, "run").await?;
    assert_eq!(call["prior_request_id"], old_id);
    assert_eq!(call["session_id"], fresh["session_id"]);
    assert_eq!(
        call["state_dir"],
        state.path().join(old_id).join("native").to_str().unwrap()
    );
    assert_eq!(
        c.start_after(old_id, &request).await?["session_id"],
        fresh["session_id"]
    );
    assert_eq!(count(state.path(), &fresh_id), 1);
    assert_eq!(
        c.continue_turn(old_id, "fresh prompt", &fresh_id).await?["status"],
        "conflict"
    );
    assert_eq!(
        c.start("fresh prompt", &fresh_id).await?["status"],
        "conflict"
    );
    let foreign = Uuid::new_v4().to_string();
    assert_eq!(
        c.start_after(&foreign, &request).await?["status"],
        "conflict"
    );
    let continued_id = Uuid::new_v4().to_string();
    let continued = c
        .continue_turn(old_id, "same session", &continued_id)
        .await?;
    assert_eq!(continued["request_mode"], "continue");
    assert_eq!(continued["session_id"], old["session_id"]);
    assert_eq!(continued["native_dir_id"], old["native_dir_id"]);
    let call = log_for(state.path(), &continued_id, "continue").await?;
    assert_eq!(call["prior_request_id"], old_id);
    assert_eq!(call["session_id"], old["session_id"]);
    until(&c, &continued_id, "completed").await?;
    // Records written before request_mode existed remain conservatively identifiable.
    let path = state.path().join(&continued_id).join("operation.json");
    let mut legacy: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    legacy.as_object_mut().unwrap().remove("request_mode");
    std::fs::write(&path, serde_json::to_vec(&legacy)?)?;
    assert_eq!(
        c.continue_turn(old_id, "same session", &continued_id)
            .await?["status"],
        "completed"
    );
    assert_eq!(
        c.start_after(
            old_id,
            &options(&continued_id, "same session", Capability::Text)
        )
        .await?["status"],
        "conflict"
    );
    let path = state.path().join(old_id).join("operation.json");
    let mut legacy: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    legacy.as_object_mut().unwrap().remove("request_mode");
    std::fs::write(path, serde_json::to_vec(&legacy)?)?;
    assert_eq!(
        c.start("prior owned chat", old_id).await?["status"],
        "completed"
    );

    let failed_id = Uuid::new_v4().to_string();
    c.start_with_options(&options(&failed_id, "mode-failed-start", Capability::Image))
        .await?;
    let failed = until(&c, &failed_id, "failed").await?;
    let next_id = Uuid::new_v4().to_string();
    let after_failed = c
        .start_after(
            &failed_id,
            &options(&next_id, "new after failed", Capability::Text),
        )
        .await?;
    assert_eq!(after_failed["request_mode"], "fresh_after");
    assert_ne!(after_failed["session_id"], failed["session_id"]);
    assert_eq!(after_failed["native_dir_id"], failed["native_dir_id"]);
    assert_eq!(
        log_for(state.path(), &next_id, "run").await?["prior_request_id"],
        failed_id
    );
    let media_id = Uuid::new_v4().to_string();
    let media_request = options(&media_id, "fresh image", Capability::Image);
    let media = c.start_after(old_id, &media_request).await?;
    assert_eq!(
        log_for(state.path(), &media_id, "run").await?["prior_request_id"],
        old_id
    );
    std::fs::write(
        state.path().join("fixture-capability-disabled"),
        b"disabled",
    )?;
    assert_eq!(
        c.start_after(old_id, &media_request).await?["session_id"],
        media["session_id"]
    );
    assert_eq!(count(state.path(), &media_id), 1);
    let absent = Uuid::new_v4().to_string();
    let blocked_id = Uuid::new_v4().to_string();
    let blocked = c
        .start_after(
            &absent,
            &options(&blocked_id, "missing media predecessor", Capability::Image),
        )
        .await?;
    assert_eq!(blocked["reason"], "prior_operation_not_found");
    assert_eq!(count(state.path(), &blocked_id), 0);
    std::fs::remove_file(state.path().join("fixture-capability-disabled"))?;
    let running_id = Uuid::new_v4().to_string();
    c.start("still running", &running_id).await?;
    let rejected_id = Uuid::new_v4().to_string();
    let rejected = c
        .start_after(
            &running_id,
            &options(&rejected_id, "blocked running", Capability::Text),
        )
        .await?;
    assert_eq!(rejected["status"], "blocked");
    assert_eq!(rejected["reason"], "prior_turn_not_completed");
    assert_eq!(count(state.path(), &rejected_id), 0);
    let unknown_id = Uuid::new_v4().to_string();
    c.start_with_options(&options(&unknown_id, "mode-text-start", Capability::Image))
        .await?;
    until(&c, &unknown_id, "unknown").await?;
    let rejected_id = Uuid::new_v4().to_string();
    assert_eq!(
        c.start_after(
            &unknown_id,
            &options(&rejected_id, "blocked unknown", Capability::Text)
        )
        .await?["status"],
        "blocked"
    );
    assert_eq!(count(state.path(), &rejected_id), 0);
    let missing_id = Uuid::new_v4().to_string();
    let rejected_id = Uuid::new_v4().to_string();
    let missing = c
        .start_after(
            &missing_id,
            &options(&rejected_id, "blocked missing", Capability::Text),
        )
        .await?;
    assert_eq!(missing["reason"], "prior_operation_not_found");
    assert_eq!(count(state.path(), &rejected_id), 0);
    let forged_id = Uuid::new_v4().to_string();
    let forged_dir = state.path().join(&forged_id);
    std::fs::create_dir(&forged_dir)?;
    std::fs::set_permissions(&forged_dir, std::fs::Permissions::from_mode(0o700))?;
    let mut forged: Value = serde_json::from_slice(&std::fs::read(
        state.path().join(old_id).join("operation.json"),
    )?)?;
    std::fs::write(
        forged_dir.join("operation.json"),
        serde_json::to_vec(&forged)?,
    )?;
    let rejected_id = Uuid::new_v4().to_string();
    let blocked = c
        .start_after(
            &forged_id,
            &options(&rejected_id, "blocked foreign", Capability::Text),
        )
        .await?;
    assert_eq!(blocked["reason"], "prior_identity_mismatch");
    assert_eq!(count(state.path(), &rejected_id), 0);
    forged["operation_id"] = json!(forged_id);
    forged["request_id"] = json!(forged_id);
    std::fs::write(
        forged_dir.join("operation.json"),
        serde_json::to_vec(&forged)?,
    )?;
    let rejected_id = Uuid::new_v4().to_string();
    let blocked = c
        .start_after(
            &forged_id,
            &options(&rejected_id, "blocked wrong binding", Capability::Text),
        )
        .await?;
    assert_eq!(blocked["reason"], "prior_native_binding_unverified");
    assert_eq!(count(state.path(), &rejected_id), 0);
    // All fixture workers finish before their private state directory is dropped.
    until(&c, &fresh_id, "waiting_human").await?;
    until(&c, &media_id, "waiting_human").await?;
    until(&c, &next_id, "waiting_human").await?;
    until(&c, &running_id, "waiting_human").await?;
    c.close().await?;
    unsafe { std::env::remove_var("GEMINI_DESKTOP_MCP_STATE_DIR") };
    Ok(())
}
