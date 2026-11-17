//! How agents prove who they are (`PYO_AGENT_AUTH`):
//!
//! - `token` (self-hosted, the default): the cluster's bearer token.
//! - `mtls`: a client certificate on the mTLS listener (agent_tls.rs), for
//!   a direct connection.
//! - `signed` (Pyo Cloud): every request signed with the agent's own key
//!   (pyo-proto `agentauth`), over plain HTTPS. It works behind a proxy that
//!   ends TLS, such as Cloudflare in front of agent.pyo.dev.
//!
//! In `mtls` and `signed` a bearer token alone gets nothing.
//!
//! Signed mode: the agent makes an Ed25519 key in its cluster and enrolls
//! its public half at `/enroll-key` with the connect command's token, which
//! works once. It gets a key id, good for a day, and swaps in a new key at
//! `/renew-key` at half that (the old one stays good for a few minutes).
//! Rotating the cluster's token revokes its keys. Each request's time must
//! be within a minute of Pyo's clock, and its nonce new.

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::post,
};
use pyo_proto::agentauth;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AppState,
    agent::AgentCluster,
    error::{ApiError, ApiResult, Json},
    ids::sha256_hex,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Token,
    Mtls,
    Signed,
}

pub fn mode() -> Mode {
    static M: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *M.get_or_init(|| {
        match std::env::var("PYO_AGENT_AUTH").unwrap_or_default().trim() {
            "signed" => Mode::Signed,
            "mtls" => Mode::Mtls,
            // Before PYO_AGENT_AUTH: certificates were asked for this way.
            _ if std::env::var("PYO_AGENT_REQUIRE_CERT").is_ok_and(|v| matches!(v.trim(), "1" | "true")) => Mode::Mtls,
            _ => Mode::Token,
        }
    })
}

/// Whether a bearer token alone is refused on agent routes.
pub fn refuse_tokens() -> bool {
    mode() != Mode::Token
}

/// A key's lifetime.
const KEY_HOURS: i64 = 24;

/// The agent a signed request came from (set by [`signed`]).
#[derive(Clone, Debug)]
pub struct VerifiedAgent {
    pub cluster_id: String,
    pub team_id: String,
}

fn refused(why: &str) -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": why }))).into_response()
}

