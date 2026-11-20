//! The dashboard's terminal: a shell in one of a service's replicas.
//!
//! The browser opens a WebSocket here; the service's cluster agent is asked to
//! open a shell and dial back to `/api/agent/v1/terminals/<id>`; this process
//! relays bytes between the two. With several web replicas the agent may land
//! on another one, which forwards its socket to the replica holding the
//! browser (`relay`, its pod address) with a relay ticket ([`hand_over`]).

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
/// The header a forwarded agent socket carries its relay ticket in.
const RELAY_TICKET: &str = "x-pyo-relay-ticket";

/// Sockets waiting for their agent's, by session id: terminals' and log
/// follows' (logfollow.rs).
pub type Waiting = Arc<Mutex<HashMap<String, oneshot::Sender<WebSocket>>>>;

pub fn routes() -> Router<AppState> {
    Router::new().route("/services/{id}/terminal", get(open))
}

pub fn agent_routes() -> Router<AppState> {
    Router::new()
        .route("/terminals/{id}", get(agent_side))
        .route("/relay/{id}", get(relayed))
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
    hand_over(&s, &id, relay_addr, ws, "Terminal session").await
}

/// The agent's socket for session `id`, to whoever waits for it: on this
/// replica, or forwarded to `relay_addr`, the replica holding the other
/// end. Terminals and log follows.
///
/// Only this replica can check the agent: a signed request's nonce is
/// spent here, and a client certificate ends here. So the hop carries a
/// relay ticket instead, a random secret good once for a minute, whose
/// hash this replica leaves in the database for the other one to take
/// ([`relayed`]). Whatever the agent sent is never passed on.
pub(crate) async fn hand_over(
    s: &AppState,
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
    let ticket = crate::ids::random_string(48);
    sqlx::query("INSERT INTO pyo.relay_tickets (hash, session_id) VALUES ($1, $2)")
        .bind(crate::ids::sha256_hex(&ticket))
        .bind(id)
        .execute(&s.db)
        .await?;
    let _ =
        sqlx::query("DELETE FROM pyo.relay_tickets WHERE created_at < now() - interval '1 minute'")
            .execute(&s.db)
            .await;
    let mut req = format!("ws://{addr}/api/agent/v1/relay/{id}")
        .into_client_request()
        .map_err(anyhow::Error::from)?;
    req.headers_mut().insert(
        RELAY_TICKET,
        header::HeaderValue::from_str(&ticket).map_err(anyhow::Error::from)?,
    );
    let (upstream, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| {
        ApiError::from(anyhow::anyhow!(
            "couldn't reach the replica holding this session: {e}"
        ))
    })?;
    Ok(ws.on_upgrade(move |agent| forward(agent, upstream)))
}

