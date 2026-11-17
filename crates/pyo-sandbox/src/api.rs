//! The daemon's API. It listens only on the configured address, the
//! server's tailnet address in production. Every call except the health
//! check needs a token Pyo signed (`pyo_proto::sandbox::token`), in
//! `Authorization: Bearer <token>`. Browsers opening the WebSocket name it
//! as the subprotocol `pyo.token.<token>` (next to `pyo`, which the daemon
//! then answers with), or put it in `?token=<token>`.
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
//! | `ANY /v1/sandboxes/{id}/web/{port}/...` | ports, for that sandbox only |
//!
//! `.../web/{port}/` serves a web page on a port of the sandbox to the
//! browser that opens it, over the tailnet, like the dashboard's Open
//! desktop (voxl on 6080). The port needn't be open on the edge. The
//! token comes once as `?pyo_token=`, which sets an HttpOnly cookie for
//! that sandbox's pages (SameSite=Lax, so other sites can't use it) and
//! redirects, then in the cookie, or in `Authorization`. The rest of the
//! path goes to the port, WebSocket upgrades too. Pages there must use
//! relative links.
//!
//! The WebSocket speaks the envd wire protocol, one frame per binary
//! message. The first message is envd's `Hello`. The daemon checks every
//! request against the token's scopes and drops anything that isn't a
//! request, stdin or EOF. On the way it:
//!
//! - adds the sandbox's env under each exec's and terminal's own, as
//!   `POST .../exec` does;
//! - kills an exec after its `timeoutMs` (60 seconds when left out, none
//!   for 0) and keeps at most 8 MiB of each output, then marks the exit
//!   `timedOut` or `truncated`;
//! - lets resizes and signals on streams the connection opened pass after
//!   the token runs out, so a long terminal keeps working, and takes a
//!   fresh token as the text message `{"token": "..."}`;
//! - refuses a request with the error kind `unauthorized` (the token ran
//!   out) or `forbidden` (its scopes don't allow it).
//!
//! Errors are `{"type": "error", "error": {"code", "message"}}`. A token
//! the daemon refuses is `unauthorized` (401) or `forbidden` (403). The
//! sandbox's own refusal, like a file its user can't read, is
//! `permissionDenied` (also 403): a new token won't help.
//!
//! A refused upgrade is accepted and closed at once with code 4000 plus
//! the HTTP status (4401, 4403, 4404…) and the reason, since browsers
//! don't show an HTTP refusal.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
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
        .route("/v1/sandboxes/{id}/web/{port}", any(web))
        .route("/v1/sandboxes/{id}/web/{port}/", any(web))
        .route("/v1/sandboxes/{id}/web/{port}/{*path}", any(web))
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
                sandbox: mgr.set_ttl(&id, ttl_secs).await?,
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
        DaemonCommand::SetEdgeCert { cert, key } => {
            st.authorize(token, None, Scope::Admin)?;
            let Some(tls) = mgr.edge_tls() else {
                return Err(fail(
                    ErrorCode::Conflict,
                    "This server's edge proxy has no HTTPS listener. Run sandbox.sh again with --edge-domain.",
                ));
            };
            let fingerprint = tls
                .install(&cert, &key.0)
                .map_err(|e| fail(ErrorCode::InvalidSpec, format!("{e:#}")))?;
            DaemonResult::EdgeCert { fingerprint }
        }
        DaemonCommand::PrepareTemplate {
            template,
            image,
            plain_http,
            auth,
            max_mib,
        } => {
            st.authorize(token, None, Scope::Admin)?;
            DaemonResult::Template {
                template: mgr.prepare_template(&template, &image, plain_http, auth, max_mib)?,
            }
        }
        DaemonCommand::DeleteTemplate { template } => {
            st.authorize(token, None, Scope::Admin)?;
            mgr.delete_template(&template).await?;
            DaemonResult::TemplateDeleted { template }
        }
        DaemonCommand::Template { template } => {
            st.authorize(token, None, Scope::Admin)?;
            DaemonResult::Template {
                template: mgr.template_progress(&template).ok_or_else(|| {
                    fail(
                        ErrorCode::NotFound,
                        format!("The sandbox template {template} isn't on this server."),
                    )
                })?,
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

/// A port's web pages through the daemon: see the module's docs.
async fn web(
    State(st): S,
    Path(p): Path<HashMap<String, String>>,
    mut req: axum::extract::Request,
) -> Response {
    let fail = |status: StatusCode, msg: &str| crate::edge::text(status, msg).into_response();
    let id = p.get("id").cloned().unwrap_or_default();
    let Some(port) = p.get("port").and_then(|v| v.parse::<u16>().ok()) else {
        return fail(StatusCode::NOT_FOUND, "That isn't a port.");
    };
    let prefix = format!("/v1/sandboxes/{id}/web/{port}/");
    let Some((token, from_query)) = crate::edge::take_token(&mut req) else {
        return fail(StatusCode::UNAUTHORIZED, "This needs a sandbox token.");
    };
    // Only a token for this sandbox, never a server-wide one.
    if !matches!(st.authorize(Some(&token), Some(&id), Scope::Ports), Ok(c) if c.sbx == id) {
        return fail(
            StatusCode::UNAUTHORIZED,
            "The sandbox token isn't valid here. Open it again from Pyo.",
        );
    }
    let path = req.uri().path().to_owned();
    let rest = path.strip_prefix(&prefix);
    if from_query || rest.is_none() {
        // Into a cookie for this sandbox's pages, out of the address bar.
        // Its pages are under the prefix, so it ends in a slash.
        let query = req
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default();
        let to = format!("{prefix}{}{query}", rest.unwrap_or_default());
        let cookie = from_query.then(|| {
            format!(
                "{}={token}; Path=/v1/sandboxes/{id}/web/; HttpOnly; SameSite=Lax",
                crate::edge::TOKEN_COOKIE
            )
        });
        return crate::edge::redirect(StatusCode::SEE_OTHER, &to, cookie.as_deref())
            .into_response();
    }
    match st.mgr.status(&id).map(|s| s.state) {
        Ok(pyo_proto::sandbox::SandboxState::Running) => {}
        Ok(pyo_proto::sandbox::SandboxState::Paused) => {
            return fail(StatusCode::SERVICE_UNAVAILABLE, "The sandbox is paused.");
        }
        Ok(_) => {
            return fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "The sandbox isn't running.",
            );
        }
        Err(_) => {
            return fail(
                StatusCode::NOT_FOUND,
                "There is no such sandbox on this server.",
            );
        }
    }
    // The page sees itself at the root of the port, as from inside.
    let pq = format!(
        "/{}{}",
        rest.unwrap_or_default(),
        req.uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    match pq.parse() {
        Ok(uri) => *req.uri_mut() = uri,
        Err(_) => return fail(StatusCode::BAD_REQUEST, "That address isn't valid."),
    }
    if let Ok(v) = header::HeaderValue::from_str(&format!("localhost:{port}")) {
        req.headers_mut().insert(header::HOST, v);
    }
    crate::edge::proxy(&st, req, &id, port)
        .await
        .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TtlBody {
    /// Null: the sandbox runs until it's deleted.
    #[serde(deserialize_with = "Option::deserialize")]
    ttl_secs: Option<u64>,
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
        ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
        ErrorKind::Forbidden => ErrorCode::Forbidden,
        ErrorKind::Unauthorized => ErrorCode::Unauthorized,
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

/// The subprotocol a client may name to carry its token, as
/// `pyo.token.<token>`, so the token stays out of the URL. Browsers can't
/// set headers on a WebSocket but can name subprotocols. The daemon picks
/// [`WS_PROTOCOL`] when the client offers it too, else the token's.
pub const WS_TOKEN_PROTOCOL: &str = "pyo.token.";
pub const WS_PROTOCOL: &str = "pyo";

/// The token from `Sec-WebSocket-Protocol`, and the subprotocol to answer.
fn ws_protocol_token(h: &HeaderMap) -> Option<(String, String)> {
    let offered: Vec<&str> = h
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect();
    let (proto, token) = offered
        .iter()
        .find_map(|p| p.strip_prefix(WS_TOKEN_PROTOCOL).map(|t| (*p, t)))?;
    let answer = if offered.contains(&WS_PROTOCOL) {
        WS_PROTOCOL
    } else {
        proto
    };
    Some((token.to_string(), answer.to_string()))
}

/// The close code for a refused upgrade: 4000 plus the HTTP status, like
/// 4401 for a token that ran out.
pub fn close_code(code: ErrorCode) -> u16 {
    4000 + code.http_status()
}

/// A close reason fits in 123 bytes.
fn close_reason(msg: &str) -> String {
    let mut end = msg.len().min(123);
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    msg[..end].to_string()
}

async fn connect(
    State(st): S,
    Path(id): Path<String>,
    h: HeaderMap,
    Query(q): Q,
    ws: WebSocketUpgrade,
) -> Response {
    let (token, ws) = match ws_protocol_token(&h) {
        Some((token, answer)) => (Some(token), ws.protocols([answer])),
        None => (token_of(&h, &q), ws),
    };
    let opened = async {
        let claims = st.authorize_any(token.as_deref(), &id)?;
        let (sbx, conn) = st.mgr.envd(&id).await?;
        Ok::<_, ApiError>((claims, sbx.spec.env.clone(), conn))
    }
    .await;
    match opened {
        Ok((claims, env, conn)) => ws.on_upgrade(move |socket| {
            proxy(
                socket,
                Session {
                    st,
                    sandbox: id,
                    claims,
                    env,
                },
                conn,
            )
        }),
        // Browsers don't show why an upgrade failed, so it's accepted and
        // closed at once with the reason.
        Err(ApiError(e)) => ws.on_upgrade(move |mut socket| async move {
            let _ = socket
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: close_code(e.code),
                    reason: close_reason(&e.message).into(),
                })))
                .await;
        }),
    }
}

