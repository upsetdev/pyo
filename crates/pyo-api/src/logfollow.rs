//! Following a service's logs: `pyo logs -f` and the dashboard's live logs.
//!
//! One signed command (`follow_logs`) per follow, not one every few
//! seconds: the reader opens a WebSocket here, the service's cluster agent
//! is asked to follow its replicas and dial back to
//! `/api/agent/v1/log-follows/<id>`, and this process relays the agent's
//! frames ([`pyo_proto::LogFrame`]) to the reader. The agent stops when the
//! reader goes, or after [`pyo_proto::LOG_FOLLOW_SECS`], when the reader
//! opens another. With several web replicas the agent's socket is
//! forwarded to the one holding the reader's, as a terminal's is
//! (terminal.rs `hand_over`).

use std::time::Duration;

use axum::{
    Router,
    extract::{
        Path, State,
        ws::{Message, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
    routing::get,
};
use futures::SinkExt;
use pyo_proto::{Command, LOG_FOLLOW_SECS, LogFrame};
use tokio::sync::oneshot;

use crate::{
    AppState,
    agent::AgentCluster,
    auth::SessionUser,
    error::{ApiError, ApiResult, Query},
    ids::new_id,
    ops::{self, LogsQuery, Role},
    terminal,
};

/// How long the reader waits for the agent to dial back.
const AGENT_WAIT: Duration = Duration::from_secs(20);

pub fn routes() -> Router<AppState> {
    Router::new().route("/services/{id}/logs/follow", get(open))
}

pub fn agent_routes() -> Router<AppState> {
    Router::new().route("/log-follows/{id}", get(agent_side))
}

/// GET /api/v1/services/{id}/logs/follow?since=&replica=&tail= (WebSocket).
async fn open(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    headers: HeaderMap,
    Path(service_id): Path<String>,
    Query(q): Query<LogsQuery>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    if !terminal::same_origin(&headers) {
        return Err(ApiError::forbidden("Follow logs from the Pyo dashboard."));
    }
    let replica = ops::replica_name(q.replica)?;
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let team_id = ops::team_of(&mut tx, "services", &service_id).await?;
    ops::require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    let cluster_id: String =
        sqlx::query_scalar("SELECT cluster_id FROM pyo.services WHERE id = $1")
            .bind(&service_id)
            .fetch_one(&mut *tx)
            .await?;
    let service = ops::service_ref(&mut tx, &service_id).await?;
    let id = new_id("lgf");
    sqlx::query(
        "INSERT INTO pyo.log_follows (id, team_id, service_id, relay) VALUES ($1, $2, $3, $4)",
    )
    .bind(&id)
    .bind(&team_id)
    .bind(&service_id)
    .bind(&s.config.relay_addr)
    .execute(&mut *tx)
    .await?;
    crate::deploy::enqueue(
        &mut tx,
        &cluster_id,
        Command::FollowLogs {
            session_id: id.clone(),
            service,
            tail_lines: q.tail.unwrap_or(300).clamp(1, 2000),
            since_seconds: ops::since_seconds(q.since, pyo_proto::now_ms()),
            replica,
        },
    )
    .await?;
    let (send, recv) = oneshot::channel();
    s.terminals.lock().await.insert(id.clone(), send);
    if let Err(e) = tx.commit().await {
        s.terminals.lock().await.remove(&id);
        return Err(e.into());
    }
    s.wake(&cluster_id).await;
    // Follows that ended a day ago are of no more use.
    let _ = sqlx::query("DELETE FROM pyo.log_follows WHERE created_at < now() - interval '1 day'")
        .execute(&s.db)
        .await;

    Ok(ws.on_upgrade(move |reader| async move {
        let agent = tokio::time::timeout(AGENT_WAIT, recv).await;
        s.terminals.lock().await.remove(&id);
        match agent {
            Ok(Ok(agent)) => {
                let cap = Duration::from_secs(LOG_FOLLOW_SECS + 30);
                let _ = tokio::time::timeout(cap, terminal::relay(reader, agent)).await;
            }
            _ => {
                let mut reader = reader;
                let frame = LogFrame::Error {
                    message: "The cluster's agent didn't answer. Is the server online?".into(),
                };
                let _ = reader
                    .send(Message::text(
                        serde_json::to_string(&frame).unwrap_or_default(),
                    ))
                    .await;
                let _ = reader.close().await;
            }
        }
        let _ = sqlx::query("UPDATE pyo.log_follows SET ended_at = now() WHERE id = $1")
            .bind(&id)
            .execute(&s.db)
            .await;
    }))
}

/// GET /api/agent/v1/log-follows/{id} (WebSocket): the agent's end.
async fn agent_side(
    State(s): State<AppState>,
    cluster: AgentCluster,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT f.relay FROM pyo.log_follows f JOIN pyo.services a ON a.id = f.service_id
          WHERE f.id = $1 AND a.cluster_id = $2 AND f.ended_at IS NULL AND f.created_at > now() - interval '1 minute'",
    )
    .bind(&id)
    .bind(&cluster.id)
    .fetch_optional(&s.db)
    .await?;
    let Some((relay_addr,)) = row else {
        return Err(ApiError::not_found("Log follow"));
    };
    terminal::hand_over(&s, &id, relay_addr, ws, "Log follow").await
}
