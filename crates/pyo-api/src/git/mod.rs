//! Git providers a team connects with an access token: GitLab, on
//! gitlab.com or self-managed. (GitHub works through Pyo's GitHub App
//! instead, in github.rs; what both share lives here: deploying a push and
//! reporting on commits.)
//!
//! - A team's owner or admin adds a connection: the provider's address and
//!   a token. The token is sealed with the team's key; Pyo checks what it
//!   can do when it's saved and on Test.
//! - A service built from one of its repositories names the connection in
//!   its source (`gitConnectionId`) next to the repository's URL.
//! - A build never gets the connection's token: when a builder takes the
//!   job, Pyo makes a deploy token that can only read that repository and
//!   expires within the hour, and revokes it when the build ends.
//! - Pushes deploy: Pyo adds a webhook to each repository its services
//!   follow, with a secret it checks, and removes it when no service
//!   follows the repository any more. A provider on the internet
//!   (gitlab.com) gets Pyo's public address, one on a private network or
//!   the tailnet its tailnet address first ([`hook_base`]). Without one it
//!   can reach, the branch is checked every minute.
//! - Each deploy sets a commit status (pending, running, success, failed)
//!   linking to the deployment.
//!
//! Every call to the provider goes through the guarded client
//! ([`crate::outbound`]), since the address is the team's.
//!
//! Another provider (Forgejo, Gitea) is a module like gitlab.rs and an arm
//! in [`Connection::api`]'s callers' `match`es: the tables, ops, webhook
//! route, builds and polling don't change.

pub mod gitlab;
pub mod remote;

use crate::error::Json;
use crate::error::Query;
use std::time::Duration;

use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::{SessionUser, User},
    deploy::{self, DeployRequest},
    error::{ApiError, ApiResult},
    ids::{new_id, secret_token},
    ops::{Role, log_activity, require_role},
};

use gitlab::Gitlab;

/// The providers a connection can be for.
pub const PROVIDERS: &[&str] = &["gitlab"];

/// How long a build's deploy token lasts at most (it's revoked when the
/// build ends; this is for builds that never report back).
const CLONE_TOKEN_MINUTES: i64 = 60;

/// A connection as saved, with its secrets opened.
pub struct Connection {
    pub id: String,
    pub provider: String,
    pub base_url: String,
    pub account: String,
    pub public_url: String,
    token: String,
    webhook_secret: String,
}

fn provider_name(p: &str) -> &'static str {
    match p {
        "gitlab" => "GitLab",
        _ => "Git",
    }
}

fn upstream(e: gitlab::Error) -> ApiError {
    ApiError::new(StatusCode::BAD_GATEWAY, e.message)
}

impl Connection {
    /// A client for the provider's API.
    pub async fn api(&self) -> ApiResult<Gitlab> {
        match self.provider.as_str() {
            "gitlab" => Gitlab::connect(&self.base_url, &self.token)
                .await
                .map_err(upstream),
            p => Err(ApiError::bad_request(format!("Pyo doesn't know {p}."))),
        }
    }

    pub fn name(&self) -> &'static str {
        provider_name(&self.provider)
    }

    /// Where webhooks go ([`hook_base`]), or None when Pyo has no address
    /// the provider can reach; and whether the provider is on the internet.
    async fn hook_url(&self) -> (Option<String>, bool) {
        let public = match gitlab::normalize(&self.base_url) {
            // One Pyo can't resolve now counts as on the internet, so it's
            // never handed a private address.
            Ok(u) => crate::outbound::resolves_public(&u).await.unwrap_or(true),
            Err(_) => true,
        };
        let a = crate::web::addresses();
        let addrs = PyoAddresses {
            given: Some(self.public_url.clone()).filter(|u| !u.is_empty()),
            configured: crate::oauth::configured_public_url().map(str::to_owned),
            tailnet: a.tailnet.map(|h| format!("https://{h}")),
            public: a.public.map(|h| format!("https://{h}")),
        };
        let url = hook_base(public, &addrs)
            .map(|b| format!("{}/api/git/{}/webhook", b.trim_end_matches('/'), self.id));
        (url, public)
    }

    /// Why there's no webhook, when Pyo has no address the provider can reach.
    fn no_address(&self, public: bool) -> String {
        if public {
            format!("Pyo has no public address {} can reach.", self.name())
        } else {
            format!("Pyo has no address {} can reach.", self.name())
        }
    }

    /// The repository a URL of this connection names ("group/sub/project").
    pub fn repo_of(&self, repo_url: &str) -> Option<String> {
        repo_path(&self.base_url, repo_url)
    }
}

type Row = (String, String, String, String, String, String, String);

/// A connection by id, its secrets opened. `team`: only that team's.
pub async fn load(db: &sqlx::PgPool, team: Option<&str>, id: &str) -> ApiResult<Connection> {
    opened(row(db, team, id).await?).await
}

/// [`load`] for a person's request: read within their teams (rls.rs), by
/// a viewer of `team` or more, then its secrets opened.
pub async fn load_for(
    db: &sqlx::PgPool,
    user: &crate::auth::User,
    team: &str,
    id: &str,
) -> ApiResult<Connection> {
    let mut tx = crate::rls::begin(db, user).await?;
    crate::ops::require_role(&mut tx, user, team, crate::ops::Role::Viewer).await?;
    let row = row(&mut *tx, Some(team), id).await?;
    tx.commit().await?;
    opened(row).await
}

async fn row<'e, E: sqlx::PgExecutor<'e>>(db: E, team: Option<&str>, id: &str) -> ApiResult<Row> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT id, provider, base_url, account, public_url, token, webhook_secret
           FROM pyo.git_connections WHERE id = $1 AND ($2::text IS NULL OR team_id = $2)",
    )
    .bind(id)
    .bind(team)
    .fetch_optional(db)
    .await?;
    row.ok_or_else(|| ApiError::not_found("Git connection"))
}

async fn opened(row: Row) -> ApiResult<Connection> {
    let (id, provider, base_url, account, public_url, token, secret) = row;
    let v = crate::vault::get();
    Ok(Connection {
        id,
        provider,
        base_url,
        account,
        public_url,
        token: v.open(&token).await?,
        webhook_secret: v.open(&secret).await?,
    })
}

/// "group/sub/project" of a repository URL at `base` (its web address,
/// with or without ".git"). None for anything else: another host, a page
/// inside the repository, a path that steps out.
pub fn repo_path(base: &str, repo_url: &str) -> Option<String> {
    let base = base.trim().trim_end_matches('/');
    let rest = repo_url.trim().trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest
        .get(..base.len())
        .filter(|b| b.eq_ignore_ascii_case(base))
        .and_then(|_| rest[base.len()..].strip_prefix('/'))?;
    let parts: Vec<&str> = rest.split('/').collect();
    let ok = parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "-"
                && !p.starts_with('.')
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        });
    ok.then(|| rest.to_owned())
}

