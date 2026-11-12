//! Passkeys (WebAuthn): sign in without a password, and manage them on
//! the profile page. webauthn-rs checks every ceremony; its state stays in
//! `pyo.passkey_challenges`, single use and a few minutes long, so the
//! browser only ever holds a challenge id.
//!
//! A passkey belongs to a host name (its relying party ID): one added at
//! `https://pyo.example.com` doesn't work at `http://localhost:8000`. The
//! dashboard's address is the browser's `Origin`, accepted when Pyo is
//! served there:
//!
//! - `localhost` on any port (http is fine there: browsers treat it as secure);
//! - its tailnet HTTPS address (`PYO_TAILNET_NAME`, web.rs);
//! - the dashboard address set in Settings → Email;
//! - any HTTPS origin in `PYO_PASSKEY_ORIGINS` (comma-separated), for custom
//!   domains.
//!
//! Adding a passkey takes the account's password. Every account keeps its
//! password, so removing passkeys never locks anyone out, and needs only the
//! session.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    routing::{patch, post},
};
use axum_extra::extract::CookieJar;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use webauthn_rs::prelude::{
    DiscoverableAuthentication, Passkey, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Uuid, Webauthn, WebauthnBuilder,
};

use crate::{
    AppState,
    auth::{SessionUser, User, password_matches, require_csrf, start_session},
    error::{ApiError, ApiResult},
    ids::{new_id, secret_token},
    ratelimit::{self, ClientIp},
    util::require_text,
};

/// How long a ceremony may take, browser prompt included.
const CHALLENGE_MINUTES: i32 = 5;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/passkey/login/start", post(login_start))
        .route("/passkey/login/finish", post(login_finish))
        .route("/passkeys", axum::routing::get(list))
        .route("/passkeys/register/start", post(register_start))
        .route("/passkeys/register/finish", post(register_finish))
        .route("/passkeys/{id}", patch(rename).delete(remove))
}

// ── Relying party ──────────────────────────────────────────────────────────

/// Where a ceremony runs: the host name passkeys belong to, and the origin
/// the browser must report.
#[derive(Debug, Clone, PartialEq)]
struct Rp {
    id: String,
    origin: Url,
}

impl Rp {
    fn webauthn(&self) -> ApiResult<Webauthn> {
        WebauthnBuilder::new(&self.id, &self.origin)
            .and_then(|b| b.rp_name("Pyo").build())
            .map_err(|e| anyhow::anyhow!("passkey setup for {}: {e:?}", self.id).into())
    }
}

/// HTTPS origins Pyo's dashboard is served at, besides localhost.
async fn allowed_origins(state: &AppState) -> Vec<Url> {
    let mut out: Vec<Url> = Vec::new();
    if let Some(u) = crate::web::public_url() {
        out.extend(Url::parse(u).ok());
    }
    if let Ok(Some(mail)) = crate::mail::settings(&state.db).await {
        out.extend(Url::parse(mail.public_url.trim()).ok());
    }
    out.extend(env_origins());
    out.retain(|u| u.scheme() == "https" && u.domain().is_some());
    out
}

/// `PYO_PASSKEY_ORIGINS`: more HTTPS addresses of the dashboard.
fn env_origins() -> Vec<Url> {
    std::env::var("PYO_PASSKEY_ORIGINS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| Url::parse(s).ok())
        .collect()
}

/// The ceremony's relying party, if `origin` is one Pyo is served at.
fn rp_for(origin: &Url, allowed: &[Url]) -> Option<Rp> {
    let host = origin.domain()?.to_ascii_lowercase();
    let ok = if host == "localhost" {
        matches!(origin.scheme(), "http" | "https")
    } else {
        origin.scheme() == "https" && allowed.iter().any(|a| a.origin() == origin.origin())
    };
    let origin = Url::parse(&origin.origin().ascii_serialization()).ok()?;
    ok.then_some(Rp { id: host, origin })
}

async fn relying_party(state: &AppState, headers: &HeaderMap) -> ApiResult<Rp> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|o| Url::parse(o).ok());
    let allowed = allowed_origins(state).await;
    origin
        .as_ref()
        .and_then(|o| rp_for(o, &allowed))
        .ok_or_else(|| {
            ApiError::bad_request(match allowed.first() {
                Some(u) => format!("Passkeys work at {}.", u.origin().ascii_serialization()),
                None => "Passkeys don't work at this address.".into(),
            })
        })
}

// ── Challenges ─────────────────────────────────────────────────────────────

