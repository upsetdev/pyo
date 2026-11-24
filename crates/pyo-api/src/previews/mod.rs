//! Preview environments: each open pull request (GitHub) or merge request
//! (GitLab) gets a copy of the services that turned previews on, built
//! from its head, and loses it when it closes or merges.
//!
//! - **A copy is a service** (`pyo.services.preview_of`), next to the one
//!   it previews: same project, namespace and cluster, its own name
//!   (`web-pr-42`), env, routes and deployments. Builds, deploys (signed
//!   agent commands on Pyo Cloud), logs and removal are the ones every
//!   service has. The dashboard lists copies under their service only.
//! - **What runs** ([`reconcile`]): a pull request's previews are brought
//!   in line with it and with each service's settings (`spec.previews`,
//!   [`Settings`]) whenever something changes: a webhook, a poll of the
//!   provider ([`runner`]), a deployment that settled, a setting saved.
//!   It decides per service ([`decide`]): run (start, redeploy the new
//!   head), wait (over the service's limit) or remove (closed, merged,
//!   idle, label gone, fork, previews off).
//! - **URLs** ([`domain`], [`naming`]): `<service>-pr-<n>` on the
//!   cluster's preview domain, else nip.io on its address, or its tailnet
//!   for private previews. Kept for the pull request's life.
//! - **The bot** ([`report`], [`bot`], [`github`], [`gitlab`]): one comment
//!   edited in place, a check (GitHub) or commit status (GitLab), and a
//!   deployment per preview, through the GitHub App or the GitLab
//!   connection. A job per change, idempotent: it says only what changed.
//! - **Forks** are not deployed unless the service allows it, and never
//!   with secrets unless it allows that too.

pub mod bot;
pub mod domain;
pub mod github;
pub mod gitlab;
pub mod naming;
#[cfg(test)]
mod tests;

use std::time::Duration;

use pyo_proto::{Command, now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};

use crate::{
    AppState,
    auth::User,
    deploy::{self, DeployRequest},
    error::{ApiError, ApiResult},
    ids::new_id,
    ops::{Role, log_activity, require_role},
};

use bot::Shown;

/// Advisory lock of the previews loop ([`runner`]).
pub const LOCK: i64 = 7_071;

// ── Settings ───────────────────────────────────────────────────────────────

/// A service's previews (`spec.previews`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub enabled: bool,
    /// "all", or "label": only pull requests with `label`.
    pub pulls: String,
    pub label: String,
    /// Previews of the service at once.
    pub limit: i64,
    /// Removed after this many days without a push (0: never).
    pub idle_days: i64,
    /// "public", or "private" (on the cluster's tailnet).
    pub access: String,
    /// Copy the service's secret variables.
    pub copy_secrets: bool,
    /// Pull requests from forks: "off", "no-secrets" or "secrets".
    pub forks: String,
    /// Variables for previews: they replace the service's by key, or add.
    pub env: Vec<Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            pulls: "all".into(),
            label: "preview".into(),
            limit: 5,
            idle_days: 0,
            access: "public".into(),
            copy_secrets: false,
            forks: "off".into(),
            env: vec![],
        }
    }
}

impl Settings {
    pub fn of(spec: &Value) -> Settings {
        serde_json::from_value(spec["previews"].clone()).unwrap_or_default()
    }
}

/// A variable's name: letters, digits, `_`, `.` and `-`, not starting with
/// a digit.
fn env_key(k: &str) -> bool {
    let mut b = k.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
        && k.len() <= 200
}

/// Checks a service's `previews` before it's saved, and stores them tidy
/// (defaults filled in). Previews need a repository Pyo can read pull
/// requests of: the GitHub App's, or a GitLab connection's.
pub fn check_settings(spec: &mut Value) -> ApiResult<()> {
    let Some(v) = spec.get("previews").cloned() else {
        return Ok(());
    };
    if v.is_null() {
        if let Some(o) = spec.as_object_mut() {
            o.remove("previews");
        }
        return Ok(());
    }
    let bad = |m: &str| Err(ApiError::bad_request(m.to_owned()));
    let mut s: Settings = serde_json::from_value(v)
        .map_err(|_| ApiError::bad_request("Pyo can't read these preview settings."))?;
    s.label = s.label.trim().to_owned();
    if !matches!(s.pulls.as_str(), "all" | "label") {
        return bad("Previews are for all pull requests or ones with a label.");
    }
    if s.pulls == "label" && (s.label.is_empty() || s.label.chars().count() > 50) {
        return bad("Give the label pull requests need for a preview.");
    }
    if !(1..=50).contains(&s.limit) {
        return bad("Previews at once: 1 to 50.");
    }
    if !(0..=90).contains(&s.idle_days) {
        return bad("Remove previews after 1 to 90 days without a push, or never.");
    }
    if !matches!(s.access.as_str(), "public" | "private") {
        return bad("Previews are public, or private on the tailnet.");
    }
    if !matches!(s.forks.as_str(), "off" | "no-secrets" | "secrets") {
        return bad("Pick what happens to pull requests from forks.");
    }
    let mut keys = std::collections::HashSet::new();
    for e in &mut s.env {
        let k = e["key"].as_str().unwrap_or("").trim().to_owned();
        if !env_key(&k) {
            return Err(ApiError::bad_request(format!(
                "{} isn't a valid variable name.",
                if k.is_empty() { "An empty name" } else { &k }
            )));
        }
        if !keys.insert(k.clone()) {
            return Err(ApiError::bad_request(format!("{k} is there twice.")));
        }
        e["key"] = json!(k);
        if !e["id"].is_string() {
            e["id"] = json!(new_id("env"));
        }
    }
    if s.enabled {
        let src = &spec["source"];
        let git = src["type"] == json!("git")
            && (src["installationId"].is_i64()
                || src["gitConnectionId"]
                    .as_str()
                    .is_some_and(|c| !c.is_empty()));
        if !git {
            return bad(
                "Previews need a repository from GitHub (through Pyo's GitHub App) or a GitLab connection.",
            );
        }
        if spec["workloadKind"] == json!("CronJob") {
            return bad("Cron jobs have no previews.");
        }
    }
    spec["previews"] = serde_json::to_value(&s).map_err(anyhow::Error::from)?;
    Ok(())
}

// ── Pull requests ──────────────────────────────────────────────────────────

/// A pull request or merge request, as its provider tells it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pull {
    /// "github" or "gitlab".
    pub provider: &'static str,
    pub installation_id: Option<i64>,
    pub connection_id: Option<String>,
    /// "owner/repo" or "group/sub/project".
    pub repo: String,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub fork: bool,
    pub labels: Vec<String>,
    /// "open", "closed" or "merged".
    pub state: &'static str,
}

impl Pull {
    /// Whose it is: the installation's id or the connection's.
    fn source(&self) -> String {
        match self.provider {
            "github" => self.installation_id.unwrap_or(0).to_string(),
            _ => self.connection_id.clone().unwrap_or_default(),
        }
    }
}

/// A pull request as Pyo keeps it.
#[derive(sqlx::FromRow, Debug, Clone, Default)]
pub struct PullRow {
    pub id: String,
    pub team_id: String,
    pub provider: String,
    pub installation_id: Option<i64>,
    pub connection_id: Option<String>,
    pub repo: String,
    pub number: i64,
    pub author: String,
    pub head_ref: String,
    pub head_sha: String,
    pub fork: bool,
    pub labels: Vec<String>,
    pub state: String,
    pub pushed_ms: i64,
    pub comment_id: Option<i64>,
    pub comment_hash: Option<String>,
    pub check_id: Option<i64>,
    pub check_sha: Option<String>,
    pub check_hash: Option<String>,
}

