//! `pyo login`, the CLI's sign-in.
//!
//! The CLI asks for a sign-in at `POST /api/cli/start` with a challenge
//! (sha256 of a verifier it keeps) and gets back the request's id and a
//! short code. It shows the code and opens the dashboard's `/cli-login`
//! page for that id. A signed-in person types the code there to approve
//! it (or refuses it). Meanwhile the CLI polls `POST /api/cli/token` with
//! the id and its verifier, and gets an API key named after its computer
//! once approved, good for a year unless `pyo login --expires` says
//! otherwise (`pyo login` again makes a new one).
//!
//! Nothing goes from the browser to the CLI, so it works from any address
//! and any browser, also over SSH. The code stops a link someone else
//! sends from approving their CLI: it's only in the terminal that asked.
//! The key only goes to whoever holds the verifier. Only hashes are
//! stored. Requests last 10 minutes, work once, and end after five wrong
//! codes.

use crate::error::Json;
use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::{
    AppState,
    auth::SessionUser,
    error::{ApiError, ApiResult},
    ids::{new_id, secret_token, sha256_hex},
    ratelimit::{self, ClientIp},
};

/// How long a request waits for its answer.
const MINUTES: i32 = 10;
/// Wrong codes a request takes before it ends.
const MAX_TRIES: i32 = 5;
/// The code's letters: consonants only, so no words and nothing that
/// reads like a digit.
const CODE_LETTERS: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

/// Approve or refuse, from the dashboard (a browser session, not an API key).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/cli/logins/{id}", get(show))
        .route("/cli/logins/{id}/approve", post(approve))
        .route("/cli/logins/{id}/deny", post(deny))
}

/// The CLI's side: no session, the verifier is the proof.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/start", post(start))
        .route("/token", post(token))
}

/// Gone, used or expired. (400 is a wrong code, so the page can tell.)
fn expired() -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "This sign-in expired. Run pyo login again.",
    )
}

fn refused() -> ApiError {
    ApiError::forbidden("This sign-in was refused in the browser. Run pyo login again.")
}

/// A new code: 8 letters, shown as ABCD-EFGH.
fn new_code() -> String {
    let mut out = String::with_capacity(9);
    while out.len() < 9 {
        let mut b = [0u8; 16];
        rand::fill(&mut b[..]);
        // 240 is 12 × 20: anything above would favor the first letters.
        for x in b.into_iter().filter(|x| *x < 240) {
            if out.len() == 4 {
                out.push('-');
            }
            if out.len() == 9 {
                break;
            }
            out.push(CODE_LETTERS[(x % 20) as usize] as char);
        }
    }
    out
}

/// A typed code as stored: its letters only, in capitals.
fn code_key(code: &str) -> String {
    let letters: String = code
        .chars()
        .filter(char::is_ascii_alphabetic)
        .map(|c| c.to_ascii_uppercase())
        .collect();
    sha256_hex(&letters)
}

/// Up to `max` printable characters.
fn clean(s: &str, max: usize) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect()
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

#[derive(Deserialize)]
struct StartBody {
    challenge: String,
    #[serde(default)]
    name: String,
    /// The CLI's version and OS ("pyo 0.5.0 on linux").
    #[serde(default)]
    client: String,
    /// "30d", "90d", "1y" (the default) or "never".
    #[serde(default)]
    expires: Option<String>,
}

/// A new sign-in request: its id (for the page's link) and its code.
async fn start(
    State(s): State<AppState>,
    ClientIp(ip): ClientIp,
    Json(b): Json<StartBody>,
) -> ApiResult<Json<Value>> {
    ratelimit::take(&s.db, &[(ratelimit::CLI_LOGIN_IP, &ip)]).await?;
    let (id, code) = start_login(&s.db, &b).await?;
    Ok(Json(json!({
        "id": id,
        "code": code,
        "expiresIn": MINUTES * 60,
        "interval": 2,
    })))
}

async fn start_login(db: &PgPool, b: &StartBody) -> ApiResult<(String, String)> {
    if b.challenge.len() != 64 || !b.challenge.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request(
            "Invalid login request. Run pyo login again.",
        ));
    }
    let days = cli_key_days(b.expires.as_deref())?;
    let (id, code) = (new_id("clg"), new_code());
    sqlx::query("DELETE FROM pyo.cli_login_requests WHERE expires_at < now()")
        .execute(db)
        .await?;
    sqlx::query("INSERT INTO pyo.cli_login_requests (id, challenge, code_hash, name, client, key_days, expires_at) VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(mins => $7))")
        .bind(&id)
        .bind(b.challenge.to_ascii_lowercase())
        .bind(code_key(&code))
        .bind(clean(&b.name, 40))
        .bind(clean(&b.client, 60))
        .bind(days)
        .bind(MINUTES)
        .execute(db)
        .await?;
    Ok((id, code))
}

