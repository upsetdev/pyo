//! The dashboard's terminal: a shell in one of a service's replicas.
//!
//! The browser opens a WebSocket here; the service's cluster agent is asked to
//! open a shell and dial back to `/api/agent/v1/terminals/<id>`; this process
//! relays bytes between the two. With several web replicas the agent may land
//! on another one, which forwards its socket to the replica holding the
//! browser (`relay`, its pod address).

use crate::error::Query;
use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{
        Path, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, header},
    response::Response,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use pyo_proto::{Command, TERMINAL_SHELLS, TerminalFrame};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

use crate::{
    AppState,
    agent::AgentCluster,
    auth::{KeyAccess, SessionUser},
    error::{ApiError, ApiResult},
    ids::new_id,
    ops::{self, Role},
};

/// Longest a shell stays open.
const MAX_SESSION: Duration = Duration::from_secs(4 * 3600);
/// How long the browser waits for the agent to dial back.
const AGENT_WAIT: Duration = Duration::from_secs(20);

/// Sockets waiting for their agent's, by session id: terminals' and log
/// follows' (logfollow.rs).
pub type Waiting = Arc<Mutex<HashMap<String, oneshot::Sender<WebSocket>>>>;

pub fn routes() -> Router<AppState> {
    Router::new().route("/services/{id}/terminal", get(open))
}

pub fn agent_routes() -> Router<AppState> {
    Router::new().route("/terminals/{id}", get(agent_side))
}

fn text(f: &TerminalFrame) -> Message {
    Message::text(serde_json::to_string(f).unwrap_or_default())
}

/// A page on another site can open WebSockets with the user's cookie, so
/// browsers must come from this origin.
pub(crate) fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    origin.split_once("://").map(|(_, h)| h) == Some(host)
}

#[derive(Deserialize)]
struct OpenQuery {
    /// A replica (pod) name; any ready one when empty.
    #[serde(default)]
    replica: String,
    /// One of TERMINAL_SHELLS.
    #[serde(default)]
    shell: String,
    /// A command to run instead of a shell, as a JSON array of its words
    /// (`pyo exec -- cmd args`).
    #[serde(default)]
    command: String,
    /// "false": no TTY (separate output streams, for scripts and pipes).
    #[serde(default)]
    tty: String,
}

/// A command's words from the query (a JSON array), checked: at most 256
/// words and 64 KiB, no NUL bytes. Empty: a shell.
fn command_words(q: &str) -> ApiResult<Vec<String>> {
    if q.is_empty() {
        return Ok(vec![]);
    }
    let words: Vec<String> = serde_json::from_str(q)
        .map_err(|_| ApiError::bad_request("The command is a JSON array of strings."))?;
    if words.len() > 256
        || words.iter().map(String::len).sum::<usize>() > 64 * 1024
        || words.iter().any(|w| w.contains('\0'))
    {
        return Err(ApiError::bad_request("That command is too long."));
    }
    if words.first().is_some_and(|w| w.is_empty()) {
        return Err(ApiError::bad_request("Name the command to run."));
    }
    Ok(words)
}