/// GET /api/agent/v1/relay/{id} (WebSocket): an agent's socket another
/// replica checked and forwarded here ([`hand_over`]), with its ticket.
/// Handed to whoever waits for it on this replica, never forwarded again.
async fn relayed(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let ticket = headers
        .get(RELAY_TICKET)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let taken = !ticket.is_empty()
        && sqlx::query(
            "DELETE FROM pyo.relay_tickets WHERE hash = $1 AND session_id = $2 AND created_at > now() - interval '1 minute'",
        )
        .bind(crate::ids::sha256_hex(ticket))
        .bind(&id)
        .execute(&s.db)
        .await?
        .rows_affected()
            == 1;
    if !taken {
        return Err(ApiError::new(
            axum::http::StatusCode::UNAUTHORIZED,
            "This relay ticket is unknown or was used.",
        ));
    }
    let Some(waiting) = s.terminals.lock().await.remove(&id) else {
        return Err(ApiError::not_found("Session"));
    };
    Ok(ws.on_upgrade(move |agent| async move {
        let _ = waiting.send(agent);
    }))
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

    /// A web replica on a port of its own, like a pod: the agent's routes
    /// behind the signed request check.
    async fn replica(db: &sqlx::PgPool) -> (AppState, String) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let mut s = crate::rls::tests::state(db.clone());
        let mut config = (*s.config).clone();
        config.relay_addr = Some(addr.clone());
        s.config = Arc::new(config);
        let app = Router::new()
            .nest(
                "/api/agent/v1",
                agent_routes()
                    .merge(crate::logfollow::agent_routes())
                    .layer(axum::middleware::from_fn_with_state(
                        s.clone(),
                        crate::agent_auth::signed,
                    )),
            )
            .with_state(s.clone());
        tokio::spawn(async move { axum::serve(l, app).await });
        (s, addr)
    }

    /// Against a throwaway Postgres: an agent's signed socket (no bearer
    /// token, as on Pyo Cloud) lands on another web replica than the one
    /// holding the reader. It reaches the reader all the same, and the
    /// hop's ticket works once and can't be made up.
    #[tokio::test]
    #[ignore = "needs DATABASE_URL (a throwaway Postgres)"]
    async fn a_signed_agent_socket_reaches_the_reader_on_another_replica() {
        let db = sqlx::PgPool::connect(&std::env::var("DATABASE_URL").unwrap())
            .await
            .unwrap();
        crate::migrate::run(&db).await.unwrap();
        let run = crate::ids::random_string(8);
        let (team, cluster, project, service) = (
            format!("team_r{run}"),
            format!("cl_r{run}"),
            format!("prj_r{run}"),
            format!("svc_r{run}"),
        );
        let q = |sql: &'static str| sqlx::query(sql);
        q("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        q("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&db).await.unwrap();
        q("INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $1)")
            .bind(&project)
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        q("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status) VALUES ($1, $2, $3, $4, 'web', '{}', 'running')").bind(&service).bind(&team).bind(&project).bind(&cluster).execute(&db).await.unwrap();
        let key = pyo_proto::agentauth::RequestKey::from_pkcs8(
            &pyo_proto::agentauth::RequestKey::generate_pkcs8().unwrap(),
        )
        .unwrap();
        let key_id = format!("akey_r{run}");
        q("INSERT INTO pyo.agent_keys (id, cluster_id, public_key, expires_at) VALUES ($1, $2, $3, now() + interval '1 hour')").bind(&key_id).bind(&cluster).bind(key.public_key()).execute(&db).await.unwrap();

        let (_a, a_addr) = replica(&db).await;
        let (b, b_addr) = replica(&db).await;
        // The reader follows on B.
        let follow = new_id("lgf");
        q("INSERT INTO pyo.log_follows (id, team_id, service_id, relay) VALUES ($1, $2, $3, $4)")
            .bind(&follow)
            .bind(&team)
            .bind(&service)
            .bind(&b_addr)
            .execute(&db)
            .await
            .unwrap();
        let (send, recv) = oneshot::channel();
        b.terminals.lock().await.insert(follow.clone(), send);

        // The agent dials A.
        let path = format!("/api/agent/v1/log-follows/{follow}");
        let mut req = format!("ws://{a_addr}{path}")
            .into_client_request()
            .unwrap();
        for (h, v) in key.sign(&key_id, "GET", &path, b"", pyo_proto::now_ms() / 1000) {
            req.headers_mut().insert(h, v.parse().unwrap());
        }
        let (mut agent, _) = tokio_tungstenite::connect_async(req)
            .await
            .expect("the agent's socket is taken");
        let mut reader_end = tokio::time::timeout(Duration::from_secs(5), recv)
            .await
            .expect("B gets the agent's socket")
            .unwrap();
        let hello = r#"{"type":"lines","lines":[]}"#;
        agent.send(tungstenite::Message::text(hello)).await.unwrap();
        match reader_end.recv().await {
            Some(Ok(Message::Text(t))) => assert_eq!(t.as_str(), hello),
            other => panic!("the agent's frame, not {other:?}"),
        }

        // The ticket went with the hop, and one made up gets nothing.
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pyo.relay_tickets WHERE session_id = $1")
                .bind(&follow)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(left, 0);
        let (send, _recv) = oneshot::channel();
        b.terminals.lock().await.insert(follow.clone(), send);
        let mut forged = format!("ws://{b_addr}/api/agent/v1/relay/{follow}")
            .into_client_request()
            .unwrap();
        forged
            .headers_mut()
            .insert(RELAY_TICKET, "made-up".parse().unwrap());
        assert!(tokio_tungstenite::connect_async(forged).await.is_err());
        assert!(
            b.terminals.lock().await.contains_key(&follow),
            "the reader still waits for its agent"
        );

        q("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
    }
}