const PULL_COLS: &str = "id, team_id, provider, installation_id, connection_id, repo, number, author, head_ref, head_sha, fork, labels, state, pyo.ms(pushed_at) AS pushed_ms, comment_id, comment_hash, check_id, check_sha, check_hash";

impl PullRow {
    /// The ref a build fetches: the branch, or for a fork the pull
    /// request's own ref in the repository (its branch is in the fork).
    pub fn build_ref(&self) -> String {
        match (self.fork, self.provider.as_str()) {
            (true, "github") => format!("refs/pull/{}/head", self.number),
            (true, _) => format!("refs/merge-requests/{}/head", self.number),
            _ => self.head_ref.clone(),
        }
    }
}

pub async fn load_pull(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    lock: bool,
) -> sqlx::Result<Option<PullRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PULL_COLS} FROM pyo.pull_requests WHERE id = $1{}",
        if lock { " FOR UPDATE" } else { "" }
    )))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
}

/// A preview as stored.
#[derive(sqlx::FromRow, Debug, Clone, Default)]
pub struct PreviewRow {
    pub id: String,
    pub team_id: String,
    pub pull_id: String,
    pub service_id: String,
    pub preview_service_id: Option<String>,
    pub name: String,
    pub hostname: Option<String>,
    pub state: String,
    pub message: Option<String>,
    pub removed_reason: Option<String>,
    pub sha: String,
    pub deployment_id: Option<String>,
    pub remote_deployment_id: Option<i64>,
    pub remote_deployment_sha: Option<String>,
    pub remote_environment_id: Option<i64>,
    pub reported: Option<String>,
}

const PREVIEW_COLS: &str = "id, team_id, pull_id, service_id, preview_service_id, name, hostname, state, message, removed_reason, sha, deployment_id, remote_deployment_id, remote_deployment_sha, remote_environment_id, reported";

/// A service whose previews follow a repository.
#[derive(sqlx::FromRow, Debug, Clone)]
struct Source {
    id: String,
    team_id: String,
    project_id: String,
    cluster_id: String,
    name: String,
    spec: Value,
}

/// The team's services with previews on that build `repo`, from the
/// installation's repositories (GitHub) or the connection's (GitLab).
async fn sources(
    tx: &mut Transaction<'_, Postgres>,
    team_id: &str,
    provider: &str,
    connection_id: Option<&str>,
    repo: &str,
) -> ApiResult<Vec<Source>> {
    let rows: Vec<Source> = sqlx::query_as(
        "SELECT id, team_id, project_id, cluster_id, name, spec FROM pyo.services
          WHERE team_id = $1 AND NOT deleting AND preview_of IS NULL AND spec->'source'->>'type' = 'git'
            AND coalesce((spec->'previews'->>'enabled')::bool, false)
            AND CASE WHEN $2 = 'github' THEN spec->'source'->>'installationId' IS NOT NULL
                     ELSE spec->'source'->>'gitConnectionId' = $3 END
          ORDER BY created_at",
    )
    .bind(team_id)
    .bind(provider)
    .bind(connection_id)
    .fetch_all(&mut **tx)
    .await?;
    let base: Option<String> = match connection_id {
        Some(c) if provider == "gitlab" => {
            sqlx::query_scalar("SELECT base_url FROM pyo.git_connections WHERE id = $1")
                .bind(c)
                .fetch_optional(&mut **tx)
                .await?
        }
        _ => None,
    };
    Ok(rows
        .into_iter()
        .filter(|r| {
            let url = r.spec["source"]["repoUrl"].as_str().unwrap_or("");
            let theirs = match provider {
                "github" => crate::github::full_name(url),
                _ => base.as_deref().and_then(|b| crate::git::repo_path(b, url)),
            };
            theirs.is_some_and(|t| t.eq_ignore_ascii_case(repo))
        })
        .collect())
}

/// A pull request changed (or was seen by a poll): keep it, when a service
/// of the team previews its repository or it's known already, and bring its
/// previews in line. Whether it's followed.
pub async fn pull_event(s: &AppState, team_id: &str, p: &Pull) -> ApiResult<bool> {
    let mut tx = s.db.begin().await?;
    let followed = !sources(
        &mut tx,
        team_id,
        p.provider,
        p.connection_id.as_deref(),
        &p.repo,
    )
    .await?
    .is_empty();
    let repo = p.repo.to_lowercase();
    let known: Option<String> = sqlx::query_scalar(
        "SELECT id FROM pyo.pull_requests WHERE team_id = $1 AND provider = $2 AND source = $3 AND repo = $4 AND number = $5",
    )
    .bind(team_id)
    .bind(p.provider)
    .bind(p.source())
    .bind(&repo)
    .bind(p.number)
    .fetch_optional(&mut *tx)
    .await?;
    if !followed && known.is_none() {
        return Ok(false);
    }
    let id: String = sqlx::query_scalar(
        "INSERT INTO pyo.pull_requests (id, team_id, provider, installation_id, connection_id, source, repo, number, title, url, author, head_ref, head_sha, base_ref, fork, labels, state, closed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, CASE WHEN $17 = 'open' THEN NULL ELSE now() END)
         ON CONFLICT (team_id, provider, source, repo, number) DO UPDATE SET
           title = excluded.title, url = excluded.url, author = excluded.author, head_ref = excluded.head_ref,
           base_ref = excluded.base_ref, fork = excluded.fork, labels = excluded.labels, state = excluded.state,
           pushed_at = CASE WHEN excluded.head_sha <> '' AND pyo.pull_requests.head_sha <> excluded.head_sha THEN now() ELSE pyo.pull_requests.pushed_at END,
           head_sha = CASE WHEN excluded.head_sha = '' THEN pyo.pull_requests.head_sha ELSE excluded.head_sha END,
           closed_at = CASE WHEN excluded.state = 'open' THEN NULL ELSE coalesce(pyo.pull_requests.closed_at, now()) END,
           updated_at = now()
         RETURNING id",
    )
    .bind(new_id("pr"))
    .bind(team_id)
    .bind(p.provider)
    .bind(p.installation_id)
    .bind(&p.connection_id)
    .bind(p.source())
    .bind(&repo)
    .bind(p.number)
    .bind(p.title.chars().take(300).collect::<String>())
    .bind(&p.url)
    .bind(&p.author)
    .bind(&p.head_ref)
    .bind(&p.head_sha)
    .bind(&p.base_ref)
    .bind(p.fork)
    .bind(&p.labels)
    .bind(p.state)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    reconcile(s, &id).await?;
    Ok(true)
}

// ── Deciding ───────────────────────────────────────────────────────────────

/// What a service's preview of a pull request should be.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Run it. `copy_secrets`: with the service's secret variables;
    /// `secret_overrides`: with the secret preview variables.
    Run {
        copy_secrets: bool,
        secret_overrides: bool,
    },
    Remove {
        reason: &'static str,
        message: String,
    },
}

