//! The dashboard's API: one snapshot to read, one endpoint of typed
//! operations to write (mirroring the dashboard store's `api` methods), and
//! service logs on demand.
//!
//! The dashboard makes ids itself so it can update optimistically; they're
//! validated here and the database decides uniqueness.

use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{get, post},
};
use pyo_proto::{Command, CommandResult, ServiceRef, now_ms};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::{KeyAccess, SessionUser, User},
    deploy::{self, DeployRequest},
    error::{ApiError, ApiResult, ErrorBody},
    ids::{new_id, secret_token, sha256_hex, valid_client_id},
    mail, snapshot,
    util::{hostname, require_dns_label, require_text, slugify},
};

/// What this Pyo is.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Meta {
    /// The version of Pyo it runs.
    version: &'static str,
    /// Features a hosted Pyo turned on. None when self-hosted.
    #[schema(value_type = Vec<String>)]
    features: &'static [String],
    /// Where Pyo is reached over HTTPS on its tailnet (https://<host>.<tailnet>.ts.net:8443). Null when it's only on its own machine.
    public_url: Option<String>,
}

/// What this Pyo is
///
/// Its version, and the features a hosted Pyo turned on (none when
/// self-hosted). No API key needed.
#[utoipa::path(get, path = "/meta", operation_id = "getMeta", tag = "Account", security(()),
    responses((status = 200, description = "The version and features.", body = Meta)),
    extensions(("x-pyo-access" = json!("none"))))]
pub(crate) async fn meta() -> Json<Meta> {
    Json(Meta {
        version: env!("CARGO_PKG_VERSION"),
        features: crate::features(),
        public_url: crate::web::public_url().map(str::to_owned),
    })
}

/// You
///
/// The account these credentials act as: an API key's owner, or the person
/// who connected an assistant.
#[utoipa::path(get, path = "/me", operation_id = "getMe", tag = "Account",
    responses((status = 200, description = "Your account.", body = User)),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn me(SessionUser(user): SessionUser) -> Json<User> {
    Json(user)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/state", get(state))
        .route("/meta", get(meta))
        .route("/me", get(me))
        .route("/ops", post(run))
        .route("/services/{id}/logs", get(service_logs))
        .route("/volumes/{id}/snapshots", get(crate::volumes::snapshots))
        .route(
            "/volumes/{id}/snapshots/{snapshot_id}",
            axum::routing::delete(crate::volumes::delete_snapshot),
        )
        .route("/services/{id}/preview", get(service_preview))
        .route("/observability", get(observability))
        .route("/access-logs", get(access_logs))
        .route("/metrics", get(metrics))
        .route("/cron-runs", get(crate::cronruns::list))
        .route("/services/{id}/traffic", get(crate::traffic::get))
        .route("/services/{id}/connections", get(connections))
        .route("/firewall/blocked", get(firewall_blocked))
        .route("/firewall/me", get(firewall_me))
        .route("/waf/hits", get(waf_hits))
        .route("/mcp/calls", get(crate::mcp::calls))
        .route("/images", get(crate::images::list))
        .route("/registry", get(crate::images::registry))
        .route("/databases", get(crate::databases::list))
        .route("/databases/{id}", get(crate::databases::get))
        .route("/database-extensions", get(crate::databases::extensions))
        .route("/databases/{id}/logs", get(crate::database_logs::logs))
        .route(
            "/databases/{id}/analytics",
            get(crate::database_analytics::analytics),
        )
        .route("/database-backups", post(crate::database_backups::add))
        .route("/compose/preview", post(crate::compose::preview))
}

/// Everything you can see
///
/// One snapshot of every team you're in (or the API key's teams): their
/// projects, services and deployments, clusters and servers, domains,
/// certificates, volumes, backups, registry logins, builders and recent
/// activity. The dashboard renders from this, and scripts can read it too.
#[utoipa::path(get, path = "/state", operation_id = "getState", tag = "State",
    responses((status = 200, description = "The snapshot: `user`, `teams`, `projects`, `services`, `deployments`, `clusters`, `servers` and the rest, each a list.", body = Object)),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn state(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        snapshot::build(&s.db, &user, &s.config.agent_api_url).await?,
    ))
}

// ── Roles ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Developer,
    Admin,
    Owner,
}

fn parse_role(s: &str) -> ApiResult<Role> {
    match s {
        "owner" => Ok(Role::Owner),
        "admin" => Ok(Role::Admin),
        "developer" => Ok(Role::Developer),
        "viewer" => Ok(Role::Viewer),
        _ => Err(ApiError::bad_request("Unknown role.")),
    }
}

/// For routes outside an op: the team's owners and admins only.
pub async fn require_team_admin(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
) -> ApiResult<()> {
    require_role(tx, user, team_id, Role::Admin)
        .await
        .map(|_| ())
}

/// Installation-wide settings are for the installation's admin.
fn require_admin(user: &User) -> ApiResult<()> {
    if user.is_admin {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "Only the installation's admin can change this.",
        ))
    }
}

/// "Invalid credentials" rather than a chain of transport errors.
fn smtp_error(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    full.rsplit(": ")
        .next()
        .unwrap_or(&full)
        .chars()
        .take(300)
        .collect()
}

/// Mail an invite when email is set up. The invite stands either way; the
/// result says whether it was emailed, and the link to share otherwise.
/// `invite`: the invite link's secret, for someone without an account yet.
async fn mail_invite(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
    email: &str,
    role: &str,
    invite: Option<&str>,
) -> ApiResult<Value> {
    let (team, slug): (String, String) =
        sqlx::query_as("SELECT name, slug FROM pyo.teams WHERE id = $1")
            .bind(team_id)
            .fetch_one(&mut **tx)
            .await?;
    let path = invite
        .map(crate::auth::invite_path)
        .unwrap_or_else(|| format!("/{slug}"));
    let Some(cfg) = mail::settings(&s.db).await? else {
        return Ok(json!({ "ok": true, "emailed": false, "link": path }));
    };
    let link = cfg.link(&path);
    let (subject, text, html) = mail::invite(&user.name, &team, role, &link, invite.is_none());
    Ok(
        match mail::send(s, &cfg, email, &subject, text, html).await {
            Ok(()) => json!({ "ok": true, "emailed": true, "link": link }),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "invite email not sent; retrying in the background");
                // Tried right away so the person gets an answer; retried as a job.
                // The link's secret waits in the job sealed (when there's a key).
                let sealed = invite.map(|t| s.sealer.seal(t).unwrap_or_else(|_| t.to_owned()));
                crate::jobs::enqueue(&mut **tx, "email.invite", json!({ "teamId": team_id, "email": email, "role": role, "inviter": user.name, "hasAccount": invite.is_none(), "invite": sealed })).await?;
                json!({ "ok": true, "emailed": false, "emailError": format!("The invite email didn't send ({}). Pyo keeps retrying.", smtp_error(&e)), "link": link })
            }
        },
    )
}

/// The invite email, for the job that retries it.
pub async fn send_invite_email(
    s: &AppState,
    team_id: &str,
    email: &str,
    role: &str,
    inviter: &str,
    invite: Option<&str>,
) -> anyhow::Result<()> {
    let (team, slug): (String, String) =
        sqlx::query_as("SELECT name, slug FROM pyo.teams WHERE id = $1")
            .bind(team_id)
            .fetch_one(&s.db)
            .await?;
    let invite = match invite.filter(|t| !t.is_empty()) {
        Some(t) if t.starts_with("sb1:") => Some(s.sealer.open(t)?),
        t => t.map(str::to_owned),
    };
    // Joined, revoked or sent again with a new link meanwhile: nothing to send.
    let still: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.members WHERE team_id = $1 AND email = $2 AND (invite_token_hash = $3 AND status = 'invited' OR $3 IS NULL))")
        .bind(team_id)
        .bind(email)
        .bind(invite.as_deref().map(crate::ids::sha256_hex))
        .fetch_one(&s.db)
        .await?;
    if !still {
        return Ok(());
    }
    let cfg = mail::settings(&s.db)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?
        .ok_or_else(|| anyhow::anyhow!("email isn't set up"))?;
    let path = invite
        .as_deref()
        .map(crate::auth::invite_path)
        .unwrap_or_else(|| format!("/{slug}"));
    let (subject, text, html) =
        mail::invite(inviter, &team, role, &cfg.link(&path), invite.is_none());
    mail::send(s, &cfg, email, &subject, text, html).await
}

pub async fn require_role(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    team_id: &str,
    min: Role,
) -> ApiResult<Role> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM pyo.members WHERE team_id = $1 AND user_id = $2 AND status = 'active'",
    )
    .bind(team_id)
    .bind(&user.id)
    .fetch_optional(&mut **tx)
    .await?;
    let role = parse_role(&role.ok_or_else(|| ApiError::not_found("Team"))?)?;
    if let Some(k) = &user.key {
        if k.teams
            .as_ref()
            .is_some_and(|ts| !ts.iter().any(|t| t == team_id))
        {
            return Err(ApiError::not_found("Team"));
        }
        if k.access == KeyAccess::Read && min > Role::Viewer {
            return Err(ApiError::forbidden("This API key can only read."));
        }
    }
    if role < min {
        return Err(ApiError::forbidden(match min {
            Role::Owner => "Only the team owner can do this.",
            Role::Admin => "Only team owners and admins can do this.",
            _ => "Viewers can't make changes.",
        }));
    }
    Ok(role)
}

/// The team an entity belongs to, then the role check.
pub async fn team_of(
    tx: &mut Transaction<'_, Postgres>,
    table: &str,
    id: &str,
) -> ApiResult<String> {
    let sql = format!("SELECT team_id FROM pyo.{table} WHERE id = $1");
    let what = match table {
        "projects" => "Project",
        "services" => "Service",
        "clusters" => "Cluster",
        "deployments" => "Deployment",
        "certificates" | "origin_pull_certificates" => "Certificate",
        "secret_stores" => "Secret store",
        "members" => "Member",
        "deploy_hooks" => "Deploy hook",
        "volumes" => "Volume",
        "databases" => "Database",
        _ => "Item",
    };
    // `table` is one of a fixed set of names, never user input.
    sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| ApiError::not_found(what))
}

// ── Shared helpers ─────────────────────────────────────────────────────────

/// Top-level paths the dashboard owns can't be team slugs.
// "pyo": Pyo registry keeps Pyo's own builds under pyo/.
const RESERVED_SLUGS: [&str; 16] = [
    "profile",
    "settings",
    "oauth",
    "api-keys",
    "mcp",
    "login",
    "signup",
    "logout",
    "api",
    "new",
    "teams",
    "assets",
    "install",
    "pyo",
    "invite",
    "cli-login",
];

fn check_autoscale(a: &pyo_proto::Autoscale) -> ApiResult<()> {
    if !(1..=50).contains(&a.min) || !(1..=50).contains(&a.max) {
        return Err(ApiError::bad_request(
            "Autoscale between 1 and 50 replicas.",
        ));
    }
    if a.min >= a.max {
        return Err(ApiError::bad_request(
            "The maximum has to be above the minimum.",
        ));
    }
    if !(10..=95).contains(&a.target_cpu) {
        return Err(ApiError::bad_request(
            "Set the CPU target between 10% and 95%.",
        ));
    }
    if a.target_memory.is_some_and(|m| !(10..=95).contains(&m)) {
        return Err(ApiError::bad_request(
            "Set the memory target between 10% and 95%.",
        ));
    }
    Ok(())
}

pub async fn unique_team_slug(
    tx: &mut Transaction<'_, Postgres>,
    base: &str,
    except: Option<&str>,
) -> ApiResult<String> {
    let base = if base.is_empty() { "team" } else { base };
    let mut slug = base.to_owned();
    for i in 2.. {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.teams WHERE slug = $1 AND id IS DISTINCT FROM $2)",
        )
        .bind(&slug)
        .bind(except)
        .fetch_one(&mut **tx)
        .await?;
        if !taken && !RESERVED_SLUGS.contains(&slug.as_str()) {
            break;
        }
        slug = format!("{base}-{i}");
    }
    Ok(slug)
}

/// One entry in the team's activity log (the dashboard's ActivityEvent).
#[allow(clippy::too_many_arguments)]
pub async fn log_activity(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    actor: &str,
    kind: &str,
    status: &str,
    title: &str,
    target: Value,
    context: Value,
) -> ApiResult<()> {
    let mut data = json!({
        "type": kind,
        "status": status,
        "title": title,
        "actor": { "name": actor, "kind": if actor == "pyo" { "system" } else { "user" } },
        "target": target,
        "details": {},
    });
    if let (Some(d), Some(c)) = (data.as_object_mut(), context.as_object()) {
        for (k, v) in c {
            d.insert(k.clone(), v.clone());
        }
    }
    sqlx::query("INSERT INTO pyo.activity (id, team_id, data) VALUES ($1, $2, $3)")
        .bind(new_id("evt"))
        .bind(team_id)
        .bind(data)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn client_id(id: &str, prefix: &str) -> ApiResult<()> {
    if valid_client_id(id, prefix) {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!("Invalid {prefix} id.")))
    }
}

/// A new volume's id: valid, and never one of a deleted volume (its claim
/// may still be on a cluster, labelled with it).
async fn new_volume_id(tx: &mut Transaction<'_, Postgres>, id: &str) -> ApiResult<()> {
    client_id(id, "vol")?;
    let used: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.deleted_volumes WHERE id = $1)")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    if used {
        return Err(ApiError::conflict(
            "That volume id was used before. Pick a new one.",
        ));
    }
    Ok(())
}

/// Fields the API owns; whatever the dashboard sends for them is dropped.
fn service_spec_from(service: &Value) -> Value {
    let mut spec = service.clone();
    if let Some(o) = spec.as_object_mut() {
        for k in [
            "id",
            "teamId",
            "projectId",
            "clusterId",
            "status",
            "runtime",
            "currentDeploymentId",
            "pendingChanges",
            "waf",
            "createdAt",
            "updatedAt",
        ] {
            o.remove(k);
        }
        // Worked out for each response (template_source::show); not stored.
        if let Some(src) = o.get_mut("source").and_then(Value::as_object_mut) {
            src.remove("customized");
        }
        // The snapshot's marker for a secret that has a value; not stored.
        if let Some(env) = o.get_mut("env").and_then(Value::as_array_mut) {
            for e in env.iter_mut().filter_map(Value::as_object_mut) {
                e.remove("hasValue");
            }
        }
    }
    spec
}

/// A secret sent back blank keeps its stored value: matched by id, else key.
/// Returns the keys of secrets this makes plain without a new value, which
/// shows their stored value to everyone in the team: only admins may.
fn keep_secret_values(spec: &mut Value, old: &Value) -> Vec<String> {
    let old_env = old["env"].as_array().cloned().unwrap_or_default();
    let mut unhidden = vec![];
    let Some(env) = spec.get_mut("env").and_then(Value::as_array_mut) else {
        return unhidden;
    };
    for e in env.iter_mut() {
        // A secret, or one that was (made plain without a new value).
        let was_set = e["secret"] == json!(true) || e["hasValue"] == json!(true);
        if !was_set || e["value"].as_str().is_some_and(|v| !v.is_empty()) {
            continue;
        }
        let prev = old_env
            .iter()
            .find(|o| o["id"].is_string() && o["id"] == e["id"])
            .or_else(|| old_env.iter().find(|o| o["key"] == e["key"]));
        if let Some(o) = prev
            && let Some(v) = o["value"].as_str()
        {
            if o["secret"] == json!(true) && e["secret"] != json!(true) {
                unhidden.push(e["key"].as_str().unwrap_or("").to_owned());
            }
            e["value"] = json!(v);
        }
    }
    unhidden
}

/// A GitHub App source must name one of this team's installations, so a
/// team can't build with (and read) another team's private repositories.
fn one() -> u8 {
    1
}

fn is_in() -> String {
    "in".into()
}

fn yes() -> bool {
    true
}

/// Who changes a service's firewall and WAF: its team's developers.
/// Returns its team, cluster and name.
async fn fw_access(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    service_id: &str,
) -> ApiResult<(String, String, String)> {
    let team_id = team_of(tx, "services", service_id).await?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    let (cluster_id, name): (String, String) =
        sqlx::query_as("SELECT cluster_id, name FROM pyo.services WHERE id = $1 AND NOT deleting")
            .bind(service_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| ApiError::not_found("Service"))?;
    Ok((team_id, cluster_id, name))
}

/// A service's custom WAF rules changed: applied now while its WAF is on
/// (they run only then).
async fn waf_changed(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    cluster_id: &str,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    let waf: Value = sqlx::query_scalar("SELECT waf FROM pyo.services WHERE id = $1")
        .bind(service_id)
        .fetch_one(&mut **tx)
        .await?;
    if crate::waf::is_on(&waf) {
        crate::firewall::sync(tx, cluster_id).await?;
        wake.push(cluster_id.to_owned());
    }
    Ok(())
}

/// A hostname routes to one service: two services claiming it would fight over the
/// cluster's ingress. Checked across every team; another team's service isn't named.
async fn domains_unique(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    service_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    let hosts: Vec<String> = spec["domains"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|d| d["hostname"].as_str())
        .map(|h| h.trim().to_lowercase())
        .collect();
    if hosts.is_empty() {
        return Ok(());
    }
    let taken: Option<(String, String, String)> = sqlx::query_as(
        "SELECT lower(d->>'hostname'), a.team_id, a.name FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
          WHERE a.id <> $1 AND NOT a.deleting AND lower(d->>'hostname') = ANY($2) LIMIT 1",
    )
    .bind(service_id)
    .bind(&hosts)
    .fetch_optional(&mut **tx)
    .await?;
    match taken {
        Some((host, team, service)) if team == team_id => Err(ApiError::conflict(format!(
            "{host} is already used by {service}."
        ))),
        Some((host, _, _)) => Err(ApiError::conflict(format!(
            "{host} is already used by another service."
        ))),
        None => Ok(()),
    }
}

/// How a machine outside the cluster reaches Pyo: the API (over the
/// tailnet when there's one), the team's Tailscale key, and the registry
/// door for Pyo-built images with the cluster's pull token (when Pyo is on
/// a tailnet). Tailnet addresses by Pyo's tailnet name when it has one.
async fn reach(
    tx: &mut Transaction<'_, Postgres>,
    s: &AppState,
    team_id: &str,
    cluster_id: &str,
) -> ApiResult<(String, Option<String>, Option<(String, String)>)> {
    let api = if s.config.tailnet_api_url.is_empty() {
        s.config.agent_api_url.clone()
    } else {
        crate::join::friendly(&s.config.tailnet_api_url)
    };
    let ts_key = match sqlx::query_scalar::<_, String>(
        "SELECT secret FROM pyo.integrations WHERE team_id = $1 AND kind = 'tailscale'",
    )
    .bind(team_id)
    .fetch_optional(&mut **tx)
    .await?
    {
        Some(sealed) => s.sealer.open(&sealed).ok(),
        None => None,
    };
    let door = crate::join::friendly(&deploy::registry().push);
    let registry = if door.is_empty() {
        None
    } else {
        Some((door, registry_token(tx, s, cluster_id).await?))
    };
    Ok((api, ts_key, registry))
}

/// The cluster's pull token at Pyo's registry door (its servers'
/// registries.yaml): made on first use, then the same for every server.
async fn registry_token(
    tx: &mut Transaction<'_, Postgres>,
    s: &AppState,
    cluster_id: &str,
) -> ApiResult<String> {
    let sealed: Option<String> =
        sqlx::query_scalar("SELECT registry_token_sealed FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
    if let Some(t) = sealed.and_then(|x| s.sealer.open(&x).ok()) {
        return Ok(t);
    }
    let token = secret_token();
    sqlx::query("UPDATE pyo.clusters SET registry_token_hash = $2, registry_token_sealed = $3 WHERE id = $1")
        .bind(cluster_id)
        .bind(sha256_hex(&token))
        .bind(s.sealer.seal(&token)?)
        .execute(&mut **tx)
        .await?;
    Ok(token)
}

/// The one-line command that sets up a machine as this cluster: the agent
/// reaches Pyo over the tailnet when there's one, pulls Pyo-built images
/// from its registry, and joins the tailnet with the team's key. With
/// `public`, over the public internet at that URL instead (no tailnet, so
/// no registry). Also: whether a Tailscale key is saved, and whether this
/// installation is on a tailnet.
async fn connect_command(
    tx: &mut Transaction<'_, Postgres>,
    s: &AppState,
    team_id: &str,
    cluster_id: &str,
    token: &str,
    public: Option<&str>,
) -> ApiResult<(String, bool, bool)> {
    let (api, ts_key, registry) = reach(tx, s, team_id, cluster_id).await?;
    let tailnet = !s.config.tailnet_api_url.is_empty();
    let saved = ts_key.is_some();
    if let Some(url) = public {
        let url = url.trim().trim_end_matches('/');
        if !(url.starts_with("https://") || url.starts_with("http://"))
            || url.contains(char::is_whitespace)
        {
            return Err(ApiError::bad_request(
                "The public address is an http(s) URL, where you use Pyo.",
            ));
        }
        // The agent's token and the secrets in its commands go this way.
        if !crate::join::secure_transport(url) {
            return Err(ApiError::bad_request(
                "Use https:// for a public address. Plain http:// works only on a private network or the tailnet.",
            ));
        }
        return Ok((
            crate::join::connect_command(
                &crate::join::scripts_url(Some(url)),
                url,
                token,
                None,
                None,
            ),
            saved,
            tailnet,
        ));
    }
    Ok((
        crate::join::connect_command(
            &crate::join::scripts_url(Some(&api)),
            &api,
            token,
            ts_key.as_deref(),
            registry.as_ref().map(|(r, t)| (r.as_str(), t.as_str())),
        ),
        saved,
        tailnet,
    ))
}

/// A join command for a new server of a cluster (a k3s join token the agent
/// makes, valid an hour). `builder`: a build-only server (join.sh --builder).
async fn join_command(
    tx: &mut Transaction<'_, Postgres>,
    s: &AppState,
    team_id: &str,
    cluster_id: &str,
    server_name: &str,
    builder: bool,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    crate::check_add_server(tx, team_id).await?;
    let (status, version): (String, String) =
        sqlx::query_as("SELECT status, coalesce(version, '') FROM pyo.clusters WHERE id = $1")
            .bind(cluster_id)
            .fetch_one(&mut **tx)
            .await?;
    if status != "connected" && status != "degraded" {
        return Err(ApiError::conflict(
            "The cluster's agent isn't connected, so it can't let a server in. Check the cluster first.",
        ));
    }
    // A new server installs the cluster's own k3s version (join.sh --k3s).
    if !crate::join::is_k3s_version(&version) {
        return Err(ApiError::conflict(if version.is_empty() {
            "The cluster hasn't reported its k3s version yet. Try again in a minute.".to_owned()
        } else {
            format!("Servers can only join a k3s cluster. This one runs Kubernetes {version}.")
        }));
    }
    // The control plane's address, which new servers join through.
    let server_ip: Option<String> = sqlx::query_scalar(
        "SELECT report->>'internalIP' FROM pyo.servers WHERE cluster_id = $1 AND report->'roles' ? 'control-plane'
          ORDER BY (report->>'status' = 'ready') DESC LIMIT 1",
    )
    .bind(cluster_id)
    .fetch_optional(&mut **tx)
    .await?;
    let server_ip = server_ip.filter(|ip| !ip.is_empty()).ok_or_else(|| {
        ApiError::conflict("The cluster has no control-plane server reported yet.")
    })?;
    let (_, ts_key, registry) = reach(tx, s, team_id, cluster_id).await?;
    let alnum = |n: usize| {
        crate::ids::secret_token()
            .chars()
            .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .take(n)
            .collect::<String>()
    };
    let (token_id, token_secret) = (alnum(6), alnum(16));
    let ttl = 3600;
    deploy::enqueue(
        tx,
        cluster_id,
        Command::CreateJoinToken {
            token_id: token_id.clone(),
            token_secret: token_secret.clone(),
            ttl_seconds: ttl,
        },
    )
    .await?;
    wake.push(cluster_id.to_owned());
    let base = crate::join::scripts_url(None);
    let command = crate::join::command(
        &base,
        &crate::join::Join {
            server_ip: &crate::join::friendly(&server_ip),
            token: &format!("{token_id}.{token_secret}"),
            server_name,
            k3s_version: &version,
            tailscale_key: ts_key.as_deref(),
            builder,
            registry: registry.as_ref().map(|(r, t)| (r.as_str(), t.as_str())),
        },
    );
    Ok(
        json!({ "ok": true, "command": command, "scriptUrl": format!("{base}/join.sh"), "expiresAt": now_ms() + ttl as i64 * 1000, "tailscale": ts_key.is_some() }),
    )
}

/// A machine runs at most one builder: they'd share its BuildKit cache and disk.
async fn one_builder_per_machine(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    cluster_id: &str,
    node: &str,
) -> ApiResult<()> {
    let other: Option<String> = sqlx::query_scalar("SELECT name FROM pyo.builders WHERE cluster_id = $1 AND server = $2 AND id <> $3 AND status <> 'removing' LIMIT 1")
        .bind(cluster_id)
        .bind(node)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    match other {
        Some(b) => Err(ApiError::conflict(format!(
            "{node} already runs builder {b}: one builder per machine."
        ))),
        None => Ok(()),
    }
}

/// Tags as given, checked and tidied: lowercase, no repeats, at most 10.
fn valid_tags(tags: &[String]) -> ApiResult<Vec<String>> {
    let mut out: Vec<String> = vec![];
    for t in tags {
        let v = pyo_proto::valid_tag(t).ok_or_else(|| {
            ApiError::bad_request(format!(
                "\"{}\" isn't a valid tag: use lowercase letters, digits and dashes, up to 32.",
                t.trim()
            ))
        })?;
        if !out.contains(&v) {
            out.push(v);
        }
    }
    if out.len() > 10 {
        return Err(ApiError::bad_request("Up to 10 tags."));
    }
    Ok(out)
}

/// A server's tags, kept by name (it may not have joined yet).
async fn save_server_tags(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    name: &str,
    tags: &[String],
) -> ApiResult<()> {
    sqlx::query("INSERT INTO pyo.server_tags (cluster_id, name, tags) VALUES ($1, $2, $3) ON CONFLICT (cluster_id, name) DO UPDATE SET tags = $3")
        .bind(cluster_id)
        .bind(name)
        .bind(tags)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Names that are dashboard pages under /clusters.
/// Why a new service can't be called `name`: the project has one, or had
/// one that the cluster is still deleting.
fn name_taken_message(name: &str, deleting: bool) -> String {
    if deleting {
        format!("{name} is still being deleted. Try again in a few seconds.")
    } else {
        format!("This project already has a service called {name}.")
    }
}

fn reserved_cluster_name(name: &str) -> ApiResult<()> {
    if ["new", "workloads", "networking", "storage"].contains(&name) {
        return Err(ApiError::bad_request(
            "That cluster name is used by a dashboard page. Pick another.",
        ));
    }
    Ok(())
}

/// A cluster's location: "City, CC" or "CC", CC an ISO country code (uppercased).
/// A region: "City, CC", "CC", or a name of its own ("US East").
fn require_location(location: &str) -> ApiResult<String> {
    let l = location.split_whitespace().collect::<Vec<_>>().join(" ");
    let (city, cc) = match l.rsplit_once(',') {
        Some((city, cc)) => (city.trim(), cc.trim()),
        None => ("", l.as_str()),
    };
    if cc.len() != 2 || !cc.bytes().all(|b| b.is_ascii_alphabetic()) {
        if l.chars().count() > 40 {
            return Err(ApiError::bad_request(
                "Keep the region's name under 40 characters.",
            ));
        }
        if pyo_proto::region_slug(&l).is_empty() {
            return Err(ApiError::bad_request(
                "Use letters or digits in the region's name, like Singapore or US East.",
            ));
        }
        return Ok(l);
    }
    if city.chars().count() > 80 {
        return Err(ApiError::bad_request("The city name is too long."));
    }
    let cc = cc.to_ascii_uppercase();
    Ok(if city.is_empty() {
        cc
    } else {
        format!("{city}, {cc}")
    })
}

async fn github_source_ok(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    let Some(inst) = spec["source"]["installationId"].as_i64() else {
        return Ok(());
    };
    let mine: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.github_installations WHERE id = $1 AND team_id = $2)",
    )
    .bind(inst)
    .bind(team_id)
    .fetch_one(&mut **tx)
    .await?;
    if !mine {
        return Err(ApiError::bad_request(
            "That GitHub connection belongs to another team. Connect GitHub in this team's Integrations.",
        ));
    }
    if crate::github::full_name(spec["source"]["repoUrl"].as_str().unwrap_or("")).is_none() {
        return Err(ApiError::bad_request(
            "Repositories from a GitHub connection use their https://github.com/owner/name URL.",
        ));
    }
    Ok(())
}

/// Project namespaces and service names that would shadow a dashboard page in
/// /<project>/<service> URLs (kept in step with dash/src/lib/pyo/paths.ts).
const RESERVED_PROJECT_SLUGS: &[&str] = &[
    "activity",
    "analytics",
    "api-keys",
    "services",
    "backups",
    "builders",
    "certificates",
    "clusters",
    "cron-jobs",
    "databases",
    "deploy",
    "deployments",
    "domains",
    "images",
    "integrations",
    "kv",
    "mcp",
    "members",
    "mtls",
    "mysql",
    "new",
    "notifications",
    "observability",
    "onboarding",
    "postgres",
    "projects",
    "registry",
    "security",
    "servers",
    "services",
    "settings",
    "templates",
];
const RESERVED_SERVICE_NAMES: &[&str] = &[
    "analytics",
    "deployments",
    "networking",
    "services",
    "settings",
    "storage",
];

/// GPUs one service may ask for (they all have to be on one server).
const MAX_GPUS: u64 = 8;

/// A service asking for GPUs: the cluster has a server with that many, and
/// it doesn't autoscale (a new replica would need a spare GPU).
async fn gpu_ok(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    let gpus = spec["resources"]["gpu"].as_u64().unwrap_or(0);
    if gpus == 0 {
        return Ok(());
    }
    if spec.get("autoscale").is_some_and(|a| !a.is_null()) {
        return Err(ApiError::bad_request(
            "Services with GPUs can't autoscale yet. Turn autoscaling off first.",
        ));
    }
    let row: Option<(String, Value)> = sqlx::query_as(
        "SELECT name, coalesce(capabilities->'gpu', '{}') FROM pyo.clusters WHERE id = $1",
    )
    .bind(cluster_id)
    .fetch_optional(&mut **tx)
    .await?;
    let (cluster, gpu) = row.ok_or_else(|| ApiError::not_found("Cluster"))?;
    let plural = |n: u64| {
        if n == 1 {
            "1 GPU".to_owned()
        } else {
            format!("{n} GPUs")
        }
    };
    let max = gpu["maxPerServer"].as_u64().unwrap_or(0);
    if gpu["servers"].as_u64().unwrap_or(0) == 0 {
        return Err(ApiError::bad_request(format!(
            "This service asks for {}, but {cluster} has no server with an NVIDIA GPU set up. Add one with Servers › Add server: the join script sets up its GPU.",
            plural(gpus)
        )));
    }
    if max == 0 {
        let plugin = gpu["devicePlugin"].as_str().unwrap_or("starting");
        return Err(ApiError::bad_request(format!(
            "{cluster}'s GPU servers aren't ready yet (NVIDIA device plugin: {plugin}). Try again in a minute."
        )));
    }
    if gpus > max {
        return Err(ApiError::bad_request(format!(
            "This service asks for {gpus} GPUs, but the most on one server in {cluster} is {max}. A service's GPUs all have to be on one server."
        )));
    }
    Ok(())
}

/// Five cron fields. The cluster has the last word on what they mean.
fn check_schedule(schedule: &str) -> ApiResult<()> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    let field = |f: &&str| {
        f.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '*' | '/' | ',' | '-'))
    };
    if fields.len() != 5 || !fields.iter().all(field) {
        return Err(ApiError::bad_request(
            "Enter a cron schedule with five fields, like 0 3 * * *.",
        ));
    }
    Ok(())
}

