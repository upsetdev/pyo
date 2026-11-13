//! Email + password accounts and browser sessions.
//!
//! The session cookie holds a random token; the database keeps only its
//! sha256. Every browser request that changes something must also carry
//! `x-pyo-csrf: 1`, which a cross-site form or image can't add.

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{
    Json, Router,
    extract::{FromRequestParts, State},
    http::{HeaderMap, StatusCode, request::Parts},
    routing::{get, post},
};
use axum_extra::extract::{
    CookieJar,
    cookie::{Cookie, SameSite},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    AppState,
    error::{ApiError, ApiResult},
    ids::{new_id, secret_token, sha256_hex},
    ratelimit::{self, ClientIp},
    util::{require_text, slugify},
};

pub const SESSION_COOKIE: &str = "pyo_session";
/// A session ends 30 days after sign-in, whatever happens.
const SESSION_DAYS: i64 = 30;
/// And after 7 days without a request; each day of use moves that on.
const SESSION_IDLE_DAYS: i64 = 7;
/// The sign-in the installer prints. Replaced on first sign-in.
const FIRST_ADMIN_EMAIL: &str = "admin@pyo.local";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/signup", post(signup))
        .route("/login", post(login))
        .route("/setup", post(setup))
        .route("/logout", post(logout))
        .route("/me", get(me))
        .route("/config", get(config))
        .route("/invite/accept", post(accept))
        .merge(crate::passkeys::routes())
}

/// A Pyo account.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub name: String,
    pub email: String,
    pub timezone: String,
    /// Administers this installation (outgoing email and the like).
    pub is_admin: bool,
    /// Still on the installer's printed sign-in: nothing else works until
    /// they've chosen their own email and password.
    pub must_set_up: bool,
    /// Set when acting through an API key: what the key may do.
    #[sqlx(skip)]
    #[serde(skip)]
    pub key: Option<KeyScope>,
}

/// An API key's limits on top of its owner's permissions.
#[derive(Debug, Clone)]
pub struct KeyScope {
    pub access: KeyAccess,
    /// None: all of the owner's teams.
    pub teams: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyAccess {
    /// Look, never change.
    Read,
    /// Also deploy, roll back, restart, stop and scale services.
    Deploy,
    /// Everything the owner can do in those teams.
    Full,
}

impl KeyAccess {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(Self::Read),
            "deploy" => Some(Self::Deploy),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

/// The signed-in user. Rejects with 401 when there's no valid session, and
/// with 428 while the account is still on the installer's printed sign-in.
pub struct SessionUser(pub User);

impl FromRequestParts<AppState> for SessionUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let AnySession(user) = AnySession::from_request_parts(parts, state).await?;
        if user.must_set_up {
            return Err(ApiError::new(
                StatusCode::PRECONDITION_REQUIRED,
                "Choose your own email and password first.",
            ));
        }
        Ok(SessionUser(user))
    }
}

/// Any valid session, set up or not: only for what the first sign-in needs.
struct AnySession(User);

impl FromRequestParts<AppState> for AnySession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Scripts: an API key instead of a browser session (no CSRF header
        // needed: browsers never send it on their own).
        if parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .is_some()
        {
            let TokenUser(user) = TokenUser::from_request_parts(parts, state).await?;
            return Ok(AnySession(user));
        }
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(SESSION_COOKIE)
            .map(|c| c.value().to_owned())
            .ok_or_else(ApiError::unauthorized)?;
        let user = session_user(&state.db, &token)
            .await?
            .ok_or_else(ApiError::unauthorized)?;
        if parts.method != axum::http::Method::GET
            && parts.headers.get("x-pyo-csrf").map(|v| v.as_bytes()) != Some(b"1")
        {
            return Err(ApiError::forbidden("Missing request header."));
        }
        Ok(AnySession(user))
    }
}

