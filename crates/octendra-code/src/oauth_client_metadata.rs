//! Offline public projection: never initialize Store, deserialize Client or log SQL errors.
use libsqlite3_sys as ffi;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{ConnectOptions, Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    fs,
    path::Path,
    time::{Duration, Instant},
};

const SQL: &str = "SELECT kind,key,CASE WHEN length(value)<=65536 AND json_valid(value) THEN json_object('client_id',json_extract(value,'$.client_id'),'client_name',json_extract(value,'$.client_name'),'redirect_uris',json_extract(value,'$.redirect_uris'),'token_endpoint_auth_method',json_extract(value,'$.token_endpoint_auth_method')) END FROM kv WHERE kind IN ('client','oauth2_client') AND expires>? ORDER BY kind,key LIMIT 1001";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PublicClient {
    client_id: String,
    client_name: Option<String>,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: String,
}

#[derive(Debug)]
struct Failure {
    error: &'static str,
    stage: &'static str,
}
impl Failure {
    fn at(error: &'static str, stage: &'static str) -> Self {
        Self { error, stage }
    }
}
type Result<T> = std::result::Result<T, Failure>;

fn regular(path: &Path) -> Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        Failure::at(
            match error.kind() {
                std::io::ErrorKind::NotFound => "state_path_missing",
                std::io::ErrorKind::PermissionDenied => "filesystem_access_denied",
                _ => "state_path_unavailable",
            },
            "path_stat",
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(Failure::at("state_file_not_regular", "path_stat"));
    }
    Ok(metadata)
}

// No user data or allocation in the callback; deny every non-projection action.
unsafe extern "C" fn authorize(
    _: *mut c_void,
    action: c_int,
    first: *const c_char,
    second: *const c_char,
    database: *const c_char,
    source: *const c_char,
) -> c_int {
    fn is(pointer: *const c_char, choices: &[&[u8]]) -> bool {
        // SAFETY: SQLite supplies valid NUL-terminated arguments for this callback.
        !pointer.is_null() && choices.contains(&unsafe { CStr::from_ptr(pointer) }.to_bytes())
    }
    let allowed = source.is_null()
        && match action {
            ffi::SQLITE_SELECT => true,
            ffi::SQLITE_READ => {
                is(database, &[b"main"])
                    && is(first, &[b"kv"])
                    && is(second, &[b"kind", b"key", b"value", b"expires"])
            }
            ffi::SQLITE_FUNCTION => is(
                second,
                &[b"length", b"json_valid", b"json_extract", b"json_object"],
            ),
            _ => false,
        };
    if allowed {
        ffi::SQLITE_OK
    } else {
        ffi::SQLITE_DENY
    }
}

async fn guard(connection: &mut SqliteConnection) -> Result<()> {
    let mut handle = connection
        .lock_handle()
        .await
        .map_err(|_| Failure::at("sqlite_guard_failed", "readonly_setup"))?;
    let deadline = Instant::now() + Duration::from_secs(3);
    handle.set_progress_handler(1000, move || Instant::now() < deadline);
    // SAFETY: locked SQLx handle is exclusively held, callback is static and has no userdata.
    // libsqlite3-sys is the exact existing SQLx dependency, not a second SQLite library.
    let code = unsafe {
        ffi::sqlite3_set_authorizer(
            handle.as_raw_handle().as_ptr(),
            Some(authorize),
            std::ptr::null_mut(),
        )
    };
    if code != ffi::SQLITE_OK {
        return Err(Failure::at("sqlite_guard_failed", "readonly_setup"));
    }
    Ok(())
}

async fn inspect(path: &Path) -> Result<Value> {
    if !path.is_absolute() {
        return Err(Failure::at("absolute_state_path_required", "path_stat"));
    }
    let before = regular(path)?;
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match fs::symlink_metadata(&name) {
            Ok(_) => {
                regular(Path::new(&name))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(Failure::at("state_sidecar_unavailable", "path_stat")),
        }
    }
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .create_if_missing(false)
        .busy_timeout(Duration::from_secs(2))
        .pragma("query_only", "ON")
        .pragma("trusted_schema", "OFF")
        .pragma("temp_store", "MEMORY")
        .disable_statement_logging();
    let mut connection = options
        .connect()
        .await
        .map_err(|_| Failure::at("sqlite_open_failed", "sqlite_connect"))?;
    let result = inspect_connection(&mut connection).await;
    connection
        .close()
        .await
        .map_err(|_| Failure::at("sqlite_close_failed", "sqlite_close"))?;
    let output = result?;
    let after = regular(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
            return Err(Failure::at("state_file_changed", "path_recheck"));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (before, after);
    }
    Ok(output)
}