fn validate_service_spec(spec: &Value) -> ApiResult<()> {
    if spec["workloadKind"] == json!("CronJob") {
        check_schedule(spec["schedule"].as_str().unwrap_or(""))?;
        if spec["command"].as_str().is_some_and(|c| c.len() > 4096) {
            return Err(ApiError::bad_request("That command is too long."));
        }
        if spec["domains"].as_array().is_some_and(|d| !d.is_empty()) {
            return Err(ApiError::bad_request(
                "A cron job doesn't serve traffic, so it can't have a domain.",
            ));
        }
    }
    let name = spec["name"].as_str().unwrap_or("");
    require_dns_label(name, "Service name")?;
    if RESERVED_SERVICE_NAMES.contains(&name) {
        return Err(ApiError::bad_request(
            "That service name is used by a project page. Pick another.",
        ));
    }
    if name.starts_with(pyo_proto::PUBLIC_SERVICE_PREFIX) {
        return Err(ApiError::bad_request(format!(
            "Service names can't start with {}; Pyo uses those names itself.",
            pyo_proto::PUBLIC_SERVICE_PREFIX
        )));
    }
    if let Some(l) = spec["location"].as_str().filter(|l| !l.trim().is_empty()) {
        require_location(l)?;
    }
    deploy::image_ref(&spec["source"])?;
    crate::template_source::check(&spec["source"])?;
    if spec["resources"]["gpu"]
        .as_u64()
        .is_some_and(|g| g > MAX_GPUS)
    {
        return Err(ApiError::bad_request(format!(
            "A service can have at most {MAX_GPUS} GPUs."
        )));
    }
    let ports = spec["ports"].as_array().cloned().unwrap_or_default();
    for p in &ports {
        let port = p["containerPort"].as_i64().unwrap_or(0);
        if !(1..=65535).contains(&port) {
            return Err(ApiError::bad_request("Ports must be between 1 and 65535."));
        }
        require_dns_label(p["name"].as_str().unwrap_or(""), "Port name")?;
        validate_public_port(p)?;
    }
    // Two proxied ports the proxy would listen on at the same place inside
    // (53 and 10053 both end up on 10053).
    let mut inside: Vec<(String, i32)> = vec![];
    for p in ports
        .iter()
        .filter_map(deploy::port_spec)
        .filter(|p| p.proxied())
    {
        let key = (
            p.transport().to_owned(),
            pyo_proto::proxy_listen_port(p.public()),
        );
        if inside.contains(&key) {
            return Err(ApiError::bad_request(format!(
                "Port {}/{} can't go through the gateway together with another of this service's ports: inside the gateway, ports below 1024 move up by 10000.",
                p.public(),
                p.transport()
            )));
        }
        inside.push(key);
    }
    for d in spec["domains"].as_array().cloned().unwrap_or_default() {
        let host = d["hostname"].as_str().unwrap_or("");
        if !hostname(host) {
            return Err(ApiError::bad_request(format!(
                "{host} isn't a valid hostname."
            )));
        }
        let port = d["portName"].as_str().unwrap_or("");
        if !ports.iter().any(|p| p["name"] == json!(port)) {
            return Err(ApiError::bad_request(format!(
                "{host} points at a port the service doesn't have."
            )));
        }
    }
    for e in spec["env"].as_array().cloned().unwrap_or_default() {
        let key = e["key"].as_str().unwrap_or("");
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')
            || key.as_bytes()[0].is_ascii_digit()
        {
            return Err(ApiError::bad_request(format!(
                "{key:?} isn't a valid environment variable name."
            )));
        }
    }
    Ok(())
}

/// A public TCP or UDP port's settings: its number, and what the gateway
/// does for it (nothing of that for a port that bypasses the gateway, and
/// only the gateway itself for UDP).
fn validate_public_port(p: &Value) -> ApiResult<()> {
    let Some(port) = deploy::port_spec(p) else {
        return Ok(());
    };
    let label = format!("{}/{}", port.public(), port.transport());
    if port.exposure != "loadbalancer" {
        if p["ipFilter"].is_object()
            || p["connectionLimit"].is_number()
            || p["acceptProxyProtocol"] == json!(true)
            || p["sendProxyProtocol"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
        {
            return Err(ApiError::bad_request(format!(
                "Port {}: IP rules, connection limits and PROXY protocol are only for public TCP and UDP ports.",
                port.name
            )));
        }
        return Ok(());
    }
    if !(1..=65535).contains(&port.public()) {
        return Err(ApiError::bad_request(
            "Public ports must be between 1 and 65535.",
        ));
    }
    if let Some(why) = pyo_proto::reserved_public_port(port.transport(), port.public()) {
        return Err(ApiError::bad_request(format!("Port {label}: {why}")));
    }
    let Some(opts) = &port.proxy else {
        if p["ipFilter"].is_object()
            || p["connectionLimit"].is_number()
            || p["acceptProxyProtocol"] == json!(true)
            || p["sendProxyProtocol"]
                .as_str()
                .is_some_and(|v| !v.is_empty())
        {
            return Err(ApiError::bad_request(format!(
                "Port {label} bypasses the gateway: IP rules, connection limits and PROXY protocol need it on."
            )));
        }
        return Ok(());
    };
    if port.transport() == "UDP" && *opts != pyo_proto::PortProxy::default() {
        return Err(ApiError::bad_request(format!(
            "Port {label}: IP rules, connection limits and PROXY protocol aren't available for UDP."
        )));
    }
    if let Some(f) = &opts.ip_filter {
        if f.mode != "allow" && f.mode != "deny" {
            return Err(ApiError::bad_request(
                "An IP rule either allows only its addresses (\"allow\") or blocks them (\"deny\").",
            ));
        }
        for c in &p["ipFilter"]["cidrs"]
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let c = c.as_str().unwrap_or("");
            if deploy::normalize_cidr(c).is_none() {
                return Err(ApiError::bad_request(format!(
                    "{c:?} isn't an IP address or range (like 203.0.113.7 or 198.51.100.0/24)."
                )));
            }
        }
        if f.cidrs.is_empty() {
            return Err(ApiError::bad_request(format!(
                "Port {label}: add at least one address to its IP rule, or turn the rule off."
            )));
        }
        if f.cidrs.len() > pyo_proto::system::MAX_IP_FILTER {
            return Err(ApiError::bad_request(format!(
                "An IP rule holds at most {} addresses.",
                pyo_proto::system::MAX_IP_FILTER
            )));
        }
    }
    if let Some(n) = opts.connection_limit
        && !(1..=1_000_000).contains(&n)
    {
        return Err(ApiError::bad_request(
            "A connection limit is between 1 and 1,000,000.",
        ));
    }
    if !matches!(opts.send_proxy_protocol.as_str(), "" | "V1" | "V2") {
        return Err(ApiError::bad_request(
            "PROXY protocol to the service is version \"V1\" or \"V2\".",
        ));
    }
    Ok(())
}

/// A public TCP or UDP port is one service's on its cluster (the servers
/// publish it, or the gateway listens on it); a port the proxy takes
/// can't land where another proxied port already listens inside it.
async fn ports_unique(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    service_id: &str,
    cluster_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    let mine: Vec<pyo_proto::PortSpec> = spec["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(deploy::port_spec)
        .filter(|p| p.exposure == "loadbalancer")
        .collect();
    if mine.is_empty() {
        return Ok(());
    }
    let others: Vec<(String, String, Value)> = sqlx::query_as("SELECT team_id, name, spec->'ports' FROM pyo.services WHERE cluster_id = $1 AND id <> $2 AND NOT deleting").bind(cluster_id).bind(service_id).fetch_all(&mut **tx).await?;
    for (team, name, ports) in others {
        for theirs in ports
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(deploy::port_spec)
            .filter(|p| p.exposure == "loadbalancer")
        {
            for p in &mine {
                let same = p.transport() == theirs.transport() && p.public() == theirs.public();
                let inside = p.proxied()
                    && theirs.proxied()
                    && p.transport() == theirs.transport()
                    && pyo_proto::proxy_listen_port(p.public())
                        == pyo_proto::proxy_listen_port(theirs.public());
                if same || inside {
                    let who = if team == team_id {
                        name.clone()
                    } else {
                        "another service".to_owned()
                    };
                    let what = if same {
                        format!(
                            "Port {}/{} is already used by {who} on this cluster.",
                            p.public(),
                            p.transport()
                        )
                    } else {
                        format!(
                            "Port {}/{} would clash with port {}/{} of {who} inside the gateway (ports below 1024 move up by 10000 there).",
                            p.public(),
                            p.transport(),
                            theirs.public(),
                            theirs.transport()
                        )
                    };
                    return Err(ApiError::conflict(what));
                }
            }
        }
    }
    Ok(())
}

// ── Operations ─────────────────────────────────────────────────────────────

/// A database a new service is connected to (createService).
#[derive(Deserialize)]
pub(crate) struct ServiceDatabase {
    id: String,
    #[serde(default)]
    env: Option<crate::databases::EnvMap>,
}

/// A change, posted to `/api/v1/ops` as `{"op": "<name>", …fields}`. The doc
/// comment on each variant is its description in the API reference
/// (`cargo run -p pyo-api --bin openapi`), so keep the first line plain.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(crate) enum Op {
    /// Create a team. You become its owner.
    CreateTeam {
        /// A new id you choose: `team_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        name: String,
        #[serde(default)]
        description: String,
    },
    /// Rename a team, change its description, default cluster or preferences.
    UpdateTeam {
        id: String,
        name: Option<String>,
        description: Option<String>,
        default_cluster_id: Option<String>,
        /// The team's dashboard preferences, saved as given.
        #[schema(value_type = Option<Object>)]
        preferences: Option<Value>,
    },
    /// Delete a team and everything in it. Owners only.
    DeleteTeam { id: String },
    /// Invite someone to a team by email, as owner, admin, developer or viewer.
    InviteMember {
        /// A new id you choose: `mem_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        team_id: String,
        email: String,
        /// "owner", "admin", "developer" or "viewer".
        role: String,
    },
    /// Make a new invite link (the earlier one stops working) and mail it, or with `send: false` only return it to share.
    ResendInvite {
        id: String,
        /// Mail the new link (default true).
        #[serde(default)]
        send: Option<bool>,
    },
    /// Back up Pyo's own data (installation admins only): where, how often and how many to keep. A blank secret keeps the saved one.
    SaveBackupSettings {
        #[serde(default)]
        destination: Option<String>,
        enabled: bool,
        schedule: String,
        hour: u8,
        keep: u32,
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        access_key: String,
        secret_key: Option<String>,
    },
    /// Stop backing up Pyo's own data.
    RemoveBackupSettings {},
    /// Back up a team's volumes and databases: where, how often and how many to keep. A blank secret keeps the saved one.
    SaveTeamBackupSettings {
        team_id: String,
        /// "s3" (any S3-compatible storage) when given.
        #[serde(default)]
        destination: Option<String>,
        enabled: bool,
        /// "daily" or "weekly".
        schedule: String,
        /// The hour of day it runs, 0 to 23 (UTC).
        hour: u8,
        /// How many backups to keep, 1 to 365.
        keep: u32,
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        access_key: String,
        secret_key: Option<String>,
    },
    /// Stop a team's backups.
    RemoveTeamBackupSettings { team_id: String },
    /// Choose what a team backs up: "all" but `exceptions`, or only the "selected" ones.
    SetTeamBackupSelection {
        team_id: String,
        /// "all" or "selected".
        mode: String,
        /// Service and volume ids: left out with "all", the only ones with "selected".
        exceptions: Vec<String>,
    },
    /// Back up now: one service or volume (by id), or everything selected.
    RunTeamBackup {
        team_id: String,
        target: Option<String>,
    },
    /// Create a Postgres, MySQL or KV (Redis-compatible key-value, Valkey) database on one of the team's clusters. The first database on a cluster also sets up databases there, which takes a minute or two. `size` sets CPU and memory per instance: "small" (0.25 CPU, 512 MiB, 5 GiB), "medium" (0.5, 1 GiB, 10 GiB), "large" (1, 2 GiB, 20 GiB) or "xlarge" (2, 4 GiB, 50 GiB). MySQL needs 1 GiB, so "medium" or larger. `instances` is 1 for a single server with no automatic failover. For high availability, use a primary and 2 to 8 replicas for Postgres (instances 3 to 9), 2 to 4 for KV (3 to 5), or 2 or 4 for MySQL (3 or 5). A replica takes over when the primary's server is lost. MySQL has no pooler, read replicas or REST API. Developers and up.
    CreateDatabase {
        /// A new id you choose: `mdb_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        team_id: String,
        cluster_id: String,
        /// Lowercase letters, digits and dashes, unique in the team.
        name: String,
        /// "postgres", "mysql" or "valkey".
        engine: String,
        /// "small", "medium", "large", "xlarge", or "custom" with `cpuMillis` and `memoryMi`.
        size: String,
        /// 1 (a single server), or high availability: 3 to 9 for Postgres (2 to 8 replicas), 3 to 5 for KV (2 to 4), 3 or 5 for MySQL (2 or 4).
        instances: i32,
        /// Postgres "16", "17" or "18" (the default). MySQL "8.4". Valkey "8.0" or "8.1" (the default).
        #[serde(default)]
        version: Option<String>,
        /// Storage per instance in GiB. Defaults to the size's storage.
        #[serde(default)]
        storage_gi: Option<i32>,
        /// Postgres: a connection pooler in front (on by default).
        #[serde(default)]
        pooler: Option<bool>,
        /// KV: a REST API, for serverless and edge code.
        #[serde(default)]
        rest: bool,
        /// A custom size's CPU per instance in millicores: 125 to 8000, in steps of 125.
        #[serde(default)]
        cpu_millis: Option<i32>,
        /// A custom size's memory per instance in MiB: 512 (Postgres), 1024 (MySQL) or 256 (KV) to 32768, in steps of 256.
        #[serde(default)]
        memory_mi: Option<i32>,
        /// Dedicated read replicas: 0 (the default) to 5 for Postgres, or to 4 for KV. MySQL has none.
        #[serde(default)]
        read_replicas: Option<i32>,
        /// Postgres extensions or KV modules to turn on with it, from `listDatabaseExtensions` with `section` "managed" for its engine. For example ["vector", "pg_cron"] or ["valkey-json"].
        #[serde(default)]
        extensions: Option<Vec<String>>,
    },
    /// Turn a Postgres database's extensions or a KV database's modules on and off. `extensions` is the full list to have on, from `listDatabaseExtensions` with `section` "managed" for its engine. The database's owner adds Postgres's "additional" ones with CREATE EXTENSION. Some load a library, which restarts the database (`restart`). With high availability, it restarts one instance at a time and the primary hands over last. Every KV module restarts the database and needs KV 8.1. Turning a Postgres extension off drops it (DROP EXTENSION, without CASCADE). Postgres refuses while other objects depend on it, and the database's `extensions` shows this. Developers and up.
    SetDatabaseExtensions { id: String, extensions: Vec<String> },
    /// Change a database's size, instances, storage, version, pooler, REST API, read replicas or deletion protection. Instances are 1 for a single server, or for high availability 2 to 8 replicas for Postgres, 2 to 4 for KV, or 2 or 4 for MySQL. MySQL instances only grow. Storage only grows and the version only goes up. Only owners and admins can turn deletion protection off. Connected services get changed variables on their next deploy.
    UpdateDatabase {
        id: String,
        #[serde(default)]
        size: Option<String>,
        #[serde(default)]
        instances: Option<i32>,
        #[serde(default)]
        storage_gi: Option<i32>,
        #[serde(default)]
        version: Option<String>,
        #[serde(default)]
        pooler: Option<bool>,
        #[serde(default)]
        rest: Option<bool>,
        #[serde(default)]
        deletion_protection: Option<bool>,
        /// Dedicated read replicas, each the database's size: 1 to 5 for Postgres, or 1 to 4 for KV. 0 turns them off. They only serve reads and never become the primary. Postgres: connected services get DATABASE_URL_READ (through their own pooler) while they're on. KV: REDIS_URL sends reads to them automatically.
        #[serde(default)]
        read_replicas: Option<i32>,
        /// With size "custom": CPU per instance in millicores (125 to 8000, steps of 125).
        #[serde(default)]
        cpu_millis: Option<i32>,
        /// With size "custom": memory per instance in MiB, from 512 for Postgres, 1024 for MySQL or 256 for KV, up to 32768, in steps of 256.
        #[serde(default)]
        memory_mi: Option<i32>,
    },
    /// Delete a database and its data. Refused while deletion protection is on or a service is connected. Owners and admins.
    DeleteDatabase { id: String },
    /// Connect a service to a database on its cluster. Postgres adds DATABASE_URL (through the pooler when it's on) and DATABASE_URL_DIRECT, plus DATABASE_URL_READ with read replicas. MySQL adds DATABASE_URL (mysql://, to the primary), MYSQL_HOST, MYSQL_PORT, MYSQL_USER, MYSQL_PASSWORD and MYSQL_DATABASE. KV adds REDIS_URL (which follows failover and sends reads to read replicas), plus KV_REST_API_URL, KV_REST_API_TOKEN and KV_REST_API_READ_ONLY_TOKEN (reads only) with its REST API. With `env`, it adds only those, under the service's own names. With `prefix`, it adds all of them with that prefix, so two databases' variables don't clash. They're secret variables and apply on the service's next deploy.
    ConnectDatabase {
        id: String,
        service_id: String,
        /// The service's own names for them: `{"WORDPRESS_DB_HOST": "MYSQL_HOST", …}`, each a variable the database offers, or several joined by ":" (`MYSQL_HOST:MYSQL_PORT`). Under its own names, Postgres also offers POSTGRES_HOST, POSTGRES_PORT, POSTGRES_USER, POSTGRES_PASSWORD and POSTGRES_DATABASE (the primary, not the pooler), and KV REDIS_HOST, REDIS_PORT and REDIS_PASSWORD. Kept, so they follow its changes.
        #[serde(default)]
        #[schema(value_type = Option<Object>)]
        env: Option<crate::databases::EnvMap>,
        /// Instead of `env`: all its variables, each named with this prefix (`MYSQL` gives MYSQL_DATABASE_URL…), so a service can use databases whose variables would clash.
        #[serde(default)]
        prefix: Option<String>,
    },
    /// Disconnect a service from a database: removes the variables it added (on the next deploy).
    DisconnectDatabase { id: String, service_id: String },
    /// Show a database's password, and a KV database's REST token and read-only token. Owners and admins. Recorded in the activity log.
    RevealDatabasePassword { id: String },
    /// Turn a database's backups on or off, to the team's Backups destination. Postgres: every change archived continuously and a base backup daily, so it can be restored to any moment in the last `retentionDays`. MySQL: a full backup daily with the changes since the last one, so it can be restored to any moment in the last `retentionDays` up to its latest backup. Both are stored in the team's folder of the destination. KV: a snapshot `schedule`d daily or weekly, the last `keep` kept, listed in Backups with the team's other databases. Developers and up.
    SetDatabaseBackups {
        id: String,
        enabled: bool,
        /// Postgres and MySQL: 1 to 90, 7 by default.
        #[serde(default)]
        retention_days: Option<u32>,
        /// The hour backups run, 0 to 23 (UTC). Defaults to 3 for Postgres and MySQL, and 2 for KV.
        #[serde(default)]
        hour: Option<u8>,
        /// KV: "daily" (default) or "weekly".
        #[serde(default)]
        schedule: Option<String>,
        /// KV: how many snapshots to keep, 1 to 365, 14 by default.
        #[serde(default)]
        keep: Option<u32>,
    },
    /// Restore a Postgres or MySQL database into a new one, on the same cluster with the same size and availability: at `targetTime` (RFC 3339, within what its backups cover: from `status.backup.firstRecoverabilityPoint` to now for Postgres, to `status.backup.lastSuccessfulBackup` for MySQL) or the latest when omitted. Developers and up.
    RestoreDatabase {
        /// The new database's id you choose: `mdb_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        /// The database to restore.
        source_id: String,
        /// The new database's name.
        name: String,
        #[serde(default)]
        target_time: Option<String>,
    },
    /// Change a backed-up database's schedule, or pause its backups.
    UpdateDatabaseBackup {
        id: String,
        enabled: bool,
        schedule: String,
        hour: u8,
        keep: u32,
    },
    /// Stop backing up a database. Its existing backups stay in storage.
    DeleteDatabaseBackup { id: String },
    /// Back up a database now.
    RunDatabaseBackup { id: String },
    /// Show the team's backup repository password, to restore without Pyo. Logged.
    RevealBackupPassword { team_id: String },
    /// Back up Pyo's own data now.
    RunBackup {},
    /// Set up outgoing email for this installation (admins only). A missing password keeps the saved one.
    SaveEmailSettings {
        host: String,
        port: u16,
        security: String,
        username: String,
        password: Option<String>,
        from_email: String,
        from_name: String,
        public_url: String,
    },
    /// Turn off outgoing email.
    RemoveEmailSettings {},
    /// What analytics keeps of visitors' IP addresses: "none", "anonymized" or "full".
    SaveObservabilitySettings {
        ip_mode: crate::observability::IpMode,
    },
    /// How Pyo updates itself: "manual" or "auto" in a weekly window (UTC).
    SaveUpdateSettings {
        mode: String,
        days: Vec<String>,
        hour: u8,
        check: bool,
    },
    /// Look for a new release now.
    CheckForUpdates {},
    /// Update to the latest release found (the version the admin saw).
    StartUpdate { version: String },
    /// Mail the signed-in admin a test message with the saved settings.
    SendTestEmail {},
    /// Create a personal API key. The key is returned only once.
    CreateApiToken {
        id: String,
        name: String,
        /// "read", "deploy" or "full" (the default).
        access: Option<String>,
        /// Only these teams; all of yours when omitted.
        team_ids: Option<Vec<String>>,
        /// 30, 90 or 365. Never expires when omitted.
        #[serde(default)]
        expires_in_days: Option<u32>,
    },
    /// Revoke an API key.
    RevokeApiToken { id: String },
    /// Upload a certificate for the team's domains: the chain and its private key, PEM.
    UploadCertificate {
        /// A new id you choose: `crt_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        team_id: String,
        name: String,
        /// The certificate chain, PEM.
        certificate: String,
        /// Its private key, PEM.
        private_key: String,
    },
    /// Replace a certificate with a renewed one. Services using it switch over in place, without a redeploy.
    ReplaceCertificate {
        id: String,
        certificate: String,
        private_key: String,
    },
    /// Remove an uploaded certificate no domain uses.
    RemoveCertificate { id: String },
    /// A client certificate for a CDN. Its key is only ever in the response.
    CreateOriginPullCertificate {
        team_id: String,
        name: String,
        common_name: String,
        years: i64,
    },
    /// Remove a CDN client certificate.
    RemoveOriginPullCertificate { id: String },
    /// The client certificates a cluster requires on HTTPS; none turns the requirement off.
    SetClusterOriginPull {
        cluster_id: String,
        certificate_ids: Vec<String>,
    },
    /// "local" (keep traffic on the same server), "region" (within the same region) or "spread" (across all servers).
    SetClusterTrafficMode { cluster_id: String, mode: String },
    /// Read the visitor's address from a PROXY protocol header on the
    /// cluster's websites (ports 80 and 443), for a cluster behind another
    /// load balancer. Connections without one still work.
    SetClusterWebProxyProtocol { cluster_id: String, on: bool },
    /// Whether a cluster takes public traffic: the gateway answers on every
    /// server but build-only ones (ports 80 and 443, and public TCP and UDP
    /// ports through it). Off: services are only reachable inside the cluster.
    SetClusterPublicTraffic { cluster_id: String, on: bool },
    /// Add an access rule to the end of a service's firewall. Allow and block rules are checked in order, and the first match decides. With no match, the client gets in. Rate limit rules then apply to websites, and each matching one counts the request, per client address and per server.
    AddFirewallRule {
        /// A new id you choose: `fwr_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        service_id: String,
        /// "allow", "block" or "rateLimit".
        action: String,
        /// "in" (the default) or "notIn": whether the client's address or country is in `sources`.
        #[serde(default = "is_in")]
        operator: String,
        /// Up to 50 IP addresses and ranges ("198.51.100.0/24", "2001:db8::/32") or two-letter country codes ("DE"), but not both. None (with "in") matches any client. Countries apply to websites only, since TCP and UDP ports never see a client's country. Rate limits take no countries, and only one value with "notIn".
        #[serde(default)]
        sources: Vec<String>,
        /// A rate limit's requests from each client address per `per`: 1 to 1,000,000.
        requests: Option<u32>,
        /// A rate limit's window: "second" or "minute" (the default).
        per: Option<String>,
        #[serde(default)]
        note: String,
    },
    /// A service's web application firewall (WAF) settings, for all its domains. "log" or "block" turns it on: the WAF starts on the service's cluster with the first service there that turns it on, and stops after the last one turns it off.
    SetServiceWaf {
        service_id: String,
        /// "off" (a new service's), "log" (log only) or "block".
        mode: String,
        /// How strict, 1 (fewest false alarms) to 4: the Core Rule Set's paranoia level.
        #[serde(default = "one")]
        level: u8,
        /// Rule ids not applied to this service, e.g. 942100.
        #[serde(default)]
        exceptions: Vec<u32>,
        /// The inbound anomaly score at which a request is blocked: 5 (the default), 10, 20 or 50.
        #[serde(default)]
        threshold: Option<u32>,
    },
    /// Add a custom web application firewall rule to a service. Custom rules run in order before the OWASP rules, while the service's WAF is on. It goes last in the list.
    AddWafRule {
        /// A new id you choose: `wfr_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        service_id: String,
        /// Up to 80 characters. Findings show it.
        name: String,
        /// "all" (every condition must match) or "any" (one is enough).
        #[serde(rename = "match")]
        matching: String,
        /// 1 to 10 tests on the request.
        conditions: Vec<crate::waf::ConditionInput>,
        /// "block" (403), "log", "allow" (skips later rules and the OWASP rules) or "skipOwasp" (later custom rules still run).
        action: String,
        #[serde(default = "yes")]
        enabled: bool,
    },
    /// Change a custom web application firewall rule. Omitted fields stay as they are.
    UpdateWafRule {
        id: String,
        name: Option<String>,
        #[serde(rename = "match")]
        matching: Option<String>,
        conditions: Option<Vec<crate::waf::ConditionInput>>,
        action: Option<String>,
        enabled: Option<bool>,
    },
    /// Put a service's custom web application firewall rules in this order. List every one of its rules.
    ReorderWafRules {
        service_id: String,
        ids: Vec<String>,
    },
    /// Remove a custom web application firewall rule.
    RemoveWafRule { id: String },
    /// Change a firewall access rule. Omitted fields stay as they are (as in `addFirewallRule`).
    UpdateFirewallRule {
        id: String,
        action: Option<String>,
        operator: Option<String>,
        sources: Option<Vec<String>>,
        requests: Option<u32>,
        per: Option<String>,
        note: Option<String>,
    },
    /// Put a service's firewall access rules in this order: allow and block rules are checked from the first. List every one of its rules.
    ReorderFirewallRules {
        service_id: String,
        ids: Vec<String>,
    },
    /// Remove a firewall access rule.
    RemoveFirewallRule { id: String },
    /// Disconnect an assistant connected over OAuth.
    RevokeOauthGrant { id: String },
    /// Start creating this installation's GitHub App (admins): returns the
    /// form to post to GitHub.
    GithubManifest {
        public_url: String,
        org: Option<String>,
    },
    /// Forget the GitHub App (and every team's installation of it).
    RemoveGithubApp {},
    /// The GitHub page where a team installs the GitHub App.
    GithubInstallUrl { team_id: String },
    /// Unlink a GitHub installation from a team.
    RemoveGithubInstallation { team_id: String, id: i64 },
    /// Create a deploy hook: a secret URL that deploys a service, or every service of a project.
    CreateDeployHook {
        /// A new id you choose: `hook_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        project_id: String,
        /// Only this service. Every service of the project when omitted.
        service_id: Option<String>,
        name: String,
    },
    /// Show a deploy hook's URL path (its secret token). Developers and up.
    RevealDeployHook { id: String },
    /// Remove a deploy hook. Its URL stops working.
    RemoveDeployHook { id: String },
    /// Change a member's role: "owner", "admin", "developer" or "viewer".
    UpdateMemberRole { id: String, role: String },
    /// Remove a member from a team, or revoke an invite.
    RemoveMember { id: String },
    /// Create a project in a team.
    CreateProject {
        /// A new id you choose: `prj_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        team_id: String,
        name: String,
        /// The project's slug, used in service addresses: lowercase letters, digits and dashes, unique in the team.
        namespace: String,
        #[serde(default)]
        description: String,
    },
    /// Rename a project or change its description.
    UpdateProject {
        id: String,
        name: Option<String>,
        description: Option<String>,
    },
    /// Delete a project and its services. Its volumes are kept, detached, unless `deleteVolumes`.
    DeleteProject {
        id: String,
        /// Delete its volumes and their data too, for good. Admins only.
        #[serde(default)]
        delete_volumes: bool,
    },
    /// Isolate a project's network: only its own services and the web server reach them.
    SetProjectIsolation { id: String, isolate: bool },
    /// Give the team a new data key and seal its stored secrets again with it.
    RotateDataKey { team_id: String },
    /// Create a service and deploy it.
    #[serde(rename = "createService")]
    CreateService {
        /// The service, as in the snapshot's services: id (`svc_…`), projectId, clusterId, name, source, ports, env and the rest.
        /// A template's service records where it came from in `source`: `templateVersion`, `appVersion`, `images` (each Compose service's image, `repository:tag@digest`, by name) and `fields` (the deploy form's values, with a secret one only as `{"secret": true, "set": …, "env": …}`). Saves keep them. Responses add `customized` when its image is no longer the template's.
        /// Its ports: `name`, `containerPort`, `protocol` ("HTTP", "TCP" or "UDP"), `exposure` ("internal", "http" for a website, "nodeport", or "loadbalancer" for a public TCP or UDP port) and `publicPort`.
        /// A public TCP or UDP port goes through the gateway unless `proxy` is false. Then, for TCP only, it can have `ipFilter` (`{"mode": "allow" or "deny", "cidrs": [...]}`), `connectionLimit` (open connections on the port, from everyone together), `sendProxyProtocol` ("V1" or "V2", to the service) and `acceptProxyProtocol` (read the visitor's address from a header sent by a load balancer in front, while connections without one still work). Ports 22, 53, 80, 443 and a few others can't be public. Each public port belongs to one service on its cluster.
        #[schema(value_type = Object)]
        service: Value,
        /// A new id you choose for its first deployment: `dpl_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        deployment_id: String,
        /// Volumes to create with it.
        #[serde(default)]
        #[schema(value_type = Vec<Object>)]
        volumes: Vec<Value>,
        /// Databases to connect it to before its first deploy, as `connectDatabase` does: each `{"id": "mdb_…", "env": {"WORDPRESS_DB_HOST": "MYSQL_HOST", …}}` (`env` optional).
        #[serde(default)]
        #[schema(value_type = Vec<Object>)]
        databases: Vec<ServiceDatabase>,
    },
    /// Save a service's configuration. With `deploymentId`, roll it out too. Changes to its `domains` go live without a deploy once it has been deployed.
    #[serde(rename = "saveService")]
    SaveService {
        /// The whole service, as in the snapshot.
        /// Its ports: `name`, `containerPort`, `protocol` ("HTTP", "TCP" or "UDP"), `exposure` ("internal", "http" for a website, "nodeport", or "loadbalancer" for a public TCP or UDP port) and `publicPort`.
        /// A public TCP or UDP port goes through the gateway unless `proxy` is false. Then, for TCP only, it can have `ipFilter` (`{"mode": "allow" or "deny", "cidrs": [...]}`), `connectionLimit` (open connections on the port, from everyone together), `sendProxyProtocol` ("V1" or "V2", to the service) and `acceptProxyProtocol` (read the visitor's address from a header sent by a load balancer in front, while connections without one still work). Ports 22, 53, 80, 443 and a few others can't be public. Each public port belongs to one service on its cluster.
        #[schema(value_type = Object)]
        service: Value,
        deployment_id: Option<String>,
        trigger: Option<String>,
    },
    /// Delete a service and stop it everywhere. Its volumes are kept, detached, unless `deleteVolumes`.
    #[serde(rename = "deleteService")]
    DeleteService {
        id: String,
        /// Delete its volumes and their data too, for good, once the service is gone. Admins only.
        #[serde(default)]
        delete_volumes: bool,
    },
    /// Deploy a service, roll back to an earlier deployment, or retry a failed one.
    Deploy {
        service_id: String,
        /// A new id you choose: `dpl_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        deployment_id: String,
        /// What started it, shown in the history: "manual" (also for anything else), "redeploy", "rollback", "retry" or "config_change".
        trigger: String,
        /// The deployment to go back to.
        rollback_of: Option<String>,
        /// The failed deployment to try again.
        retry_of: Option<String>,
    },
    /// Cancel a deployment that hasn't finished.
    Cancel { deployment_id: String },
    /// Restart a service's replicas one by one, with the same configuration.
    #[serde(rename = "restartService")]
    RestartService { id: String },
    /// Stop a service. Deploy it again to start it.
    #[serde(rename = "stopService")]
    StopService { id: String },
    /// Change a service's number of replicas without a new deployment.
    #[serde(rename = "scaleService")]
    ScaleService { id: String, replicas: i32 },
    /// Add a cron job to a service, or change one (by id).
    SaveCronJob {
        service_id: String,
        id: Option<String>,
        name: String,
        /// Five fields, in UTC: minute, hour, day of month, month, day of week.
        schedule: String,
        /// What it runs, through `sh -c`. Empty runs the image's default command.
        command: String,
        paused: bool,
    },
    /// Remove a cron job from a service.
    RemoveCronJob { service_id: String, id: String },
    /// Run a cron job now.
    RunCronJob { service_id: String, id: String },
    /// Turn autoscaling on or change it. Null turns it off.
    SetAutoscale {
        id: String,
        autoscale: Option<pyo_proto::Autoscale>,
    },
    /// Add a persistent volume to a service, mounted at `mountPath` from its next deploy.
    AddVolume {
        /// A new id you choose: `vol_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        service_id: String,
        name: String,
        mount_path: String,
        size_gi: i64,
    },
    /// Detach a volume from its service: unmounted from the next deploy. The volume and its data stay, to attach again or delete.
    DetachVolume { id: String },
    /// Attach a detached volume to a service on its cluster, mounted at `mountPath` from the service's next deploy.
    /// A service of another project (or any project, for a volume whose project was deleted) gets the volume moved into its project first, data, id and backups kept: the volume shows `moving` until its cluster has moved it, then it's attached (or `moveError` says why not). A volume can't move to another cluster.
    /// A volume on a one-server storage class keeps its data on that server, so the service must be able to run there.
    AttachVolume {
        id: String,
        service_id: String,
        mount_path: String,
    },
    /// Restore a volume from one of its snapshots (see `listVolumeSnapshots`), replacing its data. Owners and admins.
    /// Its service stops while the restore runs and starts again afterwards, even when it fails. The snapshot is restored next to the data and swapped in only once complete, so a failed restore leaves the data as it was. The volume needs room for the snapshot while this runs.
    /// The volume shows `restore` while it runs, then `restoredAt`, or `restoreError` with why it failed.
    RestoreVolume {
        id: String,
        /// The snapshot's `id` (or its first 8 or more characters).
        snapshot_id: String,
    },
    /// Grow a volume to `sizeGi` GB (never shrink it). Owners and admins.
    /// Only on a storage class that can grow volumes (`allowVolumeExpansion`). A `local-path` volume has no size limit of its own, so there's nothing to grow. The volume shows `resizeTo` until its cluster reports the new size. Some storage classes finish when its service next starts.
    ResizeVolume { id: String, size_gi: i64 },
    /// Delete a detached volume and its data, for good. Admins only.
    /// The volume stays listed, being deleted, until its cluster confirms. Also deletes a volume Pyo has no record of, listed in its cluster's `strayVolumes`: pass that `volumeId` as `id`, with `clusterId`.
    DeleteVolume {
        id: String,
        /// Only for a volume from a cluster's `strayVolumes`.
        cluster_id: Option<String>,
    },
    /// Check a domain's DNS now.
    VerifyDomain {
        service_id: String,
        domain_id: String,
    },
    /// Show a secret environment variable's value. Admins only, and logged.
    RevealSecret { service_id: String, env_id: String },
    /// Add a cluster. The response has the command that connects its first server.
    CreateCluster {
        /// A new id you choose: `cl_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        team_id: String,
        name: String,
        #[serde(default)]
        description: String,
        /// Where it is, "City, CC" or a region name.
        location: String,
        #[serde(default)]
        tags: Vec<String>,
        /// The machine Pyo itself runs on, connected with a command run there.
        #[serde(default)]
        this_machine: bool,
        /// Connect over the public internet instead of the tailnet: the agent reaches Pyo at this URL (where you use Pyo). Services Pyo builds can't run there (its registry is on the tailnet only).
        #[serde(default)]
        public_api_url: Option<String>,
    },
    /// Rename a cluster or change its description, location or tags.
    UpdateCluster {
        id: String,
        name: Option<String>,
        description: Option<String>,
        location: Option<String>,
        tags: Option<Vec<String>>,
    },
    /// Set a server's tags.
    SetServerTags {
        server_id: String,
        tags: Vec<String>,
    },
    /// On a GPU server: also run regular services, or keep it for GPU work. Null goes back to the default.
    SetServerRegularServices { server_id: String, on: Option<bool> },
    /// A new connect command for a cluster. The old one stops working.
    RotateAgentToken {
        id: String,
        #[serde(default)]
        this_machine: bool,
        /// As in `createCluster`: over the public internet, at this URL.
        #[serde(default)]
        public_api_url: Option<String>,
    },
    /// Discard a cluster that was added but never connected.
    DiscardClusterDraft { id: String },
    /// Remove a cluster from Pyo. Its servers keep running.
    RemoveCluster {
        id: String,
        /// For a cluster unreachable for 10 minutes, with services on it: Pyo forgets the cluster, its services, databases and volumes. Anything still running there is left alone.
        #[serde(default)]
        force: bool,
    },
    /// Whether new work may be placed on a server.
    SetServerSchedulable {
        server_id: String,
        schedulable: bool,
    },
    /// Put a server in maintenance (its services move elsewhere), or bring it back.
    SetServerMaintenance { server_id: String, on: bool },
    /// Take a server out of its cluster, moving its services off first. `force` removes it at once, for a machine that's gone.
    RemoveServer {
        server_id: String,
        #[serde(default)]
        force: bool,
    },
    /// Stop a server removal that's waiting or failed. The server takes work again.
    CancelServerRemoval { server_id: String },
    /// A script that joins a new server to a cluster (its token is valid for an hour).
    JoinServer {
        cluster_id: String,
        server_name: String,
        #[serde(default)]
        tags: Vec<String>,
    },
    /// Set a server's region: a name ("US East", "Singapore, SG"), or "" to detect it again.
    SetServerLocation { server_id: String, location: String },
    /// Change your name and time zone.
    UpdateProfile { name: String, timezone: String },
    /// Add a builder that builds images from Git on one of the team's servers (`kind` "cluster").
    CreateBuilder {
        id: String,
        team_id: String,
        name: String,
        /// "cluster".
        kind: String,
        cluster_id: Option<String>,
        /// The server it runs on.
        server: Option<String>,
    },
    /// Remove a builder.
    RemoveBuilder { id: String },
    /// A new token for a builder. The old one stops working.
    RotateBuilderToken { id: String },
    /// Save a team integration's key (`kind` "tailscale": an auth key, used when servers join over Tailscale).
    SaveIntegration {
        team_id: String,
        kind: String,
        secret: String,
    },
    /// Connect an external secret store, or change one. No credential keeps
    /// the saved one.
    SaveSecretStore {
        id: Option<String>,
        team_id: String,
        kind: String,
        name: String,
        addr: String,
        #[schema(value_type = Object)]
        options: Value,
        credential: Option<String>,
    },
    /// Disconnect a secret store.
    RemoveSecretStore { id: String },
    /// The key names at a path of a secret store, to check it before importing. Never values.
    CheckSecretImport { store_id: String, path: String },
    /// Save a login for a private registry, one per host. Pyo checks it with the registry first.
    SaveRegistryLogin {
        id: String,
        team_id: String,
        host: String,
        username: String,
        password: String,
    },
    /// Remove a registry login.
    RemoveRegistryLogin { id: String },
    /// Create a token to push (or only pull) the team's own images in Pyo registry, as `<host>:5443/<team>/<project>/<service>:<tag>`. These are slugs, and `<host>` is the installation's tailnet name. A push needs that project and service to exist. It works over HTTPS from a device on the installation's tailnet. Team owners and admins. The token comes back only once, with the username, address and team path to log in and push with.
    CreateRegistryToken {
        /// A new id you choose: `rtk_` then 4 to 40 lowercase letters, digits, dashes or underscores.
        id: String,
        team_id: String,
        name: String,
        /// "push" (push and pull, the default) or "pull".
        #[serde(default)]
        scope: Option<String>,
        /// 30 or 90. Never expires when omitted.
        #[serde(default)]
        expires_in_days: Option<u32>,
    },
    /// Revoke a Pyo registry token. Team owners and admins.
    RevokeRegistryToken { id: String },
    /// Remove a team integration's key.
    RemoveIntegration { team_id: String, kind: String },
    /// Create a project and its services from a Docker Compose plan (from `POST /compose/preview`, edited if you like), and deploy them in order.
    ImportCompose {
        team_id: String,
        /// Where they run. The team's default cluster when omitted.
        cluster_id: Option<String>,
        /// Add them to this project instead of creating one.
        #[serde(default)]
        project_id: Option<String>,
        plan: crate::compose::ComposePlan,
    },
}

