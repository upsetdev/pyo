//! The daemon's API. It listens only on the configured address, the
//! server's tailnet address in production. Every call except the health
//! check needs a token Pyo signed (`pyo_proto::sandbox::token`), in
//! `Authorization: Bearer <token>` or, for browsers opening the WebSocket,
//! `?token=<token>`.
//!
//! | Route | Scope |
//! |---|---|
//! | `GET /v1/health` | none |
//! | `GET /v1/report`, `GET /v1/sandboxes` | admin |
//! | `POST /v1/commands` | depends on the command |
//! | `POST /v1/sandboxes` | create, for that sandbox ID and team |
//! | `GET /v1/sandboxes/{id}` | any scope for the sandbox |
//! | `DELETE /v1/sandboxes/{id}`, `POST .../pause`, `.../resume`, `.../ttl` | lifecycle |
//! | `POST /v1/sandboxes/{id}/exec` | exec |
//! | `POST /v1/sandboxes/{id}/ports`, `DELETE .../ports/{port}` | ports |
//! | `GET PUT DELETE .../files`, `GET .../files/list`, `.../files/stat`, `POST .../files/mkdir` | files |
//! | `GET /v1/sandboxes/{id}/connect` (WebSocket) | any scope, then each request is checked |
//!
//! The WebSocket speaks the envd wire protocol, one frame per binary
//! message. The first message is envd's `Hello`. The daemon checks every
//! request against the token's scopes and drops anything that isn't a
//! request, stdin or EOF.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use pyo_proto::sandbox::token::{self, Claims, Expect, LEEWAY_SECS, TokenError, VerifyingKey};
use pyo_proto::sandbox::wire::{self, ErrorKind, Frame, Kind, Request, Scope};
use pyo_proto::sandbox::{
    DaemonCommand, DaemonError, DaemonResult, ErrorCode, ExecRequest, SandboxSpec,
};
use serde::Deserialize;

use crate::envd::{EnvdError, FileTooBig};
use crate::manager::Manager;

/// The most a files call moves in one go. Bigger files go over the
/// WebSocket.
pub const FILE_CAP: usize = 64 << 20;

pub struct AppState {
    pub mgr: Arc<Manager>,
    pub keys: Vec<VerifyingKey>,
    pub server_id: String,
}

type S = State<Arc<AppState>>;

pub struct ApiError(pub DaemonError);

impl From<DaemonError> for ApiError {
    fn from(e: DaemonError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, axum::Json(DaemonResult::Error { error: self.0 })).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn fail(code: ErrorCode, msg: impl Into<String>) -> ApiError {
    ApiError(DaemonError::new(code, msg))
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/report", get(report))
        .route("/v1/commands", post(command))
        .route("/v1/sandboxes", get(list).post(create))
        .route("/v1/sandboxes/{id}", get(get_one).delete(delete))
        .route("/v1/sandboxes/{id}/pause", post(pause))
        .route("/v1/sandboxes/{id}/resume", post(resume))
        .route("/v1/sandboxes/{id}/ttl", post(set_ttl))
        .route("/v1/sandboxes/{id}/exec", post(exec))
        .route("/v1/sandboxes/{id}/ports", post(open_port))
        .route(
            "/v1/sandboxes/{id}/ports/{port}",
            axum::routing::delete(close_port),
        )
        .route(
            "/v1/sandboxes/{id}/files",
            get(read_file)
                .put(write_file)
                .delete(remove_file)
                .layer(DefaultBodyLimit::max(FILE_CAP)),
        )
        .route("/v1/sandboxes/{id}/files/list", get(list_files))
        .route("/v1/sandboxes/{id}/files/stat", get(stat_file))
        .route("/v1/sandboxes/{id}/files/mkdir", post(mkdir))
        .route("/v1/sandboxes/{id}/connect", get(connect))
        .layer(axum::middleware::from_fn(cors))
        .with_state(state)
}

/// Lets the dashboard call the daemon from the browser. Every call carries
/// its token in a header or the query, never a cookie, so any origin may
/// ask. The preflight also allows a public page to reach the tailnet
/// address (Chrome's private network access).
async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let preflight = req.method() == axum::http::Method::OPTIONS;
    let mut res = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        header::HeaderValue::from_static("*"),
    );
    if preflight {
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::HeaderValue::from_static("GET, POST, PUT, DELETE"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            header::HeaderValue::from_static("authorization, content-type"),
        );
        h.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            header::HeaderValue::from_static("600"),
        );
        h.insert(
            "access-control-allow-private-network",
            header::HeaderValue::from_static("true"),
        );
    }
    res
}