/// Why a client request is refused, if it is. `open` is whether the
/// request acts on a stream this connection opened already (a resize or a
/// signal): those pass after the token runs out, so a long terminal keeps
/// working. New requests need a token that's still good.
pub fn refuse(claims: &Claims, req: &Request, now_secs: u64, open: bool) -> Option<Refusal> {
    let acts_on_open = open && !req.opens_stream();
    if !acts_on_open && now_secs > claims.exp + LEEWAY_SECS {
        return Some(Refusal(
            ErrorKind::Unauthorized,
            TokenError::Expired.to_string(),
        ));
    }
    match req.scope() {
        Some(s) if !claims.has(s) => Some(Refusal(
            ErrorKind::Forbidden,
            TokenError::MissingScope(s).to_string(),
        )),
        _ => None,
    }
}

#[derive(Debug)]
pub struct Refusal(pub ErrorKind, pub String);

/// One WebSocket's state.
struct Session {
    st: Arc<AppState>,
    sandbox: String,
    claims: Claims,
    /// The sandbox's env, under each exec's and terminal's own, as
    /// `POST .../exec` does.
    env: std::collections::BTreeMap<String, String>,
}

/// A stream a client opened on this connection.
#[derive(Default)]
struct Open {
    /// An exec with the limits on: the timeout and the output cap.
    limited: bool,
    started: bool,
    out: usize,
    err: usize,
    truncated: bool,
    timed_out: bool,
    timer: Option<tokio::task::AbortHandle>,
}

