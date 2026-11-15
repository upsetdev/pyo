//! The sandbox endpoints under `/api/v1`. Creating and using sandboxes
//! takes an API key with the "sandboxes" (or full) access. Clients then
//! talk to the sandbox server with the connection's token: exec, terminals,
//! files and ports never go through Pyo.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AddServer, Connection, CreateSandbox, CreateTemplate, Need};
use crate::{
    AppState,
    auth::SessionUser,
    error::{ApiError, ApiResult, ErrorBody},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sandboxes", get(list).post(create))
        .route("/sandboxes/{id}", get(get_one).delete(delete))
        .route("/sandboxes/{id}/pause", post(pause))
        .route("/sandboxes/{id}/resume", post(resume))
        .route("/sandboxes/{id}/extend", post(extend))
        .route("/sandboxes/{id}/token", post(token))
        .route("/sandboxes/{id}/ports", post(open_port))
        .route("/sandbox-servers", get(list_servers).post(add_server))
        .route(
            "/sandbox-servers/{id}",
            axum::routing::delete(remove_server),
        )
        .route(
            "/sandbox-templates",
            get(list_templates).post(create_template),
        )
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// The team's id.
    team: String,
    /// "live" (the default: not stopped or failed) or "all" (the latest 500).
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TeamQuery {
    /// The team's id.
    team: String,
}

async fn team_read(s: &AppState, user: &crate::auth::User, team: &str) -> ApiResult<()> {
    let mut c = s.db.acquire().await?;
    super::require(&mut c, user, team, Need::Read).await
}

/// Sandboxes
///
/// A team's sandboxes: their template, size, state, server and lifetime.
/// Env values are never shown, only their names.
#[utoipa::path(get, path = "/sandboxes", operation_id = "listSandboxes", tag = "Sandboxes",
    params(ListQuery),
    responses(
        (status = 200, description = "`sandboxes`, newest first, each with `id`, `teamId`, `projectId`, `serverId`, `server`, `template`, `templateId`, `state` (\"creating\", \"running\", \"pausing\", \"paused\", \"resuming\", \"stopping\", \"stopped\" or \"failed\"), `runtime` (\"firecracker\" or \"gvisor\"), `cpus`, `memoryMib`, `diskMib`, `network`, `isolation`, `labels`, `env` (names only), `url` (its server's daemon), `edgeUrl` (its server's edge proxy, or null), `ports` (each `port`, `public` and `url`), `error`, `createdBy`, `createdAt`, `startedAt`, `expiresAt`, `pausedAt` and `endedAt`. Also `limits` (the team's) and `inUse` (what its sandboxes take now).", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn list(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    team_read(&s, &user, &q.team).await?;
    let live = match q.state.as_deref() {
        None | Some("live") => true,
        Some("all") => false,
        Some(_) => return Err(ApiError::bad_request("state is \"live\" or \"all\".")),
    };
    let teams = [q.team.clone()];
    let mut c = s.db.acquire().await?;
    let limits = super::limits::load(&mut c, &q.team).await?;
    let in_use = super::limits::in_use(&mut c, &q.team).await?;
    Ok(Json(json!({
        "sandboxes": super::sandbox_views(&s.db, &teams, None, live).await?,
        "limits": limits,
        "inUse": in_use,
    })))
}

/// A sandbox
///
/// One sandbox, as `listSandboxes` lists them, with a fresh `connection`
/// for its server when you may use it.
#[utoipa::path(get, path = "/sandboxes/{id}", operation_id = "getSandbox", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id, `sbx_…`.")),
    responses(
        (status = 200, description = "The sandbox, with `connection` (as in `createSandbox`) when you may use sandboxes and it lives. Otherwise `connection` is null and `connectionNote` says why: a read-only key, a viewer, or a sandbox that has ended.", body = Object),
        (status = 404, description = "No such sandbox, or you can't see it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn get_one(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::get(&s, &user, &id).await?))
}

/// Create a sandbox
///
/// Makes an isolated machine from a sandbox template on one of the team's
/// sandbox servers (or the installation's), within the team's limits. It
/// runs until its lifetime ends or it's deleted. The answer has the
/// `connection`: the server's address and a token, valid an hour, for exec,
/// terminals, files and ports there. Ask for a new token with
/// `createSandboxToken`. `teamId` may be left out when the API key (or
/// you) can use only one team.
#[utoipa::path(post, path = "/sandboxes", operation_id = "createSandbox", tag = "Sandboxes",
    request_body(content = CreateSandbox),
    responses(
        (status = 201, description = "`sandbox` (as in `listSandboxes`) and `connection`: `url`, `sandboxId`, `token` and `expiresAt`.", body = Object),
        (status = 400, description = "A size, lifetime or other value Pyo can't accept, or no `teamId` when you can use more than one team.", body = ErrorBody),
        (status = 403, description = "Your role, the API key's access or the team's sandbox limits don't allow it.", body = ErrorBody),
        (status = 404, description = "No such team, template or project.", body = ErrorBody),
        (status = 409, description = "No sandbox server can run it: none yet, none with the template, or none with KVM for `isolation: vm`.", body = ErrorBody),
        (status = 503, description = "Every sandbox server is full or down. Try again soon.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn create(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<CreateSandbox>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(super::create(&s, &user, req).await?),
    ))
}

/// Delete a sandbox
///
/// Ends the sandbox and throws away its files. When its server can't be
/// reached right now, it shows as stopping and Pyo tries again.
#[utoipa::path(delete, path = "/sandboxes/{id}", operation_id = "deleteSandbox", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    responses(
        (status = 200, description = "The sandbox, now stopped or stopping.", body = Object),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn delete(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::delete(&s, &user, &id).await?))
}

/// Pause a sandbox
///
/// Stops its machine and keeps its memory and files on the server, so it
/// resumes where it was. Connections to it drop.
#[utoipa::path(post, path = "/sandboxes/{id}/pause", operation_id = "pauseSandbox", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    responses(
        (status = 200, description = "The sandbox, paused.", body = Object),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
        (status = 409, description = "It has ended, or isn't running.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn pause(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::pause(&s, &user, &id).await?))
}

/// Resume a sandbox
///
/// Starts a paused sandbox again where it was.
#[utoipa::path(post, path = "/sandboxes/{id}/resume", operation_id = "resumeSandbox", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    responses(
        (status = 200, description = "The sandbox, running.", body = Object),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
        (status = 409, description = "It has ended, or isn't paused.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn resume(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::resume(&s, &user, &id).await?))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Extend {
    /// The sandbox now ends this long from now, like "30m" or "1h", or
    /// seconds. Within the team's longest lifetime.
    #[schema(value_type = String)]
    ttl: Value,
}

/// Extend a sandbox
///
/// Sets when the sandbox ends: `ttl` from now.
#[utoipa::path(post, path = "/sandboxes/{id}/extend", operation_id = "extendSandbox", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    request_body(content = Extend),
    responses(
        (status = 200, description = "The sandbox, with its new `expiresAt`.", body = Object),
        (status = 403, description = "Longer than the team's sandboxes may live.", body = ErrorBody),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn extend(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Json(b): Json<Extend>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::extend(&s, &user, &id, Some(&b.ttl)).await?))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OpenPort {
    /// The port the sandbox listens on, like 3000.
    port: u16,
    /// Whether anyone can open the URL. A private port needs a sandbox
    /// token. True when left out.
    #[serde(default)]
    public: Option<bool>,
}

/// Open a sandbox port
///
/// Serves a port the sandbox listens on at its own URL. A public port
/// answers anyone. A private one needs a sandbox token, in
/// `Authorization: Bearer` or once as `?pyo_token=` in a browser.
#[utoipa::path(post, path = "/sandboxes/{id}/ports", operation_id = "openSandboxPort", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    request_body = OpenPort,
    responses(
        (status = 200, description = "`url`, `port` and `public`.", body = Object),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
        (status = 409, description = "It has ended, or its server has no edge proxy.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn open_port(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Json(b): Json<OpenPort>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        super::open_port(&s, &user, &id, b.port, b.public.unwrap_or(true)).await?,
    ))
}