// ── Auth ───────────────────────────────────────────────────────────────────

fn token_of(headers: &HeaderMap, query: &HashMap<String, String>) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_string())
        .or_else(|| query.get("token").cloned())
}

fn token_error(e: TokenError) -> ApiError {
    let code = if e.is_unauthenticated() {
        ErrorCode::Unauthorized
    } else {
        ErrorCode::Forbidden
    };
    fail(code, e.to_string())
}

impl AppState {
    fn expect<'a>(&'a self, sandbox: Option<&'a str>, scope: Scope) -> Expect<'a> {
        Expect {
            server: &self.server_id,
            sandbox,
            scope,
            now: crate::now_ms() / 1000,
        }
    }

    /// Checks the token for `scope` on `sandbox` (or the whole server), and
    /// that a sandbox token's team owns the sandbox.
    pub fn authorize(
        &self,
        token: Option<&str>,
        sandbox: Option<&str>,
        scope: Scope,
    ) -> ApiResult<Claims> {
        let token =
            token.ok_or_else(|| fail(ErrorCode::Unauthorized, "A sandbox token is needed."))?;
        let claims =
            token::verify(token, &self.keys, self.expect(sandbox, scope)).map_err(token_error)?;
        self.check_team(&claims, sandbox)?;
        Ok(claims)
    }

    /// Like `authorize`, for calls any scope on the sandbox may make.
    fn authorize_any(&self, token: Option<&str>, sandbox: &str) -> ApiResult<Claims> {
        let t = token.ok_or_else(|| fail(ErrorCode::Unauthorized, "A sandbox token is needed."))?;
        let unchecked = token::verify_signature(t, &self.keys).map_err(token_error)?;
        let scope = *unchecked
            .scopes
            .first()
            .ok_or_else(|| token_error(TokenError::MissingScope(Scope::Exec)))?;
        self.authorize(token, Some(sandbox), scope)
    }

    fn check_team(&self, claims: &Claims, sandbox: Option<&str>) -> ApiResult<()> {
        if claims.has(Scope::Admin) {
            return Ok(());
        }
        if let Some(id) = sandbox
            && let Ok(sbx) = self.mgr.find(id)
            && sbx.spec.team != claims.team
        {
            return Err(fail(
                ErrorCode::Forbidden,
                "The sandbox token is for another team.",
            ));
        }
        Ok(())
    }
}

type Q = Query<HashMap<String, String>>;

// ── Handlers ───────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({"ok": true, "protocol": pyo_proto::sandbox::SANDBOX_PROTOCOL}))
}

async fn report(State(st): S, h: HeaderMap, Query(q): Q) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), None, Scope::Admin)?;
    Ok(axum::Json(st.mgr.report()))
}

async fn list(State(st): S, h: HeaderMap, Query(q): Q) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), None, Scope::Admin)?;
    Ok(axum::Json(DaemonResult::Sandboxes {
        sandboxes: st.mgr.list(),
    }))
}

