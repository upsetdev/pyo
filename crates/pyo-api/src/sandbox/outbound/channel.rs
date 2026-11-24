//! An outbound sandbox server's channel (`/api/sandbox-agent/v1`): its
//! join, then requests signed with its own key (pyo-proto `agentauth`, the
//! agents' headers): renewing that key, the long poll for its commands,
//! their results and its report.
//!
//! A server's key opens only that server's channel: never another
//! server's, team's, agent's or person's route. Every query here is bound
//! to the key's server and team, and runs in a transaction lowered to that
//! team (rls.rs).
//!
//! Commands are queued per server ([`enqueue`], sealed at rest), signed by
//! the signer when the server takes them (`pyosbx1`, the signer picks the
//! sequence number), and answered with the daemon's [`DaemonResult`].
//! [`ask`] queues one and waits for its answer.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Extension, Router,
    body::Body,
    extract::{FromRequestParts, Path, Request, State},
    http::{HeaderMap, StatusCode, request::Parts},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use pyo_proto::agentauth;
use pyo_proto::sandbox::{
    DaemonCommand, DaemonError, DaemonResult, ErrorCode, ServerReport, outbound as wire,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::{
    AppState, CommandSigner,
    error::{ApiError, ApiResult, Json, Query},
    ids::sha256_hex,
};

/// The signer, for the routes (None: commands can't be signed here).
#[derive(Clone)]
pub struct Signer(pub Option<Arc<dyn CommandSigner>>);

/// The channel's routes, for `/api/sandbox-agent/v1` on a hosted Pyo.
pub fn routes(state: AppState, signer: Option<Arc<dyn CommandSigner>>) -> Router<AppState> {
    let signed = Router::new()
        .route(wire::RENEW_KEY_PATH, post(renew_key))
        .route(wire::COMMANDS_PATH, get(poll))
        .route("/commands/{id}", post(result))
        .route(wire::REPORT_PATH, post(report))
        .layer(axum::middleware::from_fn_with_state(state, verify));
    Router::new()
        .route(
            wire::JOIN_PATH,
            post(join).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .merge(signed)
        .layer(Extension(Signer(signer)))
}

// ── Who's asking ───────────────────────────────────────────────────────────

/// The sandbox server a signed request came from (set by [`verify`]).
#[derive(Clone, Debug)]
pub struct SandboxServer {
    pub id: String,
    pub team_id: String,
    pub key_id: String,
}

impl<S: Send + Sync> FromRequestParts<S> for SandboxServer {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SandboxServer>()
            .cloned()
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Sign this request with the server's key."))
    }
}

/// The largest body a server sends (a report, or a command's result).
const MAX_BODY: usize = 4 * 1024 * 1024;

fn refused(why: &str) -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": why }))).into_response()
}

/// Why a key doesn't open anything. The daemon stops when it gets it.
pub const KEY_GONE: &str = wire::KEY_GONE;

