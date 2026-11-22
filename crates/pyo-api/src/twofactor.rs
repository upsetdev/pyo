//! Two-factor authentication with an authenticator app (TOTP, RFC 6238).
//!
//! Set up on the profile page: the account's password first, then a QR code
//! (or the secret to type in), then one code from the app to confirm. That
//! turns it on and shows 10 one-time recovery codes, once. The secret is
//! sealed with the installation's key (it's the account's, not a team's);
//! only the recovery codes' sha256 is kept.
//!
//! Signing in with a password (or a sign-in link) then asks for a code
//! from the app or a recovery code ([`start_challenge`], `/login/code`). A
//! passkey is enough on its own: it already proves the device and the
//! person. Each code works once (the last accepted time step is kept), and
//! wrong codes count per account and address (ratelimit.rs).
//!
//! Turning it off, or making new recovery codes, takes a code too. It
//! can't be turned off while it's required: a team the account is in
//! requires it (Team › Security), the installation does (Settings ›
//! Security), or the account is Pyo Cloud staff (an admin of a hosted
//! Pyo). Until then those teams are out of reach (rls.rs leaves them out
//! of the request's scope), and staff aren't admins, but the account and
//! its profile still work, so it can be set up.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{Router, extract::State, http::HeaderMap, routing::post};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use totp_rs::{Algorithm, Builder, Secret, Totp};

use crate::{
    AppState,
    audit::{self, Entry, Who},
    auth::{SessionUser, User, password_matches, require_csrf, start_session},
    error::{ApiError, ApiResult, Json},
    ids::{secret_token, sha256_hex},
    ratelimit::{self, ClientIp},
};

/// Recovery codes made at a time.
pub const RECOVERY_CODES: usize = 10;
/// How long the code step of a sign-in waits.
const CHALLENGE_MINUTES: i32 = 5;
/// Codes one sign-in may try before it starts over.
const CHALLENGE_TRIES: i32 = 5;
const STEP: u64 = 30;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/login/code", post(login_code))
        .route("/two-factor/setup", post(setup))
        .route("/two-factor/enable", post(enable))
        .route("/two-factor/disable", post(disable))
        .route("/two-factor/recovery-codes", post(new_recovery_codes))
}

// ── Codes ──────────────────────────────────────────────────────────────────

