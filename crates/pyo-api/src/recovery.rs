//! Getting back into an account: one-time sign-in links that let their
//! owner choose a new password.
//!
//! A link comes from one of three places:
//! - `pyo-reset-login` on the server Pyo runs on (install.rs
//!   `reset_login`, with the host's bootstrap key), for any account. It's
//!   how the only admin gets back in.
//! - "Forgot password?" on the sign-in page, when email is set up: the link
//!   goes to the address, never to the page. The answer is the same whether
//!   or not an account has it, and it's rate-limited per address and email.
//! - A team owner's "Reset sign-in" on Members (ops.rs): emailed to the
//!   member, or shown to the owner to pass on when email isn't set up and
//!   the account belongs to that team alone.
//!
//! Only the token's sha256 is kept (pyo.sign_in_links). A link works once,
//! for [`LINK_MINUTES`], and a new one for the same account replaces it.
//! Using it sets the new password, ends the account's sessions and signs
//! this browser in.

use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState,
    auth::User,
    error::{ApiError, ApiResult},
    ids::{secret_token, sha256_hex},
    mail,
    ratelimit::{self, ClientIp},
};

/// How long a sign-in link works.
pub const LINK_MINUTES: i32 = 30;

/// Where a sign-in link points.
pub fn link_path(token: &str) -> String {
    format!("/reset-password?token={token}")
}

/// A new sign-in link's secret for the account, replacing any earlier one.
pub async fn new_link(conn: &mut sqlx::PgConnection, user_id: &str) -> ApiResult<String> {
    let token = secret_token();
    sqlx::query("DELETE FROM pyo.sign_in_links WHERE user_id = $1 OR expires_at < now()")
        .bind(user_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO pyo.sign_in_links (token_hash, user_id, expires_at) VALUES ($1, $2, now() + make_interval(mins => $3))")
        .bind(sha256_hex(&token))
        .bind(user_id)
        .bind(LINK_MINUTES)
        .execute(&mut *conn)
        .await?;
    Ok(token)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/forgot", post(forgot))
        .route("/reset", get(link_info).post(reset))
}

fn used_or_expired() -> ApiError {
    ApiError::bad_request("This link expired or was already used. Ask for a new one.")
}

#[derive(Deserialize)]
struct ForgotBody {
    email: String,
}

/// "Forgot password?": mails a sign-in link when an account has the email.
/// Answers the same either way, before any email goes out.
async fn forgot(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Json(body): Json<ForgotBody>,
) -> ApiResult<StatusCode> {
    crate::auth::require_csrf(&headers)?;
    let email = crate::auth::normalize_email(&body.email)?;
    let Some(cfg) = mail::settings(&state.db).await? else {
        return Err(ApiError::bad_request(
            "This Pyo can't send email. Ask its admin to reset your sign-in.",
        ));
    };
    ratelimit::take(
        &state.db,
        &[
            (ratelimit::FORGOT_IP, &ip),
            (ratelimit::FORGOT_ACCOUNT, &email),
        ],
    )
    .await?;
    tokio::spawn(async move {
        if let Err(e) = mail_link(&state, &cfg, &email).await {
            tracing::warn!(error = %format!("{e:#}"), "sign-in link email not sent");
        }
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn mail_link(state: &AppState, cfg: &mail::EmailSettings, email: &str) -> anyhow::Result<()> {
    let user: Option<String> =
        sqlx::query_scalar("SELECT id FROM pyo.users WHERE email = $1 AND NOT must_set_up")
            .bind(email)
            .fetch_optional(&state.db)
            .await?;
    let Some(user) = user else {
        return Ok(());
    };
    let mut conn = state.db.acquire().await?;
    let token = new_link(&mut conn, &user)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let (subject, text, html) = mail::sign_in_link(&cfg.link(&link_path(&token)), LINK_MINUTES);
    mail::send(state, cfg, email, &subject, text, html).await
}

#[derive(Deserialize)]
struct TokenQuery {
    token: String,
}

/// The account a link is for, while it works.
async fn link_info(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    Query(q): Query<TokenQuery>,
) -> ApiResult<Json<Value>> {
    ratelimit::take(&state.db, &[(ratelimit::SIGN_IN_LINK_IP, &ip)]).await?;
    let email: Option<String> = sqlx::query_scalar(
        "SELECT u.email FROM pyo.sign_in_links l JOIN pyo.users u ON u.id = l.user_id
          WHERE l.token_hash = $1 AND l.expires_at > now()",
    )
    .bind(sha256_hex(q.token.trim()))
    .fetch_optional(&state.db)
    .await?;
    let email = email.ok_or_else(used_or_expired)?;
    ratelimit::forgive(&state.db, &ratelimit::SIGN_IN_LINK_IP, &ip).await?;
    Ok(Json(json!({ "email": email })))
}

#[derive(Deserialize)]
struct ResetBody {
    token: String,
    password: String,
}

/// A link used: the new password, every other session ended, and this
/// browser signed in.
async fn reset(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<ResetBody>,
) -> ApiResult<(CookieJar, Json<User>)> {
    crate::auth::require_csrf(&headers)?;
    crate::auth::check_password(&body.password)?;
    ratelimit::take(&state.db, &[(ratelimit::SIGN_IN_LINK_IP, &ip)]).await?;
    let hash = crate::auth::hash_password(&body.password).await?;
    let mut tx = state.db.begin().await?;
    let user_id: Option<String> = sqlx::query_scalar(
        "DELETE FROM pyo.sign_in_links WHERE token_hash = $1 AND expires_at > now() RETURNING user_id",
    )
    .bind(sha256_hex(body.token.trim()))
    .fetch_optional(&mut *tx)
    .await?;
    let user_id = user_id.ok_or_else(used_or_expired)?;
    let (id, name, email, timezone, is_admin, must_set_up): (String, String, String, String, bool, bool) =
        sqlx::query_as("UPDATE pyo.users SET password_hash = $2 WHERE id = $1 RETURNING id, name, email, timezone, is_admin, must_set_up")
            .bind(&user_id)
            .bind(&hash)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("DELETE FROM pyo.sessions WHERE user_id = $1")
        .bind(&user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    ratelimit::forgive(&state.db, &ratelimit::SIGN_IN_LINK_IP, &ip).await?;
    ratelimit::reset(&state.db, &ratelimit::LOGIN_ACCOUNT, &email).await?;
    tracing::info!(user = %id, "password replaced with a sign-in link");
    let jar = crate::auth::start_session(&state, jar, &id, &headers).await?;
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

/// Can this sign-in link be shown to a team's owner, rather than only
/// emailed? Only when the account is in that team alone and doesn't run
/// the installation: the link would let the owner into everything else.
pub async fn owner_may_see(
    conn: &mut sqlx::PgConnection,
    user_id: &str,
    team_id: &str,
) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT NOT u.is_admin AND NOT EXISTS (
            SELECT 1 FROM pyo.members m WHERE m.user_id = u.id AND m.team_id <> $2)
           FROM pyo.users u WHERE u.id = $1",
    )
    .bind(user_id)
    .bind(team_id)
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_paths() {
        assert_eq!(link_path("abc"), "/reset-password?token=abc");
    }
}
