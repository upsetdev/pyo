//! OAuth 2.1 authorization server for MCP clients, as the MCP
//! authorization spec describes:
//!
//! - discovery: protected resource metadata (RFC 9728) for `/api/mcp`, and
//!   authorization server metadata (RFC 8414);
//! - dynamic client registration (RFC 7591): public clients, no secret;
//! - authorization code with PKCE (S256 only): the signed-in user approves
//!   on the dashboard's `/oauth/authorize` page;
//! - access tokens (1 hour) and refresh tokens (30 days, rotated on use),
//!   bound to the MCP resource. Only hashes are stored.
//!
//! API keys (Profile → API keys) stay the way scripts call the REST API.

use axum::{
    Form, Json, Router,
    extract::{FromRequestParts, Query, State},
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    AppState,
    auth::{KeyAccess, KeyScope, SessionUser, User},
    error::{ApiError, ApiResult},
    ids::{new_id, secret_token, sha256_hex},
};

const SCOPE: &str = "pyo";
/// What a connected assistant may do, the same levels as API keys: "pyo"
/// (everything you can), "pyo:deploy" (look, and deploy, roll back,
/// restart, stop and scale) or "pyo:read" (look only). The user picks on the
/// consent page.
const SCOPES: [&str; 3] = [SCOPE, "pyo:deploy", "pyo:read"];

/// A grant's scope as API key access. A scope without one of Pyo's is full
/// access, as an authorization request without a scope gets.
pub fn scope_access(scope: &str) -> KeyAccess {
    let has = |s: &str| scope.split_whitespace().any(|t| t == s);
    if has(SCOPE) {
        KeyAccess::Full
    } else if has("pyo:deploy") {
        KeyAccess::Deploy
    } else if has("pyo:read") {
        KeyAccess::Read
    } else {
        KeyAccess::Full
    }
}

/// The scope stored for an access level.
fn access_scope(a: KeyAccess) -> &'static str {
    match a {
        KeyAccess::Full => SCOPE,
        KeyAccess::Deploy => "pyo:deploy",
        KeyAccess::Read => "pyo:read",
    }
}
const ACCESS_SECS: i64 = 3600;
const REFRESH_DAYS: i64 = 30;

/// This installation's address: `PYO_PUBLIC_URL` when set, else its HTTPS
/// address on the tailnet when the request came for that host, else plain
/// http at the request's Host (this machine, the tailnet's :8000).
/// X-Forwarded-Host/Proto aren't read: anyone could send them.
pub fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost:8000");
    pick_base_url(configured_public_url(), crate::web::public_url(), host)
}

fn configured_public_url() -> Option<&'static str> {
    static U: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    U.get_or_init(|| {
        std::env::var("PYO_PUBLIC_URL")
            .ok()
            .map(|u| u.trim().trim_end_matches('/').to_owned())
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
    })
    .as_deref()
}

fn pick_base_url(configured: Option<&str>, tailnet: Option<&str>, host: &str) -> String {
    if let Some(u) = configured {
        return u.to_owned();
    }
    let host =
        if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '/' || c == '@') {
            "localhost:8000"
        } else {
            host
        };
    match tailnet {
        Some(t)
            if t.strip_prefix("https://")
                .is_some_and(|h| h.eq_ignore_ascii_case(host)) =>
        {
            t.to_owned()
        }
        _ => format!("http://{host}"),
    }
}

pub fn mcp_resource(headers: &HeaderMap) -> String {
    format!("{}/api/mcp", base_url(headers))
}

/// Discovery documents live at the site root.
pub fn well_known() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource/api/mcp",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/.well-known/openid-configuration", get(server_metadata))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/token", post(token))
        .route("/client", get(client_info))
        .route("/approve", post(approve))
}

async fn resource_metadata(headers: HeaderMap) -> Json<Value> {
    Json(json!({
        "resource": mcp_resource(&headers),
        "authorization_servers": [base_url(&headers)],
        "scopes_supported": SCOPES,
        "bearer_methods_supported": ["header"],
        "resource_name": "Pyo",
    }))
}