pub fn decide(pull: &PullRow, s: &Settings, spec: &Value, now_ms: i64) -> Decision {
    let remove = |reason: &'static str, message: String| Decision::Remove { reason, message };
    match pull.state.as_str() {
        "merged" => return remove("merged", "Merged.".into()),
        "open" => {}
        _ => return remove("closed", "Closed.".into()),
    }
    if !s.enabled {
        return remove("settings", "Previews are off for this service.".into());
    }
    if spec["workloadKind"] == json!("CronJob") {
        return remove("settings", "Cron jobs have no previews.".into());
    }
    if pull.fork && s.forks == "off" {
        return remove(
            "fork",
            "Pull requests from forks aren't deployed. The service's preview settings can allow it."
                .into(),
        );
    }
    if s.pulls == "label" && !pull.labels.iter().any(|l| l.eq_ignore_ascii_case(&s.label)) {
        return remove("label", format!("Add the {} label to deploy it.", s.label));
    }
    if s.idle_days > 0 && now_ms - pull.pushed_ms > s.idle_days * 86_400_000 {
        let days = if s.idle_days == 1 {
            "a day".to_owned()
        } else {
            format!("{} days", s.idle_days)
        };
        return remove(
            "idle",
            format!("Removed after {days} without a push. Push to bring it back."),
        );
    }
    let trusted = !pull.fork || s.forks == "secrets";
    Decision::Run {
        copy_secrets: s.copy_secrets && trusted,
        secret_overrides: trusted,
    }
}

/// The copy's port for its URL: the first one websites use, else the
/// first HTTP one. None for a service that serves no website.
fn web_port(spec: &Value) -> Option<String> {
    let ports = spec["ports"].as_array()?;
    ports
        .iter()
        .find(|p| p["exposure"] == json!("http"))
        .or_else(|| {
            ports
                .iter()
                .find(|p| p["protocol"].as_str().is_none_or(|x| x == "HTTP"))
        })
        .and_then(|p| p["name"].as_str())
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
}

/// The copy's configuration: the service's, built from the pull request's
/// head, with its own name, URL and variables. No volumes (they're
/// attached apart), cron jobs, autoscaling, public TCP or UDP ports or
/// secret store imports (unless secrets are copied); one replica.
pub fn copy_spec(
    src: &Value,
    s: &Settings,
    name: &str,
    domain: Option<Value>,
    pull: &PullRow,
    copy_secrets: bool,
    secret_overrides: bool,
) -> Value {
    let mut spec = src.clone();
    // Where it runs: the service's servers and tags (placement.rs), as
    // `placement`. Nothing else pins it: it has no volumes of its own.
    let placed = crate::placement::Placement::of(src);
    if let Some(o) = spec.as_object_mut() {
        o.remove("servers");
        o.remove("serverTags");
        o.remove("placement");
        if !placed.any() {
            o.insert("placement".into(), placed.to_value());
        }
    }
    let url = domain
        .as_ref()
        .and_then(|d| d["hostname"].as_str())
        .map(|h| format!("https://{h}"));
    let port = domain
        .as_ref()
        .and_then(|d| d["portName"].as_str())
        .map(str::to_owned);
    let Some(o) = spec.as_object_mut() else {
        return spec;
    };
    for k in ["previews", "autoscale", "cronJobs", "description"] {
        o.remove(k);
    }
    if !copy_secrets {
        o.remove("secretImports");
    }
    o.insert("name".into(), json!(name));
    o.insert("replicas".into(), json!(1));
    if let Some(src) = o.get_mut("source").and_then(Value::as_object_mut) {
        src.insert("commit".into(), json!(pull.head_sha));
        src.insert("branch".into(), json!(pull.build_ref()));
        src.insert("autoDeploy".into(), json!(false));
    }
    if let Some(ports) = o.get_mut("ports").and_then(Value::as_array_mut) {
        ports.retain(|p| !matches!(p["exposure"].as_str(), Some("nodeport" | "loadbalancer")));
        for p in ports.iter_mut() {
            let theirs = port.as_deref().is_some_and(|n| p["name"] == json!(n));
            if p["exposure"] == json!("http") || theirs {
                p["exposure"] = json!(if theirs { "http" } else { "internal" });
            }
        }
    }
    o.insert("domains".into(), Value::Array(domain.into_iter().collect()));
    let mut env: Vec<Value> = o
        .get("env")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| copy_secrets || e["secret"] != json!(true))
        .collect();
    let mut set = |e: Value| match env.iter_mut().find(|x| x["key"] == e["key"]) {
        Some(x) => *x = e,
        None => env.push(e),
    };
    for e in &s.env {
        if e["secret"] == json!(true) && !secret_overrides {
            continue;
        }
        set(e.clone());
    }
    let mut ours = vec![
        ("PYO_PREVIEW", "1".to_owned()),
        ("PYO_PULL_REQUEST", pull.number.to_string()),
    ];
    if let Some(u) = url {
        ours.push(("PYO_PREVIEW_URL", u));
    }
    for (k, v) in ours {
        set(
            json!({ "id": format!("env_{}", k.to_lowercase()), "key": k, "value": v, "secret": false }),
        );
    }
    o.insert("env".into(), Value::Array(env));
    spec
}

// ── Running ────────────────────────────────────────────────────────────────

async fn load_previews(
    tx: &mut Transaction<'_, Postgres>,
    pull_id: &str,
) -> sqlx::Result<Vec<PreviewRow>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PREVIEW_COLS} FROM pyo.previews WHERE pull_id = $1 ORDER BY created_at FOR UPDATE"
    )))
    .bind(pull_id)
    .fetch_all(&mut **tx)
    .await
}

/// Brings a pull request's previews in line with it and with its services'
/// settings: starts, redeploys, holds or removes them. The bot reports
/// what changed afterwards (a job).
pub async fn reconcile(s: &AppState, pull_id: &str) -> ApiResult<()> {
    let mut wake: Vec<String> = vec![];
    let mut tx = s.db.begin().await?;
    let Some(pull) = load_pull(&mut tx, pull_id, true).await? else {
        return Ok(());
    };
    let sources = sources(
        &mut tx,
        &pull.team_id,
        &pull.provider,
        pull.connection_id.as_deref(),
        &pull.repo,
    )
    .await?;
    let previews = load_previews(&mut tx, pull_id).await?;
    let now = now_ms();
    let mut changed = false;
    // A service that stopped previewing (turned off, deleted, moved to
    // another repository): its preview goes.
    for p in &previews {
        if p.state != "removed" && !sources.iter().any(|x| x.id == p.service_id) {
            remove(
                &mut tx,
                p,
                "settings",
                "Previews are off for this service.",
                &mut wake,
            )
            .await?;
            changed = true;
        }
    }
    for src in &sources {
        let settings = Settings::of(&src.spec);
        let existing = previews.iter().find(|p| p.service_id == src.id);
        match decide(&pull, &settings, &src.spec, now) {
            Decision::Remove { reason, message } => {
                if let Some(p) = existing.filter(|p| p.state != "removed") {
                    remove(&mut tx, p, reason, &message, &mut wake).await?;
                    changed = true;
                }
            }
            Decision::Run {
                copy_secrets,
                secret_overrides,
            } => {
                changed |= run(
                    &mut tx,
                    &pull,
                    src,
                    &settings,
                    (copy_secrets, secret_overrides),
                    existing,
                    &mut wake,
                )
                .await?;
            }
        }
    }
    if changed {
        crate::jobs::enqueue(&mut *tx, "preview.report", json!({ "pullId": pull_id })).await?;
    }
    tx.commit().await?;
    for c in wake {
        s.wake(&c).await;
    }
    if changed {
        s.wake(&format!("builders:{}", pull.team_id)).await;
    }
    Ok(())
}

