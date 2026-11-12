//! `pyo login`, the CLI's sign-in.
//!
//! The CLI listens on localhost and opens the dashboard's `/cli-login` page
//! with a challenge (sha256 of a verifier it keeps). The signed-in user
//! approves there: `POST /api/v1/cli/approve` makes a one-time code, which
//! the page hands to the CLI's localhost address. The CLI trades code and
//! verifier at `POST /api/cli/token` for an API key named after its machine,
//! good for a year unless `pyo login --expires` says otherwise (`pyo login`
//! again makes a new one). Only hashes are stored; codes last 5 minutes and work once.

use axum::{Json, Router, extract::State, routing::post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState,
    auth::SessionUser,
    error::{ApiError, ApiResult},
    ids::{new_id, secret_token, sha256_hex},
};

/// Approve, from the dashboard (a browser session, not an API key).
pub fn routes() -> Router<AppState> {
    Router::new().route("/cli/approve", post(approve))
}

/// The CLI's side: no session, the code and verifier are the proof.
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/token", post(token))
}

#[derive(Deserialize)]
struct ApproveBody {
    challenge: String,
    name: String,
}

async fn approve(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(b): Json<ApproveBody>,
) -> ApiResult<Json<Value>> {
    if user.key.is_some() {
        return Err(ApiError::forbidden("Approve the CLI from the dashboard."));
    }
    if b.challenge.len() != 64 || !b.challenge.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request(
            "Invalid login request. Run pyo login again.",
        ));
    }
    let name: String = b
        .name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    let name = if name.is_empty() {
        "CLI".to_owned()
    } else {
        format!("CLI · {name}")
    };
    let code = secret_token();
    sqlx::query("DELETE FROM pyo.cli_logins WHERE expires_at < now()")
        .execute(&s.db)
        .await?;
    sqlx::query("INSERT INTO pyo.cli_logins (code_hash, user_id, challenge, name, expires_at) VALUES ($1, $2, $3, $4, now() + interval '5 minutes')")
        .bind(sha256_hex(&code))
        .bind(&user.id)
        .bind(b.challenge.to_ascii_lowercase())
        .bind(&name)
        .execute(&s.db)
        .await?;
    Ok(Json(json!({ "ok": true, "code": code })))
}

#[derive(Deserialize)]
struct TokenBody {
    code: String,
    verifier: String,
    /// "30d", "90d", "1y" (the default) or "never".
    #[serde(default)]
    expires: Option<String>,
}

/// Days until a CLI key expires (None: never).
fn cli_key_days(expires: Option<&str>) -> ApiResult<Option<i32>> {
    match expires {
        None | Some("1y") => Ok(Some(crate::auth::CLI_KEY_DAYS)),
        Some("30d") => Ok(Some(30)),
        Some("90d") => Ok(Some(90)),
        Some("never") => Ok(None),
        Some(_) => Err(ApiError::bad_request(
            "The key expires in 30d, 90d, 1y or never.",
        )),
    }
}

async fn token(State(s): State<AppState>, Json(b): Json<TokenBody>) -> ApiResult<Json<Value>> {
    let days = cli_key_days(b.expires.as_deref())?;
    let row: Option<(String, String, String)> = sqlx::query_as("DELETE FROM pyo.cli_logins WHERE code_hash = $1 AND expires_at > now() RETURNING user_id, challenge, name")
        .bind(sha256_hex(&b.code))
        .fetch_optional(&s.db)
        .await?;
    let Some((user_id, challenge, name)) = row else {
        return Err(ApiError::bad_request(
            "That login expired or was used. Run pyo login again.",
        ));
    };
    if sha256_hex(&b.verifier) != challenge {
        return Err(ApiError::bad_request(
            "That login doesn't match. Run pyo login again.",
        ));
    }
    let key = format!("{}{}", crate::auth::API_KEY_PREFIX, secret_token());
    sqlx::query("INSERT INTO pyo.api_tokens (id, user_id, name, token_hash, prefix, access, expires_at) VALUES ($1, $2, $3, $4, $5, 'full', now() + make_interval(days => $6))")
        .bind(new_id("tok"))
        .bind(&user_id)
        .bind(&name)
        .bind(sha256_hex(&key))
        .bind(key.chars().take(10).collect::<String>())
        .bind(days)
        .execute(&s.db)
        .await?;
    let (user_name, email): (String, String) =
        sqlx::query_as("SELECT name, email FROM pyo.users WHERE id = $1")
            .bind(&user_id)
            .fetch_one(&s.db)
            .await?;
    Ok(Json(
        json!({ "token": key, "user": { "name": user_name, "email": email } }),
    ))
}

const INSTALL_SCRIPT: &str = include_str!("../assets/cli.sh");

/// GET /install/cli.sh: the installer, pointed at this Pyo.
pub async fn install_script(headers: axum::http::HeaderMap) -> impl axum::response::IntoResponse {
    let url = crate::oauth::base_url(&headers);
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/x-shellscript; charset=utf-8",
        )],
        INSTALL_SCRIPT.replace("__PYO_URL__", &url),
    )
}

/// GET /install/pyo: the CLI binary (Linux x86-64, static), built into
/// the image next to the builder's.
pub async fn binary() -> ApiResult<impl axum::response::IntoResponse> {
    let path = std::env::var("PYO_CLI_BIN").unwrap_or_else(|_| "/app/bin/pyo".into());
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("CLI binary"))?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"pyo\"",
            ),
        ],
        bytes,
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_key_lifetimes() {
        use super::cli_key_days as d;
        assert_eq!(d(None).unwrap(), Some(365));
        assert_eq!(d(Some("1y")).unwrap(), Some(365));
        assert_eq!(d(Some("30d")).unwrap(), Some(30));
        assert_eq!(d(Some("90d")).unwrap(), Some(90));
        assert_eq!(d(Some("never")).unwrap(), None);
        assert!(d(Some("7d")).is_err());
    }
}
