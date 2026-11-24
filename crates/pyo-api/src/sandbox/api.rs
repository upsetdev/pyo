//! The sandbox endpoints under `/api/v1`. Creating and using sandboxes
//! takes an API key with the "sandboxes" (or full) access. Clients then
//! talk to the sandbox server with the connection's token: exec, terminals,
//! files and ports never go through Pyo.

use crate::error::Json;
use crate::error::Query;
use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AddCloudServer, AddServer, Connection, CreateSandbox, CreateTemplate, Need};
use crate::{
    AppState,
    auth::SessionUser,
    error::{ApiError, ApiResult, ErrorBody},
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/sandboxes", get(list).post(create))
        .route("/sandboxes/{id}", get(get_one).patch(rename).delete(delete))
        .route("/sandboxes/{id}/pause", post(pause))
        .route("/sandboxes/{id}/resume", post(resume))
        .route("/sandboxes/{id}/extend", post(extend))
        .route("/sandboxes/{id}/token", post(token))
        .route("/sandboxes/{id}/ports", post(open_port))
        .route(
            "/sandboxes/{id}/ports/{port}",
            axum::routing::delete(close_port),
        )
        .route("/sandbox-servers", get(list_servers).post(add_server))
        .route("/sandbox-servers/cloud", post(add_cloud_server))
        .route("/sandbox-servers/join-codes", post(create_join_code))
        .route("/sandbox-servers/{id}/reset-access", post(reset_access))
        .route(
            "/sandbox-servers/{id}",
            axum::routing::delete(remove_server),
        )
        .route("/sandbox-servers/{id}/certificate", post(issue_certificate))
        .route("/sandbox-servers/{id}/dns-record", post(create_dns_record))
        .route(
            "/sandbox-templates",
            get(list_templates).post(create_template),
        )
        .route(
            "/sandbox-templates/{id}",
            axum::routing::delete(delete_template),
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
    /// Only sandboxes whose name or id has this text, ignoring case.
    #[serde(default)]
    q: Option<String>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TeamQuery {
    /// The team's id.
    team: String,
}

/// Sandboxes
///
/// A team's sandboxes: their template, size, state, server and lifetime.
/// Env values are never shown, only their names.
#[utoipa::path(get, path = "/sandboxes", operation_id = "listSandboxes", tag = "Sandboxes",
    params(ListQuery),
    responses(
        (status = 200, description = "`sandboxes`, newest first, each with `id`, `name`, `teamId`, `projectId`, `serverId`, `server`, `template`, `templateId`, `state` (\"creating\", \"running\", \"pausing\", \"paused\", \"resuming\", \"stopping\", \"stopped\" or \"failed\"), `preparing` (true while it waits for its server to make its template), `runtime` (\"firecracker\" or \"gvisor\"), `cpus`, `memoryMib`, `diskMib`, `network`, `isolation`, `labels`, `env` (names only), `url` (its server's daemon), `edgeUrl` (its server's edge proxy, or null), `ports` (each `port`, `public` and `url`), `error`, `createdBy`, `createdAt`, `startedAt`, `expiresAt` (null when it runs until it's deleted), `pausedAt` and `endedAt`. Also `limits` (the team's, `maxTtlSecs` null when there's no limit) and `inUse` (what its sandboxes take now).", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn list(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Value>> {
    let live = match q.state.as_deref() {
        None | Some("live") => true,
        Some("all") => false,
        Some(_) => return Err(ApiError::bad_request("state is \"live\" or \"all\".")),
    };
    let teams = [q.team.clone()];
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    super::require(&mut tx, &user, &q.team, Need::Read).await?;
    let limits = super::limits::load(&mut tx, &q.team).await?;
    let in_use = super::limits::in_use(&mut tx, &q.team).await?;
    let sandboxes =
        super::sandbox_views_matching(&mut *tx, &teams, None, live, q.q.as_deref()).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "sandboxes": sandboxes,
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
/// you) can use only one team. `server` runs it on that server only.
///
/// A server that doesn't have Pyo's template `base` (or `desktop`) yet
/// makes it first: the first sandbox from it there takes a minute or so
/// longer, and later ones start at once.
#[utoipa::path(post, path = "/sandboxes", operation_id = "createSandbox", tag = "Sandboxes",
    request_body(content = CreateSandbox),
    responses(
        (status = 201, description = "`sandbox` (as in `listSandboxes`) and `connection`: `url`, `sandboxId`, `token` and `expiresAt`.", body = Object),
        (status = 400, description = "A size, lifetime or other value Pyo can't accept, or no `teamId` when you can use more than one team.", body = ErrorBody),
        (status = 403, description = "Your role, the API key's access or the team's sandbox limits don't allow it.", body = ErrorBody),
        (status = 404, description = "No such team, template, project or server.", body = ErrorBody),
        (status = 409, description = "No sandbox server can run it: none yet, none with the template, or none with KVM for `isolation: vm`.", body = ErrorBody),
        (status = 502, description = "The server couldn't make `base` or `desktop`. The error says why.", body = ErrorBody),
        (status = 503, description = "Every sandbox server is full or down, the server asked for has no room, or `base` or `desktop` is still being made there. Try again soon.", body = ErrorBody),
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

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Rename {
    /// The new name: up to 63 letters, digits, spaces, dashes, underscores
    /// and dots. It needn't be unique.
    name: String,
}

/// Rename a sandbox
///
/// Gives the sandbox another name. Its id stays the same, so scripts that
/// use it keep working. Works on ended sandboxes too.
#[utoipa::path(patch, path = "/sandboxes/{id}", operation_id = "renameSandbox", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    request_body(content = Rename),
    responses(
        (status = 200, description = "The sandbox, with its new `name`.", body = Object),
        (status = 400, description = "A name Pyo can't accept. The error says why.", body = ErrorBody),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn rename(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Json(b): Json<Rename>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::rename(&s, &user, &id, &b.name).await?))
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
    /// The sandbox now ends this long from now, like "30m", "1h" or "7d",
    /// or seconds. "none": it runs until it's deleted. Within the team's
    /// longest lifetime.
    #[schema(value_type = String)]
    ttl: Value,
}

/// Extend a sandbox
///
/// Sets when the sandbox ends: `ttl` from now, or never with `"none"`.
#[utoipa::path(post, path = "/sandboxes/{id}/extend", operation_id = "extendSandbox", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox's id.")),
    request_body(content = Extend),
    responses(
        (status = 200, description = "The sandbox, with its new `expiresAt` (null when it runs until it's deleted).", body = Object),
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

/// Close a sandbox port
///
/// Stops serving the port at its URL. Closing a port that isn't open is
/// fine.
#[utoipa::path(delete, path = "/sandboxes/{id}/ports/{port}", operation_id = "closeSandboxPort", tag = "Sandboxes",
    params(
        ("id" = String, Path, description = "The sandbox's id."),
        ("port" = u16, Path, description = "The port, like 3000."),
    ),
    responses(
        (status = 200, description = "`port` and `closed`.", body = Object),
        (status = 404, description = "No such sandbox.", body = ErrorBody),
        (status = 409, description = "It has ended.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn close_port(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path((id, port)): Path<(String, u16)>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::close_port(&s, &user, &id, port).await?))
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
        (status = 200, description = "`servers`, each with `id`, `teamId` (null for the installation's), `name`, `address`, `state` (\"joining\", \"ready\", \"unreachable\" or \"refused\"), `enabled`, `runtime` (\"firecracker\" or \"gvisor\", what runs its sandboxes, null until it reports), `capability` (`runtime`, `kvm`, `bootMs`, `arch`, `cpus`, `memory`, `disk`, `gvisorVersion` and the checks), `summary` (one line about it), `capacity`, `templates`, `daemonVersion`, `lastReportAt`, `error`, `sandboxes` (how many run there), `createdAt`, `edgeDomain` and, with an edge domain, `edge`: its `domain`, `https` (whether it serves HTTPS), `cert` (`state`: \"none\", \"issuing\", \"ready\" or \"failed\", `notAfter`, `issuedAt`, `error`, `served`), `cloudflare` (the connected zone that covers it: `teamId`, `zone`, `canEdit`, or null) and `dns` (`name`, the `records` to point it at the server, and what Pyo `made`).", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn list_servers(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<TeamQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    super::require(&mut tx, &user, &q.team, Need::Read).await?;
    let servers = super::server_views(&mut tx, &[q.team], None).await?;
    tx.commit().await?;
    Ok(Json(json!({ "servers": servers })))
}

/// Add a sandbox server
///
/// Makes a server's one-time join code and the command to run on it. The
/// code works once, for 15 minutes. The command joins your tailnet, installs
/// Pyo's sandbox daemon, checks what the server can run (Firecracker where it
/// has KVM, gVisor elsewhere) and joins Pyo. The server then gets its work
/// from Pyo over HTTPS: Pyo never calls it. Adding a server that hasn't
/// joined yet again makes a new code. Team owners and admins. Needs
/// Tailscale. On Pyo Cloud, use createSandboxJoinCode.
#[utoipa::path(post, path = "/sandbox-servers", operation_id = "addSandboxServer", tag = "Sandboxes",
    request_body(content = AddServer),
    responses(
        (status = 200, description = "`server`, `command` (run it on the server as root) and `expiresAt` (when the code runs out).", body = Object),
        (status = 400, description = "A bad name, or Tailscale isn't set up.", body = ErrorBody),
        (status = 409, description = "A server with that name has joined already, Pyo isn't on the tailnet yet, or the server's tailnet can't reach Pyo.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn add_server(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<AddServer>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::add_server(&s, &user, req).await?))
}

/// Buy a sandbox server at a cloud provider
///
/// Makes a server in the team's Hetzner project and sets it up as a sandbox
/// server, the way Pyo makes a cluster's servers there. Read the locations,
/// server types and prices with getCloudOptions first. On its first boot it
/// joins your tailnet and runs sandbox.sh with a one-time code. Hetzner
/// Cloud servers have no KVM, so it runs sandboxes on gVisor. Follow it in
/// the server's `cloud` (creating, booting, joining, ready) and `state`.
/// Without `confirm` it answers 409 with the monthly price. Confirm that
/// with the user, then send it again with `confirm: true`. Team owners and
/// admins. Needs Tailscale and the team's Hetzner connection.
#[utoipa::path(post, path = "/sandbox-servers/cloud", operation_id = "createCloudSandboxServer", tag = "Sandboxes",
    request_body(content = AddCloudServer),
    responses(
        (status = 200, description = "`server` (as listSandboxServers shows it) and `cloudServer` (`id`, `name`, `state`, `monthly`, `currency`).", body = Object),
        (status = 400, description = "A bad name, location or server type, or Tailscale or Hetzner isn't set up.", body = ErrorBody),
        (status = 409, description = "`needsConfirm` with the monthly price, a name in use, a type Hetzner can't make now, or Pyo isn't on the tailnet yet.", body = ErrorBody),
        (status = 502, description = "Hetzner failed.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn add_cloud_server(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<AddCloudServer>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::add_cloud_server(&s, &user, req).await?))
}

/// What a server that joins over HTTPS runs, and when its code runs out.
fn joining_view(j: &super::outbound::Joining, server: Option<Value>) -> Value {
    let base = crate::join::scripts_url_before_tailnet();
    json!({
        "server": server,
        "command": j.command(&base, env!("CARGO_PKG_VERSION")),
        "expiresAt": j.expires_at,
        "pyoUrl": j.pyo_url,
    })
}

/// The signer a hosted Pyo signs sandbox servers' commands with.
fn hosted_signer() -> ApiResult<&'static std::sync::Arc<dyn crate::CommandSigner>> {
    if !crate::hosted() {
        return Err(ApiError::conflict(super::outbound::SELF_HOSTED));
    }
    crate::command_signer()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, super::outbound::NO_SIGNER))
}

async fn server_view(s: &AppState, team: Option<&str>, id: &str) -> ApiResult<Option<Value>> {
    let mut c = s.db.acquire().await?;
    let teams: Vec<String> = team.iter().map(|t| (*t).to_owned()).collect();
    Ok(super::server_views(&mut c, &teams, Some(id))
        .await?
        .into_iter()
        .next())
}

/// Make a sandbox server's join code
///
/// On Pyo Cloud: adds a sandbox server to the team, or makes a new code for
/// one that hasn't joined yet, and the command to run on it. The code works
/// once, for 15 minutes. The command joins your tailnet with a key from your
/// team's Tailscale, installs Pyo's sandbox daemon and joins Pyo over
/// HTTPS. The server then gets its work from Pyo, which never reaches into
/// your tailnet. Team owners and admins. Needs Tailscale on Integrations.
#[utoipa::path(post, path = "/sandbox-servers/join-codes", operation_id = "createSandboxJoinCode", tag = "Sandboxes",
    request_body(content = super::outbound::CreateJoinCode),
    responses(
        (status = 200, description = "`server`, `command` (run it on the server as root), `expiresAt` (when the code runs out) and `pyoUrl`.", body = Object),
        (status = 400, description = "A bad name, or Tailscale isn't set up.", body = ErrorBody),
        (status = 409, description = "A server with that name has joined already, the cluster that keeps your Tailscale isn't connected, or this Pyo adds sandbox servers with addSandboxServer.", body = ErrorBody),
        (status = 429, description = "Too many codes for the team in an hour.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn create_join_code(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(req): Json<super::outbound::CreateJoinCode>,
) -> ApiResult<Json<Value>> {
    let signer = hosted_signer()?;
    let j = super::outbound::create_join_code(&s, signer.as_ref(), &user, req).await?;
    let server = server_view(&s, j.team_id.as_deref(), &j.server_id).await?;
    Ok(Json(joining_view(&j, server)))
}

/// Reset a sandbox server's access
///
/// The server's key stops working at once, and its waiting commands fail.
/// Answers with a new command for the same server, which works once, for 15
/// minutes. Run it there to join again. On a self-hosted Pyo, a server that
/// shows `needsUpdate` gets the command that updates it instead: it keeps
/// working until the command runs there. Team owners and admins, or the
/// installation's admin for its own servers.
#[utoipa::path(post, path = "/sandbox-servers/{id}/reset-access", operation_id = "resetSandboxServerAccess", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox server's id, `sbs_…`.")),
    responses(
        (status = 200, description = "`server`, `command` (run it on the server as root), `expiresAt` (when the code runs out) and `pyoUrl`.", body = Object),
        (status = 404, description = "No such sandbox server.", body = ErrorBody),
        (status = 409, description = "On Pyo Cloud, the server doesn't join over HTTPS. Or the cluster that keeps your Tailscale isn't connected, or the server's tailnet can't reach Pyo.", body = ErrorBody),
        (status = 429, description = "Too many codes for the team in an hour.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn reset_access(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let signer = super::outbound::signer::get()
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, super::outbound::NO_SIGNER))?;
    let j = super::outbound::reset_access(&s, signer.as_ref(), &user, &id, &super::outbound::Mode::now()).await?;
    let server = server_view(&s, j.team_id.as_deref(), &j.server_id).await?;
    Ok(Json(joining_view(&j, server)))
}

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct RemoveQuery {
    /// Delete a server Pyo made at a cloud provider: without it the answer
    /// is 409 with the monthly cost that stops.
    #[serde(default)]
    confirm: bool,
}

/// Remove a sandbox server
///
/// Deletes its sandboxes where it still answers, and forgets it. To take the
/// daemon off the server, run `sandbox.sh --uninstall` there. A server Pyo
/// made at a cloud provider leaves your tailnet and is deleted there, which
/// takes `confirm`.
#[utoipa::path(delete, path = "/sandbox-servers/{id}", operation_id = "removeSandboxServer", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox server's id, `sbs_…`."), RemoveQuery),
    responses(
        (status = 200, description = "`ok`, `stopped` (how many sandboxes ended) and `deleting` (whether it's being deleted at the provider).", body = Object),
        (status = 404, description = "No such sandbox server.", body = ErrorBody),
        (status = 409, description = "`needsConfirm`: it's deleted at the provider, with the monthly cost that stops.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn remove_server(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    Query(q): Query<RemoveQuery>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::remove_server(&s, &user, &id, q.confirm).await?))
}

/// Get a sandbox server's certificate
///
/// Gets a Let's Encrypt certificate for `*.<edge domain>` with a DNS-01
/// challenge in the team's Cloudflare connection, and puts it on the
/// server, whose edge serves it at once. It runs in the background: the
/// server's `edge.cert.state` says how it goes. Pyo renews it 30 days
/// before it runs out. Also renews it now. The zone must be connected and
/// editable. Team owners and admins, or the installation's admin for its
/// own servers.
#[utoipa::path(post, path = "/sandbox-servers/{id}/certificate", operation_id = "issueSandboxServerCertificate", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox server's id, `sbs_…`.")),
    request_body(content = super::edgecert::EdgeRequest),
    responses(
        (status = 200, description = "`state`: \"issuing\".", body = Object),
        (status = 400, description = "The server has no edge domain, or no connected Cloudflare zone covers it.", body = ErrorBody),
        (status = 409, description = "Pyo is getting it already.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn issue_certificate(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    body: Option<Json<super::edgecert::EdgeRequest>>,
) -> ApiResult<Json<Value>> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    Ok(Json(super::edgecert::issue(&s, &user, &id, req).await?))
}

/// Create a sandbox server's DNS record
///
/// Creates the wildcard record `*.<edge domain>` in the team's Cloudflare
/// connection: A (and AAAA) to the public addresses the server reports,
/// not proxied. Records already on the name are never changed: another one
/// there is a conflict. Team owners and admins, or the installation's
/// admin for its own servers.
#[utoipa::path(post, path = "/sandbox-servers/{id}/dns-record", operation_id = "createSandboxServerDnsRecord", tag = "Sandboxes",
    params(("id" = String, Path, description = "The sandbox server's id, `sbs_…`.")),
    request_body(content = super::edgecert::EdgeRequest),
    responses(
        (status = 200, description = "`ok`, `status` (\"created\", \"exists\" or \"conflict\") and `existing` (the records in the way).", body = Object),
        (status = 400, description = "The server has no edge domain, or no connected Cloudflare zone covers it.", body = ErrorBody),
        (status = 409, description = "The server reports no public address.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("full"))))]
pub(crate) async fn create_dns_record(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
    body: Option<Json<super::edgecert::EdgeRequest>>,
) -> ApiResult<Json<Value>> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    Ok(Json(
        super::edgecert::create_dns(&s, &user, &id, req).await?,
    ))
}

/// Sandbox templates
///
/// What a team's sandboxes can start from: its own templates and the
/// installation's.
#[utoipa::path(get, path = "/sandbox-templates", operation_id = "listSandboxTemplates", tag = "Sandboxes",
    params(TeamQuery),
    responses(
        (status = 200, description = "`templates`, each with `id`, `teamId` (null for the installation's), `name`, `source` (`kind` and `image`), `state` (\"pending\", \"building\", \"ready\" or \"failed\", from its servers, and \"unavailable\" for `base` or `desktop` while Pyo has no image for it), `builtin`, `servers`, `error` and `createdAt`. Pyo's own `base` and `desktop` come first, are always listed and are ready to use: servers make them on first use, so their `servers` are those that have them. Each of the other templates' `servers` is a sandbox server that may use it, with `id`, `name`, `state` (\"pending\", \"building\", \"ready\" or \"failed\"), `phase` while building (\"queued\", \"pulling\", \"unpacking\" or \"snapshotting\"), `pulled` and `unpacked` of `imageBytes` (asked of the server as the list is read, while it builds), `pullMs`, `unpackMs` and `snapshotMs`, `bytes` (disk it takes there), `snapshots` and `error`.", body = Object),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn list_templates(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<TeamQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    super::require(&mut tx, &user, &q.team, Need::Read).await?;
    let rows = super::stored_template_rows(&mut *tx, &[q.team], None).await?;
    tx.commit().await?;
    let rows = super::with_builtins(&s.db, None, rows).await;
    Ok(Json(
        json!({ "templates": super::live_views(&s, rows).await }),
    ))
}

/// Create a sandbox template
///
/// A template made from an OCI image, like "node:22", or an image in Pyo's
/// registry. Every sandbox server that may use it pulls the image and makes
/// a root disk from it. A Firecracker server boots it once and snapshots
/// it, and a gVisor server starts sandboxes from the disk. The template is
/// ready once one server has it.
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

/// Delete a sandbox template
///
/// Deletes it here and on the sandbox servers. Refused while sandboxes run
/// from it.
#[utoipa::path(delete, path = "/sandbox-templates/{id}", operation_id = "deleteSandboxTemplate", tag = "Sandboxes",
    params(("id" = String, Path, description = "The template's id, `stp_…`.")),
    responses(
        (status = 200, description = "`ok`.", body = Object),
        (status = 404, description = "No such template.", body = ErrorBody),
        (status = 409, description = "Sandboxes still run from it.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("sandboxes"))))]
pub(crate) async fn delete_template(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(super::delete_template(&s, &user, &id).await?))
}
