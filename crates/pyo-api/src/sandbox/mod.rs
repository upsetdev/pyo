//! Pyo Sandbox, the control plane: records, keys, limits, metering and the
//! scheduler. Sandboxes run on dedicated sandbox servers (the `pyo-sandbox`
//! daemon), which clients then talk to directly with a short-lived token
//! Pyo signs, so Pyo isn't in the hot path.
//!
//! - [`keys`]: the installation's signing key and the tokens.
//! - [`join`]: a sandbox server's one-time join code and sandbox.sh.
//! - [`scheduler`]: which server a new sandbox runs on.
//! - [`limits`]: per-team caps, checked on create.
//! - [`daemon`]: Pyo's calls to a daemon over the tailnet.
//! - [`runner`]: reads each server's report, keeps the records in step,
//!   meters usage and ends sandboxes past their lifetime.
//! - [`api`]: the REST endpoints. [`mcp`]: the assistants' tools.
//!
//! Sandboxes belong to a team, with an optional project as a label. Sandbox
//! servers and templates belong to a team, or to the installation (no team),
//! which every team may use. Nothing here runs or shows until the first
//! sandbox server exists ([`available`]).

pub mod api;
pub mod daemon;
#[cfg(test)]
mod e2e;
pub mod join;
pub mod keys;
pub mod limits;
pub mod mcp;
pub mod runner;
pub mod scheduler;

use axum::http::StatusCode;
use pyo_proto::sandbox::{
    self as proto, DaemonCommand, DaemonError, ErrorCode, Isolation, NetworkMode, SandboxSpec,
    SandboxState, SandboxStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::{
    AppState,
    auth::{KeyAccess, User},
    error::{ApiError, ApiResult},
    ops::{Role, require_role},
};

/// "1h", "1h30m", "90s".
pub fn format_ttl(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if m > 0 {
        out.push_str(&format!("{m}m"));
    }
    if s > 0 || out.is_empty() {
        out.push_str(&format!("{s}s"));
    }
    out
}

/// Whether any of these teams can use a sandbox server: its own, or the
/// installation's. Until then the sandbox tools stay hidden.
pub async fn available(db: &PgPool, team_ids: &[String]) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.sandbox_servers WHERE team_id IS NULL OR team_id = ANY($1))",
    )
    .bind(team_ids)
    .fetch_one(db)
    .await
}

/// The teams a user (or an API key) can see.
pub async fn teams_of(db: &PgPool, user: &User) -> sqlx::Result<Vec<String>> {
    let mut ids: Vec<String> = sqlx::query_scalar(
        "SELECT team_id FROM pyo.members WHERE user_id = $1 AND status = 'active'",
    )
    .bind(&user.id)
    .fetch_all(db)
    .await?;
    if let Some(allowed) = user.key.as_ref().and_then(|k| k.teams.as_ref()) {
        ids.retain(|t| allowed.contains(t));
    }
    Ok(ids)
}

// ── Who may do what ────────────────────────────────────────────────────────

/// What a call needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// See sandboxes, servers and templates: any member, any key.
    Read,
    /// Create and use sandboxes: developers, with a "sandboxes" or full key.
    Use,
    /// Add and remove sandbox servers: owners and admins, never a key that
    /// isn't full.
    Manage,
}

/// Whether an API key's access covers `need`. None: allowed.
pub fn key_refusal(access: KeyAccess, need: Need) -> Option<&'static str> {
    match (need, access) {
        (Need::Read, _) | (_, KeyAccess::Full) | (Need::Use, KeyAccess::Sandboxes) => None,
        (_, KeyAccess::Read) => Some("This API key can only read."),
        (_, KeyAccess::Sandboxes) => Some("This API key can only use sandboxes."),
        (_, KeyAccess::Deploy) => Some(
            "This API key can only deploy, roll back, restart, stop and scale services. Sandboxes need a sandboxes or full key.",
        ),
    }
}

/// The team role and API key access `need` takes.
pub async fn require(
    tx: &mut PgConnection,
    user: &User,
    team_id: &str,
    need: Need,
) -> ApiResult<()> {
    if let Some(k) = &user.key
        && let Some(why) = key_refusal(k.access, need)
    {
        return Err(ApiError::forbidden(why));
    }
    let role = role_in(tx, user, team_id).await?;
    let min = match need {
        Need::Read => Role::Viewer,
        Need::Use => Role::Developer,
        Need::Manage => Role::Admin,
    };
    if role < min {
        return Err(ApiError::forbidden(match need {
            Need::Manage => "Only team owners and admins can do this.",
            _ => "Viewers can't use sandboxes.",
        }));
    }
    Ok(())
}

/// The user's role in a team, and the key's team limit. Read-only keys
/// pass here: [`require`] checks the access.
async fn role_in(tx: &mut PgConnection, user: &User, team_id: &str) -> ApiResult<Role> {
    let mut viewer = user.clone();
    // require_role refuses a read-only key for more than viewing, so ask
    // it for viewing and compare the role here.
    if let Some(k) = viewer.key.as_mut() {
        k.access = KeyAccess::Full;
    }
    let mut t: Transaction<'_, Postgres> = sqlx::Acquire::begin(tx).await?;
    let role = require_role(&mut t, &viewer, team_id, Role::Viewer).await?;
    t.commit().await?;
    Ok(role)
}