/// The user of a session cookie's token, while the session lasts. In use,
/// its idle limit moves on (at most once an hour, never past the absolute
/// one).
async fn session_user(db: &sqlx::PgPool, token: &str) -> ApiResult<Option<User>> {
    let hash = sha256_hex(token);
    let user = sqlx::query_as::<_, User>(
        "SELECT u.id, u.name, u.email, u.timezone, u.is_admin, u.must_set_up
           FROM pyo.sessions s JOIN pyo.users u ON u.id = s.user_id
          WHERE s.token_hash = $1 AND s.expires_at > now() AND s.created_at > now() - make_interval(days => $2)",
    )
    .bind(&hash)
    .bind(SESSION_DAYS as i32)
    .fetch_optional(db)
    .await?;
    if user.is_some() {
        let _ = sqlx::query(
            "UPDATE pyo.sessions SET expires_at = least(created_at + make_interval(days => $2), now() + make_interval(days => $3))
              WHERE token_hash = $1 AND expires_at < now() + make_interval(days => $3) - interval '1 hour'",
        )
        .bind(&hash)
        .bind(SESSION_DAYS as i32)
        .bind(SESSION_IDLE_DAYS as i32)
        .execute(db)
        .await;
    }
    Ok(user)
}

/// An API key's user and scope: id, name, email, timezone, is_admin,
/// must_set_up, access, team_ids, expired.
type KeyRow = (
    String,
    String,
    String,
    String,
    bool,
    bool,
    String,
    Option<Vec<String>>,
    bool,
);

/// A user acting through an API key (`Authorization: Bearer pyo_…`, made on
/// the API keys page): scripts calling the REST API. Same permissions as the
/// user. MCP clients use OAuth instead (oauth.rs).
pub struct TokenUser(pub User);

impl FromRequestParts<AppState> for TokenUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| t.starts_with(API_KEY_PREFIX))
            .ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Send a Pyo API key: Authorization: Bearer pyo_…",
                )
            })?;
        Ok(TokenUser(key_user(&state.db, token).await?))
    }
}

/// The user behind an API key; expired keys are refused. Records when the
/// key was last used, at most once a minute so busy clients don't write on
/// every call.
pub(crate) async fn key_user(db: &sqlx::PgPool, token: &str) -> ApiResult<User> {
    let hash = sha256_hex(token);
    let row: Option<KeyRow> = sqlx::query_as(
        "SELECT u.id, u.name, u.email, u.timezone, u.is_admin, u.must_set_up, t.access, t.team_ids,
                COALESCE(t.expires_at <= now(), false)
           FROM pyo.api_tokens t JOIN pyo.users u ON u.id = t.user_id
          WHERE t.token_hash = $1",
    )
    .bind(&hash)
    .fetch_optional(db)
    .await?;
    let (id, name, email, timezone, is_admin, must_set_up, access, teams, expired) = row
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "That API key isn't valid (revoked, or mistyped).",
            )
        })?;
    if expired {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, KEY_EXPIRED));
    }
    let key = KeyScope {
        access: KeyAccess::parse(&access).unwrap_or(KeyAccess::Read),
        teams,
    };
    let _ = sqlx::query("UPDATE pyo.api_tokens SET last_used_at = now() WHERE token_hash = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')")
        .bind(&hash)
        .execute(db)
        .await;
    Ok(User {
        id,
        name,
        email,
        timezone,
        is_admin,
        must_set_up,
        key: Some(key),
    })
}

/// Prefix of every API key (and CLI token) Pyo hands out.
pub const API_KEY_PREFIX: &str = "pyo_";

/// The 401 for an API key past its `expires_at`.
pub const KEY_EXPIRED: &str = "This API key expired. Create a new one.";

/// The lifetimes an API key can have: 30 days, 90 days or a year; never
/// expires when omitted. Postgres days, for `make_interval`.
pub(crate) fn key_lifetime(days: Option<u32>) -> ApiResult<Option<i32>> {
    match days {
        None => Ok(None),
        Some(d @ (30 | 90 | 365)) => Ok(Some(d as i32)),
        Some(_) => Err(ApiError::bad_request(
            "An API key expires in 30, 90 or 365 days, or never.",
        )),
    }
}

/// Keys `pyo login` makes last a year by default; logging in again makes a
/// new one.
pub(crate) const CLI_KEY_DAYS: i32 = 365;

pub(crate) fn require_csrf(headers: &HeaderMap) -> ApiResult<()> {
    if headers.get("x-pyo-csrf").map(|v| v.as_bytes()) == Some(b"1") {
        Ok(())
    } else {
        Err(ApiError::forbidden("Missing request header."))
    }
}

fn session_cookie(state: &AppState, token: String) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure)
        .max_age(time_days(SESSION_DAYS))
        .build()
}

fn time_days(days: i64) -> cookie::time::Duration {
    cookie::time::Duration::days(days)
}