/// Where Pyo is reached, for webhooks and links on commits: what the
/// dashboard says (its own address), else `PYO_PUBLIC_URL`, else its
/// tailnet address, else its public one.
pub fn public_url(given: Option<&str>) -> String {
    given
        .map(|u| u.trim().trim_end_matches('/'))
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .map(str::to_owned)
        .or_else(|| crate::oauth::configured_public_url().map(str::to_owned))
        .or_else(crate::web::public_url)
        .unwrap_or_default()
}

/// Who can reach one of Pyo's addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// Anyone on the internet.
    Public,
    /// Devices on the tailnet.
    Tailnet,
    /// Its own network only (a LAN, this machine).
    Private,
}

/// An IP address's reach: the tailnet's 100.64.0.0/10 is the tailnet.
fn ip_reach(ip: std::net::IpAddr) -> Reach {
    match ip {
        std::net::IpAddr::V4(v4)
            if v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) =>
        {
            Reach::Tailnet
        }
        ip if crate::outbound::is_public_ip(ip) => Reach::Public,
        _ => Reach::Private,
    }
}

/// Who can reach a Pyo address, from its host alone: a tailnet name or
/// address, a private one (an IP on a private network, a name with no
/// dot, .local, .internal, .lan, .home.arpa), a nip.io or sslip.io name as
/// the address it holds; any other name is on the internet.
fn reach_of(url: &str) -> Option<Reach> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url
        .host_str()?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(ip_reach(ip));
    }
    if host.ends_with(".ts.net") {
        return Some(Reach::Tailnet);
    }
    for suffix in [".nip.io", ".sslip.io"] {
        if let Some(rest) = host.strip_suffix(suffix) {
            let labels: Vec<&str> = rest.split('.').collect();
            let dotted = labels[labels.len().saturating_sub(4)..].join(".");
            let dashed = labels
                .last()
                .map(|l| l.rsplit('-').take(4).collect::<Vec<_>>());
            let dashed = dashed.map(|mut p| {
                p.reverse();
                p.join(".")
            });
            if let Some(ip) = [Some(dotted), dashed]
                .into_iter()
                .flatten()
                .find_map(|t| t.parse::<std::net::Ipv4Addr>().ok())
            {
                return Some(ip_reach(ip.into()));
            }
        }
    }
    let private = !host.contains('.')
        || [".local", ".internal", ".lan", ".home.arpa", ".localhost"]
            .iter()
            .any(|s| host.ends_with(s));
    Some(if private {
        Reach::Private
    } else {
        Reach::Public
    })
}

/// The addresses Pyo knows for itself.
#[derive(Debug, Default)]
struct PyoAddresses {
    /// The connection's: what the dashboard was opened at when it was saved.
    given: Option<String>,
    /// `PYO_PUBLIC_URL`.
    configured: Option<String>,
    /// On the tailnet.
    tailnet: Option<String>,
    /// On the server's public IP.
    public: Option<String>,
}

/// Where a provider's webhooks go. One on the internet (gitlab.com) gets
/// only an address on the internet, and none without one: its pushes are
/// checked every minute instead. One on a private network or the tailnet
/// gets Pyo's tailnet address first, then the connection's own.
fn hook_base(provider_public: bool, a: &PyoAddresses) -> Option<String> {
    let all = [&a.given, &a.configured, &a.tailnet, &a.public];
    let with = |want: Reach| {
        all.iter()
            .filter_map(|u| u.as_deref())
            .find(|u| reach_of(u) == Some(want))
            .map(str::to_owned)
    };
    if provider_public {
        with(Reach::Public)
    } else {
        with(Reach::Tailnet)
            .or_else(|| a.given.clone())
            .or_else(|| a.configured.clone())
            .or_else(|| with(Reach::Public))
    }
}

// ── Connections (ops) ────────────────────────────────────────────────────────

async fn seal(team_id: &str, plain: &str) -> ApiResult<String> {
    let sealed = crate::vault::get().seal(team_id, plain).await?;
    if !crate::vault::Vault::is_sealed(&sealed) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "This installation has no master key, so it can't store tokens.",
        ));
    }
    Ok(sealed)
}

/// What a check found, as the API returns it.
fn check_json(c: &gitlab::Check) -> Value {
    json!({ "user": c.user, "account": c.account, "scopes": c.scopes, "expiresAt": c.expires_at, "version": c.version, "projects": c.projects, "problems": c.problems })
}

/// Check a token before it's saved: it has to work and have the api scope.
async fn check_new(provider: &str, base: &str, token: &str) -> ApiResult<gitlab::Check> {
    if token.trim().is_empty() {
        return Err(ApiError::bad_request("Enter the access token."));
    }
    let api = match provider {
        "gitlab" => Gitlab::connect(base, token.trim())
            .await
            .map_err(|e| ApiError::bad_request(e.message))?,
        _ => return Err(ApiError::bad_request("Choose a supported Git provider.")),
    };
    let c = api.check().await.map_err(upstream)?;
    if c.fatal {
        return Err(ApiError::bad_request(c.problems.join(" ")));
    }
    Ok(c)
}

#[allow(clippy::too_many_arguments)]
pub async fn add(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    team_id: &str,
    provider: &str,
    url: &str,
    token: &str,
    given_public_url: Option<&str>,
) -> ApiResult<Value> {
    require_role(tx, user, team_id, Role::Admin).await?;
    if !PROVIDERS.contains(&provider) {
        return Err(ApiError::bad_request("Choose a supported Git provider."));
    }
    let url = if url.trim().is_empty() && provider == "gitlab" {
        "https://gitlab.com"
    } else {
        url
    };
    let base = gitlab::normalize(url)
        .map(|u| gitlab::base_text(&u))
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let c = check_new(provider, &base, token).await?;
    let sealed = seal(team_id, token.trim()).await?;
    let secret = seal(team_id, &secret_token()).await?;
    let done = sqlx::query(
        "INSERT INTO pyo.git_connections (id, team_id, provider, base_url, account, token, webhook_secret, public_url, info)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (id) DO NOTHING",
    )
    .bind(id)
    .bind(team_id)
    .bind(provider)
    .bind(&base)
    .bind(&c.account)
    .bind(&sealed)
    .bind(&secret)
    .bind(public_url(given_public_url))
    .bind(check_json(&c))
    .execute(&mut **tx)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::conflict("That id is taken."));
    }
    let name = provider_name(provider);
    log_activity(
        tx,
        team_id,
        &user.name,
        "integration.saved",
        "success",
        &format!("Connected {name} ({})", c.account),
        json!({ "kind": "integration", "id": id, "label": c.account }),
        json!({}),
    )
    .await?;
    let mut out = json!({ "ok": true, "id": id, "check": check_json(&c) });
    if !c.problems.is_empty() {
        out["warning"] = json!(c.problems.join(" "));
    }
    Ok(out)
}