/// The installation's own servers and templates: its admin, in the
/// dashboard (installation settings aren't for API keys).
fn require_install_admin(user: &User) -> ApiResult<()> {
    if user.is_admin && user.key.is_none() {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "Only the installation's admin can add the installation's own sandbox servers and templates, from the dashboard.",
        ))
    }
}

/// A daemon's error as the API's. A token the server refuses is Pyo's
/// problem, not the caller's.
pub fn daemon_error(e: DaemonError) -> ApiError {
    let status = match e.code {
        ErrorCode::Unauthorized | ErrorCode::Forbidden => {
            return ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("The sandbox server refused Pyo's token. {}", e.message),
            );
        }
        c => StatusCode::from_u16(c.http_status()).unwrap_or(StatusCode::BAD_GATEWAY),
    };
    ApiError::new(status, e.message)
}

fn state_name(s: SandboxState) -> &'static str {
    match s {
        SandboxState::Creating => "creating",
        SandboxState::Running => "running",
        SandboxState::Pausing => "pausing",
        SandboxState::Paused => "paused",
        SandboxState::Resuming => "resuming",
        SandboxState::Stopping => "stopping",
        SandboxState::Stopped => "stopped",
        SandboxState::Failed => "failed",
    }
}

fn runtime_name(r: proto::Runtime) -> Option<&'static str> {
    match r {
        proto::Runtime::Firecracker => Some("firecracker"),
        proto::Runtime::Gvisor => Some("gvisor"),
        proto::Runtime::None => None,
    }
}

/// Saves what a daemon says of a sandbox.
pub async fn save_status(db: &mut PgConnection, st: &SandboxStatus) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE pyo.sandboxes SET state = $2, runtime = coalesce($3, runtime), error = $4,
                started_at = coalesce(started_at, to_timestamp($5::double precision / 1000)),
                expires_at = to_timestamp($6::double precision / 1000),
                paused_at = CASE WHEN $2 = 'paused' THEN coalesce(paused_at, now()) ELSE NULL END,
                ended_at = CASE WHEN $2 IN ('stopped', 'failed') THEN coalesce(ended_at, now()) ELSE NULL END
          WHERE id = $1",
    )
    .bind(&st.id)
    .bind(state_name(st.state))
    .bind(runtime_name(st.runtime))
    .bind(&st.error)
    .bind(st.started_at.map(|t| t as f64))
    .bind(st.expires_at as f64)
    .execute(db)
    .await?;
    Ok(())
}

/// Marks a sandbox gone.
pub async fn end(
    db: &mut PgConnection,
    id: &str,
    state: &str,
    error: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE pyo.sandboxes SET state = $2, error = coalesce($3, error), paused_at = NULL, ended_at = coalesce(ended_at, now())
          WHERE id = $1 AND state NOT IN ('stopped', 'failed')",
    )
    .bind(id)
    .bind(state)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

// ── Views ──────────────────────────────────────────────────────────────────

/// Sandboxes as the API shows them. Env values are never kept, only names.
pub async fn sandbox_views(
    db: &PgPool,
    team_ids: &[String],
    id: Option<&str>,
    live_only: bool,
) -> sqlx::Result<Vec<Value>> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'id', x.id, 'teamId', x.team_id, 'projectId', x.project_id,
            'serverId', x.server_id, 'server', sv.name,
            'template', coalesce(t.name, x.spec->>'template'), 'templateId', x.template_id,
            'state', x.state, 'runtime', x.runtime,
            'cpus', x.spec->'cpus', 'memoryMib', x.spec->'memoryMib', 'diskMib', x.spec->'diskMib',
            'network', x.spec->'network', 'isolation', x.spec->'isolation',
            'labels', coalesce(x.spec->'labels', '{}'),
            'pauseWhenIdleSecs', x.spec->'pauseWhenIdleSecs',
            'env', (SELECT coalesce(jsonb_agg(k ORDER BY k), '[]') FROM jsonb_object_keys(coalesce(x.spec->'env', '{}')) k),
            'url', sv.address,
            'error', x.error, 'createdBy', x.created_by,
            'createdAt', pyo.ms(x.created_at), 'startedAt', pyo.ms(x.started_at),
            'expiresAt', pyo.ms(x.expires_at), 'pausedAt', pyo.ms(x.paused_at), 'endedAt', pyo.ms(x.ended_at))
           FROM pyo.sandboxes x
           LEFT JOIN pyo.sandbox_servers sv ON sv.id = x.server_id
           LEFT JOIN pyo.sandbox_templates t ON t.id = x.template_id
          WHERE x.team_id = ANY($1) AND ($2::text IS NULL OR x.id = $2)
            AND (NOT $3 OR x.state NOT IN ('stopped', 'failed'))
          ORDER BY x.created_at DESC LIMIT 500",
    )
    .bind(team_ids)
    .bind(id)
    .bind(live_only)
    .fetch_all(db)
    .await
    .map(|mut rows: Vec<Value>| {
        // The server's tailnet address as its daemon's URL.
        for r in &mut rows {
            r["url"] = match r["url"].as_str() {
                Some(a) => json!(daemon::base_url(a)),
                None => Value::Null,
            };
        }
        rows
    })
}