type Streams = Arc<std::sync::Mutex<HashMap<u32, Open>>>;

/// A client's text message: a fresh token for this connection, as
/// `{"token": "..."}`. The daemon answers
/// `{"type": "token", "ok": true, "expiresAt": <ms>}` or `"ok": false` with
/// a `message`, and keeps the old token when it refuses the new one.
#[derive(Deserialize)]
struct TokenMessage {
    token: String,
}

impl Session {
    fn renew(&mut self, text: &str) -> serde_json::Value {
        let refused = |m: String| serde_json::json!({"type": "token", "ok": false, "message": m});
        let Ok(m) = serde_json::from_str::<TokenMessage>(text) else {
            return refused("The daemon takes only {\"token\": \"...\"} as text.".into());
        };
        match self.st.authorize_any(Some(&m.token), &self.sandbox) {
            Ok(c) if c.team != self.claims.team && !c.has(Scope::Admin) => {
                refused("The sandbox token is for another team.".into())
            }
            Ok(c) => {
                let exp = c.exp;
                self.claims = c;
                serde_json::json!({"type": "token", "ok": true, "expiresAt": exp * 1000})
            }
            Err(ApiError(e)) => refused(e.message),
        }
    }
}

/// The request as it goes to envd: the sandbox's env under an exec's or a
/// terminal's own.
fn with_env(base: &std::collections::BTreeMap<String, String>, req: Request) -> Request {
    let merge = |own: std::collections::BTreeMap<String, String>| {
        let mut env = base.clone();
        env.extend(own);
        env
    };
    match req {
        Request::Exec(mut e) => {
            e.env = merge(std::mem::take(&mut e.env));
            Request::Exec(e)
        }
        Request::Pty(mut p) => {
            p.env = merge(std::mem::take(&mut p.env));
            Request::Pty(p)
        }
        r => r,
    }
}