pub(crate) async fn start_session(
    state: &AppState,
    jar: CookieJar,
    user_id: &str,
    headers: &HeaderMap,
) -> ApiResult<CookieJar> {
    let agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(300)
        .collect::<String>();
    let token = new_session(&state.db, user_id, &agent).await?;
    Ok(jar.add(session_cookie(state, token)))
}

/// A new session's token.
async fn new_session(db: &sqlx::PgPool, user_id: &str, agent: &str) -> ApiResult<String> {
    let token = secret_token();
    sqlx::query(
        "INSERT INTO pyo.sessions (token_hash, user_id, expires_at, user_agent)
         VALUES ($1, $2, now() + make_interval(days => $3), $4)",
    )
    .bind(sha256_hex(&token))
    .bind(user_id)
    .bind(SESSION_IDLE_DAYS as i32)
    .bind(agent)
    .execute(db)
    .await?;
    Ok(token)
}

#[derive(Deserialize)]
struct SignupBody {
    name: String,
    email: String,
    password: String,
    /// An invite link's secret.
    #[serde(default)]
    invite: Option<String>,
}

fn check_password(password: &str) -> ApiResult<()> {
    if password.chars().count() < 10 {
        return Err(ApiError::bad_request(
            "Use at least 10 characters for the password.",
        ));
    }
    if password.len() > 512 {
        return Err(ApiError::bad_request("That password is too long."));
    }
    Ok(())
}

/// Each Argon2 hash takes 19 MiB while it runs: a burst of sign-ins would
/// take that much per request at once. Two at a time keep the peak small.
static ARGON2_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Hash or verify a password on a blocking thread, two at a time.
async fn argon2_task<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    // Never closed, so this always gets a slot.
    let slot = ARGON2_SLOTS.acquire().await.ok();
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        f()
    })
    .await
}

async fn hash_password(password: &str) -> anyhow::Result<String> {
    let password = password.to_owned();
    argon2_task(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
    })
    .await?
    .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))
}

fn team_name(user_name: &str) -> String {
    format!(
        "{}'s team",
        user_name.split_whitespace().next().unwrap_or(user_name)
    )
}

/// The team someone starts with when nobody invited them anywhere.
async fn create_own_team(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: &User,
) -> ApiResult<()> {
    let team_id = new_id("team");
    let base = slugify(&user.name);
    let slug = crate::ops::unique_team_slug(tx, if base.is_empty() { "team" } else { &base }, None)
        .await?;
    let name = team_name(&user.name);
    sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $2, $3)")
        .bind(&team_id)
        .bind(&name)
        .bind(&slug)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')")
        .bind(new_id("mem"))
        .bind(&team_id)
        .bind(&user.id)
        .bind(&user.email)
        .execute(&mut **tx)
        .await?;
    crate::ops::log_activity(
        tx,
        &team_id,
        &user.name,
        "team.created",
        "info",
        &format!("Created team {name}"),
        json!({ "kind": "team", "id": team_id, "label": name }),
        json!({}),
    )
    .await?;
    Ok(())
}

/// A new installation has an admin before it takes its first request, so
/// there's never a first sign-up to race for. The installer sets
/// PYO_ADMIN_PASSWORD and prints it; run without one, the password goes to
/// this process's log, once. Only the web role may call this: it's the one
/// that gets the variable.
pub async fn ensure_first_admin(db: &sqlx::PgPool) -> anyhow::Result<()> {
    let mut tx = db.begin().await?;
    // Replicas start together.
    sqlx::query("SELECT pg_advisory_xact_lock(4242)")
        .execute(&mut *tx)
        .await?;
    let has_users: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.users)")
        .fetch_one(&mut *tx)
        .await?;
    if has_users {
        return Ok(());
    }
    let from_env = std::env::var("PYO_ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());
    let password = from_env.clone().unwrap_or_else(secret_token);
    let hash = hash_password(&password).await?;
    let user = User {
        id: new_id("usr"),
        name: "Admin".into(),
        email: FIRST_ADMIN_EMAIL.into(),
        timezone: "UTC".into(),
        is_admin: true,
        must_set_up: true,
        key: None,
    };
    sqlx::query("INSERT INTO pyo.users (id, email, name, password_hash, is_admin, must_set_up) VALUES ($1, $2, $3, $4, true, true)")
        .bind(&user.id)
        .bind(&user.email)
        .bind(&user.name)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    create_own_team(&mut tx, &user)
        .await
        .map_err(|e| anyhow::anyhow!("creating the first team failed: {e:?}"))?;
    // A fresh install: observability starts off on a small server.
    let observability = crate::observability::first_default(&mut tx).await?;
    tx.commit().await?;
    tracing::info!(
        on = observability,
        "observability's default for this install"
    );
    if from_env.is_some() {
        tracing::info!(email = FIRST_ADMIN_EMAIL, "created the first admin");
    } else {
        tracing::warn!(email = FIRST_ADMIN_EMAIL, password = %password, "created the first admin. Sign in, then choose your own email and password");
    }
    Ok(())
}