/// A new token for a connection, and Pyo's address again.
pub async fn replace(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    token: &str,
    given_public_url: Option<&str>,
) -> ApiResult<Value> {
    let team_id = crate::ops::team_of(tx, "git_connections", id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let (provider, base): (String, String) =
        sqlx::query_as("SELECT provider, base_url FROM pyo.git_connections WHERE id = $1")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    let c = check_new(&provider, &base, token).await?;
    let sealed = seal(&team_id, token.trim()).await?;
    sqlx::query(
        "UPDATE pyo.git_connections SET token = $2, account = $3, info = $4, public_url = $5 WHERE id = $1",
    )
    .bind(id)
    .bind(&sealed)
    .bind(&c.account)
    .bind(check_json(&c))
    .bind(public_url(given_public_url))
    .execute(&mut **tx)
    .await?;
    // Webhooks follow a new address.
    queue_sync(&mut **tx, id).await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "integration.saved",
        "info",
        &format!(
            "Replaced the {} token ({})",
            provider_name(&provider),
            c.account
        ),
        json!({ "kind": "integration", "id": id, "label": c.account }),
        json!({}),
    )
    .await?;
    let mut out = json!({ "ok": true, "check": check_json(&c) });
    if !c.problems.is_empty() {
        out["warning"] = json!(c.problems.join(" "));
    }
    Ok(out)
}

/// Check what the saved token can do now: `problems` says what's missing.
pub async fn test(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
) -> ApiResult<Value> {
    let team_id = crate::ops::team_of(tx, "git_connections", id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let conn = load(&s.db, Some(&team_id), id).await?;
    let c = match conn.api().await {
        Ok(api) => api.check().await.map_err(|e| e.message),
        Err(e) => Err(e.message),
    };
    let c = match c {
        Ok(c) => c,
        Err(problem) => {
            return Ok(json!({ "ok": true, "working": false, "problems": [problem] }));
        }
    };
    sqlx::query("UPDATE pyo.git_connections SET info = $2, account = CASE WHEN $3 = '' THEN account ELSE $3 END WHERE id = $1")
        .bind(id)
        .bind(check_json(&c))
        .bind(&c.account)
        .execute(&mut **tx)
        .await?;
    let hooks: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT repo, error FROM pyo.git_webhooks WHERE connection_id = $1 AND hook_id IS NULL ORDER BY repo",
    )
    .bind(id)
    .fetch_all(&mut **tx)
    .await?;
    let mut problems = c.problems.clone();
    let (hook_url, provider_public) = conn.hook_url().await;
    if hook_url.is_none() {
        problems.push(format!(
            "{} So there's no webhook: pushes are checked every minute instead.",
            conn.no_address(provider_public)
        ));
    } else if let Some((repo, error)) = hooks.first() {
        problems.push(format!(
            "No webhook on {repo}, so its pushes are checked every minute instead.{}",
            error
                .as_deref()
                .map(|e| format!(" {e}"))
                .unwrap_or_default()
        ));
    }
    let mut out = check_json(&c);
    out["ok"] = json!(true);
    out["working"] = json!(!c.fatal);
    out["problems"] = json!(problems);
    Ok(out)
}

