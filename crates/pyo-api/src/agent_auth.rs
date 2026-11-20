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
//!
//! Components: the cluster's builders and its pyo-static sign their
//! requests the same way, in every mode, with keys the agent makes in the
//! cluster and registers at `/component-keys` (one live key per builder,
//! one for pyo-static). A component's key is only its own: never the
//! agent's routes. Rotating the token or enrolling again revokes them too,
//! and the agent registers new ones (builders get their spec again,
//! builders.rs). Their bearer tokens are refused when agents don't use
//! tokens, and otherwise only until that component has a key.

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

/// The agent (or component) a signed request came from (set by [`signed`]).
#[derive(Clone, Debug)]
pub struct VerifiedAgent {
    pub cluster_id: String,
    pub team_id: String,
    /// "agent", "builder" or "static".
    pub component: String,
    /// The builder, for a builder's key.
    pub builder_id: Option<String>,
}

impl VerifiedAgent {
    pub fn is_agent(&self) -> bool {
        self.component == AGENT
    }
}

pub const AGENT: &str = "agent";
pub const BUILDER: &str = "builder";
pub const STATIC: &str = "static";

/// A body declared by its hash (agentauth `BODY_SHA256_HEADER`), checked as
/// it streams: a body that doesn't match ends in an error, so nothing
/// downstream takes it as complete (the store checks the same hash too).
fn checked_body(body: Body, want: String) -> Body {
    use futures::StreamExt;
    use sha2::Digest;
    let state = (body.into_data_stream(), Some(sha2::Sha256::new()), want);
    let stream = futures::stream::unfold(state, |(mut st, hasher, want)| async move {
        let mut h = hasher?;
        match st.next().await {
            Some(Ok(chunk)) => {
                h.update(&chunk);
                Some((Ok(chunk), (st, Some(h), want)))
            }
            Some(Err(e)) => Some((Err(std::io::Error::other(e)), (st, None, want))),
            None if hex::encode(h.finalize()) == want => None,
            None => Some((
                Err(std::io::Error::other("the body doesn't match the hash it was signed with")),
                (st, None, want),
            )),
        }
    });
    Body::from_stream(stream)
}

fn refused(why: &str) -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": why }))).into_response()
}

/// A key's cluster, team, public key, component and builder.
type KeyRow = (String, String, String, String, Option<String>);

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
    // A body signed by the hash it declares streams through, checked on the
    // way. Any other is read whole and signed as it is.
    let declared = parts
        .headers
        .get(agentauth::BODY_SHA256_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase);
    let (body, body_sha) = match declared {
        Some(sha) => (checked_body(body, sha.clone()), sha),
        None => {
            let Ok(bytes) = axum::body::to_bytes(body, 16 * 1024 * 1024).await else {
                return refused("The request's body is too large.");
            };
            let sha = agentauth::body_sha256(&bytes);
            (Body::from(bytes), sha)
        }
    };
    let row: sqlx::Result<Option<KeyRow>> = sqlx::query_as(
        "SELECT k.cluster_id, c.team_id, k.public_key, k.component, k.builder_id FROM pyo.agent_keys k JOIN pyo.clusters c ON c.id = k.cluster_id
          WHERE k.id = $1 AND k.revoked_at IS NULL AND k.expires_at > now()",
    )
    .bind(&key_id)
    .fetch_optional(&s.db)
    .await;
    let Ok(Some((cluster_id, team_id, public_key, component, builder_id))) = row else {
        return refused("This key is unknown, expired or revoked. Run the cluster's connect command again.");
    };
    let now = pyo_proto::now_ms() / 1000;
    let nonce = match agentauth::verify_hashed(&public_key, &key_id, parts.method.as_str(), &path, &body_sha, &sig, now) {
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
    parts.extensions.insert(VerifiedAgent { cluster_id, team_id, component, builder_id });
    next.run(Request::from_parts(parts, body)).await
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
        .merge(component_routes())
}

/// Where the agent registers its components' keys (on the mTLS listener too).
pub fn component_routes() -> Router<AppState> {
    Router::new()
        .route("/component-keys", post(component_key))
        .route("/component-keys/{id}", axum::routing::get(component_key_live))
}