fn normalize_email(email: &str) -> ApiResult<String> {
    let e = email.trim().to_lowercase();
    let valid = e.len() <= 254
        && e.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if valid {
        Ok(e)
    } else {
        Err(ApiError::bad_request("Enter a valid email address."))
    }
}

/// How long an invite link works. Sending it again makes a new one.
const INVITE_DAYS: i32 = 14;

/// Where an invite link points.
pub(crate) fn invite_path(token: &str) -> String {
    format!("/signup?invite={token}")
}

/// A new secret for an invite, replacing any earlier one: the invite is
/// accepted with it (signing up with the address isn't enough).
pub(crate) async fn new_invite(tx: &mut sqlx::PgConnection, member_id: &str) -> ApiResult<String> {
    let token = secret_token();
    sqlx::query("UPDATE pyo.members SET invite_token_hash = $2, invite_expires_at = now() + make_interval(days => $3) WHERE id = $1 AND status = 'invited'")
        .bind(member_id)
        .bind(sha256_hex(&token))
        .bind(INVITE_DAYS)
        .execute(&mut *tx)
        .await?;
    Ok(token)
}

/// A waiting invite, found by its link's secret.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct Invite {
    pub member_id: String,
    pub email: String,
    pub team: String,
}

pub(crate) async fn find_invite(
    tx: &mut sqlx::PgConnection,
    token: &str,
) -> ApiResult<Option<Invite>> {
    if token.is_empty() {
        return Ok(None);
    }
    Ok(sqlx::query_as(
        "SELECT m.id AS member_id, m.email, t.name AS team FROM pyo.members m JOIN pyo.teams t ON t.id = m.team_id
          WHERE m.invite_token_hash = $1 AND m.status = 'invited' AND m.invite_expires_at > now()",
    )
    .bind(sha256_hex(token))
    .fetch_optional(&mut *tx)
    .await?)
}

/// The invite becomes `user_id`'s membership; its link stops working.
pub(crate) async fn accept_invite(
    tx: &mut sqlx::PgConnection,
    invite: &Invite,
    user_id: &str,
) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.members SET user_id = $2, status = 'active', joined_at = now(), invite_token_hash = NULL, invite_expires_at = NULL WHERE id = $1 AND status = 'invited'")
        .bind(&invite.member_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

fn wrong_invite() -> ApiError {
    ApiError::bad_request("This invite link has expired or was replaced. Ask for a new one.")
}

#[derive(Deserialize)]
struct ConfigQuery {
    /// An invite link's secret: its invite may sign up even when sign-up is
    /// closed.
    invite: Option<String>,
}

async fn config(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ConfigQuery>,
) -> ApiResult<Json<Value>> {
    let mut conn = state.db.acquire().await?;
    let invite = match q.invite.as_deref() {
        Some(t) => find_invite(&mut conn, t).await?,
        None => None,
    };
    Ok(Json(json!({
        "signupOpen": state.config.allow_signup || invite.is_some(),
        "invite": invite.map(|i| json!({ "email": i.email, "team": i.team })),
    })))
}

async fn signup(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<SignupBody>,
) -> ApiResult<(CookieJar, Json<User>)> {
    require_csrf(&headers)?;
    let name = require_text(&body.name, "Name", 80)?;
    let email = normalize_email(&body.email)?;
    check_password(&body.password)?;
    ratelimit::take(&state.db, &[(ratelimit::SIGNUP_IP, &ip)]).await?;

    let mut tx = state.db.begin().await?;
    let invite = match body.invite.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => Some(find_invite(&mut tx, t).await?.ok_or_else(wrong_invite)?),
        None => None,
    };
    if let Some(i) = &invite
        && i.email != email
    {
        return Err(ApiError::bad_request(format!(
            "This invite is for {}.",
            i.email
        )));
    }
    if !state.config.allow_signup && invite.is_none() {
        return Err(ApiError::forbidden(
            "Sign-up is closed. Ask a team owner to invite you.",
        ));
    }
    let taken: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.users WHERE email = $1)")
            .bind(&email)
            .fetch_one(&mut *tx)
            .await?;
    if taken {
        return Err(ApiError::conflict(
            "An account with this email already exists. Sign in instead.",
        ));
    }

    let hash = hash_password(&body.password).await?;

    let user = User {
        id: new_id("usr"),
        name: name.clone(),
        email: email.clone(),
        timezone: "UTC".into(),
        is_admin: false,
        must_set_up: false,
        key: None,
    };
    sqlx::query("INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, $3, $4)")
        .bind(&user.id)
        .bind(&email)
        .bind(&name)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;

    // The invite they followed becomes their membership. Other invites to
    // this address wait for their own links.
    match &invite {
        Some(i) => accept_invite(&mut tx, i, &user.id).await?,
        // Nobody invited them anywhere: they start with a team of their own.
        None => create_own_team(&mut tx, &user).await?,
    }
    tx.commit().await?;

    let jar = start_session(&state, jar, &user.id, &headers).await?;
    Ok((jar, Json(user)))
}