/// Starts or redeploys a service's preview, or holds it over the limit.
/// Whether anything changed.
async fn run(
    tx: &mut Transaction<'_, Postgres>,
    pull: &PullRow,
    src: &Source,
    settings: &Settings,
    secrets: (bool, bool),
    p: Option<&PreviewRow>,
    wake: &mut Vec<String>,
) -> ApiResult<bool> {
    if let Some(p) = p {
        match p.state.as_str() {
            // Removed by someone, nothing pushed since: it stays removed.
            "removed" if p.removed_reason.as_deref() == Some("user") && p.sha == pull.head_sha => {
                return Ok(false);
            }
            // Tried this commit already: the next push, or Redeploy.
            "failed" if p.sha == pull.head_sha => return Ok(false),
            "active" if p.sha == pull.head_sha && copy_alive(tx, p).await? => return Ok(false),
            _ => {}
        }
    }
    let starting = p.is_none_or(|p| matches!(p.state.as_str(), "removed" | "waiting"));
    if starting {
        let running: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pyo.previews WHERE service_id = $1 AND state IN ('active', 'failed') AND pull_id <> $2",
        )
        .bind(&src.id)
        .bind(&pull.id)
        .fetch_one(&mut **tx)
        .await?;
        if running >= settings.limit {
            let message = format!(
                "{} previews of {} run already, its limit. This one starts when one of them goes.",
                running, src.name
            );
            return hold(tx, pull, src, p, &message).await;
        }
    }
    start(tx, pull, src, settings, secrets, p, wake).await?;
    Ok(true)
}

/// Whether a preview's copy is there and not going.
async fn copy_alive(tx: &mut Transaction<'_, Postgres>, p: &PreviewRow) -> ApiResult<bool> {
    let Some(copy) = &p.preview_service_id else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pyo.services WHERE id = $1 AND NOT deleting)",
    )
    .bind(copy)
    .fetch_one(&mut **tx)
    .await?)
}

/// A preview that waits: over the limit, or for its last copy to go.
async fn hold(
    tx: &mut Transaction<'_, Postgres>,
    pull: &PullRow,
    src: &Source,
    p: Option<&PreviewRow>,
    message: &str,
) -> ApiResult<bool> {
    if let Some(p) = p {
        if p.state == "waiting" && p.message.as_deref() == Some(message) {
            return Ok(false);
        }
        sqlx::query("UPDATE pyo.previews SET state = 'waiting', message = $2, removed_reason = NULL, updated_at = now() WHERE id = $1")
            .bind(&p.id)
            .bind(message)
            .execute(&mut **tx)
            .await?;
        return Ok(true);
    }
    let name = pick_name(tx, src, pull.number).await?;
    sqlx::query("INSERT INTO pyo.previews (id, team_id, pull_id, service_id, name, state, message) VALUES ($1, $2, $3, $4, $5, 'waiting', $6)")
        .bind(new_id("pvw"))
        .bind(&src.team_id)
        .bind(&pull.id)
        .bind(&src.id)
        .bind(&name)
        .bind(message)
        .execute(&mut **tx)
        .await?;
    Ok(true)
}

/// A free name for a copy in the service's project.
async fn pick_name(
    tx: &mut Transaction<'_, Postgres>,
    src: &Source,
    number: i64,
) -> ApiResult<String> {
    for name in [
        naming::label(&src.name, number, naming::NAME_MAX),
        naming::label_salted(&src.name, number, naming::NAME_MAX, &src.id),
    ] {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.services WHERE project_id = $1 AND name = $2)
                 OR EXISTS (SELECT 1 FROM pyo.previews WHERE name = $2 AND service_id IN (SELECT id FROM pyo.services WHERE project_id = $1) AND service_id <> $3)",
        )
        .bind(&src.project_id)
        .bind(&name)
        .bind(&src.id)
        .fetch_one(&mut **tx)
        .await?;
        if !taken {
            return Ok(name);
        }
    }
    Err(ApiError::conflict(format!(
        "{} has another service named like its preview.",
        src.name
    )))
}

/// Creates or updates the copy and deploys the pull request's head. A
/// deploy Pyo can't start (no builder, a check that failed) leaves the
/// preview failed, with why.
async fn start(
    tx: &mut Transaction<'_, Postgres>,
    pull: &PullRow,
    src: &Source,
    settings: &Settings,
    (copy_secrets, secret_overrides): (bool, bool),
    p: Option<&PreviewRow>,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    let id = p.map(|p| p.id.clone()).unwrap_or_else(|| new_id("pvw"));
    let name = match p {
        Some(p) => p.name.clone(),
        None => pick_name(tx, src, pull.number).await?,
    };
    // The last copy is still going (its name is taken): wait for it.
    let live_copy = match p {
        Some(p) if copy_alive(tx, p).await? => p.preview_service_id.clone(),
        _ => None,
    };
    if live_copy.is_none() {
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pyo.services WHERE project_id = $1 AND name = $2)",
        )
        .bind(&src.project_id)
        .bind(&name)
        .fetch_one(&mut **tx)
        .await?;
        if busy {
            return hold(tx, pull, src, p, "Starts once its last copy is gone.")
                .await
                .map(|_| ());
        }
    }
    let port = web_port(&src.spec);
    let host = match &port {
        None => Ok(None),
        Some(_) => {
            domain::host_for(
                tx,
                &src.cluster_id,
                settings,
                &src.name,
                pull.number,
                &src.id,
                p.and_then(|p| p.hostname.as_deref()),
                &id,
                live_copy.as_deref(),
            )
            .await?
        }
    };
    sqlx::query(
        "INSERT INTO pyo.previews (id, team_id, pull_id, service_id, name, state) VALUES ($1, $2, $3, $4, $5, 'active')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&id)
    .bind(&src.team_id)
    .bind(&pull.id)
    .bind(&src.id)
    .bind(&name)
    .execute(&mut **tx)
    .await?;
    let host = match host {
        Ok(h) => h,
        Err(why) => {
            fail(tx, &id, &pull.head_sha, &why).await?;
            return Ok(());
        }
    };
    // Its domain: the one it has when the host is the same (its DNS and
    // certificate state stay), else a new one.
    let current: Value = match &live_copy {
        Some(c) => {
            sqlx::query_scalar(
                "SELECT coalesce(spec->'domains', '[]') FROM pyo.services WHERE id = $1",
            )
            .bind(c)
            .fetch_one(&mut **tx)
            .await?
        }
        None => json!([]),
    };
    let domain = match (&host, &port) {
        (Some(h), Some(port)) => Some(
            current
                .as_array()
                .and_then(|ds| ds.iter().find(|d| d["hostname"] == json!(h)).cloned())
                .map(|mut d| {
                    d["portName"] = json!(port);
                    d
                })
                .unwrap_or_else(|| domain::new_domain(h, port)),
        ),
        _ => None,
    };
    let domain = match domain {
        Some(d) => Some(domain::with_certificate(tx, &src.cluster_id, d).await?),
        None => None,
    };
    let mut spec = copy_spec(
        &src.spec,
        settings,
        &name,
        domain,
        pull,
        copy_secrets,
        secret_overrides,
    );
    crate::vault::get()
        .seal_env(&src.team_id, &mut spec)
        .await?;
    let copy = match &live_copy {
        Some(c) => {
            let routes_changed = crate::routes::differ(&current, &spec["domains"]);
            sqlx::query("UPDATE pyo.services SET spec = $2, updated_at = now() WHERE id = $1")
                .bind(c)
                .bind(&spec)
                .execute(&mut **tx)
                .await?;
            if routes_changed {
                crate::routes::changed(tx, c, false).await?;
            }
            c.clone()
        }
        None => {
            let c = new_id("svc");
            sqlx::query("INSERT INTO pyo.services (id, team_id, project_id, cluster_id, name, spec, status, preview_of) VALUES ($1, $2, $3, $4, $5, $6, 'deploying', $7)")
                .bind(&c)
                .bind(&src.team_id)
                .bind(&src.project_id)
                .bind(&src.cluster_id)
                .bind(&name)
                .bind(&spec)
                .bind(&src.id)
                .execute(&mut **tx)
                .await?;
            c
        }
    };
    sqlx::query("UPDATE pyo.previews SET preview_service_id = $2, hostname = $3, state = 'active', removed_reason = NULL, updated_at = now() WHERE id = $1")
        .bind(&id)
        .bind(&copy)
        .bind(&host)
        .execute(&mut **tx)
        .await?;
    // A build of an older head still running: it stops first, and once
    // it settled the next pass deploys this one.
    let in_flight: Option<(String, String)> = sqlx::query_as(
        "SELECT id, coalesce(data->'source'->>'commit', '') FROM pyo.deployments
          WHERE service_id = $1 AND status NOT IN ('succeeded', 'failed', 'cancelled') ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&copy)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((dep, commit)) = in_flight {
        if commit == pull.head_sha {
            sqlx::query("UPDATE pyo.previews SET sha = $2, deployment_id = $3, message = NULL WHERE id = $1")
                .bind(&id)
                .bind(&pull.head_sha)
                .bind(&dep)
                .execute(&mut **tx)
                .await?;
        } else {
            wake.push(deploy::cancel(tx, &dep).await?);
        }
        return Ok(());
    }
    let dep = new_id("dpl");
    let actor = if pull.author.is_empty() {
        "Preview".to_owned()
    } else {
        pull.author.clone()
    };
    let mut sp = sqlx::Acquire::begin(&mut **tx).await?;
    let made = deploy::create(
        &mut sp,
        DeployRequest {
            deployment_id: &dep,
            service_id: &copy,
            trigger: "preview",
            actor: &actor,
            rollback_of: None,
            retry_of: None,
            commit: Some(&pull.head_sha),
            upload: None,
        },
    )
    .await;
    match made {
        Ok(cluster) => {
            sp.commit().await?;
            wake.push(cluster);
            sqlx::query("UPDATE pyo.previews SET sha = $2, deployment_id = $3, message = NULL, updated_at = now() WHERE id = $1")
                .bind(&id)
                .bind(&pull.head_sha)
                .bind(&dep)
                .execute(&mut **tx)
                .await?;
        }
        Err(e) => {
            sp.rollback().await?;
            fail(tx, &id, &pull.head_sha, &e.message).await?;
        }
    }
    Ok(())
}