/// Runs one command from Pyo, after checking the token for it.
pub async fn run_command(
    st: &AppState,
    token: Option<&str>,
    cmd: DaemonCommand,
) -> ApiResult<DaemonResult> {
    let mgr = &st.mgr;
    Ok(match cmd {
        DaemonCommand::Create { spec } => {
            let claims = st.authorize(token, Some(&spec.id), Scope::Create)?;
            if !claims.has(Scope::Admin) && claims.team != spec.team {
                return Err(fail(
                    ErrorCode::Forbidden,
                    "The sandbox token is for another team.",
                ));
            }
            DaemonResult::Sandbox {
                sandbox: mgr.create(spec).await?,
            }
        }
        DaemonCommand::Delete { id } => {
            st.authorize(token, Some(&id), Scope::Lifecycle)?;
            mgr.delete(&id).await?;
            DaemonResult::Deleted { id }
        }
        DaemonCommand::Pause { id } => {
            st.authorize(token, Some(&id), Scope::Lifecycle)?;
            DaemonResult::Sandbox {
                sandbox: mgr.pause(&id).await?,
            }
        }
        DaemonCommand::Resume { id } => {
            st.authorize(token, Some(&id), Scope::Lifecycle)?;
            DaemonResult::Sandbox {
                sandbox: mgr.resume(&id).await?,
            }
        }
        DaemonCommand::SetTtl { id, ttl_secs } => {
            st.authorize(token, Some(&id), Scope::Lifecycle)?;
            DaemonResult::Sandbox {
                sandbox: mgr.set_ttl(&id, ttl_secs)?,
            }
        }
        DaemonCommand::OpenPort { id, port, public } => {
            st.authorize(token, Some(&id), Scope::Ports)?;
            DaemonResult::Sandbox {
                sandbox: mgr.open_port(&id, port, public).await?,
            }
        }
        DaemonCommand::ClosePort { id, port } => {
            st.authorize(token, Some(&id), Scope::Ports)?;
            DaemonResult::Sandbox {
                sandbox: mgr.close_port(&id, port).await?,
            }
        }
        DaemonCommand::Get { id } => {
            st.authorize_any(token, &id)?;
            DaemonResult::Sandbox {
                sandbox: mgr.status(&id)?,
            }
        }
        DaemonCommand::List => {
            st.authorize(token, None, Scope::Admin)?;
            DaemonResult::Sandboxes {
                sandboxes: mgr.list(),
            }
        }
        DaemonCommand::Report => {
            st.authorize(token, None, Scope::Admin)?;
            DaemonResult::Report {
                report: Box::new(mgr.report()),
            }
        }
        DaemonCommand::SnapshotTemplate {
            template,
            cpus,
            memory_mib,
            disk_mib,
            network,
        } => {
            st.authorize(token, None, Scope::Admin)?;
            let (key, took_ms) = mgr
                .snapshot_template(&template, cpus, memory_mib, disk_mib, network)
                .await?;
            DaemonResult::TemplateSnapshot {
                template,
                key,
                took_ms,
            }
        }
    })
}

async fn command(
    State(st): S,
    h: HeaderMap,
    Query(q): Q,
    axum::Json(cmd): axum::Json<DaemonCommand>,
) -> ApiResult<impl IntoResponse> {
    Ok(axum::Json(
        run_command(&st, token_of(&h, &q).as_deref(), cmd).await?,
    ))
}

async fn create(
    State(st): S,
    h: HeaderMap,
    Query(q): Q,
    axum::Json(spec): axum::Json<SandboxSpec>,
) -> ApiResult<impl IntoResponse> {
    let res = run_command(
        &st,
        token_of(&h, &q).as_deref(),
        DaemonCommand::Create { spec },
    )
    .await?;
    Ok((StatusCode::CREATED, axum::Json(res)))
}

async fn get_one(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    Ok(axum::Json(
        run_command(&st, token_of(&h, &q).as_deref(), DaemonCommand::Get { id }).await?,
    ))
}

async fn delete(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    Ok(axum::Json(
        run_command(
            &st,
            token_of(&h, &q).as_deref(),
            DaemonCommand::Delete { id },
        )
        .await?,
    ))
}

async fn pause(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    Ok(axum::Json(
        run_command(
            &st,
            token_of(&h, &q).as_deref(),
            DaemonCommand::Pause { id },
        )
        .await?,
    ))
}