/// New sandbox token
///
/// A new token for the sandbox's server, valid an hour. Ask before the
/// last one runs out.
#[utoipa::path(post, path = "/sandboxes/{id}/token", operation_id = "createSandboxToken", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    responses(
        (status = 200, description = "The connection: `url`, `sandboxId`, `token` and `expiresAt`.", body = Connection),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
        (status = 409, description = "It has ended.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn token(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Connection>> {
    Ok(Json(super::token(&s, &user, &id).await?))
}

/// Sandbox servers
///
/// The servers a team's sandboxes run on: its own and the installation's.
#[utoipa::path(get, path = "/sandbox-servers", operation_id = "listSandboxServers", tag = "Sandboxes",
    params(TeamQuery),
    responses(
        (status = 200, description = "`servers`, each with `id`, `teamId` (null for the installation's), `name`, `address`, `state` (\"joining\", \"ready\", \"unreachable\" or \"refused\"), `enabled`, `capability` (`runtime`, `kvm`, `bootMs`, `arch`, `cpus`, `memory`, `disk` and the checks), `summary` (one line about it), `capacity`, `templates`, `daemonVersion`, `lastReportAt`, `error`, `sandboxes` (how many run there) and `createdAt`.", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn list_servers(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<TeamQuery>,
) -> ApiResult<Json<Value>> {
    team_read(&s, &user, &q.team).await?;
    let teams = [q.team];
    Ok(Json(
        json!({ "servers": super::server_views(&s.db, &teams, None).await? }),
    ))
}

/// Add a sandbox server
///
/// Makes a server's one-time join code and the command to run on it. The
/// code works once, for 15 minutes. The command joins your tailnet, installs
/// Pyo's sandbox daemon, checks what the server can run (Firecracker where it
/// has KVM, gVisor elsewhere) and joins. Adding a server that hasn't joined
/// yet again makes a new code. Team owners and admins. Needs Tailscale.
#[utoipa::path(post, path = "/sandbox-servers", operation_id = "addSandboxServer", tag = "Sandboxes",
    request_body(content = AddServer),
    responses(
        (status = 200, description = "`server`, `command` (run it on the server as root) and `expiresAt` (when the code runs out).", body = Object),
        (status = 400, description = "A bad name, or Tailscale isn't set up.", body = ErrorBody),
        (status = 409, description = "A server with that name has joined already, or Pyo isn't on the tailnet yet.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn add_server(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<AddServer>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::add_server(&s, &user, req).await?))
}

/// Remove a sandbox server
///
/// Deletes its sandboxes where it still answers, and forgets it. To take the
/// daemon off the server, run `sandbox.sh --uninstall` there.
#[utoipa::path(delete, path = "/sandbox-servers/{id}", operation_id = "removeSandboxServer", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox server's id, `sbs_…`.")),
    responses(
        (status = 200, description = "`ok`, and `stopped`: how many sandboxes ended.", body = Object),
        (status = 404, description = "No such sandbox server.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn remove_server(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::remove_server(&s, &user, &id).await?))
}

/// Sandbox templates
///
/// What a team's sandboxes can start from: its own templates and the
/// installation's.
#[utoipa::path(get, path = "/sandbox-templates", operation_id = "listSandboxTemplates", tag = "Sandboxes",
    params(TeamQuery),
    responses(
        (status = 200, description = "`templates`, each with `id`, `teamId` (null for the installation's), `name`, `source` (`kind` and `image`), `state` (\"pending\", \"building\", \"ready\" or \"failed\"), `snapshots` (per runtime), `servers` (the servers that have it), `error` and `createdAt`.", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn list_templates(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<TeamQuery>,
) -> ApiResult<Json<Value>> {
    team_read(&s, &user, &q.team).await?;
    let teams = [q.team];
    Ok(Json(
        json!({ "templates": super::template_views(&s.db, &teams, None).await? }),
    ))
}

/// Create a sandbox template
///
/// A template made from an OCI image, like "node:22". It waits as pending
/// until Pyo has built it and put it on the sandbox servers.
#[utoipa::path(post, path = "/sandbox-templates", operation_id = "createSandboxTemplate", tag = "Sandboxes",
    request_body(content = CreateTemplate),
    responses(
        (status = 200, description = "The template, as in `listSandboxTemplates`.", body = Object),
        (status = 400, description = "A bad name or image.", body = ErrorBody),
        (status = 409, description = "The team has a template with that name.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn create_template(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<CreateTemplate>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::create_template(&s, &user, req).await?))
}