async fn fail(tx: &mut Transaction<'_, Postgres>, id: &str, sha: &str, why: &str) -> ApiResult<()> {
    sqlx::query("UPDATE pyo.previews SET state = 'failed', sha = $2, message = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(sha)
        .bind(why)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Removes a preview: its copy goes from the cluster (as a deleted service
/// does), and it's kept as removed, with why.
async fn remove(
    tx: &mut Transaction<'_, Postgres>,
    p: &PreviewRow,
    reason: &str,
    message: &str,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    if let Some(copy) = &p.preview_service_id {
        delete_copy(tx, copy, wake).await?;
    }
    sqlx::query("UPDATE pyo.previews SET state = 'removed', removed_reason = $2, message = $3, updated_at = now() WHERE id = $1")
        .bind(&p.id)
        .bind(reason)
        .bind(message)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Deletes a copy as DeleteService does: what runs stops, the agent removes
/// its objects, and the row goes when it reports back.
async fn delete_copy(
    tx: &mut Transaction<'_, Postgres>,
    copy: &str,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    let row: Option<(String, bool)> =
        sqlx::query_as("SELECT cluster_id, deleting FROM pyo.services WHERE id = $1 FOR UPDATE")
            .bind(copy)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((cluster, deleting)) = row else {
        return Ok(());
    };
    if deleting {
        return Ok(());
    }
    let service = crate::ops::service_ref(tx, copy).await?;
    sqlx::query("UPDATE pyo.services SET deleting = true WHERE id = $1")
        .bind(copy)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE pyo.build_jobs SET status = 'cancelled', finished_at = now() WHERE status IN ('queued', 'running') AND deployment_id IN (SELECT id FROM pyo.deployments WHERE service_id = $1)")
        .bind(copy)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE pyo.deployments SET status = 'cancelled', finished_at = now() WHERE service_id = $1 AND status NOT IN ('succeeded', 'failed', 'cancelled')")
        .bind(copy)
        .execute(&mut **tx)
        .await?;
    deploy::enqueue(tx, &cluster, Command::DeleteService(service)).await?;
    crate::sites::sync(tx, &cluster).await?;
    wake.push(cluster);
    Ok(())
}

// ── Hooks from the rest of Pyo ─────────────────────────────────────────────

/// Whether a service is a preview's copy.
pub async fn is_copy<'e>(db: impl sqlx::PgExecutor<'e>, service_id: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pyo.services WHERE id = $1 AND preview_of IS NOT NULL)",
    )
    .bind(service_id)
    .fetch_one(db)
    .await
}

/// A deployment of a copy moved on (the "git.status" job, instead of a
/// commit status): the bot says so, and its pull request is brought in line
/// (a newer head may wait for this one to stop).
pub async fn deployment_changed(s: &AppState, deployment_id: &str) -> anyhow::Result<bool> {
    let pull: Option<String> = sqlx::query_scalar(
        "SELECT p.pull_id FROM pyo.previews p JOIN pyo.deployments d ON d.service_id = p.preview_service_id WHERE d.id = $1",
    )
    .bind(deployment_id)
    .fetch_optional(&s.db)
    .await?;
    let Some(pull) = pull else {
        return Ok(false);
    };
    reconcile(s, &pull)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    crate::jobs::enqueue(&s.db, "preview.report", json!({ "pullId": pull })).await?;
    Ok(true)
}

/// A service's placement changed (and went live): its running copies
/// follow, in place too. One mid-deploy keeps it for its next deploy (a
/// push re-copies the service anyway).
pub async fn placement_changed(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    let spec: Value = sqlx::query_scalar("SELECT spec FROM pyo.services WHERE id = $1")
        .bind(service_id)
        .fetch_one(&mut **tx)
        .await?;
    let placed = crate::placement::Placement::of(&spec);
    let value = if placed.any() {
        Value::Null
    } else {
        placed.to_value()
    };
    let copies: Vec<(String, Value)> = sqlx::query_as(
        "SELECT id, spec FROM pyo.services WHERE preview_of = $1 AND NOT deleting FOR UPDATE",
    )
    .bind(service_id)
    .fetch_all(&mut **tx)
    .await?;
    for (copy, mut cspec) in copies {
        if let Some(o) = cspec.as_object_mut() {
            o.remove("servers");
            o.remove("serverTags");
            o.remove("placement");
            if !value.is_null() {
                o.insert("placement".into(), value.clone());
            }
        }
        sqlx::query("UPDATE pyo.services SET spec = $2, updated_at = now() WHERE id = $1")
            .bind(&copy)
            .bind(&cspec)
            .execute(&mut **tx)
            .await?;
        let mut sp = sqlx::Acquire::begin(&mut **tx).await?;
        match crate::placement::go_live(&mut sp, &copy, &cspec).await {
            Ok(c) => {
                sp.commit().await?;
                wake.extend(c);
            }
            // A deploy is running: it goes out with the next one.
            Err(_) => sp.rollback().await?,
        }
    }
    Ok(())
}

/// A service is being deleted: its previews go with it (their copies too).
/// A copy deleted by hand: its preview stays removed until the next push.
pub async fn service_deleted(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    wake: &mut Vec<String>,
) -> ApiResult<()> {
    let rows: Vec<PreviewRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PREVIEW_COLS} FROM pyo.previews WHERE (service_id = $1 OR preview_service_id = $1) AND state <> 'removed' FOR UPDATE"
    )))
    .bind(service_id)
    .fetch_all(&mut **tx)
    .await?;
    for p in &rows {
        let own = p.preview_service_id.as_deref() == Some(service_id);
        let (reason, message) = if own {
            ("user", "Removed. The next push brings it back.")
        } else {
            ("settings", "The service was deleted.")
        };
        // The copy being deleted by hand is the caller's to delete.
        if own {
            sqlx::query("UPDATE pyo.previews SET state = 'removed', removed_reason = $2, message = $3, updated_at = now() WHERE id = $1")
                .bind(&p.id)
                .bind(reason)
                .bind(message)
                .execute(&mut **tx)
                .await?;
        } else {
            remove(tx, p, reason, message, wake).await?;
        }
        crate::jobs::enqueue(&mut **tx, "preview.report", json!({ "pullId": p.pull_id })).await?;
    }
    Ok(())
}