async fn server_metadata(headers: HeaderMap) -> Json<Value> {
    let base = base_url(&headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/api/oauth/token"),
        "registration_endpoint": format!("{base}/api/oauth/register"),
        "scopes_supported": SCOPES,
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

/// OAuth errors are JSON with `error` and `error_description` (RFC 6749).
fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

/// Redirect URIs: https with a host, http on this machine (CLI clients
/// and editors), or a native app's own scheme (cursor://,
/// com.example.app:/). Never one with credentials, a fragment, or a scheme
/// a browser runs or reads locally.
fn redirect_ok(uri: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(uri) else {
        return false;
    };
    if u.fragment().is_some() || !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    match u.scheme() {
        "https" => u.host_str().is_some_and(|h| !h.is_empty()),
        "http" => matches!(u.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
        "javascript" | "data" | "file" | "vbscript" | "blob" | "about" | "filesystem" | "ws"
        | "wss" | "ftp" => false,
        // Native apps' private schemes (e.g. cursor://).
        s => s.contains('.') || s.chars().all(|c| c.is_ascii_lowercase()),
    }
}

/// Where a redirect URI sends people, for the consent page: the scheme and
/// host (and port), which the app's name can't disguise.
fn redirect_origin(uri: &str) -> String {
    let Ok(u) = reqwest::Url::parse(uri) else {
        return String::new();
    };
    match (u.scheme(), u.host_str()) {
        (scheme, Some(host)) => match u.port() {
            Some(p) => format!("{scheme}://{host}:{p}"),
            None => format!("{scheme}://{host}"),
        },
        (scheme, None) => format!("{scheme}:"),
    }
}

/// The resource a client asked for must be this server's MCP endpoint
/// (RFC 8707), at one of the addresses it has: a token is only ever good
/// there. None asked: fine.
fn resource_ok(asked: Option<&str>, ours: &[String]) -> bool {
    asked.is_none_or(|r| {
        r.is_empty()
            || ours.iter().any(|o| {
                o.trim_end_matches('/')
                    .eq_ignore_ascii_case(r.trim_end_matches('/'))
            })
    })
}

/// This installation's MCP endpoint at each of its addresses: the one the
/// request came to, the configured and tailnet ones, and this machine's.
fn our_resources(s: &AppState, headers: &HeaderMap) -> Vec<String> {
    let mut bases = vec![base_url(headers)];
    bases.extend(configured_public_url().map(str::to_owned));
    bases.extend(crate::web::public_url().map(str::to_owned));
    bases.extend(
        Some(s.config.tailnet_api_url.trim_end_matches('/').to_owned()).filter(|u| !u.is_empty()),
    );
    bases.extend([
        "http://localhost:8000".to_owned(),
        "http://127.0.0.1:8000".to_owned(),
    ]);
    bases.into_iter().map(|b| format!("{b}/api/mcp")).collect()
}

// ── Registration ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct RegisterBody {
    #[serde(default)]
    client_name: String,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

async fn register(
    State(s): State<AppState>,
    crate::ratelimit::ClientIp(ip): crate::ratelimit::ClientIp,
    Json(b): Json<RegisterBody>,
) -> Response {
    if b.redirect_uris.is_empty()
        || b.redirect_uris.len() > 10
        || !b.redirect_uris.iter().all(|u| redirect_ok(u))
    {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "Use https redirect URIs, or http on localhost.",
        );
    }
    // Unauthenticated, and each one is a row: all of them count.
    match crate::ratelimit::take(&s.db, &[(crate::ratelimit::OAUTH_REGISTER_IP, &ip)]).await {
        Ok(()) => {}
        Err(e) if e.status == StatusCode::TOO_MANY_REQUESTS => {
            let mut r = oauth_error(
                StatusCode::TOO_MANY_REQUESTS,
                "temporarily_unavailable",
                &e.message,
            );
            if let Some(secs) = e.retry_after {
                r.headers_mut().insert(header::RETRY_AFTER, secs.into());
            }
            return r;
        }
        Err(_) => {
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "Couldn't register the client.",
            );
        }
    }
    let name: String = if b.client_name.trim().is_empty() {
        "An MCP client".into()
    } else {
        b.client_name.trim().chars().take(80).collect()
    };
    let id = new_id("mcpc");
    if let Err(e) =
        sqlx::query("INSERT INTO pyo.oauth_clients (id, name, redirect_uris) VALUES ($1, $2, $3)")
            .bind(&id)
            .bind(&name)
            .bind(json!(b.redirect_uris))
            .execute(&s.db)
            .await
    {
        tracing::warn!(error = %e, "client registration");
        return oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Couldn't register the client.",
        );
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": id, "client_name": name, "redirect_uris": b.redirect_uris,
            "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"], "token_endpoint_auth_method": "none",
            "client_id_issued_at": pyo_proto::now_ms() / 1000,
        })),
    )
        .into_response()
}

