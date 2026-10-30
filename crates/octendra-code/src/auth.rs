//! Single-owner OAuth authorization-code/PKCE service with hashed opaque tokens.
mod clients;
use crate::{GatewayConfig, SCOPES, hash, now, random, secret_eq, store::Store};
use axum::{
    Form, Json, Router,
    extract::{Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clients::{Client, redirect_uri_allowed};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

type HttpResult<T> = Result<T, (StatusCode, Json<Value>)>;
/// Keep confidential-client state invisible to binaries that cannot authenticate it.
/// This is a namespace in the existing KV store, not a separate database.
pub fn oauth_state_kind(kind: &str, opaque: &str) -> String {
    if opaque.starts_with("rh2_") {
        format!("oauth2_{kind}")
    } else {
        kind.into()
    }
}
fn oauth_random(confidential: bool) -> String {
    if confidential {
        format!("rh2_{}", random())
    } else {
        random()
    }
}
fn invalid() -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"invalid_request"})),
    )
}
fn internal(_: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"server_error"})),
    )
}
pub struct Auth {
    pub config: Arc<GatewayConfig>,
    pub store: Store,
    rate: Arc<Mutex<HashMap<String, (i64, u32)>>>,
    pub(crate) live_config: Option<Arc<crate::live_config::LiveConfig>>,
}
impl Clone for Auth {
    fn clone(&self) -> Self {
        let config = self
            .live_config
            .as_ref()
            .map(|live| live.snapshot())
            .unwrap_or_else(|| self.config.clone());
        self.with_config(config)
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Principal {
    pub owner: String,
    pub scopes: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Grant {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    resource: String,
    scope: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Access {
    principal: Principal,
    client_id: String,
    resource: String,
    family: String,
}
// Preserve the deployed policy. Longer sessions need a separately reviewed policy change.
const ACCESS_TTL: i64 = 3600;
const REFRESH_TTL: i64 = 30 * 86400;
const REFRESH_RETRY_WINDOW: i64 = 30;

#[derive(Serialize, Deserialize)]
struct CurrentTokens {
    access: Access,
    bearer: String,
    refresh: String,
    access_expires_at: i64,
}
impl CurrentTokens {
    fn response(&self, observed_at: i64) -> Json<Value> {
        Json(json!({
            "access_token": self.bearer,
            "refresh_token": self.refresh,
            "token_type": "Bearer",
            "expires_in": (self.access_expires_at - observed_at).max(0),
            "scope": self.access.principal.scopes.join(" "),
            "resource": self.access.resource
        }))
    }
}
async fn put_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    key: &str,
    value: &impl Serialize,
    expires: i64,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO kv VALUES(?,?,?,?) ON CONFLICT(kind,key) DO UPDATE SET value=excluded.value, expires=excluded.expires")
        .bind(kind).bind(key).bind(serde_json::to_string(value)?).bind(expires)
        .execute(&mut **tx).await?;
    Ok(())
}
async fn get_tx<T: serde::de::DeserializeOwned>(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    kind: &str,
    key: &str,
    observed_at: i64,
) -> anyhow::Result<Option<T>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT value FROM kv WHERE kind=? AND key=? AND expires>?")
            .bind(kind)
            .bind(key)
            .bind(observed_at)
            .fetch_optional(&mut **tx)
            .await?;
    row.map(|(value,)| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}
async fn revoke_family_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    family: &str,
) -> anyhow::Result<()> {
    // A durable tombstone also covers already-issued long-lived legacy successors.
    // The same writer transaction serializes revocation against every refresh issuance.
    put_tx(tx, "revoked", family, &true, i64::MAX).await?;
    sqlx::query("DELETE FROM kv WHERE kind='oauth_refresh_family' AND key=?")
        .bind(family)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn issue_tokens_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    access: Access,
    confidential: bool,
    issued_at: i64,
) -> anyhow::Result<CurrentTokens> {
    let tokens = CurrentTokens {
        bearer: oauth_random(access.client_id.starts_with("rh2_")),
        refresh: oauth_random(access.client_id.starts_with("rh2_")),
        access,
        access_expires_at: issued_at + ACCESS_TTL,
    };
    put_tx(
        tx,
        &oauth_state_kind("access", &tokens.bearer),
        &hash(&tokens.bearer),
        &tokens.access,
        tokens.access_expires_at,
    )
    .await?;
    put_tx(
        tx,
        &oauth_state_kind("refresh", &tokens.refresh),
        &hash(&tokens.refresh),
        &tokens.access,
        issued_at + REFRESH_TTL,
    )
    .await?;
    if confidential {
        // Only a short authenticated retry window retains recoverable token values.
        put_tx(
            tx,
            "oauth_refresh_family",
            &tokens.access.family,
            &tokens,
            issued_at + REFRESH_RETRY_WINDOW,
        )
        .await?;
    }
    Ok(tokens)
}
impl Auth {
    pub fn new(config: Arc<GatewayConfig>, store: Store) -> Self {
        Self {
            config,
            store,
            rate: Arc::new(Mutex::new(HashMap::new())),
            live_config: None,
        }
    }
    pub(crate) fn with_config(&self, config: Arc<GatewayConfig>) -> Self {
        Self {
            config,
            store: self.store.clone(),
            rate: self.rate.clone(),
            live_config: self.live_config.clone(),
        }
    }
    fn limit(&self, key: &str, limit: u32) -> bool {
        let Ok(mut rate) = self.rate.lock() else {
            return false;
        };
        let entry = rate.entry(key.into()).or_insert((now(), 0));
        if now() - entry.0 >= 60 {
            *entry = (now(), 0)
        }
        entry.1 += 1;
        entry.1 <= limit
    }
    pub(crate) fn allow_owner_password_attempt(&self) -> bool {
        self.limit("password", 10)
    }
    fn owner_password_retry_after(&self) -> i64 {
        self.rate
            .lock()
            .ok()
            .and_then(|rate| {
                rate.get("password")
                    .map(|entry| (entry.0 + 60 - now()).clamp(1, 60))
            })
            .unwrap_or(60)
    }
    pub(crate) async fn owner_password_valid(&self, password: String) -> bool {
        let encoded = self.config.password_hash.clone();
        tokio::task::spawn_blocking(move || {
            use argon2::{Argon2, PasswordHash, PasswordVerifier};
            PasswordHash::new(&encoded).ok().is_some_and(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
        })
        .await
        .unwrap_or(false)
    }
    pub fn routes(&self) -> Router {
        Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(resource_metadata),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get(resource_metadata),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(auth_metadata),
            )
            .route("/oauth/register", post(register))
            .route("/oauth/authorize", get(authorize))
            .route("/oauth/approve", post(approve))
            .route("/oauth/token", post(token))
            .route("/oauth/revoke", post(revoke))
            .with_state(self.clone())
    }
    fn at_public_endpoint(&self, headers: &HeaderMap) -> Self {
        let public_url = self
            .config
            .public_url_for_request(headers)
            .unwrap_or(&self.config.public_url);
        if public_url == self.config.public_url {
            return self.clone();
        }
        let mut config = (*self.config).clone();
        config.public_url = public_url.into();
        Self {
            config: Arc::new(config),
            live_config: None,
            ..self.clone()
        }
    }
    pub async fn principal(&self, headers: &HeaderMap) -> Option<Principal> {
        let token = headers
            .get(header::AUTHORIZATION)?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")?;
        let access: Access = self
            .store
            .get(&oauth_state_kind("access", token), &hash(token))
            .await
            .ok()??;
        if access.resource
            != format!(
                "{}/mcp",
                self.config
                    .public_url_for_request(headers)
                    .unwrap_or(&self.config.public_url)
            )
            || self
                .store
                .get::<bool>("revoked", &access.family)
                .await
                .ok()?
                .is_some()
        {
            return None;
        }
        if access.principal.owner != self.config.owner {
            return None;
        }
        Some(access.principal)
    }
    pub fn challenge(&self, headers: &HeaderMap) -> Response {
        let mut r = (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"unauthorized"})),
        )
            .into_response();
        if let Ok(v) = format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\", scope=\"{}\"",
            self.config
                .public_url_for_request(headers)
                .unwrap_or(&self.config.public_url),
            SCOPES
        )
        .parse()
        {
            r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
        }
        r
    }
}
pub async fn require_auth(State(auth): State<Auth>, mut req: Request, next: Next) -> Response {
    let Some(principal) = auth.principal(req.headers()).await else {
        return auth.challenge(req.headers());
    };
    req.extensions_mut().insert(principal);
    next.run(req).await
}
async fn resource_metadata(State(a): State<Auth>, headers: HeaderMap) -> Json<Value> {
    let a = a.at_public_endpoint(&headers);
    Json(
        json!({"resource":format!("{}/mcp",a.config.public_url),"resource_name":crate::branding::NAME,"resource_documentation":format!("{}/status",a.config.public_url),"authorization_servers":[a.config.public_url],"scopes_supported":crate::SUPPORTED_SCOPES.split_whitespace().collect::<Vec<_>>(),"bearer_methods_supported":["header"]}),
    )
}
async fn auth_metadata(State(a): State<Auth>, headers: HeaderMap) -> Json<Value> {
    let a = a.at_public_endpoint(&headers);
    let u = &a.config.public_url;
    Json(
        json!({"issuer":u,"service_documentation":format!("{u}/status"),"authorization_endpoint":format!("{u}/oauth/authorize"),"token_endpoint":format!("{u}/oauth/token"),"registration_endpoint":format!("{u}/oauth/register"),"revocation_endpoint":format!("{u}/oauth/revoke"),"response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],"token_endpoint_auth_methods_supported":["none","client_secret_basic","client_secret_post"],"revocation_endpoint_auth_methods_supported":["none","client_secret_basic","client_secret_post"],"code_challenge_methods_supported":["S256"],"scopes_supported":crate::SUPPORTED_SCOPES.split_whitespace().collect::<Vec<_>>(),"authorization_response_iss_parameter_supported":true}),
    )
}
async fn register(
    State(a): State<Auth>,
    Json(v): Json<Value>,
) -> HttpResult<(StatusCode, Json<Value>)> {
    if !a.limit("register", 20) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"rate_limited"})),
        ));
    }
    let client = a.register_client(&v).await?;
    Ok((StatusCode::CREATED, Json(client)))
}
#[derive(Deserialize)]
struct Authorize {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    code_challenge: String,
    code_challenge_method: String,
    state: String,
    resource: Option<String>,
    scope: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Pending {
    grant: Grant,
    state: String,
}
async fn authorize(
    State(a): State<Auth>,
    headers: HeaderMap,
    Query(q): Query<Authorize>,
) -> HttpResult<Response> {
    let a = a.at_public_endpoint(&headers);
    if !a.limit("authorize", 60) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"rate_limited"})),
        ));
    }
    let c: Client = a
        .store
        .get(&oauth_state_kind("client", &q.client_id), &q.client_id)
        .await
        .map_err(internal)?
        .ok_or_else(invalid)?;
    let resource = a.client_resource(&c, q.resource.as_deref())?;
    let scope = a.client_scope(&c, q.scope.as_deref())?;
    if !c.redirect_uris.contains(&q.redirect_uri)
        || !redirect_uri_allowed(&a.config, &q.redirect_uri)
        || q.response_type != "code"
        || q.code_challenge_method != "S256"
        || q.code_challenge.len() != 43
        || !q
            .code_challenge
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        || q.state.len() > 4096
    {
        return Err(invalid());
    }
    let nonce = oauth_random(c.client_id.starts_with("rh2_"));
    let pending = Pending {
        grant: Grant {
            client_id: q.client_id,
            redirect_uri: q.redirect_uri,
            code_challenge: q.code_challenge,
            resource,
            scope: scope.clone(),
        },
        state: q.state,
    };
    a.store
        .put(
            &oauth_state_kind("pending", &nonce),
            &hash(&nonce),
            &pending,
            now() + 600,
        )
        .await
        .map_err(internal)?;
    let html = format!(
        "<!doctype html><html lang=zh-CN><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>Octendra 授权</title>{}<body><main><h1>连接你的 Octendra</h1><p>客户端名称（由申请方提供，未经验证）：{}</p><p>客户端 ID：{}</p><p>授权回调：{}</p><p>允许该客户端访问已登记的个人电脑。终端权限可以在本机用户权限范围内执行任意命令，包括项目目录以外的操作。</p><p>本次权限：{}</p><form method=post action=/oauth/approve><input type=hidden name=nonce value='{}'><label>网关登录密码 <input type=password name=password required autocomplete=current-password></label><p><button type=submit>登录并授权</button></p></form></main></body></html>",
        crate::branding::HEAD,
        html_escape(c.client_name.as_deref().unwrap_or("未提供")),
        html_escape(&c.client_id),
        html_escape(&pending.grant.redirect_uri),
        html_escape(&scope),
        nonce
    );
    let mut response = Html(html).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        a.config
            .authorization_csp_for_callback(Some(&pending.grant.redirect_uri))
            .parse()
            .map_err(internal)?,
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        format!(
            "{}={nonce}; Secure; HttpOnly; SameSite=Lax; Path=/oauth; Max-Age=600",
            approval_cookie(&nonce)
        )
        .parse()
        .map_err(internal)?,
    );
    Ok(response)
}
#[derive(Deserialize)]
struct Approval {
    nonce: String,
    password: String,
}
fn approval_cookie(nonce: &str) -> String {
    format!("octendra_oauth_{}", &hash(nonce)[..16])
}
async fn approve(state: State<Auth>, headers: HeaderMap, form: Form<Approval>) -> Response {
    let auth = state.0.clone();
    let browser = headers.get_all(header::ACCEPT).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|mime| mime.trim().split(';').next() == Some("text/html"))
        })
    });
    match approve_request(state, headers, form).await {
        Ok(response) => response,
        Err((status, body))
            if status == StatusCode::TOO_MANY_REQUESTS && body.0["error"] == "rate_limited" =>
        {
            let wait = auth.owner_password_retry_after();
            let mut response = if browser {
                (status, Html(format!(
                    "<!doctype html><html lang=zh-CN><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>请稍后重试 · Octendra</title>{}<body><main><h1>请求过于频繁，请稍后重试</h1><p>请等待至少 {wait} 秒，再返回发起连接的客户端重新开始授权。</p><p>请停止刷新或重复提交此页。此请求未完成授权，无需重置网关密码。</p></main></body></html>",
                    crate::branding::HEAD
                ))).into_response()
            } else {
                (status, body).into_response()
            };
            response.headers_mut().insert(header::RETRY_AFTER, wait.to_string().parse().expect("bounded retry delay"));
            response.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            response.headers_mut().insert(header::VARY, "Accept".parse().unwrap());
            response
        }
        Err((status, body))
            if browser
                && status == StatusCode::BAD_REQUEST
                && body.0["error"] == "invalid_request" =>
        {
            (
                status,
                [
                    (header::CACHE_CONTROL, "no-store"),
                    (header::VARY, "Accept"),
                ],
                Html(format!(
                    "<!doctype html><html lang=zh-CN><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>请重新发起授权 · Octendra</title>{}<body><main><h1>本次授权请求已失效或无效</h1><p>请返回发起连接的客户端，重新开始连接或授权，再在新打开的页面完成操作。</p><p>请勿刷新或重复提交此页。此请求未完成授权，无需因此重置网关密码。</p></main></body></html>",
                    crate::branding::HEAD
                )),
            )
                .into_response()
        }
        Err(error) => error.into_response(),
    }
}