/// A service's settings were saved: its open pull requests' previews follow
/// (on, off, a new limit or label), in the background.
pub async fn settings_saved(
    tx: &mut Transaction<'_, Postgres>,
    service_id: &str,
    before: &Value,
    after: &Value,
) -> ApiResult<()> {
    if before.get("previews") == after.get("previews") && before["source"] == after["source"] {
        return Ok(());
    }
    crate::jobs::enqueue(
        &mut **tx,
        "preview.sync",
        json!({ "serviceId": service_id }),
    )
    .await?;
    Ok(())
}

/// The "preview.sync" job: every known open pull request of the service's
/// repository, and a fresh look at the provider's open ones (previews just
/// turned on).
pub async fn sync_service(s: &AppState, service_id: &str) -> anyhow::Result<()> {
    let row: Option<(String, Value)> =
        sqlx::query_as("SELECT team_id, spec FROM pyo.services WHERE id = $1")
            .bind(service_id)
            .fetch_optional(&s.db)
            .await?;
    let Some((team, spec)) = row else {
        return Ok(());
    };
    let src = &spec["source"];
    if Settings::of(&spec).enabled
        && let Err(e) = poll_source(s, &team, src).await
    {
        tracing::info!(service = %service_id, error = %format!("{e:#}"), "previews: couldn't list pull requests");
    }
    let pulls: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT pull_id FROM pyo.previews WHERE service_id = $1 AND state <> 'removed'
         UNION SELECT id FROM pyo.pull_requests WHERE team_id = $2 AND state = 'open'",
    )
    .bind(service_id)
    .bind(&team)
    .fetch_all(&s.db)
    .await?;
    for p in pulls {
        reconcile(s, &p)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    }
    Ok(())
}

// ── The bot ────────────────────────────────────────────────────────────────

/// A preview with what the bot needs to say about it.
#[derive(sqlx::FromRow, Debug, Clone)]
pub struct View {
    #[sqlx(flatten)]
    pub row: PreviewRow,
    pub service: String,
    pub deployment_status: Option<String>,
    /// Where its deployment's page is: the project's namespace and the
    /// service's name.
    pub deployment_project: Option<String>,
    pub deployment_service: Option<String>,
    pub copy_left: bool,
    pub updated_ms: i64,
}

impl View {
    pub fn shown(&self) -> Shown {
        Shown::of(
            &self.row.state,
            self.deployment_status.as_deref(),
            self.copy_left,
        )
    }

    pub fn url(&self) -> Option<String> {
        self.row.hostname.as_ref().map(|h| format!("https://{h}"))
    }
}