/// 200 while one of this cluster's components' keys is live, else 404.
async fn component_key_live(
    State(s): State<AppState>,
    cluster: AgentCluster,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> ApiResult<Json<Value>> {
    let live: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.agent_keys WHERE id = $1 AND cluster_id = $2 AND component <> 'agent'
                         AND revoked_at IS NULL AND expires_at > now())",
    )
    .bind(&id)
    .bind(&cluster.id)
    .fetch_one(&s.db)
    .await?;
    if !live {
        return Err(ApiError::not_found("Key"));
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComponentKey {
    /// "builder" or "static".
    component: String,
    #[serde(default)]
    builder_id: Option<String>,
    public_key: String,
}

/// A key for one of the cluster's components, made by its agent in the
/// cluster: the builder's (one of this cluster's) or pyo-static's. It
/// replaces that component's key before it.
async fn component_key(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Json(b): Json<ComponentKey>,
) -> ApiResult<Json<Value>> {
    if !agentauth::valid_public_key(&b.public_key) {
        return Err(ApiError::bad_request("That isn't an Ed25519 public key."));
    }
    let builder = match (b.component.as_str(), b.builder_id.as_deref()) {
        (BUILDER, Some(id)) => Some(id.to_owned()),
        (STATIC, None) => None,
        _ => return Err(ApiError::bad_request("Keys are for a builder (with its id) or pyo-static.")),
    };
    let mut tx = s.db.begin().await?;
    if let Some(id) = &builder {
        let ours: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.builders WHERE id = $1 AND cluster_id = $2 AND status <> 'removing')",
        )
        .bind(id)
        .bind(&cluster.id)
        .fetch_one(&mut *tx)
        .await?;
        if !ours {
            return Err(ApiError::not_found("Builder"));
        }
    }
    sqlx::query(
        "UPDATE pyo.agent_keys SET revoked_at = now()
          WHERE cluster_id = $1 AND component = $2 AND builder_id IS NOT DISTINCT FROM $3 AND revoked_at IS NULL",
    )
    .bind(&cluster.id)
    .bind(&b.component)
    .bind(&builder)
    .execute(&mut *tx)
    .await?;
    let id = crate::ids::new_id("akey");
    sqlx::query(
        "INSERT INTO pyo.agent_keys (id, cluster_id, public_key, expires_at, component, builder_id) VALUES ($1, $2, $3, 'infinity', $4, $5)",
    )
    .bind(&id)
    .bind(&cluster.id)
    .bind(b.public_key.trim())
    .bind(&b.component)
    .bind(&builder)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    tracing::info!(cluster = %cluster.id, component = %b.component, builder = ?builder, key = %id, "registered a component's key");
    Ok(Json(json!({ "keyId": id })))
}

/// Whether a component has a live key: its bearer token no longer counts.
pub async fn has_key(
    db: &sqlx::PgPool,
    cluster_id: &str,
    component: &str,
    builder_id: Option<&str>,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.agent_keys WHERE cluster_id = $1 AND component = $2
                         AND builder_id IS NOT DISTINCT FROM $3 AND revoked_at IS NULL AND expires_at > now())",
    )
    .bind(cluster_id)
    .bind(component)
    .bind(builder_id)
    .fetch_one(db)
    .await
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
    // A cluster has one agent: a new enrollment ends the keys before it,
    // its components' too (the new agent registers theirs).
    sqlx::query("UPDATE pyo.agent_keys SET revoked_at = now() WHERE cluster_id = $1 AND revoked_at IS NULL")
        .bind(&cluster)
        .execute(&mut *tx)
        .await?;
    builders_need_keys(&mut tx, &cluster).await?;
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
          WHERE cluster_id = $1 AND component = 'agent' AND revoked_at IS NULL",
    )
    .bind(&cluster.id)
    .execute(&mut *tx)
    .await?;
    let (key_id, not_after) = add_key(&mut tx, &cluster.id, &b.public_key).await?;
    tx.commit().await?;
    Ok(Json(json!({ "keyId": key_id, "notAfter": not_after })))
}

/// The cluster's keys stop working, its components' too (its token was
/// rotated, or it was removed).
pub async fn revoke(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, cluster_id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.agent_keys SET revoked_at = now() WHERE cluster_id = $1 AND revoked_at IS NULL")
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
    builders_need_keys(tx, cluster_id).await
}