async fn approve_request(
    State(a): State<Auth>,
    headers: HeaderMap,
    Form(f): Form<Approval>,
) -> HttpResult<Response> {
    let a = a.at_public_endpoint(&headers);
    if !a.allow_owner_password_attempt() {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"rate_limited"})),
        ));
    }
    if f.password.len() > 1024
        || !(f.nonce.len() == 64 || (f.nonce.starts_with("rh2_") && f.nonce.len() == 68))
    {
        return Err(invalid());
    }
    let cookie_name = approval_cookie(&f.nonce);
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok());
    let cookie = cookies.and_then(|value| {
        value
            .split(';')
            .filter_map(|part| part.trim().split_once('='))
            .find_map(|(name, value)| (name == cookie_name).then_some(value))
            .or_else(|| {
                value
                    .split(';')
                    .find_map(|part| part.trim().strip_prefix("octendra_oauth="))
            })
    });
    if cookie.is_none_or(|c| !secret_eq(c, &f.nonce)) {
        return Err(invalid());
    }
    let valid = a.owner_password_valid(f.password).await;
    if !valid {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"invalid_login"})),
        ));
    }
    let pending: Pending = a
        .store
        .take(&oauth_state_kind("pending", &f.nonce), &hash(&f.nonce))
        .await
        .map_err(internal)?
        .ok_or_else(invalid)?;
    if !redirect_uri_allowed(&a.config, &pending.grant.redirect_uri)
        || pending.grant.resource != format!("{}/mcp", a.config.public_url)
    {
        return Err(invalid());
    }
    let code = oauth_random(pending.grant.client_id.starts_with("rh2_"));
    a.store
        .put(
            &oauth_state_kind("code", &code),
            &hash(&code),
            &pending.grant,
            now() + 120,
        )
        .await
        .map_err(internal)?;
    let mut redirect = reqwest::Url::parse(&pending.grant.redirect_uri).map_err(internal)?;
    redirect
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &pending.state)
        .append_pair("iss", &a.config.public_url);
    let mut response = Redirect::to(redirect.as_str()).into_response();
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        a.config
            .authorization_csp_for_callback(Some(&pending.grant.redirect_uri))
            .parse()
            .map_err(internal)?,
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        format!("{cookie_name}=; Secure; HttpOnly; SameSite=Lax; Path=/oauth; Max-Age=0")
            .parse()
            .map_err(internal)?,
    );
    Ok(response)
}
#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    client_id: Option<String>,
    client_secret: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    resource: Option<String>,
    refresh_token: Option<String>,
}
async fn token(State(a): State<Auth>, headers: HeaderMap, Form(f): Form<TokenRequest>) -> Response {
    client_response(token_inner(a, &headers, f).await)
}
fn client_response(result: HttpResult<Json<Value>>) -> Response {
    let mut response = result.into_response();
    if response.status() == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Basic realm=\"octendra-code\""),
        );
    }
    response
}
async fn token_inner(a: Auth, headers: &HeaderMap, f: TokenRequest) -> HttpResult<Json<Value>> {
    let a = a.at_public_endpoint(headers);
    if !a.limit("token", 120) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"rate_limited"})),
        ));
    }
    let client = a
        .authenticate_client(headers, f.client_id.as_deref(), f.client_secret.as_deref())
        .await?;
    let client_id = &client.client_id;
    let access = match f.grant_type.as_str() {
        "authorization_code" => {
            let resource = a.client_resource(&client, f.resource.as_deref())?;
            let code = f.code.ok_or_else(invalid)?;
            let verifier = f.code_verifier.ok_or_else(invalid)?;
            if !(43..=128).contains(&verifier.len())
                || !verifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
            {
                return Err(invalid());
            }
            let g: Grant = a
                .store
                .get(&oauth_state_kind("code", &code), &hash(&code))
                .await
                .map_err(internal)?
                .ok_or_else(invalid)?;
            if &g.client_id != client_id
                || Some(g.redirect_uri) != f.redirect_uri
                || g.resource != resource
                || !secret_eq(
                    &g.code_challenge,
                    &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
                )
            {
                return Err(invalid());
            }
            // Invalid credentials, redirect or PKCE must not consume another
            // client's code. The final atomic take still enforces single use.
            a.store
                .take::<Grant>(&oauth_state_kind("code", &code), &hash(&code))
                .await
                .map_err(internal)?
                .ok_or_else(invalid)?;
            Access {
                principal: Principal {
                    owner: a.config.owner.clone(),
                    scopes: g.scope.split_whitespace().map(str::to_owned).collect(),
                },
                client_id: client_id.clone(),
                resource,
                family: random(),
            }
        }
        "refresh_token" => {
            let refresh = f.refresh_token.ok_or_else(invalid)?;
            let requested_resource = match f.resource.as_deref() {
                Some(explicit) => Some(a.client_resource(&client, Some(explicit))?),
                None => None,
            };
            let key = hash(&refresh);
            let refresh_kind = oauth_state_kind("refresh", &refresh);
            let used_kind = oauth_state_kind("used_refresh", &refresh);
            // Acquire the writer before reading either tokens or revocation. Concurrent
            // refresh/revoke calls cannot observe a half-issued successor or miss a revoke.
            let mut tx = a
                .store
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(internal)?;
            let observed_at = now();
            let row: Option<(String, i64)> =
                sqlx::query_as("SELECT value,expires FROM kv WHERE kind=? AND key=? AND expires>?")
                    .bind(&refresh_kind)
                    .bind(&key)
                    .bind(observed_at)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(internal)?;
            let Some((encoded, old_expires)) = row else {
                let used = match get_tx::<UsedRefresh>(
                    &mut tx,
                    "oauth_refresh_binding",
                    &key,
                    observed_at,
                )
                .await
                .map_err(internal)?
                {
                    Some(used) => Some(used),
                    None => get_tx::<UsedRefresh>(&mut tx, &used_kind, &key, observed_at)
                        .await
                        .map_err(internal)?,
                };
                let (family, retry_until) = match used {
                    Some(UsedRefresh::Bound {
                        family,
                        client_id: owner,
                        retry_until,
                    }) if &owner == client_id => (family, retry_until),
                    Some(UsedRefresh::Legacy(family))
                        if client.token_endpoint_auth_method == "none" =>
                    {
                        (family, None)
                    }
                    _ => return Err(invalid()),
                };
                if get_tx::<bool>(&mut tx, "revoked", &family, observed_at)
                    .await
                    .map_err(internal)?
                    .is_some()
                {
                    return Err(invalid());
                }
                if client.token_endpoint_auth_method != "none"
                    && retry_until.is_some_and(|until| observed_at < until)
                    && let Some(current) = get_tx::<CurrentTokens>(
                        &mut tx,
                        "oauth_refresh_family",
                        &family,
                        observed_at,
                    )
                    .await
                    .map_err(internal)?
                    && &current.access.client_id == client_id
                    && current.access.family == family
                    && requested_resource
                        .as_deref()
                        .is_none_or(|r| r == current.access.resource)
                    && current.access_expires_at > observed_at
                    && get_tx::<Access>(
                        &mut tx,
                        &oauth_state_kind("access", &current.bearer),
                        &hash(&current.bearer),
                        observed_at,
                    )
                    .await
                    .map_err(internal)?
                    .is_some()
                    && get_tx::<Access>(
                        &mut tx,
                        &oauth_state_kind("refresh", &current.refresh),
                        &hash(&current.refresh),
                        observed_at,
                    )
                    .await
                    .map_err(internal)?
                    .is_some()
                {
                    tx.commit().await.map_err(internal)?;
                    return Ok(current.response(observed_at));
                }
                // Public client IDs are not sender proof. An old token cannot enable
                // unlimited replay, even during the confidential-client retry grace.
                revoke_family_tx(&mut tx, &family).await.map_err(internal)?;
                tx.commit().await.map_err(internal)?;
                return Err(invalid());
            };
            let access: Access = serde_json::from_str(&encoded).map_err(internal)?;
            if &access.client_id != client_id
                || requested_resource
                    .as_deref()
                    .is_some_and(|r| r != access.resource)
                || get_tx::<bool>(&mut tx, "revoked", &access.family, observed_at)
                    .await
                    .map_err(internal)?
                    .is_some()
            {
                return Err(invalid());
            }
            sqlx::query("DELETE FROM kv WHERE kind=? AND key=?")
                .bind(&refresh_kind)
                .bind(&key)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
            let used = UsedRefresh::Bound {
                family: access.family.clone(),
                client_id: client_id.clone(),
                retry_until: (client.token_endpoint_auth_method != "none")
                    .then_some(observed_at + REFRESH_RETRY_WINDOW),
            };
            put_tx(
                &mut tx,
                &used_kind,
                &key,
                &access.family,
                old_expires.max(observed_at + REFRESH_TTL),
            )
            .await
            .map_err(internal)?;
            put_tx(
                &mut tx,
                "oauth_refresh_binding",
                &key,
                &used,
                old_expires.max(observed_at + REFRESH_TTL),
            )
            .await
            .map_err(internal)?;
            let tokens = issue_tokens_tx(
                &mut tx,
                access,
                client.token_endpoint_auth_method != "none",
                observed_at,
            )
            .await
            .map_err(internal)?;
            tx.commit().await.map_err(internal)?;
            return Ok(tokens.response(observed_at));
        }
        _ => return Err(invalid()),
    };
    let mut tx = a
        .store
        .pool
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(internal)?;
    let issued_at = now();
    let tokens = issue_tokens_tx(
        &mut tx,
        access,
        client.token_endpoint_auth_method != "none",
        issued_at,
    )
    .await
    .map_err(internal)?;
    tx.commit().await.map_err(internal)?;
    Ok(tokens.response(issued_at))
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum UsedRefresh {
    Bound {
        family: String,
        client_id: String,
        #[serde(default)]
        retry_until: Option<i64>,
    },
    Legacy(String),
}