/// GET /api/v1/services/{id}/terminal?replica=&shell=&command=&tty= (WebSocket).
async fn open(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    headers: HeaderMap,
    Path(service_id): Path<String>,
    Query(q): Query<OpenQuery>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let shell = if q.shell.is_empty() {
        "auto".to_owned()
    } else {
        q.shell
    };
    if !TERMINAL_SHELLS.contains(&shell.as_str()) {
        return Err(ApiError::bad_request(format!(
            "Pick a shell: {}.",
            TERMINAL_SHELLS.join(", ")
        )));
    }
    if q.replica.len() > 253
        || !q
            .replica
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        return Err(ApiError::bad_request("That isn't a replica name."));
    }
    let command = command_words(&q.command)?;
    let tty = q.tty != "false";
    if !same_origin(&headers) {
        return Err(ApiError::forbidden(
            "Open the terminal from the Pyo dashboard.",
        ));
    }
    // A shell needs a key with full access. One command (a migration from
    // CI) also runs with a deploy key.
    if let Some(k) = &user.key {
        let ok = match k.access {
            KeyAccess::Full => true,
            KeyAccess::Deploy => !command.is_empty(),
            _ => false,
        };
        if !ok {
            return Err(ApiError::forbidden(if command.is_empty() {
                "Terminals need an API key with full access."
            } else {
                "Running a command needs an API key with deploy or full access."
            }));
        }
    }
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    let team_id = ops::team_of(&mut tx, "services", &service_id).await?;
    // A shell can read the service's secrets and change its files: developers and up.
    ops::require_role(&mut tx, &user, &team_id, Role::Developer).await?;
    let (cluster_id, status): (String, String) =
        sqlx::query_as("SELECT cluster_id, status FROM pyo.services WHERE id = $1")
            .bind(&service_id)
            .fetch_one(&mut *tx)
            .await?;
    if status == "stopped" {
        return Err(ApiError::conflict(
            "The service is stopped, so there's nothing to open a terminal in. Start it first.",
        ));
    }
    // The cluster's owner may have turned terminals off (pyo-system/pyo-policy);
    // the agent refuses them then, so say so now rather than wait for it.
    let policy: Option<Value> = sqlx::query_scalar(
        "SELECT capabilities->'agentScope'->'policy' FROM pyo.clusters WHERE id = $1",
    )
    .bind(&cluster_id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(p) =
        policy.filter(|p| p["noTerminal"] == json!(true) || p["readOnly"] == json!(true))
    {
        let why = if p["readOnly"] == json!(true) {
            "This cluster's policy (pyo-system/pyo-policy) is read only, so terminals are off."
        } else {
            "This cluster's policy (pyo-system/pyo-policy) turns terminals off."
        };
        return Err(ApiError::forbidden(why));
    }
    let service = ops::service_ref(&mut tx, &service_id).await?;
    let id = new_id("trm");
    sqlx::query("INSERT INTO pyo.terminal_sessions (id, team_id, service_id, user_id, relay, pod) VALUES ($1, $2, $3, $4, $5, nullif($6, ''))")
        .bind(&id)
        .bind(&team_id)
        .bind(&service_id)
        .bind(&user.id)
        .bind(&s.config.relay_addr)
        .bind(&q.replica)
        .execute(&mut *tx)
        .await?;
    // Not the command's words: they may hold secrets.
    let what = if command.is_empty() {
        format!("Opened a terminal in {}", service.name)
    } else {
        format!("Ran a command in {}", service.name)
    };
    ops::log_activity(
        &mut tx,
        &team_id,
        &user.name,
        "service.terminal",
        "info",
        &what,
        json!({ "kind": "service", "id": service_id, "label": service.name }),
        json!({ "serviceId": service_id }),
    )
    .await?;
    let (send, recv) = oneshot::channel();
    s.terminals.lock().await.insert(id.clone(), send);
    crate::deploy::enqueue(
        &mut tx,
        &cluster_id,
        Command::OpenTerminal {
            session_id: id.clone(),
            service,
            pod: q.replica,
            shell,
            command,
            tty,
        },
    )
    .await?;
    tx.commit().await?;
    s.wake(&cluster_id).await;

    Ok(ws.on_upgrade(move |browser| async move {
        let agent = tokio::time::timeout(AGENT_WAIT, recv).await;
        s.terminals.lock().await.remove(&id);
        match agent {
            Ok(Ok(agent)) => {
                let _ = tokio::time::timeout(MAX_SESSION, relay(browser, agent)).await;
            }
            _ => {
                let mut browser = browser;
                let _ = browser
                    .send(text(&TerminalFrame::Error {
                        message: "The cluster's agent didn't answer. Is the server online?".into(),
                    }))
                    .await;
                let _ = browser.close().await;
            }
        }
        let _ = sqlx::query("UPDATE pyo.terminal_sessions SET ended_at = now() WHERE id = $1")
            .bind(&id)
            .execute(&s.db)
            .await;
    }))
}

/// GET /api/agent/v1/terminals/{id} (WebSocket): the agent's end.
async fn agent_side(
    State(s): State<AppState>,
    cluster: AgentCluster,
    headers: HeaderMap,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT t.relay FROM pyo.terminal_sessions t JOIN pyo.services a ON a.id = t.service_id
          WHERE t.id = $1 AND a.cluster_id = $2 AND t.ended_at IS NULL AND t.created_at > now() - interval '1 minute'",
    )
    .bind(&id)
    .bind(&cluster.id)
    .fetch_optional(&s.db)
    .await?;
    let Some((relay_addr,)) = row else {
        return Err(ApiError::not_found("Terminal session"));
    };
    hand_over(
        &s,
        &headers,
        &format!("terminals/{id}"),
        &id,
        relay_addr,
        ws,
        "Terminal session",
    )
    .await
}