/// Checks a signed request: the server's key (live, for an outbound
/// server of its team), its signature over the method, path, time, nonce
/// and body, the time, and that the nonce is new. Unsigned requests get
/// nothing.
pub async fn verify(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let (key_id, sig) = {
        let h = req.headers();
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        (get(agentauth::KEY_HEADER), get(agentauth::SIGNATURE_HEADER))
    };
    let (Some(key_id), Some(sig)) = (key_id, sig) else {
        return refused("Sign this request with the server's key.");
    };
    let (mut parts, body) = req.into_parts();
    let path = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map(|u| u.0.clone())
        .unwrap_or_else(|| parts.uri.clone());
    let path = path.path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_default();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return refused("The request's body is too large.");
    };
    let row: sqlx::Result<Option<(String, String, String)>> = sqlx::query_as(
        "SELECT k.server_id, k.team_id, k.public_key FROM pyo.sandbox_server_keys k
           JOIN pyo.sandbox_servers s ON s.id = k.server_id AND s.team_id = k.team_id
          WHERE k.id = $1 AND k.revoked_at IS NULL AND k.expires_at > now() AND s.outbound",
    )
    .bind(&key_id)
    .fetch_optional(&s.db)
    .await;
    let (server_id, team_id, public_key) = match row {
        Ok(Some(r)) => r,
        Ok(None) => return refused(KEY_GONE),
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response(),
    };
    let now = pyo_proto::now_ms() / 1000;
    let nonce = match agentauth::verify(&public_key, &key_id, parts.method.as_str(), &path, &bytes, &sig, now) {
        Ok(n) => n,
        Err(e) => return refused(&e.to_string()),
    };
    // Kept with the agents' nonces (swept after five minutes), under the key.
    let fresh = sqlx::query("INSERT INTO pyo.agent_nonces (nonce) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(format!("{key_id}:{nonce}"))
        .execute(&s.db)
        .await;
    match fresh {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return refused("This request was already used."),
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response(),
    }
    parts.extensions.insert(SandboxServer { id: server_id, team_id, key_id });
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// A transaction lowered to the server's team.
async fn team_tx(s: &AppState, server: &SandboxServer) -> ApiResult<sqlx::Transaction<'static, sqlx::Postgres>> {
    let mut tx = s.db.begin().await?;
    crate::rls::scope_team(&mut tx, &server.team_id).await?;
    Ok(tx)
}

// ── Join and keys ──────────────────────────────────────────────────────────

async fn join(
    State(s): State<AppState>,
    Extension(signer): Extension<Signer>,
    Json(ask): Json<wire::JoinRequest>,
) -> ApiResult<Json<wire::JoinAnswer>> {
    let keys = signer.0.as_ref().map(|k| k.public_keys()).unwrap_or_default();
    Ok(Json(
        super::join(&s.db, &s.sealer, &keys, &s.config.agent_api_url, &ask).await?,
    ))
}

/// A new key, asked for with the current one. The keys before it keep
/// working for a few minutes, for requests already on their way.
async fn renew_key(
    State(s): State<AppState>,
    server: SandboxServer,
    Json(b): Json<wire::RenewKey>,
) -> ApiResult<Json<wire::KeyAnswer>> {
    if !agentauth::valid_public_key(&b.public_key) {
        return Err(ApiError::bad_request("That isn't an Ed25519 public key."));
    }
    let mut tx = team_tx(&s, &server).await?;
    sqlx::query(
        "UPDATE pyo.sandbox_server_keys SET expires_at = least(expires_at, now() + make_interval(secs => $3))
          WHERE server_id = $1 AND team_id = $2 AND revoked_at IS NULL",
    )
    .bind(&server.id)
    .bind(&server.team_id)
    .bind(super::RENEWED_KEY_GRACE.as_secs_f64())
    .execute(&mut *tx)
    .await?;
    let (key_id, not_after) = super::add_key(&mut tx, &server.id, &server.team_id, &b.public_key).await?;
    tx.commit().await?;
    tracing::info!(server = %server.id, key = %key_id, old = %server.key_id, "sandbox server renewed its key");
    Ok(Json(wire::KeyAnswer { key_id, not_after }))
}

// ── Commands ───────────────────────────────────────────────────────────────

/// How long a command may wait for its server, and then for its answer.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Kinds a team whose plan holds commands back still gets: reads and
/// removals (gate.rs).
const ALLOWED_WHEN_HELD: &[&str] = &["get", "list", "report", "template", "delete", "closePort", "deleteTemplate"];