async fn revoke(
    State(a): State<Auth>,
    headers: HeaderMap,
    Form(v): Form<HashMap<String, String>>,
) -> Response {
    client_response(revoke_inner(a, &headers, v).await)
}
async fn revoke_inner(
    a: Auth,
    headers: &HeaderMap,
    v: HashMap<String, String>,
) -> HttpResult<Json<Value>> {
    if !a.limit("revoke", 120) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"rate_limited"})),
        ));
    }
    let token = v.get("token").ok_or_else(invalid)?;
    let key = hash(token);
    let access: Option<Access> = match a
        .store
        .get(&oauth_state_kind("access", token), &key)
        .await
        .map_err(internal)?
    {
        Some(access) => Some(access),
        None => a
            .store
            .get(&oauth_state_kind("refresh", token), &key)
            .await
            .map_err(internal)?,
    };
    let binding = match access {
        Some(access) => Some((access.family, access.client_id)),
        None => match a
            .store
            .get::<UsedRefresh>("oauth_refresh_binding", &key)
            .await
            .map_err(internal)?
        {
            Some(UsedRefresh::Bound {
                family, client_id, ..
            }) => Some((family, client_id)),
            _ => None,
        },
    };
    // Preserve token-possession revocation for legacy public clients. A
    // confidential client always has to authenticate with its registered method.
    let legacy_id =
        if !headers.contains_key(header::AUTHORIZATION) && !v.contains_key("client_secret") {
            binding.as_ref().map(|(_, client_id)| client_id.as_str())
        } else {
            None
        };
    let id = v.get("client_id").map(String::as_str).or(legacy_id);
    if id.is_none()
        && !headers.contains_key(header::AUTHORIZATION)
        && !v.contains_key("client_secret")
    {
        return Ok(Json(json!({})));
    }
    let client = a
        .authenticate_client(headers, id, v.get("client_secret").map(String::as_str))
        .await?;
    if let Some((family, owner)) = binding
        && owner == client.client_id
    {
        let mut tx = a
            .store
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(internal)?;
        revoke_family_tx(&mut tx, &family).await.map_err(internal)?;
        for kind in ["access", "refresh"] {
            sqlx::query("DELETE FROM kv WHERE kind=? AND key=?")
                .bind(oauth_state_kind(kind, token))
                .bind(&key)
                .execute(&mut *tx)
                .await
                .map_err(internal)?;
        }
        tx.commit().await.map_err(internal)?;
    }
    Ok(Json(json!({})))
}
fn html_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