/// A request, for the approval page.
async fn show(
    State(s): State<AppState>,
    SessionUser(_): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT name, client, status FROM pyo.cli_login_requests WHERE id = $1 AND expires_at > now()",
    )
    .bind(&id)
    .fetch_optional(&s.db)
    .await?;
    let Some((name, client, status)) = row else {
        return Err(expired());
    };
    Ok(Json(
        json!({ "name": name, "client": client, "status": status }),
    ))
}

#[derive(Deserialize)]
struct ApproveBody {
    code: String,
}

async fn approve(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Json(b): Json<ApproveBody>,
) -> ApiResult<Json<Value>> {
    if user.key.is_some() {
        return Err(ApiError::forbidden("Approve the CLI from the dashboard."));
    }
    approve_login(&s.db, &id, &user.id, &b.code).await?;
    Ok(Json(json!({ "ok": true })))
}

async fn approve_login(db: &PgPool, id: &str, user_id: &str, code: &str) -> ApiResult<()> {
    let mut tx = db.begin().await?;
    let row: Option<(String, String, i32)> = sqlx::query_as(
        "SELECT code_hash, status, tries FROM pyo.cli_login_requests WHERE id = $1 AND expires_at > now() FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((code_hash, status, tries)) = row else {
        return Err(expired());
    };
    match status.as_str() {
        "pending" => {}
        "approved" => return Err(ApiError::conflict("This sign-in is already approved.")),
        _ => return Err(refused()),
    }
    if code_key(code) != code_hash {
        let tries = tries + 1;
        let end = tries >= MAX_TRIES;
        sqlx::query("UPDATE pyo.cli_login_requests SET tries = $2, status = CASE WHEN $3 THEN 'denied' ELSE status END WHERE id = $1")
            .bind(id)
            .bind(tries)
            .bind(end)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(if end {
            ApiError::forbidden("Too many wrong codes. Run pyo login again.")
        } else {
            ApiError::bad_request("That's not the code in your terminal.")
        });
    }
    sqlx::query("UPDATE pyo.cli_login_requests SET status = 'approved', user_id = $2 WHERE id = $1")
        .bind(id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn deny(
    State(s): State<AppState>,
    SessionUser(_): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    sqlx::query("UPDATE pyo.cli_login_requests SET status = 'denied' WHERE id = $1 AND status = 'pending'")
        .bind(&id)
        .execute(&s.db)
        .await?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct TokenBody {
    id: String,
    verifier: String,
}

/// The CLI's poll: `{"status": "pending"}` until the page answers, then
/// the key once (approved), or an error that says why not.
async fn token(State(s): State<AppState>, Json(b): Json<TokenBody>) -> ApiResult<Json<Value>> {
    Ok(Json(match poll_login(&s.db, &b.id, &b.verifier).await? {
        None => json!({ "status": "pending" }),
        Some((key, user_name, email)) => {
            json!({ "token": key, "user": { "name": user_name, "email": email } })
        }
    }))
}

/// A request as the poll reads it: challenge, status, who approved it,
/// the computer's name and the key's days.
type Polled = (String, String, Option<String>, String, Option<i32>);

/// None while pending, else the new key and its user's name and email.
async fn poll_login(
    db: &PgPool,
    id: &str,
    verifier: &str,
) -> ApiResult<Option<(String, String, String)>> {
    let mut tx = db.begin().await?;
    let row: Option<Polled> = sqlx::query_as(
        "SELECT challenge, status, user_id, name, key_days FROM pyo.cli_login_requests WHERE id = $1 AND expires_at > now() FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((challenge, status, user_id, name, days)) = row else {
        return Err(expired());
    };
    if sha256_hex(verifier) != challenge {
        return Err(ApiError::bad_request(
            "That login doesn't match. Run pyo login again.",
        ));
    }
    let user_id = match (status.as_str(), user_id) {
        ("pending", _) => return Ok(None),
        ("approved", Some(u)) => u,
        _ => {
            sqlx::query("DELETE FROM pyo.cli_login_requests WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Err(refused());
        }
    };
    sqlx::query("DELETE FROM pyo.cli_login_requests WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let name = if name.is_empty() {
        "CLI".to_owned()
    } else {
        format!("CLI · {name}")
    };
    let key = format!("{}{}", crate::auth::API_KEY_PREFIX, secret_token());
    sqlx::query("INSERT INTO pyo.api_tokens (id, user_id, name, token_hash, prefix, access, expires_at) VALUES ($1, $2, $3, $4, $5, 'full', now() + make_interval(days => $6))")
        .bind(new_id("tok"))
        .bind(&user_id)
        .bind(&name)
        .bind(sha256_hex(&key))
        .bind(key.chars().take(10).collect::<String>())
        .bind(days)
        .execute(&mut *tx)
        .await?;
    let (user_name, email): (String, String) =
        sqlx::query_as("SELECT name, email FROM pyo.users WHERE id = $1")
            .bind(&user_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(Some((key, user_name, email)))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn codes_read_easily() {
        for _ in 0..200 {
            let c = new_code();
            assert_eq!(c.len(), 9);
            assert_eq!(&c[4..5], "-");
            assert!(
                c.bytes()
                    .filter(|b| *b != b'-')
                    .all(|b| CODE_LETTERS.contains(&b))
            );
        }
        // Typed in lower case, without the dash or with spaces: the same code.
        assert_eq!(code_key("BCDF-GHJK"), code_key("bcdf ghjk"));
        assert_ne!(code_key("BCDF-GHJK"), code_key("BCDF-GHJL"));
    }

    /// A code that isn't `code`.
    fn other(code: &str) -> &'static str {
        if code.starts_with('B') {
            "CCCC-CCCC"
        } else {
            "BBBB-BBBB"
        }
    }

    /// Against a real Postgres (DATABASE_URL, a throwaway one): a request
    /// waits, takes only its own code, and gives its key once and only to
    /// its verifier. Refused, mistyped and expired ones end.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn sign_in_requests() {
        let pool = PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&pool).await.unwrap();
        let user = new_id("usr");
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'C', '')",
        )
        .bind(&user)
        .bind(format!("{user}@cli.test"))
        .execute(&pool)
        .await
        .unwrap();
        let verifier = secret_token();
        let body = |expires: Option<&str>| StartBody {
            challenge: sha256_hex(&verifier),
            name: "laptop\n".into(),
            client: "pyo 0.5.0 on linux".into(),
            expires: expires.map(str::to_owned),
        };
        let msg = |e: ApiError| e.message;

        // Approved with its code: the key goes to the verifier, once.
        let (id, code) = start_login(&pool, &body(Some("30d"))).await.unwrap();
        assert!(poll_login(&pool, &id, &verifier).await.unwrap().is_none());
        assert!(msg(poll_login(&pool, &id, "wrong").await.unwrap_err()).contains("doesn't match"));
        assert!(
            msg(approve_login(&pool, &id, &user, other(&code))
                .await
                .unwrap_err())
            .contains("not the code")
        );
        approve_login(&pool, &id, &user, &code.to_lowercase())
            .await
            .unwrap();
        assert!(approve_login(&pool, &id, &user, &code).await.is_err());
        let (key, _, email) = poll_login(&pool, &id, &verifier).await.unwrap().unwrap();
        assert_eq!(email, format!("{user}@cli.test"));
        let (name, days): (String, f64) = sqlx::query_as(
            "SELECT name, extract(epoch FROM expires_at - now())::float8 / 86400 FROM pyo.api_tokens WHERE token_hash = $1",
        )
        .bind(sha256_hex(&key))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(name, "CLI · laptop");
        assert!((29.9..=30.0).contains(&days), "{days}");
        assert!(
            msg(poll_login(&pool, &id, &verifier).await.unwrap_err()).contains("expired"),
            "used"
        );

        // Refused: the CLI hears so, then it's gone.
        let (id, code) = start_login(&pool, &body(None)).await.unwrap();
        sqlx::query("UPDATE pyo.cli_login_requests SET status = 'denied' WHERE id = $1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(approve_login(&pool, &id, &user, &code).await.is_err());
        assert!(msg(poll_login(&pool, &id, &verifier).await.unwrap_err()).contains("refused"));
        assert!(msg(poll_login(&pool, &id, &verifier).await.unwrap_err()).contains("expired"));

        // Five wrong codes end it, the right one after them too.
        let (id, code) = start_login(&pool, &body(None)).await.unwrap();
        for _ in 0..MAX_TRIES {
            assert!(
                approve_login(&pool, &id, &user, other(&code))
                    .await
                    .is_err()
            );
        }
        assert!(approve_login(&pool, &id, &user, &code).await.is_err());
        assert!(msg(poll_login(&pool, &id, &verifier).await.unwrap_err()).contains("refused"));

        // Expired: nothing to approve or poll.
        let (id, code) = start_login(&pool, &body(None)).await.unwrap();
        sqlx::query(
            "UPDATE pyo.cli_login_requests SET expires_at = now() - interval '1 second' WHERE id = $1",
        )
        .bind(&id)
        .execute(&pool)
        .await
        .unwrap();
        assert!(
            msg(approve_login(&pool, &id, &user, &code).await.unwrap_err()).contains("expired")
        );
        assert!(msg(poll_login(&pool, &id, &verifier).await.unwrap_err()).contains("expired"));

        // A bad challenge or lifetime never starts.
        let bad = StartBody {
            challenge: "abc".into(),
            ..body(None)
        };
        assert!(start_login(&pool, &bad).await.is_err());
        assert!(start_login(&pool, &body(Some("7d"))).await.is_err());
        sqlx::query("DELETE FROM pyo.users WHERE id = $1")
            .bind(&user)
            .execute(&pool)
            .await
            .unwrap();
    }
}