async fn save_challenge(
    state: &AppState,
    kind: &str,
    user_id: Option<&str>,
    rp: &Rp,
    ceremony: Value,
) -> ApiResult<String> {
    // Expired ceremonies go whenever a new one starts.
    sqlx::query("DELETE FROM pyo.passkey_challenges WHERE expires_at < now()")
        .execute(&state.db)
        .await?;
    let id = secret_token();
    sqlx::query(
        "INSERT INTO pyo.passkey_challenges (id, kind, user_id, rp_id, origin, state, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(mins => $7))",
    )
    .bind(&id)
    .bind(kind)
    .bind(user_id)
    .bind(&rp.id)
    .bind(rp.origin.as_str())
    .bind(ceremony)
    .bind(CHALLENGE_MINUTES)
    .execute(&state.db)
    .await?;
    Ok(id)
}

/// The ceremony's state, used up: a challenge answers once.
async fn take_challenge(
    state: &AppState,
    id: &str,
    kind: &str,
    user_id: Option<&str>,
) -> ApiResult<(Rp, Value)> {
    let row: Option<(String, String, Value)> = sqlx::query_as(
        "DELETE FROM pyo.passkey_challenges
          WHERE id = $1 AND kind = $2 AND user_id IS NOT DISTINCT FROM $3 AND expires_at > now()
         RETURNING rp_id, origin, state",
    )
    .bind(id)
    .bind(kind)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?;
    let (rp_id, origin, ceremony) =
        row.ok_or_else(|| ApiError::bad_request("That took too long. Try again."))?;
    let origin = Url::parse(&origin).map_err(|e| anyhow::anyhow!("stored passkey origin: {e}"))?;
    Ok((Rp { id: rp_id, origin }, ceremony))
}

fn failed(e: impl std::fmt::Debug) -> ApiError {
    tracing::info!(error = ?e, "passkey ceremony failed");
    ApiError::bad_request("That passkey didn't work. Try again.")
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    anyhow::anyhow!("passkey state: {e}").into()
}

// ── Sign in ────────────────────────────────────────────────────────────────

/// Options for `navigator.credentials.get`: any passkey of this host name.
async fn login_start(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_csrf(&headers)?;
    // Each start stores a challenge: all of them count.
    ratelimit::take(&state.db, &[(ratelimit::PASSKEY_START_IP, &ip)]).await?;
    let rp = relying_party(&state, &headers).await?;
    let (options, ceremony) = rp
        .webauthn()?
        .start_discoverable_authentication()
        .map_err(failed)?;
    let id = save_challenge(
        &state,
        "login",
        None,
        &rp,
        serde_json::to_value(&ceremony).map_err(internal)?,
    )
    .await?;
    Ok(Json(
        json!({ "challengeId": id, "publicKey": options.public_key }),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginFinish {
    challenge_id: String,
    credential: PublicKeyCredential,
}

async fn login_finish(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<LoginFinish>,
) -> ApiResult<(CookieJar, Json<User>)> {
    require_csrf(&headers)?;
    // Only failures keep their slot.
    ratelimit::take(&state.db, &[(ratelimit::PASSKEY_FINISH_IP, &ip)]).await?;
    let user = verify_passkey(&state, body).await?;
    ratelimit::forgive(&state.db, &ratelimit::PASSKEY_FINISH_IP, &ip).await?;
    let jar = start_session(&state, jar, &user.id, &headers).await?;
    Ok((jar, Json(user)))
}

/// Whose passkey signed the challenge.
async fn verify_passkey(state: &AppState, body: LoginFinish) -> ApiResult<User> {
    let (rp, ceremony) = take_challenge(state, &body.challenge_id, "login", None).await?;
    let ceremony: DiscoverableAuthentication =
        serde_json::from_value(ceremony).map_err(internal)?;
    let webauthn = rp.webauthn()?;
    let (handle, cred_id) = webauthn
        .identify_discoverable_authentication(&body.credential)
        .map_err(failed)?;
    let row: Option<(String, String, Value)> = sqlx::query_as("SELECT id, user_id, passkey FROM pyo.passkeys WHERE credential_id = $1 AND rp_id = $2 AND user_handle = $3")
        .bind(URL_SAFE_NO_PAD.encode(cred_id))
        .bind(&rp.id)
        .bind(handle.to_string())
        .fetch_optional(&state.db)
        .await?;
    let (id, user_id, stored) = row.ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "That passkey isn't set up here. Sign in with your password.",
        )
    })?;
    let mut passkey: Passkey = serde_json::from_value(stored).map_err(internal)?;
    let result = webauthn
        .finish_discoverable_authentication_v2(&body.credential, ceremony, &passkey)
        .map_err(failed)?;
    let updated = passkey.update_credential(&result) == Some(true);
    sqlx::query("UPDATE pyo.passkeys SET last_used_at = now(), passkey = CASE WHEN $2 THEN $3 ELSE passkey END WHERE id = $1")
        .bind(&id)
        .bind(updated)
        .bind(serde_json::to_value(&passkey).map_err(internal)?)
        .execute(&state.db)
        .await?;
    Ok(sqlx::query_as::<_, User>(
        "SELECT id, name, email, timezone, is_admin, must_set_up FROM pyo.users WHERE id = $1",
    )
    .bind(&user_id)
    .fetch_one(&state.db)
    .await?)
}