/// The timeout of a WebSocket exec, None for none (`timeoutMs: 0`).
fn ws_exec_timeout(ms: Option<u64>) -> Option<std::time::Duration> {
    match ms {
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
        None => Some(crate::envd::DEFAULT_EXEC_TIMEOUT),
    }
}

/// What goes on to the client of a frame from envd. Keeps each limited
/// exec's output under the cap, and adds `timedOut` and `truncated` to its
/// exit.
fn from_envd(streams: &Streams, mut f: Frame) -> Option<Frame> {
    let mut map = streams.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = map.get_mut(&f.stream) else {
        return Some(f);
    };
    match f.kind {
        Kind::Stdout | Kind::Stderr if s.limited => {
            let used = if f.kind == Kind::Stdout {
                &mut s.out
            } else {
                &mut s.err
            };
            let room = crate::envd::EXEC_OUTPUT_CAP.saturating_sub(*used);
            if f.payload.len() > room {
                s.truncated = true;
                f.payload.truncate(room);
            }
            *used += f.payload.len();
            (!f.payload.is_empty()).then_some(f)
        }
        Kind::Response => {
            match f.parse::<wire::Response>() {
                Ok(wire::Response::Started { .. }) => s.started = true,
                Ok(wire::Response::Exit { .. }) => {
                    let s = map.remove(&f.stream).unwrap_or_default();
                    if let Some(t) = s.timer {
                        t.abort();
                    }
                    if (s.truncated || s.timed_out)
                        && let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&f.payload)
                    {
                        v["truncated"] = s.truncated.into();
                        v["timedOut"] = s.timed_out.into();
                        f.payload = serde_json::to_vec(&v).unwrap_or(f.payload);
                    }
                }
                // A stream that never started has ended.
                Ok(wire::Response::Error { .. }) if !s.started => {
                    if let Some(t) = map.remove(&f.stream).and_then(|s| s.timer) {
                        t.abort();
                    }
                }
                _ => {}
            }
            Some(f)
        }
        _ => Some(f),
    }
}