/// Checks a signed agent request: its key, its signature over the method,
/// path, time, nonce and body, the time, and that the nonce is new. A
/// request without a signature goes on (the routes refuse it unless
/// tokens are allowed).
pub async fn signed(State(s): State<AppState>, req: Request, next: Next) -> Response {
    // Owned before any await: the request (its body) isn't Sync.
    let (key_id, sig) = {
        let h = req.headers();
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        (get(agentauth::KEY_HEADER), get(agentauth::SIGNATURE_HEADER))
    };
    let (Some(key_id), Some(sig)) = (key_id, sig) else {
        return next.run(req).await;
    };
    let (mut parts, body) = req.into_parts();
    let path = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map(|u| u.0.clone())
        .unwrap_or_else(|| parts.uri.clone());
    let path = path.path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_default();
    let Ok(bytes) = axum::body::to_bytes(body, 16 * 1024 * 1024).await else {
        return refused("The request's body is too large.");
    };
    let row: Result<Option<(String, String, String)>, _> = sqlx::query_as(
        "SELECT k.cluster_id, c.team_id, k.public_key FROM pyo.agent_keys k JOIN pyo.clusters c ON c.id = k.cluster_id
          WHERE k.id = $1 AND k.revoked_at IS NULL AND k.expires_at > now()",
    )
    .bind(&key_id)
    .fetch_optional(&s.db)
    .await;
    let Ok(Some((cluster_id, team_id, public_key))) = row else {
        return refused("This agent key is unknown, expired or revoked. Run the cluster's connect command again.");
    };
    let now = pyo_proto::now_ms() / 1000;
    let nonce = match agentauth::verify(&public_key, &key_id, parts.method.as_str(), &path, &bytes, &sig, now) {
        Ok(n) => n,
        Err(e) => return refused(&e.to_string()),
    };
    let fresh = sqlx::query("INSERT INTO pyo.agent_nonces (nonce) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(format!("{key_id}:{nonce}"))
        .execute(&s.db)
        .await;
    match fresh {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return refused("This request was already used."),
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response(),
    }
    parts.extensions.insert(VerifiedAgent { cluster_id, team_id });
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Forgets nonces once their requests are too old to pass anyway.
pub async fn sweeper(db: sqlx::PgPool) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        let _ = sqlx::query("DELETE FROM pyo.agent_nonces WHERE seen_at < now() - interval '5 minutes'")
            .execute(&db)
            .await;
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/enroll-key", post(enroll_key))
        .route("/renew-key", post(renew_key))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnrollKey {
    token: String,
    public_key: String,
}

async fn add_key(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, cluster_id: &str, public_key: &str) -> ApiResult<(String, i64)> {
    let id = crate::ids::new_id("akey");
    let not_after: i64 = sqlx::query_scalar(
        "INSERT INTO pyo.agent_keys (id, cluster_id, public_key, expires_at) VALUES ($1, $2, $3, now() + make_interval(hours => $4))
         RETURNING (extract(epoch FROM expires_at) * 1000)::bigint",
    )
    .bind(&id)
    .bind(cluster_id)
    .bind(public_key.trim())
    .bind(KEY_HOURS as i32)
    .fetch_one(&mut **tx)
    .await?;
    Ok((id, not_after))
}

/// The agent's first key, for the connect command's token. Once.
async fn enroll_key(State(s): State<AppState>, Json(b): Json<EnrollKey>) -> ApiResult<Json<Value>> {
    if mode() != Mode::Signed {
        return Err(ApiError::not_found("Agent keys"));
    }
    if !agentauth::valid_public_key(&b.public_key) {
        return Err(ApiError::bad_request("That isn't an Ed25519 public key."));
    }
    let hash = sha256_hex(b.token.trim());
    let mut tx = s.db.begin().await?;
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT id, agent_token_used_hash FROM pyo.clusters WHERE agent_token_hash = $1 FOR UPDATE")
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?;
    let (cluster, used) = row.ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Unknown connect token."))?;
    if used.as_deref() == Some(hash.as_str()) {
        return Err(ApiError::conflict(
            "This connect command was used already. Get a new one in the cluster's settings.",
        ));
    }
    // A cluster has one agent: a new enrollment ends the keys before it.
    sqlx::query("UPDATE pyo.agent_keys SET revoked_at = now() WHERE cluster_id = $1 AND revoked_at IS NULL")
        .bind(&cluster)
        .execute(&mut *tx)
        .await?;
    let (key_id, not_after) = add_key(&mut tx, &cluster, &b.public_key).await?;
    sqlx::query("UPDATE pyo.clusters SET agent_token_used_hash = $2 WHERE id = $1")
        .bind(&cluster)
        .bind(&hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(%cluster, %key_id, "agent enrolled its key");
    Ok(Json(json!({ "keyId": key_id, "notAfter": not_after })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RenewKey {
    public_key: String,
}

/// A new key for the agent, asked for with its current one. The current
/// one keeps working for 15 minutes, for requests already on their way.
async fn renew_key(
    State(s): State<AppState>,
    cluster: AgentCluster,
    verified: Option<axum::Extension<VerifiedAgent>>,
    Json(b): Json<RenewKey>,
) -> ApiResult<Json<Value>> {
    if verified.is_none() {
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "Renewing needs a signed request."));
    }
    if !agentauth::valid_public_key(&b.public_key) {
        return Err(ApiError::bad_request("That isn't an Ed25519 public key."));
    }
    let mut tx = s.db.begin().await?;
    sqlx::query(
        "UPDATE pyo.agent_keys SET expires_at = least(expires_at, now() + interval '15 minutes')
          WHERE cluster_id = $1 AND revoked_at IS NULL",
    )
    .bind(&cluster.id)
    .execute(&mut *tx)
    .await?;
    let (key_id, not_after) = add_key(&mut tx, &cluster.id, &b.public_key).await?;
    tx.commit().await?;
    Ok(Json(json!({ "keyId": key_id, "notAfter": not_after })))
}

/// The cluster's keys stop working (its token was rotated, or it was removed).
pub async fn revoke(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, cluster_id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.agent_keys SET revoked_at = now() WHERE cluster_id = $1 AND revoked_at IS NULL")
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