/// The cluster's builders get their spec (and a new key) from the next
/// agent as soon as it reports in (builders.rs).
async fn builders_need_keys(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, cluster_id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.builders SET info = info - 'keyAskedAt' WHERE cluster_id = $1 AND info ? 'keyAskedAt'")
        .bind(cluster_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Method, Request as HttpRequest};
    use pyo_proto::agentauth::RequestKey;
    use tower::ServiceExt as _;

    #[tokio::test]
    async fn a_streamed_body_must_match_its_signed_hash() {
        let collect = |b: Body| async move { axum::body::to_bytes(b, 1 << 20).await };
        let file = b"<h1>hi</h1>".to_vec();
        let good = checked_body(Body::from(file.clone()), agentauth::body_sha256(&file));
        assert_eq!(collect(good).await.unwrap().to_vec(), file);
        let bad = checked_body(Body::from(b"<h1>other</h1>".to_vec()), agentauth::body_sha256(&file));
        assert!(collect(bad).await.is_err());
    }

    fn state(db: sqlx::PgPool) -> AppState {
        let config = crate::Config {
            agent_api_url: String::new(),
            agent_image: String::new(),
            waf_image: String::new(),
            geoip_image: String::new(),
            cookie_secure: false,
            allow_signup: false,
            builder_image: String::new(),
            relay_addr: None,
        };
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        AppState {
            db: db.clone(),
            config: std::sync::Arc::new(config),
            observability: crate::observability::Observability::from_env(db),
            sealer: std::sync::Arc::new(crate::secrets::Sealer::for_tests()),
            wakers: Default::default(),
            terminals: Default::default(),
            stopping,
        }
    }

    struct Key(String, RequestKey);

    fn new_key() -> (RequestKey, String) {
        let k = RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap();
        let pk = k.public_key();
        (k, pk)
    }

    async fn call(app: &Router, method: Method, path: &str, key: Option<&Key>, bearer: Option<&str>, body: &[u8]) -> (StatusCode, Value) {
        let mut req = HttpRequest::builder().method(method.clone()).uri(path).header("content-type", "application/json");
        if let Some(Key(id, k)) = key {
            for (h, v) in k.sign(id, method.as_str(), path, body, pyo_proto::now_ms() / 1000) {
                req = req.header(h, v);
            }
        }
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let res = app.clone().oneshot(req.body(Body::from(body.to_vec())).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Against a throwaway Postgres: the agent registers its builders' and
    /// pyo-static's keys; each signs only as itself; a builder's token stops
    /// counting once it has a key; a new key revokes the one before, and
    /// revoking the cluster's keys takes the components' too.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn components_sign_with_keys_the_agent_registers() {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap()).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        let run = crate::ids::random_string(8);
        let (team, cl, other) = (format!("team_k{run}"), format!("cl_k{run}"), format!("cl_o{run}"));
        let (bld, theirs) = (format!("bld_k{run}"), format!("bld_o{run}"));
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)").bind(&team).execute(&db).await.unwrap();
        for c in [&cl, &other] {
            q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash) VALUES ($1, $2, $1, $1)").bind(c).bind(&team).execute(&db).await.unwrap();
        }
        let door = format!("door{run}");
        for (b, c) in [(&bld, &cl), (&theirs, &other)] {
            q("INSERT INTO pyo.builders (id, team_id, name, kind, cluster_id, server, token_hash, status) VALUES ($1, $2, $1, 'cluster', $3, 's', $4, 'ready')")
                .bind(b).bind(&team).bind(c).bind(sha256_hex(&format!("{door}{b}"))).execute(&db).await.unwrap();
        }
        let (agent_key, agent_pk) = new_key();
        let agent_id = format!("akey_a{run}");
        q("INSERT INTO pyo.agent_keys (id, cluster_id, public_key, expires_at) VALUES ($1, $2, $3, now() + interval '1 hour')")
            .bind(&agent_id).bind(&cl).bind(&agent_pk).execute(&db).await.unwrap();
        let agent = Key(agent_id, agent_key);
        let s = state(db.clone());
        let layer = || axum::middleware::from_fn_with_state(s.clone(), signed);
        let app = Router::new()
            .nest("/api/agent/v1", routes().layer(layer()))
            .nest("/api/builder/v1", crate::builders::routes().merge(crate::sites::builder_routes()).layer(layer()))
            .nest("/api/static/v1", crate::sites::routes().layer(layer()))
            .with_state(s.clone());

        // Before it has a key, a builder's token still counts (agents use tokens here).
        let hb = br#"{"version":"t","buildkit":true,"cpus":1,"memoryGi":1}"#;
        let token = format!("{door}{bld}");
        assert_eq!(call(&app, Method::POST, "/api/builder/v1/heartbeat", None, Some(&token), hb).await.0, StatusCode::OK);

        // The agent registers its builder's key.
        let (bkey, bpk) = new_key();
        let body = serde_json::to_vec(&json!({ "component": "builder", "builderId": bld, "publicKey": bpk })).unwrap();
        let (st, v) = call(&app, Method::POST, "/api/agent/v1/component-keys", Some(&agent), None, &body).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let builder = Key(v["keyId"].as_str().unwrap().to_owned(), bkey);
        assert_eq!(call(&app, Method::POST, "/api/builder/v1/heartbeat", Some(&builder), None, hb).await.0, StatusCode::OK);
        // Its token no longer counts.
        assert_eq!(call(&app, Method::POST, "/api/builder/v1/heartbeat", None, Some(&token), hb).await.0, StatusCode::UNAUTHORIZED);
        // Each key only as itself.
        let (_, x) = new_key();
        let body2 = serde_json::to_vec(&json!({ "component": "static", "publicKey": x })).unwrap();
        assert_eq!(call(&app, Method::POST, "/api/agent/v1/component-keys", Some(&builder), None, &body2).await.0, StatusCode::FORBIDDEN);
        assert_eq!(call(&app, Method::POST, "/api/builder/v1/heartbeat", Some(&agent), None, hb).await.0, StatusCode::FORBIDDEN);
        // Not for another cluster's builder.
        let body3 = serde_json::to_vec(&json!({ "component": "builder", "builderId": theirs, "publicKey": x })).unwrap();
        assert_eq!(call(&app, Method::POST, "/api/agent/v1/component-keys", Some(&agent), None, &body3).await.0, StatusCode::NOT_FOUND);

        // pyo-static: its key reads (an unknown site, once let in, is a 404).
        let (skey, spk) = new_key();
        let body4 = serde_json::to_vec(&json!({ "component": "static", "publicKey": spk })).unwrap();
        let (st, v) = call(&app, Method::POST, "/api/agent/v1/component-keys", Some(&agent), None, &body4).await;
        assert_eq!(st, StatusCode::OK);
        let reader = Key(v["keyId"].as_str().unwrap().to_owned(), skey);
        let manifest = "/api/static/v1/sites/svc_x/deploys/dpl_x/manifest";
        assert_eq!(call(&app, Method::GET, manifest, Some(&reader), None, b"").await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&app, Method::GET, manifest, Some(&builder), None, b"").await.0, StatusCode::FORBIDDEN);
        assert_eq!(call(&app, Method::GET, manifest, None, None, b"").await.0, StatusCode::UNAUTHORIZED);
        let live = format!("/api/agent/v1/component-keys/{}", reader.0);
        assert_eq!(call(&app, Method::GET, &live, Some(&agent), None, b"").await.0, StatusCode::OK);

        // A new builder key revokes the one before.
        let (bkey2, bpk2) = new_key();
        let body5 = serde_json::to_vec(&json!({ "component": "builder", "builderId": bld, "publicKey": bpk2 })).unwrap();
        let (_, v) = call(&app, Method::POST, "/api/agent/v1/component-keys", Some(&agent), None, &body5).await;
        let builder2 = Key(v["keyId"].as_str().unwrap().to_owned(), bkey2);
        assert_eq!(call(&app, Method::POST, "/api/builder/v1/heartbeat", Some(&builder), None, hb).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&app, Method::POST, "/api/builder/v1/heartbeat", Some(&builder2), None, hb).await.0, StatusCode::OK);

        // The cluster's keys revoked (a new connect command): the components' go too.
        let mut tx = db.begin().await.unwrap();
        revoke(&mut tx, &cl).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(call(&app, Method::POST, "/api/builder/v1/heartbeat", Some(&builder2), None, hb).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(call(&app, Method::GET, manifest, Some(&reader), None, b"").await.0, StatusCode::UNAUTHORIZED);
        let agent2 = Key(format!("akey_b{run}"), RequestKey::from_pkcs8(&RequestKey::generate_pkcs8().unwrap()).unwrap());
        q("INSERT INTO pyo.agent_keys (id, cluster_id, public_key, expires_at) VALUES ($1, $2, $3, now() + interval '1 hour')")
            .bind(&agent2.0).bind(&cl).bind(agent2.1.public_key()).execute(&db).await.unwrap();
        assert_eq!(call(&app, Method::GET, &live, Some(&agent2), None, b"").await.0, StatusCode::NOT_FOUND);
    }
}