async fn proxy(socket: WebSocket, mut session: Session, conn: crate::envd::Conn) {
    let hello = Frame::json(Kind::Hello, 0, &conn.hello);
    let (mut from_envd_rx, mut to_envd) = conn.into_split();
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(64);
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<Frame>(64);
    let streams: Streams = Arc::default();

    // One writer to the client, fed by envd and by the daemon's own answers.
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if ws_tx.send(m).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });
    let binary = |f: Frame| Message::Binary(f.encode().into());
    let _ = out_tx.send(binary(hello)).await;
    // One writer to envd, fed by the client and by exec timeouts.
    let envd_writer = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(f) = in_rx.recv().await {
            if to_envd.write_all(&f.encode()).await.is_err() {
                break;
            }
        }
    });
    let envd_tx = out_tx.clone();
    let reader_streams = streams.clone();
    let reader = tokio::spawn(async move {
        while let Ok(Some(f)) = from_envd_rx.recv().await {
            let Some(f) = from_envd(&reader_streams, f) else {
                continue;
            };
            if envd_tx.send(binary(f)).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = ws_rx.next().await {
        let bytes = match msg {
            Message::Binary(b) => b,
            Message::Text(t) => {
                let answer = session.renew(t.as_str());
                let _ = out_tx.send(Message::Text(answer.to_string().into())).await;
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(mut f) = Frame::decode(&bytes) else {
            break;
        };
        match f.kind {
            Kind::Request => {
                let req = match f.parse::<Request>() {
                    Ok(r) => r,
                    Err(e) => {
                        let r = wire::Response::error(ErrorKind::InvalidInput, e.to_string());
                        let _ = out_tx.send(binary(Frame::response(f.stream, &r))).await;
                        continue;
                    }
                };
                let open = streams
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&f.stream);
                if let Some(Refusal(kind, why)) =
                    refuse(&session.claims, &req, crate::now_ms() / 1000, open)
                {
                    let r = wire::Response::error(kind, why);
                    let _ = out_tx.send(binary(Frame::response(f.stream, &r))).await;
                    continue;
                }
                if matches!(req, Request::Exec(_) | Request::Pty(_)) {
                    let mut o = Open::default();
                    if let Request::Exec(e) = &req
                        && let Some(after) = ws_exec_timeout(e.timeout_ms)
                    {
                        o.limited = true;
                        let (streams, kill, id) = (streams.clone(), in_tx.clone(), f.stream);
                        let t = tokio::spawn(async move {
                            tokio::time::sleep(after).await;
                            if let Some(s) = streams
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .get_mut(&id)
                            {
                                s.timed_out = true;
                            } else {
                                return;
                            }
                            let sig = Request::Signal {
                                signal: libc::SIGKILL,
                            };
                            let _ = kill.send(Frame::request(id, &sig)).await;
                        });
                        o.timer = Some(t.abort_handle());
                    }
                    f = Frame::request(f.stream, &with_env(&session.env, req));
                    let old = streams
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(f.stream, o);
                    if let Some(t) = old.and_then(|o| o.timer) {
                        t.abort();
                    }
                }
            }
            Kind::Stdin | Kind::Eof => {}
            _ => continue,
        }
        if in_tx.send(f).await.is_err() {
            break;
        }
    }
    reader.abort();
    for s in streams
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .map(|(_, s)| s)
    {
        if let Some(t) = s.timer {
            t.abort();
        }
    }
    drop(in_tx);
    let _ = envd_writer.await;
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
        let stat = Request::Stat { path: "/".into() };
        assert!(refuse(&c, &stat, 150, false).is_none());
        let Refusal(kind, why) = refuse(&c, &Request::PortConnect { port: 1 }, 150, false).unwrap();
        assert_eq!(kind, ErrorKind::Forbidden);
        assert!(why.contains("ports"), "{why}");
        assert!(refuse(&c, &Request::SetTime { unix_ms: 0 }, 150, false).is_some());
        assert!(refuse(&c, &Request::Ping, 150, false).is_none());
        let late = 200 + LEEWAY_SECS + 1;
        let Refusal(kind, why) = refuse(&c, &Request::Ping, late, false).unwrap();
        assert_eq!(kind, ErrorKind::Unauthorized);
        assert!(why.contains("expired"), "{why}");
        let admin = claims(vec![Scope::Admin]);
        assert!(refuse(&admin, &Request::SetTime { unix_ms: 0 }, 150, false).is_none());
    }

    #[test]
    fn open_streams_outlive_the_token() {
        let c = claims(vec![Scope::Pty]);
        let late = 200 + LEEWAY_SECS + 1;
        let resize = Request::Resize { cols: 1, rows: 1 };
        let signal = Request::Signal { signal: 2 };
        // On a stream this connection opened: still fine.
        assert!(refuse(&c, &resize, late, true).is_none());
        assert!(refuse(&c, &signal, late, true).is_none());
        // On a stream it didn't, or a new request on an open one: refused.
        assert!(refuse(&c, &resize, late, false).is_some());
        let pty = Request::Pty(wire::PtySpec {
            cmd: None,
            cwd: None,
            env: Default::default(),
            cols: 80,
            rows: 24,
        });
        assert!(refuse(&c, &pty, late, true).is_some());
        assert!(refuse(&c, &pty, 150, false).is_none());
    }

    #[test]
    fn the_token_may_come_as_a_subprotocol() {
        let mut h = HeaderMap::new();
        h.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "pyo, pyo.token.pyosbx1.abc.def".parse().unwrap(),
        );
        assert_eq!(
            ws_protocol_token(&h),
            Some(("pyosbx1.abc.def".into(), "pyo".into()))
        );
        h.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            "pyo.token.t1".parse().unwrap(),
        );
        assert_eq!(
            ws_protocol_token(&h),
            Some(("t1".into(), "pyo.token.t1".into()))
        );
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, "chat".parse().unwrap());
        assert_eq!(ws_protocol_token(&h), None);
        assert_eq!(close_code(ErrorCode::Unauthorized), 4401);
        assert_eq!(close_reason(&"é".repeat(100)).len(), 122);
    }

    #[test]
    fn websocket_execs_get_the_limits() {
        use std::time::Duration;
        assert_eq!(ws_exec_timeout(None), Some(Duration::from_secs(60)));
        assert_eq!(ws_exec_timeout(Some(5)), Some(Duration::from_millis(5)));
        assert_eq!(ws_exec_timeout(Some(0)), None);
        let streams: Streams = Arc::default();
        streams.lock().unwrap().insert(
            3,
            Open {
                limited: true,
                ..Open::default()
            },
        );
        let cap = crate::envd::EXEC_OUTPUT_CAP;
        let f = from_envd(&streams, Frame::new(Kind::Stdout, 3, vec![b'a'; cap - 2])).unwrap();
        assert_eq!(f.payload.len(), cap - 2);
        let f = from_envd(&streams, Frame::new(Kind::Stdout, 3, vec![b'a'; 5])).unwrap();
        assert_eq!(f.payload.len(), 2);
        assert!(from_envd(&streams, Frame::new(Kind::Stdout, 3, vec![b'a'; 5])).is_none());
        // Stderr has its own cap, and other streams none.
        assert!(from_envd(&streams, Frame::new(Kind::Stderr, 3, vec![b'a'; 5])).is_some());
        assert!(from_envd(&streams, Frame::new(Kind::Stdout, 4, vec![b'a'; cap + 1])).is_some());
        let exit = wire::Response::Exit {
            code: Some(0),
            signal: None,
        };
        let f = from_envd(&streams, Frame::response(3, &exit)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&f.payload).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["timedOut"], false);
        assert!(streams.lock().unwrap().is_empty());
    }

    #[test]
    fn execs_and_terminals_get_the_sandbox_env() {
        let base = [
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ]
        .into();
        let exec = Request::Exec(wire::ExecSpec {
            cmd: wire::Cmd::Shell("env".into()),
            cwd: None,
            env: [("B".to_string(), "own".to_string())].into(),
            stdin: false,
            timeout_ms: None,
        });
        let Request::Exec(e) = with_env(&base, exec) else {
            unreachable!()
        };
        assert_eq!(e.env["A"], "1");
        assert_eq!(e.env["B"], "own");
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
        assert_eq!(e.0.code, ErrorCode::PermissionDenied);
        assert_eq!(e.0.code.http_status(), 403);
        let json = serde_json::to_value(&e.0).unwrap();
        assert_eq!(json["code"], "permissionDenied");
        assert_eq!(envd_code(ErrorKind::Forbidden), ErrorCode::Forbidden);
        assert_eq!(envd_code(ErrorKind::Unauthorized), ErrorCode::Unauthorized);
        let e = file_error(anyhow::Error::new(FileTooBig(1)));
        assert_eq!(e.0.code, ErrorCode::InvalidSpec);
    }
}