// ── Consent (the dashboard's /oauth/authorize page) ─────────────────────────

#[derive(Deserialize)]
struct AuthorizeParams {
    client_id: String,
    redirect_uri: String,
    #[serde(default)]
    response_type: String,
    #[serde(default)]
    code_challenge: String,
    #[serde(default)]
    code_challenge_method: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

/// Checks the request the consent page shows; errors are for the page, not
/// redirects (an unknown client's redirect URI isn't trusted).
async fn checked(s: &AppState, headers: &HeaderMap, p: &AuthorizeParams) -> ApiResult<String> {
    let row: Option<(String, Value)> =
        sqlx::query_as("SELECT name, redirect_uris FROM pyo.oauth_clients WHERE id = $1")
            .bind(&p.client_id)
            .fetch_optional(&s.db)
            .await?;
    let (name, uris) = row.ok_or_else(|| {
        ApiError::bad_request(
            "This app isn't registered with Pyo. Try connecting again from the app.",
        )
    })?;
    if !uris
        .as_array()
        .is_some_and(|u| u.iter().any(|x| x == &json!(p.redirect_uri)))
    {
        return Err(ApiError::bad_request(
            "This app asked to return somewhere it didn't register.",
        ));
    }
    if p.response_type != "code" {
        return Err(ApiError::bad_request(
            "Unsupported response_type. Pyo only issues authorization codes.",
        ));
    }
    if p.code_challenge_method != "S256" || p.code_challenge.len() < 43 {
        return Err(ApiError::bad_request(
            "This app didn't send a PKCE (S256) challenge, which Pyo requires.",
        ));
    }
    if !resource_ok(p.resource.as_deref(), &our_resources(s, headers)) {
        return Err(ApiError::bad_request(
            "This app asked for access to something other than Pyo's MCP server.",
        ));
    }
    Ok(name)
}

/// What the consent page shows: who is asking, and where it sends you back.
/// The name is the app's own claim (anyone can register one), so the page
/// also shows where it sends you back.
async fn client_info(
    State(s): State<AppState>,
    headers: HeaderMap,
    SessionUser(_user): SessionUser,
    Query(p): Query<AuthorizeParams>,
) -> ApiResult<Json<Value>> {
    let name = checked(&s, &headers, &p).await?;
    Ok(Json(
        json!({ "name": name, "redirectHost": redirect_origin(&p.redirect_uri) }),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApproveBody {
    #[serde(flatten)]
    params: AuthorizeParams,
    allow: bool,
}

/// The user's answer: a redirect back to the client with a code, or with
/// access_denied.
async fn approve(
    State(s): State<AppState>,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Json(b): Json<ApproveBody>,
) -> ApiResult<Json<Value>> {
    let p = &b.params;
    checked(&s, &headers, p).await?;
    let mut url = reqwest::Url::parse(&p.redirect_uri)
        .map_err(|_| ApiError::bad_request("Invalid redirect URI."))?;
    let code = if b.allow {
        let code = secret_token();
        let resource = p
            .resource
            .clone()
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| mcp_resource(&headers));
        sqlx::query(
            "INSERT INTO pyo.oauth_codes (code_hash, client_id, user_id, redirect_uri, code_challenge, scope, resource, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, now() + interval '5 minutes')",
        )
        .bind(sha256_hex(&code))
        .bind(&p.client_id)
        .bind(&user.id)
        .bind(&p.redirect_uri)
        .bind(&p.code_challenge)
        .bind(access_scope(scope_access(p.scope.as_deref().unwrap_or(SCOPE))))
        .bind(&resource)
        .execute(&s.db)
        .await?;
        Some(code)
    } else {
        None
    };
    {
        let mut q = url.query_pairs_mut();
        match &code {
            Some(c) => q.append_pair("code", c),
            None => q.append_pair("error", "access_denied"),
        };
        if let Some(st) = &p.state {
            q.append_pair("state", st);
        }
        q.append_pair("iss", &base_url(&headers));
    }
    Ok(Json(json!({ "redirect": url.to_string() })))
}

// ── Tokens ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    refresh_token: String,
    /// RFC 8707: when sent, it has to be the grant's.
    #[serde(default)]
    resource: Option<String>,
}

fn pkce_ok(verifier: &str, challenge: &str) -> bool {
    if !(43..=128).contains(&verifier.len()) {
        return false;
    }
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest) == challenge
}

/// A fresh access/refresh pair for a grant, replacing the previous one
/// (whose refresh token is remembered, to notice it coming back).
async fn issue(s: &AppState, grant_id: &str, scope: &str) -> sqlx::Result<Value> {
    let access = format!("pyoa_{}", secret_token());
    let refresh = format!("pyor_{}", secret_token());
    sqlx::query(
        "UPDATE pyo.oauth_grants SET access_hash = $2, access_expires_at = now() + make_interval(secs => $4),
                previous_refresh_hash = refresh_hash,
                refresh_hash = $3, refresh_expires_at = now() + make_interval(days => $5) WHERE id = $1",
    )
    .bind(grant_id)
    .bind(sha256_hex(&access))
    .bind(sha256_hex(&refresh))
    .bind(ACCESS_SECS as f64)
    .bind(REFRESH_DAYS as i32)
    .execute(&s.db)
    .await?;
    Ok(
        json!({ "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_SECS, "refresh_token": refresh, "scope": scope }),
    )
}

async fn token(State(s): State<AppState>, Form(f): Form<TokenForm>) -> Response {
    let r: Result<Response, sqlx::Error> = async {
        match f.grant_type.as_str() {
            "authorization_code" => {
                // A code works once, even if the exchange fails.
                let row: Option<(String, String, String, String, String, String)> = sqlx::query_as(
                    "DELETE FROM pyo.oauth_codes WHERE code_hash = $1 AND expires_at > now()
                     RETURNING client_id, user_id, redirect_uri, code_challenge, scope, resource",
                )
                .bind(sha256_hex(&f.code))
                .fetch_optional(&s.db)
                .await?;
                let Some((client_id, user_id, redirect_uri, challenge, scope, resource)) = row else {
                    return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "The code is unknown, used or expired."));
                };
                if client_id != f.client_id || redirect_uri != f.redirect_uri || !pkce_ok(&f.code_verifier, &challenge) {
                    return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "The code doesn't match this client, redirect URI or PKCE verifier."));
                }
                if !resource_ok(f.resource.as_deref(), std::slice::from_ref(&resource)) {
                    return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_target", "The code was issued for another resource."));
                }
                let grant = new_id("grant");
                sqlx::query("INSERT INTO pyo.oauth_grants (id, client_id, user_id, scope, resource) VALUES ($1, $2, $3, $4, $5)")
                    .bind(&grant)
                    .bind(&client_id)
                    .bind(&user_id)
                    .bind(&scope)
                    .bind(&resource)
                    .execute(&s.db)
                    .await?;
                Ok(Json(issue(&s, &grant, &scope).await?).into_response())
            }
            "refresh_token" => {
                let hash = sha256_hex(&f.refresh_token);
                let row: Option<(String, String, String, String)> = sqlx::query_as("SELECT id, client_id, scope, resource FROM pyo.oauth_grants WHERE refresh_hash = $1 AND refresh_expires_at > now()")
                    .bind(&hash)
                    .fetch_optional(&s.db)
                    .await?;
                let Some((grant, client_id, scope, resource)) = row else {
                    // A refresh token that was already exchanged: someone
                    // else has a copy. The grant ends for both.
                    let revoked = sqlx::query("DELETE FROM pyo.oauth_grants WHERE previous_refresh_hash = $1").bind(&hash).execute(&s.db).await?.rows_affected();
                    if revoked > 0 {
                        tracing::warn!("a used OAuth refresh token came back; its grant is revoked");
                    }
                    return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "The refresh token is unknown, used or expired. Connect again."));
                };
                if !f.client_id.is_empty() && f.client_id != client_id {
                    return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", "This refresh token belongs to another client."));
                }
                if !resource_ok(f.resource.as_deref(), std::slice::from_ref(&resource)) {
                    return Ok(oauth_error(StatusCode::BAD_REQUEST, "invalid_target", "This refresh token is for another resource."));
                }
                Ok(Json(issue(&s, &grant, &scope).await?).into_response())
            }
            _ => Ok(oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "Use authorization_code or refresh_token.")),
        }
    }
    .await;
    let mut resp = r.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "token endpoint");
        oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "Couldn't issue a token.",
        )
    });
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    resp
}

