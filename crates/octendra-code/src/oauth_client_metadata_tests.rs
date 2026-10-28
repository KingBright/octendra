use super::*;
use sqlx::{Executor, sqlite::SqliteJournalMode};

const SECRET: &str = "SYNTHETIC_SECRET_MUST_NEVER_APPEAR";
const CALLBACK: &str = "https://chatgpt.com/connector_platform_oauth_redirect";
const SCHEMA: &str = "CREATE TABLE kv (kind TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL CHECK(json_valid(value)), expires INTEGER NOT NULL, PRIMARY KEY(kind,key)); CREATE INDEX kv_kind_state_created ON kv(kind,json_extract(value,'$.state'),json_extract(value,'$.created_at')); CREATE INDEX kv_expiring ON kv(expires,kind,key) WHERE expires<9223372036854775807";

#[tokio::test]
async fn actual_sqlite_340_fixture_opens_with_trusted_schema_off() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let original = include_bytes!("../tests/fixtures/oauth-clients-sqlite-3.40.sqlite");
    fs::write(&path, original).unwrap();
    let (output, code) = run(&path).await;
    assert_eq!(code, 0, "{output}");
    assert_eq!(output["clients"][0]["client_id"], "a".repeat(64));
    assert_eq!(fs::read(&path).unwrap(), original);
    no_secret(&output);
}

async fn fixture(wal: bool) -> (tempfile::TempDir, std::path::PathBuf, SqliteConnection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let mut connection = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(if wal {
            SqliteJournalMode::Wal
        } else {
            SqliteJournalMode::Delete
        })
        .connect()
        .await
        .unwrap();
    connection.execute(SCHEMA).await.unwrap();
    (dir, path, connection)
}
fn client(id: &str) -> Value {
    json!({"client_id":id,"client_name":"ChatGPT fixture","redirect_uris":[CALLBACK],
        "token_endpoint_auth_method":"client_secret_basic","client_secret_hash":SECRET,
        "password":SECRET,"access_token":SECRET,"refresh_token":SECRET})
}
async fn put(connection: &mut SqliteConnection, kind: &str, id: &str, value: Value, expires: i64) {
    sqlx::query("INSERT INTO kv VALUES(?,?,?,?)")
        .bind(kind)
        .bind(id)
        .bind(value.to_string())
        .bind(expires)
        .execute(connection)
        .await
        .unwrap();
}
fn no_secret(value: &Value) {
    let text = value.to_string();
    for forbidden in [
        SECRET,
        "client_secret_hash",
        "access_token",
        "refresh_token",
        "password",
    ] {
        assert!(
            !text.contains(forbidden),
            "sensitive field in diagnostic output"
        );
    }
}

#[tokio::test]
async fn historical_check_and_expression_index_public_projection() {
    let (_dir, path, mut writer) = fixture(false).await;
    let id = "a".repeat(64);
    let v2 = format!("rh2_{}", "b".repeat(64));
    put(&mut writer, "client", &id, client(&id), i64::MAX).await;
    put(&mut writer, "oauth2_client", &v2, client(&v2), i64::MAX).await;
    for kind in [
        "session",
        "oauth2_session",
        "token",
        "owner",
        "pending",
        "oauth2_pending",
    ] {
        put(
            &mut writer,
            kind,
            SECRET,
            json!({"password":SECRET}),
            i64::MAX,
        )
        .await;
    }
    put(
        &mut writer,
        "client",
        &"c".repeat(64),
        client(&"c".repeat(64)),
        1,
    )
    .await;
    let before = fs::read(&path).unwrap();
    let (output, code) = run(&path).await;
    assert_eq!(code, 0, "{output}");
    assert_eq!(output["clients"].as_array().unwrap().len(), 2);
    assert_eq!(output["clients"][0]["redirect_uris"][0], CALLBACK);
    assert_eq!(output["clients"][1]["client_id"], v2);
    assert_eq!(before, fs::read(&path).unwrap());
    assert_eq!(output["sqlite_version"], "3.51.3");
    no_secret(&output);
}

#[tokio::test]
async fn live_wal_visible_without_database_or_wal_writes() {
    let (_dir, path, mut writer) = fixture(true).await;
    let id = "a".repeat(64);
    put(&mut writer, "client", &id, client(&id), i64::MAX).await;
    let wal = path.with_file_name("state.sqlite-wal");
    let before = fs::read(&path).unwrap();
    let before_wal = fs::read(&wal).unwrap();
    let before_mode = fs::metadata(&path).unwrap().permissions();
    let (output, code) = run(&path).await;
    assert_eq!(code, 0, "{output}");
    assert_eq!(output["clients"][0]["client_id"], id);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&wal).unwrap(), before_wal);
    assert_eq!(fs::metadata(&path).unwrap().permissions(), before_mode);
    no_secret(&output);
}