async fn resume(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    Ok(axum::Json(
        run_command(
            &st,
            token_of(&h, &q).as_deref(),
            DaemonCommand::Resume { id },
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PortBody {
    port: u16,
    #[serde(default = "yes")]
    public: bool,
}

fn yes() -> bool {
    true
}

async fn open_port(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
    axum::Json(b): axum::Json<PortBody>,
) -> ApiResult<impl IntoResponse> {
    let cmd = DaemonCommand::OpenPort {
        id,
        port: b.port,
        public: b.public,
    };
    Ok(axum::Json(
        run_command(&st, token_of(&h, &q).as_deref(), cmd).await?,
    ))
}

async fn close_port(
    State(st): S,
    Path((id, port)): Path<(String, u16)>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    let cmd = DaemonCommand::ClosePort { id, port };
    Ok(axum::Json(
        run_command(&st, token_of(&h, &q).as_deref(), cmd).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TtlBody {
    ttl_secs: u64,
}

async fn set_ttl(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
    axum::Json(b): axum::Json<TtlBody>,
) -> ApiResult<impl IntoResponse> {
    let cmd = DaemonCommand::SetTtl {
        id,
        ttl_secs: b.ttl_secs,
    };
    Ok(axum::Json(
        run_command(&st, token_of(&h, &q).as_deref(), cmd).await?,
    ))
}

async fn exec(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
    axum::Json(req): axum::Json<ExecRequest>,
) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), Some(&id), Scope::Exec)?;
    Ok(axum::Json(st.mgr.exec(&id, &req).await?))
}

fn path_param(q: &HashMap<String, String>) -> ApiResult<String> {
    q.get("path")
        .filter(|p| !p.is_empty())
        .cloned()
        .ok_or_else(|| fail(ErrorCode::InvalidSpec, "The path is missing."))
}

/// Maps errors from envd file calls to API errors.
fn file_error(e: anyhow::Error) -> ApiError {
    if let Some(t) = e.downcast_ref::<FileTooBig>() {
        return fail(ErrorCode::InvalidSpec, t.to_string());
    }
    if let Some(EnvdError { kind, message }) = e.downcast_ref::<EnvdError>() {
        return fail(envd_code(*kind), message.clone());
    }
    fail(ErrorCode::Runtime, format!("{e:#}"))
}

fn envd_code(kind: ErrorKind) -> ErrorCode {
    match kind {
        ErrorKind::NotFound => ErrorCode::NotFound,
        ErrorKind::PermissionDenied => ErrorCode::Forbidden,
        ErrorKind::AlreadyExists | ErrorKind::Busy => ErrorCode::Conflict,
        ErrorKind::InvalidInput => ErrorCode::InvalidSpec,
        ErrorKind::Unsupported => ErrorCode::Unsupported,
        ErrorKind::Io => ErrorCode::Runtime,
    }
}

async fn read_file(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), Some(&id), Scope::Files)?;
    let path = path_param(&q)?;
    let (_, mut conn) = st.mgr.envd(&id).await?;
    let data = conn
        .read_file(&path, FILE_CAP as u64)
        .await
        .map_err(file_error)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data))
}

async fn write_file(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), Some(&id), Scope::Files)?;
    let path = path_param(&q)?;
    let mode = match q.get("mode") {
        Some(m) => Some(
            u32::from_str_radix(m, 8)
                .map_err(|_| fail(ErrorCode::InvalidSpec, "The mode must be octal, like 644."))?,
        ),
        None => None,
    };
    let (_, mut conn) = st.mgr.envd(&id).await?;
    let bytes = conn
        .write_file(&path, &body, mode)
        .await
        .map_err(file_error)?;
    Ok(axum::Json(serde_json::json!({"bytes": bytes})))
}

/// One request with one response from envd, mapped to JSON.
async fn simple(st: &AppState, id: &str, req: Request) -> ApiResult<axum::Json<wire::Response>> {
    let (_, mut conn) = st.mgr.envd(id).await?;
    match conn.call(req).await.map_err(file_error)? {
        wire::Response::Error { kind, message } => Err(fail(envd_code(kind), message)),
        r => Ok(axum::Json(r)),
    }
}

async fn remove_file(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), Some(&id), Scope::Files)?;
    let recursive = q.get("recursive").is_some_and(|v| v == "true" || v == "1");
    simple(
        &st,
        &id,
        Request::Remove {
            path: path_param(&q)?,
            recursive,
        },
    )
    .await
}

async fn list_files(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), Some(&id), Scope::Files)?;
    simple(
        &st,
        &id,
        Request::List {
            path: path_param(&q)?,
        },
    )
    .await
}

async fn stat_file(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), Some(&id), Scope::Files)?;
    simple(
        &st,
        &id,
        Request::Stat {
            path: path_param(&q)?,
        },
    )
    .await
}

async fn mkdir(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
) -> ApiResult<impl IntoResponse> {
    st.authorize(token_of(&h, &q).as_deref(), Some(&id), Scope::Files)?;
    simple(
        &st,
        &id,
        Request::Mkdir {
            path: path_param(&q)?,
        },
    )
    .await
}

// ── WebSocket ──────────────────────────────────────────────────────────────

async fn connect(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let claims = st.authorize_any(token_of(&h, &q).as_deref(), &id)?;
    let (_, conn) = st.mgr.envd(&id).await?;
    Ok(ws.on_upgrade(move |socket| proxy(socket, claims, conn)))
}