/// A command's `type`.
pub fn kind_of(command: &DaemonCommand) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Queues a command for an outbound server of `team_id`, sealed with the
/// team's data key (vault.rs). Its id.
/// Wake the server after the transaction commits (`AppState::wake` with
/// the server's id). A command past [`wire::MAX_COMMAND`] is refused.
pub async fn enqueue(
    conn: &mut PgConnection,
    server_id: &str,
    team_id: &str,
    command: &DaemonCommand,
) -> ApiResult<String> {
    let json = serde_json::to_string(command).map_err(anyhow::Error::from)?;
    if json.len() > wire::MAX_COMMAND {
        return Err(ApiError::bad_request(TOO_BIG));
    }
    let sealed = crate::vault::get().seal_credential(team_id, &json).await?;
    let id = crate::ids::new_id("sbc");
    let n = sqlx::query(
        "INSERT INTO pyo.sandbox_commands (id, server_id, team_id, kind, command_sealed)
         SELECT $1, s.id, s.team_id, $4, $5 FROM pyo.sandbox_servers s
          WHERE s.id = $2 AND s.team_id = $3 AND s.outbound",
    )
    .bind(&id)
    .bind(server_id)
    .bind(team_id)
    .bind(kind_of(command))
    .bind(sealed)
    .execute(conn)
    .await?
    .rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("Sandbox server"));
    }
    Ok(id)
}

/// Why a command can't go to a server over its channel.
pub const TOO_BIG: &str =
    "That's too big to send to the sandbox server through Pyo. Send it from a device on your tailnet.";

fn error_result(code: ErrorCode, message: &str) -> DaemonResult {
    DaemonResult::Error { error: DaemonError::new(code, message) }
}

/// Fails a command that waits or is out, with `why`.
async fn fail(conn: &mut PgConnection, server_id: &str, id: &str, why: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE pyo.sandbox_commands SET status = 'failed', result = $3, command_sealed = NULL, done_at = now()
          WHERE id = $1 AND server_id = $2 AND status IN ('pending', 'sent')",
    )
    .bind(id)
    .bind(server_id)
    .bind(json!(error_result(ErrorCode::Runtime, why)))
    .execute(conn)
    .await?;
    Ok(())
}

/// Fails every command of a server that waits or is out, with `why`.
pub async fn fail_open(conn: &mut PgConnection, server_id: &str, why: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE pyo.sandbox_commands SET status = 'failed', result = $2, command_sealed = NULL, done_at = now()
          WHERE server_id = $1 AND status IN ('pending', 'sent')",
    )
    .bind(server_id)
    .bind(json!(error_result(ErrorCode::Runtime, why)))
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PollQuery {
    /// Seconds to hold the request open when there's nothing to do.
    wait: Option<u64>,
}

/// The long poll: the server's waiting commands, signed, in the order they
/// were queued (at most 20), or none once `wait` passed.
async fn poll(
    State(s): State<AppState>,
    Extension(signer): Extension<Signer>,
    server: SandboxServer,
    headers: HeaderMap,
    Query(q): Query<PollQuery>,
) -> ApiResult<Json<Vec<wire::CommandEnvelope>>> {
    let Some(signer) = signer.0 else {
        return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, super::NO_SIGNER));
    };
    let wait = Duration::from_secs(q.wait.unwrap_or(25).min(55));
    let instance: String = headers
        .get(wire::INSTANCE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .take(64)
        .collect();
    // Commands handed to an earlier process of this daemon went with it:
    // they fail rather than run twice.
    if !instance.is_empty() {
        sqlx::query(
            "UPDATE pyo.sandbox_commands SET status = 'failed', result = $4, command_sealed = NULL, done_at = now()
              WHERE server_id = $1 AND team_id = $2 AND status = 'sent' AND sent_to IS DISTINCT FROM $3",
        )
        .bind(&server.id)
        .bind(&server.team_id)
        .bind(&instance)
        .bind(json!(error_result(ErrorCode::Runtime, "The sandbox server restarted before it answered.")))
        .execute(&s.db)
        .await?;
    }
    let deadline = tokio::time::Instant::now() + wait;
    let waker = s.waker(&server.id).await;
    loop {
        let notified = waker.notified();
        let out = take(&s, signer.as_ref(), &server, &instance).await?;
        if !out.is_empty() {
            return Ok(Json(out));
        }
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() || s.stopping() {
            return Ok(Json(vec![]));
        }
        let _ = tokio::time::timeout(left.min(Duration::from_secs(5)), notified).await;
    }
}