#[tokio::test]
async fn query_and_fragment_redacted() {
    let (_dir, path, mut writer) = fixture(false).await;
    let id = "a".repeat(64);
    let mut value = client(&id);
    value["redirect_uris"] = json!([format!("{CALLBACK}?code={SECRET}#{SECRET}")]);
    put(&mut writer, "client", &id, value, i64::MAX).await;
    let (output, code) = run(&path).await;
    assert_eq!(code, 0);
    assert_eq!(output["clients"][0]["redirect_uris"][0], CALLBACK);
    assert_eq!(output["redacted_redirect_uris"], 1);
    no_secret(&output);
}

#[tokio::test]
async fn invalid_public_fields_never_return_exception_or_partial_rows() {
    for change in [
        json!({"client_id":SECRET}),
        json!({"client_name":42}),
        json!({"redirect_uris":[format!("https://user:{SECRET}@example.com/callback")]}),
        json!({"redirect_uris":"wrong"}),
        json!({"token_endpoint_auth_method":SECRET}),
    ] {
        let (_dir, path, mut writer) = fixture(false).await;
        let id = "a".repeat(64);
        put(&mut writer, "client", &id, client(&id), i64::MAX).await;
        let bad_id = "b".repeat(64);
        let mut value = client(&bad_id);
        for (field, invalid) in change.as_object().unwrap() {
            value[field] = invalid.clone();
        }
        put(&mut writer, "client", &bad_id, value, i64::MAX).await;
        let (output, code) = run(&path).await;
        assert_eq!(code, 2, "{output}");
        assert!(output.get("clients").is_none());
        no_secret(&output);
    }
}

#[tokio::test]
async fn invalid_database_and_missing_path_are_sanitized_and_never_created() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join(SECRET);
    let (output, code) = run(&missing).await;
    assert_eq!(code, 2);
    assert_eq!(output["error"], "state_path_missing");
    assert!(!missing.exists());
    no_secret(&output);
    fs::write(&missing, SECRET).unwrap();
    let (output, code) = run(&missing).await;
    assert_eq!(code, 2);
    no_secret(&output);
    assert_eq!(fs::read_to_string(&missing).unwrap(), SECRET);
    let (output, code) = run(Path::new(SECRET)).await;
    assert_eq!(code, 2);
    no_secret(&output);
}

#[tokio::test]
async fn view_and_wrong_schema_are_rejected_without_migration() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    let mut writer = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    for schema in [
        "CREATE VIEW kv AS SELECT 1 AS kind",
        "CREATE TABLE kv(secret TEXT)",
    ] {
        writer.execute(schema).await.unwrap();
        let before = fs::read(&path).unwrap();
        let (output, code) = run(&path).await;
        assert_eq!(code, 2);
        assert_eq!(output["error"], "state_schema_unexpected");
        assert_eq!(before, fs::read(&path).unwrap());
        no_secret(&output);
        writer
            .execute(if schema.contains("VIEW") {
                "DROP VIEW kv"
            } else {
                "DROP TABLE kv"
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn authorizer_denies_write_attach_other_tables_and_extension_loading() {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    connection.execute(SCHEMA).await.unwrap();
    connection
        .execute("CREATE TABLE sessions(secret TEXT)")
        .await
        .unwrap();
    guard(&mut connection).await.unwrap();
    for sql in [
        "INSERT INTO kv VALUES('x','y','{}',1)",
        "DELETE FROM kv",
        "CREATE TABLE x(a)",
        "PRAGMA trusted_schema=ON",
        "ATTACH ':memory:' AS other",
        "SELECT * FROM sessions",
        "SELECT load_extension('fixture')",
    ] {
        assert!(sqlx::query(sql).execute(&mut connection).await.is_err());
    }
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(SQL)
        .bind(1i64)
        .fetch_all(&mut connection)
        .await
        .unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn capacity_bound_returns_no_partial_clients() {
    let (_dir, path, mut writer) = fixture(false).await;
    for n in 0..1001 {
        let id = format!("{n:064x}");
        put(&mut writer, "client", &id, client(&id), i64::MAX).await;
    }
    let (output, code) = run(&path).await;
    assert_eq!(code, 2);
    assert_eq!(output["error"], "client_count_exceeds_limit");
    assert!(output.get("clients").is_none());
    no_secret(&output);
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_database_or_sidecar_rejected_without_following() {
    use std::os::unix::fs::symlink;
    let (dir, path, _writer) = fixture(false).await;
    let link = dir.path().join("link.sqlite");
    symlink(&path, &link).unwrap();
    assert_eq!(run(&link).await.0["error"], "state_file_not_regular");
    let target = dir.path().join("secret");
    fs::write(&target, SECRET).unwrap();
    let sidecar = path.with_file_name("state.sqlite-wal");
    symlink(&target, &sidecar).unwrap();
    let (output, code) = run(&path).await;
    assert_eq!(code, 2);
    no_secret(&output);
    assert_eq!(fs::read_to_string(target).unwrap(), SECRET);
}