fn totp(secret: &[u8]) -> ApiResult<Totp> {
    Builder::new()
        .with_algorithm(Algorithm::SHA1)
        .with_digits(6)
        .with_skew(1)
        .with_step_duration(STEP)
        .with_secret(Secret::from(secret.to_vec()))
        .build()
        .map_err(|e| anyhow::anyhow!("authenticator secret: {e:?}").into())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The time step `code` is good for at `time`, if it's the app's code and
/// newer than `last_step` (a code works once).
fn check_code(secret: &[u8], code: &str, time: u64, last_step: u64) -> Option<u64> {
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    if code.len() != 6 || !code.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    totp(secret)
        .ok()?
        .check(&code, time)
        .filter(|step| *step > last_step)
}

/// Letters and digits that can't be mistaken for each other.
const RECOVERY_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";

/// One recovery code: `xxxxx-xxxxx` (about 49 bits).
fn recovery_code() -> String {
    let mut bytes = [0u8; 10];
    rand::fill(&mut bytes);
    let chars: String = bytes
        .iter()
        .map(|b| RECOVERY_ALPHABET[*b as usize % RECOVERY_ALPHABET.len()] as char)
        .collect();
    format!("{}-{}", &chars[..5], &chars[5..])
}

/// A recovery code as typed: case, spaces and the dash don't matter.
fn normalize_recovery(code: &str) -> Option<String> {
    let c: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (c.len() == 10).then_some(c)
}

fn recovery_hash(code: &str) -> Option<String> {
    normalize_recovery(code).map(|c| sha256_hex(&c))
}

/// New recovery codes for the account, replacing the old ones.
async fn replace_recovery_codes(conn: &mut PgConnection, user_id: &str) -> ApiResult<Vec<String>> {
    let codes: Vec<String> = (0..RECOVERY_CODES).map(|_| recovery_code()).collect();
    sqlx::query("DELETE FROM pyo.recovery_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    for c in &codes {
        sqlx::query("INSERT INTO pyo.recovery_codes (user_id, code_hash) VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(user_id)
            .bind(recovery_hash(c))
            .execute(&mut *conn)
            .await?;
    }
    Ok(codes)
}

/// Which kind of code worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Used {
    App,
    Recovery,
}

/// Checks a code from the app, or a recovery code (used up), for an account
/// with two-factor on. Wrong ones count against the account and address.
async fn verify(state: &AppState, user_id: &str, ip: &str, code: &str) -> ApiResult<Option<Used>> {
    ratelimit::take(
        &state.db,
        &[
            (ratelimit::TWO_FACTOR_ACCOUNT, user_id),
            (ratelimit::TWO_FACTOR_IP, ip),
        ],
    )
    .await?;
    let used = verify_once(&state.db, &state.sealer, user_id, code, now()).await?;
    if used.is_some() {
        ratelimit::reset(&state.db, &ratelimit::TWO_FACTOR_ACCOUNT, user_id).await?;
        ratelimit::forgive(&state.db, &ratelimit::TWO_FACTOR_IP, ip).await?;
    }
    Ok(used)
}

/// [`verify`] without the limits, at `time`.
async fn verify_once(
    db: &PgPool,
    sealer: &crate::secrets::Sealer,
    user_id: &str,
    code: &str,
    time: u64,
) -> ApiResult<Option<Used>> {
    let mut tx = db.begin().await?;
    // Locked: two requests with the same code can't both get in.
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT secret_sealed, last_step FROM pyo.user_totp WHERE user_id = $1 AND enabled_at IS NOT NULL FOR UPDATE",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((sealed, last_step)) = row else {
        return Ok(None);
    };
    let secret = open_secret(sealer, &sealed)?;
    if let Some(step) = check_code(&secret, code, time, last_step.max(0) as u64) {
        sqlx::query("UPDATE pyo.user_totp SET last_step = $2 WHERE user_id = $1")
            .bind(user_id)
            .bind(step as i64)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(Some(Used::App));
    }
    let Some(hash) = recovery_hash(code) else {
        return Ok(None);
    };
    let n = sqlx::query(
        "UPDATE pyo.recovery_codes SET used_at = now() WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(hash)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok((n == 1).then_some(Used::Recovery))
}

fn open_secret(sealer: &crate::secrets::Sealer, sealed: &str) -> ApiResult<Vec<u8>> {
    let b32 = sealer
        .open(sealed)
        .map_err(|e| anyhow::anyhow!("authenticator secret: {e}"))?;
    Secret::try_from_base32(&b32)
        .map(|s| s.as_bytes().to_vec())
        .map_err(|e| anyhow::anyhow!("authenticator secret: {e:?}").into())
}

// ── State ──────────────────────────────────────────────────────────────────

/// Whether the account has two-factor on.
pub async fn enabled(db: impl sqlx::PgExecutor<'_>, user_id: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.user_totp WHERE user_id = $1 AND enabled_at IS NOT NULL)",
    )
    .bind(user_id)
    .fetch_one(db)
    .await
}