/// Takes the server's waiting commands, opens and signs them.
async fn take(
    s: &AppState,
    signer: &dyn CommandSigner,
    server: &SandboxServer,
    instance: &str,
) -> ApiResult<Vec<wire::CommandEnvelope>> {
    let mut tx = team_tx(s, server).await?;
    let mut rows: Vec<(String, String, Option<String>, i64)> = sqlx::query_as(
        "UPDATE pyo.sandbox_commands SET status = 'sent', sent_at = now(), sent_to = nullif($3, '')
          WHERE id IN (SELECT id FROM pyo.sandbox_commands WHERE server_id = $1 AND team_id = $2 AND status = 'pending'
                        ORDER BY created_at LIMIT 20 FOR UPDATE SKIP LOCKED)
          RETURNING id, kind, command_sealed, (extract(epoch FROM created_at) * 1000000)::bigint",
    )
    .bind(&server.id)
    .bind(&server.team_id)
    .bind(instance)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    if rows.is_empty() {
        return Ok(vec![]);
    }
    rows.sort_by_key(|r| r.3);
    let held = crate::gate::held_back(&s.db, &server.team_id).await?;
    let mut c = s.db.acquire().await?;
    let mut ready: Vec<(String, String, String)> = Vec::with_capacity(rows.len());
    for (id, kind, sealed, _) in rows {
        if let Some(why) = &held
            && !ALLOWED_WHEN_HELD.contains(&kind.as_str())
        {
            fail(&mut c, &server.id, &id, why).await?;
            crate::gate::mark_held(&s.db, &server.team_id).await;
            continue;
        }
        let opened = match sealed.as_deref() {
            Some(x) => crate::vault::get().open_credential(&s.sealer, x).await.ok(),
            None => None,
        };
        match opened {
            Some(json) => ready.push((id, kind, json)),
            _ => {
                tracing::error!(server = %server.id, %id, "can't open a sandbox command");
                fail(&mut c, &server.id, &id, "Pyo couldn't open it.").await?;
            }
        }
    }
    if ready.is_empty() {
        return Ok(vec![]);
    }
    let items: Vec<crate::ToSign> = ready
        .iter()
        .map(|(id, kind, json)| crate::ToSign {
            id: id.clone(),
            kind: kind.clone(),
            sha256: sha256_hex(json),
            command: None,
        })
        .collect();
    match signer.sign_sandbox(&server.id, &server.team_id, &items).await {
        Ok(sigs) if sigs.len() == ready.len() => {
            let mut out = Vec::with_capacity(ready.len());
            for ((id, kind, json), sig) in ready.into_iter().zip(sigs) {
                if let Some(why) = sig.refused {
                    tracing::warn!(server = %server.id, %id, %why, "the signer refused a sandbox command");
                    fail(&mut c, &server.id, &id, &format!("Pyo's signer refused it: {why}")).await?;
                    continue;
                }
                out.push(wire::CommandEnvelope {
                    id,
                    kind,
                    signed: pyo_proto::SignedCommandWire {
                        claims: sig.claims,
                        sig: sig.sig,
                        kid: sig.kid,
                        command: json,
                        delegation: sig.delegation,
                    },
                });
            }
            Ok(out)
        }
        other => {
            let why = match other {
                Ok(_) => "the signer signed a different number of commands".to_owned(),
                Err(e) => format!("{e:#}"),
            };
            tracing::error!(server = %server.id, error = %why, "couldn't sign sandbox commands; they go back to the queue");
            let ids: Vec<&str> = ready.iter().map(|r| r.0.as_str()).collect();
            sqlx::query(
                "UPDATE pyo.sandbox_commands SET status = 'pending', sent_at = NULL, sent_to = NULL
                  WHERE server_id = $1 AND id = ANY($2) AND status = 'sent'",
            )
            .bind(&server.id)
            .bind(&ids)
            .execute(&mut *c)
            .await?;
            Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Commands can't be signed right now."))
        }
    }
}