#[derive(Deserialize)]
struct AcceptBody {
    invite: String,
}

/// A signed-in account accepts an invite to its own address by its link.
async fn accept(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    headers: HeaderMap,
    Json(body): Json<AcceptBody>,
) -> ApiResult<Json<Value>> {
    require_csrf(&headers)?;
    if user.key.is_some() {
        return Err(ApiError::forbidden("Accept invites in the dashboard."));
    }
    let mut tx = state.db.begin().await?;
    let invite = find_invite(&mut tx, body.invite.trim())
        .await?
        .ok_or_else(wrong_invite)?;
    if invite.email != user.email {
        return Err(ApiError::forbidden(format!(
            "This invite is for {}. Sign in with that address to accept it.",
            invite.email
        )));
    }
    accept_invite(&mut tx, &invite, &user.id).await?;
    let (team_id, slug): (String, String) = sqlx::query_as("SELECT t.id, t.slug FROM pyo.members m JOIN pyo.teams t ON t.id = m.team_id WHERE m.id = $1").bind(&invite.member_id).fetch_one(&mut *tx).await?;
    crate::ops::log_activity(
        &mut tx,
        &team_id,
        &user.name,
        "member.joined",
        "info",
        &format!("{} joined", user.email),
        json!({ "kind": "team", "id": team_id, "label": user.email }),
        json!({}),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "ok": true, "team": slug })))
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<LoginBody>,
) -> ApiResult<(CookieJar, Json<User>)> {
    require_csrf(&headers)?;
    let email = body.email.trim().to_lowercase();
    // Before the lookup, keyed by the email as typed: the same for accounts
    // that exist and ones that don't. Only failures keep their slot.
    ratelimit::take(
        &state.db,
        &[
            (ratelimit::LOGIN_ACCOUNT, &email),
            (ratelimit::LOGIN_IP, &ip),
        ],
    )
    .await?;
    let row: Option<(String, String, String, String, bool, bool)> = sqlx::query_as("SELECT id, name, timezone, password_hash, is_admin, must_set_up FROM pyo.users WHERE email = $1")
        .bind(&email)
        .fetch_optional(&state.db)
        .await?;
    // Same work and same answer whether or not the account exists.
    let (id, name, timezone, hash, is_admin, must_set_up) = row.unwrap_or_else(|| {
        (String::new(), String::new(), String::new(), "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$ZmFrZWZha2VmYWtlZmFrZWZha2VmYWtlZmFrZWZha2U".into(), false, false)
    });
    let password = body.password.clone();
    let ok = argon2_task(move || {
        PasswordHash::new(&hash)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .map_err(anyhow::Error::from)?;
    if !ok || id.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "Wrong email or password.",
        ));
    }
    ratelimit::reset(&state.db, &ratelimit::LOGIN_ACCOUNT, &email).await?;
    ratelimit::forgive(&state.db, &ratelimit::LOGIN_IP, &ip).await?;
    let jar = start_session(&state, jar, &id, &headers).await?;
    Ok((
        jar,
        Json(User {
            id,
            name,
            email,
            timezone,
            is_admin,
            must_set_up,
            key: None,
        }),
    ))
}