// ── Using a token (the MCP endpoint) ─────────────────────────────────────────

/// A user acting through an OAuth access token issued for the MCP resource,
/// and the name of the assistant they approved.
pub struct OAuthUser(pub User, pub String);

impl FromRequestParts<AppState> for OAuthUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, s: &AppState) -> Result<Self, Self::Rejection> {
        // 401 points the client at how to get a token (RFC 9728).
        let challenge = |error: Option<&str>| {
            let meta = format!(
                "{}/.well-known/oauth-protected-resource/api/mcp",
                base_url(&parts.headers)
            );
            let value = match error {
                Some(e) => {
                    format!("Bearer error=\"{e}\", resource_metadata=\"{meta}\", scope=\"{SCOPE}\"")
                }
                None => format!("Bearer resource_metadata=\"{meta}\", scope=\"{SCOPE}\""),
            };
            let mut r = (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Sign in to Pyo: this MCP server uses OAuth." })),
            )
                .into_response();
            if let Ok(v) = header::HeaderValue::from_str(&value) {
                r.headers_mut().insert(header::WWW_AUTHENTICATE, v);
            }
            r
        };
        let Some(token) = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
        else {
            return Err(challenge(None));
        };
        let hash = sha256_hex(token);
        let (id, name, email, timezone, is_admin, must_set_up, client, scope): (String, String, String, String, bool, bool, String, String) = sqlx::query_as(
            "SELECT u.id, u.name, u.email, u.timezone, u.is_admin, u.must_set_up, c.name, g.scope
               FROM pyo.oauth_grants g JOIN pyo.users u ON u.id = g.user_id JOIN pyo.oauth_clients c ON c.id = g.client_id
              WHERE g.access_hash = $1 AND g.access_expires_at > now()",
        )
        .bind(&hash)
        .fetch_optional(&s.db)
        .await
        .map_err(|_| challenge(None))?
        .ok_or_else(|| challenge(Some("invalid_token")))?;
        // Still on the installer's printed sign-in: nothing works until it's replaced.
        if must_set_up {
            return Err(challenge(Some("invalid_token")));
        }
        // The same limits as an API key with the access the user chose, in
        // all their teams: what the account and the installation allow stays
        // with the dashboard.
        let user = User {
            id,
            name,
            email,
            timezone,
            is_admin,
            must_set_up,
            key: Some(KeyScope {
                access: scope_access(&scope),
                teams: None,
            }),
        };
        let _ = sqlx::query("UPDATE pyo.oauth_grants SET last_used_at = now() WHERE access_hash = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')")
            .bind(&hash)
            .execute(&s.db)
            .await;
        Ok(OAuthUser(user, client))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_access_levels() {
        assert_eq!(scope_access("pyo"), KeyAccess::Full);
        assert_eq!(scope_access("pyo:deploy"), KeyAccess::Deploy);
        assert_eq!(scope_access("pyo:read"), KeyAccess::Read);
        assert_eq!(scope_access("openid pyo:read"), KeyAccess::Read);
        assert_eq!(scope_access(""), KeyAccess::Full);
        assert_eq!(access_scope(scope_access("pyo:deploy")), "pyo:deploy");
    }