/// A command's answer: only for a command sent to this server.
async fn result(
    State(s): State<AppState>,
    server: SandboxServer,
    Path(id): Path<String>,
    Json(r): Json<DaemonResult>,
) -> ApiResult<Json<Value>> {
    let ok = !matches!(r, DaemonResult::Error { .. });
    let mut tx = team_tx(&s, &server).await?;
    let n = sqlx::query(
        "UPDATE pyo.sandbox_commands SET status = $4, result = $5, command_sealed = NULL, done_at = now()
          WHERE id = $1 AND server_id = $2 AND team_id = $3 AND status = 'sent'",
    )
    .bind(&id)
    .bind(&server.id)
    .bind(&server.team_id)
    .bind(if ok { "done" } else { "failed" })
    .bind(json!(r))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    if n == 0 {
        return Err(ApiError::not_found("Command"));
    }
    s.wake(&id).await;
    Ok(Json(json!({ "ok": true })))
}

/// The server's report: its capability, capacity, templates and sandboxes,
/// kept as the runner keeps a self-hosted server's (runner.rs).
async fn report(
    State(s): State<AppState>,
    server: SandboxServer,
    Json(r): Json<ServerReport>,
) -> ApiResult<Json<Value>> {
    if r.server_id != server.id {
        return Err(ApiError::bad_request("This report is for another sandbox server."));
    }
    let mut tx = team_tx(&s, &server).await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT address, state FROM pyo.sandbox_servers WHERE id = $1 AND team_id = $2 AND outbound AND address IS NOT NULL",
    )
    .bind(&server.id)
    .bind(&server.team_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (address, state) = row.ok_or_else(|| ApiError::not_found("Sandbox server"))?;
    let srv = super::super::runner::Server {
        id: server.id.clone(),
        team_id: Some(server.team_id.clone()),
        address,
        state,
    };
    super::super::runner::apply_in(&mut tx, &srv, &r).await?;
    tx.commit().await?;
    // Templates it lacks, and ones Pyo dropped, as the runner does for a
    // server it calls. Through the channel, so not while this answers.
    tokio::spawn(sync_templates(s.clone(), server, srv.address, r));
    Ok(Json(json!({ "ok": true })))
}

/// Servers whose templates are being synced now.
static SYNCING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(Default::default);