// ── Manage ─────────────────────────────────────────────────────────────────

/// Passkeys are managed from a browser session, never with an API key.
fn browser_only(user: &User) -> ApiResult<()> {
    if user.key.is_some() {
        Err(ApiError::forbidden("Manage passkeys in the dashboard."))
    } else {
        Ok(())
    }
}

async fn list(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
) -> ApiResult<Json<Vec<Value>>> {
    browser_only(&user)?;
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', id, 'name', name, 'rpId', rp_id, 'createdAt', pyo.ms(created_at), 'lastUsedAt', pyo.ms(last_used_at))
           FROM pyo.passkeys WHERE user_id = $1 ORDER BY created_at",
    )
    .bind(&user.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct RegisterStart {
    password: String,
}

/// Options for `navigator.credentials.create`. Only with the account's
/// password: a stolen session alone can't add a way back in.
async fn register_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Json(body): Json<RegisterStart>,
) -> ApiResult<Json<Value>> {
    browser_only(&user)?;
    ratelimit::take(&state.db, &[(ratelimit::PASSKEY_PASSWORD_USER, &user.id)]).await?;
    if !password_matches(&state.db, &user.id, &body.password).await? {
        return Err(ApiError::forbidden("Wrong password."));
    }
    ratelimit::reset(&state.db, &ratelimit::PASSKEY_PASSWORD_USER, &user.id).await?;
    let rp = relying_party(&state, &headers).await?;
    let existing: Vec<(String, Value)> =
        sqlx::query_as("SELECT user_handle, passkey FROM pyo.passkeys WHERE user_id = $1")
            .bind(&user.id)
            .fetch_all(&state.db)
            .await?;
    // One handle per account: a passkey manager groups them by it.
    let handle = existing
        .first()
        .and_then(|(h, _)| Uuid::parse_str(h).ok())
        .unwrap_or_else(|| {
            let mut b = [0u8; 16];
            rand::fill(&mut b);
            uuid_v4(b)
        });
    // Not the same authenticator twice.
    let exclude: Vec<_> = existing
        .into_iter()
        .filter_map(|(_, p)| serde_json::from_value::<Passkey>(p).ok())
        .map(|p| p.cred_id().clone())
        .collect();
    let (options, ceremony) = rp
        .webauthn()?
        .start_passkey_registration(handle, &user.email, &user.name, Some(exclude))
        .map_err(failed)?;
    let mut public_key = serde_json::to_value(&options.public_key).map_err(internal)?;
    // A discoverable credential, so sign-in needs no email: what platform
    // passkeys make anyway, asked for explicitly.
    if let Some(sel) = public_key
        .get_mut("authenticatorSelection")
        .and_then(Value::as_object_mut)
    {
        sel.insert("residentKey".into(), json!("required"));
        sel.insert("requireResidentKey".into(), json!(true));
    }
    let ceremony = json!({ "handle": handle.to_string(), "registration": serde_json::to_value(&ceremony).map_err(internal)? });
    let id = save_challenge(&state, "register", Some(&user.id), &rp, ceremony).await?;
    Ok(Json(json!({ "challengeId": id, "publicKey": public_key })))
}