async fn views(tx: &mut Transaction<'_, Postgres>, pull_id: &str) -> sqlx::Result<Vec<View>> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {}, s.name AS service, d.status AS deployment_status,
                dp.namespace AS deployment_project, ds.name AS deployment_service,
                (c.id IS NOT NULL) AS copy_left, pyo.ms(greatest(p.updated_at, coalesce(d.finished_at, d.created_at, p.updated_at))) AS updated_ms
           FROM pyo.previews p JOIN pyo.services s ON s.id = p.service_id
           LEFT JOIN pyo.deployments d ON d.id = p.deployment_id
           LEFT JOIN pyo.services ds ON ds.id = d.service_id
           LEFT JOIN pyo.projects dp ON dp.id = ds.project_id
           LEFT JOIN pyo.services c ON c.id = p.preview_service_id
          WHERE p.pull_id = $1 ORDER BY s.name",
        PREVIEW_COLS
            .split(", ")
            .map(|c| format!("p.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
    .bind(pull_id)
    .fetch_all(&mut **tx)
    .await
}

/// One place a pull request lives (GitHub through the app, GitLab through
/// the team's connection): what the bot can do there.
pub trait Forge {
    /// Posts the comment, or edits it in place (posting it again when it
    /// was deleted). Its id.
    async fn comment(&self, id: Option<i64>, body: &str) -> anyhow::Result<i64>;
    /// Sets the check on `sha`: a new one when `id` is None. Its id.
    async fn check(
        &self,
        id: Option<i64>,
        sha: &str,
        check: &bot::Check,
        details: Option<&str>,
    ) -> anyhow::Result<Option<i64>>;
    /// Brings the preview's deployment (and environment) on the provider in
    /// line with how it's shown.
    async fn deployment(&self, d: &mut Remote, shown: Shown) -> anyhow::Result<()>;
}

/// A preview's deployment on the provider.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Remote {
    /// The copy's name ("web-pr-42") and the service's ("web").
    pub name: String,
    pub service: String,
    pub sha: String,
    pub head_ref: String,
    pub url: Option<String>,
    pub logs: Option<String>,
    pub deployment_id: Option<i64>,
    pub deployment_sha: Option<String>,
    pub environment_id: Option<i64>,
}

/// The "preview.report" job: the bot says where a pull request's previews
/// stand, on the provider. Only what changed since it last said it.
pub async fn report(s: &AppState, pull_id: &str) -> anyhow::Result<()> {
    let mut tx = s.db.begin().await?;
    let Some(pull) = load_pull(&mut tx, pull_id, true).await? else {
        return Ok(());
    };
    let views = views(&mut tx, pull_id).await?;
    if views.is_empty() && pull.comment_id.is_none() {
        return Ok(());
    }
    let slug: String = sqlx::query_scalar("SELECT slug FROM pyo.teams WHERE id = $1")
        .bind(&pull.team_id)
        .fetch_one(&mut *tx)
        .await?;
    let out = match pull.provider.as_str() {
        "github" => {
            let Some(f) = github::Forge::for_pull(s, &pull).await? else {
                return Ok(());
            };
            let base = format!("{}/{slug}", f.dashboard());
            say(&mut tx, &f, &pull, &views, &base).await
        }
        _ => {
            let Some(f) = gitlab::Forge::for_pull(s, &pull).await? else {
                return Ok(());
            };
            let base = format!("{}/{slug}", f.dashboard());
            say(&mut tx, &f, &pull, &views, &base).await
        }
    };
    tx.commit().await?;
    out
}

/// What the bot says, and what it keeps of it. Goes on past a call that
/// failed (and keeps what worked), then returns its error so the job tries
/// again.
pub async fn say<F: Forge>(
    tx: &mut Transaction<'_, Postgres>,
    forge: &F,
    pull: &PullRow,
    views: &[View],
    base: &str,
) -> anyhow::Result<()> {
    let base = base.trim_end_matches('/');
    let logs = |v: &View| {
        v.row
            .deployment_id
            .as_ref()
            .filter(|_| base.starts_with("http"))
            .map(|d| {
                let page = crate::deploy::deployment_page(
                    v.deployment_project.as_deref(),
                    v.deployment_service.as_deref(),
                    d,
                );
                format!("{base}{page}")
            })
    };
    let rows: Vec<bot::Row> = views
        .iter()
        .map(|v| bot::Row {
            service: v.service.clone(),
            shown: v.shown(),
            url: v.url(),
            message: v.row.message.clone(),
            updated_ms: v.updated_ms,
            logs: logs(v),
        })
        .collect();
    let open = pull.state == "open";
    let mut first_error: Option<anyhow::Error> = None;
    let mut keep = |e: anyhow::Error| {
        tracing::info!(pull = %pull.id, error = %format!("{e:#}"), "previews: the bot couldn't say it");
        if first_error.is_none() {
            first_error = Some(e);
        }
    };

    let body = bot::comment(&pull.team_id, &rows, open);
    let h = bot::hash(&body);
    if pull.comment_hash.as_deref() != Some(h.as_str()) {
        match forge.comment(pull.comment_id, &body).await {
            Ok(id) => {
                sqlx::query(
                    "UPDATE pyo.pull_requests SET comment_id = $2, comment_hash = $3 WHERE id = $1",
                )
                .bind(&pull.id)
                .bind(id)
                .bind(&h)
                .execute(&mut **tx)
                .await?;
            }
            Err(e) => keep(e),
        }
    }

    // The check, on the head. A closed pull request's check that was still
    // running is finished, neutral.
    let check = if open && !rows.is_empty() {
        Some(bot::check(&rows))
    } else if !open && pull.check_sha.as_deref() == Some(pull.head_sha.as_str()) {
        Some(bot::Check {
            status: "completed",
            conclusion: Some("neutral"),
            title: "Removed".into(),
            summary: "The pull request closed.".into(),
        })
    } else {
        None
    };
    if let Some(c) = check {
        // "<status>:<hash>": a check that finished and builds again is a new
        // one (GitHub keeps the finished one).
        let key = format!(
            "{}:{}",
            c.status,
            bot::hash(&format!("{}:{:?}", pull.head_sha, c))
        );
        if pull.check_hash.as_deref() != Some(key.as_str()) && !pull.head_sha.is_empty() {
            let was_running = pull
                .check_hash
                .as_deref()
                .is_some_and(|h| h.starts_with("in_progress:"));
            let id = if pull.check_sha.as_deref() == Some(pull.head_sha.as_str())
                && (c.status != "in_progress" || was_running)
            {
                pull.check_id
            } else {
                None
            };
            let details = rows.iter().find_map(|r| r.logs.clone());
            match forge
                .check(id, &pull.head_sha, &c, details.as_deref())
                .await
            {
                Ok(id) => {
                    sqlx::query("UPDATE pyo.pull_requests SET check_id = $2, check_sha = $3, check_hash = $4 WHERE id = $1")
                        .bind(&pull.id)
                        .bind(id)
                        .bind(&pull.head_sha)
                        .bind(&key)
                        .execute(&mut **tx)
                        .await?;
                }
                Err(e) => keep(e),
            }
        }
    }

    for v in views {
        let shown = v.shown();
        // Nothing deployed yet: nothing to show on the provider.
        if v.row.sha.is_empty() && v.row.remote_deployment_id.is_none() {
            continue;
        }
        let key = format!(
            "{}:{}:{}",
            v.row.sha,
            shown.key(),
            v.row.hostname.as_deref().unwrap_or("")
        );
        if v.row.reported.as_deref() == Some(key.as_str()) {
            continue;
        }
        let mut r = Remote {
            name: v.row.name.clone(),
            service: v.service.clone(),
            sha: v.row.sha.clone(),
            head_ref: pull.head_ref.clone(),
            url: v.url(),
            logs: logs(v),
            deployment_id: v.row.remote_deployment_id,
            deployment_sha: v.row.remote_deployment_sha.clone(),
            environment_id: v.row.remote_environment_id,
        };
        let out = forge.deployment(&mut r, shown).await;
        sqlx::query("UPDATE pyo.previews SET remote_deployment_id = $2, remote_deployment_sha = $3, remote_environment_id = $4, reported = $5 WHERE id = $1")
            .bind(&v.row.id)
            .bind(r.deployment_id)
            .bind(&r.deployment_sha)
            .bind(r.environment_id)
            .bind(if out.is_ok() { Some(key.as_str()) } else { v.row.reported.as_deref() })
            .execute(&mut **tx)
            .await?;
        if let Err(e) = out {
            keep(e);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

// ── The loop ───────────────────────────────────────────────────────────────

/// Every 30 seconds, on one process: pull requests from providers that send
/// no webhooks (each minute; every 10 minutes everywhere, for missed ones),
/// previews waiting or idle, the clusters' preview domains, and GitHub's
/// permissions (every 30 minutes).
pub async fn runner(s: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    let mut n: u64 = 0;
    loop {
        tick.tick().await;
        let i = n;
        n += 1;
        crate::bg::as_leader(&s.db, LOCK, || pass(&s, i)).await;
    }
}

async fn pass(s: &AppState, n: u64) {
    if n.is_multiple_of(2)
        && let Err(e) = poll(s, n.is_multiple_of(20)).await
    {
        tracing::warn!(error = %e.message, "previews: polling");
    }
    if n.is_multiple_of(2)
        && let Err(e) = sweep(s).await
    {
        tracing::warn!(error = %e.message, "previews: sweep");
    }
    if let Err(e) = domain::tick(s).await {
        tracing::warn!(error = %e.message, "previews: domains");
    }
    if n.is_multiple_of(60) {
        github::check_all(s).await;
    }
}

/// Open pull requests with previews that may have to move: waiting ones,
/// idle ones, ones whose deploy didn't start. Each is brought in line.
async fn sweep(s: &AppState) -> ApiResult<()> {
    let pulls: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT pr.id FROM pyo.pull_requests pr JOIN pyo.previews p ON p.pull_id = pr.id
          WHERE (pr.state = 'open' AND p.state IN ('waiting', 'active', 'failed'))
             OR (pr.state <> 'open' AND p.state <> 'removed')",
    )
    .fetch_all(&s.db)
    .await?;
    for p in pulls {
        if let Err(e) = reconcile(s, &p).await {
            tracing::info!(pull = %p, error = %e.message, "previews: couldn't bring it in line");
        }
    }
    Ok(())
}

/// The repositories with previews on: their open pull requests, from the
/// provider. Those a webhook covers only every 10 minutes (`all`).
async fn poll(s: &AppState, all: bool) -> ApiResult<()> {
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT DISTINCT team_id, spec->'source' FROM pyo.services
          WHERE NOT deleting AND preview_of IS NULL AND coalesce((spec->'previews'->>'enabled')::bool, false)",
    )
    .fetch_all(&s.db)
    .await?;
    let github_hooks = crate::github::app(&s.db)
        .await?
        .is_some_and(|a| a.webhook_active);
    for (team, src) in rows {
        let hooked = if src["installationId"].is_i64() {
            github_hooks
        } else {
            gitlab::hooked(&s.db, &src).await.unwrap_or(false)
        };
        if hooked && !all {
            continue;
        }
        if let Err(e) = poll_source(s, &team, &src).await {
            tracing::info!(repo = %src["repoUrl"], error = %format!("{e:#}"), "previews: couldn't list pull requests");
        }
    }
    Ok(())
}

/// One repository's open pull requests, and the ones Pyo has open that
/// aren't any more.
async fn poll_source(s: &AppState, team: &str, src: &Value) -> anyhow::Result<()> {
    let (open, key): (Vec<Pull>, (String, String, String)) =
        if let Some(inst) = src["installationId"].as_i64() {
            let Some(repo) = crate::github::full_name(src["repoUrl"].as_str().unwrap_or("")) else {
                return Ok(());
            };
            let c = github::Client::for_installation(s, inst).await?;
            (
                c.open_pulls(&repo, inst).await?,
                ("github".into(), inst.to_string(), repo.to_lowercase()),
            )
        } else if let Some(conn) = src["gitConnectionId"].as_str() {
            let c = crate::git::load(&s.db, Some(team), conn)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e.message))?;
            let Some(repo) = c.repo_of(src["repoUrl"].as_str().unwrap_or("")) else {
                return Ok(());
            };
            let api = c
                .api()
                .await
                .map_err(|e| anyhow::anyhow!("{}", e.message))?;
            let list = api.merge_requests(&repo).await?;
            (
                list.iter()
                    .filter_map(|v| gitlab::pull_of_api(v, conn, &repo))
                    .collect(),
                ("gitlab".into(), conn.to_owned(), repo.to_lowercase()),
            )
        } else {
            return Ok(());
        };
    for p in &open {
        pull_event(s, team, p)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e.message))?;
    }
    // Known open, not in the list: closed or merged since.
    let known: Vec<i64> = sqlx::query_scalar(
        "SELECT number FROM pyo.pull_requests WHERE team_id = $1 AND provider = $2 AND source = $3 AND repo = $4 AND state = 'open'",
    )
    .bind(team)
    .bind(&key.0)
    .bind(&key.1)
    .bind(&key.2)
    .fetch_all(&s.db)
    .await?;
    for n in known {
        if open.iter().any(|p| p.number == n) {
            continue;
        }
        let p = match key.0.as_str() {
            "github" => {
                let inst = key.1.parse().unwrap_or(0);
                github::Client::for_installation(s, inst)
                    .await?
                    .pull(&key.2, n, inst)
                    .await?
            }
            _ => {
                let c = crate::git::load(&s.db, Some(team), &key.1)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e.message))?;
                let api = c
                    .api()
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e.message))?;
                gitlab::pull_of_api(&api.merge_request(&key.2, n).await?, &key.1, &key.2)
            }
        };
        if let Some(p) = p {
            pull_event(s, team, &p)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        }
    }
    Ok(())
}