/// Sandbox servers as the API shows them, with the one line the dashboard
/// shows next to the runtime badge.
pub async fn server_views(
    db: &PgPool,
    team_ids: &[String],
    id: Option<&str>,
) -> sqlx::Result<Vec<Value>> {
    let mut rows: Vec<Value> = sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'id', s.id, 'teamId', s.team_id, 'name', s.name, 'address', s.address,
            'state', s.state, 'enabled', s.enabled,
            'capability', s.capability, 'capacity', s.capacity, 'templates', s.templates,
            'daemonVersion', s.daemon_version, 'lastReportAt', pyo.ms(s.last_report_at),
            'error', s.error, 'createdBy', s.created_by, 'createdAt', pyo.ms(s.created_at),
            'joinedAt', pyo.ms(s.joined_at),
            'sandboxes', (SELECT count(*) FROM pyo.sandboxes x WHERE x.server_id = s.id AND x.state NOT IN ('stopped', 'failed')))
           FROM pyo.sandbox_servers s
          WHERE (s.team_id IS NULL OR s.team_id = ANY($1)) AND ($2::text IS NULL OR s.id = $2)
          ORDER BY s.team_id NULLS FIRST, s.name",
    )
    .bind(team_ids)
    .bind(id)
    .fetch_all(db)
    .await?;
    for r in &mut rows {
        let summary = serde_json::from_value::<proto::CapabilityReport>(r["capability"].clone())
            .ok()
            .map(|c| c.summary());
        r["summary"] = json!(summary);
    }
    Ok(rows)
}

pub async fn template_views(
    db: &PgPool,
    team_ids: &[String],
    id: Option<&str>,
) -> sqlx::Result<Vec<Value>> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
            'id', t.id, 'teamId', t.team_id, 'name', t.name, 'source', t.source, 'state', t.state,
            'snapshots', t.snapshots, 'error', t.error, 'createdBy', t.created_by,
            'createdAt', pyo.ms(t.created_at), 'updatedAt', pyo.ms(t.updated_at),
            'servers', (SELECT coalesce(jsonb_agg(s.id ORDER BY s.name), '[]') FROM pyo.sandbox_servers s
                         WHERE (s.team_id IS NULL OR s.team_id = t.team_id OR t.team_id IS NULL)
                           AND s.templates @> jsonb_build_array(jsonb_build_object('name', t.id))))
           FROM pyo.sandbox_templates t
          WHERE (t.team_id IS NULL OR t.team_id = ANY($1)) AND ($2::text IS NULL OR t.id = $2)
          ORDER BY t.team_id NULLS FIRST, t.name",
    )
    .bind(team_ids)
    .bind(id)
    .fetch_all(db)
    .await
}

// ── Create ─────────────────────────────────────────────────────────────────

/// What a client asks for. Everything but the team and template has a
/// default.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateSandbox {
    pub team_id: String,
    /// A sandbox template's name (or id): the team's own, else the
    /// installation's.
    pub template: String,
    /// 1 when omitted.
    #[serde(default)]
    pub cpus: Option<u32>,
    /// 512 when omitted.
    #[serde(default)]
    pub memory_mib: Option<u32>,
    /// The writable disk, sparse. 2048 when omitted.
    #[serde(default)]
    pub disk_mib: Option<u32>,
    /// How long it lives, like "30m" or "1h", or seconds. 5 minutes when
    /// omitted.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub ttl: Option<Value>,
    /// Pause after this many seconds without use. Kept for now, acted on in
    /// a later release.
    #[serde(default)]
    pub pause_when_idle_secs: Option<u64>,
    /// Environment variables. Pyo passes them on and keeps only the names.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// "internet" (the default) or "none".
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub network: Option<NetworkMode>,
    /// "auto" (the default: Firecracker where the server has KVM, gVisor
    /// elsewhere) or "vm" (Firecracker only).
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub isolation: Option<Isolation>,
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    /// A project of the team, as a label.
    #[serde(default)]
    pub project_id: Option<String>,
}

/// A lifetime given as "30m", "1h" or seconds.
pub fn parse_ttl_value(v: Option<&Value>) -> ApiResult<u64> {
    let bad =
        || ApiError::bad_request("The lifetime is like \"30m\", \"1h\" or a number of seconds.");
    let secs = match v {
        None | Some(Value::Null) => proto::DEFAULT_TTL_SECS,
        Some(Value::Number(n)) => n.as_u64().ok_or_else(bad)?,
        Some(Value::String(s)) => proto::parse_ttl(s).ok_or_else(bad)?,
        Some(_) => return Err(bad()),
    };
    if !(1..=proto::MAX_TTL_SECS).contains(&secs) {
        return Err(ApiError::bad_request(
            "The lifetime must be 1 second to 24 hours.",
        ));
    }
    Ok(secs)
}