/// Why the account must keep two-factor, if it must: the installation, Pyo
/// Cloud's staff rule, or the first team that requires it.
async fn required_by(db: &PgPool, user_id: &str) -> ApiResult<Option<String>> {
    let s = audit::settings(db).await?;
    if s.require_two_factor {
        return Ok(Some("This Pyo requires two-factor authentication.".into()));
    }
    let admin: bool = sqlx::query_scalar("SELECT is_admin FROM pyo.users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(db)
        .await?
        .unwrap_or(false);
    if crate::hosted() && admin {
        return Ok(Some(
            "Pyo Cloud staff always use two-factor authentication.".into(),
        ));
    }
    let team: Option<String> = sqlx::query_scalar(
        "SELECT t.name FROM pyo.members m JOIN pyo.teams t ON t.id = m.team_id
          WHERE m.user_id = $1 AND m.status = 'active' AND t.require_two_factor ORDER BY t.name LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(db)
    .await?;
    Ok(team.map(|t| format!("{t} requires two-factor authentication.")))
}

/// What the dashboard shows about the account's two-factor: `enabled`,
/// `recoveryCodesLeft`, `required` (the installation or the staff rule
/// requires it of this account), `setupNeeded` (required and not on yet:
/// only the profile works) and `lockedTeams` (teams it can't open until
/// it's on).
pub async fn summary(db: &PgPool, user_id: &str) -> ApiResult<Value> {
    let (on, left, setup_needed, admin): (bool, i64, bool, bool) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM pyo.user_totp WHERE user_id = $1 AND enabled_at IS NOT NULL),
                (SELECT count(*) FROM pyo.recovery_codes WHERE user_id = $1 AND used_at IS NULL),
                pyo.needs_two_factor($1, $2),
                coalesce((SELECT is_admin FROM pyo.users WHERE id = $1), false)",
    )
    .bind(user_id)
    .bind(crate::hosted())
    .fetch_one(db)
    .await?;
    let required = (crate::hosted() && admin) || audit::settings(db).await?.require_two_factor;
    let locked: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object('id', t.id, 'name', t.name, 'slug', t.slug)
           FROM pyo.members m JOIN pyo.teams t ON t.id = m.team_id
          WHERE m.user_id = $1 AND m.status = 'active' AND NOT $2 AND ($3 OR t.require_two_factor)
          ORDER BY t.created_at",
    )
    .bind(user_id)
    .bind(on)
    .bind(setup_needed)
    .fetch_all(db)
    .await?;
    Ok(json!({
        "enabled": on,
        "recoveryCodesLeft": if on { left } else { 0 },
        "required": required,
        "setupNeeded": setup_needed,
        "lockedTeams": locked,
    }))
}

// ── Signing in ─────────────────────────────────────────────────────────────

/// A sign-in that needs its code next: its secret, for `/login/code`.
pub async fn start_challenge(db: &PgPool, user_id: &str, method: &str) -> ApiResult<String> {
    sqlx::query("DELETE FROM pyo.sign_in_challenges WHERE expires_at < now() OR user_id = $1")
        .bind(user_id)
        .execute(db)
        .await?;
    let token = secret_token();
    sqlx::query(
        "INSERT INTO pyo.sign_in_challenges (token_hash, user_id, method, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(mins => $4))",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(method)
    .bind(CHALLENGE_MINUTES)
    .execute(db)
    .await?;
    Ok(token)
}

/// What a sign-in answers once the password is right: the account, or
/// that a code comes next.
pub fn code_needed(challenge: &str) -> Value {
    json!({ "twoFactor": true, "challenge": challenge })
}

#[derive(Deserialize)]
struct CodeBody {
    challenge: String,
    code: String,
}

fn wrong_code() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::UNAUTHORIZED,
        "That code didn't work. Try the newest one.",
    )
}