    #[test]
    fn pkce_s256() {
        // RFC 7636 appendix B.
        assert!(pkce_ok(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!pkce_ok(
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
            "nope"
        ));
    }

    #[test]
    fn redirect_uris() {
        // What MCP clients register: Claude, Claude Code, Cursor, VS Code.
        for ok in [
            "https://claude.ai/api/mcp/auth_callback",
            "http://localhost:33418/callback",
            "http://127.0.0.1:5000/cb",
            "http://[::1]:5000/cb",
            "cursor://anysphere.cursor-retrieval/oauth/user-pyo/callback",
            "https://vscode.dev/redirect",
            "com.example.app:/oauth",
        ] {
            assert!(redirect_ok(ok), "{ok}");
        }
        for bad in [
            "http://evil.example.com/cb",
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,x",
            "file:///etc/passwd",
            "vbscript:x",
            "https://x.example.com/cb#frag",
            "https://user:pw@x.example.com/cb",
            "http://localhost.evil.com/cb",
            "not a url",
        ] {
            assert!(!redirect_ok(bad), "{bad}");
        }
        assert_eq!(
            redirect_origin("https://claude.ai/api/mcp/auth_callback"),
            "https://claude.ai"
        );
        assert_eq!(
            redirect_origin("http://127.0.0.1:5000/cb"),
            "http://127.0.0.1:5000"
        );
        assert_eq!(
            redirect_origin("cursor://anysphere.cursor-retrieval/oauth"),
            "cursor://anysphere.cursor-retrieval"
        );
        assert_eq!(
            redirect_origin("com.example.app:/oauth"),
            "com.example.app:"
        );
    }

    #[test]
    fn tokens_are_for_this_mcp_server_only() {
        let ours = [
            "https://pyo.tail1.ts.net:8443/api/mcp".to_owned(),
            "http://100.64.0.1:8000/api/mcp".to_owned(),
        ];
        assert!(resource_ok(None, &ours));
        assert!(resource_ok(Some(""), &ours));
        assert!(resource_ok(
            Some("https://pyo.tail1.ts.net:8443/api/mcp"),
            &ours
        ));
        assert!(resource_ok(Some("http://100.64.0.1:8000/api/mcp"), &ours));
        assert!(resource_ok(
            Some("https://pyo.tail1.ts.net:8443/api/mcp/"),
            &ours
        ));
        assert!(!resource_ok(
            Some("https://other.example.com/api/mcp"),
            &ours
        ));
        assert!(!resource_ok(
            Some("https://pyo.tail1.ts.net:8443/api"),
            &ours
        ));
    }

    #[test]
    fn the_issuer_isnt_read_from_forwarded_headers() {
        let tailnet = Some("https://pyo.tail1.ts.net:8443");
        // Set: always that.
        assert_eq!(
            pick_base_url(Some("https://pyo.example.com"), tailnet, "evil.example.com"),
            "https://pyo.example.com"
        );
        // The tailnet name: https.
        assert_eq!(
            pick_base_url(None, tailnet, "pyo.tail1.ts.net:8443"),
            "https://pyo.tail1.ts.net:8443"
        );
        assert_eq!(
            pick_base_url(None, tailnet, "PYO.tail1.ts.net:8443"),
            "https://pyo.tail1.ts.net:8443"
        );
        // Anything else: plain http at that host, as it came.
        assert_eq!(
            pick_base_url(None, tailnet, "100.64.0.1:8000"),
            "http://100.64.0.1:8000"
        );
        assert_eq!(
            pick_base_url(None, None, "localhost:8000"),
            "http://localhost:8000"
        );
        assert_eq!(
            pick_base_url(None, None, "evil.com/x@"),
            "http://localhost:8000"
        );
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "localhost:8000".parse().unwrap());
        h.insert("x-forwarded-host", "evil.example.com".parse().unwrap());
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(!base_url(&h).contains("evil"));
    }
}