/// The template a name stands for: the team's, else the installation's.
async fn template_for(
    tx: &mut PgConnection,
    team_id: &str,
    name: &str,
) -> ApiResult<(String, String, String)> {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, state FROM pyo.sandbox_templates
          WHERE (team_id = $1 OR team_id IS NULL) AND (name = $2 OR id = $2)
          ORDER BY team_id NULLS LAST LIMIT 1",
    )
    .bind(team_id)
    .bind(name)
    .fetch_optional(tx)
    .await?;
    let (id, name, state) = row.ok_or_else(|| ApiError::not_found("Sandbox template"))?;
    if state == "failed" {
        return Err(ApiError::conflict(format!(
            "The sandbox template {name} failed to build. Make it again."
        )));
    }
    Ok((id, name, state))
}

/// The servers a team's sandbox can go to, with their addresses.
pub async fn candidates(
    tx: &mut PgConnection,
    team_id: &str,
) -> ApiResult<Vec<(scheduler::Candidate, String)>> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        address: Option<String>,
        state: String,
        enabled: bool,
        capability: Option<Value>,
        capacity: Option<Value>,
        templates: Value,
        age: Option<i64>,
        pending_mib: i64,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.id, s.address, s.state, s.enabled, s.capability, s.capacity, s.templates,
                extract(epoch FROM now() - s.last_report_at)::bigint AS age,
                coalesce((SELECT sum((x.spec->>'memoryMib')::bigint) FROM pyo.sandboxes x
                           WHERE x.server_id = s.id AND x.state NOT IN ('stopped', 'failed', 'paused')
                             AND (x.state IN ('creating', 'resuming') OR s.last_report_at IS NULL OR x.created_at > s.last_report_at)), 0)::bigint AS pending_mib
           FROM pyo.sandbox_servers s
          WHERE s.team_id IS NULL OR s.team_id = $1",
    )
    .bind(team_id)
    .fetch_all(tx)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let c = scheduler::Candidate {
                id: r.id,
                enabled: r.enabled,
                ready: r.state == "ready" && r.address.is_some(),
                report_age_secs: r.age.unwrap_or(i64::MAX),
                capability: r
                    .capability
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default(),
                capacity: r
                    .capacity
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default(),
                templates: serde_json::from_value(r.templates).unwrap_or_default(),
                pending_bytes: (r.pending_mib.max(0) as u64) << 20,
            };
            (c, r.address.unwrap_or_default())
        })
        .collect())
}

/// A new sandbox ID.
pub fn new_sandbox_id() -> String {
    let mut b = [0u8; 16];
    rand::fill(&mut b);
    proto::sandbox_id_from(b)
}

/// Where a client reaches its sandbox, and with what.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// The sandbox server's daemon, on your tailnet.
    pub url: String,
    pub sandbox_id: String,
    /// `Authorization: Bearer <token>` for exec, terminals, files and ports.
    pub token: String,
    /// When the token runs out (epoch milliseconds). Ask for a new one with
    /// `createSandboxToken` before.
    pub expires_at: u64,
}

async fn connection(
    s: &AppState,
    sandbox: &str,
    team: &str,
    server: &str,
    address: &str,
) -> ApiResult<Connection> {
    let key = keys::signing_key(&s.db, &s.sealer).await?;
    let t = keys::client_token(&key, sandbox, team, server);
    Ok(Connection {
        url: daemon::base_url(address),
        sandbox_id: sandbox.to_owned(),
        token: t.token,
        expires_at: t.expires_at,
    })
}