/// A random (version 4) UUID from 16 random bytes.
fn uuid_v4(mut b: [u8; 16]) -> Uuid {
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterFinish {
    challenge_id: String,
    credential: RegisterPublicKeyCredential,
    name: Option<String>,
}

async fn register_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Json(body): Json<RegisterFinish>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    browser_only(&user)?;
    let (rp, ceremony) =
        take_challenge(&state, &body.challenge_id, "register", Some(&user.id)).await?;
    let handle = ceremony["handle"].as_str().unwrap_or_default().to_owned();
    let registration: PasskeyRegistration =
        serde_json::from_value(ceremony["registration"].clone()).map_err(internal)?;
    let passkey = rp
        .webauthn()?
        .finish_passkey_registration(&body.credential, &registration)
        .map_err(failed)?;
    let name = match body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        Some(n) => require_text(n, "Name", 60)?,
        None => default_name(&headers),
    };
    let id = new_id("pk");
    let added = sqlx::query(
        "INSERT INTO pyo.passkeys (id, user_id, credential_id, user_handle, rp_id, name, passkey)
         VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (credential_id) DO NOTHING",
    )
    .bind(&id)
    .bind(&user.id)
    .bind(URL_SAFE_NO_PAD.encode(passkey.cred_id()))
    .bind(&handle)
    .bind(&rp.id)
    .bind(&name)
    .bind(serde_json::to_value(&passkey).map_err(internal)?)
    .execute(&state.db)
    .await?
    .rows_affected();
    if added == 0 {
        return Err(ApiError::conflict("This passkey is already added."));
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "name": name, "rpId": rp.id })),
    ))
}

/// "Chrome on macOS": what the passkey was added from.
fn default_name(headers: &HeaderMap) -> String {
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(r) = woothee::parser::Parser::new().parse(ua) else {
        return "Passkey".into();
    };
    let os = r.os.replace("Mac OSX", "macOS");
    match (r.name, os.as_str()) {
        ("UNKNOWN", _) => "Passkey".into(),
        (b, "UNKNOWN") => b.to_owned(),
        (b, os) => format!("{b} on {os}"),
    }
}

#[derive(Deserialize)]
struct RenameBody {
    name: String,
}

async fn rename(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Json(body): Json<RenameBody>,
) -> ApiResult<StatusCode> {
    browser_only(&user)?;
    let name = require_text(&body.name, "Name", 60)?;
    let n = sqlx::query("UPDATE pyo.passkeys SET name = $3 WHERE id = $1 AND user_id = $2")
        .bind(&id)
        .bind(&user.id)
        .bind(&name)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        Err(ApiError::not_found("Passkey"))
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

async fn remove(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    browser_only(&user)?;
    // The password stays, so this can't lock anyone out.
    let n = sqlx::query("DELETE FROM pyo.passkeys WHERE id = $1 AND user_id = $2")
        .bind(&id)
        .bind(&user.id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if n == 0 {
        Err(ApiError::not_found("Passkey"))
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn passkeys_run_where_the_dashboard_is_served() {
        let allowed = [
            url("https://server-1.tail1234.ts.net:8443"),
            url("https://pyo.example.com/"),
        ];
        let rp = |o: &str| rp_for(&url(o), &allowed).map(|r| (r.id, r.origin.to_string()));
        assert_eq!(
            rp("http://localhost:5173"),
            Some(("localhost".into(), "http://localhost:5173/".into()))
        );
        assert_eq!(
            rp("https://server-1.tail1234.ts.net:8443"),
            Some((
                "server-1.tail1234.ts.net".into(),
                "https://server-1.tail1234.ts.net:8443/".into()
            ))
        );
        assert_eq!(
            rp("https://pyo.example.com"),
            Some(("pyo.example.com".into(), "https://pyo.example.com/".into()))
        );
        // Another port, plain HTTP, an IP address or a stranger: no.
        assert_eq!(rp("https://server-1.tail1234.ts.net"), None);
        assert_eq!(rp("http://pyo.example.com"), None);
        assert_eq!(rp("http://127.0.0.1:8000"), None);
        assert_eq!(rp("https://evil.example.com"), None);
        assert_eq!(rp("https://pyo.example.com.evil.net"), None);
    }

    #[test]
    fn every_relying_party_builds() {
        for o in [
            "http://localhost:8000",
            "https://server-1.tail1234.ts.net:8443",
        ] {
            let rp = rp_for(&url(o), &[url("https://server-1.tail1234.ts.net:8443")]).unwrap();
            let (options, _) = rp
                .webauthn()
                .unwrap()
                .start_discoverable_authentication()
                .unwrap();
            assert_eq!(options.public_key.rp_id, rp.id);
        }
    }

    #[test]
    fn handles_are_v4_uuids() {
        let u = uuid_v4([0xff; 16]);
        assert_eq!(u.get_version_num(), 4);
        assert_ne!(uuid_v4([1; 16]), uuid_v4([2; 16]));
    }
}