/// After an outbound server's report: asks it to prepare the templates it
/// lacks and delete the ones Pyo dropped. One at a time per server: a
/// report that comes in meanwhile is skipped.
async fn sync_templates(s: AppState, server: SandboxServer, address: String, r: ServerReport) {
    if !SYNCING.lock().unwrap_or_else(|p| p.into_inner()).insert(server.id.clone()) {
        return;
    }
    let res = async {
        let key = crate::sandbox::keys::signing_key(&s.db, &s.sealer)
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
        crate::sandbox::templates::sync_server(&s.db, &key, &server.id, Some(&server.team_id), &address, &r).await?;
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = res {
        tracing::warn!(server = %server.id, error = %format!("{e:#}"), "sandboxes: couldn't sync an outbound server's templates");
    }
    SYNCING.lock().unwrap_or_else(|p| p.into_inner()).remove(&server.id);
}

// ── Asking ─────────────────────────────────────────────────────────────────

/// Queues `command` for an outbound server and waits up to `wait` for its
/// answer. One still waiting then is taken back. One out with the server
/// keeps going, and the answer says it didn't come in time.
pub async fn ask(
    s: &AppState,
    server_id: &str,
    command: &DaemonCommand,
    wait: Duration,
) -> Result<DaemonResult, DaemonError> {
    let internal = |e: ApiError| DaemonError::new(ErrorCode::Internal, e.message);
    let team: Option<String> = sqlx::query_scalar(
        "SELECT s.team_id FROM pyo.sandbox_servers s WHERE s.id = $1 AND s.outbound
            AND EXISTS (SELECT 1 FROM pyo.sandbox_server_keys k
                         WHERE k.server_id = s.id AND k.revoked_at IS NULL AND k.expires_at > now())",
    )
    .bind(server_id)
    .fetch_optional(&s.db)
    .await
    .map_err(|e| internal(e.into()))?
    .flatten();
    let Some(team) = team else {
        return Err(DaemonError::new(ErrorCode::Runtime, "The sandbox server hasn't joined yet."));
    };
    let mut c = s.db.acquire().await.map_err(|e| internal(e.into()))?;
    let id = enqueue(&mut c, server_id, &team, command).await.map_err(|e| {
        if e.message == TOO_BIG {
            DaemonError::new(ErrorCode::InvalidSpec, TOO_BIG)
        } else {
            internal(e)
        }
    })?;
    drop(c);
    s.wake(server_id).await;
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT status, result FROM pyo.sandbox_commands WHERE id = $1")
                .bind(&id)
                .fetch_optional(&s.db)
                .await
                .map_err(|e| internal(e.into()))?;
        match row {
            Some((st, Some(r))) if st == "done" || st == "failed" => {
                return serde_json::from_value::<DaemonResult>(r).map_err(|e| {
                    DaemonError::new(ErrorCode::Internal, format!("The sandbox server's answer didn't parse: {e}"))
                });
            }
            None => return Err(DaemonError::new(ErrorCode::Runtime, "The command is gone.")),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = sqlx::query("DELETE FROM pyo.sandbox_commands WHERE id = $1 AND status = 'pending'")
                .bind(&id)
                .execute(&s.db)
                .await;
            return Err(DaemonError::new(ErrorCode::Runtime, "The sandbox server didn't answer in time."));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// ── Housekeeping ───────────────────────────────────────────────────────────

/// From the sandbox runner: commands that waited too long fail, answered
/// ones go after a day, keys long past their end go, and outbound servers
/// whose report is old turn unreachable.
pub async fn sweep(db: &sqlx::PgPool) -> sqlx::Result<()> {
    let secs = COMMAND_TIMEOUT.as_secs_f64();
    sqlx::query(
        "UPDATE pyo.sandbox_commands SET status = 'failed', result = $1, command_sealed = NULL, done_at = now()
          WHERE status = 'pending' AND created_at < now() - make_interval(secs => $2)",
    )
    .bind(json!(error_result(ErrorCode::Runtime, "The sandbox server didn't take it in time.")))
    .bind(secs)
    .execute(db)
    .await?;
    sqlx::query(
        "UPDATE pyo.sandbox_commands SET status = 'failed', result = $1, command_sealed = NULL, done_at = now()
          WHERE status = 'sent' AND sent_at < now() - make_interval(secs => $2)",
    )
    .bind(json!(error_result(ErrorCode::Runtime, "The sandbox server didn't answer in time.")))
    .bind(secs)
    .execute(db)
    .await?;
    sqlx::query("DELETE FROM pyo.sandbox_commands WHERE done_at < now() - interval '1 day'")
        .execute(db)
        .await?;
    sqlx::query(
        "DELETE FROM pyo.sandbox_server_keys WHERE coalesce(revoked_at, expires_at) < now() - interval '1 day'",
    )
    .execute(db)
    .await?;
    let gone: Vec<String> = sqlx::query_scalar(
        "UPDATE pyo.sandbox_servers SET state = 'unreachable', error = $1, updated_at = now()
          WHERE outbound AND state = 'ready'
            AND coalesce(last_report_at, joined_at, created_at) < now() - make_interval(secs => $2)
          RETURNING id",
    )
    .bind("No report from the server for a while.")
    .bind(super::super::runner::UNREACHABLE_SECS as f64)
    .fetch_all(db)
    .await?;
    for id in gone {
        tracing::warn!(server = %id, "outbound sandbox server is unreachable");
    }
    Ok(())
}