/// Creates a sandbox: checks the limits, picks a server, records it, then
/// asks the server. Returns the sandbox and how to reach it.
pub async fn create(s: &AppState, user: &User, req: CreateSandbox) -> ApiResult<Value> {
    let ttl_secs = parse_ttl_value(req.ttl.as_ref())?;
    let mut tx = s.db.begin().await?;
    require(&mut tx, user, &req.team_id, Need::Use).await?;
    // One create at a time per team, so two can't both fit the last slot.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('pyo-sandbox:' || $1))")
        .bind(&req.team_id)
        .execute(&mut *tx)
        .await?;
    let (template_id, template_name, _) =
        template_for(&mut tx, &req.team_id, req.template.trim()).await?;
    if let Some(p) = &req.project_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.projects WHERE id = $1 AND team_id = $2)",
        )
        .bind(p)
        .bind(&req.team_id)
        .fetch_one(&mut *tx)
        .await?;
        if !ok {
            return Err(ApiError::not_found("Project"));
        }
    }
    let mut spec = SandboxSpec::new(new_sandbox_id(), &req.team_id, &template_id);
    spec.cpus = req.cpus.unwrap_or(proto::DEFAULT_CPUS);
    spec.memory_mib = req.memory_mib.unwrap_or(proto::DEFAULT_MEMORY_MIB);
    spec.disk_mib = req.disk_mib.unwrap_or(proto::DEFAULT_DISK_MIB);
    spec.ttl_secs = ttl_secs;
    spec.pause_when_idle_secs = req.pause_when_idle_secs;
    spec.env = req.env;
    spec.network = req.network.unwrap_or_default();
    spec.isolation = req.isolation.unwrap_or_default();
    spec.labels = req.labels;
    spec.validate().map_err(ApiError::bad_request)?;
    let lim = limits::load(&mut tx, &req.team_id).await?;
    let used = limits::in_use(&mut tx, &req.team_id).await?;
    limits::check(&lim, &used, &spec).map_err(ApiError::forbidden)?;
    let servers = candidates(&mut tx, &req.team_id).await?;
    let list: Vec<scheduler::Candidate> = servers.iter().map(|(c, _)| c.clone()).collect();
    let chosen = scheduler::pick(&list, &spec).map_err(|n| {
        let n = match n {
            scheduler::NoServer::NoTemplate(_) => {
                scheduler::NoServer::NoTemplate(template_name.clone())
            }
            n => n,
        };
        ApiError::new(n.status(), n.message())
    })?;
    let address = servers
        .iter()
        .find(|(c, _)| c.id == chosen.id)
        .map(|(_, a)| a.clone())
        .unwrap_or_default();
    let server_id = chosen.id.clone();
    // Kept without env values: they may be secrets.
    let mut kept = spec.clone();
    for v in kept.env.values_mut() {
        v.clear();
    }
    sqlx::query(
        "INSERT INTO pyo.sandboxes (id, team_id, project_id, server_id, template_id, spec, state, created_by, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, 'creating', $7, now() + make_interval(secs => $8))",
    )
    .bind(&spec.id)
    .bind(&req.team_id)
    .bind(&req.project_id)
    .bind(&server_id)
    .bind(&template_id)
    .bind(json!(kept))
    .bind(&user.name)
    .bind(ttl_secs as f64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let key = keys::signing_key(&s.db, &s.sealer).await?;
    let token = keys::create_token(&key, &spec.id, &req.team_id, &server_id);
    let d = daemon::Daemon::new(&address);
    let st = match d
        .sandbox(&token, &DaemonCommand::Create { spec: spec.clone() })
        .await
    {
        Ok(st) => st,
        Err(e) => {
            let mut c = s.db.acquire().await?;
            end(&mut c, &spec.id, "failed", Some(&e.message)).await?;
            return Err(daemon_error(e));
        }
    };
    let mut c = s.db.acquire().await?;
    save_status(&mut c, &st).await?;
    let conn = connection(s, &spec.id, &req.team_id, &server_id, &address).await?;
    let teams = [req.team_id.clone()];
    let view = sandbox_views(&s.db, &teams, Some(&spec.id), false)
        .await?
        .into_iter()
        .next();
    Ok(json!({ "sandbox": view, "connection": conn }))
}

// ── One sandbox ────────────────────────────────────────────────────────────

/// A sandbox's team, server and address, after checking `need`.
pub struct Found {
    pub id: String,
    pub team_id: String,
    pub server_id: Option<String>,
    pub address: Option<String>,
    pub state: String,
}

pub async fn find(s: &AppState, user: &User, id: &str, need: Need) -> ApiResult<Found> {
    let row: Option<(String, Option<String>, Option<String>, String)> = sqlx::query_as(
        "SELECT x.team_id, x.server_id, sv.address, x.state FROM pyo.sandboxes x
           LEFT JOIN pyo.sandbox_servers sv ON sv.id = x.server_id WHERE x.id = $1",
    )
    .bind(id)
    .fetch_optional(&s.db)
    .await?;
    let (team_id, server_id, address, state) = row.ok_or_else(|| ApiError::not_found("Sandbox"))?;
    let mut c = s.db.acquire().await?;
    require(&mut c, user, &team_id, need).await?;
    Ok(Found {
        id: id.to_owned(),
        team_id,
        server_id,
        address,
        state,
    })
}

impl Found {
    fn is_final(&self) -> bool {
        matches!(self.state.as_str(), "stopped" | "failed")
    }

    /// The server's daemon and an admin token for it, while the sandbox
    /// lives.
    pub async fn daemon(&self, s: &AppState) -> ApiResult<(daemon::Daemon, String, String)> {
        if self.is_final() {
            return Err(ApiError::conflict(format!(
                "The sandbox {} has ended.",
                self.id
            )));
        }
        let (Some(server), Some(address)) = (&self.server_id, &self.address) else {
            return Err(ApiError::conflict("The sandbox's server was removed."));
        };
        let key = keys::signing_key(&s.db, &s.sealer).await?;
        Ok((
            daemon::Daemon::new(address),
            keys::admin_token(&key, server),
            server.clone(),
        ))
    }

    /// A client token for this sandbox, for Pyo's own calls on a user's
    /// behalf (the assistants' tools).
    pub async fn client(&self, s: &AppState) -> ApiResult<(daemon::Daemon, String)> {
        let (d, _, server) = self.daemon(s).await?;
        let key = keys::signing_key(&s.db, &s.sealer).await?;
        Ok((
            d,
            keys::client_token(&key, &self.id, &self.team_id, &server).token,
        ))
    }
}

