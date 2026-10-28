//! Exercise the real early CLI dispatch, including parser errors and stderr.
use std::process::Command;

#[test]
fn offline_metadata_cli_does_not_echo_arguments_or_start_services() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("SYNTHETIC_SECRET_MUST_NEVER_APPEAR");
    for args in [
        vec!["--state-db", missing.to_str().unwrap()],
        vec!["--unknown", missing.to_str().unwrap()],
        vec![],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_octendra-code"))
            .arg("inspect-oauth-clients")
            .args(args)
            .env("RUST_LOG", "trace")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stderr.is_empty());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(json.get("error").is_some());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SYNTHETIC_SECRET"));
        assert!(!missing.exists());
        assert!(!dir.path().join("service.lock").exists());
    }
}

#[tokio::test]
async fn successful_native_cli_keeps_secret_fields_and_sql_logs_out_of_output() {
    use sqlx::{Connection, Executor, sqlite::SqliteConnectOptions};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let mut db = sqlx::SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    db.execute("CREATE TABLE kv(kind TEXT NOT NULL,key TEXT NOT NULL,value TEXT NOT NULL CHECK(json_valid(value)),expires INTEGER NOT NULL,PRIMARY KEY(kind,key)); CREATE INDEX kv_json ON kv(json_extract(value,'$.state'))").await.unwrap();
    let id = "a".repeat(64);
    let value = serde_json::json!({"client_id":id,"client_name":"fixture","redirect_uris":["https://chatgpt.com/connector_platform_oauth_redirect"],"token_endpoint_auth_method":"none","client_secret_hash":"SYNTHETIC_SECRET_MUST_NEVER_APPEAR","token":"SYNTHETIC_SECRET_MUST_NEVER_APPEAR"});
    sqlx::query("INSERT INTO kv VALUES('client',?,?,?)")
        .bind(&id)
        .bind(value.to_string())
        .bind(i64::MAX)
        .execute(&mut db)
        .await
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_octendra-code"))
        .args(["inspect-oauth-clients", "--state-db"])
        .arg(&path)
        .env("RUST_LOG", "trace")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("SYNTHETIC_SECRET"));
    assert!(!text.contains("client_secret_hash"));
    let projected: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(projected["clients"][0]["client_id"], id);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