async fn login_code(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    from: crate::admin_sign_in::Source,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<CodeBody>,
) -> ApiResult<(CookieJar, Json<User>)> {
    require_csrf(&headers)?;
    let hash = sha256_hex(body.challenge.trim());
    // A try used up whatever the answer.
    let row: Option<(String, String, i32)> = sqlx::query_as(
        "UPDATE pyo.sign_in_challenges SET attempts = attempts + 1
          WHERE token_hash = $1 AND expires_at > now() RETURNING user_id, method, attempts",
    )
    .bind(&hash)
    .fetch_optional(&state.db)
    .await?;
    let Some((user_id, method, attempts)) = row else {
        return Err(ApiError::bad_request("That took too long. Sign in again."));
    };
    if attempts > CHALLENGE_TRIES {
        sqlx::query("DELETE FROM pyo.sign_in_challenges WHERE token_hash = $1")
            .bind(&hash)
            .execute(&state.db)
            .await?;
        return Err(ApiError::bad_request(
            "Too many wrong codes. Sign in again.",
        ));
    }
    let user = sqlx::query_as::<_, User>(
        "SELECT id, name, email, timezone, is_admin, must_set_up FROM pyo.users WHERE id = $1",
    )
    .bind(&user_id)
    .fetch_one(&state.db)
    .await?;
    // An admin from where admins may not sign in: as a wrong code, before
    // the code (a recovery code stays unused).
    if !crate::admin_sign_in::sign_in_allowed(&state, &user, &from, "with a code").await? {
        return Err(wrong_code());
    }
    let Some(used) = verify(&state, &user_id, &ip, &body.code).await? else {
        audit::account_event(
            &state.db,
            &user_id,
            &Who::person(&user),
            Entry {
                action: "auth.sign_in_failed",
                title: "Wrong two-factor code",
                target: json!({}),
                failed: true,
            },
        )
        .await;
        return Err(wrong_code());
    };
    sqlx::query("DELETE FROM pyo.sign_in_challenges WHERE token_hash = $1")
        .bind(&hash)
        .execute(&state.db)
        .await?;
    let how = match (method.as_str(), used) {
        ("link", Used::App) => "Signed in with a sign-in link and a code",
        ("link", Used::Recovery) => "Signed in with a sign-in link and a recovery code",
        (_, Used::App) => "Signed in with a password and a code",
        (_, Used::Recovery) => "Signed in with a password and a recovery code",
    };
    audit::account_event(
        &state.db,
        &user_id,
        &Who::person(&user),
        Entry {
            action: if used == Used::Recovery {
                "auth.recovery_code_used"
            } else {
                "auth.sign_in"
            },
            title: how,
            target: json!({}),
            failed: false,
        },
    )
    .await;
    let jar = start_session(&state, jar, &user_id, &headers).await?;
    Ok((jar, Json(user)))
}

// ── Managing ───────────────────────────────────────────────────────────────