/// The agent's socket for session `id`, to whoever waits for it: on this
/// replica, or forwarded to `relay_addr`, the replica holding the other
/// end (at `/api/agent/v1/<path>` there). Terminals and log follows.
pub(crate) async fn hand_over(
    s: &AppState,
    headers: &HeaderMap,
    path: &str,
    id: &str,
    relay_addr: Option<String>,
    ws: WebSocketUpgrade,
    what: &str,
) -> ApiResult<Response> {
    let local = s.terminals.lock().await.remove(id);
    if let Some(waiting) = local {
        return Ok(ws.on_upgrade(move |agent| async move {
            let _ = waiting.send(agent);
        }));
    }
    // The other end is on another replica: forward this socket there.
    let Some(addr) = relay_addr.filter(|a| Some(a) != s.config.relay_addr.as_ref()) else {
        return Err(ApiError::not_found(what));
    };
    let mut req = format!("ws://{addr}/api/agent/v1/{path}")
        .into_client_request()
        .map_err(anyhow::Error::from)?;
    if let Some(auth) = headers.get(header::AUTHORIZATION) {
        req.headers_mut()
            .insert(header::AUTHORIZATION, auth.clone());
    }
    let (upstream, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| {
        ApiError::from(anyhow::anyhow!(
            "couldn't reach the replica holding this session: {e}"
        ))
    })?;
    Ok(ws.on_upgrade(move |agent| forward(agent, upstream)))
}

/// Shuttle frames between the browser and the agent until either side closes.
pub(crate) async fn relay(browser: WebSocket, agent: WebSocket) {
    let (mut b_tx, mut b_rx) = browser.split();
    let (mut a_tx, mut a_rx) = agent.split();
    let up = async {
        while let Some(Ok(m)) = b_rx.next().await {
            if matches!(m, Message::Close(_)) || a_tx.send(m).await.is_err() {
                break;
            }
        }
    };
    let down = async {
        while let Some(Ok(m)) = a_rx.next().await {
            if matches!(m, Message::Close(_)) || b_tx.send(m).await.is_err() {
                break;
            }
        }
    };
    tokio::select! { _ = up => {}, _ = down => {} }
    let _ = a_tx.close().await;
    let _ = b_tx.close().await;
}

/// The cross-replica hop: an axum socket on one side, a client on the other.
async fn forward(
    agent: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut a_tx, mut a_rx) = agent.split();
    let (mut u_tx, mut u_rx) = upstream.split();
    let up = async {
        while let Some(Ok(m)) = a_rx.next().await {
            let m = match m {
                Message::Binary(b) => tungstenite::Message::Binary(b),
                Message::Text(t) => tungstenite::Message::text(t.as_str()),
                Message::Close(_) => break,
                _ => continue,
            };
            if u_tx.send(m).await.is_err() {
                break;
            }
        }
    };
    let down = async {
        while let Some(Ok(m)) = u_rx.next().await {
            let m = match m {
                tungstenite::Message::Binary(b) => Message::Binary(b),
                tungstenite::Message::Text(t) => Message::text(t.as_str()),
                tungstenite::Message::Close(_) => break,
                _ => continue,
            };
            if a_tx.send(m).await.is_err() {
                break;
            }
        }
    };
    tokio::select! { _ = up => {}, _ = down => {} }
    let _ = u_tx.close().await;
    let _ = a_tx.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        let h = |origin: Option<&str>| {
            let mut m = HeaderMap::new();
            m.insert(header::HOST, "localhost:8000".parse().unwrap());
            if let Some(o) = origin {
                m.insert(header::ORIGIN, o.parse().unwrap());
            }
            m
        };
        assert!(same_origin(&h(Some("http://localhost:8000"))));
        assert!(same_origin(&h(None)));
        assert!(!same_origin(&h(Some("https://evil.example"))));
    }

    #[test]
    fn command_words_are_checked() {
        assert_eq!(command_words("").unwrap(), Vec::<String>::new());
        assert_eq!(command_words(r#"["ls","-la"]"#).unwrap(), ["ls", "-la"]);
        assert!(command_words("ls -la").is_err());
        assert!(command_words(r#"[""]"#).is_err());
        assert!(command_words(r#"["a\u0000b"]"#).is_err());
        let long = serde_json::to_string(&vec!["x"; 300]).unwrap();
        assert!(command_words(&long).is_err());
    }
}