/// Is this the account's password? The same check, and the same work, as
/// signing in.
pub(crate) async fn password_matches(
    db: &sqlx::PgPool,
    user_id: &str,
    password: &str,
) -> ApiResult<bool> {
    let hash: Option<String> =
        sqlx::query_scalar("SELECT password_hash FROM pyo.users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(db)
            .await?;
    let hash = hash.unwrap_or_else(|| "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$ZmFrZWZha2VmYWtlZmFrZWZha2VmYWtlZmFrZWZha2U".into());
    let password = password.to_owned();
    Ok(argon2_task(move || {
        PasswordHash::new(&hash)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .map_err(anyhow::Error::from)?)
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
) -> ApiResult<(CookieJar, StatusCode)> {
    require_csrf(&headers)?;
    if let Some(c) = jar.get(SESSION_COOKIE) {
        sqlx::query("DELETE FROM pyo.sessions WHERE token_hash = $1")
            .bind(sha256_hex(c.value()))
            .execute(&state.db)
            .await?;
    }
    Ok((
        jar.remove(Cookie::build(SESSION_COOKIE).path("/").build()),
        StatusCode::NO_CONTENT,
    ))
}

async fn me(AnySession(user): AnySession) -> Json<User> {
    Json(user)
}

#[derive(Deserialize)]
struct SetupBody {
    name: String,
    email: String,
    password: String,
}

/// The first sign-in: the printed login becomes the admin's own.
async fn setup(
    State(state): State<AppState>,
    headers: HeaderMap,
    jar: CookieJar,
    AnySession(user): AnySession,
    Json(body): Json<SetupBody>,
) -> ApiResult<(CookieJar, Json<User>)> {
    require_csrf(&headers)?;
    if !user.must_set_up || user.key.is_some() {
        return Err(ApiError::conflict("This account is already set up."));
    }
    let name = require_text(&body.name, "Name", 80)?;
    let email = normalize_email(&body.email)?;
    if email == FIRST_ADMIN_EMAIL {
        return Err(ApiError::bad_request("Enter your own email address."));
    }
    check_password(&body.password)?;

    let old: String = sqlx::query_scalar("SELECT password_hash FROM pyo.users WHERE id = $1")
        .bind(&user.id)
        .fetch_one(&state.db)
        .await?;
    let password = body.password.clone();
    let same = argon2_task(move || {
        PasswordHash::new(&old)
            .map(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .await
    .map_err(anyhow::Error::from)?;
    if same {
        return Err(ApiError::bad_request(
            "Choose a new password, not the one the installer printed.",
        ));
    }
    let hash = hash_password(&body.password).await?;

    let mut tx = state.db.begin().await?;
    let taken: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.users WHERE email = $1)")
            .bind(&email)
            .fetch_one(&mut *tx)
            .await?;
    if taken {
        return Err(ApiError::conflict(
            "An account with this email already exists.",
        ));
    }
    let done = sqlx::query("UPDATE pyo.users SET name = $2, email = $3, password_hash = $4, must_set_up = false WHERE id = $1 AND must_set_up")
        .bind(&user.id)
        .bind(&name)
        .bind(&email)
        .bind(&hash)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if done == 0 {
        return Err(ApiError::conflict("This account is already set up."));
    }
    sqlx::query("UPDATE pyo.members SET email = $2 WHERE user_id = $1")
        .bind(&user.id)
        .bind(&email)
        .execute(&mut *tx)
        .await?;
    // The team was named after "Admin": it takes its owner's name, in the URL too.
    let team_id: String = sqlx::query_scalar("SELECT team_id FROM pyo.members WHERE user_id = $1")
        .bind(&user.id)
        .fetch_one(&mut *tx)
        .await?;
    let slug = crate::ops::unique_team_slug(&mut tx, &slugify(&name), Some(&team_id)).await?;
    sqlx::query("UPDATE pyo.teams SET name = $2, slug = $3 WHERE id = $1")
        .bind(&team_id)
        .bind(team_name(&name))
        .bind(&slug)
        .execute(&mut *tx)
        .await?;
    // Whoever else used the printed login is out, this browser too: it gets
    // a new session below.
    sqlx::query("DELETE FROM pyo.sessions WHERE user_id = $1")
        .bind(&user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let jar = start_session(&state, jar, &user.id, &headers).await?;
    Ok((
        jar,
        Json(User {
            name,
            email,
            must_set_up: false,
            ..user
        }),
    ))
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn key_lifetimes() {
        assert_eq!(key_lifetime(None).unwrap(), None);
        assert_eq!(key_lifetime(Some(30)).unwrap(), Some(30));
        assert_eq!(key_lifetime(Some(90)).unwrap(), Some(90));
        assert_eq!(key_lifetime(Some(365)).unwrap(), Some(365));
        assert!(key_lifetime(Some(0)).is_err());
        assert!(key_lifetime(Some(7)).is_err());
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): expired keys
    /// are refused, never-expiring ones work, and last use is written at most
    /// once a minute.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn key_expiry_and_last_use() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = new_id("t");
        let user = format!("usr_{run}");
        sqlx::query("INSERT INTO pyo.users (id, name, email, password_hash) VALUES ($1, $1, $1 || '@example.com', '')").bind(&user).execute(&pool).await.unwrap();
        let add = |id: &'static str, expires_in_secs: Option<f64>| {
            let (pool, user, run) = (pool.clone(), user.clone(), run.clone());
            async move {
                let token = format!("{API_KEY_PREFIX}{id}_{run}");
                sqlx::query("INSERT INTO pyo.api_tokens (id, user_id, name, token_hash, prefix, expires_at) VALUES ($1, $2, $1, $3, 'pyo_', now() + make_interval(secs => $4))")
                    .bind(format!("tok_{id}_{run}"))
                    .bind(&user)
                    .bind(sha256_hex(&token))
                    .bind(expires_in_secs)
                    .execute(&pool)
                    .await
                    .unwrap();
                token
            }
        };
        let expired = add("old", Some(-1.0)).await;
        let never = add("never", None).await;
        let later = add("later", Some(90.0 * 86400.0)).await;

        let err = key_user(&pool, &expired).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert_eq!(err.message, KEY_EXPIRED);
        assert_eq!(key_user(&pool, &never).await.unwrap().id, user);
        assert_eq!(key_user(&pool, &later).await.unwrap().id, user);

        let last = |token: String| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Option<i64>>(
                    "SELECT pyo.ms(last_used_at) FROM pyo.api_tokens WHERE token_hash = $1",
                )
                .bind(sha256_hex(&token))
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(
            last(expired.clone()).await,
            None,
            "an expired key isn't marked used"
        );
        let first = last(never.clone()).await.unwrap();
        key_user(&pool, &never).await.unwrap();
        assert_eq!(
            last(never.clone()).await.unwrap(),
            first,
            "within a minute: not written again"
        );
        sqlx::query("UPDATE pyo.api_tokens SET last_used_at = now() - interval '2 minutes' WHERE token_hash = $1").bind(sha256_hex(&never)).execute(&pool).await.unwrap();
        let backdated = last(never.clone()).await.unwrap();
        key_user(&pool, &never).await.unwrap();
        assert!(
            last(never.clone()).await.unwrap() > backdated,
            "after a minute: written"
        );

        sqlx::query("DELETE FROM pyo.users WHERE id = $1")
            .bind(&user)
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod invite_tests {
    use super::*;

    #[test]
    fn invite_links_carry_the_secret() {
        assert_eq!(invite_path("abc"), "/signup?invite=abc");
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): an invite is
    /// found only by its current link, until it's accepted or expires.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn invites_are_accepted_by_their_link() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = new_id("t");
        let (team, member, user) = (
            format!("team_{run}"),
            format!("mem_{run}"),
            format!("usr_{run}"),
        );
        let email = format!("{run}@invite.test");
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, 'Acme', $1)")
            .bind(&team)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, email, role, status) VALUES ($1, $2, $3, 'admin', 'invited')").bind(&member).bind(&team).bind(&email).execute(&mut *tx).await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'I', '')",
        )
        .bind(&user)
        .bind(&email)
        .execute(&mut *tx)
        .await
        .unwrap();

        // No link yet: the address alone finds nothing.
        assert!(find_invite(&mut tx, "").await.unwrap().is_none());
        let first = new_invite(&mut tx, &member).await.unwrap();
        let second = new_invite(&mut tx, &member).await.unwrap();
        assert!(
            find_invite(&mut tx, &first).await.unwrap().is_none(),
            "replaced"
        );
        assert!(find_invite(&mut tx, "nope").await.unwrap().is_none());
        let invite = find_invite(&mut tx, &second).await.unwrap().unwrap();
        assert_eq!(
            (invite.email.as_str(), invite.team.as_str()),
            (email.as_str(), "Acme")
        );

        // Expired: gone.
        sqlx::query(
            "UPDATE pyo.members SET invite_expires_at = now() - interval '1 second' WHERE id = $1",
        )
        .bind(&member)
        .execute(&mut *tx)
        .await
        .unwrap();
        assert!(find_invite(&mut tx, &second).await.unwrap().is_none());
        let third = new_invite(&mut tx, &member).await.unwrap();
        let invite = find_invite(&mut tx, &third).await.unwrap().unwrap();

        // Accepted: a membership, and the link stops working.
        accept_invite(&mut tx, &invite, &user).await.unwrap();
        let (status, uid, hash): (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, user_id, invite_token_hash FROM pyo.members WHERE id = $1",
        )
        .bind(&member)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            (status.as_str(), uid.as_deref(), hash),
            ("active", Some(user.as_str()), None)
        );
        assert!(find_invite(&mut tx, &third).await.unwrap().is_none());
        // A new link for someone who joined: nothing to find.
        let late = new_invite(&mut tx, &member).await.unwrap();
        assert!(find_invite(&mut tx, &late).await.unwrap().is_none());
        tx.rollback().await.unwrap();
    }

    /// Sessions end after a week unused, and 30 days after sign-in.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn sessions_slide_up_to_their_absolute_end() {
        let pool = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let run = new_id("t");
        let user = format!("usr_{run}");
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'S', '')",
        )
        .bind(&user)
        .bind(format!("{run}@s.test"))
        .execute(&pool)
        .await
        .unwrap();
        let token = new_session(&pool, &user, "test").await.unwrap();
        let ask = |token: String| {
            let pool = pool.clone();
            async move {
                session_user(&pool, &token)
                    .await
                    .unwrap()
                    .map(|u| u.id)
                    .ok_or(())
            }
        };
        let left = |pool: sqlx::PgPool, token: String| async move {
            let days: f64 = sqlx::query_scalar("SELECT extract(epoch FROM expires_at - now())::float8 / 86400 FROM pyo.sessions WHERE token_hash = $1").bind(sha256_hex(&token)).fetch_one(&pool).await.unwrap();
            days
        };
        assert!((6.9..7.1).contains(&left(pool.clone(), token.clone()).await));
        assert_eq!(ask(token.clone()).await.unwrap(), user);

        // Used after 3 idle days: a week again.
        sqlx::query("UPDATE pyo.sessions SET expires_at = now() + interval '4 days', created_at = now() - interval '3 days' WHERE token_hash = $1").bind(sha256_hex(&token)).execute(&pool).await.unwrap();
        ask(token.clone()).await.unwrap();
        assert!((6.9..7.1).contains(&left(pool.clone(), token.clone()).await));
        // Near the absolute end: never past it.
        sqlx::query("UPDATE pyo.sessions SET expires_at = now() + interval '1 day', created_at = now() - interval '28 days' WHERE token_hash = $1").bind(sha256_hex(&token)).execute(&pool).await.unwrap();
        ask(token.clone()).await.unwrap();
        assert!((1.9..2.1).contains(&left(pool.clone(), token.clone()).await));
        // Past it, or idle too long: signed out.
        sqlx::query("UPDATE pyo.sessions SET expires_at = now() + interval '1 day', created_at = now() - interval '31 days' WHERE token_hash = $1").bind(sha256_hex(&token)).execute(&pool).await.unwrap();
        assert!(ask(token.clone()).await.is_err());
        sqlx::query("UPDATE pyo.sessions SET expires_at = now() - interval '1 second', created_at = now() - interval '8 days' WHERE token_hash = $1").bind(sha256_hex(&token)).execute(&pool).await.unwrap();
        assert!(ask(token.clone()).await.is_err());

        sqlx::query("DELETE FROM pyo.users WHERE id = $1")
            .bind(&user)
            .execute(&pool)
            .await
            .unwrap();
    }
}