/// Why a client request is refused, if it is.
pub fn refuse(claims: &Claims, req: &Request, now_secs: u64) -> Option<String> {
    if now_secs > claims.exp + LEEWAY_SECS {
        return Some(TokenError::Expired.to_string());
    }
    match req.scope() {
        Some(s) if !claims.has(s) => Some(TokenError::MissingScope(s).to_string()),
        _ => None,
    }
}

async fn proxy(socket: WebSocket, claims: Claims, conn: crate::envd::Conn) {
    let hello = Frame::json(Kind::Hello, 0, &conn.hello);
    let (mut from_envd, mut to_envd) = conn.into_split();
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Frame>(64);

    // One writer to the client, fed by envd and by the daemon's own refusals.
    let writer = tokio::spawn(async move {
        while let Some(f) = out_rx.recv().await {
            if ws_tx
                .send(Message::Binary(f.encode().into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });
    let _ = out_tx.send(hello).await;
    let envd_tx = out_tx.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(f)) = from_envd.recv().await {
            if envd_tx.send(f).await.is_err() {
                break;
            }
        }
    });

    use tokio::io::AsyncWriteExt;
    while let Some(Ok(msg)) = ws_rx.next().await {
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(f) = Frame::decode(&bytes) else { break };
        match f.kind {
            Kind::Request => {
                let req = match f.parse::<Request>() {
                    Ok(r) => r,
                    Err(e) => {
                        let r = wire::Response::error(ErrorKind::InvalidInput, e.to_string());
                        let _ = out_tx.send(Frame::response(f.stream, &r)).await;
                        continue;
                    }
                };
                if let Some(why) = refuse(&claims, &req, crate::now_ms() / 1000) {
                    let r = wire::Response::error(ErrorKind::PermissionDenied, why);
                    let _ = out_tx.send(Frame::response(f.stream, &r)).await;
                    continue;
                }
            }
            Kind::Stdin | Kind::Eof => {}
            _ => continue,
        }
        if to_envd.write_all(&f.encode()).await.is_err() {
            break;
        }
    }
    reader.abort();
    drop(out_tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(scopes: Vec<Scope>) -> Claims {
        Claims {
            kid: "k".into(),
            sbx: "sbx_abcdefghijklmnop".into(),
            team: "t".into(),
            srv: "s".into(),
            iat: 100,
            exp: 200,
            scopes,
        }
    }

    #[test]
    fn websocket_requests_are_checked() {
        let c = claims(vec![Scope::Files]);
        assert!(refuse(&c, &Request::Stat { path: "/".into() }, 150).is_none());
        let why = refuse(&c, &Request::PortConnect { port: 1 }, 150).unwrap();
        assert!(why.contains("ports"), "{why}");
        assert!(refuse(&c, &Request::SetTime { unix_ms: 0 }, 150).is_some());
        assert!(refuse(&c, &Request::Ping, 150).is_none());
        assert!(
            refuse(&c, &Request::Ping, 200 + LEEWAY_SECS + 1)
                .unwrap()
                .contains("expired")
        );
        let admin = claims(vec![Scope::Admin]);
        assert!(refuse(&admin, &Request::SetTime { unix_ms: 0 }, 150).is_none());
    }

    #[tokio::test]
    async fn browsers_may_call_the_daemon() {
        use tower::ServiceExt;
        let app = Router::new()
            .route("/v1/health", get(health))
            .layer(axum::middleware::from_fn(cors));
        let pre = axum::http::Request::builder()
            .method("OPTIONS")
            .uri("/v1/health")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.clone().oneshot(pre).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(res.headers()["access-control-allow-origin"], "*");
        assert!(
            res.headers()["access-control-allow-headers"]
                .to_str()
                .unwrap()
                .contains("authorization")
        );
        assert_eq!(
            res.headers()["access-control-allow-private-network"],
            "true"
        );
        let get = axum::http::Request::builder()
            .uri("/v1/health")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.oneshot(get).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["access-control-allow-origin"], "*");
    }

    #[test]
    fn envd_errors_map_to_api_codes() {
        assert_eq!(envd_code(ErrorKind::NotFound), ErrorCode::NotFound);
        let e = file_error(anyhow::Error::new(EnvdError {
            kind: ErrorKind::PermissionDenied,
            message: "no".into(),
        }));
        assert_eq!(e.0.code, ErrorCode::Forbidden);
        let e = file_error(anyhow::Error::new(FileTooBig(1)));
        assert_eq!(e.0.code, ErrorCode::InvalidSpec);
    }
}