/// Disconnect: refused while a service builds from it. Its webhooks go too.
pub async fn remove(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
) -> ApiResult<Value> {
    let team_id = crate::ops::team_of(tx, "git_connections", id).await?;
    require_role(tx, user, &team_id, Role::Admin).await?;
    let used: Option<String> = sqlx::query_scalar(
        "SELECT name FROM pyo.services WHERE team_id = $1 AND NOT deleting AND spec->'source'->>'gitConnectionId' = $2 LIMIT 1",
    )
    .bind(&team_id)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(service) = used {
        return Err(ApiError::conflict(format!(
            "{service} deploys from this connection. Change its source first."
        )));
    }
    let conn = load(&s.db, Some(&team_id), id).await?;
    remove_hooks(tx, &conn).await?;
    sqlx::query("DELETE FROM pyo.git_connections WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;
    log_activity(
        tx,
        &team_id,
        &user.name,
        "integration.removed",
        "info",
        &format!("Disconnected {} ({})", conn.name(), conn.account),
        json!({ "kind": "integration", "id": id, "label": conn.account }),
        json!({}),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// Remove the webhooks Pyo made for a connection that's going. Best
/// effort: one GitLab can't reach or refuses is logged and left.
async fn remove_hooks(tx: &mut Transaction<'_, Postgres>, conn: &Connection) -> ApiResult<()> {
    let hooks: Vec<(String, i64)> = sqlx::query_as(
        "SELECT repo, hook_id FROM pyo.git_webhooks WHERE connection_id = $1 AND hook_id IS NOT NULL",
    )
    .bind(&conn.id)
    .fetch_all(&mut **tx)
    .await?;
    if hooks.is_empty() {
        return Ok(());
    }
    let failed = match conn.api().await {
        Ok(api) => api.delete_hooks(&hooks).await,
        Err(e) => hooks
            .iter()
            .map(|(r, _)| {
                let e = gitlab::Error {
                    status: None,
                    message: e.message.clone(),
                };
                (r.clone(), e)
            })
            .collect(),
    };
    for (repo, e) in failed {
        tracing::warn!(connection = %conn.id, %repo, error = %e, "webhook not removed");
    }
    Ok(())
}

/// Before a team is deleted (its connections go with it): remove the
/// webhooks they made, best effort.
pub async fn forget_team(
    s: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
) -> ApiResult<()> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM pyo.git_connections WHERE team_id = $1")
            .bind(team_id)
            .fetch_all(&mut **tx)
            .await?;
    for id in ids {
        match load(&s.db, Some(team_id), &id).await {
            Ok(conn) => remove_hooks(tx, &conn).await?,
            Err(e) => {
                tracing::warn!(connection = %id, error = %e.message, "webhooks not removed")
            }
        }
    }
    Ok(())
}

/// A service's source, when it names a connection: the team's, a
/// repository of it, and not a GitHub one at the same time.
pub async fn source_ok(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    spec: &Value,
) -> ApiResult<()> {
    let src = &spec["source"];
    let Some(id) = src["gitConnectionId"].as_str().filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    if src["installationId"].is_i64() {
        return Err(ApiError::bad_request(
            "A source comes from GitHub or another Git connection, not both.",
        ));
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT base_url, provider FROM pyo.git_connections WHERE id = $1 AND team_id = $2",
    )
    .bind(id)
    .bind(team_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((base, provider)) = row else {
        return Err(ApiError::bad_request(
            "That Git connection isn't this team's. Add one in this team's Integrations.",
        ));
    };
    if repo_path(&base, src["repoUrl"].as_str().unwrap_or("")).is_none() {
        return Err(ApiError::bad_request(format!(
            "Repositories from this {} connection use their {base}/group/project URL.",
            provider_name(&provider)
        )));
    }
    Ok(())
}

/// The connections a source names, before and after a change: their
/// webhooks are brought in line once it commits.
pub async fn queue_sync_for(
    tx: &mut Transaction<'_, Postgres>,
    sources: &[&Value],
) -> sqlx::Result<()> {
    let mut ids: Vec<&str> = sources
        .iter()
        .filter_map(|s| s["gitConnectionId"].as_str())
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    for id in ids {
        queue_sync(&mut **tx, id).await?;
    }
    Ok(())
}

/// Every connection of a team (a project's services are going).
pub async fn queue_sync_team(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "WITH j AS (INSERT INTO pyo.jobs (kind, payload) SELECT 'git.hooks', jsonb_build_object('connectionId', id) FROM pyo.git_connections WHERE team_id = $1 RETURNING id)
         SELECT pg_notify('pyo_jobs', '') FROM j LIMIT 1",
    )
    .bind(team_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn queue_sync<'e>(db: impl sqlx::PgExecutor<'e>, id: &str) -> sqlx::Result<()> {
    crate::jobs::enqueue(db, "git.hooks", json!({ "connectionId": id })).await
}

// ── Reads (API) ──────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct ReposQuery {
    /// The team's id.
    team: String,
    /// Only repositories whose path contains this.
    #[serde(default)]
    search: Option<String>,
}

/// Git repositories
///
/// The repositories the team's Git connections (GitLab) can deploy: those
/// their token has the Maintainer role on, latest activity first.
#[utoipa::path(get, path = "/git/repos", operation_id = "listGitRepos", tag = "Git providers",
    params(ReposQuery),
    responses(
        (status = 200, description = "`repositories`: each with `connectionId`, `provider`, `host`, `fullName`, `name`, `group`, `private`, `defaultBranch`, `htmlUrl` and `pushedAt`. `errors`: connections that couldn't be listed, with `connectionId` and `message`.", body = Object),
        (status = 400, description = "No `team`.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team, or you aren't in it.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn repos(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<ReposQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = crate::rls::begin(&s.db, &user).await?;
    require_role(&mut tx, &user, &q.team, Role::Viewer).await?;
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM pyo.git_connections WHERE team_id = $1 ORDER BY created_at",
    )
    .bind(&q.team)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let (mut out, mut errors) = (vec![], vec![]);
    for id in ids {
        let conn = load_for(&s.db, &user, &q.team, &id).await?;
        let listed = match conn.api().await {
            Ok(api) => api
                .projects(q.search.as_deref())
                .await
                .map_err(|e| e.message),
            Err(e) => Err(e.message),
        };
        match listed {
            Ok(list) => {
                let host = reqwest::Url::parse(&conn.base_url)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_default();
                out.extend(list.into_iter().map(|p| json!({
                    "connectionId": conn.id, "provider": conn.provider, "host": host,
                    "fullName": p.path, "name": p.name, "group": p.group, "private": p.private,
                    "defaultBranch": p.default_branch, "htmlUrl": p.web_url, "pushedAt": p.last_activity_at,
                })));
            }
            Err(message) => errors.push(json!({ "connectionId": conn.id, "message": message })),
        }
    }
    Ok(Json(json!({ "repositories": out, "errors": errors })))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct BranchesQuery {
    /// The team's id.
    team: String,
    /// The Git connection's id.
    connection: String,
    /// The repository: its path ("group/project") or URL.
    repo: String,
    /// Only branches whose name contains this.
    #[serde(default)]
    search: Option<String>,
}

/// The repository's path from a path or a URL of the connection.
fn repo_arg(conn: &Connection, repo: &str) -> ApiResult<String> {
    let repo = repo.trim();
    conn.repo_of(repo)
        .or_else(|| repo_path(&conn.base_url, &format!("{}/{repo}", conn.base_url)))
        .ok_or_else(|| ApiError::bad_request("Give the repository as group/project, or its URL."))
}

/// Git branches
///
/// A repository's branches, through one of the team's Git connections.
#[utoipa::path(get, path = "/git/branches", operation_id = "listGitBranches", tag = "Git providers",
    params(BranchesQuery),
    responses(
        (status = 200, description = "`branches`: each with `name`, `commit`, `default` and `protected`.", body = Object),
        (status = 400, description = "A missing or invalid field.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team or connection, or you aren't in the team.", body = crate::error::ErrorBody),
        (status = 502, description = "The provider couldn't be reached or refused.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn branches(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<BranchesQuery>,
) -> ApiResult<Json<Value>> {
    let conn = load_for(&s.db, &user, &q.team, &q.connection).await?;
    let repo = repo_arg(&conn, &q.repo)?;
    let list = conn
        .api()
        .await?
        .branches(&repo, q.search.as_deref())
        .await
        .map_err(upstream)?;
    Ok(Json(json!({
        "branches": list.iter().map(|b| json!({ "name": b.name, "commit": b.commit, "default": b.default, "protected": b.protected })).collect::<Vec<_>>()
    })))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(crate) struct BuildFileQuery {
    /// The team's id.
    team: String,
    /// The Git connection's id.
    connection: String,
    /// The repository: its path ("group/project") or URL.
    repo: String,
    /// The branch to look at.
    branch: String,
    /// The build context, a folder of the repository ("." for its root).
    #[serde(default)]
    context: String,
}

/// Git build file
///
/// The Dockerfile a build of a repository of the team's Git connections
/// would use by default, in the build context. Null: the build is
/// zero-config (Railpack).
#[utoipa::path(get, path = "/git/build-file", operation_id = "getGitBuildFile", tag = "Git providers",
    params(BuildFileQuery),
    responses(
        (status = 200, description = "`file`: the Dockerfile Pyo would use, or null when there's none.", body = Object),
        (status = 400, description = "A missing or invalid field.", body = crate::error::ErrorBody),
        (status = 404, description = "No such team or connection, or you aren't in the team.", body = crate::error::ErrorBody),
        (status = 502, description = "The provider couldn't be reached or refused.", body = crate::error::ErrorBody),
    ),
    extensions(("x-pyo-access" = json!("read"))))]
pub(crate) async fn build_file(
    State(s): State<AppState>,
    SessionUser(user): SessionUser,
    Query(q): Query<BuildFileQuery>,
) -> ApiResult<Json<Value>> {
    let conn = load_for(&s.db, &user, &q.team, &q.connection).await?;
    let repo = repo_arg(&conn, &q.repo)?;
    let dir = crate::github::build_dir(&q.context)
        .ok_or_else(|| ApiError::bad_request("Build context can't contain '..'."))?;
    if q.branch.trim().is_empty() {
        return Err(ApiError::bad_request("Give a branch."));
    }
    let api = conn.api().await?;
    for f in pyo_proto::BUILD_FILES {
        if api
            .file_exists(&repo, q.branch.trim(), &format!("{dir}{f}"))
            .await
            .map_err(upstream)?
        {
            return Ok(Json(json!({ "file": f })));
        }
    }
    Ok(Json(json!({ "file": null })))
}

// ── Builds ───────────────────────────────────────────────────────────────────

/// Get a claimed build ready to clone: a short-lived credential for a
/// private repository (never stored), the branch's commit when the build
/// follows a branch, and "running" on the commit.
pub async fn prepare_build(s: &AppState, job: &mut pyo_proto::BuildJob) {
    if let Some(inst) = job.github_installation_id {
        // Private repositories: clone with a short-lived token, and
        // resolve the branch through the API (git ls-remote can't).
        match crate::github::installation_token(s, inst).await {
            Ok(tok) => job.clone_token = Some(tok),
            Err(e) => {
                tracing::warn!(job = %job.id, error = %format!("{e:#}"), "no GitHub token for build")
            }
        }
        if job.commit.is_empty()
            && let Some(repo) = crate::github::full_name(&job.repo_url)
            && let Ok(sha) = crate::github::branch_head(s, inst, &repo, &job.branch).await
        {
            job.commit = sha;
        }
    } else if let Some(id) = job.git_connection_id.clone() {
        if let Err(e) = prepare_connection_build(s, &id, job).await {
            tracing::warn!(job = %job.id, error = %e.message, "no clone token for build");
        }
    } else {
        return;
    }
    crate::jobs::enqueue_now(
        s,
        "git.status",
        crate::jobs::git_status(&job.deployment_id, "running", "Building…"),
    )
    .await;
}

async fn prepare_connection_build(
    s: &AppState,
    id: &str,
    job: &mut pyo_proto::BuildJob,
) -> ApiResult<()> {
    let conn = load(&s.db, None, id).await?;
    let repo = conn
        .repo_of(&job.repo_url)
        .ok_or_else(|| ApiError::bad_request("The repository isn't one of the connection's."))?;
    let api = conn.api().await?;
    if job.commit.is_empty()
        && let Ok(sha) = api.branch_head(&repo, &job.branch).await
    {
        job.commit = sha;
    }
    let expires = jiff::Timestamp::now()
        .checked_add(jiff::SignedDuration::from_mins(CLONE_TOKEN_MINUTES))
        .map_err(anyhow::Error::from)?;
    let token = api
        .create_deploy_token(
            &repo,
            &format!("pyo-build-{}", job.id),
            &expires.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        )
        .await
        .map_err(upstream)?;
    // Revoked when the build ends (`after_build`): its id, never the token.
    sqlx::query("UPDATE pyo.build_jobs SET job = job || jsonb_build_object('cloneTokenId', $2::bigint) WHERE id = $1")
        .bind(&job.id)
        .bind(token.id as i64)
        .execute(&s.db)
        .await?;
    if !job.repo_url.ends_with(".git") {
        job.repo_url = format!("{}.git", job.repo_url.trim_end_matches('/'));
    }
    job.clone_username = Some(token.username);
    job.clone_token = Some(token.token);
    Ok(())
}

/// When a build ends: revoke the deploy token it cloned with.
pub async fn after_build(s: &AppState, job_id: &str) {
    let row: Result<Option<(Option<String>, Option<i64>)>, _> = sqlx::query_as(
        "SELECT job->>'gitConnectionId', (job->>'cloneTokenId')::bigint FROM pyo.build_jobs WHERE id = $1",
    )
    .bind(job_id)
    .fetch_optional(&s.db)
    .await;
    if let Ok(Some((Some(conn), Some(token)))) = row {
        let repo: Option<String> =
            sqlx::query_scalar("SELECT job->>'repoUrl' FROM pyo.build_jobs WHERE id = $1")
                .bind(job_id)
                .fetch_optional(&s.db)
                .await
                .ok()
                .flatten();
        crate::jobs::enqueue_now(
            s,
            "git.revoke",
            json!({ "connectionId": conn, "repoUrl": repo, "tokenId": token }),
        )
        .await;
    }
}

/// Revoke a build's deploy token (the "git.revoke" job).
pub async fn revoke(s: &AppState, p: &Value) -> anyhow::Result<()> {
    let (Some(id), Some(url), Some(token)) = (
        p["connectionId"].as_str(),
        p["repoUrl"].as_str(),
        p["tokenId"].as_u64(),
    ) else {
        return Ok(());
    };
    let conn = match load(&s.db, None, id).await {
        Ok(c) => c,
        // Disconnected: nothing Pyo can do (it expires on its own).
        Err(e) if e.status == StatusCode::NOT_FOUND => return Ok(()),
        Err(e) => anyhow::bail!("{}", e.message),
    };
    let Some(repo) = conn.repo_of(url) else {
        return Ok(());
    };
    let api = conn
        .api()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    api.delete_deploy_token(&repo, token).await?;
    Ok(())
}

// ── Commit statuses ──────────────────────────────────────────────────────────

/// Whether a deployment's source reports on its commit: a GitHub App
/// repository, or one of a Git connection.
pub fn reports_status(source: &Value) -> bool {
    source["installationId"].is_i64()
        || source["gitConnectionId"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
}

/// Report a deployment on its commit (the "git.status" job). `state`:
/// "pending", "running", "success", "failure" or "error" (cancelled).
pub async fn report(
    s: &AppState,
    deployment_id: &str,
    state: &str,
    description: &str,
) -> anyhow::Result<()> {
    let row: Option<(Value, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT d.data, t.slug, p.namespace, sv.name FROM pyo.deployments d JOIN pyo.teams t ON t.id = d.team_id
           LEFT JOIN pyo.services sv ON sv.id = d.service_id LEFT JOIN pyo.projects p ON p.id = sv.project_id
          WHERE d.id = $1",
    )
    .bind(deployment_id)
    .fetch_optional(&s.db)
    .await?;
    let Some((data, slug, namespace, service)) = row else {
        return Ok(());
    };
    // Its page on the dashboard: "/acme/shop/web/deployments/dpl_1".
    let page = format!(
        "/{slug}{}",
        crate::deploy::deployment_page(namespace.as_deref(), service.as_deref(), deployment_id)
    );
    // A preview's deployment: its pull request's bot says it instead.
    if crate::previews::deployment_changed(s, deployment_id).await? {
        return Ok(());
    }
    let src = &data["source"];
    if src["installationId"].is_i64() {
        return crate::github::report(s, &data, &page, state, description).await;
    }
    let (Some(id), Some(sha)) = (
        src["gitConnectionId"].as_str(),
        src["commit"].as_str().filter(|c| c.len() == 40),
    ) else {
        return Ok(());
    };
    let conn = match load(&s.db, None, id).await {
        Ok(c) => c,
        Err(e) if e.status == StatusCode::NOT_FOUND => return Ok(()),
        Err(e) => anyhow::bail!("{}", e.message),
    };
    let Some(repo) = conn.repo_of(src["repoUrl"].as_str().unwrap_or("")) else {
        return Ok(());
    };
    let link = match public_url(Some(&conn.public_url)) {
        u if u.is_empty() => String::new(),
        u => format!("{u}{page}"),
    };
    let api = conn
        .api()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    api.set_status(&repo, sha, gitlab::status_state(state), description, &link)
        .await?;
    Ok(())
}

// ── Pushes ───────────────────────────────────────────────────────────────────

/// Which services a push is for.
pub enum Follow<'a> {
    /// GitHub App repositories named "owner/repo".
    Github(&'a str),
    /// A Git connection's repository.
    Connection { conn: &'a Connection, repo: &'a str },
}

/// Deploy `sha` to every service that follows the repository at `branch`
/// with deploy on push. Skips services pinned to a commit, rolled back,
/// already on `sha`, or mid-deploy (the poller picks those up once they're
/// free).
pub async fn deploy_push(
    s: &AppState,
    follow: Follow<'_>,
    branch: &str,
    sha: &str,
    actor: &str,
) -> ApiResult<usize> {
    let connection = match &follow {
        Follow::Github(_) => None,
        Follow::Connection { conn, .. } => Some(conn.id.as_str()),
    };
    let services: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT a.id, a.team_id, a.spec->'source' FROM pyo.services a
          WHERE NOT a.deleting AND a.preview_of IS NULL AND a.spec->'source'->>'type' = 'git'
            AND CASE WHEN $2::text IS NULL THEN a.spec->'source'->>'installationId' IS NOT NULL
                     ELSE a.spec->'source'->>'gitConnectionId' = $2 END
            AND coalesce(a.spec->'source'->>'branch', 'main') = $1 AND coalesce((a.spec->'source'->>'autoDeploy')::bool, true)
            AND coalesce(a.spec->'source'->>'commit', '') = ''",
    )
    .bind(branch)
    .bind(connection)
    .fetch_all(&s.db)
    .await?;
    let mut started = 0;
    for (service_id, team_id, src) in services {
        let url = src["repoUrl"].as_str().unwrap_or("");
        let same = match &follow {
            Follow::Github(repo) => {
                crate::github::full_name(url).is_some_and(|r| r.eq_ignore_ascii_case(repo))
            }
            Follow::Connection { conn, repo } => conn
                .repo_of(url)
                .is_some_and(|r| r.eq_ignore_ascii_case(repo)),
        };
        if !same {
            continue;
        }
        let mut tx = s.db.begin().await?;
        // Off the newest deploy: what it built, whether it's a rollback, whether it's running.
        let last: Option<(String, Value)> = sqlx::query_as("SELECT status, data FROM pyo.deployments WHERE service_id = $1 ORDER BY created_at DESC LIMIT 1").bind(&service_id).fetch_optional(&mut *tx).await?;
        let current_trigger: Option<String> = sqlx::query_scalar("SELECT d.data->>'trigger' FROM pyo.services a JOIN pyo.deployments d ON d.id = a.current_deployment_id WHERE a.id = $1").bind(&service_id).fetch_optional(&mut *tx).await?.flatten();
        if let Some((status, data)) = &last
            && (data["source"]["commit"].as_str() == Some(sha) || !deploy::is_final(status))
        {
            continue;
        }
        if current_trigger.as_deref() == Some("rollback") {
            // Rolled back on purpose: wait for someone to deploy again.
            continue;
        }
        let dep = new_id("dpl");
        match deploy::create(
            &mut tx,
            DeployRequest {
                deployment_id: &dep,
                service_id: &service_id,
                trigger: "git_push",
                actor,
                rollback_of: None,
                retry_of: None,
                commit: Some(sha),
                upload: None,
            },
        )
        .await
        {
            Ok(_) => {
                log_activity(
                    &mut tx,
                    &team_id,
                    actor,
                    "deployment.started",
                    "info",
                    &format!(
                        "Push to {branch} ({}) is deploying",
                        &sha[..sha.len().min(7)]
                    ),
                    json!({ "kind": "deployment", "id": dep, "label": dep }),
                    json!({ "serviceId": service_id }),
                )
                .await?;
                crate::jobs::enqueue(
                    &mut *tx,
                    "git.status",
                    crate::jobs::git_status(&dep, "pending", "Waiting for a builder…"),
                )
                .await?;
                tx.commit().await?;
                s.wake(&format!("builders:{team_id}")).await;
                started += 1;
            }
            Err(e) => {
                tracing::info!(service = %service_id, reason = %e.message, "push not deployed")
            }
        }
    }
    Ok(started)
}

/// A connection's webhook: a push deploys the services that follow it.
async fn webhook(
    State(s): State<AppState>,
    crate::ratelimit::ClientIp(ip): crate::ratelimit::ClientIp,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<impl IntoResponse> {
    let wrong = || async {
        crate::ratelimit::take(&s.db, &[(crate::ratelimit::GIT_WEBHOOK_WRONG_IP, &ip)]).await
    };
    let conn = match load(&s.db, None, &id).await {
        Ok(c) => c,
        Err(e) if e.status == StatusCode::NOT_FOUND => {
            wrong().await?;
            return Ok((StatusCode::NOT_FOUND, "no such connection"));
        }
        Err(e) => return Err(e),
    };
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    if !gitlab::token_ok(&conn.webhook_secret, header("x-gitlab-token")) {
        wrong().await?;
        return Ok((StatusCode::UNAUTHORIZED, "bad token"));
    }
    // Per connection: a push storm can't queue builds without end.
    crate::ratelimit::take(
        &s.db,
        &[(crate::ratelimit::GIT_WEBHOOK_CALLS, &format!("gitlab:{}", conn.id))],
    )
    .await?;
    let Ok(v) = serde_json::from_slice::<Value>(&body) else {
        return Ok((StatusCode::BAD_REQUEST, "not JSON"));
    };
    // A merge request: its previews (previews/gitlab.rs).
    if let Some(mr) = crate::previews::gitlab::pull_of_hook(header("x-gitlab-event"), &v, &conn) {
        let team: String =
            sqlx::query_scalar("SELECT team_id FROM pyo.git_connections WHERE id = $1")
                .bind(&conn.id)
                .fetch_one(&s.db)
                .await?;
        let followed = crate::previews::pull_event(&s, &team, &mr).await?;
        return Ok((
            StatusCode::OK,
            if followed { "previews" } else { "no previews" },
        ));
    }
    let Some(push) = gitlab::push_of(header("x-gitlab-event"), &v) else {
        return Ok((StatusCode::OK, "ignored"));
    };
    let n = deploy_push(
        &s,
        Follow::Connection {
            conn: &conn,
            repo: &push.repo,
        },
        &push.branch,
        &push.sha,
        &push.actor,
    )
    .await?;
    Ok((
        StatusCode::OK,
        if n > 0 {
            "deploying"
        } else {
            "no matching services"
        },
    ))
}

// ── Webhooks on the provider ─────────────────────────────────────────────────

/// Bring a connection's webhooks in line with the repositories its services
/// follow (the "git.hooks" job): add what's missing, remove what nothing
/// follows any more, and remake hooks whose address changed.
pub async fn sync_hooks(s: &AppState, id: &str) -> anyhow::Result<()> {
    let conn = match load(&s.db, None, id).await {
        Ok(c) => c,
        Err(e) if e.status == StatusCode::NOT_FOUND => return Ok(()),
        Err(e) => anyhow::bail!("{}", e.message),
    };
    // One sync of a connection at a time, across workers.
    let mut tx = s.db.begin().await?;
    sqlx::query("SELECT 1 FROM pyo.git_connections WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let urls: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT spec->'source'->>'repoUrl' FROM pyo.services
          WHERE NOT deleting AND spec->'source'->>'gitConnectionId' = $1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let mut wanted: Vec<String> = urls.iter().filter_map(|u| conn.repo_of(u)).collect();
    wanted.sort_by_key(|r| r.to_lowercase());
    wanted.dedup_by_key(|r| r.to_lowercase());
    let rows: Vec<(String, Option<i64>, String, bool)> = sqlx::query_as(
        "SELECT repo, hook_id, url, merge_requests FROM pyo.git_webhooks WHERE connection_id = $1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    // Hooks made before previews send pushes only: they're made again.
    let old_hooks: Vec<String> = rows
        .iter()
        .filter(|r| r.1.is_some() && !r.3)
        .map(|r| r.0.clone())
        .collect();
    let have: Vec<(String, Option<i64>, String)> =
        rows.into_iter().map(|(r, h, u, _)| (r, h, u)).collect();
    let api = conn
        .api()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let (hook_url, provider_public) = conn.hook_url().await;
    let mut failed = None;
    for (repo, hook, _) in &have {
        if wanted.iter().any(|w| w.eq_ignore_ascii_case(repo)) {
            continue;
        }
        if let Some(h) = hook
            && let Err(e) = api.delete_hook(repo, *h as u64).await
        {
            failed = Some(format!("{repo}: {e}"));
            continue;
        }
        sqlx::query("DELETE FROM pyo.git_webhooks WHERE connection_id = $1 AND repo = $2")
            .bind(id)
            .bind(repo)
            .execute(&mut *tx)
            .await?;
    }
    for repo in &wanted {
        let key = repo.to_lowercase();
        let saved = have.iter().find(|(r, _, _)| *r == key);
        if let Some((_, Some(_), url)) = saved
            && Some(url) == hook_url.as_ref()
            && !old_hooks.contains(&key)
        {
            continue;
        }
        let (hook, error) = match &hook_url {
            None => {
                // Polled instead: the hook Pyo made before goes.
                if let Some((_, Some(old), _)) = saved
                    && let Err(e) = api.delete_hook(repo, *old as u64).await
                {
                    failed = Some(format!("{repo}: {e}"));
                    continue;
                }
                (None, Some(conn.no_address(provider_public)))
            }
            Some(url) => match make_hook(&api, repo, url, &conn.webhook_secret, saved).await {
                Ok(h) => (Some(h as i64), None),
                Err(e) => (None, Some(e.message)),
            },
        };
        sqlx::query(
            "INSERT INTO pyo.git_webhooks (connection_id, repo, hook_id, url, error, merge_requests, updated_at) VALUES ($1, $2, $3, $4, $5, $3 IS NOT NULL, now())
             ON CONFLICT (connection_id, repo) DO UPDATE SET hook_id = $3, url = $4, error = $5, merge_requests = $3 IS NOT NULL, updated_at = now()",
        )
        .bind(id)
        .bind(&key)
        .bind(hook)
        .bind(hook_url.clone().unwrap_or_default())
        .bind(error)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if let Some(f) = failed {
        anyhow::bail!("a webhook couldn't be removed ({f})");
    }
    Ok(())
}

/// Remove the hook Pyo made before (and any other to the same address, left
/// from before), then make one with the connection's secret.
async fn make_hook(
    api: &Gitlab,
    repo: &str,
    url: &str,
    secret: &str,
    saved: Option<&(String, Option<i64>, String)>,
) -> gitlab::Result<u64> {
    if let Some((_, Some(old), _)) = saved {
        api.delete_hook(repo, *old as u64).await?;
    }
    for (h, u) in api.hooks(repo).await? {
        if u == url {
            api.delete_hook(repo, h).await?;
        }
    }
    api.create_hook(repo, url, secret).await
}

/// Each minute: check the branches no webhook covers. Every 15 minutes:
/// bring every connection's webhooks in line (and retry those that failed).
pub async fn poller(s: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        let sync = n.is_multiple_of(15);
        n += 1;
        crate::bg::as_leader(&s.db, crate::bg::LOCK_GIT, || poll_once(&s, sync)).await;
    }
}

async fn poll_once(s: &AppState, sync: bool) {
    if sync {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM pyo.git_connections")
            .fetch_all(&s.db)
            .await
            .unwrap_or_default();
        for id in ids {
            crate::jobs::enqueue_now(s, "git.hooks", json!({ "connectionId": id })).await;
        }
    }
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT DISTINCT a.spec->'source'->>'gitConnectionId', a.spec->'source'->>'repoUrl', coalesce(a.spec->'source'->>'branch', 'main')
           FROM pyo.services a
          WHERE NOT a.deleting AND a.current_deployment_id IS NOT NULL AND a.spec->'source'->>'gitConnectionId' IS NOT NULL
            AND coalesce((a.spec->'source'->>'autoDeploy')::bool, true) AND coalesce(a.spec->'source'->>'commit', '') = ''",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    let hooked: Vec<(String, String)> = sqlx::query_as(
        "SELECT connection_id, repo FROM pyo.git_webhooks WHERE hook_id IS NOT NULL",
    )
    .fetch_all(&s.db)
    .await
    .unwrap_or_default();
    let mut conns: Vec<Connection> = vec![];
    for (id, url, branch) in rows {
        if !conns.iter().any(|c| c.id == id) {
            match load(&s.db, None, &id).await {
                Ok(c) => conns.push(c),
                Err(_) => continue,
            }
        }
        let Some(conn) = conns.iter().find(|c| c.id == id) else {
            continue;
        };
        let Some(repo) = conn.repo_of(&url) else {
            continue;
        };
        if hooked
            .iter()
            .any(|(c, r)| *c == id && r.eq_ignore_ascii_case(&repo))
        {
            continue;
        }
        let head = match conn.api().await {
            Ok(api) => api.branch_head(&repo, &branch).await.map_err(|e| e.message),
            Err(e) => Err(e.message),
        };
        match head {
            Ok(sha) => {
                let follow = Follow::Connection { conn, repo: &repo };
                if let Err(e) = deploy_push(s, follow, &branch, &sha, conn.name()).await {
                    tracing::warn!(%repo, error = ?e.message, "push poll");
                }
            }
            Err(e) => tracing::debug!(%repo, error = %e, "branch head"),
        }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/{id}/webhook", post(webhook))
}

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/git/repos", get(repos))
        .route("/git/branches", get(branches))
        .route("/git/build-file", get(build_file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_paths() {
        let b = "https://gitlab.example.com/gitlab";
        assert_eq!(
            repo_path(b, "https://gitlab.example.com/gitlab/acme/web/shop").as_deref(),
            Some("acme/web/shop")
        );
        assert_eq!(
            repo_path(b, "https://GitLab.example.com/gitlab/acme/shop.git/").as_deref(),
            Some("acme/shop")
        );
        for bad in [
            "https://gitlab.example.com/gitlab/acme",
            "https://gitlab.example.com/acme/shop",
            "https://gitlab.example.com/gitlabx/acme/shop",
            "https://gitlab.example.com/gitlab/acme/shop/-/tree/main",
            "https://gitlab.example.com/gitlab/acme/../shop",
            "https://gitlab.example.com/gitlab/acme//shop",
            "https://gitlab.example.com/gitlab/acme/sh op",
            "https://evil.example.com/gitlab/acme/shop",
        ] {
            assert_eq!(repo_path(b, bad), None, "{bad}");
        }
        assert_eq!(
            repo_path("https://gitlab.com", "https://gitlab.com/a/b").as_deref(),
            Some("a/b")
        );
    }

    #[test]
    fn statuses_for_connection_sources() {
        assert!(reports_status(&json!({ "installationId": 7 })));
        assert!(reports_status(&json!({ "gitConnectionId": "gitc_abcd" })));
        assert!(!reports_status(&json!({ "gitConnectionId": "" })));
        assert!(!reports_status(
            &json!({ "repoUrl": "https://gitlab.com/a/b" })
        ));
    }

    #[test]
    fn who_reaches_an_address() {
        for (url, want) in [
            ("https://pyo.example.com", Reach::Public),
            ("https://203.0.113.9:8443", Reach::Public),
            ("https://pyo.tail1234.ts.net", Reach::Tailnet),
            ("https://100.64.0.103", Reach::Tailnet),
            ("http://192.168.1.20:8000", Reach::Private),
            ("http://localhost:8000", Reach::Private),
            ("https://pyo.lan", Reach::Private),
            ("https://pyo.203.0.113.9.nip.io", Reach::Public),
            ("https://pyo-10-0-0-5.sslip.io", Reach::Private),
        ] {
            assert_eq!(reach_of(url), Some(want), "{url}");
        }
    }

    #[test]
    fn webhooks_go_where_the_provider_reaches() {
        let all = PyoAddresses {
            given: Some("https://pyo.tail1234.ts.net".into()),
            configured: None,
            tailnet: Some("https://pyo.tail1234.ts.net".into()),
            public: Some("https://pyo.203.0.113.9.nip.io".into()),
        };
        // gitlab.com: the public address, even when the dashboard was
        // opened on the tailnet.
        assert_eq!(
            hook_base(true, &all).as_deref(),
            Some("https://pyo.203.0.113.9.nip.io")
        );
        // A GitLab on the LAN or the tailnet: the tailnet address.
        assert_eq!(
            hook_base(false, &all).as_deref(),
            Some("https://pyo.tail1234.ts.net")
        );
        // gitlab.com and only private addresses: no webhook (polled).
        let private = PyoAddresses {
            given: Some("http://192.168.1.20:8000".into()),
            tailnet: Some("https://pyo.tail1234.ts.net".into()),
            ..Default::default()
        };
        assert_eq!(hook_base(true, &private), None);
        // A GitLab on the LAN with no tailnet: the address it was saved with.
        let lan = PyoAddresses {
            given: Some("http://192.168.1.20:8000".into()),
            ..Default::default()
        };
        assert_eq!(
            hook_base(false, &lan).as_deref(),
            Some("http://192.168.1.20:8000")
        );
        // PYO_PUBLIC_URL counts as public.
        let configured = PyoAddresses {
            configured: Some("https://pyo.example.com".into()),
            ..Default::default()
        };
        assert_eq!(
            hook_base(true, &configured).as_deref(),
            Some("https://pyo.example.com")
        );
        assert_eq!(hook_base(false, &PyoAddresses::default()), None);
    }

    #[test]
    fn public_address() {
        assert_eq!(
            public_url(Some("https://pyo.example.com/")),
            "https://pyo.example.com"
        );
        // Anything else falls back to what the installation knows.
        assert_eq!(public_url(Some("javascript:x")), public_url(None));
    }
}