/// Make a change
///
/// Every change goes through here: one operation per request, named by
/// `op`, with its fields. Ids of new things are chosen by you (the kind's
/// prefix and `_`, then 4 to 40 lowercase letters, digits, dashes or
/// underscores), so a retried request can't create something twice. Each
/// operation says which API key access it needs. Operations on your account
/// and on this installation aren't open to API keys and aren't listed.
#[utoipa::path(post, path = "/ops", operation_id = "runOp", tag = "Operations",
    request_body(content = Op, description = "The operation: `op` and its fields."),
    responses(
        (status = 200, description = "Done. Always `ok: true`, and some operations add more, such as a new API key or a join command.", body = Object),
        (status = 400, description = "Not a known operation, a missing field, or a value Pyo can't accept.", body = ErrorBody),
        (status = 403, description = "Your role in the team, or the API key's access, doesn't allow it.", body = ErrorBody),
        (status = 404, description = "The team or the thing it names doesn't exist, or isn't yours to see.", body = ErrorBody),
        (status = 409, description = "It conflicts with what's there, such as a name already taken.", body = ErrorBody),
        (status = 502, description = "An outside service it needed (a secret store, a registry) refused or failed.", body = ErrorBody),
        (status = 503, description = "This installation isn't set up to store the secret it would save.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("varies"))))]
async fn run(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Json(op): Json<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(run_json(&s, &user, op).await?))
}

/// Run an operation as `user` from anywhere else (MCP tools): `op` is the
/// same JSON the dashboard sends, e.g. {"op": "scaleService", "id": …}.
pub async fn run_json(s: &AppState, user: &User, op: Value) -> ApiResult<Value> {
    let op: Op = serde_json::from_value(op)
        .map_err(|e| ApiError::bad_request(format!("Invalid operation: {e}")))?;
    run_op(s, user, op).await
}

async fn run_op(s: &AppState, user: &User, op: Op) -> ApiResult<Value> {
    key_allows(user, &op)?;
    let s = s.clone();
    let mut tx = s.db.begin().await?;
    // Clusters whose agent should look for new work after this commits.
    let mut wake: Vec<String> = vec![];
    let out = apply(&s, &mut tx, user, op, &mut wake).await?;
    tx.commit().await?;
    for c in wake {
        s.wake(&c).await;
    }
    // A new deployment may have queued a build.
    s.wake("builders:*").await;
    Ok(out)
}

/// The least API key access an operation needs, or None when keys can't run
/// it at all: the account and the installation stay with the dashboard, so a
/// key can't make itself a broader key. The API reference shows this per
/// operation.
pub(crate) fn key_access_for(op: &Op) -> Option<KeyAccess> {
    if matches!(
        op,
        Op::CreateApiToken { .. }
            | Op::RevokeApiToken { .. }
            | Op::RevokeOauthGrant { .. }
            | Op::UpdateProfile { .. }
            | Op::SaveEmailSettings { .. }
            | Op::SaveBackupSettings { .. }
            | Op::RemoveBackupSettings {}
            | Op::RunBackup {}
            | Op::SaveObservabilitySettings { .. }
            | Op::SaveUpdateSettings { .. }
            | Op::CheckForUpdates {}
            | Op::StartUpdate { .. }
            | Op::RemoveEmailSettings {}
            | Op::SendTestEmail {}
            | Op::GithubManifest { .. }
            | Op::RemoveGithubApp {}
            | Op::CreateOriginPullCertificate { .. }
            | Op::RemoveOriginPullCertificate { .. }
            | Op::SetClusterOriginPull { .. }
            | Op::SetClusterTrafficMode { .. }
            | Op::SaveSecretStore { .. }
            | Op::RemoveSecretStore { .. }
    ) {
        return None;
    }
    let deploy_op = matches!(
        op,
        Op::Deploy { .. }
            | Op::Cancel { .. }
            | Op::RestartService { .. }
            | Op::StopService { .. }
            | Op::SetAutoscale { .. }
            | Op::RunCronJob { .. }
            | Op::ScaleService { .. }
    );
    Some(if deploy_op {
        KeyAccess::Deploy
    } else {
        KeyAccess::Full
    })
}

/// Where an operation is listed in the API reference, and the prefix of the
/// ids its `id` field takes (for the examples there). A new operation must
/// be placed here.
pub(crate) fn op_doc(op: &Op) -> (&'static str, &'static str) {
    match op {
        Op::CreateTeam { .. } | Op::UpdateTeam { .. } | Op::DeleteTeam { .. } => ("Teams", "team"),
        Op::InviteMember { .. }
        | Op::ResendInvite { .. }
        | Op::UpdateMemberRole { .. }
        | Op::RemoveMember { .. } => ("Teams", "mem"),
        Op::RotateDataKey { .. } => ("Teams", "team"),
        Op::CreateProject { .. }
        | Op::UpdateProject { .. }
        | Op::DeleteProject { .. }
        | Op::SetProjectIsolation { .. }
        | Op::ImportCompose { .. } => ("Projects", "prj"),
        Op::CreateService { .. }
        | Op::SaveService { .. }
        | Op::DeleteService { .. }
        | Op::RestartService { .. }
        | Op::StopService { .. }
        | Op::ScaleService { .. }
        | Op::SetAutoscale { .. }
        | Op::RevealSecret { .. } => ("Services", "svc"),
        Op::Deploy { .. } | Op::Cancel { .. } => ("Deployments", "dpl"),
        Op::CreateDeployHook { .. } | Op::RevealDeployHook { .. } | Op::RemoveDeployHook { .. } => {
            ("Deployments", "hook")
        }
        Op::SaveCronJob { .. } | Op::RemoveCronJob { .. } | Op::RunCronJob { .. } => {
            ("Cron jobs", "crn")
        }
        Op::VerifyDomain { .. } => ("Domains", "dom"),
        Op::UploadCertificate { .. }
        | Op::ReplaceCertificate { .. }
        | Op::RemoveCertificate { .. } => ("Domains", "crt"),
        Op::CreateOriginPullCertificate { .. }
        | Op::RemoveOriginPullCertificate { .. }
        | Op::SetClusterOriginPull { .. } => ("Domains", "crt"),
        Op::AddVolume { .. }
        | Op::DetachVolume { .. }
        | Op::AttachVolume { .. }
        | Op::DeleteVolume { .. }
        | Op::RestoreVolume { .. }
        | Op::ResizeVolume { .. } => ("Volumes", "vol"),
        Op::UpdateDatabaseBackup { .. }
        | Op::DeleteDatabaseBackup { .. }
        | Op::RunDatabaseBackup { .. } => ("Databases", "db"),
        Op::CreateDatabase { .. }
        | Op::UpdateDatabase { .. }
        | Op::SetDatabaseExtensions { .. }
        | Op::DeleteDatabase { .. }
        | Op::ConnectDatabase { .. }
        | Op::DisconnectDatabase { .. }
        | Op::RevealDatabasePassword { .. }
        | Op::SetDatabaseBackups { .. }
        | Op::RestoreDatabase { .. } => ("Databases", "mdb"),
        Op::SaveTeamBackupSettings { .. }
        | Op::RemoveTeamBackupSettings { .. }
        | Op::SetTeamBackupSelection { .. }
        | Op::RunTeamBackup { .. }
        | Op::RevealBackupPassword { .. } => ("Backups", "team"),
        Op::CreateCluster { .. }
        | Op::UpdateCluster { .. }
        | Op::RotateAgentToken { .. }
        | Op::DiscardClusterDraft { .. }
        | Op::RemoveCluster { .. }
        | Op::SetClusterTrafficMode { .. }
        | Op::SetClusterWebProxyProtocol { .. }
        | Op::SetClusterPublicTraffic { .. } => ("Clusters", "cl"),
        Op::AddFirewallRule { .. }
        | Op::UpdateFirewallRule { .. }
        | Op::ReorderFirewallRules { .. }
        | Op::RemoveFirewallRule { .. } => ("Firewall", "fwr"),
        Op::SetServiceWaf { .. } => ("Firewall", "svc"),
        Op::AddWafRule { .. }
        | Op::UpdateWafRule { .. }
        | Op::ReorderWafRules { .. }
        | Op::RemoveWafRule { .. } => ("Firewall", "wfr"),
        Op::JoinServer { .. }
        | Op::SetServerTags { .. }
        | Op::SetServerRegularServices { .. }
        | Op::SetServerSchedulable { .. }
        | Op::SetServerMaintenance { .. }
        | Op::RemoveServer { .. }
        | Op::CancelServerRemoval { .. }
        | Op::SetServerLocation { .. } => ("Servers", "srv"),
        Op::CreateBuilder { .. } | Op::RemoveBuilder { .. } | Op::RotateBuilderToken { .. } => {
            ("Builders", "bld")
        }
        Op::SaveRegistryLogin { .. } | Op::RemoveRegistryLogin { .. } => ("Images", "rgl"),
        Op::CreateRegistryToken { .. } | Op::RevokeRegistryToken { .. } => ("Images", "rtk"),
        Op::GithubInstallUrl { .. }
        | Op::RemoveGithubInstallation { .. }
        | Op::GithubManifest { .. }
        | Op::RemoveGithubApp {} => ("GitHub", "team"),
        Op::SaveIntegration { .. }
        | Op::RemoveIntegration { .. }
        | Op::CheckSecretImport { .. }
        | Op::SaveSecretStore { .. }
        | Op::RemoveSecretStore { .. } => ("Integrations", "sst"),
        // Not for API keys, so not in the reference.
        Op::CreateApiToken { .. }
        | Op::RevokeApiToken { .. }
        | Op::RevokeOauthGrant { .. }
        | Op::UpdateProfile { .. } => ("Account", "tok"),
        Op::SaveBackupSettings { .. }
        | Op::RemoveBackupSettings {}
        | Op::RunBackup {}
        | Op::SaveEmailSettings { .. }
        | Op::RemoveEmailSettings {}
        | Op::SendTestEmail {}
        | Op::SaveObservabilitySettings { .. }
        | Op::SaveUpdateSettings { .. }
        | Op::CheckForUpdates {}
        | Op::StartUpdate { .. } => ("Settings", ""),
    }
}

/// What an API key may change; the team and role checks still apply on top.
fn key_allows(user: &User, op: &Op) -> ApiResult<()> {
    let Some(k) = &user.key else { return Ok(()) };
    let Some(need) = key_access_for(op) else {
        return Err(ApiError::forbidden(
            "API keys can't manage your account or this installation. Use the dashboard.",
        ));
    };
    match k.access {
        KeyAccess::Read => Err(ApiError::forbidden("This API key can only read.")),
        KeyAccess::Deploy if need > KeyAccess::Deploy => Err(ApiError::forbidden(
            "This API key can only deploy, roll back, restart, stop and scale services.",
        )),
        // A new team would be outside the key's teams.
        KeyAccess::Full if k.teams.is_some() && matches!(op, Op::CreateTeam { .. }) => Err(
            ApiError::forbidden("This API key is limited to some teams, so it can't create teams."),
        ),
        _ => Ok(()),
    }
}

async fn apply(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    op: Op,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let ok = json!({ "ok": true });
    match op {
        // ── Teams and members ──
        Op::CreateTeam {
            id,
            name,
            description,
        } => {
            client_id(&id, "team")?;
            let name = require_text(&name, "Team name", 60)?;
            let slug = unique_team_slug(tx, &slugify(&name), None).await?;
            sqlx::query(
                "INSERT INTO pyo.teams (id, name, slug, description) VALUES ($1, $2, $3, $4)",
            )
            .bind(&id)
            .bind(&name)
            .bind(&slug)
            .bind(description.trim())
            .execute(&mut **tx)
            .await?;
            sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')")
                .bind(new_id("mem"))
                .bind(&id)
                .bind(&user.id)
                .bind(&user.email)
                .execute(&mut **tx)
                .await?;
            log_activity(
                tx,
                &id,
                &user.name,
                "team.created",
                "info",
                &format!("Created team {name}"),
                json!({ "kind": "team", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(json!({ "ok": true, "slug": slug }))
        }
        Op::UpdateTeam {
            id,
            name,
            description,
            default_cluster_id,
            preferences,
        } => {
            require_role(tx, user, &id, Role::Admin).await?;
            let mut slug = None;
            if let Some(n) = name {
                let n = require_text(&n, "Team name", 60)?;
                let sl = unique_team_slug(tx, &slugify(&n), Some(&id)).await?;
                sqlx::query("UPDATE pyo.teams SET name = $2, slug = $3 WHERE id = $1")
                    .bind(&id)
                    .bind(&n)
                    .bind(&sl)
                    .execute(&mut **tx)
                    .await?;
                slug = Some(sl);
            }
            if let Some(d) = description {
                sqlx::query("UPDATE pyo.teams SET description = $2 WHERE id = $1")
                    .bind(&id)
                    .bind(d.trim())
                    .execute(&mut **tx)
                    .await?;
            }
            if let Some(c) = default_cluster_id {
                let c = if c.is_empty() { None } else { Some(c) };
                sqlx::query("UPDATE pyo.teams SET default_cluster_id = $2 WHERE id = $1")
                    .bind(&id)
                    .bind(c)
                    .execute(&mut **tx)
                    .await?;
            }
            if let Some(p) = preferences {
                sqlx::query("UPDATE pyo.teams SET preferences = preferences || $2 WHERE id = $1")
                    .bind(&id)
                    .bind(p)
                    .execute(&mut **tx)
                    .await?;
            }
            log_activity(
                tx,
                &id,
                &user.name,
                "team.updated",
                "info",
                "Updated team settings",
                json!({ "kind": "team", "id": id, "label": "" }),
                json!({}),
            )
            .await?;
            Ok(json!({ "ok": true, "slug": slug }))
        }
        Op::DeleteTeam { id } => {
            require_role(tx, user, &id, Role::Owner).await?;
            let services: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE team_id = $1")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            if services > 0 {
                return Err(ApiError::conflict(
                    "Delete the team's services first. Their workloads are still running.",
                ));
            }
            let teams: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pyo.members WHERE user_id = $1 AND status = 'active'",
            )
            .bind(&user.id)
            .fetch_one(&mut **tx)
            .await?;
            if teams <= 1 {
                return Err(ApiError::conflict(
                    "This is your only team. Create another one before deleting it.",
                ));
            }
            sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::InviteMember {
            id,
            team_id,
            email,
            role,
        } => {
            client_id(&id, "mem")?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let r = parse_role(&role)?;
            if r == Role::Owner {
                return Err(ApiError::bad_request(
                    "Invite as admin, developer or viewer.",
                ));
            }
            let email = email.trim().to_lowercase();
            if !email.contains('@') {
                return Err(ApiError::bad_request("Enter a valid email address."));
            }
            // An existing account joins right away; anyone else when they sign up.
            let existing: Option<String> =
                sqlx::query_scalar("SELECT id FROM pyo.users WHERE email = $1")
                    .bind(&email)
                    .fetch_optional(&mut **tx)
                    .await?;
            sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(&id)
                .bind(&team_id)
                .bind(&existing)
                .bind(&email)
                .bind(&role)
                .bind(if existing.is_some() { "active" } else { "invited" })
                .execute(&mut **tx)
                .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "member.invited",
                "info",
                &format!("Invited {email} as {role}"),
                json!({ "kind": "team", "id": team_id, "label": email }),
                json!({}),
            )
            .await?;
            let invite = match existing {
                Some(_) => None,
                None => Some(crate::auth::new_invite(tx, &id).await?),
            };
            Ok(mail_invite(s, tx, user, &team_id, &email, &role, invite.as_deref()).await?)
        }
        Op::ResendInvite { id, send } => {
            let team_id = team_of(tx, "members", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let (email, role, status): (String, String, String) =
                sqlx::query_as("SELECT email, role, status FROM pyo.members WHERE id = $1")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            if status != "invited" {
                return Err(ApiError::bad_request("They already joined."));
            }
            // A new link each time; the one sent before stops working.
            let invite = crate::auth::new_invite(tx, &id).await?;
            if !send.unwrap_or(true) {
                return Ok(
                    json!({ "ok": true, "emailed": false, "link": crate::auth::invite_path(&invite) }),
                );
            }
            let out = mail_invite(s, tx, user, &team_id, &email, &role, Some(&invite)).await?;
            if out["emailed"] != json!(true) {
                return Err(ApiError::bad_request(
                    out["emailError"]
                        .as_str()
                        .unwrap_or(
                            "Email isn't set up. An admin can set it up in Settings, under Email.",
                        )
                        .to_owned(),
                ));
            }
            Ok(out)
        }
        Op::SaveBackupSettings {
            destination,
            enabled,
            schedule,
            hour,
            keep,
            endpoint,
            region,
            bucket,
            prefix,
            access_key,
            secret_key,
        } => {
            require_admin(user)?;
            if !s.sealer.enabled() {
                return Err(ApiError::bad_request(
                    "Set PYO_SECRET_KEY on the API first: backups are encrypted with it.",
                ));
            }
            let saved = crate::backup::settings(&s.db).await?;
            let secret = secret_key
                .filter(|k| !k.trim().is_empty())
                .map(|k| k.trim().to_owned());
            let cfg = crate::backup::Settings {
                destination: destination.unwrap_or_else(|| "local".into()),
                enabled,
                schedule,
                hour,
                keep,
                endpoint: endpoint.trim().trim_end_matches('/').to_owned(),
                region: region.trim().to_owned(),
                bucket: bucket.trim().to_owned(),
                prefix: prefix.trim().to_owned(),
                access_key: access_key.trim().to_owned(),
                secret_sealed: match &secret {
                    Some(k) => Some(s.sealer.seal(k)?),
                    None => saved.and_then(|x| x.secret_sealed),
                },
            };
            cfg.validate()
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            // Prove the destination works before relying on it.
            crate::backup::probe(
                s,
                &cfg,
                secret.as_deref(),
                crate::outbound::Reach::installation(),
            )
            .await
            .map_err(|e| ApiError::bad_request(format!("Couldn't use that bucket: {e:#}")))?;
            sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('backup', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()")
                .bind(serde_json::to_value(&cfg).map_err(anyhow::Error::from)?)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::SaveTeamBackupSettings {
            team_id,
            destination,
            enabled,
            schedule,
            hour,
            keep,
            endpoint,
            region,
            bucket,
            prefix,
            access_key,
            secret_key,
        } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            if !s.sealer.enabled() {
                return Err(ApiError::bad_request(
                    "Set PYO_SECRET_KEY on the API first: backups are encrypted with it.",
                ));
            }
            let saved = crate::teambackup::settings(&s.db, &team_id).await?;
            let secret = secret_key
                .filter(|k| !k.trim().is_empty())
                .map(|k| k.trim().to_owned());
            let base = crate::backup::Settings {
                destination: destination.unwrap_or_else(|| "local".into()),
                enabled,
                schedule,
                hour,
                keep,
                endpoint: endpoint.trim().trim_end_matches('/').to_owned(),
                region: region.trim().to_owned(),
                bucket: bucket.trim().to_owned(),
                prefix: prefix.trim().to_owned(),
                access_key: access_key.trim().to_owned(),
                secret_sealed: match &secret {
                    Some(k) => Some(s.sealer.seal(k)?),
                    None => saved.as_ref().and_then(|x| x.base.secret_sealed.clone()),
                },
            };
            base.validate()
                .map_err(|e| ApiError::bad_request(e.to_string()))?;
            if base.is_local() {
                // The team's own login to Pyo's store, made now: proves it works.
                crate::backup::team_login(&s.db, &team_id)
                    .await
                    .map_err(|e| {
                        ApiError::bad_request(format!(
                            "Couldn't use this server's backup store: {e:#}"
                        ))
                    })?;
            } else {
                crate::backup::probe(s, &base, secret.as_deref(), crate::outbound::Reach::team())
                    .await
                    .map_err(|e| {
                        ApiError::bad_request(format!("Couldn't use that bucket: {e:#}"))
                    })?;
            }
            let cfg = crate::teambackup::TeamSettings {
                repo_password_sealed: crate::teambackup::repo_password(&team_id, saved.as_ref())
                    .await?,
                // Saving the destination keeps what's picked.
                mode: saved
                    .as_ref()
                    .map(|x| x.mode.clone())
                    .unwrap_or_else(|| "all".into()),
                exceptions: saved.map(|x| x.exceptions).unwrap_or_default(),
                base,
            };
            crate::teambackup::save(&s.db, &team_id, &cfg).await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "backup.settings",
                "info",
                "Set up backups",
                json!({ "kind": "team", "id": team_id, "label": "Backups" }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::SetTeamBackupSelection {
            team_id,
            mode,
            exceptions,
        } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            if !matches!(mode.as_str(), "all" | "selected") {
                return Err(ApiError::bad_request(
                    "Back up everything, or only what's selected.",
                ));
            }
            // Works on the default destination too (this server's store).
            let mut cfg = crate::teambackup::effective(&s.db, &team_id).await?;
            cfg.mode = mode;
            let mut exceptions = exceptions;
            exceptions.sort();
            exceptions.dedup();
            cfg.exceptions = exceptions;
            crate::teambackup::save(&s.db, &team_id, &cfg).await?;
            Ok(ok)
        }
        Op::RemoveTeamBackupSettings { team_id } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            sqlx::query("DELETE FROM pyo.team_backup_settings WHERE team_id = $1")
                .bind(&team_id)
                .execute(&mut **tx)
                .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "backup.settings",
                "warning",
                "Turned off backups",
                json!({ "kind": "team", "id": team_id, "label": "Backups" }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::RunTeamBackup { team_id, target } => {
            require_role(tx, user, &team_id, Role::Developer).await?;
            let n = crate::teambackup::queue(s, tx, &team_id, target.as_deref(), "manual").await?;
            if n == 0 {
                return Err(ApiError::bad_request(
                    "Nothing to back up right now: it's already running, or nothing is deployed with data.",
                ));
            }
            Ok(json!({ "ok": true, "queued": n }))
        }
        Op::UpdateDatabaseBackup {
            id,
            enabled,
            schedule,
            hour,
            keep,
        } => {
            let team_id = team_of(tx, "backup_databases", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            crate::database_backups::schedule_ok(&schedule, hour, keep)?;
            sqlx::query("UPDATE pyo.backup_databases SET enabled = $2, schedule = $3, hour = $4, keep = $5 WHERE id = $1")
                .bind(&id)
                .bind(enabled)
                .bind(&schedule)
                .bind(i32::from(hour))
                .bind(keep as i32)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::DeleteDatabaseBackup { id } => {
            let team_id = team_of(tx, "backup_databases", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let name: String =
                sqlx::query_scalar("DELETE FROM pyo.backup_databases WHERE id = $1 RETURNING name")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "backup.database",
                "warning",
                &format!("Stopped backing up {name}"),
                json!({ "kind": "database", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::RunDatabaseBackup { id } => {
            let team_id = team_of(tx, "backup_databases", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let backup = crate::database_backups::queue(s, tx, &id, "manual").await?;
            Ok(json!({ "ok": true, "id": backup }))
        }
        Op::RevealBackupPassword { team_id } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            let cfg = crate::teambackup::settings(&s.db, &team_id)
                .await?
                .ok_or_else(|| ApiError::not_found("Backup settings"))?;
            let password = crate::vault::get().open(&cfg.repo_password_sealed).await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "secret.revealed",
                "warning",
                "Revealed the backup repository password",
                json!({ "kind": "team", "id": team_id, "label": "Backups" }),
                json!({}),
            )
            .await?;
            Ok(json!({ "ok": true, "password": password }))
        }
        Op::RemoveBackupSettings {} => {
            require_admin(user)?;
            sqlx::query("DELETE FROM pyo.settings WHERE key = 'backup'")
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::RunBackup {} => {
            require_admin(user)?;
            if crate::backup::settings(&s.db).await?.is_none()
                && crate::backup::local_store().is_none()
            {
                return Err(ApiError::bad_request("Set up a backup destination first."));
            }
            let id = crate::backup::queue(&s.db, "manual").await?;
            Ok(json!({ "ok": true, "id": id }))
        }
        Op::SaveEmailSettings {
            host,
            port,
            security,
            username,
            password,
            from_email,
            from_name,
            public_url,
        } => {
            require_admin(user)?;
            let saved = mail::settings(&s.db).await?.unwrap_or_default();
            let password_sealed = match password.filter(|p| !p.is_empty()) {
                Some(p) => {
                    if !s.sealer.enabled() {
                        return Err(ApiError::bad_request(
                            "This installation has no secret key (PYO_SECRET_KEY), so it can't store the password.",
                        ));
                    }
                    Some(s.sealer.seal(&p)?)
                }
                // Same login as before: keep the password.
                None if username.trim() == saved.username => saved.password_sealed,
                None => None,
            };
            let cfg = mail::EmailSettings {
                host: host.trim().to_owned(),
                port,
                security,
                username: username.trim().to_owned(),
                password_sealed,
                from_email: from_email.trim().to_owned(),
                from_name: from_name.trim().to_owned(),
                public_url: public_url.trim().trim_end_matches('/').to_owned(),
            };
            mail::validate(&cfg)?;
            sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('email', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()")
                .bind(serde_json::to_value(&cfg).map_err(anyhow::Error::from)?)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::SaveUpdateSettings {
            mode,
            days,
            hour,
            check,
        } => {
            require_admin(user)?;
            let mut days = days;
            days.dedup();
            crate::updates::save_settings(
                tx,
                &crate::updates::Settings {
                    mode,
                    days,
                    hour,
                    check,
                },
            )
            .await?;
            Ok(ok)
        }
        Op::CheckForUpdates {} => {
            require_admin(user)?;
            if crate::updates::checks_off_by_env() {
                return Err(ApiError::bad_request(
                    "Update checks are off on this installation (PYO_UPDATE_CHECK=off).",
                ));
            }
            let c = crate::updates::check(&s.db).await?;
            Ok(
                json!({ "ok": c.error.is_none(), "error": c.error, "version": c.found.map(|f| f.release.version) }),
            )
        }
        Op::StartUpdate { version } => {
            require_admin(user)?;
            let id = crate::updates::start(s, &version, Some(user)).await?;
            Ok(json!({ "ok": true, "id": id }))
        }
        Op::SaveObservabilitySettings { ip_mode } => {
            require_admin(user)?;
            let mut v: Value =
                sqlx::query_scalar("SELECT value FROM pyo.settings WHERE key = 'observability'")
                    .fetch_optional(&mut **tx)
                    .await?
                    .unwrap_or_else(|| json!({}));
            v["ipMode"] = json!(ip_mode);
            sqlx::query("INSERT INTO pyo.settings (key, value) VALUES ('observability', $1) ON CONFLICT (key) DO UPDATE SET value = excluded.value")
                .bind(&v)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::RemoveEmailSettings {} => {
            require_admin(user)?;
            sqlx::query("DELETE FROM pyo.settings WHERE key = 'email'")
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::GithubManifest { public_url, org } => {
            require_admin(user)?;
            if !s.sealer.enabled() {
                return Err(ApiError::bad_request(
                    "This installation has no secret key (PYO_SECRET_KEY), so it can't store the app's keys.",
                ));
            }
            crate::github::manifest(&user.id, &public_url, org.as_deref()).await
        }
        Op::RemoveGithubApp {} => {
            require_admin(user)?;
            sqlx::query("DELETE FROM pyo.settings WHERE key = 'github'")
                .execute(&mut **tx)
                .await?;
            sqlx::query("DELETE FROM pyo.github_installations")
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::GithubInstallUrl { team_id } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            Ok(
                json!({ "ok": true, "url": crate::github::install_url(s, &user.id, &team_id).await? }),
            )
        }
        Op::RemoveGithubInstallation { team_id, id } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            sqlx::query("DELETE FROM pyo.github_installations WHERE id = $1 AND team_id = $2")
                .bind(id)
                .bind(&team_id)
                .execute(&mut **tx)
                .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "integration.removed",
                "info",
                "Disconnected a GitHub account",
                json!({ "kind": "team", "id": team_id, "label": "GitHub" }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::CreateDeployHook {
            id,
            project_id,
            service_id,
            name,
        } => {
            client_id(&id, "hook")?;
            let team_id = team_of(tx, "projects", &project_id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let name = require_text(&name, "Hook name", 60)?;
            if let Some(a) = &service_id {
                let in_project: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pyo.services WHERE id = $1 AND project_id = $2)",
                )
                .bind(a)
                .bind(&project_id)
                .fetch_one(&mut **tx)
                .await?;
                if !in_project {
                    return Err(ApiError::bad_request("That service isn't in this project."));
                }
            }
            if !s.sealer.enabled() {
                return Err(ApiError::bad_request(
                    "This installation has no secret key (PYO_SECRET_KEY), so it can't store hook URLs.",
                ));
            }
            let token = format!("dh_{}", secret_token());
            sqlx::query("INSERT INTO pyo.deploy_hooks (id, team_id, project_id, service_id, name, token_hash, token_sealed, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
                .bind(&id)
                .bind(&team_id)
                .bind(&project_id)
                .bind(&service_id)
                .bind(&name)
                .bind(sha256_hex(&token))
                .bind(s.sealer.seal(&token)?)
                .bind(&user.name)
                .execute(&mut **tx)
                .await?;
            Ok(json!({ "ok": true, "path": format!("/api/hooks/deploy/{token}") }))
        }
        Op::RevealDeployHook { id } => {
            let team_id = team_of(tx, "deploy_hooks", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let sealed: String =
                sqlx::query_scalar("SELECT token_sealed FROM pyo.deploy_hooks WHERE id = $1")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            Ok(
                json!({ "ok": true, "path": format!("/api/hooks/deploy/{}", s.sealer.open(&sealed)?) }),
            )
        }
        Op::RemoveDeployHook { id } => {
            let team_id = team_of(tx, "deploy_hooks", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            sqlx::query("DELETE FROM pyo.deploy_hooks WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::CreateApiToken {
            id,
            name,
            access,
            team_ids,
            expires_in_days,
        } => {
            client_id(&id, "tok")?;
            let name = require_text(&name, "Key name", 60)?;
            let access = access.unwrap_or_else(|| "full".into());
            if KeyAccess::parse(&access).is_none() {
                return Err(ApiError::bad_request("Access is read, deploy or full."));
            }
            let days = crate::auth::key_lifetime(expires_in_days)?;
            let team_ids = team_ids.filter(|t| !t.is_empty());
            for t in team_ids.iter().flatten() {
                require_role(tx, user, t, Role::Viewer).await?;
            }
            let token = format!("{}{}", crate::auth::API_KEY_PREFIX, secret_token());
            sqlx::query(
                "INSERT INTO pyo.api_tokens (id, user_id, name, token_hash, prefix, access, team_ids, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(days => $8))",
            )
            .bind(&id)
            .bind(&user.id)
            .bind(&name)
            .bind(sha256_hex(&token))
            .bind(token.chars().take(10).collect::<String>())
            .bind(&access)
            .bind(&team_ids)
            .bind(days)
            .execute(&mut **tx)
            .await?;
            Ok(json!({ "ok": true, "token": token }))
        }
        Op::UploadCertificate {
            id,
            team_id,
            name,
            certificate,
            private_key,
        } => {
            client_id(&id, "crt")?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let p = crate::certs::parse(&certificate, &private_key, now_ms() / 1000)?;
            let name = match name.trim() {
                "" => p.hostnames[0].clone(),
                n => require_text(n, "Certificate name", 60)?,
            };
            let sealed = s.sealer.seal(&p.key_pem).map_err(|_| {
                ApiError::bad_request("Set PYO_SECRET_KEY on the API to store private keys.")
            })?;
            sqlx::query(
                "INSERT INTO pyo.certificates (id, team_id, name, hostnames, issuer, not_before, not_after, fingerprint, chain_pem, key_sealed)
                 VALUES ($1, $2, $3, $4, $5, to_timestamp($6), to_timestamp($7), $8, $9, $10)",
            )
            .bind(&id)
            .bind(&team_id)
            .bind(&name)
            .bind(&p.hostnames)
            .bind(&p.issuer)
            .bind(p.not_before as f64)
            .bind(p.not_after as f64)
            .bind(&p.fingerprint)
            .bind(&p.chain_pem)
            .bind(&sealed)
            .execute(&mut **tx)
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "domain.certificate_uploaded",
                "info",
                &format!("Uploaded certificate {name}"),
                json!({ "kind": "certificate", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(json!({ "ok": true, "hostnames": p.hostnames, "warnings": p.warnings }))
        }
        Op::CreateOriginPullCertificate {
            team_id,
            name,
            common_name,
            years,
        } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            let name = require_text(&name, "Certificate name", 60)?;
            let common_name = common_name.trim().to_owned();
            let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ');
            // The CA adds " CA", and X.509 allows 64 characters.
            if common_name.is_empty() || common_name.len() > 60 || !common_name.chars().all(valid) {
                return Err(ApiError::bad_request(
                    "Use up to 60 letters, numbers, dots, dashes and spaces for the common name.",
                ));
            }
            if !(1..=15).contains(&years) {
                return Err(ApiError::bad_request("Make it valid for 1 to 15 years."));
            }
            let id = new_id("opc");
            let g = crate::certs::generate_origin_pull(&common_name, years, now_ms() / 1000)?;
            sqlx::query(
                "INSERT INTO pyo.origin_pull_certificates (id, team_id, name, common_name, ca_pem, fingerprint, not_after)
                 VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7))",
            )
            .bind(&id)
            .bind(&team_id)
            .bind(&name)
            .bind(&common_name)
            .bind(&g.ca_pem)
            .bind(&g.fingerprint)
            .bind(g.not_after as f64)
            .execute(&mut **tx)
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "domain.certificate_uploaded",
                "info",
                &format!("Created mTLS certificate {name}"),
                json!({ "kind": "certificate", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(
                json!({ "ok": true, "id": id, "certificate": g.certificate_pem, "privateKey": g.private_key_pem, "caCertificate": g.ca_pem }),
            )
        }
        Op::RemoveOriginPullCertificate { id } => {
            let team_id = team_of(tx, "origin_pull_certificates", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let used: Option<String> = sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE origin_pull->'enforcedIds' ? $1 OR (origin_pull->>'state' = 'applying' AND origin_pull->'certificateIds' ? $1) LIMIT 1").bind(&id).fetch_optional(&mut **tx).await?;
            if let Some(cluster) = used {
                return Err(ApiError::conflict(format!(
                    "{cluster} requires this certificate. Stop using it there first."
                )));
            }
            let name: String = sqlx::query_scalar(
                "DELETE FROM pyo.origin_pull_certificates WHERE id = $1 RETURNING name",
            )
            .bind(&id)
            .fetch_one(&mut **tx)
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "domain.certificate_removed",
                "info",
                &format!("Removed mTLS certificate {name}"),
                json!({ "kind": "certificate", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::SetClusterTrafficMode { cluster_id, mode } => {
            let team_id = team_of(tx, "clusters", &cluster_id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            if ![
                pyo_proto::TRAFFIC_LOCAL,
                pyo_proto::TRAFFIC_REGION,
                pyo_proto::TRAFFIC_SPREAD,
            ]
            .contains(&mode.as_str())
            {
                return Err(ApiError::bad_request(
                    "Traffic routing is \"local\" (same server), \"region\" (same region) or \"spread\" (all servers).",
                ));
            }
            let (name, status, capable, local): (String, String, bool, bool) = sqlx::query_as(
                "SELECT name, status, coalesce((capabilities->>'trafficModes')::boolean, false), coalesce((capabilities->>'trafficLocal')::boolean, false) FROM pyo.clusters WHERE id = $1",
            )
            .bind(&cluster_id)
            .fetch_one(&mut **tx)
            .await?;
            if !capable || (mode == pyo_proto::TRAFFIC_LOCAL && !local) {
                return Err(ApiError::conflict(format!(
                    "{name}'s agent can't route traffic this way yet."
                )));
            }
            if status != "connected" && status != "degraded" {
                return Err(ApiError::conflict(format!(
                    "{name}'s agent isn't connected. Try again once it is."
                )));
            }
            sqlx::query("UPDATE pyo.clusters SET traffic_mode = $2 WHERE id = $1")
                .bind(&cluster_id)
                .bind(&mode)
                .execute(&mut **tx)
                .await?;
            // Every service switches now, without a redeploy; later deploys carry it.
            deploy::enqueue(
                tx,
                &cluster_id,
                Command::SetTrafficMode { mode: mode.clone() },
            )
            .await?;
            let title = match mode.as_str() {
                pyo_proto::TRAFFIC_SPREAD => format!("Set {name}'s traffic routing to Global"),
                pyo_proto::TRAFFIC_LOCAL => format!("Set {name}'s traffic routing to Local"),
                _ => format!("Set {name}'s traffic routing to Regional"),
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "cluster.updated",
                "info",
                &title,
                json!({ "kind": "cluster", "id": cluster_id, "label": name }),
                json!({ "clusterId": cluster_id }),
            )
            .await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::SetClusterWebProxyProtocol { cluster_id, on } => {
            let team_id = team_of(tx, "clusters", &cluster_id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let (name, status): (String, String) =
                sqlx::query_as("SELECT name, status FROM pyo.clusters WHERE id = $1")
                    .bind(&cluster_id)
                    .fetch_one(&mut **tx)
                    .await?;
            if status != "connected" && status != "degraded" {
                return Err(ApiError::conflict(format!(
                    "{name}'s agent isn't connected. Try again once it is."
                )));
            }
            sqlx::query("UPDATE pyo.clusters SET web_proxy_protocol = $2 WHERE id = $1")
                .bind(&cluster_id)
                .bind(on)
                .execute(&mut **tx)
                .await?;
            deploy::enqueue(tx, &cluster_id, Command::SetWebProxyProtocol { on }).await?;
            let title = if on {
                format!("Websites on {name} accept PROXY protocol")
            } else {
                format!("Websites on {name} no longer read PROXY protocol")
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "cluster.updated",
                "info",
                &title,
                json!({ "kind": "cluster", "id": cluster_id, "label": name }),
                json!({ "clusterId": cluster_id }),
            )
            .await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::SetClusterPublicTraffic { cluster_id, on } => {
            let team_id = team_of(tx, "clusters", &cluster_id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let name: String = sqlx::query_scalar(
                "UPDATE pyo.clusters SET public_traffic = $2 WHERE id = $1 RETURNING name",
            )
            .bind(&cluster_id)
            .bind(on)
            .fetch_one(&mut **tx)
            .await?;
            // The agent publishes the gateway or not on its next heartbeat.
            let title = if on {
                format!("{name} takes public traffic")
            } else {
                format!("{name} no longer takes public traffic")
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "cluster.updated",
                "info",
                &title,
                json!({ "kind": "cluster", "id": cluster_id, "label": name }),
                json!({ "clusterId": cluster_id }),
            )
            .await?;
            Ok(ok)
        }
        Op::SetServiceWaf {
            service_id,
            mode,
            level,
            exceptions,
            threshold,
        } => {
            let (team_id, cluster_id, name) = fw_access(tx, user, &service_id).await?;
            crate::waf::check_settings(&mode, level, &exceptions)?;
            let threshold = threshold.unwrap_or(5);
            crate::waf::check_threshold(threshold)?;
            let mut exceptions = exceptions;
            exceptions.sort_unstable();
            exceptions.dedup();
            let settings = json!({ "mode": mode, "level": level, "exceptions": exceptions, "threshold": if threshold == 5 { 0 } else { threshold } });
            let was: Value =
                sqlx::query_scalar("SELECT waf FROM pyo.services WHERE id = $1 FOR UPDATE")
                    .bind(&service_id)
                    .fetch_one(&mut **tx)
                    .await?;
            sqlx::query("UPDATE pyo.services SET waf = $2 WHERE id = $1")
                .bind(&service_id)
                .bind(&settings)
                .execute(&mut **tx)
                .await?;
            let label = match mode.as_str() {
                "block" => "blocks attacks on",
                "log" => "logs attacks on",
                _ => "is off for",
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "firewall.updated",
                "info",
                &format!("The web application firewall {label} {name}"),
                json!({ "kind": "service", "id": service_id }),
                json!({ "serviceId": service_id, "clusterId": cluster_id }),
            )
            .await?;
            // On, or just turned off: the routes change, and the WAF's engine
            // may start (the first service there) or stop (the last one).
            if crate::waf::is_on(&was) || crate::waf::is_on(&settings) {
                crate::firewall::sync(tx, &cluster_id).await?;
                wake.push(cluster_id);
            }
            Ok(ok)
        }
        Op::AddWafRule {
            id,
            service_id,
            name,
            matching,
            conditions,
            action,
            enabled,
        } => {
            client_id(&id, "wfr")?;
            let (team_id, cluster_id, service_name) = fw_access(tx, user, &service_id).await?;
            if sqlx::query_scalar::<_, i32>("SELECT 1 FROM pyo.waf_rules WHERE id = $1")
                .bind(&id)
                .fetch_optional(&mut **tx)
                .await?
                .is_some()
            {
                return Ok(ok);
            }
            let (name, conditions) = crate::waf::check_rule(&name, &matching, conditions, &action)?;
            let draft = pyo_proto::WafRule {
                id: 0,
                name: name.clone(),
                matching: matching.clone(),
                conditions: conditions.clone(),
                action: action.clone(),
            };
            crate::waf::check_rules(tx, &service_id, Some(&draft), None).await?;
            sqlx::query(
                "INSERT INTO pyo.waf_rules (id, team_id, service_id, name, enabled, position, matching, conditions, action, created_by)
                 VALUES ($1, $2, $3, $4, $5, (SELECT coalesce(max(position) + 1, 0) FROM pyo.waf_rules WHERE service_id = $3), $6, $7, $8, $9)",
            )
            .bind(&id)
            .bind(&team_id)
            .bind(&service_id)
            .bind(&name)
            .bind(enabled)
            .bind(&matching)
            .bind(json!(conditions))
            .bind(&action)
            .bind(&user.name)
            .execute(&mut **tx)
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "firewall.updated",
                "info",
                &format!("Added the WAF rule {name} to {service_name}"),
                json!({ "kind": "service", "id": service_id }),
                json!({ "clusterId": cluster_id, "serviceId": service_id }),
            )
            .await?;
            waf_changed(tx, &service_id, &cluster_id, wake).await?;
            Ok(ok)
        }
        Op::UpdateWafRule {
            id,
            name,
            matching,
            conditions,
            action,
            enabled,
        } => {
            type Stored = (String, String, String, Value, String);
            let row: Option<Stored> = sqlx::query_as("SELECT service_id, name, matching, conditions, action FROM pyo.waf_rules WHERE id = $1").bind(&id).fetch_optional(&mut **tx).await?;
            let (service_id, old_name, old_matching, old_conditions, old_action) =
                row.ok_or_else(|| ApiError::not_found("WAF rule"))?;
            let (team_id, cluster_id, service_name) = fw_access(tx, user, &service_id).await?;
            let matching = matching.unwrap_or(old_matching);
            let action = action.unwrap_or(old_action);
            // Stored conditions are checked again with the rest, so they go
            // through the same door as new ones.
            let conditions = conditions.unwrap_or_else(|| {
                serde_json::from_value::<Vec<pyo_proto::WafCondition>>(old_conditions)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| crate::waf::ConditionInput {
                        field: c.field,
                        name: c.name,
                        op: c.op,
                        value: None,
                        values: c.values,
                        negate: c.negate,
                    })
                    .collect()
            });
            let (name, conditions) = crate::waf::check_rule(
                name.as_deref().unwrap_or(&old_name),
                &matching,
                conditions,
                &action,
            )?;
            let draft = pyo_proto::WafRule {
                id: 0,
                name: name.clone(),
                matching: matching.clone(),
                conditions: conditions.clone(),
                action: action.clone(),
            };
            crate::waf::check_rules(tx, &service_id, Some(&draft), Some(&id)).await?;
            sqlx::query("UPDATE pyo.waf_rules SET name = $2, matching = $3, conditions = $4, action = $5, enabled = coalesce($6, enabled), updated_at = now() WHERE id = $1")
                .bind(&id)
                .bind(&name)
                .bind(&matching)
                .bind(json!(conditions))
                .bind(&action)
                .bind(enabled)
                .execute(&mut **tx)
                .await?;
            let what = match enabled {
                Some(true) => "Turned on",
                Some(false) => "Turned off",
                None => "Changed",
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "firewall.updated",
                "info",
                &format!("{what} the WAF rule {name} of {service_name}"),
                json!({ "kind": "service", "id": service_id }),
                json!({ "clusterId": cluster_id, "serviceId": service_id }),
            )
            .await?;
            waf_changed(tx, &service_id, &cluster_id, wake).await?;
            Ok(ok)
        }
        Op::ReorderWafRules { service_id, ids } => {
            let (_, cluster_id, _) = fw_access(tx, user, &service_id).await?;
            let mut have: Vec<String> = crate::waf::rules(tx, &service_id)
                .await?
                .into_iter()
                .map(|(.., id)| id)
                .collect();
            let mut want = ids.clone();
            have.sort();
            want.sort();
            if have != want {
                return Err(ApiError::conflict(
                    "List every rule of this service once (they may have changed: reload).",
                ));
            }
            for (i, id) in ids.iter().enumerate() {
                sqlx::query("UPDATE pyo.waf_rules SET position = $2 WHERE id = $1")
                    .bind(id)
                    .bind(i as i32)
                    .execute(&mut **tx)
                    .await?;
            }
            waf_changed(tx, &service_id, &cluster_id, wake).await?;
            Ok(ok)
        }
        Op::RemoveWafRule { id } => {
            let row: Option<(String, String)> =
                sqlx::query_as("SELECT service_id, name FROM pyo.waf_rules WHERE id = $1")
                    .bind(&id)
                    .fetch_optional(&mut **tx)
                    .await?;
            let Some((service_id, name)) = row else {
                return Ok(ok);
            };
            let (team_id, cluster_id, service_name) = fw_access(tx, user, &service_id).await?;
            sqlx::query("DELETE FROM pyo.waf_rules WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "firewall.updated",
                "info",
                &format!("Removed the WAF rule {name} from {service_name}"),
                json!({ "kind": "service", "id": service_id }),
                json!({ "clusterId": cluster_id, "serviceId": service_id }),
            )
            .await?;
            waf_changed(tx, &service_id, &cluster_id, wake).await?;
            Ok(ok)
        }
        Op::AddFirewallRule {
            id,
            service_id,
            action,
            operator,
            sources,
            requests,
            per,
            note,
        } => {
            client_id(&id, "fwr")?;
            let (team_id, cluster_id, name) = fw_access(tx, user, &service_id).await?;
            if sqlx::query_scalar::<_, i32>("SELECT 1 FROM pyo.firewall_rules WHERE id = $1")
                .bind(&id)
                .fetch_optional(&mut **tx)
                .await?
                .is_some()
            {
                return Ok(ok);
            }
            let rule = crate::firewall::check_rule(
                &action,
                &operator,
                &sources,
                requests,
                per.as_deref(),
            )?;
            if note.chars().count() > 200 {
                return Err(ApiError::bad_request("Keep the note under 200 characters."));
            }
            crate::firewall::check_count(tx, &service_id, &rule, None).await?;
            sqlx::query(
                "INSERT INTO pyo.firewall_rules (id, team_id, service_id, position, action, operator, sources, requests, per, note, created_by)
                 VALUES ($1, $2, $3, (SELECT coalesce(max(position) + 1, 0) FROM pyo.firewall_rules WHERE service_id = $3), $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(&id)
            .bind(&team_id)
            .bind(&service_id)
            .bind(&rule.action)
            .bind(&rule.operator)
            .bind(&rule.sources)
            .bind(rule.limit.as_ref().map(|l| l.requests as i32))
            .bind(rule.limit.as_ref().map(|l| l.per.as_str()))
            .bind(note.trim())
            .bind(&user.name)
            .execute(&mut **tx)
            .await?;
            let what = crate::firewall::describe(&rule);
            log_activity(
                tx,
                &team_id,
                &user.name,
                "firewall.rule_added",
                "info",
                &format!("{what} on {name}"),
                json!({ "kind": "service", "id": service_id }),
                json!({ "clusterId": cluster_id, "serviceId": service_id }),
            )
            .await?;
            crate::firewall::sync(tx, &cluster_id).await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::UpdateFirewallRule {
            id,
            action,
            operator,
            sources,
            requests,
            per,
            note,
        } => {
            let service_id: String =
                sqlx::query_scalar("SELECT service_id FROM pyo.firewall_rules WHERE id = $1")
                    .bind(&id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or_else(|| ApiError::not_found("Firewall rule"))?;
            let (team_id, cluster_id, name) = fw_access(tx, user, &service_id).await?;
            if note.as_ref().is_some_and(|n| n.chars().count() > 200) {
                return Err(ApiError::bad_request("Keep the note under 200 characters."));
            }
            let old = crate::firewall::for_service(tx, &service_id)
                .await?
                .rules
                .into_iter()
                .find(|r| r.id == id)
                .ok_or_else(|| ApiError::not_found("Firewall rule"))?;
            let action = action.unwrap_or_else(|| old.action.clone());
            let rule = crate::firewall::check_rule(
                &action,
                operator.as_deref().unwrap_or(&old.operator),
                sources.as_deref().unwrap_or(&old.sources),
                requests.or(old.limit.as_ref().map(|l| l.requests)),
                per.as_deref()
                    .or(old.limit.as_ref().map(|l| l.per.as_str())),
            )?;
            crate::firewall::check_count(tx, &service_id, &rule, Some(&id)).await?;
            sqlx::query("UPDATE pyo.firewall_rules SET action = $2, operator = $3, sources = $4, requests = $5, per = $6, note = coalesce($7, note) WHERE id = $1")
                .bind(&id)
                .bind(&rule.action)
                .bind(&rule.operator)
                .bind(&rule.sources)
                .bind(rule.limit.as_ref().map(|l| l.requests as i32))
                .bind(rule.limit.as_ref().map(|l| l.per.as_str()))
                .bind(note.as_deref().map(str::trim))
                .execute(&mut **tx)
                .await?;
            let changed = rule
                != pyo_proto::firewall::FirewallRule {
                    id: String::new(),
                    ..old
                };
            if changed {
                let what = crate::firewall::describe(&rule);
                log_activity(
                    tx,
                    &team_id,
                    &user.name,
                    "firewall.updated",
                    "info",
                    &format!("Changed a firewall rule on {name}: {what}"),
                    json!({ "kind": "service", "id": service_id }),
                    json!({ "clusterId": cluster_id, "serviceId": service_id }),
                )
                .await?;
                crate::firewall::sync(tx, &cluster_id).await?;
                wake.push(cluster_id);
            }
            Ok(ok)
        }
        Op::ReorderFirewallRules { service_id, ids } => {
            let (team_id, cluster_id, name) = fw_access(tx, user, &service_id).await?;
            let rules = crate::firewall::for_service(tx, &service_id).await?.rules;
            let mut have: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
            let mut want: Vec<&str> = ids.iter().map(String::as_str).collect();
            have.sort();
            want.sort();
            if have != want {
                return Err(ApiError::conflict(
                    "List every rule of this service once (they may have changed: reload).",
                ));
            }
            for (i, id) in ids.iter().enumerate() {
                sqlx::query("UPDATE pyo.firewall_rules SET position = $2 WHERE id = $1")
                    .bind(id)
                    .bind(i as i32)
                    .execute(&mut **tx)
                    .await?;
            }
            log_activity(
                tx,
                &team_id,
                &user.name,
                "firewall.updated",
                "info",
                &format!("Reordered the firewall rules of {name}"),
                json!({ "kind": "service", "id": service_id }),
                json!({ "clusterId": cluster_id, "serviceId": service_id }),
            )
            .await?;
            crate::firewall::sync(tx, &cluster_id).await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::RemoveFirewallRule { id } => {
            let row: Option<String> =
                sqlx::query_scalar("SELECT service_id FROM pyo.firewall_rules WHERE id = $1")
                    .bind(&id)
                    .fetch_optional(&mut **tx)
                    .await?;
            let Some(service_id) = row else { return Ok(ok) };
            let (team_id, cluster_id, name) = fw_access(tx, user, &service_id).await?;
            let what = crate::firewall::for_service(tx, &service_id)
                .await?
                .rules
                .into_iter()
                .find(|r| r.id == id)
                .map(|r| crate::firewall::describe(&r))
                .unwrap_or_default();
            sqlx::query("DELETE FROM pyo.firewall_rules WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "firewall.rule_removed",
                "info",
                &format!("Removed the firewall rule \"{what}\" from {name}"),
                json!({ "kind": "service", "id": service_id }),
                json!({ "clusterId": cluster_id, "serviceId": service_id }),
            )
            .await?;
            crate::firewall::sync(tx, &cluster_id).await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::SetClusterOriginPull {
            cluster_id,
            certificate_ids,
        } => {
            let team_id = team_of(tx, "clusters", &cluster_id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let (name, status): (String, String) =
                sqlx::query_as("SELECT name, status FROM pyo.clusters WHERE id = $1")
                    .bind(&cluster_id)
                    .fetch_one(&mut **tx)
                    .await?;
            let mut ids = certificate_ids;
            ids.sort();
            ids.dedup();
            let certs: Vec<(String, String, bool)> = sqlx::query_as("SELECT name, ca_pem, not_after <= now() FROM pyo.origin_pull_certificates WHERE team_id = $1 AND id = ANY($2)")
                .bind(&team_id)
                .bind(&ids)
                .fetch_all(&mut **tx)
                .await?;
            if certs.len() != ids.len() {
                return Err(ApiError::not_found("Certificate"));
            }
            if let Some((cert, ..)) = certs.iter().find(|c| c.2) {
                return Err(ApiError::conflict(format!(
                    "{cert} has expired, so nothing could connect with it."
                )));
            }
            // Turning it off can wait for the agent; turning it on shouldn't
            // take effect some time later, when nobody's watching.
            if !ids.is_empty() && status != "connected" && status != "degraded" {
                return Err(ApiError::conflict(format!(
                    "{name}'s agent isn't connected. Try again once it is."
                )));
            }
            let ca_pem = certs
                .iter()
                .map(|c| c.1.trim())
                .collect::<Vec<_>>()
                .join("\n");
            let revision = new_id("opr");
            sqlx::query("UPDATE pyo.clusters SET origin_pull = origin_pull || jsonb_build_object('certificateIds', to_jsonb($2::text[]), 'revision', $3::text, 'state', 'applying', 'error', null) WHERE id = $1")
                .bind(&cluster_id)
                .bind(&ids)
                .bind(&revision)
                .execute(&mut **tx)
                .await?;
            deploy::enqueue(tx, &cluster_id, Command::SetOriginPull { ca_pem, revision }).await?;
            let title = if ids.is_empty() {
                format!("Stopped requiring a CDN certificate on {name}")
            } else {
                format!("Required a CDN certificate on {name}")
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "cluster.updated",
                "warning",
                &title,
                json!({ "kind": "cluster", "id": cluster_id, "label": name }),
                json!({ "clusterId": cluster_id }),
            )
            .await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::ReplaceCertificate {
            id,
            certificate,
            private_key,
        } => {
            let team_id = team_of(tx, "certificates", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let p = crate::certs::parse(&certificate, &private_key, now_ms() / 1000)?;
            let users = crate::certs::used_by(tx, &id).await?;
            if let Some((_, host)) = users
                .iter()
                .find(|(_, h)| !crate::certs::covers(&p.hostnames, h))
            {
                return Err(ApiError::bad_request(format!(
                    "The new certificate isn't valid for {host}, which uses this one."
                )));
            }
            let sealed = s.sealer.seal(&p.key_pem).map_err(|_| {
                ApiError::bad_request("Set PYO_SECRET_KEY on the API to store private keys.")
            })?;
            let name: String = sqlx::query_scalar(
                "UPDATE pyo.certificates SET hostnames = $2, issuer = $3, not_before = to_timestamp($4), not_after = to_timestamp($5), fingerprint = $6,
                        chain_pem = $7, key_sealed = $8, warned_days = NULL, updated_at = now() WHERE id = $1 RETURNING name",
            )
            .bind(&id)
            .bind(&p.hostnames)
            .bind(&p.issuer)
            .bind(p.not_before as f64)
            .bind(p.not_after as f64)
            .bind(&p.fingerprint)
            .bind(&p.chain_pem)
            .bind(&sealed)
            .fetch_one(&mut **tx)
            .await?;
            // Services using it get it in place, without a redeploy (a
            // service never deployed gets it with its first deploy).
            let mut services: Vec<String> = users.into_iter().map(|(a, _)| a).collect();
            services.sort();
            services.dedup();
            let mut updated = 0;
            for service_id in services {
                if let Some(c) = crate::routes::changed(tx, &service_id, true).await? {
                    wake.push(c);
                    updated += 1;
                }
            }
            log_activity(
                tx,
                &team_id,
                &user.name,
                "domain.certificate_replaced",
                "info",
                &format!("Replaced certificate {name}"),
                json!({ "kind": "certificate", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(
                json!({ "ok": true, "hostnames": p.hostnames, "warnings": p.warnings, "updated": updated }),
            )
        }
        Op::RemoveCertificate { id } => {
            let team_id = team_of(tx, "certificates", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let users = crate::certs::used_by(tx, &id).await?;
            if !users.is_empty() {
                let hosts: Vec<String> = users.into_iter().map(|(_, h)| h).collect();
                return Err(ApiError::conflict(format!(
                    "It's in use by {}. Switch those domains to another certificate first.",
                    hosts.join(", ")
                )));
            }
            sqlx::query("DELETE FROM pyo.certificates WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::RevokeOauthGrant { id } => {
            sqlx::query("DELETE FROM pyo.oauth_grants WHERE id = $1 AND user_id = $2")
                .bind(&id)
                .bind(&user.id)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::RevokeApiToken { id } => {
            sqlx::query("DELETE FROM pyo.api_tokens WHERE id = $1 AND user_id = $2")
                .bind(&id)
                .bind(&user.id)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::SendTestEmail {} => {
            require_admin(user)?;
            let cfg = mail::settings(&s.db)
                .await?
                .ok_or_else(|| ApiError::bad_request("Save the email settings first."))?;
            let (subject, text, html) = mail::test(&cfg.link("/"));
            mail::send(s, &cfg, &user.email, &subject, text, html)
                .await
                .map_err(|e| ApiError::bad_request(format!("Couldn't send: {}", smtp_error(&e))))?;
            Ok(json!({ "ok": true, "to": user.email }))
        }
        Op::UpdateMemberRole { id, role } => {
            let team_id = team_of(tx, "members", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            parse_role(&role)?;
            let current: String = sqlx::query_scalar("SELECT role FROM pyo.members WHERE id = $1")
                .bind(&id)
                .fetch_one(&mut **tx)
                .await?;
            if current == "owner" || role == "owner" {
                return Err(ApiError::bad_request(
                    "Ownership can't be changed here yet.",
                ));
            }
            sqlx::query("UPDATE pyo.members SET role = $2 WHERE id = $1")
                .bind(&id)
                .bind(&role)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::RemoveMember { id } => {
            let team_id = team_of(tx, "members", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let role: String = sqlx::query_scalar("SELECT role FROM pyo.members WHERE id = $1")
                .bind(&id)
                .fetch_one(&mut **tx)
                .await?;
            if role == "owner" {
                return Err(ApiError::bad_request("The owner can't be removed."));
            }
            sqlx::query("DELETE FROM pyo.members WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }

        // ── Projects ──
        Op::ImportCompose {
            team_id,
            cluster_id,
            project_id,
            plan,
        } => {
            require_role(tx, user, &team_id, Role::Developer).await?;
            if plan.services.is_empty() || plan.services.len() > 40 {
                return Err(ApiError::bad_request(
                    "A Compose import has 1 to 40 services.",
                ));
            }
            for svc in &plan.services {
                require_dns_label(&svc.name, "Service name")?;
                if svc.source.kind == "git" && svc.source.repo_url.trim().is_empty() {
                    return Err(ApiError::bad_request(format!(
                        "{} is built from source: import from its repository, or give it an image.",
                        svc.name
                    )));
                }
            }
            let cluster_id = match cluster_id.filter(|c| !c.is_empty()) {
                Some(c) => c,
                None => sqlx::query_scalar::<_, Option<String>>(
                    "SELECT default_cluster_id FROM pyo.teams WHERE id = $1",
                )
                .bind(&team_id)
                .fetch_one(&mut **tx)
                .await?
                .ok_or_else(|| {
                    ApiError::bad_request(
                        "This team has no cluster to deploy to. Add a server first.",
                    )
                })?,
            };
            if team_of(tx, "clusters", &cluster_id).await? != team_id {
                return Err(ApiError::not_found("Cluster"));
            }
            // The project: given, or made with a free namespace.
            let project_id = match project_id.filter(|p| !p.is_empty()) {
                Some(p) => {
                    if team_of(tx, "projects", &p).await? != team_id {
                        return Err(ApiError::not_found("Project"));
                    }
                    p
                }
                None => {
                    let base = crate::compose::dns_name(
                        if plan.project.namespace.is_empty() {
                            &plan.project.name
                        } else {
                            &plan.project.namespace
                        },
                        40,
                    );
                    let mut namespace = base.clone();
                    let mut n = 2;
                    loop {
                        let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.projects WHERE team_id = $1 AND namespace = $2)").bind(&team_id).bind(&namespace).fetch_one(&mut **tx).await?;
                        let reserved = namespace.starts_with("kube-")
                            || namespace == "default"
                            || namespace == "pyo-system"
                            || RESERVED_PROJECT_SLUGS.contains(&namespace.as_str());
                        if !taken && !reserved {
                            break;
                        }
                        namespace = format!("{base}-{n}");
                        n += 1;
                    }
                    let id = new_id("prj");
                    let name = if plan.project.name.trim().is_empty() {
                        namespace.clone()
                    } else {
                        plan.project.name.trim().chars().take(60).collect()
                    };
                    create_project(
                        tx,
                        user,
                        &id,
                        &team_id,
                        &name,
                        &namespace,
                        "Imported from Docker Compose",
                    )
                    .await?;
                    id
                }
            };
            // Each service, in the plan's order (dependencies first).
            let now = pyo_proto::now_ms();
            let mut made = vec![];
            for svc in &plan.services {
                let id = new_id("svc");
                let deployment_id = new_id("dpl");
                let service = crate::compose::service_spec(
                    svc,
                    &id,
                    &team_id,
                    &project_id,
                    &cluster_id,
                    now,
                    &new_id,
                );
                let volumes = crate::compose::volume_specs(svc, &new_id);
                create_service(tx, user, service, &deployment_id, volumes, vec![], wake).await?;
                made.push(json!({ "name": svc.name, "id": id, "deploymentId": deployment_id }));
            }
            Ok(json!({ "ok": true, "projectId": project_id, "services": made }))
        }
        Op::CreateProject {
            id,
            team_id,
            name,
            namespace,
            description,
        } => {
            create_project(tx, user, &id, &team_id, &name, &namespace, &description).await?;
            Ok(ok)
        }
        Op::UpdateProject {
            id,
            name,
            description,
        } => {
            let team_id = team_of(tx, "projects", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            if let Some(n) = name {
                let n = require_text(&n, "Project name", 60)?;
                sqlx::query("UPDATE pyo.projects SET name = $2, updated_at = now() WHERE id = $1")
                    .bind(&id)
                    .bind(n)
                    .execute(&mut **tx)
                    .await?;
            }
            if let Some(d) = description {
                sqlx::query(
                    "UPDATE pyo.projects SET description = $2, updated_at = now() WHERE id = $1",
                )
                .bind(&id)
                .bind(d.trim())
                .execute(&mut **tx)
                .await?;
            }
            Ok(ok)
        }
        Op::SetProjectIsolation { id, isolate } => {
            let team_id = team_of(tx, "projects", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let (name, namespace): (String, String) = sqlx::query_as("UPDATE pyo.projects SET network_isolation = $2, updated_at = now() WHERE id = $1 RETURNING name, namespace")
                .bind(&id)
                .bind(isolate)
                .fetch_one(&mut **tx)
                .await?;
            // Every cluster its services run on.
            let clusters: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT cluster_id FROM pyo.services WHERE project_id = $1",
            )
            .bind(&id)
            .fetch_all(&mut **tx)
            .await?;
            for c in &clusters {
                deploy::enqueue(
                    tx,
                    c,
                    Command::SetIsolation {
                        namespace: namespace.clone(),
                        isolate,
                    },
                )
                .await?;
            }
            let title = if isolate {
                format!("Isolated {name}'s network")
            } else {
                format!("Opened {name}'s network to other projects")
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "project.network",
                "info",
                &title,
                json!({ "kind": "project", "id": id, "label": name }),
                json!({ "projectId": id }),
            )
            .await?;
            Ok(ok)
        }
        Op::RotateDataKey { team_id } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            let resealed = crate::vault::get().rotate(&team_id).await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "security.key_rotated",
                "info",
                "Rotated the team's encryption key",
                json!({ "kind": "team", "id": team_id, "label": "Team" }),
                json!({}),
            )
            .await?;
            Ok(json!({ "ok": true, "resealed": resealed }))
        }
        Op::DeleteProject { id, delete_volumes } => {
            let team_id = team_of(tx, "projects", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let (namespace, name): (String, String) =
                sqlx::query_as("SELECT namespace, name FROM pyo.projects WHERE id = $1")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            // Its services go first, each on its cluster (the agent runs a
            // namespace's deletions in order: services, volumes, namespace).
            let services: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, cluster_id FROM pyo.services WHERE project_id = $1 ORDER BY id",
            )
            .bind(&id)
            .fetch_all(&mut **tx)
            .await?;
            // Their deploys that haven't reached the agent are withdrawn.
            sqlx::query(
                "UPDATE pyo.commands SET status = 'failed', done_at = now(), result = '{\"ok\":false,\"message\":\"Cancelled\",\"logs\":[]}'
                  WHERE status = 'pending' AND command->>'kind' = 'deploy'
                    AND command->>'deploymentId' IN (SELECT d.id FROM pyo.deployments d JOIN pyo.services a ON a.id = d.service_id WHERE a.project_id = $1)",
            )
            .bind(&id)
            .execute(&mut **tx)
            .await?;
            for (service_id, cluster_id) in &services {
                let service = service_ref(tx, service_id).await?;
                deploy::enqueue(tx, cluster_id, Command::DeleteService(service)).await?;
                wake.push(cluster_id.clone());
            }
            // Its volumes: deleted (each on its cluster), or kept.
            let volumes =
                crate::volumes::project_deleted(tx, &id, &namespace, delete_volumes).await?;
            // The rows go now: its services' with it, their volumes detached.
            sqlx::query("DELETE FROM pyo.projects WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            // Its namespace goes too, on each of the team's clusters that has
            // it (the agent deletes only this project's, and keeps one that
            // still has volumes; the last of them deleted lets it go).
            let clusters: Vec<String> = sqlx::query_scalar("SELECT id FROM pyo.clusters WHERE team_id = $1 AND last_seen_at IS NOT NULL ORDER BY id").bind(&team_id).fetch_all(&mut **tx).await?;
            for c in clusters {
                deploy::enqueue(
                    tx,
                    &c,
                    Command::DeleteNamespace {
                        namespace: namespace.clone(),
                        project_id: id.clone(),
                    },
                )
                .await?;
                wake.push(c);
            }
            let title = match (volumes.len(), delete_volumes) {
                (0, _) if !services.is_empty() => {
                    format!("Deleted project {name} and its services")
                }
                (0, _) => format!("Deleted project {name}"),
                (_, true) => format!("Deleted project {name} and its volumes"),
                (_, false) => format!(
                    "Deleted project {name}; kept its volumes {}",
                    volumes.join(", ")
                ),
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "project.deleted",
                "warning",
                &title,
                json!({ "kind": "project", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(
                json!({ "ok": true, "services": services.len(), "volumes": volumes, "volumesDeleted": delete_volumes }),
            )
        }

        // ── Services ──
        Op::CreateService {
            service,
            deployment_id,
            volumes,
            databases,
        } => {
            create_service(tx, user, service, &deployment_id, volumes, databases, wake).await?;
            Ok(ok)
        }
        Op::SaveService {
            service,
            deployment_id,
            trigger,
        } => {
            let id = service["id"].as_str().unwrap_or("").to_owned();
            let team_id = team_of(tx, "services", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let (old_name, current, old_spec, pending): (String, Option<String>, Value, bool) =
                sqlx::query_as("SELECT name, current_deployment_id, spec, pending_changes FROM pyo.services WHERE id = $1").bind(&id).fetch_one(&mut **tx).await?;
            // The dashboard never gets secret values back, so a secret it
            // sends without one keeps the stored (sealed) value.
            let mut service = service;
            let unhidden = keep_secret_values(&mut service, &old_spec);
            if !unhidden.is_empty() {
                // Same as revealing it, for the whole team: admins, logged.
                if require_role(tx, user, &team_id, Role::Admin).await.is_err() {
                    return Err(ApiError::forbidden(format!(
                        "Only admins can make {} visible. Enter a new value to make it plain.",
                        unhidden.join(", ")
                    )));
                }
                let project_id: String =
                    sqlx::query_scalar("SELECT project_id FROM pyo.services WHERE id = $1")
                        .bind(&id)
                        .fetch_one(&mut **tx)
                        .await?;
                for key in &unhidden {
                    log_activity(
                        tx,
                        &team_id,
                        &user.name,
                        "secret.revealed",
                        "warning",
                        &format!("Made {key} of {old_name} visible"),
                        json!({ "kind": "service", "id": id, "label": old_name }),
                        json!({ "projectId": project_id, "serviceId": id }),
                    )
                    .await?;
                }
            }
            let mut spec = service_spec_from(&service);
            // Where it came from stays, and a secret field's value never does.
            crate::template_source::keep_record(&mut spec, &old_spec);
            crate::template_source::scrub_fields(&mut spec);
            // Autoscaling has its own op: a save from a tab that hasn't seen
            // the latest setting can't change it, or the size it manages.
            match old_spec.get("autoscale") {
                Some(a) => {
                    spec["autoscale"] = a.clone();
                    spec["replicas"] = old_spec["replicas"].clone();
                }
                None => {
                    if let Some(o) = spec.as_object_mut() {
                        o.remove("autoscale");
                    }
                }
            }
            // Cron jobs have their own ops too.
            match old_spec.get("cronJobs") {
                Some(jobs) => spec["cronJobs"] = jobs.clone(),
                None => {
                    if let Some(o) = spec.as_object_mut() {
                        o.remove("cronJobs");
                    }
                }
            }
            validate_service_spec(&spec)?;
            // Only a change of GPUs is checked against the cluster: a save
            // of anything else shouldn't fail because a server is down.
            if spec["resources"]["gpu"] != old_spec["resources"]["gpu"] {
                let cluster_id: String =
                    sqlx::query_scalar("SELECT cluster_id FROM pyo.services WHERE id = $1")
                        .bind(&id)
                        .fetch_one(&mut **tx)
                        .await?;
                gpu_ok(tx, &cluster_id, &spec).await?;
            }
            github_source_ok(tx, &team_id, &spec).await?;
            crate::certs::domains_ok(tx, &team_id, &spec).await?;
            domains_unique(tx, &team_id, &id, &spec).await?;
            let cluster_id: String =
                sqlx::query_scalar("SELECT cluster_id FROM pyo.services WHERE id = $1")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            ports_unique(tx, &team_id, &id, &cluster_id, &spec).await?;
            let name = spec["name"].as_str().unwrap_or("").to_owned();
            let name = name.as_str();
            if name != old_name && current.is_some() {
                return Err(ApiError::bad_request(
                    "A deployed service can't be renamed yet, because its name is in use on the cluster.",
                ));
            }
            // Settings that don't change what runs need no redeploy, and nor
            // do domains: they go live in place (routes.rs), and a port's
            // exposure "http" only says a domain uses it.
            let rollout = |v: &Value| {
                let mut v = v.clone();
                if let Some(o) = v.as_object_mut() {
                    o.remove("description");
                    o.remove("autoRollback");
                    o.remove("domains");
                    if let Some(src) = o.get_mut("source").and_then(|x| x.as_object_mut()) {
                        src.remove("autoDeploy");
                    }
                    for p in o
                        .get_mut("ports")
                        .and_then(Value::as_array_mut)
                        .into_iter()
                        .flatten()
                    {
                        if p["exposure"] == json!("http") {
                            p["exposure"] = json!("internal");
                        }
                    }
                }
                v
            };
            let domains_changed = crate::routes::differ(&spec["domains"], &old_spec["domains"]);
            // Compared in the clear: sealing the same value twice differs.
            let (mut a, mut b) = (spec.clone(), old_spec.clone());
            crate::vault::get().open_env(&mut a).await?;
            crate::vault::get().open_env(&mut b).await?;
            let changed = rollout(&a) != rollout(&b);
            crate::vault::get().seal_env(&team_id, &mut spec).await?;
            sqlx::query("UPDATE pyo.services SET spec = $2, name = $3, pending_changes = $4, updated_at = now() WHERE id = $1")
                .bind(&id)
                .bind(&spec)
                .bind(name)
                .bind(deployment_id.is_none() && (pending || changed))
                .execute(&mut **tx)
                .await?;
            // New domains go live now; with a deploy, they go out with it.
            if domains_changed
                && let Some(c) = crate::routes::changed(tx, &id, deployment_id.is_none()).await?
            {
                wake.push(c);
            }
            if let Some(dep) = deployment_id {
                client_id(&dep, "dpl")?;
                let trigger = trigger.unwrap_or_else(|| "config_change".into());
                let c = deploy::create(
                    tx,
                    DeployRequest {
                        deployment_id: &dep,
                        service_id: &id,
                        trigger: &trigger,
                        actor: &user.name,
                        rollback_of: None,
                        retry_of: None,
                        commit: None,
                    },
                )
                .await?;
                wake.push(c);
            }
            Ok(ok)
        }
        Op::DeleteService { id, delete_volumes } => {
            let team_id = team_of(tx, "services", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let service = service_ref(tx, &id).await?;
            // Its volumes: deleted once its objects are gone, or kept, detached.
            let volumes: Vec<String> = if delete_volumes {
                crate::volumes::delete_with_service(tx, user, &team_id, &id).await?
            } else {
                sqlx::query_scalar("SELECT data->>'name' FROM pyo.volumes WHERE service_id = $1 ORDER BY data->>'name'").bind(&id).fetch_all(&mut **tx).await?
            };
            let (cluster_id, name, project_id): (String, String, String) = sqlx::query_as(
                "SELECT cluster_id, name, project_id FROM pyo.services WHERE id = $1",
            )
            .bind(&id)
            .fetch_one(&mut **tx)
            .await?;
            // The agent removes the objects; the row goes when it reports back.
            sqlx::query("UPDATE pyo.services SET deleting = true WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            sqlx::query("UPDATE pyo.deployments SET status = 'cancelled', finished_at = now() WHERE service_id = $1 AND status NOT IN ('succeeded', 'failed', 'cancelled')")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            deploy::enqueue(tx, &cluster_id, Command::DeleteService(service)).await?;
            // The last service there with its WAF on: the WAF's engine goes too.
            let waf: Value = sqlx::query_scalar("SELECT waf FROM pyo.services WHERE id = $1")
                .bind(&id)
                .fetch_one(&mut **tx)
                .await?;
            if crate::waf::is_on(&waf) && !crate::waf::wanted(tx, &cluster_id).await? {
                crate::firewall::sync(tx, &cluster_id).await?;
            }
            let title = match (volumes.len(), delete_volumes) {
                (0, _) => format!("Deleted {name}"),
                (_, true) => format!("Deleted {name} and its volumes"),
                (_, false) => format!("Deleted {name}; kept its volumes {}", volumes.join(", ")),
            };
            log_activity(
                tx,
                &team_id,
                &user.name,
                "service.deleted",
                "warning",
                &title,
                json!({ "kind": "service", "id": id, "label": name }),
                json!({ "projectId": project_id }),
            )
            .await?;
            wake.push(cluster_id);
            Ok(json!({ "ok": true, "volumes": volumes, "volumesDeleted": delete_volumes }))
        }
        Op::Deploy {
            service_id,
            deployment_id,
            trigger,
            rollback_of,
            retry_of,
        } => {
            client_id(&deployment_id, "dpl")?;
            let team_id = team_of(tx, "services", &service_id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let trigger = match trigger.as_str() {
                "manual" | "redeploy" | "rollback" | "retry" | "config_change" => trigger,
                _ => "manual".into(),
            };
            let c = deploy::create(
                tx,
                DeployRequest {
                    deployment_id: &deployment_id,
                    service_id: &service_id,
                    trigger: &trigger,
                    actor: &user.name,
                    rollback_of: rollback_of.as_deref(),
                    retry_of: retry_of.as_deref(),
                    commit: None,
                },
            )
            .await?;
            wake.push(c);
            Ok(ok)
        }
        Op::Cancel { deployment_id } => {
            let team_id = team_of(tx, "deployments", &deployment_id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let (status, cluster_id): (String, String) =
                sqlx::query_as("SELECT status, cluster_id FROM pyo.deployments WHERE id = $1")
                    .bind(&deployment_id)
                    .fetch_one(&mut **tx)
                    .await?;
            if deploy::is_final(&status) {
                return Err(ApiError::conflict("This deployment already finished."));
            }
            deploy::enqueue(
                tx,
                &cluster_id,
                Command::Cancel {
                    deployment_id: deployment_id.clone(),
                },
            )
            .await?;
            // A build that hasn't finished is dropped; its result is ignored.
            sqlx::query("UPDATE pyo.build_jobs SET status = 'cancelled', finished_at = now() WHERE deployment_id = $1 AND status IN ('queued', 'running')")
                .bind(&deployment_id)
                .execute(&mut **tx)
                .await?;
            // Queued work the agent hasn't picked up yet is withdrawn here.
            sqlx::query("UPDATE pyo.commands SET status = 'failed', done_at = now(), result = '{\"ok\":false,\"message\":\"Cancelled\",\"logs\":[]}' WHERE status = 'pending' AND command->>'kind' = 'deploy' AND command->>'deploymentId' = $1")
                .bind(&deployment_id)
                .execute(&mut **tx)
                .await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::RestartService { id } => service_command(tx, user, &id, wake, true).await,
        Op::StopService { id } => service_command(tx, user, &id, wake, false).await,
        Op::AddVolume {
            id,
            service_id,
            name,
            mount_path,
            size_gi,
        } => {
            new_volume_id(tx, &id).await?;
            let team_id = team_of(tx, "services", &service_id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            crate::volumes::target_ok(tx, &service_id, true).await?;
            let (service_name, project_id, cluster_id, namespace): (String, String, String, String) =
                sqlx::query_as("SELECT a.name, a.project_id, a.cluster_id, p.namespace FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id WHERE a.id = $1").bind(&service_id).fetch_one(&mut **tx).await?;
            // Named after the service, so two services of a project can't collide.
            let label = slugify(&name);
            if label.is_empty() {
                return Err(ApiError::bad_request(
                    "Name the volume, e.g. data or uploads.",
                ));
            }
            let full = format!("{service_name}-{label}");
            require_dns_label(&full, "Volume name")?;
            let path = crate::volumes::mount_path(&mount_path)?;
            if !(1..=1000).contains(&size_gi) {
                return Err(ApiError::bad_request("Pick a size between 1 and 1000 GB."));
            }
            // A name in use: a volume's, one being deleted, or a claim on the
            // cluster Pyo has no volume for (a new volume would take its data).
            let clash: Option<String> = sqlx::query_scalar(
                "SELECT CASE WHEN data->>'name' = $2 AND deleting THEN 'deleting' WHEN data->>'name' = $2 THEN 'name' ELSE 'path' END FROM pyo.volumes
                  WHERE (project_id = $1 AND data->>'name' = $2) OR (service_id = $3 AND data->>'mountPath' = $4)
                 UNION ALL
                 SELECT 'stray' FROM pyo.clusters c, jsonb_array_elements(c.stray_volumes) e WHERE c.id = $5 AND e->>'namespace' = $6 AND e->>'name' = $2
                 LIMIT 1",
            )
            .bind(&project_id)
            .bind(&full)
            .bind(&service_id)
            .bind(&path)
            .bind(&cluster_id)
            .bind(&namespace)
            .fetch_optional(&mut **tx)
            .await?;
            match clash.as_deref() {
                Some("name") => {
                    return Err(ApiError::conflict(format!(
                        "This project already has a volume named {full}."
                    )));
                }
                Some("deleting") => {
                    return Err(ApiError::conflict(format!(
                        "{full} is still being deleted. Try again in a minute, or pick another name."
                    )));
                }
                Some("stray") => {
                    return Err(ApiError::conflict(format!(
                        "The cluster has a volume named {full} that isn't in Pyo. Delete it on the cluster's Storage page, or pick another name."
                    )));
                }
                Some(_) => {
                    return Err(ApiError::conflict(format!(
                        "Another volume is already mounted at {path}."
                    )));
                }
                None => {}
            }
            let caps: Value =
                sqlx::query_scalar("SELECT capabilities FROM pyo.clusters WHERE id = $1")
                    .bind(&cluster_id)
                    .fetch_one(&mut **tx)
                    .await?;
            let class = caps["storageClasses"]
                .as_array()
                .and_then(|cs| cs.iter().find(|c| c["isDefault"] == json!(true)))
                .and_then(|c| c["name"].as_str())
                .unwrap_or("local-path")
                .to_owned();
            let data = json!({
                "name": full, "mountPath": path, "sizeGi": size_gi, "storageClass": class, "accessMode": "ReadWriteOnce",
                "status": "pending",
            });
            sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, service_id, data, namespace) VALUES ($1, $2, $3, $4, $5, $6, $7)")
                .bind(&id)
                .bind(&team_id)
                .bind(&cluster_id)
                .bind(&project_id)
                .bind(&service_id)
                .bind(&data)
                .bind(&namespace)
                .execute(&mut **tx)
                .await?;
            sqlx::query(
                "UPDATE pyo.services SET pending_changes = true, updated_at = now() WHERE id = $1",
            )
            .bind(&service_id)
            .execute(&mut **tx)
            .await?;
            log_activity(tx, &team_id, &user.name, "volume.created", "info", &format!("Added volume {full} ({size_gi} GB at {path})"), json!({ "kind": "volume", "id": id, "label": full }), json!({ "projectId": project_id, "serviceId": service_id, "clusterId": cluster_id })).await?;
            Ok(ok)
        }
        Op::DetachVolume { id } => crate::volumes::detach(tx, user, &id).await,
        Op::AttachVolume {
            id,
            service_id,
            mount_path,
        } => crate::volumes::attach(tx, user, &id, &service_id, &mount_path, wake).await,
        Op::RestoreVolume { id, snapshot_id } => {
            crate::volumes::restore(s, tx, user, &id, &snapshot_id, wake).await
        }
        Op::ResizeVolume { id, size_gi } => {
            crate::volumes::resize(tx, user, &id, size_gi, wake).await
        }
        Op::DeleteVolume { id, cluster_id } => {
            crate::volumes::delete(tx, user, &id, cluster_id.as_deref(), wake).await
        }
        Op::ScaleService { id, replicas } => {
            let team_id = team_of(tx, "services", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            if !(1..=50).contains(&replicas) {
                return Err(ApiError::bad_request(
                    "Scale between 1 and 50 replicas. To scale to zero, stop the service.",
                ));
            }
            let (cluster_id, current, name, project_id, status, spec): (String, Option<String>, String, String, String, Value) =
                sqlx::query_as("SELECT cluster_id, current_deployment_id, name, project_id, status, spec FROM pyo.services WHERE id = $1").bind(&id).fetch_one(&mut **tx).await?;
            let Some(current) = current else {
                return Err(ApiError::conflict("Deploy the service before scaling it."));
            };
            crate::volumes::not_restoring(tx, &id).await?;
            if status == "deploying" {
                return Err(ApiError::conflict(
                    "A deploy is running. Scale once it finishes.",
                ));
            }
            // Autoscaled: only starting a stopped service sets a size.
            let replicas =
                match serde_json::from_value::<pyo_proto::Autoscale>(spec["autoscale"].clone()) {
                    Ok(a) if status == "stopped" => replicas.clamp(a.min, a.max),
                    Ok(_) => {
                        return Err(ApiError::conflict(
                            "Autoscaling is on. Turn it off to set the replicas yourself.",
                        ));
                    }
                    Err(_) => replicas,
                };
            // The setting, and the live deployment's spec so a rollback to
            // it (or an automatic one) keeps this size.
            sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{replicas}', to_jsonb($2::int)), updated_at = now() WHERE id = $1").bind(&id).bind(replicas).execute(&mut **tx).await?;
            sqlx::query(
                "UPDATE pyo.deployments SET spec = jsonb_set(spec, '{replicas}', to_jsonb($2::int)),
                        data = jsonb_set(data, '{config,replicas}', to_jsonb($2::int)) WHERE id = $1",
            )
            .bind(&current)
            .bind(replicas)
            .execute(&mut **tx)
            .await?;
            let service = service_ref(tx, &id).await?;
            deploy::enqueue(tx, &cluster_id, Command::Scale { service, replicas }).await?;
            if status == "stopped" {
                sqlx::query("UPDATE pyo.services SET status = 'degraded' WHERE id = $1")
                    .bind(&id)
                    .execute(&mut **tx)
                    .await?;
            }
            log_activity(
                tx,
                &team_id,
                &user.name,
                "service.scaled",
                "info",
                &format!("Scaled {name} to {replicas}"),
                json!({ "kind": "service", "id": id, "label": name }),
                json!({ "projectId": project_id, "serviceId": id, "clusterId": cluster_id }),
            )
            .await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::SaveCronJob {
            service_id,
            id,
            name,
            schedule,
            command,
            paused,
        } => {
            let name = require_text(&name, "Name", 60)?;
            let schedule = schedule.split_whitespace().collect::<Vec<_>>().join(" ");
            check_schedule(&schedule)?;
            let command = command.trim().to_owned();
            if command.is_empty() || command.len() > 4096 {
                return Err(ApiError::bad_request(
                    "Enter the command to run, like npm run cleanup.",
                ));
            }
            let id = id.unwrap_or_else(|| new_id("crn"));
            let title = format!("Saved cron job {name}");
            update_cron_jobs(tx, user, &service_id, wake, &title, |jobs| {
                let task = pyo_proto::CronTask {
                    id: id.clone(),
                    name,
                    schedule,
                    command,
                    paused,
                };
                match jobs.iter().position(|j| j.id == id) {
                    Some(i) => jobs[i] = task,
                    None if jobs.len() >= 20 => {
                        return Err(ApiError::bad_request(
                            "A service can have up to 20 cron jobs.",
                        ));
                    }
                    None => jobs.push(task),
                }
                Ok(())
            })
            .await?;
            Ok(json!({ "ok": true, "id": id }))
        }
        Op::RemoveCronJob { service_id, id } => {
            update_cron_jobs(tx, user, &service_id, wake, "Removed a cron job", |jobs| {
                let before = jobs.len();
                jobs.retain(|j| j.id != id);
                if jobs.len() == before {
                    Err(ApiError::not_found("Cron job"))
                } else {
                    Ok(())
                }
            })
            .await?;
            Ok(ok)
        }
        Op::RunCronJob { service_id, id } => {
            let team_id = team_of(tx, "services", &service_id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let (cluster_id, spec, name): (String, Value, String) =
                sqlx::query_as("SELECT cluster_id, spec, name FROM pyo.services WHERE id = $1")
                    .bind(&service_id)
                    .fetch_one(&mut **tx)
                    .await?;
            let jobs: Vec<pyo_proto::CronTask> =
                serde_json::from_value(spec["cronJobs"].clone()).unwrap_or_default();
            let job = jobs
                .iter()
                .find(|j| j.id == id)
                .ok_or_else(|| ApiError::not_found("Cron job"))?;
            let service = service_ref(tx, &service_id).await?;
            deploy::enqueue(
                tx,
                &cluster_id,
                Command::RunCronJob {
                    service,
                    id: id.clone(),
                },
            )
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "service.restarted",
                "info",
                &format!("Ran {} on {name}", job.name),
                json!({ "kind": "service", "id": service_id, "label": name }),
                json!({ "serviceId": service_id }),
            )
            .await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::SetAutoscale { id, autoscale } => {
            let team_id = team_of(tx, "services", &id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let (cluster_id, current, name, project_id, status, spec, runtime): (String, Option<String>, String, String, String, Value, Value) =
                sqlx::query_as("SELECT cluster_id, current_deployment_id, name, project_id, status, spec, runtime FROM pyo.services WHERE id = $1").bind(&id).fetch_one(&mut **tx).await?;
            let Some(current) = current else {
                return Err(ApiError::conflict(
                    "Deploy the service before autoscaling it.",
                ));
            };
            if status == "deploying" {
                return Err(ApiError::conflict(
                    "A deploy is running. Change autoscaling once it finishes.",
                ));
            }
            let service = service_ref(tx, &id).await?;
            if let Some(a) = &autoscale {
                check_autoscale(a)?;
                let single_writer: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.volumes WHERE service_id = $1 AND data->>'accessMode' = 'ReadWriteOnce')").bind(&id).fetch_one(&mut **tx).await?;
                if service.workload_kind == "CronJob" {
                    return Err(ApiError::conflict(
                        "A cron job runs once per schedule, so there's nothing to autoscale.",
                    ));
                }
                if spec["resources"]["gpu"].as_u64().is_some_and(|g| g > 0) {
                    return Err(ApiError::conflict(
                        "Services with GPUs can't autoscale yet: each new replica would need a spare GPU.",
                    ));
                }
                if service.workload_kind == "StatefulSet" || single_writer {
                    return Err(ApiError::conflict(
                        "Services with a single-writer volume can't autoscale: they can only run one replica safely.",
                    ));
                }
                if spec["resources"]["cpuRequest"]
                    .as_i64()
                    .is_some_and(|c| c < 1)
                {
                    return Err(ApiError::conflict(
                        "Set a CPU request first: autoscaling measures use against it.",
                    ));
                }
            }
            // Turned off, it stays the size it reached.
            let stopped = status == "stopped";
            let reached = runtime["readyReplicas"].as_i64().unwrap_or(0).clamp(1, 50) as i32;
            let saved = match &autoscale {
                Some(a) => a.min,
                None if stopped => spec["replicas"].as_i64().unwrap_or(1) as i32,
                None => reached,
            };
            let mut change = json!({ "replicas": saved });
            if let Some(a) = &autoscale {
                change["autoscale"] = json!(a);
            }
            sqlx::query("UPDATE pyo.services SET spec = (spec - 'autoscale') || $2, updated_at = now() WHERE id = $1").bind(&id).bind(&change).execute(&mut **tx).await?;
            sqlx::query("UPDATE pyo.deployments SET spec = (spec - 'autoscale') || $2, data = jsonb_set(data, '{config,replicas}', $2->'replicas') WHERE id = $1")
                .bind(&current)
                .bind(&change)
                .execute(&mut **tx)
                .await?;
            let title = match &autoscale {
                Some(a) => format!(
                    "Autoscaling {name} between {} and {} replicas",
                    a.min, a.max
                ),
                None => format!("Stopped autoscaling {name}"),
            };
            deploy::enqueue(
                tx,
                &cluster_id,
                Command::Autoscale {
                    service,
                    autoscale,
                    replicas: if stopped { 0 } else { saved },
                },
            )
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "service.scaled",
                "info",
                &title,
                json!({ "kind": "service", "id": id, "label": name }),
                json!({ "projectId": project_id, "serviceId": id, "clusterId": cluster_id }),
            )
            .await?;
            wake.push(cluster_id);
            Ok(ok)
        }
        Op::CreateDatabase {
            id,
            team_id,
            cluster_id,
            name,
            engine,
            size,
            instances,
            version,
            storage_gi,
            pooler,
            rest,
            cpu_millis,
            memory_mi,
            read_replicas,
            extensions,
        } => {
            let r = crate::databases::create(
                s,
                tx,
                user,
                crate::databases::Create {
                    id,
                    team_id,
                    cluster_id,
                    name,
                    engine,
                    size,
                    instances,
                    version,
                    storage_gi,
                    pooler,
                    rest,
                    restore: None,
                    restored_from: None,
                    cpu_millis,
                    memory_mi,
                    read_replicas,
                    extensions: extensions.unwrap_or_default(),
                },
            )
            .await?;
            wake.extend(r["clusterId"].as_str().map(str::to_owned));
            Ok(r)
        }
        Op::UpdateDatabase {
            id,
            size,
            instances,
            storage_gi,
            version,
            pooler,
            rest,
            deletion_protection,
            read_replicas,
            cpu_millis,
            memory_mi,
        } => {
            let r = crate::databases::update(
                s,
                tx,
                user,
                crate::databases::Update {
                    id,
                    size,
                    instances,
                    storage_gi,
                    version,
                    pooler,
                    rest,
                    deletion_protection,
                    read_replicas,
                    cpu_millis,
                    memory_mi,
                },
            )
            .await?;
            wake.extend(r["clusterId"].as_str().map(str::to_owned));
            Ok(r)
        }
        Op::SetDatabaseExtensions { id, extensions } => {
            let r = crate::databases::set_extensions(s, tx, user, &id, extensions).await?;
            wake.extend(r["clusterId"].as_str().map(str::to_owned));
            Ok(r)
        }
        Op::DeleteDatabase { id } => {
            let r = crate::databases::delete(tx, user, &id).await?;
            wake.extend(r["clusterId"].as_str().map(str::to_owned));
            Ok(r)
        }
        Op::ConnectDatabase {
            id,
            service_id,
            env,
            prefix,
        } => {
            let env = match prefix.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                Some(_) if env.as_ref().is_some_and(|e| !e.is_empty()) => {
                    return Err(ApiError::bad_request(
                        "Give either a prefix or the variables' names, not both.",
                    ));
                }
                Some(p) => Some(crate::databases::prefixed(tx, &id, p).await?),
                None => env,
            };
            crate::databases::connect(tx, user, &id, &service_id, env).await
        }
        Op::DisconnectDatabase { id, service_id } => {
            crate::databases::disconnect(tx, user, &id, &service_id).await
        }
        Op::RevealDatabasePassword { id } => crate::databases::reveal(tx, user, &id).await,
        Op::SetDatabaseBackups {
            id,
            enabled,
            retention_days,
            hour,
            schedule,
            keep,
        } => {
            let r = crate::databases::set_backups(
                s,
                tx,
                user,
                &id,
                crate::databases::BackupInput {
                    enabled,
                    retention_days,
                    hour,
                    schedule,
                    keep,
                },
            )
            .await?;
            wake.extend(r["clusterId"].as_str().map(str::to_owned));
            Ok(r)
        }
        Op::RestoreDatabase {
            id,
            source_id,
            name,
            target_time,
        } => {
            let r =
                crate::databases::restore(s, tx, user, id, &source_id, name, target_time).await?;
            wake.extend(r["clusterId"].as_str().map(str::to_owned));
            Ok(r)
        }
        Op::RevealSecret { service_id, env_id } => {
            let team_id = team_of(tx, "services", &service_id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let (name, project_id, spec): (String, String, Value) =
                sqlx::query_as("SELECT name, project_id, spec FROM pyo.services WHERE id = $1")
                    .bind(&service_id)
                    .fetch_one(&mut **tx)
                    .await?;
            let e = spec["env"]
                .as_array()
                .and_then(|es| es.iter().find(|e| e["id"] == json!(env_id)))
                .ok_or_else(|| ApiError::not_found("Variable"))?;
            let key = e["key"].as_str().unwrap_or("").to_owned();
            let value = crate::vault::get()
                .open(e["value"].as_str().unwrap_or(""))
                .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "secret.revealed",
                "warning",
                &format!("Revealed {key} of {name}"),
                json!({ "kind": "service", "id": service_id, "label": name }),
                json!({ "projectId": project_id, "serviceId": service_id }),
            )
            .await?;
            Ok(json!({ "ok": true, "value": value }))
        }
        Op::VerifyDomain {
            service_id,
            domain_id,
        } => {
            let team_id = team_of(tx, "services", &service_id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
                .bind(&service_id)
                .fetch_one(&mut **tx)
                .await?;
            let host = spec["domains"]
                .as_array()
                .and_then(|ds| ds.iter().find(|d| d["id"] == json!(domain_id)))
                .and_then(|d| d["hostname"].as_str())
                .ok_or_else(|| ApiError::not_found("Domain"))?
                .to_owned();
            // The DNS watcher does this on its own; this is the same check, now.
            let resolved = deploy::resolve(&host).await;
            deploy::set_domain_dns(&s.db, &service_id, &domain_id, !resolved.is_empty(), &host)
                .await?;
            let message = resolved
                .is_empty()
                .then(|| format!("{host} doesn't resolve yet. Point it at the cluster's ingress."));
            Ok(json!({ "ok": message.is_none(), "message": message, "addresses": resolved }))
        }

        // ── Clusters and servers ──
        Op::CreateCluster {
            id,
            team_id,
            name,
            description,
            location,
            tags,
            this_machine,
            public_api_url,
        } => {
            let tags = valid_tags(&tags)?;
            // One agent per machine: a second "This machine" would replace the first.
            if this_machine {
                let taken: Option<String> = sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE same_cluster OR from_host_setup LIMIT 1").fetch_optional(&mut **tx).await?;
                match taken {
                    Some(t) if t == team_id => {
                        return Err(ApiError::conflict(
                            "This machine is already connected in this team.",
                        ));
                    }
                    Some(_) => {
                        return Err(ApiError::conflict(
                            "This machine is already connected to another team. Add another machine instead.",
                        ));
                    }
                    None => {}
                }
            }
            client_id(&id, "cl")?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            // A new cluster is a server about to connect.
            crate::check_add_server(tx, &team_id).await?;
            // No name: a placeholder until the agent reports its server's
            // hostname (agent.rs heartbeat).
            let from_host = name.trim().is_empty();
            let name = if from_host {
                let tail: String = crate::ids::secret_token()
                    .chars()
                    .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    .take(6)
                    .collect();
                format!("cluster-{tail}")
            } else {
                require_dns_label(&name, "Cluster name")?;
                reserved_cluster_name(&name)?;
                name
            };
            let token = secret_token();
            // Detected per server from its public address; a cluster-wide one is optional.
            let location = if location.trim().is_empty() {
                String::new()
            } else {
                require_location(&location)?
            };
            sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, description, location, agent_token_hash, tags, name_from_host) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
                .bind(&id)
                .bind(&team_id)
                .bind(&name)
                .bind(description.trim())
                .bind(&location)
                .bind(sha256_hex(&token))
                .bind(&tags)
                .bind(from_host)
                .execute(&mut **tx)
                .await?;
            sqlx::query("UPDATE pyo.teams SET default_cluster_id = coalesce(default_cluster_id, $2) WHERE id = $1").bind(&team_id).bind(&id).execute(&mut **tx).await?;
            // Run on this machine, the installer reaches Pyo at localhost,
            // so the agent is given the in-cluster address (install.rs).
            let (command, tailscale, tailnet) = if this_machine {
                (
                    crate::join::connect_command(
                        &crate::join::local_scripts_url(),
                        &crate::join::local_api_url(),
                        &token,
                        None,
                        None,
                    ),
                    true,
                    true,
                )
            } else {
                connect_command(tx, s, &team_id, &id, &token, public_api_url.as_deref()).await?
            };
            Ok(
                json!({ "ok": true, "token": token, "command": command, "tailscale": tailscale, "tailnet": tailnet }),
            )
        }
        Op::RotateAgentToken {
            id,
            this_machine,
            public_api_url,
        } => {
            let team_id = team_of(tx, "clusters", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let token = secret_token();
            sqlx::query("UPDATE pyo.clusters SET agent_token_hash = $2 WHERE id = $1")
                .bind(&id)
                .bind(sha256_hex(&token))
                .execute(&mut **tx)
                .await?;
            // As in CreateCluster: run where Pyo runs, the agent dials the in-cluster address.
            let (command, tailscale, tailnet) = if this_machine {
                (
                    crate::join::connect_command(
                        &crate::join::local_scripts_url(),
                        &crate::join::local_api_url(),
                        &token,
                        None,
                        None,
                    ),
                    true,
                    true,
                )
            } else {
                connect_command(tx, s, &team_id, &id, &token, public_api_url.as_deref()).await?
            };
            Ok(
                json!({ "ok": true, "token": token, "command": command, "tailscale": tailscale, "tailnet": tailnet }),
            )
        }
        Op::UpdateCluster {
            id,
            name,
            description,
            location,
            tags,
        } => {
            let team_id = team_of(tx, "clusters", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            if let Some(n) = name {
                if n.trim().is_empty() {
                    // Back to "use the server's hostname", for a cluster that hasn't connected.
                    sqlx::query("UPDATE pyo.clusters SET name_from_host = true WHERE id = $1 AND status = 'pending'").bind(&id).execute(&mut **tx).await?;
                } else {
                    require_dns_label(&n, "Cluster name")?;
                    reserved_cluster_name(&n)?;
                    sqlx::query(
                        "UPDATE pyo.clusters SET name = $2, name_from_host = false WHERE id = $1",
                    )
                    .bind(&id)
                    .bind(n)
                    .execute(&mut **tx)
                    .await?;
                }
            }
            if let Some(d) = description {
                sqlx::query("UPDATE pyo.clusters SET description = $2 WHERE id = $1")
                    .bind(&id)
                    .bind(d.trim())
                    .execute(&mut **tx)
                    .await?;
            }
            if let Some(l) = location {
                let l = if l.trim().is_empty() {
                    String::new()
                } else {
                    require_location(&l)?
                };
                sqlx::query("UPDATE pyo.clusters SET location = $2 WHERE id = $1")
                    .bind(&id)
                    .bind(&l)
                    .execute(&mut **tx)
                    .await?;
            }
            if let Some(t) = tags {
                sqlx::query("UPDATE pyo.clusters SET tags = $2 WHERE id = $1")
                    .bind(&id)
                    .bind(valid_tags(&t)?)
                    .execute(&mut **tx)
                    .await?;
            }
            Ok(ok)
        }
        Op::SetServerTags { server_id, tags } => {
            let (cluster_id, team_id, name): (String, String, String) =
                sqlx::query_as("SELECT n.cluster_id, c.team_id, n.name FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE n.id = $1")
                    .bind(&server_id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or_else(|| ApiError::not_found("Server"))?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            save_server_tags(tx, &cluster_id, &name, &valid_tags(&tags)?).await?;
            // Labelled on the cluster's next heartbeat; running services aren't moved.
            Ok(ok)
        }
        Op::SetServerRegularServices { server_id, on } => {
            let (cluster_id, team_id, name): (String, String, String) =
                sqlx::query_as("SELECT n.cluster_id, c.team_id, n.name FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE n.id = $1")
                    .bind(&server_id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or_else(|| ApiError::not_found("Server"))?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            sqlx::query("INSERT INTO pyo.server_tags (cluster_id, name, regular_services) VALUES ($1, $2, $3) ON CONFLICT (cluster_id, name) DO UPDATE SET regular_services = $3")
                .bind(&cluster_id)
                .bind(&name)
                .bind(on)
                .execute(&mut **tx)
                .await?;
            // Tainted or untainted on the cluster's next heartbeat; pods already there stay.
            Ok(ok)
        }
        Op::DiscardClusterDraft { id } => {
            let Some(team_id): Option<String> =
                sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
                    .bind(&id)
                    .fetch_optional(&mut **tx)
                    .await?
            else {
                return Ok(ok);
            };
            require_role(tx, user, &team_id, Role::Admin).await?;
            let gone = sqlx::query(
                "DELETE FROM pyo.clusters c WHERE c.id = $1 AND c.status = 'pending'
                   AND NOT EXISTS (SELECT 1 FROM pyo.services a WHERE a.cluster_id = c.id)
                   AND NOT EXISTS (SELECT 1 FROM pyo.builders b WHERE b.cluster_id = c.id)",
            )
            .bind(&id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if gone > 0 {
                sqlx::query(
                    "UPDATE pyo.teams SET default_cluster_id = NULL WHERE default_cluster_id = $1",
                )
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            }
            Ok(ok)
        }
        Op::RemoveCluster { id, force } => remove_cluster(tx, user, &id, force).await,
        Op::SetServerSchedulable {
            server_id,
            schedulable,
        } => {
            node_command(tx, user, &server_id, wake, |server| {
                Command::SetSchedulable {
                    server,
                    schedulable,
                }
            })
            .await
        }
        Op::JoinServer {
            cluster_id,
            server_name,
            tags,
        } => {
            let team_id = team_of(tx, "clusters", &cluster_id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            require_dns_label(&server_name, "Server name")?;
            // Its page is /clusters/<cluster>/<server>, beside the cluster's tabs.
            if ["servers", "workloads", "networking", "storage", "settings"]
                .contains(&server_name.as_str())
            {
                return Err(ApiError::bad_request(
                    "That name is used by a cluster page. Pick another.",
                ));
            }
            let tags = valid_tags(&tags)?;
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pyo.servers WHERE cluster_id = $1 AND name = $2)",
            )
            .bind(&cluster_id)
            .bind(&server_name)
            .fetch_one(&mut **tx)
            .await?;
            if taken {
                return Err(ApiError::conflict(format!(
                    "{server_name} is already a server in this cluster."
                )));
            }
            save_server_tags(tx, &cluster_id, &server_name, &tags).await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "server.join",
                "info",
                &format!("Made a join command for {server_name}"),
                json!({ "kind": "server", "id": server_name, "label": server_name }),
                json!({ "clusterId": cluster_id }),
            )
            .await?;
            join_command(tx, s, &team_id, &cluster_id, &server_name, false, wake).await
        }
        Op::SetServerLocation {
            server_id,
            location,
        } => {
            let (team_id,): (String,) = sqlx::query_as("SELECT c.team_id FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE n.id = $1")
                .bind(&server_id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or_else(|| ApiError::not_found("Server"))?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            if location.trim().is_empty() {
                sqlx::query(
                    "UPDATE pyo.servers SET location = NULL, location_source = NULL WHERE id = $1",
                )
                .bind(&server_id)
                .execute(&mut **tx)
                .await?;
            } else {
                let l = require_location(&location)?;
                sqlx::query("UPDATE pyo.servers SET location = $2, location_source = 'manual' WHERE id = $1").bind(&server_id).bind(&l).execute(&mut **tx).await?;
            }
            Ok(ok)
        }
        Op::SetServerMaintenance { server_id, on } => {
            node_command(tx, user, &server_id, wake, |server| {
                Command::SetMaintenance { server, on }
            })
            .await
        }
        Op::RemoveServer { server_id, force } => {
            remove_node(tx, user, &server_id, force, wake).await
        }
        Op::CancelServerRemoval { server_id } => {
            let state: Option<String> =
                sqlx::query_scalar("SELECT removal->>'state' FROM pyo.servers WHERE id = $1")
                    .bind(&server_id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .flatten();
            if !matches!(state.as_deref(), Some("drained" | "failed")) {
                return Err(ApiError::bad_request(
                    "Only a removal that's waiting or failed can be cancelled.",
                ));
            }
            let out = node_command(tx, user, &server_id, wake, |server| {
                Command::SetSchedulable {
                    server,
                    schedulable: true,
                }
            })
            .await?;
            sqlx::query("UPDATE pyo.servers SET removal = NULL WHERE id = $1")
                .bind(&server_id)
                .execute(&mut **tx)
                .await?;
            Ok(out)
        }
        Op::CreateBuilder {
            id,
            team_id,
            name,
            kind,
            cluster_id,
            server: node,
        } => {
            client_id(&id, "bld")?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            require_dns_label(&name, "Builder name")?;
            let token = secret_token();
            // The add dialog re-issues the command as you edit it: a builder on
            // a new server whose server hasn't joined yet is replaced.
            let mut replaced = false;
            if kind == "server" {
                replaced = sqlx::query(
                    "DELETE FROM pyo.builders b WHERE b.id = $1 AND b.team_id = $2 AND b.kind = 'server' AND b.cluster_id IS NOT NULL
                       AND NOT EXISTS (SELECT 1 FROM pyo.servers n WHERE n.cluster_id = b.cluster_id AND n.name = b.server)",
                )
                .bind(&id)
                .bind(&team_id)
                .execute(&mut **tx)
                .await?
                .rows_affected()
                    > 0;
            }
            if let (Some(c), Some(n)) = (&cluster_id, &node) {
                one_builder_per_machine(tx, &id, c, n).await?;
            }
            match kind.as_str() {
                "cluster" => {
                    let cluster_id =
                        cluster_id.ok_or_else(|| ApiError::bad_request("Pick a cluster."))?;
                    let node = node.ok_or_else(|| ApiError::bad_request("Pick a server."))?;
                    let ok: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE c.id = $1 AND c.team_id = $2 AND n.name = $3)")
                        .bind(&cluster_id)
                        .bind(&team_id)
                        .bind(&node)
                        .fetch_one(&mut **tx)
                        .await?;
                    if !ok {
                        return Err(ApiError::bad_request(
                            "That server isn't in one of this team's clusters.",
                        ));
                    }
                    crate::builders::add_on_server(
                        tx,
                        &s.config,
                        &id,
                        &team_id,
                        &name,
                        &cluster_id,
                        &node,
                    )
                    .await?;
                    wake.push(cluster_id);
                }
                // A new server joins one of the team's clusters as a
                // build-only server; once it's ready, the builder starts on it
                // like a cluster builder (builders.rs refresh_cluster_builders).
                "server" => {
                    let cluster_id = cluster_id
                        .ok_or_else(|| ApiError::bad_request("Pick the cluster it joins."))?;
                    let node = node.ok_or_else(|| ApiError::bad_request("Name the server."))?;
                    require_dns_label(&node, "Server name")?;
                    let cluster_team: Option<String> =
                        sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
                            .bind(&cluster_id)
                            .fetch_optional(&mut **tx)
                            .await?;
                    if cluster_team.as_deref() != Some(team_id.as_str()) {
                        return Err(ApiError::bad_request("Pick one of this team's clusters."));
                    }
                    let taken: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.servers WHERE cluster_id = $1 AND name = $2)").bind(&cluster_id).bind(&node).fetch_one(&mut **tx).await?;
                    if taken {
                        return Err(ApiError::conflict(format!(
                            "{node} is already a server in this cluster."
                        )));
                    }
                    let join =
                        join_command(tx, s, &team_id, &cluster_id, &node, true, wake).await?;
                    let cluster_name: String =
                        sqlx::query_scalar("SELECT name FROM pyo.clusters WHERE id = $1")
                            .bind(&cluster_id)
                            .fetch_one(&mut **tx)
                            .await?;
                    sqlx::query(
                        "INSERT INTO pyo.builders (id, team_id, name, kind, cluster_id, server, token_hash, info, status_message)
                         VALUES ($1, $2, $3, 'server', $4, $5, $6, jsonb_build_object('specVersion', 0), $7)",
                    )
                    .bind(&id)
                    .bind(&team_id)
                    .bind(&name)
                    .bind(&cluster_id)
                    .bind(&node)
                    .bind(sha256_hex(&token))
                    .bind(format!("Waiting for {node} to join {cluster_name}."))
                    .execute(&mut **tx)
                    .await?;
                    if replaced {
                        return Ok(join);
                    }
                    log_activity(
                        tx,
                        &team_id,
                        &user.name,
                        "team.updated",
                        "info",
                        &format!("Added builder {name}"),
                        json!({ "kind": "team", "id": team_id, "label": name }),
                        json!({}),
                    )
                    .await?;
                    return Ok(join);
                }
                _ => return Err(ApiError::bad_request("Unknown builder kind.")),
            }
            log_activity(
                tx,
                &team_id,
                &user.name,
                "team.updated",
                "info",
                &format!("Added builder {name}"),
                json!({ "kind": "team", "id": team_id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::RemoveBuilder { id } => {
            let (team_id, kind, cluster_id, name): (String, String, Option<String>, String) =
                sqlx::query_as(
                    "SELECT team_id, kind, cluster_id, name FROM pyo.builders WHERE id = $1",
                )
                .bind(&id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or_else(|| ApiError::not_found("Builder"))?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            if kind == "server" && cluster_id.is_none() {
                // Set up with Docker before servers joined clusters: its
                // containers stay until you remove them.
                sqlx::query("DELETE FROM pyo.builders WHERE id = $1")
                    .bind(&id)
                    .execute(&mut **tx)
                    .await?;
            } else {
                // The builder pod goes; a build-only server stays in its
                // cluster until it's removed there.
                if let Some(c) = cluster_id {
                    deploy::enqueue(
                        tx,
                        &c,
                        Command::RemoveBuilder {
                            builder_id: id.clone(),
                        },
                    )
                    .await?;
                    wake.push(c);
                }
                sqlx::query("DELETE FROM pyo.builders WHERE id = $1")
                    .bind(&id)
                    .execute(&mut **tx)
                    .await?;
            }
            log_activity(
                tx,
                &team_id,
                &user.name,
                "team.updated",
                "warning",
                &format!("Removed builder {name}"),
                json!({ "kind": "team", "id": team_id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::RotateBuilderToken { id } => {
            let (team_id, kind, cluster_id, node): (
                String,
                String,
                Option<String>,
                Option<String>,
            ) = sqlx::query_as(
                "SELECT team_id, kind, cluster_id, server FROM pyo.builders WHERE id = $1",
            )
            .bind(&id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| ApiError::not_found("Builder"))?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            if kind != "server" {
                return Err(ApiError::bad_request(
                    "Only builders on a new server have an install command.",
                ));
            }
            // A server that joins a cluster: a fresh join command (they expire).
            if let (Some(c), Some(n)) = (cluster_id, node) {
                let joined: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pyo.servers WHERE cluster_id = $1 AND name = $2)",
                )
                .bind(&c)
                .bind(&n)
                .fetch_one(&mut **tx)
                .await?;
                if joined {
                    return Err(ApiError::bad_request(format!(
                        "{n} has already joined the cluster."
                    )));
                }
                return join_command(tx, s, &team_id, &c, &n, true, wake).await;
            }
            // Set up with Docker, before builders joined clusters.
            Err(ApiError::bad_request(
                "This builder was set up with Docker. Add a new one with \"New server\": it joins a cluster as a build-only server.",
            ))
        }
        Op::SaveRegistryLogin {
            id,
            team_id,
            host,
            username,
            password,
        } => {
            client_id(&id, "rgl")?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let host = host
                .trim()
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_lowercase();
            let host = crate::registry::image_host(&format!("{host}/x"));
            let username = require_text(&username, "Username", 200)?;
            if password.trim().is_empty() {
                return Err(ApiError::bad_request("Enter the password or access token."));
            }
            if !s.sealer.enabled() {
                return Err(ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "This installation has no PYO_SECRET_KEY, so it can't store logins.",
                ));
            }
            let verified = crate::registry::verify(&host, &username, password.trim())
                .await
                .map_err(ApiError::bad_request)?;
            let sealed = s.sealer.seal(password.trim()).map_err(ApiError::from)?;
            sqlx::query(
                "INSERT INTO pyo.registry_logins (id, team_id, host, username, secret_sealed, verified) VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (team_id, host) DO UPDATE SET username = $4, secret_sealed = $5, verified = $6, created_at = now()",
            )
            .bind(&id)
            .bind(&team_id)
            .bind(&host)
            .bind(&username)
            .bind(&sealed)
            .bind(verified)
            .execute(&mut **tx)
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "integration.registry",
                "info",
                &format!("Saved a login for {host}"),
                json!({ "kind": "integration", "id": id, "label": host }),
                json!({}),
            )
            .await?;
            Ok(json!({ "ok": true, "host": host, "verified": verified }))
        }
        Op::RemoveRegistryLogin { id } => {
            let team_id: String =
                sqlx::query_scalar("SELECT team_id FROM pyo.registry_logins WHERE id = $1")
                    .bind(&id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or_else(|| ApiError::not_found("Registry login"))?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            sqlx::query("DELETE FROM pyo.registry_logins WHERE id = $1")
                .bind(&id)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::CreateRegistryToken {
            id,
            team_id,
            name,
            scope,
            expires_in_days,
        } => {
            client_id(&id, "rtk")?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let name = require_text(&name, "Token name", 60)?;
            let scope = scope.unwrap_or_else(|| "push".into());
            if scope != "push" && scope != "pull" {
                return Err(ApiError::bad_request(
                    "A token's scope is \"push\" (push and pull) or \"pull\".",
                ));
            }
            if expires_in_days.is_some_and(|d| d != 30 && d != 90) {
                return Err(ApiError::bad_request(
                    "A token expires in 30 or 90 days, or never.",
                ));
            }
            let token = format!("pyr_{}", secret_token());
            sqlx::query(
                "INSERT INTO pyo.registry_tokens (id, team_id, name, token_hash, scope, created_by, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(days => $7))",
            )
            .bind(&id)
            .bind(&team_id)
            .bind(&name)
            .bind(sha256_hex(&token))
            .bind(&scope)
            .bind(&user.name)
            .bind(expires_in_days.map(|d| d as i32))
            .execute(&mut **tx)
            .await?;
            let slug: String = sqlx::query_scalar("SELECT slug FROM pyo.teams WHERE id = $1")
                .bind(&team_id)
                .fetch_one(&mut **tx)
                .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "integration.registry",
                "info",
                &format!("Created a Pyo registry token, {name}"),
                json!({ "kind": "integration", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(
                json!({ "ok": true, "token": token, "username": slug, "address": crate::registrydoor::address(), "path": crate::registrydoor::team_path(&slug) }),
            )
        }
        Op::RevokeRegistryToken { id } => {
            let team_id: String =
                sqlx::query_scalar("SELECT team_id FROM pyo.registry_tokens WHERE id = $1")
                    .bind(&id)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or_else(|| ApiError::not_found("Registry token"))?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let name: String =
                sqlx::query_scalar("DELETE FROM pyo.registry_tokens WHERE id = $1 RETURNING name")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "integration.registry",
                "info",
                &format!("Revoked the Pyo registry token {name}"),
                json!({ "kind": "integration", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::SaveSecretStore {
            id,
            team_id,
            kind,
            name,
            addr,
            options,
            credential,
        } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            if !matches!(
                kind.as_str(),
                "vault" | "openbao" | "infisical" | "aws" | "gcp" | "azure" | "doppler"
            ) {
                return Err(ApiError::bad_request("Choose a supported secret store."));
            }
            let name = require_text(&name, "Name", 60)?;
            let fixed = crate::secretstores::default_address(&kind, &options)?;
            let addr = if addr.trim().is_empty() {
                fixed.unwrap_or_default()
            } else {
                addr
            };
            let addr = crate::secretstores::check_address(&addr)
                .await?
                .as_str()
                .trim_end_matches('/')
                .to_owned();
            let saved = match &id {
                Some(id) => Some(crate::secretstores::load(tx, &team_id, id).await?),
                None => None,
            };
            let credential = match credential.filter(|c| !c.trim().is_empty()) {
                Some(c) => c.trim().to_owned(),
                None => saved
                    .map(|s| s.credential)
                    .ok_or_else(|| ApiError::bad_request("Enter the store's credential."))?,
            };
            let store = crate::secretstores::Store {
                name: name.clone(),
                kind: kind.clone(),
                addr: addr.clone(),
                options: options.clone(),
                credential,
            };
            crate::secretstores::check_connection(&store).await?;
            let sealed = crate::vault::get()
                .seal(&team_id, &store.credential)
                .await?;
            if !crate::vault::Vault::is_sealed(&sealed) {
                return Err(ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "This installation has no master key, so it can't store credentials.",
                ));
            }
            let id = id.unwrap_or_else(|| new_id("sst"));
            sqlx::query(
                "INSERT INTO pyo.secret_stores (id, team_id, kind, name, addr, options, credential) VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (id) DO UPDATE SET name = excluded.name, addr = excluded.addr, options = excluded.options, credential = excluded.credential",
            )
            .bind(&id)
            .bind(&team_id)
            .bind(&kind)
            .bind(&name)
            .bind(&addr)
            .bind(&options)
            .bind(&sealed)
            .execute(&mut **tx)
            .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "integration.connected",
                "info",
                &format!("Connected secret store {name}"),
                json!({ "kind": "integration", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(json!({ "ok": true, "id": id }))
        }
        Op::RemoveSecretStore { id } => {
            let team_id = team_of(tx, "secret_stores", &id).await?;
            require_role(tx, user, &team_id, Role::Admin).await?;
            let used: Option<String> = sqlx::query_scalar("SELECT a.name FROM pyo.services a, jsonb_array_elements(a.spec->'secretImports') i WHERE a.team_id = $1 AND jsonb_typeof(a.spec->'secretImports') = 'array' AND i->>'storeId' = $2 AND NOT a.deleting LIMIT 1")
                .bind(&team_id)
                .bind(&id)
                .fetch_optional(&mut **tx)
                .await?;
            if let Some(service) = used {
                return Err(ApiError::conflict(format!(
                    "{service} imports from this store. Remove the import first."
                )));
            }
            let name: String =
                sqlx::query_scalar("DELETE FROM pyo.secret_stores WHERE id = $1 RETURNING name")
                    .bind(&id)
                    .fetch_one(&mut **tx)
                    .await?;
            log_activity(
                tx,
                &team_id,
                &user.name,
                "integration.removed",
                "info",
                &format!("Removed secret store {name}"),
                json!({ "kind": "integration", "id": id, "label": name }),
                json!({}),
            )
            .await?;
            Ok(ok)
        }
        Op::CheckSecretImport { store_id, path } => {
            let team_id = team_of(tx, "secret_stores", &store_id).await?;
            require_role(tx, user, &team_id, Role::Developer).await?;
            let store = crate::secretstores::load(tx, &team_id, &store_id).await?;
            let mut keys: Vec<String> = crate::secretstores::read(&store, &path)
                .await?
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| crate::secretstores::env_key(k))
                .collect();
            keys.sort();
            Ok(json!({ "ok": true, "keys": keys }))
        }
        Op::SaveIntegration {
            team_id,
            kind,
            secret,
        } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            let secret = secret.trim().to_owned();
            let valid = match kind.as_str() {
                "tailscale" => secret.starts_with("tskey-") && secret.len() > 20,
                _ => return Err(ApiError::bad_request("Unknown integration.")),
            };
            if !valid {
                return Err(ApiError::bad_request(
                    "A Tailscale auth key starts with tskey-.",
                ));
            }
            if !s.sealer.enabled() {
                return Err(ApiError::new(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "This installation has no PYO_SECRET_KEY, so it can't store keys.",
                ));
            }
            let hint = format!("…{}", &secret[secret.len() - 4..]);
            sqlx::query(
                "INSERT INTO pyo.integrations (team_id, kind, secret, hint) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (team_id, kind) DO UPDATE SET secret = excluded.secret, hint = excluded.hint, created_at = now()",
            )
            .bind(&team_id)
            .bind(&kind)
            .bind(s.sealer.seal(&secret)?)
            .bind(&hint)
            .execute(&mut **tx)
            .await?;
            Ok(ok)
        }
        Op::RemoveIntegration { team_id, kind } => {
            require_role(tx, user, &team_id, Role::Admin).await?;
            sqlx::query("DELETE FROM pyo.integrations WHERE team_id = $1 AND kind = $2")
                .bind(&team_id)
                .bind(&kind)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
        Op::UpdateProfile { name, timezone } => {
            let name = require_text(&name, "Name", 80)?;
            let tz = require_text(&timezone, "Time zone", 64)?;
            sqlx::query("UPDATE pyo.users SET name = $2, timezone = $3 WHERE id = $1")
                .bind(&user.id)
                .bind(name)
                .bind(tz)
                .execute(&mut **tx)
                .await?;
            Ok(ok)
        }
    }
}

pub async fn service_ref(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
) -> ApiResult<ServiceRef> {
    let (id, name, namespace, spec): (String, String, String, Value) =
        sqlx::query_as("SELECT a.id, a.name, p.namespace, a.spec FROM pyo.services a JOIN pyo.projects p ON p.id = a.project_id WHERE a.id = $1")
            .bind(service_id)
            .fetch_one(&mut **tx)
            .await?;
    let kind = match spec["workloadKind"].as_str() {
        Some("StatefulSet") => "StatefulSet",
        Some("CronJob") => "CronJob",
        _ => "Deployment",
    };
    Ok(ServiceRef {
        service_id: id,
        name,
        namespace,
        workload_kind: kind.into(),
    })
}

/// Change a service's cron jobs and send the whole list to its cluster.
async fn update_cron_jobs(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    service_id: &str,
    wake: &mut Vec<String>,
    title: &str,
    change: impl FnOnce(&mut Vec<pyo_proto::CronTask>) -> ApiResult<()>,
) -> ApiResult<()> {
    let team_id = team_of(tx, "services", service_id).await?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    let (cluster_id, current, name, spec): (String, Option<String>, String, Value) =
        sqlx::query_as("SELECT cluster_id, current_deployment_id, name, spec FROM pyo.services WHERE id = $1 FOR UPDATE").bind(service_id).fetch_one(&mut **tx).await?;
    if spec["workloadKind"] == json!("CronJob") {
        return Err(ApiError::conflict(format!(
            "{name} is a cron job already. Change its schedule in its settings."
        )));
    }
    if current.is_none() {
        return Err(ApiError::conflict(format!(
            "Deploy {name} first: its cron jobs run with its image."
        )));
    }
    // Its runs would need the volume while the service holds it.
    let single_writer: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pyo.volumes WHERE service_id = $1 AND data->>'accessMode' = 'ReadWriteOnce')").bind(service_id).fetch_one(&mut **tx).await?;
    if single_writer {
        return Err(ApiError::conflict(format!(
            "{name} has a volume only one server can use, so cron jobs can't run alongside it."
        )));
    }
    let mut jobs: Vec<pyo_proto::CronTask> =
        serde_json::from_value(spec["cronJobs"].clone()).unwrap_or_default();
    change(&mut jobs)?;
    let value = json!(jobs);
    sqlx::query("UPDATE pyo.services SET spec = jsonb_set(spec, '{cronJobs}', $2), updated_at = now() WHERE id = $1").bind(service_id).bind(&value).execute(&mut **tx).await?;
    if let Some(current) = &current {
        sqlx::query(
            "UPDATE pyo.deployments SET spec = jsonb_set(spec, '{cronJobs}', $2) WHERE id = $1",
        )
        .bind(current)
        .bind(&value)
        .execute(&mut **tx)
        .await?;
    }
    let service = service_ref(tx, service_id).await?;
    deploy::enqueue(tx, &cluster_id, Command::SyncCronJobs { service, jobs }).await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "service.updated",
        "info",
        &format!("{title} on {name}"),
        json!({ "kind": "service", "id": service_id, "label": name }),
        json!({ "serviceId": service_id }),
    )
    .await?;
    wake.push(cluster_id);
    Ok(())
}

async fn service_command(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    wake: &mut Vec<String>,
    restart: bool,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "services", id).await?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    let (cluster_id, current, name, project_id): (String, Option<String>, String, String) =
        sqlx::query_as("SELECT cluster_id, current_deployment_id, name, project_id FROM pyo.services WHERE id = $1").bind(id).fetch_one(&mut **tx).await?;
    if current.is_none() {
        return Err(ApiError::conflict("This service hasn't been deployed yet."));
    }
    crate::volumes::not_restoring(tx, id).await?;
    let service = service_ref(tx, id).await?;
    let cron = service.workload_kind == "CronJob";
    let (cmd, kind, title, status) = if restart {
        (
            Command::Restart(service),
            "service.restarted",
            if cron {
                format!("Ran {name}")
            } else {
                format!("Restarted {name}")
            },
            "running",
        )
    } else {
        (
            Command::Stop(service),
            "service.stopped",
            format!("Stopped {name}"),
            "stopped",
        )
    };
    deploy::enqueue(tx, &cluster_id, cmd).await?;
    if !restart {
        sqlx::query("UPDATE pyo.services SET status = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(status)
            .execute(&mut **tx)
            .await?;
    }
    log_activity(
        tx,
        &team_id,
        &user.name,
        kind,
        "info",
        &title,
        json!({ "kind": "service", "id": id, "label": name }),
        json!({ "projectId": project_id, "serviceId": id, "clusterId": cluster_id }),
    )
    .await?;
    wake.push(cluster_id);
    Ok(json!({ "ok": true }))
}

async fn node_command(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    server_id: &str,
    wake: &mut Vec<String>,
    make: impl FnOnce(String) -> Command,
) -> ApiResult<Value> {
    let (cluster_id, team_id, name): (String, String, String) =
        sqlx::query_as("SELECT n.cluster_id, c.team_id, n.name FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE n.id = $1")
            .bind(server_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| ApiError::not_found("Server"))?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    deploy::enqueue(tx, &cluster_id, make(name)).await?;
    wake.push(cluster_id);
    Ok(json!({ "ok": true }))
}

/// Takes a cluster out of Pyo. Normally only one without services; `force`
/// also takes one that has been unreachable for a while, with its services
/// (their deployments and the rest go with them) — nothing is sent to the
/// cluster, so whatever still runs there is left alone. Its databases,
/// volumes, pending commands, rules and builders go with the cluster's row.
async fn remove_cluster(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    force: bool,
) -> ApiResult<Value> {
    let team_id = team_of(tx, "clusters", id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    // Reachable: heard from in the last 10 minutes (its status turns
    // unreachable after one; this rides out restarts and short outages).
    let (name, reachable): (String, bool) = sqlx::query_as("SELECT name, coalesce(last_seen_at > now() - interval '10 minutes', false) FROM pyo.clusters WHERE id = $1 FOR UPDATE")
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    if force && reachable {
        return Err(ApiError::conflict(
            "This cluster is reachable. Delete or move its services, then remove it.",
        ));
    }
    let services: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE cluster_id = $1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    if services > 0 && !force {
        return Err(ApiError::conflict(
            "Delete or move the services on this cluster first.",
        ));
    }
    // Backups that were waiting on it won't run; database backups set up on
    // it can't run anymore.
    sqlx::query("UPDATE pyo.backups SET status = 'failed', error = 'The cluster was removed.', finished_at = now() WHERE cluster_id = $1 AND status IN ('queued', 'running')")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM pyo.backup_databases WHERE cluster_id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM pyo.services WHERE cluster_id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE pyo.teams SET default_cluster_id = NULL WHERE default_cluster_id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("DELETE FROM pyo.clusters WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    if force {
        log_activity(
            tx,
            &team_id,
            &user.name,
            "cluster.removed",
            "warning",
            &format!("Removed unreachable cluster {name} and what Pyo had on it"),
            json!({ "kind": "cluster", "id": id, "label": name }),
            json!({}),
        )
        .await?;
    }
    Ok(json!({ "ok": true }))
}

/// Detached volumes whose data is on this server's disk, by name.
async fn detached_volumes_on(
    tx: &mut Transaction<'_, Postgres>,
    cluster_id: &str,
    server: &str,
) -> ApiResult<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT data->>'name' FROM pyo.volumes WHERE cluster_id = $1 AND service_id IS NULL AND NOT deleting AND data->>'server' = $2 ORDER BY data->>'name'")
        .bind(cluster_id)
        .bind(server)
        .fetch_all(&mut **tx)
        .await?)
}

/// Start taking a server out of its cluster. Refused for the control plane,
/// the last server, and (unless forced) an offline server or one running
/// services or detached volumes that keep data on its disk or are pinned to its location alone.
/// Builders on it are removed with it.
async fn remove_node(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    server_id: &str,
    force: bool,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let (cluster_id, team_id, name, report, location, removal): (String, String, String, Value, String, Option<Value>) = sqlx::query_as(
        "SELECT n.cluster_id, c.team_id, n.name, n.report, coalesce(n.location, c.location), n.removal
           FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE n.id = $1",
    )
    .bind(server_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("Server"))?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    if removal
        .as_ref()
        .is_some_and(|r| matches!(r["state"].as_str(), Some("draining" | "removing")))
    {
        return Err(ApiError::conflict(format!(
            "{name} is already being removed."
        )));
    }
    if report["roles"]
        .as_array()
        .is_some_and(|r| r.iter().any(|x| x == "control-plane" || x == "master"))
    {
        return Err(ApiError::bad_request(format!(
            "{name} runs the cluster itself (control plane), so it can't be removed. Delete the cluster instead."
        )));
    }
    let others: Vec<(String, Value)> = sqlx::query_as("SELECT coalesce(n.location, c.location), n.report FROM pyo.servers n JOIN pyo.clusters c ON c.id = n.cluster_id WHERE n.cluster_id = $1 AND n.id <> $2 AND n.removal IS NULL")
        .bind(&cluster_id)
        .bind(server_id)
        .fetch_all(&mut **tx)
        .await?;
    if others.is_empty() {
        return Err(ApiError::bad_request(format!(
            "{name} is the cluster's only server. Delete the cluster instead."
        )));
    }
    let online = report["status"] == json!("ready");
    if !force {
        if !online {
            return Err(ApiError::bad_request(format!(
                "{name} is offline, so its services can't be moved off it gracefully. Remove it by force instead."
            )));
        }
        // Running services that keep data on its disk can't move.
        let stateful: Vec<String> = sqlx::query_scalar(
            "SELECT a.name FROM pyo.services a
              WHERE a.cluster_id = $1 AND EXISTS (SELECT 1 FROM pyo.volumes v WHERE v.service_id = a.id)
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(coalesce(a.runtime->'replicas', '[]')) r WHERE r->>'server' = $2)
              ORDER BY a.name",
        )
        .bind(&cluster_id)
        .bind(&name)
        .fetch_all(&mut **tx)
        .await?;
        if !stateful.is_empty() {
            return Err(ApiError::bad_request(format!(
                "Move or delete these services first. Their volumes keep data on {name}'s disk: {}.",
                stateful.join(", ")
            )));
        }
        // So do detached volumes on a one-server storage class (`server`).
        let detached = detached_volumes_on(tx, &cluster_id, &name).await?;
        if !detached.is_empty() {
            return Err(ApiError::bad_request(format!(
                "Delete these volumes first. They keep data on {name}'s disk: {}.",
                detached.join(", ")
            )));
        }
        // Services pinned to a place only this server is in would have nowhere to run.
        let pinned: Vec<(String, String)> = sqlx::query_as("SELECT name, spec->>'location' FROM pyo.services WHERE cluster_id = $1 AND coalesce(spec->>'location', '') <> ''")
            .bind(&cluster_id)
            .fetch_all(&mut **tx)
            .await?;
        let in_place = |pin: &str, loc: &str| match (
            pyo_proto::location_labels(pin),
            pyo_proto::location_labels(loc),
        ) {
            (Some((cc, zone)), Some((ncc, nzone))) => {
                cc == ncc && (zone.is_none() || zone == nzone)
            }
            _ => false,
        };
        let stranded: Vec<String> = pinned
            .into_iter()
            .filter(|(_, pin)| {
                in_place(pin, &location)
                    && !others
                        .iter()
                        .any(|(loc, r)| r["status"] == json!("ready") && in_place(pin, loc))
            })
            .map(|(n, _)| n)
            .collect();
        if !stranded.is_empty() {
            return Err(ApiError::bad_request(format!(
                "These services are pinned to {}, and {name} is the only server there: {}. Change their location first.",
                location,
                stranded.join(", ")
            )));
        }
    }
    // Builders pinned to it go with it.
    let builders: Vec<String> = sqlx::query_scalar("DELETE FROM pyo.builders WHERE kind = 'cluster' AND cluster_id = $1 AND server = $2 RETURNING id").bind(&cluster_id).bind(&name).fetch_all(&mut **tx).await?;
    for builder_id in builders {
        deploy::enqueue(tx, &cluster_id, Command::RemoveBuilder { builder_id }).await?;
    }
    let state = if force { "removing" } else { "draining" };
    sqlx::query("UPDATE pyo.servers SET removal = $2 WHERE id = $1")
        .bind(server_id)
        .bind(json!({ "state": state, "force": force, "by": user.name, "startedAt": pyo_proto::now_ms() }))
        .execute(&mut **tx)
        .await?;
    deploy::enqueue(
        tx,
        &cluster_id,
        Command::RemoveServer {
            server: name.clone(),
            force,
        },
    )
    .await?;
    let title = if force {
        format!("Removing server {name} by force")
    } else {
        format!("Removing server {name}")
    };
    log_activity(
        tx,
        &team_id,
        &user.name,
        "server.removed",
        "warning",
        &title,
        json!({ "kind": "cluster", "id": cluster_id, "label": name }),
        json!({}),
    )
    .await?;
    wake.push(cluster_id);
    Ok(json!({ "ok": true }))
}

/// Whether a site can be shown in a frame.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct Preview {
    frameable: bool,
    /// Why not: "private" (no public domain), "unreachable", "refused" (it
    /// forbids framing) or "error" (it answered with a server error).
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

/// Can the site be previewed
///
/// Whether a service's site can be shown in the dashboard's preview frame:
/// it answers, and doesn't forbid framing (X-Frame-Options, CSP
/// frame-ancestors). Only the service's own first domain is asked, and never
/// a private address.
#[utoipa::path(get, path = "/services/{id}/preview", operation_id = "getServicePreview", tag = "Services",
    params(("id" = String, Path, description = "The service's id (`svc_…`).")),
    responses(
        (status = 200, description = "Whether it can be framed.", body = Preview),
        (status = 404, description = "No such service, or it isn't yours to see.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn service_preview(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Preview>> {
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "services", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
        .bind(&id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    let host = spec["domains"]
        .as_array()
        .and_then(|d| d.first())
        .and_then(|d| d["hostname"].as_str())
        .unwrap_or_default()
        .to_lowercase();
    let no = |reason: &'static str| {
        Ok(Json(Preview {
            frameable: false,
            reason: Some(reason),
        }))
    };
    if host.is_empty() || deploy::private_host(&host) {
        return no("private");
    }
    let ips = deploy::resolve(&host).await;
    fn public_v4(v: std::net::Ipv4Addr) -> bool {
        !(v.is_private()
            || v.is_loopback()
            || v.is_link_local()
            || v.is_unspecified()
            || v.is_broadcast()
            || v.is_multicast()
            || v.octets()[0] == 0
            || v.octets()[0] == 100 && (64..128).contains(&v.octets()[1]))
    }
    let public = |ip: &std::net::IpAddr| match ip {
        std::net::IpAddr::V4(v) => public_v4(*v),
        std::net::IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => public_v4(m),
            None => {
                !(v.is_loopback()
                    || v.is_unspecified()
                    || v.is_multicast()
                    || (v.segments()[0] & 0xfe00) == 0xfc00
                    || (v.segments()[0] & 0xffc0) == 0xfe80)
            }
        },
    };
    let parsed: Vec<std::net::IpAddr> = ips.iter().filter_map(|ip| ip.parse().ok()).collect();
    if parsed.is_empty() || parsed.len() != ips.len() || !parsed.iter().all(public) {
        return no("unreachable");
    }
    // Connect to the address that was just checked, so a second DNS answer
    // can't point the request somewhere private.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, std::net::SocketAddr::new(parsed[0], 443))
        .build()
        .map_err(anyhow::Error::from)?;
    let Ok(r) = http.get(format!("https://{host}/")).send().await else {
        return no("unreachable");
    };
    let h = r.headers();
    let xfo = h
        .get("x-frame-options")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let csp = h
        .get_all("content-security-policy")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect::<Vec<_>>()
        .join(";")
        .to_ascii_lowercase();
    let ancestors_block = csp
        .split(';')
        .map(str::trim)
        .find_map(|d| d.strip_prefix("frame-ancestors"))
        .is_some_and(|v| !v.split_whitespace().any(|x| x == "*"));
    if xfo.contains("deny") || xfo.contains("sameorigin") || ancestors_block {
        return no("refused");
    }
    if r.status().is_server_error() {
        return no("error");
    }
    Ok(Json(Preview {
        frameable: true,
        reason: None,
    }))
}

/// A service's recent output.
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceLogs {
    /// The last 300 lines of each replica, oldest first.
    logs: Vec<pyo_proto::LogLine>,
    /// When they were read, in milliseconds since 1970.
    fetched_at: i64,
}

/// Recent logs
///
/// The last lines each of a service's replicas printed, read from its
/// cluster on request. Takes up to 15 seconds.
#[utoipa::path(get, path = "/services/{id}/logs", operation_id = "getServiceLogs", tag = "Services",
    params(("id" = String, Path, description = "The service's id (`svc_…`).")),
    responses(
        (status = 200, description = "The lines.", body = ServiceLogs),
        (status = 404, description = "No such service, or it isn't yours to see.", body = ErrorBody),
        (status = 502, description = "The cluster couldn't read them.", body = ErrorBody),
        (status = 504, description = "The cluster didn't answer within 15 seconds (is it connected?).", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
async fn service_logs(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    Ok(Json(fetch_service_logs(&s, &user, &id).await?))
}

/// Recent output of a service's replicas, read by its cluster's agent.
pub async fn fetch_service_logs(s: &AppState, user: &User, id: &str) -> ApiResult<Value> {
    let id = id.to_owned();
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "services", &id).await?;
    require_role(&mut tx, user, &team_id, Role::Viewer).await?;
    let cluster_id: String =
        sqlx::query_scalar("SELECT cluster_id FROM pyo.services WHERE id = $1")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
    let service = service_ref(&mut tx, &id).await?;
    let cmd_id = deploy::enqueue(
        &mut tx,
        &cluster_id,
        Command::FetchLogs {
            service,
            tail_lines: 300,
        },
    )
    .await?;
    tx.commit().await?;
    s.wake(&cluster_id).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let row: Option<(String, Option<Value>)> =
            sqlx::query_as("SELECT status, result FROM pyo.commands WHERE id = $1")
                .bind(&cmd_id)
                .fetch_optional(&s.db)
                .await?;
        if let Some((status, result)) = row
            && (status == "done" || status == "failed")
        {
            let r: CommandResult = result
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            if !r.ok {
                return Err(ApiError::new(
                    axum::http::StatusCode::BAD_GATEWAY,
                    r.message
                        .unwrap_or_else(|| "The agent couldn't read the logs.".into()),
                ));
            }
            return Ok(serde_json::to_value(ServiceLogs {
                logs: r.logs,
                fetched_at: now_ms(),
            })
            .map_err(anyhow::Error::from)?);
        }
        if tokio::time::Instant::now() > deadline {
            return Err(ApiError::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "The cluster's agent didn't answer in time. Is it connected?",
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct MetricsQuery {
    /// The team's id.
    team: String,
    /// A cluster's id, or "all" (the default).
    cluster: Option<String>,
    /// "1h", "24h" (the default), "7d" or "30d".
    range: Option<String>,
}

/// Server and service usage
///
/// CPU, memory, disk and network over time for a team's servers and
/// services, sampled once a minute. Servers also get load and disk reads
/// and writes. Each series lines up with `times`, with `null` where nothing
/// was measured.
#[utoipa::path(get, path = "/metrics", operation_id = "getMetrics", tag = "Observability",
    params(MetricsQuery),
    responses(
        (status = 200, description = "`times` (milliseconds) and `servers`, `services` and `ingress`, each with its series.", body = Object),
        (status = 400, description = "No `team`.", body = ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
        (status = 503, description = "This installation doesn't store metrics.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn metrics(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Query(q): axum::extract::Query<MetricsQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    tx.commit().await?;
    let cluster = q.cluster.filter(|c| c != "all").unwrap_or_default();
    let range = q.range.as_deref().unwrap_or("24h");
    Ok(Json(
        s.observability.metrics(&q.team, &cluster, range).await?,
    ))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ObservabilityQuery {
    /// The team's id.
    team: String,
    /// Only this project's sites: its id, or "all".
    project: Option<String>,
    /// Only this domain: its id, or "all".
    site: Option<String>,
    /// Only this service's domains: its id.
    service: Option<String>,
    /// "1h", "24h", "7d" (the default) or "30d".
    range: Option<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct AccessLogQuery {
    /// The team's id.
    team: String,
    /// Only this project's sites: its id, or "all".
    project: Option<String>,
    /// Only this domain: its id, or "all".
    site: Option<String>,
    /// Only this service's domains: its id.
    service: Option<String>,
    /// "1h", "24h" (the default), "7d" or "30d".
    range: Option<String>,
    /// Only requests before this time, in milliseconds: for the next page.
    #[serde(default)]
    before: i64,
    /// At most this many, 1 to 500 (100 by default).
    limit: Option<u32>,
    /// Only one class of status: "1xx" to "5xx".
    status: Option<String>,
    /// Only this method, such as "POST".
    method: Option<String>,
    /// Only requests whose URL or host contains this (any case).
    q: Option<String>,
}

/// Request log
///
/// The web server's log of requests to a team's websites, newest first,
/// filtered like the traffic figures.
#[utoipa::path(get, path = "/access-logs", operation_id = "getAccessLogs", tag = "Observability",
    params(AccessLogQuery),
    responses(
        (status = 200, description = "`logs` (newest first: time, method, host, path, status, duration, size, country, device and address as the IP setting allows), `next` (the `before` for the next page, or null), `ipMode`, and on the first page a `summary` of the filtered requests.", body = Object),
        (status = 400, description = "No `team`.", body = ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
        (status = 502, description = "The log storage couldn't be read.", body = ErrorBody),
        (status = 503, description = "This installation doesn't store request logs.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn access_logs(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Query(q): axum::extract::Query<AccessLogQuery>,
) -> ApiResult<Json<Value>> {
    let hosts = team_hosts(
        &s,
        &user,
        &q.team,
        q.project.clone(),
        q.site.clone(),
        q.service.clone(),
    )
    .await?;
    let f = crate::observability::LogFilter {
        range: q.range.unwrap_or_else(|| "24h".into()),
        before: q.before,
        limit: q.limit.unwrap_or(100),
        status: q.status,
        method: q.method,
        q: q.q,
    };
    Ok(Json(
        s.observability.access_logs(&q.team, &hosts, &f).await?,
    ))
}

/// The hostnames of a team's websites: all, one project's, one domain, or one service's.
async fn team_hosts(
    s: &AppState,
    user: &User,
    team: &str,
    project: Option<String>,
    site: Option<String>,
    service: Option<String>,
) -> ApiResult<Vec<String>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, user, team, Role::Viewer).await?;
    let project = project.filter(|p| p != "all");
    let site = site.filter(|p| p != "all");
    let hosts: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT lower(d->>'hostname')
           FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
          WHERE a.team_id = $1 AND ($2::text IS NULL OR a.project_id = $2) AND ($3::text IS NULL OR d->>'id' = $3) AND ($4::text IS NULL OR a.id = $4)",
    )
    .bind(team)
    .bind(&project)
    .bind(&site)
    .bind(&service)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(hosts)
}

/// Website traffic
///
/// Visits, requests, errors, response times, countries, pages and referrers
/// for a team's websites: all of them, one project's, one service's, or one
/// domain.
#[utoipa::path(get, path = "/observability", operation_id = "getObservability", tag = "Observability",
    params(ObservabilityQuery),
    responses(
        (status = 200, description = "Totals, series over time and top lists for the range.", body = Object),
        (status = 400, description = "No `team`.", body = ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = ErrorBody),
        (status = 503, description = "This installation doesn't store traffic.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn observability(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Query(q): axum::extract::Query<ObservabilityQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    let project = q.project.filter(|p| p != "all");
    let site = q.site.filter(|p| p != "all");
    let hosts: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT lower(d->>'hostname')
           FROM pyo.services a, jsonb_array_elements(a.spec->'domains') d
          WHERE a.team_id = $1 AND ($2::text IS NULL OR a.project_id = $2) AND ($3::text IS NULL OR d->>'id' = $3) AND ($4::text IS NULL OR a.id = $4)",
    )
    .bind(&q.team)
    .bind(&project)
    .bind(&site)
    .bind(&q.service)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let range = match q.range.as_deref() {
        Some(r @ ("1h" | "24h" | "7d" | "30d")) => r.to_owned(),
        _ => "7d".to_owned(),
    };
    Ok(Json(s.observability.query(&q.team, &hosts, &range).await?))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ConnectionsQuery {
    /// "1h", "24h", "7d" (the default) or "30d".
    range: Option<String>,
}

/// Service connections
///
/// Connections to a service's public TCP and UDP ports through the
/// gateway: per port, over time, by country and outcome ("ok", "blocked" by an
/// IP rule, "limited" by the connection limit, "unreachable" when the service
/// didn't answer), and the latest 50. A connection is counted when it
/// closes (a UDP exchange after a minute without packets). Ports that bypass
/// the gateway aren't counted.
#[utoipa::path(get, path = "/services/{id}/connections", operation_id = "getServiceConnections", tag = "Observability",
    params(("id" = String, Path, description = "The service's id."), ConnectionsQuery),
    responses(
        (status = 200, description = "`ports`, `series`, `countries`, `outcomes`, `recent` (address as the IP setting allows) and `ipMode`.", body = Object),
        (status = 404, description = "No such service, or you can't see it.", body = ErrorBody),
        (status = 503, description = "This installation doesn't store traffic.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn connections(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<ConnectionsQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "services", &id).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    tx.commit().await?;
    let range = match q.range.as_deref() {
        Some(r @ ("1h" | "24h" | "7d" | "30d")) => r.to_owned(),
        _ => "7d".to_owned(),
    };
    Ok(Json(
        s.observability.connections(&team_id, &id, &range).await?,
    ))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct BlockedQuery {
    /// The service's id.
    service: String,
    /// "1h", "24h" (the default), "7d" or "30d".
    range: Option<String>,
}

/// Firewall refusals
///
/// What a service's firewall turned away: website requests (a 403, or a
/// 429 over a rate limit) and connections to TCP ports through the gateway.
/// It shows them over time (`series`), per rule, per source, country and
/// website path, and the latest 50. A rule is its id, or "blocked" or
/// "rate-limit" when no rule matches the source now. UDP ports and ports
/// that bypass the gateway aren't counted.
#[utoipa::path(get, path = "/firewall/blocked", operation_id = "getFirewallBlocked", tag = "Firewall",
    params(BlockedQuery),
    responses(
        (status = 200, description = "`total`, `series`, `rules`, `sources` (address as the IP setting allows), `countries`, `paths`, `recent` and `ipMode`.", body = Object),
        (status = 404, description = "No such service, or you can't see it.", body = ErrorBody),
        (status = 503, description = "This installation doesn't store traffic.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn firewall_blocked(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Query(q): axum::extract::Query<BlockedQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "services", &q.service).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    tx.commit().await?;
    let services = vec![q.service];
    let range = match q.range.as_deref() {
        Some(r @ ("1h" | "24h" | "7d" | "30d")) => r.to_owned(),
        _ => "24h".to_owned(),
    };
    Ok(Json(
        s.observability.blocked(&team_id, &services, &range).await?,
    ))
}

/// WAF findings
///
/// What a service's web application firewall found: requests it blocked,
/// and ones it only logged in log-only mode. It shows them over time
/// (`series`), per rule, domain, source, country and path, and the latest
/// 50 with the rules that matched. `custom` shows how often each custom
/// rule matched, by its number. Query strings are kept with sensitive
/// values redacted.
#[utoipa::path(get, path = "/waf/hits", operation_id = "getWafHits", tag = "Firewall",
    params(BlockedQuery),
    responses(
        (status = 200, description = "`blocked`, `logged`, `clients`, `series`, `rules`, `hosts`, `sources`, `countries`, `paths`, `recent`, `custom` and `ipMode`.", body = Object),
        (status = 404, description = "No such service, or you can't see it.", body = ErrorBody),
        (status = 503, description = "This installation doesn't store traffic.", body = ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn waf_hits(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    axum::extract::Query(q): axum::extract::Query<BlockedQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = s.db.begin().await?;
    let team_id = team_of(&mut tx, "services", &q.service).await?;
    require_role(&mut tx, &user, &team_id, Role::Viewer).await?;
    tx.commit().await?;
    let services = vec![q.service];
    let range = match q.range.as_deref() {
        Some(r @ ("1h" | "24h" | "7d" | "30d")) => r.to_owned(),
        _ => "24h".to_owned(),
    };
    Ok(Json(
        s.observability
            .waf_hits(&team_id, &services, &range)
            .await?,
    ))
}

/// Your address
///
/// The address (and its country) Pyo sees your requests come from, so the
/// dashboard can warn before a firewall rule would shut you out.
#[utoipa::path(get, path = "/firewall/me", operation_id = "getMyAddress", tag = "Firewall",
    responses((status = 200, description = "`ip` and `country` (a two-letter code, or empty when unknown).", body = Object)),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn firewall_me(
    State(s): State<AppState>,
    SessionUser(_user): SessionUser,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
) -> ApiResult<Json<Value>> {
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    let ip = crate::observability::client_ip(
        &peer.ip().to_string(),
        &h("x-forwarded-for"),
        &h("x-real-ip"),
    );
    Ok(Json(
        json!({ "ip": ip, "country": s.observability.country_of(&ip) }),
    ))
}

/// A new project (createProject, and a Compose import's project).
async fn create_project(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    team_id: &str,
    name: &str,
    namespace: &str,
    description: &str,
) -> ApiResult<()> {
    client_id(id, "prj")?;
    require_role(tx, user, team_id, Role::Developer).await?;
    let name = require_text(name, "Project name", 60)?;
    require_dns_label(namespace, "Slug")?;
    if namespace.starts_with("kube-") || namespace == "default" || namespace == "pyo-system" {
        return Err(ApiError::bad_request("That name is reserved."));
    }
    // It's the project's address (/<namespace>/...), so it can't be a team page's.
    if RESERVED_PROJECT_SLUGS.contains(&namespace) {
        return Err(ApiError::bad_request(
            "That name is used by a dashboard page. Pick another.",
        ));
    }
    sqlx::query("INSERT INTO pyo.projects (id, team_id, name, namespace, description) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(team_id)
        .bind(&name)
        .bind(namespace)
        .bind(description.trim())
        .execute(&mut **tx)
        .await?;
    log_activity(
        tx,
        team_id,
        &user.name,
        "project.created",
        "info",
        &format!("Created project {name}"),
        json!({ "kind": "project", "id": id, "label": name }),
        json!({ "projectId": id }),
    )
    .await?;
    Ok(())
}

/// A new service and its first deployment (createService, and each of a
/// Compose import's services).
async fn create_service(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    service: Value,
    deployment_id: &str,
    volumes: Vec<Value>,
    databases: Vec<ServiceDatabase>,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    let id = service["id"].as_str().unwrap_or("").to_owned();
    if !crate::ids::valid_client_id(&id, "svc") {
        return Err(ApiError::bad_request("Invalid service id."));
    }
    client_id(deployment_id, "dpl")?;
    let project_id = service["projectId"].as_str().unwrap_or("").to_owned();
    let cluster_id = service["clusterId"].as_str().unwrap_or("").to_owned();
    let team_id = team_of(tx, "projects", &project_id).await?;
    require_role(tx, user, &team_id, Role::Developer).await?;
    let cluster_team: Option<String> =
        sqlx::query_scalar("SELECT team_id FROM pyo.clusters WHERE id = $1")
            .bind(&cluster_id)
            .fetch_optional(&mut **tx)
            .await?;
    if cluster_team.as_deref() != Some(team_id.as_str()) {
        return Err(ApiError::bad_request("Pick one of this team's clusters."));
    }
    let mut spec = service_spec_from(&service);
    crate::template_source::scrub_fields(&mut spec);
    validate_service_spec(&spec)?;
    gpu_ok(tx, &cluster_id, &spec).await?;
    github_source_ok(tx, &team_id, &spec).await?;
    crate::certs::domains_ok(tx, &team_id, &spec).await?;
    domains_unique(tx, &team_id, &id, &spec).await?;
    ports_unique(tx, &team_id, &id, &cluster_id, &spec).await?;
    crate::secretstores::imports_ok(tx, &team_id, &spec).await?;
    let name = spec["name"].as_str().unwrap_or("").to_owned();
    // A deleted service keeps its row (and name) until the cluster
    // has removed its objects, so a new one can't take them over.
    let existing: Option<bool> =
        sqlx::query_scalar("SELECT deleting FROM pyo.services WHERE project_id = $1 AND name = $2")
            .bind(&project_id)
            .bind(&name)
            .fetch_optional(&mut **tx)
            .await?;
    if let Some(deleting) = existing {
        return Err(ApiError::conflict(name_taken_message(&name, deleting)));
    }
    crate::vault::get().seal_env(&team_id, &mut spec).await?;
    sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(&id)
        .bind(&team_id)
        .bind(&project_id)
        .bind(&cluster_id)
        .bind(&name)
        .bind(&spec)
        .execute(&mut **tx)
        .await?;

    let namespace: String = sqlx::query_scalar("SELECT namespace FROM pyo.projects WHERE id = $1")
        .bind(&project_id)
        .fetch_one(&mut **tx)
        .await?;
    for v in volumes {
        let vid = v["id"].as_str().unwrap_or("").to_owned();
        new_volume_id(tx, &vid).await?;
        require_dns_label(v["name"].as_str().unwrap_or(""), "Volume name")?;
        let mut data = v.clone();
        if let Some(o) = data.as_object_mut() {
            // Its backup status is worked out when read (teambackup.rs).
            for k in [
                "id",
                "teamId",
                "clusterId",
                "projectId",
                "serviceId",
                "createdAt",
                "backup",
            ] {
                o.remove(k);
            }
            o.insert("status".into(), json!("pending"));
        }
        sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, service_id, data, namespace) VALUES ($1, $2, $3, $4, $5, $6, $7)")
            .bind(&vid)
            .bind(&team_id)
            .bind(&cluster_id)
            .bind(&project_id)
            .bind(&id)
            .bind(data)
            .bind(&namespace)
            .execute(&mut **tx)
            .await?;
    }
    log_activity(
        tx,
        &team_id,
        &user.name,
        "service.created",
        "info",
        &format!("Created {name}"),
        json!({ "kind": "service", "id": id, "label": name }),
        json!({ "projectId": project_id, "serviceId": id, "clusterId": cluster_id }),
    )
    .await?;
    // Its databases' variables, in the first deploy.
    for db in databases {
        crate::databases::connect(tx, user, &db.id, &id, db.env).await?;
    }
    let c = deploy::create(
        tx,
        DeployRequest {
            deployment_id,
            service_id: &id,
            trigger: "manual",
            actor: &user.name,
            rollback_of: None,
            retry_of: None,
            commit: None,
        },
    )
    .await?;
    wake.push(c);
    Ok(())
}

#[cfg(test)]
mod location_tests {
    use super::*;

    #[test]
    fn locations() {
        assert_eq!(
            require_location("Yogyakarta, ID").unwrap(),
            "Yogyakarta, ID"
        );
        assert_eq!(
            require_location(" frankfurt , de ").unwrap(),
            "frankfurt, DE"
        );
        assert_eq!(require_location("sg").unwrap(), "SG");
        assert!(require_location("").is_err());
        assert!(require_location("!!").is_err());
        assert_eq!(require_location(" US  East ").unwrap(), "US East");
        assert_eq!(require_location("Frankfurt").unwrap(), "Frankfurt");
    }
}

#[cfg(test)]
mod op_name_tests {
    use super::Op;
    use serde_json::json;

    #[test]
    fn cron_schedules_have_five_fields() {
        for ok in ["0 3 * * *", "*/5 * * * *", "0 0 1,15 * MON-FRI"] {
            assert!(super::check_schedule(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "0 3 * * ; rm -rf /",
            "$(whoami) * * * *",
        ] {
            assert!(super::check_schedule(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reads_service_names() {
        assert!(matches!(
            serde_json::from_value::<Op>(json!({ "op": "scaleService", "id": "a", "replicas": 2 }))
                .unwrap(),
            Op::ScaleService { replicas: 2, .. }
        ));
        assert!(
            matches!(serde_json::from_value::<Op>(json!({ "op": "deploy", "serviceId": "a", "deploymentId": "d", "trigger": "manual" })).unwrap(), Op::Deploy { service_id, .. } if service_id == "a")
        );
        assert!(matches!(
            serde_json::from_value::<Op>(
                json!({ "op": "createService", "service": {}, "deploymentId": "d" })
            )
            .unwrap(),
            Op::CreateService { .. }
        ));
        assert!(
            serde_json::from_value::<Op>(json!({ "op": "scaleApp", "id": "a", "replicas": 2 }))
                .is_err()
        );
    }

    #[test]
    fn a_name_still_being_deleted_says_so() {
        assert_eq!(
            super::name_taken_message("web", true),
            "web is still being deleted. Try again in a few seconds."
        );
        assert_eq!(
            super::name_taken_message("web", false),
            "This project already has a service called web."
        );
    }
}

#[cfg(test)]
mod removal_tests {
    use super::*;

    /// Needs Postgres: DATABASE_URL=… cargo test -p pyo-api -- --ignored ops::removal_tests
    #[tokio::test]
    #[ignore]
    async fn removing_servers_and_clusters_in_the_database() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        crate::migrate::run(&pool).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            pool.clone(),
            crate::vault::Provider::from_env(),
        ));
        let run = crate::ids::new_id("t");
        let (team, cluster, project, service) = (
            format!("team_{run}"),
            format!("cl_{run}"),
            format!("prj_{run}"),
            format!("svc_{run}"),
        );
        let user = User {
            id: format!("usr_{run}"),
            name: "T".into(),
            email: format!("{run}@t.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        let mut tx = pool.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'owner', 'active')").bind(format!("mem_{run}")).bind(&team).bind(&user.id).bind(&user.email).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE pyo.teams SET default_cluster_id = $2 WHERE id = $1")
            .bind(&team)
            .bind(&cluster)
            .execute(&mut *tx)
            .await
            .unwrap();
        for n in ["a", "b"] {
            sqlx::query(
                "INSERT INTO pyo.servers (id, cluster_id, name, report) VALUES ($1, $2, $3, $4)",
            )
            .bind(format!("nd_{n}{run}"))
            .bind(&cluster)
            .bind(n)
            .bind(json!({ "status": "ready", "roles": [] }))
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $3)",
        )
        .bind(&project)
        .bind(&team)
        .bind(format!("shop-{run}"))
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, 'web', '{\"name\":\"web\"}')").bind(&service).bind(&team).bind(&project).bind(&cluster).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'succeeded', '{}', '{}')")
            .bind(format!("dpl_{run}"))
            .bind(&team)
            .bind(&project)
            .bind(&service)
            .bind(&cluster)
            .execute(&mut *tx)
            .await
            .unwrap();
        let vol = format!("vol_{run}");
        sqlx::query("INSERT INTO pyo.volumes (id, team_id, cluster_id, project_id, data, namespace) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(&vol)
            .bind(&team)
            .bind(&cluster)
            .bind(&project)
            .bind(json!({ "name": "old-data", "sizeGi": 5, "storageClass": "local-path", "server": "a" }))
            .bind(format!("shop-{run}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.databases (id, team_id, cluster_id, name, engine, version, size, storage_gi, instances, namespace, password) VALUES ($1, $2, $3, 'main', 'postgres', '17', 'small', 5, 1, $4, '')")
            .bind(format!("mdb_{run}"))
            .bind(&team)
            .bind(&cluster)
            .bind(format!("shop-{run}"))
            .execute(&mut *tx)
            .await
            .unwrap();
        deploy::enqueue(
            &mut tx,
            &cluster,
            Command::DeleteNamespace {
                namespace: "x".into(),
                project_id: "y".into(),
            },
        )
        .await
        .unwrap();

        // A detached volume's data is on server a: a can't go, b can.
        let mut wake = vec![];
        let err = remove_node(&mut tx, &user, &format!("nd_a{run}"), false, &mut wake)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("Delete these volumes first") && err.message.contains("old-data"),
            "{}",
            err.message
        );
        assert_eq!(
            detached_volumes_on(&mut tx, &cluster, "b").await.unwrap(),
            Vec::<String>::new()
        );

        // Reachable: only the normal path, which waits for its services.
        assert!(
            remove_cluster(&mut tx, &user, &cluster, false)
                .await
                .unwrap_err()
                .message
                .contains("Delete or move")
        );
        assert!(
            remove_cluster(&mut tx, &user, &cluster, true)
                .await
                .unwrap_err()
                .message
                .contains("is reachable")
        );
        sqlx::query("UPDATE pyo.clusters SET status = 'unreachable', last_seen_at = now() - interval '9 minutes' WHERE id = $1").bind(&cluster).execute(&mut *tx).await.unwrap();
        assert!(
            remove_cluster(&mut tx, &user, &cluster, true)
                .await
                .is_err(),
            "not gone long enough"
        );

        // Gone for 10 minutes: forced, Pyo forgets all of it.
        sqlx::query(
            "UPDATE pyo.clusters SET last_seen_at = now() - interval '11 minutes' WHERE id = $1",
        )
        .bind(&cluster)
        .execute(&mut *tx)
        .await
        .unwrap();
        remove_cluster(&mut tx, &user, &cluster, true)
            .await
            .unwrap();
        for table in [
            "clusters",
            "services",
            "deployments",
            "volumes",
            "databases",
            "commands",
            "servers",
        ] {
            let col = if table == "clusters" {
                "id"
            } else {
                "cluster_id"
            };
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM pyo.{table} WHERE {col} = $1"
            )))
            .bind(&cluster)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        let default: Option<String> =
            sqlx::query_scalar("SELECT default_cluster_id FROM pyo.teams WHERE id = $1")
                .bind(&team)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert!(default.is_none());
        tx.rollback().await.unwrap();
    }
}

#[cfg(test)]
mod secret_tests {
    use super::keep_secret_values;
    use serde_json::json;

    #[test]
    fn a_blank_secret_keeps_its_value() {
        let old = json!({ "env": [{ "id": "e1", "key": "TOKEN", "value": "pyo1:sealed", "secret": true }] });
        let mut spec = json!({ "env": [{ "id": "e1", "key": "TOKEN", "value": "", "secret": true, "hasValue": true }] });
        assert!(keep_secret_values(&mut spec, &old).is_empty());
        assert_eq!(spec["env"][0]["value"], "pyo1:sealed");
    }

    #[test]
    fn making_a_secret_plain_without_a_new_value_is_reported() {
        let old = json!({ "env": [{ "id": "e1", "key": "TOKEN", "value": "pyo1:sealed", "secret": true }] });
        // By id, and by key (a new row with the old key).
        for e in [
            json!({ "id": "e1", "key": "TOKEN", "value": "", "secret": false, "hasValue": true }),
            json!({ "id": "e9", "key": "TOKEN", "value": "", "hasValue": true }),
        ] {
            let mut spec = json!({ "env": [e] });
            assert_eq!(
                keep_secret_values(&mut spec, &old),
                vec!["TOKEN".to_owned()]
            );
        }
        // With a new value it's simply a new plain variable.
        let mut spec =
            json!({ "env": [{ "id": "e1", "key": "TOKEN", "value": "public", "secret": false }] });
        assert!(keep_secret_values(&mut spec, &old).is_empty());
        assert_eq!(spec["env"][0]["value"], "public");
        // A plain variable that stays plain isn't a reveal.
        let old =
            json!({ "env": [{ "id": "e1", "key": "MODE", "value": "pyo1:x", "secret": false }] });
        let mut spec =
            json!({ "env": [{ "id": "e1", "key": "MODE", "value": "", "hasValue": true }] });
        assert!(keep_secret_values(&mut spec, &old).is_empty());
    }
}

#[cfg(test)]
mod waf_lifecycle_tests {
    use super::*;

    /// The WAF's engine follows a cluster's services: installed with the
    /// first one that turns its WAF on, kept while any has it on, removed
    /// after the last one turns it off or is deleted; and the firewall and
    /// WAF rules belong to one service. Needs Postgres:
    /// DATABASE_URL=… cargo test -p pyo-api -- --ignored ops::waf_lifecycle_tests
    #[test]
    #[ignore]
    fn the_waf_engine_follows_the_services_in_the_database() {
        // The operations' future is large: a roomier stack than a test's.
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(lifecycle())
            })
            .unwrap()
            .join()
            .unwrap();
    }

    async fn lifecycle() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let db = sqlx::PgPool::connect(&url).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        let config = crate::Config {
            agent_api_url: String::new(),
            agent_image: String::new(),
            waf_image: pyo_proto::system::WAF_IMAGE_DEFAULT.into(),
            geoip_image: pyo_proto::system::GEOIP_IMAGE_DEFAULT.into(),
            cookie_secure: false,
            allow_signup: false,
            builder_image: String::new(),
            tailnet_api_url: String::new(),
            relay_addr: None,
        };
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        let s = AppState {
            db: db.clone(),
            config: std::sync::Arc::new(config),
            observability: crate::observability::Observability::from_env(db.clone()),
            sealer: std::sync::Arc::new(crate::secrets::Sealer::from_env()),
            wakers: Default::default(),
            terminals: Default::default(),
            stopping,
        };
        let run = crate::ids::new_id("t");
        let (team, cluster, project) = (
            format!("team_{run}"),
            format!("cl_{run}"),
            format!("prj_{run}"),
        );
        let [a, b, c] = ["a", "b", "c"].map(|n| format!("svc_{n}{run}"));
        let user = User {
            id: format!("usr_{run}"),
            name: "T".into(),
            email: format!("{run}@t.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        let mut tx = db.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'developer', 'active')").bind(format!("mem_{run}")).bind(&team).bind(&user.id).bind(&user.email).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&mut *tx).await.unwrap();
        sqlx::query(
            "INSERT INTO pyo.projects (id, team_id, name, namespace) VALUES ($1, $2, 'shop', $3)",
        )
        .bind(&project)
        .bind(&team)
        .bind(format!("shop-{run}"))
        .execute(&mut *tx)
        .await
        .unwrap();
        for (i, id) in [&a, &b, &c].into_iter().enumerate() {
            let name = format!("web{i}");
            sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec) VALUES ($1, $2, $3, $4, $5, $6)")
                .bind(id)
                .bind(&team)
                .bind(&project)
                .bind(&cluster)
                .bind(&name)
                .bind(json!({ "name": name }))
                .execute(&mut *tx)
                .await
                .unwrap();
            // Running: its live deployment's routes are what a sync sends.
            let dep = format!("dpl_{i}{run}");
            let spec = json!({ "service": { "serviceId": id, "name": name, "namespace": format!("shop-{run}"), "workloadKind": "Deployment" }, "projectId": project, "ports": [], "domains": [] });
            sqlx::query("INSERT INTO pyo.deployments (id, team_id, project_id, service_id, cluster_id, status, data, spec) VALUES ($1, $2, $3, $4, $5, 'succeeded', '{}', $6)")
                .bind(&dep)
                .bind(&team)
                .bind(&project)
                .bind(id)
                .bind(&cluster)
                .bind(spec)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("UPDATE pyo.services SET current_deployment_id = $2 WHERE id = $1")
                .bind(id)
                .bind(&dep)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        // The commands queued since the last look, in the agent's order:
        // (kind, the sync's `waf`).
        let seen = std::sync::Mutex::new(0usize);
        let queued = || async {
            let all: Vec<Value> = sqlx::query_scalar(
                "SELECT command FROM pyo.commands WHERE cluster_id = $1 ORDER BY created_at",
            )
            .bind(&cluster)
            .fetch_all(&db)
            .await
            .unwrap();
            let mut n = seen.lock().unwrap();
            let new: Vec<(String, Option<bool>)> = all[*n..]
                .iter()
                .map(|c| (c["kind"].as_str().unwrap().to_owned(), c["waf"].as_bool()))
                .collect();
            *n = all.len();
            new
        };
        let engine = || async {
            sqlx::query_as::<_, (String, String)>("SELECT state, message FROM pyo.cluster_bundles WHERE cluster_id = $1 AND bundle = 'waf'").bind(&cluster).fetch_optional(&db).await.unwrap()
        };
        let set_waf = |service: &str, mode: &str| json!({ "op": "setServiceWaf", "serviceId": service, "mode": mode });
        let k = |kind: &str, waf: Option<bool>| (kind.to_owned(), waf);

        // A new service's WAF is off: no engine, and changing an off
        // service's custom rules sends nothing.
        assert_eq!(engine().await, None);
        let rule = json!({ "op": "addWafRule", "id": format!("wfr_a{run}"), "serviceId": a, "name": "No wp-admin", "match": "all", "conditions": [{ "field": "path", "op": "startsWith", "value": "/wp-admin" }], "action": "block" });
        run_json(&s, &user, rule).await.unwrap();
        assert_eq!(queued().await, []);
        // The old cluster default is gone.
        assert!(run_json(&s, &user, set_waf(&a, "default")).await.is_err());

        // The first service on: the engine's install, then the sync that
        // points its routes at it.
        run_json(&s, &user, set_waf(&a, "log")).await.unwrap();
        assert_eq!(
            queued().await,
            [k("apply_bundle", None), k("sync_firewall", Some(true))]
        );
        assert_eq!(engine().await.unwrap().0, "installing");
        let sync: Value = sqlx::query_scalar("SELECT command FROM pyo.commands WHERE cluster_id = $1 AND command->>'kind' = 'sync_firewall' ORDER BY created_at DESC LIMIT 1").bind(&cluster).fetch_one(&db).await.unwrap();
        let target = |id: &str| {
            sync["targets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["service"]["serviceId"] == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(target(&a)["waf"]["settings"]["mode"], "log");
        assert_eq!(
            target(&a)["waf"]["rules"][0]["name"],
            "No wp-admin",
            "its own rules"
        );
        assert_eq!(
            target(&b)["waf"]["settings"]["mode"],
            "off",
            "the others' WAF stays off"
        );
        assert!(target(&b)["waf"].get("rules").is_none());
        // The dashboard shows it starting on the service; no cluster WAF.
        let snap = crate::snapshot::build(&db, &user, "").await.unwrap();
        let shown = |id: &str| {
            snap["services"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["id"] == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(
            shown(&a)["wafState"],
            json!({ "state": "installing", "message": "" })
        );
        assert!(shown(&b).get("wafState").is_none());
        let cl = snap["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == json!(cluster))
            .cloned()
            .unwrap();
        assert!(cl.get("waf").is_none() && cl.get("firewall").is_none());
        assert_eq!(
            snap["wafRules"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["serviceId"] == json!(a))
                .count(),
            1
        );

        // Two more on, one after the other (the second while the engine is
        // still installing): no second install.
        run_json(&s, &user, set_waf(&b, "block")).await.unwrap();
        run_json(&s, &user, set_waf(&c, "log")).await.unwrap();
        assert_eq!(
            queued().await,
            [
                k("sync_firewall", Some(true)),
                k("sync_firewall", Some(true))
            ]
        );
        // Now on, a rule change is applied; an off service's settings
        // change alone sends nothing.
        run_json(
            &s,
            &user,
            json!({ "op": "removeWafRule", "id": format!("wfr_a{run}") }),
        )
        .await
        .unwrap();
        assert_eq!(queued().await, [k("sync_firewall", Some(true))]);

        // Off while others are on: the engine stays.
        run_json(&s, &user, set_waf(&a, "off")).await.unwrap();
        run_json(&s, &user, set_waf(&c, "off")).await.unwrap();
        assert_eq!(
            queued().await,
            [
                k("sync_firewall", Some(true)),
                k("sync_firewall", Some(true))
            ]
        );
        run_json(
            &s,
            &user,
            json!({ "op": "setServiceWaf", "serviceId": a, "mode": "off", "level": 2 }),
        )
        .await
        .unwrap();
        assert_eq!(queued().await, []);
        sqlx::query("UPDATE pyo.cluster_bundles SET state = 'applied' WHERE cluster_id = $1 AND bundle = 'waf'").bind(&cluster).execute(&db).await.unwrap();

        // The last one on is deleted: its delete, the sync that takes the
        // routes off the engine, then the engine's removal.
        run_json(&s, &user, json!({ "op": "deleteService", "id": b }))
            .await
            .unwrap();
        assert_eq!(
            queued().await,
            [
                k("delete_service", None),
                k("sync_firewall", Some(false)),
                k("remove_bundle", None)
            ]
        );
        assert_eq!(engine().await.unwrap().0, "removing");
        // And on again before the removal is done: installed after it.
        run_json(&s, &user, set_waf(&a, "block")).await.unwrap();
        assert_eq!(
            queued().await,
            [k("apply_bundle", None), k("sync_firewall", Some(true))]
        );
        assert_eq!(
            engine().await.unwrap(),
            ("installing".to_owned(), "Waiting for removal".to_owned())
        );
        // The gateway refused a's policy: shown on a.
        sqlx::query("UPDATE pyo.clusters SET capabilities = jsonb_build_object('wafRejected', jsonb_build_array($2::text)) WHERE id = $1").bind(&cluster).bind(&a).execute(&db).await.unwrap();
        sqlx::query("UPDATE pyo.cluster_bundles SET state = 'applied', message = '', ready = '[]' WHERE cluster_id = $1 AND bundle = 'waf'").bind(&cluster).execute(&db).await.unwrap();
        let snap = crate::snapshot::build(&db, &user, "").await.unwrap();
        let a_shown = snap["services"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == json!(a))
            .cloned()
            .unwrap();
        assert_eq!(a_shown["wafState"]["state"], "failed", "{a_shown}");
        // The last one off: the sync, then the removal.
        sqlx::query("UPDATE pyo.cluster_bundles SET state = 'applied', message = '' WHERE cluster_id = $1 AND bundle = 'waf'").bind(&cluster).execute(&db).await.unwrap();
        run_json(&s, &user, set_waf(&a, "off")).await.unwrap();
        assert_eq!(
            queued().await,
            [k("sync_firewall", Some(false)), k("remove_bundle", None)]
        );

        // The firewall is per service, its rules in order.
        let fw = |id: &str, service: &str, action: &str, operator: &str, sources: &[&str]| json!({ "op": "addFirewallRule", "id": id, "serviceId": service, "action": action, "operator": operator, "sources": sources });
        run_json(
            &s,
            &user,
            fw(
                &format!("fwr_a{run}"),
                &a,
                "block",
                "notIn",
                &["203.0.113.7"],
            ),
        )
        .await
        .unwrap();
        run_json(
            &s,
            &user,
            fw(
                &format!("fwr_b{run}"),
                &a,
                "allow",
                "in",
                &["203.0.113.7/32"],
            ),
        )
        .await
        .unwrap();
        run_json(
            &s,
            &user,
            fw(&format!("fwr_c{run}"), &c, "block", "in", &["203.0.113.7"]),
        )
        .await
        .unwrap();
        let mut limit = fw(&format!("fwr_r{run}"), &a, "rateLimit", "in", &[]);
        limit["requests"] = json!(120);
        run_json(&s, &user, limit).await.unwrap();
        let mut too_many = fw(
            &format!("fwr_t{run}"),
            &a,
            "rateLimit",
            "in",
            &[
                "10.0.0.1",
                "10.0.0.2",
                "10.0.0.3",
                "10.0.0.4",
                "10.0.0.5",
                "10.0.0.6",
                "10.0.0.7",
                "10.0.0.8",
                "10.0.0.9",
                "10.0.0.10",
                "10.0.0.11",
                "10.0.0.12",
                "10.0.0.13",
                "10.0.0.14",
                "10.0.0.15",
            ],
        );
        too_many["requests"] = json!(1);
        assert!(
            run_json(&s, &user, too_many)
                .await
                .unwrap_err()
                .message
                .contains("at most 16")
        );
        run_json(&s, &user, json!({ "op": "reorderFirewallRules", "serviceId": a, "ids": [format!("fwr_r{run}"), format!("fwr_b{run}"), format!("fwr_a{run}")] })).await.unwrap();
        run_json(
            &s,
            &user,
            json!({ "op": "updateFirewallRule", "id": format!("fwr_r{run}"), "per": "second" }),
        )
        .await
        .unwrap();
        let mut tx = db.begin().await.unwrap();
        let f = crate::firewall::for_service(&mut tx, &a).await.unwrap();
        let ids: Vec<String> = f.rules.iter().map(|r| r.id.clone()).collect();
        assert_eq!(
            ids,
            [
                format!("fwr_r{run}"),
                format!("fwr_b{run}"),
                format!("fwr_a{run}")
            ]
        );
        assert_eq!(
            f.rules[0].limit,
            Some(pyo_proto::firewall::RateLimit {
                requests: 120,
                per: "second".into()
            })
        );
        assert_eq!(f.rules[2].sources, ["203.0.113.7/32"]);
        assert_eq!(
            crate::firewall::for_service(&mut tx, &c)
                .await
                .unwrap()
                .rules
                .len(),
            1,
            "a's rules are a's"
        );
        tx.rollback().await.unwrap();
        // The cluster-wide operations are gone.
        assert!(
            run_json(
                &s,
                &user,
                json!({ "op": "setClusterWaf", "clusterId": cluster, "on": true })
            )
            .await
            .is_err()
        );
        assert!(run_json(&s, &user, json!({ "op": "addFirewallRule", "id": format!("fwr_d{run}"), "clusterId": cluster, "action": "block", "sources": ["DE"] })).await.is_err());

        sqlx::query("DELETE FROM pyo.teams WHERE id = $1")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("DELETE FROM pyo.users WHERE id = $1")
            .bind(&user.id)
            .execute(&db)
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod compose_import_tests {
    use super::*;

    /// A Compose import creates its project and services as createProject
    /// and createService would, in one transaction; on Tokio's default
    /// 2 MiB worker stacks, which re-entering `apply` overflowed in a debug
    /// build. Needs Postgres:
    /// DATABASE_URL=… cargo test -p pyo-api -- --ignored ops::compose_import_tests
    #[test]
    #[ignore]
    fn a_compose_import_in_the_database() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { tokio::spawn(import()).await.unwrap() });
    }

    async fn import() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let db = sqlx::PgPool::connect(&url).await.unwrap();
        crate::migrate::run(&db).await.unwrap();
        crate::vault::init(crate::vault::Vault::new(
            db.clone(),
            crate::vault::Provider::from_env(),
        ));
        let config = crate::Config {
            agent_api_url: String::new(),
            agent_image: String::new(),
            waf_image: pyo_proto::system::WAF_IMAGE_DEFAULT.into(),
            geoip_image: pyo_proto::system::GEOIP_IMAGE_DEFAULT.into(),
            cookie_secure: false,
            allow_signup: false,
            builder_image: String::new(),
            tailnet_api_url: String::new(),
            relay_addr: None,
        };
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        let s = AppState {
            db: db.clone(),
            config: std::sync::Arc::new(config),
            observability: crate::observability::Observability::from_env(db.clone()),
            sealer: std::sync::Arc::new(crate::secrets::Sealer::from_env()),
            wakers: Default::default(),
            terminals: Default::default(),
            stopping,
        };
        let run = crate::ids::new_id("t");
        let (team, cluster) = (format!("team_{run}"), format!("cl_{run}"));
        let user = User {
            id: format!("usr_{run}"),
            name: "T".into(),
            email: format!("{run}@t.test"),
            timezone: "UTC".into(),
            is_admin: false,
            must_set_up: false,
            key: None,
        };
        sqlx::query(
            "INSERT INTO pyo.users (id, email, name, password_hash) VALUES ($1, $2, 'T', '')",
        )
        .bind(&user.id)
        .bind(&user.email)
        .execute(&db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pyo.teams (id, name, slug) VALUES ($1, $1, $1)")
            .bind(&team)
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pyo.members (id, team_id, user_id, email, role, status) VALUES ($1, $2, $3, $4, 'developer', 'active')").bind(format!("mem_{run}")).bind(&team).bind(&user.id).bind(&user.email).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO pyo.clusters (id, team_id, name, agent_token_hash, status, last_seen_at) VALUES ($1, $2, $1, $1, 'connected', now())").bind(&cluster).bind(&team).execute(&db).await.unwrap();
        sqlx::query("UPDATE pyo.teams SET default_cluster_id = $2 WHERE id = $1")
            .bind(&team)
            .bind(&cluster)
            .execute(&db)
            .await
            .unwrap();

        let file = "name: shop\nservices:\n  web:\n    image: nginx:1.27\n    ports: [\"8080:80\"]\n    depends_on: [db]\n  db:\n    image: postgres:18\n    volumes: [\"data:/var/lib/postgresql/data\"]\nvolumes:\n  data:\n";
        let plan = crate::compose::plan(&crate::compose::Input {
            file,
            name_hint: "shop",
            ..Default::default()
        })
        .unwrap();
        let import = |project_id: Option<&str>| json!({ "op": "importCompose", "teamId": team, "projectId": project_id, "plan": plan });

        // A new project, named after the file, and its services in order.
        let out = run_json(&s, &user, import(None)).await.unwrap();
        let project = out["projectId"].as_str().unwrap().to_owned();
        let (name, namespace, description): (String, String, String) =
            sqlx::query_as("SELECT name, namespace, description FROM pyo.projects WHERE id = $1")
                .bind(&project)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(
            (name.as_str(), namespace.as_str(), description.as_str()),
            ("shop", "shop", "Imported from Docker Compose")
        );
        let made: Vec<&str> = out["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert_eq!(made, ["db", "web"], "dependencies first");
        for v in out["services"].as_array().unwrap() {
            let (svc_cluster, svc_project): (String, String) =
                sqlx::query_as("SELECT cluster_id, project_id FROM pyo.services WHERE id = $1")
                    .bind(v["id"].as_str().unwrap())
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(
                (svc_cluster.as_str(), svc_project.as_str()),
                (cluster.as_str(), project.as_str())
            );
            let service: String =
                sqlx::query_scalar("SELECT service_id FROM pyo.deployments WHERE id = $1")
                    .bind(v["deploymentId"].as_str().unwrap())
                    .fetch_one(&db)
                    .await
                    .unwrap();
            assert_eq!(service, v["id"].as_str().unwrap());
        }
        let volumes: Vec<String> =
            sqlx::query_scalar("SELECT namespace FROM pyo.volumes WHERE project_id = $1")
                .bind(&project)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(volumes, ["shop"]);
        let events: Vec<String> = sqlx::query_scalar("SELECT data->>'type' FROM pyo.activity WHERE team_id = $1 AND data->>'type' IN ('project.created', 'service.created') ORDER BY data->>'type'").bind(&team).fetch_all(&db).await.unwrap();
        assert_eq!(
            events,
            ["project.created", "service.created", "service.created"]
        );

        // Again: a free namespace for the new project; into the first one,
        // the names are taken and nothing of it stays.
        let out = run_json(&s, &user, import(None)).await.unwrap();
        let ns: String = sqlx::query_scalar("SELECT namespace FROM pyo.projects WHERE id = $1")
            .bind(out["projectId"].as_str().unwrap())
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(ns, "shop-2");
        let err = run_json(&s, &user, import(Some(&project)))
            .await
            .unwrap_err();
        assert_eq!(err.message, "This project already has a service called db.");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pyo.services WHERE team_id = $1")
            .bind(&team)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 4);

        // A reserved name is skipped, as createProject would refuse it.
        let mut reserved = plan.clone();
        reserved.project.namespace = "default".into();
        let out = run_json(
            &s,
            &user,
            json!({ "op": "importCompose", "teamId": team, "plan": reserved }),
        )
        .await
        .unwrap();
        let ns: String = sqlx::query_scalar("SELECT namespace FROM pyo.projects WHERE id = $1")
            .bind(out["projectId"].as_str().unwrap())
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(ns, "default-2");
        let err = run_json(&s, &user, json!({ "op": "createProject", "id": format!("prj_x{run}"), "teamId": team, "name": "x", "namespace": "default" })).await.unwrap_err();
        assert_eq!(err.message, "That name is reserved.");
    }
}