/// Two-factor is managed from a browser session, never with an API key.
fn browser_only(user: &User) -> ApiResult<()> {
    if user.key.is_some() {
        Err(ApiError::forbidden("Manage two-factor in the dashboard."))
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
struct SetupBody {
    password: String,
}

/// A new secret to scan, waiting for its first code. Takes the password:
/// a stolen session alone can't put its own app on the account.
async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Json(body): Json<SetupBody>,
) -> ApiResult<Json<Value>> {
    require_csrf(&headers)?;
    browser_only(&user)?;
    if enabled(&state.db, &user.id).await? {
        return Err(ApiError::conflict("Two-factor is already on."));
    }
    ratelimit::take(&state.db, &[(ratelimit::PASSKEY_PASSWORD_USER, &user.id)]).await?;
    if !password_matches(&state.db, &user.id, &body.password).await? {
        return Err(ApiError::forbidden("Wrong password."));
    }
    ratelimit::reset(&state.db, &ratelimit::PASSKEY_PASSWORD_USER, &user.id).await?;
    if !state.sealer.enabled() {
        return Err(ApiError::bad_request(
            "This Pyo has no secret key (PYO_SECRET_KEY) to keep the app's secret with.",
        ));
    }
    let mut raw = [0u8; 20];
    rand::fill(&mut raw);
    let secret = Secret::from(raw.to_vec()).to_base32();
    let sealed = state.sealer.seal(&secret)?;
    sqlx::query(
        "INSERT INTO pyo.user_totp (user_id, secret_sealed) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET secret_sealed = excluded.secret_sealed, created_at = now(), last_step = 0
          WHERE pyo.user_totp.enabled_at IS NULL",
    )
    .bind(&user.id)
    .bind(&sealed)
    .execute(&state.db)
    .await?;
    let uri = otpauth_uri(&secret, &user.email);
    Ok(Json(json!({
        "secret": grouped(&secret),
        "uri": uri,
        "qr": qr_svg(&uri)?,
    })))
}

/// What an authenticator app scans.
fn otpauth_uri(secret: &str, email: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'@' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    format!(
        "otpauth://totp/Pyo:{}?secret={secret}&issuer=Pyo&algorithm=SHA1&digits=6&period={STEP}",
        enc(email)
    )
}

/// The secret in groups of four, easier to type.
fn grouped(secret: &str) -> String {
    secret
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn qr_svg(uri: &str) -> ApiResult<String> {
    let code = qrcode::QrCode::new(uri.as_bytes()).map_err(|e| anyhow::anyhow!("QR code: {e}"))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .quiet_zone(true)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

#[derive(Deserialize)]
struct CodeOnly {
    code: String,
}

/// The first code from the app: two-factor is on, and here are the
/// recovery codes, shown this once.
async fn enable(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Json(body): Json<CodeOnly>,
) -> ApiResult<Json<Value>> {
    require_csrf(&headers)?;
    browser_only(&user)?;
    ratelimit::take(
        &state.db,
        &[
            (ratelimit::TWO_FACTOR_ACCOUNT, &user.id),
            (ratelimit::TWO_FACTOR_IP, &ip),
        ],
    )
    .await?;
    let mut tx = state.db.begin().await?;
    let row: Option<String> = sqlx::query_scalar(
        "SELECT secret_sealed FROM pyo.user_totp WHERE user_id = $1 AND enabled_at IS NULL FOR UPDATE",
    )
    .bind(&user.id)
    .fetch_optional(&mut *tx)
    .await?;
    let sealed = row.ok_or_else(|| ApiError::bad_request("Start the setup again."))?;
    let secret = open_secret(&state.sealer, &sealed)?;
    let Some(step) = check_code(&secret, &body.code, now(), 0) else {
        return Err(ApiError::bad_request(
            "That code didn't work. Try the newest one.",
        ));
    };
    sqlx::query("UPDATE pyo.user_totp SET enabled_at = now(), last_step = $2 WHERE user_id = $1")
        .bind(&user.id)
        .bind(step as i64)
        .execute(&mut *tx)
        .await?;
    let codes = replace_recovery_codes(&mut tx, &user.id).await?;
    audit::account_event_in(
        &mut tx,
        &user.id,
        &Who::person(&user),
        Entry {
            action: "auth.two_factor_enabled",
            title: "Turned on two-factor authentication",
            target: json!({}),
            failed: false,
        },
    )
    .await?;
    tx.commit().await?;
    ratelimit::reset(&state.db, &ratelimit::TWO_FACTOR_ACCOUNT, &user.id).await?;
    ratelimit::forgive(&state.db, &ratelimit::TWO_FACTOR_IP, &ip).await?;
    Ok(Json(json!({ "recoveryCodes": codes })))
}

/// Off, with a code (from the app or a recovery code), unless it's
/// required. The account's teams hear of it, and the account by email.
async fn disable(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Json(body): Json<CodeOnly>,
) -> ApiResult<Json<Value>> {
    require_csrf(&headers)?;
    browser_only(&user)?;
    if !enabled(&state.db, &user.id).await? {
        return Err(ApiError::conflict("Two-factor is already off."));
    }
    if let Some(why) = required_by(&state.db, &user.id).await? {
        return Err(ApiError::forbidden(format!(
            "{why} It can't be turned off."
        )));
    }
    if verify(&state, &user.id, &ip, &body.code).await?.is_none() {
        return Err(ApiError::bad_request(
            "That code didn't work. Try the newest one.",
        ));
    }
    let mut tx = state.db.begin().await?;
    turn_off(&mut tx, &user.id).await?;
    notify_teams(
        &mut tx,
        &user,
        "auth.two_factor_disabled",
        "Turned off two-factor authentication",
    )
    .await?;
    tx.commit().await?;
    mail_user(&state, &user.email, "/profile", crate::mail::two_factor_off).await;
    Ok(Json(json!({ "ok": true })))
}

/// Two-factor off for the account: its secret and recovery codes go.
pub async fn turn_off(conn: &mut PgConnection, user_id: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pyo.user_totp WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM pyo.recovery_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM pyo.sign_in_challenges WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// New recovery codes (the old ones stop working), with a code.
async fn new_recovery_codes(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    SessionUser(user): SessionUser,
    Json(body): Json<CodeOnly>,
) -> ApiResult<Json<Value>> {
    require_csrf(&headers)?;
    browser_only(&user)?;
    if verify(&state, &user.id, &ip, &body.code).await?.is_none() {
        return Err(ApiError::bad_request(
            "That code didn't work. Try the newest one.",
        ));
    }
    let mut tx = state.db.begin().await?;
    let codes = replace_recovery_codes(&mut tx, &user.id).await?;
    audit::account_event_in(
        &mut tx,
        &user.id,
        &Who::person(&user),
        Entry {
            action: "auth.recovery_codes_created",
            title: "Made new recovery codes",
            target: json!({}),
            failed: false,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "recoveryCodes": codes })))
}

/// An entry in the activity of each of the account's teams (the bell, the
/// team's alerts and the audit log).
pub async fn notify_teams(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: &User,
    kind: &str,
    title: &str,
) -> ApiResult<()> {
    notify_teams_in(tx, user, None, kind, title).await
}

/// [`notify_teams`] for some of them (None: all).
pub async fn notify_teams_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: &User,
    only: Option<&[String]>,
    kind: &str,
    title: &str,
) -> ApiResult<()> {
    let teams: Vec<String> = sqlx::query_scalar(
        "SELECT team_id FROM pyo.members WHERE user_id = $1 AND status = 'active' AND ($2::text[] IS NULL OR team_id = ANY ($2)) ORDER BY team_id",
    )
    .bind(&user.id)
    .bind(only)
    .fetch_all(&mut **tx)
    .await?;
    for team in teams {
        crate::ops::log_activity(
            tx,
            &team,
            &user.name,
            kind,
            "warning",
            title,
            json!({ "kind": "member", "id": user.id, "label": user.email }),
            json!({}),
        )
        .await?;
    }
    Ok(())
}

/// An email to the account, when this Pyo sends email. Never fails the
/// request: a failure is logged.
pub async fn mail_user(
    state: &AppState,
    to: &str,
    path: &str,
    template: impl FnOnce(&str) -> crate::mail::Email,
) {
    let Ok(Some(cfg)) = crate::mail::settings(&state.db).await else {
        return;
    };
    if let Err(e) = crate::mail::send(state, &cfg, to, &template(&cfg.link(path))).await {
        tracing::warn!(error = %format!("{e:#}"), "security email not sent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"12345678901234567890";

    /// RFC 6238's SHA-1 test vectors (the last 6 digits), and each code
    /// working once.
    #[test]
    fn codes_follow_the_rfc_and_work_once() {
        let t = totp(SECRET).unwrap();
        assert_eq!(t.generate(59).to_string(), "287082");
        assert_eq!(t.generate(1_111_111_109).to_string(), "081804");
        assert_eq!(t.generate(1_234_567_890).to_string(), "005924");
        let step = check_code(SECRET, "287 082", 59, 0).expect("good at its time");
        assert_eq!(step, 1);
        // The step before and after still count (clocks drift).
        assert!(check_code(SECRET, "287082", 59 + 30, 0).is_some());
        assert!(check_code(SECRET, "287082", 59 + 90, 0).is_none());
        // Used once: not again.
        assert!(check_code(SECRET, "287082", 59, step).is_none());
        assert!(check_code(SECRET, "28708", 59, 0).is_none());
        assert!(check_code(SECRET, "abcdef", 59, 0).is_none());
    }

    #[test]
    fn recovery_codes_read_easily() {
        let c = recovery_code();
        assert_eq!(c.len(), 11);
        assert_eq!(&c[5..6], "-");
        assert!(
            c.chars()
                .filter(|c| *c != '-')
                .all(|ch| RECOVERY_ALPHABET.contains(&(ch as u8)))
        );
        assert_ne!(recovery_code(), recovery_code());
        assert_eq!(recovery_hash("ABCDE-FGHJK"), recovery_hash(" abcde fghjk "));
        assert_eq!(recovery_hash("abc"), None);
    }

    #[test]
    fn the_app_gets_a_uri_and_a_qr_code() {
        let uri = otpauth_uri("JBSWY3DPEHPK3PXP", "ada+ops@example.com");
        assert_eq!(
            uri,
            "otpauth://totp/Pyo:ada%2Bops@example.com?secret=JBSWY3DPEHPK3PXP&issuer=Pyo&algorithm=SHA1&digits=6&period=30"
        );
        assert!(qr_svg(&uri).unwrap().starts_with("<?xml"));
        assert_eq!(grouped("JBSWY3DPEHPK3PXP"), "JBSW Y3DP EHPK 3PXP");
    }

    async fn db() -> PgPool {
        let db = PgPool::connect(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        db
    }

    /// Against a throwaway Postgres: a code from the app works once, a
    /// recovery code works once, and nothing works for an account that
    /// hasn't confirmed its app.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn codes_and_recovery_codes_work_once() {
        let db = db().await;
        let sealer = crate::secrets::Sealer::for_tests();
        let user = format!("usr_{}", crate::ids::random_string(8).to_lowercase());
        sqlx::query("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $1 || '@2fa.test', 'T', '')")
            .bind(&user)
            .execute(&db)
            .await
            .unwrap();
        let b32 = Secret::from(SECRET.to_vec()).to_base32();
        sqlx::query("INSERT INTO pyo.user_totp (user_id, secret_sealed) VALUES ($1, $2)")
            .bind(&user)
            .bind(sealer.seal(&b32).unwrap())
            .execute(&db)
            .await
            .unwrap();
        // Not confirmed yet: no code counts, and it isn't on.
        assert_eq!(
            verify_once(&db, &sealer, &user, "287082", 59)
                .await
                .unwrap(),
            None
        );
        assert!(!enabled(&db, &user).await.unwrap());
        sqlx::query("UPDATE pyo.user_totp SET enabled_at = now() WHERE user_id = $1")
            .bind(&user)
            .execute(&db)
            .await
            .unwrap();
        assert!(enabled(&db, &user).await.unwrap());
        assert_eq!(
            verify_once(&db, &sealer, &user, "287082", 59)
                .await
                .unwrap(),
            Some(Used::App)
        );
        assert_eq!(
            verify_once(&db, &sealer, &user, "287082", 59)
                .await
                .unwrap(),
            None,
            "the same code again"
        );
        let mut conn = db.acquire().await.unwrap();
        let codes = replace_recovery_codes(&mut conn, &user).await.unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT code_hash FROM pyo.recovery_codes WHERE user_id = $1")
                .bind(&user)
                .fetch_all(&db)
                .await
                .unwrap();
        assert!(
            !stored.iter().any(|h| codes.contains(h)),
            "only hashes are kept"
        );
        let typed = codes[3].to_uppercase();
        assert_eq!(
            verify_once(&db, &sealer, &user, &typed, 10_000)
                .await
                .unwrap(),
            Some(Used::Recovery)
        );
        assert_eq!(
            verify_once(&db, &sealer, &user, &codes[3], 10_000)
                .await
                .unwrap(),
            None,
            "a recovery code works once"
        );
        let left: Value = summary(&db, &user).await.unwrap();
        assert_eq!(left["enabled"], true);
        assert_eq!(left["recoveryCodesLeft"], 9);
        // New codes: the old ones stop working.
        let fresh = replace_recovery_codes(&mut conn, &user).await.unwrap();
        assert_eq!(
            verify_once(&db, &sealer, &user, &codes[4], 10_000)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            verify_once(&db, &sealer, &user, &fresh[0], 10_000)
                .await
                .unwrap(),
            Some(Used::Recovery)
        );
        turn_off(&mut conn, &user).await.unwrap();
        assert!(!enabled(&db, &user).await.unwrap());
        assert_eq!(
            verify_once(&db, &sealer, &user, &fresh[1], 10_000)
                .await
                .unwrap(),
            None
        );
        sqlx::query("DELETE FROM pyo.users WHERE id = $1")
            .bind(&user)
            .execute(&db)
            .await
            .unwrap();
    }
}