// ── The dashboard ──────────────────────────────────────────────────────────

/// The previews the dashboard shows: those not removed, and those removed
/// in the last day.
pub async fn snapshot(db: &mut sqlx::PgConnection, team_ids: &[String]) -> ApiResult<Vec<Value>> {
    #[derive(sqlx::FromRow)]
    struct Listed {
        #[sqlx(flatten)]
        row: PreviewRow,
        provider: String,
        repo: String,
        number: i64,
        title: String,
        url: String,
        head_ref: String,
        author: String,
        fork: bool,
        pull_state: String,
        deployment_status: Option<String>,
        copy_left: bool,
        created_ms: i64,
        updated_ms: i64,
    }
    let rows: Vec<Listed> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {}, pr.provider, pr.repo, pr.number, pr.title, pr.url, pr.head_ref, pr.author, pr.fork, pr.state AS pull_state,
                d.status AS deployment_status, (c.id IS NOT NULL) AS copy_left,
                pyo.ms(p.created_at) AS created_ms, pyo.ms(greatest(p.updated_at, coalesce(d.finished_at, d.created_at, p.updated_at))) AS updated_ms
           FROM pyo.previews p JOIN pyo.pull_requests pr ON pr.id = p.pull_id
           LEFT JOIN pyo.deployments d ON d.id = p.deployment_id
           LEFT JOIN pyo.services c ON c.id = p.preview_service_id
          WHERE p.team_id = ANY($1) AND (p.state <> 'removed' OR c.id IS NOT NULL OR p.updated_at > now() - interval '1 day')
          ORDER BY pr.number DESC, p.created_at",
        PREVIEW_COLS
            .split(", ")
            .map(|c| format!("p.{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
    .bind(team_ids)
    .fetch_all(&mut *db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let shown = Shown::of(&r.row.state, r.deployment_status.as_deref(), r.copy_left);
            json!({
                "id": r.row.id, "teamId": r.row.team_id, "serviceId": r.row.service_id,
                "previewServiceId": r.row.preview_service_id,
                "provider": r.provider, "repo": r.repo, "number": r.number, "title": r.title, "url": r.url,
                "branch": r.head_ref, "sha": r.row.sha, "author": r.author, "fork": r.fork,
                "name": r.row.name, "hostname": r.row.hostname,
                "status": shown.key(), "message": r.row.message, "deploymentId": r.row.deployment_id,
                "pullState": r.pull_state, "createdAt": r.created_ms, "updatedAt": r.updated_ms,
            })
        })
        .collect())
}

/// deletePreview: removes a preview now. The next push brings it back.
pub async fn delete(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
    wake: &mut Vec<String>,
) -> ApiResult<Value> {
    let p = owned(tx, user, id).await?;
    if p.state != "removed" {
        remove(
            tx,
            &p,
            "user",
            "Removed. The next push brings it back.",
            wake,
        )
        .await?;
    }
    crate::jobs::enqueue(&mut **tx, "preview.report", json!({ "pullId": p.pull_id })).await?;
    log_activity(
        tx,
        &p.team_id,
        &user.name,
        "preview.removed",
        "info",
        &format!("Removed the preview {}", p.name),
        json!({ "kind": "service", "id": p.service_id, "label": p.name }),
        json!({ "serviceId": p.service_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

/// redeployPreview: the pull request's head again, once the call commits
/// (a job): a failed one is tried again, a waiting one starts when there's
/// room, a removed one comes back.
pub async fn redeploy(
    tx: &mut Transaction<'_, Postgres>,
    user: &User,
    id: &str,
) -> ApiResult<Value> {
    let p = owned(tx, user, id).await?;
    // Forget what was tried: the next pass deploys the head again.
    sqlx::query("UPDATE pyo.previews SET sha = '', state = CASE WHEN state = 'removed' THEN 'waiting' ELSE state END, removed_reason = NULL, updated_at = now() WHERE id = $1")
        .bind(&p.id)
        .execute(&mut **tx)
        .await?;
    log_activity(
        tx,
        &p.team_id,
        &user.name,
        "preview.redeployed",
        "info",
        &format!("Redeployed the preview {}", p.name),
        json!({ "kind": "service", "id": p.service_id, "label": p.name }),
        json!({ "serviceId": p.service_id }),
    )
    .await?;
    crate::jobs::enqueue(
        &mut **tx,
        "preview.reconcile",
        json!({ "pullId": p.pull_id }),
    )
    .await?;
    Ok(json!({ "ok": true }))
}

async fn owned(tx: &mut Transaction<'_, Postgres>, user: &User, id: &str) -> ApiResult<PreviewRow> {
    let p: PreviewRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {PREVIEW_COLS} FROM pyo.previews WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("Preview"))?;
    require_role(tx, user, &p.team_id, Role::Developer).await?;
    Ok(p)
}