async fn inspect_connection(connection: &mut SqliteConnection) -> Result<Value> {
    // Only ordinary kv table with the known public schema. Never open arbitrary views.
    let table: Option<(String,)> =
        sqlx::query_as("SELECT sql FROM sqlite_schema WHERE name='kv' AND type='table'")
            .fetch_optional(&mut *connection)
            .await
            .map_err(|_| Failure::at("state_schema_unavailable", "known_schema"))?;
    if !table.is_some_and(|(sql,)| {
        sql.trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE TABLE")
    }) {
        return Err(Failure::at("state_schema_unexpected", "known_schema"));
    }
    let columns: Vec<(String, String)> =
        sqlx::query_as("SELECT name,type FROM pragma_table_info('kv') ORDER BY cid")
            .fetch_all(&mut *connection)
            .await
            .map_err(|_| Failure::at("state_schema_unavailable", "known_schema"))?;
    if columns
        != [
            ("kind".into(), "TEXT".into()),
            ("key".into(), "TEXT".into()),
            ("value".into(), "TEXT".into()),
            ("expires".into(), "INTEGER".into()),
        ]
    {
        return Err(Failure::at("state_schema_unexpected", "known_schema"));
    }
    let version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&mut *connection)
        .await
        .map_err(|_| Failure::at("sqlite_version_unavailable", "readonly_setup"))?;
    guard(connection).await?;
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(SQL)
        .bind(octendra_code::now())
        .fetch_all(&mut *connection)
        .await
        .map_err(|_| Failure::at("sqlite_projection_failed", "public_projection"))?;
    if rows.len() > 1000 {
        return Err(Failure::at("client_count_exceeds_limit", "public_metadata"));
    }
    let mut clients = Vec::new();
    let mut redacted = 0;
    for (kind, key, text) in rows {
        let mut client: PublicClient = text
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or_else(|| Failure::at("public_metadata_invalid", "public_metadata"))?;
        let id = client
            .client_id
            .strip_prefix("rh2_")
            .unwrap_or(&client.client_id);
        if id.len() != 64
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || key != client.client_id
            || kind
                != if client.client_id.starts_with("rh2_") {
                    "oauth2_client"
                } else {
                    "client"
                }
        {
            return Err(Failure::at(
                "public_client_identity_invalid",
                "public_metadata",
            ));
        }
        if !["none", "client_secret_basic", "client_secret_post"]
            .contains(&client.token_endpoint_auth_method.as_str())
        {
            return Err(Failure::at("public_auth_method_invalid", "public_metadata"));
        }
        if client.client_name.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 200 || name.chars().any(char::is_control)
        }) {
            return Err(Failure::at("public_client_name_invalid", "public_metadata"));
        }
        if !(1..=6).contains(&client.redirect_uris.len()) {
            return Err(Failure::at("public_redirects_invalid", "public_metadata"));
        }
        for uri in &mut client.redirect_uris {
            if uri.len() > 4096 || uri.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(Failure::at("public_redirects_invalid", "public_metadata"));
            }
            let mut parsed = url::Url::parse(uri)
                .map_err(|_| Failure::at("public_redirects_invalid", "public_metadata"))?;
            if !["http", "https"].contains(&parsed.scheme())
                || parsed.host_str().is_none()
                || !parsed.username().is_empty()
                || parsed.password().is_some()
            {
                return Err(Failure::at("public_redirects_invalid", "public_metadata"));
            }
            if parsed.query().is_some() || parsed.fragment().is_some() {
                parsed.set_query(None);
                parsed.set_fragment(None);
                *uri = parsed.into();
                redacted += 1;
            }
        }
        clients.push(client);
    }
    Ok(json!({"clients":clients,"redacted_redirect_uris":redacted,"sqlite_version":version}))
}

pub async fn run(path: &Path) -> (Value, i32) {
    match inspect(path).await {
        Ok(output) => (output, 0),
        Err(failure) => (json!({"error":failure.error,"stage":failure.stage}), 2),
    }
}

#[cfg(test)]
#[path = "oauth_client_metadata_tests.rs"]
mod tests;