async fn view(s: &AppState, f: &Found) -> ApiResult<Value> {
    let teams = [f.team_id.clone()];
    sandbox_views(&s.db, &teams, Some(&f.id), false)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ApiError::not_found("Sandbox"))
}

/// Deletes a sandbox. When its server can't be reached, it's marked
/// stopping and the runner tries again. The server also ends it when its
/// lifetime runs out.
pub async fn delete(s: &AppState, user: &User, id: &str) -> ApiResult<Value> {
    let f = find(s, user, id, Need::Use).await?;
    if f.is_final() {
        return view(s, &f).await;
    }
    let mut c = s.db.acquire().await?;
    // Its server was removed: nothing is left to end.
    if f.server_id.is_none() || f.address.is_none() {
        end(&mut c, id, "stopped", None).await?;
        return view(s, &f).await;
    }
    let (d, token, _) = f.daemon(s).await?;
    sqlx::query("UPDATE pyo.sandboxes SET state = 'stopping' WHERE id = $1 AND state NOT IN ('stopped', 'failed')")
        .bind(id)
        .execute(&mut *c)
        .await?;
    match d
        .command(&token, &DaemonCommand::Delete { id: id.to_owned() })
        .await
    {
        Ok(_) => end(&mut c, id, "stopped", None).await?,
        Err(e) if e.code == ErrorCode::NotFound => end(&mut c, id, "stopped", None).await?,
        Err(e) => {
            tracing::warn!(%id, error = %e, "sandbox delete: the server didn't answer, trying again later")
        }
    }
    view(s, &f).await
}

pub async fn pause(s: &AppState, user: &User, id: &str) -> ApiResult<Value> {
    lifecycle(s, user, id, |id| DaemonCommand::Pause { id }).await
}

pub async fn resume(s: &AppState, user: &User, id: &str) -> ApiResult<Value> {
    lifecycle(s, user, id, |id| DaemonCommand::Resume { id }).await
}

/// The sandbox ends `ttl` from now instead, within the team's longest.
pub async fn extend(s: &AppState, user: &User, id: &str, ttl: Option<&Value>) -> ApiResult<Value> {
    let ttl_secs = parse_ttl_value(ttl)?;
    let f = find(s, user, id, Need::Use).await?;
    let mut c = s.db.acquire().await?;
    let lim = limits::load(&mut c, &f.team_id).await?;
    limits::check_ttl(&lim, ttl_secs).map_err(ApiError::forbidden)?;
    drop(c);
    lifecycle(s, user, id, move |id| DaemonCommand::SetTtl {
        id,
        ttl_secs,
    })
    .await
}

async fn lifecycle(
    s: &AppState,
    user: &User,
    id: &str,
    cmd: impl FnOnce(String) -> DaemonCommand,
) -> ApiResult<Value> {
    let f = find(s, user, id, Need::Use).await?;
    let (d, token, _) = f.daemon(s).await?;
    let st = d
        .sandbox(&token, &cmd(id.to_owned()))
        .await
        .map_err(daemon_error)?;
    let mut c = s.db.acquire().await?;
    save_status(&mut c, &st).await?;
    view(s, &f).await
}

/// Opens a sandbox port on its server's edge proxy and returns its URL. A
/// public port answers anyone. A private one needs a sandbox token, which
/// a browser can bring once as `?pyo_token=`.
pub async fn open_port(
    s: &AppState,
    user: &User,
    id: &str,
    port: u16,
    public: bool,
) -> ApiResult<Value> {
    if port == 0 {
        return Err(ApiError::bad_request("The port must be 1 to 65535."));
    }
    let f = find(s, user, id, Need::Use).await?;
    let (d, token, server) = f.daemon(s).await?;
    let capability: Option<Value> =
        sqlx::query_scalar("SELECT capability FROM pyo.sandbox_servers WHERE id = $1")
            .bind(&server)
            .fetch_optional(&s.db)
            .await?
            .flatten();
    let cap: proto::CapabilityReport = capability
        .and_then(|c| serde_json::from_value(c).ok())
        .unwrap_or_default();
    let Some(url) = cap.port_url(id, port) else {
        return Err(ApiError::conflict(
            "The sandbox's server has no edge proxy, so its ports can't be reached from outside yet. Reach the port from inside the sandbox with exec.",
        ));
    };
    let st = d
        .sandbox(
            &token,
            &DaemonCommand::OpenPort {
                id: id.to_owned(),
                port,
                public,
            },
        )
        .await
        .map_err(daemon_error)?;
    let mut c = s.db.acquire().await?;
    save_status(&mut c, &st).await?;
    Ok(json!({ "url": url, "port": port, "public": public }))
}

/// A new client token for a sandbox: the SDK asks before the old one runs
/// out.
pub async fn token(s: &AppState, user: &User, id: &str) -> ApiResult<Connection> {
    let f = find(s, user, id, Need::Use).await?;
    let (_, _, server) = f.daemon(s).await?;
    connection(
        s,
        id,
        &f.team_id,
        &server,
        f.address.as_deref().unwrap_or_default(),
    )
    .await
}

// ── Servers and templates ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AddServer {
    /// The team whose server it is. Left out: the installation's, which
    /// every team may use (the installation's admin, in the dashboard).
    #[serde(default)]
    pub team_id: Option<String>,
    /// Its name in Pyo and on your tailnet: lowercase letters, digits and
    /// dashes.
    pub name: String,
}

/// Adds a sandbox server, or makes a new code for one that hasn't joined
/// yet: its row, a one-time code and the command to run on it.
pub async fn add_server(s: &AppState, user: &User, req: AddServer) -> ApiResult<Value> {
    crate::util::require_dns_label(&req.name, "Server name")?;
    let mut tx = s.db.begin().await?;
    match &req.team_id {
        Some(t) => require(&mut tx, user, t, Need::Manage).await?,
        None => require_install_admin(user)?,
    }
    crate::cloud::needs_tailscale(&s.db).await?;
    let door = crate::deploy::registry_push();
    let api = crate::web::tailnet_api_url();
    let (false, Some(api)) = (door.is_empty(), api) else {
        return Err(ApiError::conflict(
            "Pyo's server isn't on your tailnet yet. Try again in a minute.",
        ));
    };
    if !s.sealer.enabled() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no PYO_SECRET_KEY, so it can't sign sandbox tokens.",
        ));
    }
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT id, state FROM pyo.sandbox_servers WHERE coalesce(team_id, '') = coalesce($1, '') AND name = $2",
    )
    .bind(&req.team_id)
    .bind(&req.name)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match existing {
        Some((id, state)) if state == "joining" || state == "refused" => {
            sqlx::query("UPDATE pyo.sandbox_servers SET state = 'joining', error = NULL, updated_at = now() WHERE id = $1")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            id
        }
        Some(_) => {
            return Err(ApiError::conflict(
                "A sandbox server has that name already.",
            ));
        }
        None => {
            let id = crate::ids::new_id("sbs");
            sqlx::query("INSERT INTO pyo.sandbox_servers (id, team_id, name, state, created_by) VALUES ($1, $2, $3, 'joining', $4)")
                .bind(&id)
                .bind(&req.team_id)
                .bind(&req.name)
                .bind(&user.name)
                .execute(&mut *tx)
                .await?;
            id
        }
    };
    let code = join::new_code();
    join::keep(&mut tx, &id, &code).await?;
    let ts_key =
        crate::tailscale::join_key(s, &format!("Pyo: sandbox server {}", req.name)).await?;
    if let Some(t) = &req.team_id {
        crate::ops::log_activity(
            &mut tx,
            t,
            &user.name,
            "sandbox.server",
            "info",
            &format!("Added the sandbox server {}", req.name),
            json!({ "kind": "sandboxServer", "id": id, "label": req.name }),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    let base = crate::join::scripts_url(Some(&crate::join::friendly(&api)));
    let door = crate::join::friendly(&door);
    let command = join::Command {
        base: &base,
        door: &door,
        server_id: &id,
        name: &req.name,
        code: &code,
        tailscale_key: &ts_key,
        version: env!("CARGO_PKG_VERSION"),
    }
    .render();
    let teams: Vec<String> = req.team_id.iter().cloned().collect();
    let server = server_views(&s.db, &teams, Some(&id))
        .await?
        .into_iter()
        .next();
    Ok(json!({
        "server": server,
        "command": command,
        "expiresAt": pyo_proto::now_ms() as u64 + join::CODE_TTL.as_millis() as u64,
    }))
}

/// Removes a sandbox server: its sandboxes are deleted where it still
/// answers, and Pyo forgets it. The daemon stays installed on the server
/// until `sandbox.sh --uninstall` runs there.
pub async fn remove_server(s: &AppState, user: &User, id: &str) -> ApiResult<Value> {
    let row: Option<(Option<String>, Option<String>, String)> =
        sqlx::query_as("SELECT team_id, address, name FROM pyo.sandbox_servers WHERE id = $1")
            .bind(id)
            .fetch_optional(&s.db)
            .await?;
    let (team_id, address, name) = row.ok_or_else(|| ApiError::not_found("Sandbox server"))?;
    let mut tx = s.db.begin().await?;
    match &team_id {
        Some(t) => require(&mut tx, user, t, Need::Manage).await?,
        None => require_install_admin(user)?,
    }
    let live: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM pyo.sandboxes WHERE server_id = $1 AND state NOT IN ('stopped', 'failed')",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some(address) = &address
        && !live.is_empty()
        && let Ok(key) = keys::signing_key(&s.db, &s.sealer).await
    {
        let d = daemon::Daemon::new(address);
        let token = keys::admin_token(&key, id);
        for x in &live {
            if let Err(e) = d
                .command(&token, &DaemonCommand::Delete { id: x.clone() })
                .await
            {
                tracing::warn!(sandbox = %x, error = %e, "removing a sandbox server: couldn't delete a sandbox");
            }
        }
    }
    for x in &live {
        end(
            &mut tx,
            x,
            "stopped",
            Some("Its sandbox server was removed."),
        )
        .await?;
    }
    sqlx::query("DELETE FROM pyo.sandbox_servers WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if let Some(t) = &team_id {
        crate::ops::log_activity(
            &mut tx,
            t,
            &user.name,
            "sandbox.server",
            "info",
            &format!("Removed the sandbox server {name}"),
            json!({ "kind": "sandboxServer", "id": id, "label": name }),
            json!({}),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(json!({ "ok": true, "stopped": live.len() }))
}

#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateTemplate {
    /// The team whose template it is. Left out: the installation's (its
    /// admin, in the dashboard).
    #[serde(default)]
    pub team_id: Option<String>,
    /// Lowercase letters, digits, dashes, dots and underscores, like
    /// "node-22".
    pub name: String,
    /// The OCI image it's made from, like "node:22".
    pub image: String,
}

/// Records a sandbox template made from an OCI image. Building it with
/// Pyo's builder and putting it on the servers comes next.
// TODO(sandbox): build the image into a root disk with the builder, put it
// on each server, then snapshot it once per runtime (`snapshots`).
pub async fn create_template(s: &AppState, user: &User, req: CreateTemplate) -> ApiResult<Value> {
    if !proto::is_template_name(&req.name) {
        return Err(ApiError::bad_request(
            "A template name is lowercase letters, digits, dashes, dots and underscores, like node-22.",
        ));
    }
    let image = req.image.trim();
    if image.is_empty() || image.len() > 512 || image.contains(char::is_whitespace) {
        return Err(ApiError::bad_request("Give the image, like node:22."));
    }
    let mut tx = s.db.begin().await?;
    match &req.team_id {
        Some(t) => require(&mut tx, user, t, Need::Use).await?,
        None => require_install_admin(user)?,
    }
    let id = crate::ids::new_id("stp");
    sqlx::query(
        "INSERT INTO pyo.sandbox_templates (id, team_id, name, source, state, created_by) VALUES ($1, $2, $3, $4, 'pending', $5)",
    )
    .bind(&id)
    .bind(&req.team_id)
    .bind(&req.name)
    .bind(json!({ "kind": "image", "image": image }))
    .bind(&user.name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let teams: Vec<String> = req.team_id.iter().cloned().collect();
    Ok(template_views(&s.db, &teams, Some(&id))
        .await?
        .into_iter()
        .next()
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes() {
        assert_eq!(format_ttl(3600), "1h");
        assert_eq!(format_ttl(5400), "1h30m");
        assert_eq!(format_ttl(90), "1m30s");
        assert_eq!(format_ttl(0), "0s");
        assert_eq!(parse_ttl_value(None).unwrap(), proto::DEFAULT_TTL_SECS);
        assert_eq!(parse_ttl_value(Some(&json!("1h"))).unwrap(), 3600);
        assert_eq!(parse_ttl_value(Some(&json!(90))).unwrap(), 90);
        assert!(parse_ttl_value(Some(&json!("2d"))).is_err());
        assert!(parse_ttl_value(Some(&json!(0))).is_err());
        assert!(parse_ttl_value(Some(&json!("soon"))).is_err());
        assert!(parse_ttl_value(Some(&json!(true))).is_err());
    }

    #[test]
    fn key_access_for_sandboxes() {
        use KeyAccess::*;
        for a in [Read, Deploy, Sandboxes, Full] {
            assert_eq!(key_refusal(a, Need::Read), None, "{a:?}");
        }
        assert_eq!(key_refusal(Sandboxes, Need::Use), None);
        assert_eq!(key_refusal(Full, Need::Use), None);
        assert!(key_refusal(Read, Need::Use).is_some());
        assert!(key_refusal(Deploy, Need::Use).is_some());
        assert_eq!(key_refusal(Full, Need::Manage), None);
        assert_eq!(
            key_refusal(Sandboxes, Need::Manage),
            Some("This API key can only use sandboxes.")
        );
    }

    #[test]
    fn daemon_errors_keep_their_meaning() {
        let e = daemon_error(DaemonError::new(ErrorCode::NoCapacity, "full"));
        assert_eq!(
            (e.status, e.message.as_str()),
            (StatusCode::SERVICE_UNAVAILABLE, "full")
        );
        let e = daemon_error(DaemonError::new(ErrorCode::Unauthorized, "bad"));
        assert_eq!(e.status, StatusCode::BAD_GATEWAY, "never the caller's 401");
        let e = daemon_error(DaemonError::new(ErrorCode::NotFound, "gone"));
        assert_eq!(e.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn create_takes_the_sdk_shape() {
        let r: CreateSandbox = serde_json::from_value(json!({
            "teamId": "tm_one", "template": "node-22", "ttl": "1h", "memoryMib": 1024,
            "env": { "A": "b" }, "network": "none", "isolation": "vm", "labels": { "job": "x" },
        }))
        .unwrap();
        assert_eq!(r.network, Some(NetworkMode::None));
        assert_eq!(r.isolation, Some(Isolation::Vm));
        assert!(
            serde_json::from_value::<CreateSandbox>(
                json!({ "teamId": "t", "template": "x", "nope": 1 })
            )
            .is_err()
        );
    }
}
